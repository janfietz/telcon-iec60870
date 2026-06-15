//! IEC 60870-5 outstation test daemon — `iec-server`.
//!
//! Long-running daemon that hosts a process image, accepts connections from
//! IEC 60870-5-104 (TCP) or IEC 60870-5-101 (serial) clients, and is
//! controlled via a JSON-over-Unix-socket API.

#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::BytesMut;
use clap::{Args, Parser, Subcommand};
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::types::{
    C_CI_NA_1, C_CS_NA_1, C_DC_NA_1, C_IC_NA_1, C_RC_NA_1, C_RD_NA_1, C_RP_NA_1, C_SC_NA_1,
    C_SC_TA_1, C_SE_NA_1, C_SE_NB_1, C_SE_NC_1, C_SE_TA_1, C_SE_TB_1, C_SE_TC_1,
};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::ServerSender;
use iec60870::{DefaultLoggingHandler, Server104, ServerEvent};
use tokio::sync::{broadcast, mpsc, RwLock};

use iec60870_test_tools::control::{self, ControlHandler};
use iec60870_test_tools::points::{encode_point, kind_for_group, populate_default, ProcessImage};
use iec60870_test_tools::transport::{TransportArgs, TransportChoice, TransportKind};
use iec60870_test_tools::wire::{
    DoublePointWire, Event, PointKind, PointValue, QualityWire, Request, Response, SimSchedule,
};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "iec-server", about = "IEC 60870-5 outstation test daemon")]
struct Cli {
    /// Path to the control Unix socket (for non-daemon subcommands).
    #[arg(long, global = true, default_value = "/tmp/iec-test-server.sock")]
    control: PathBuf,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand, Debug)]
enum CliCommand {
    /// Run the long-running daemon (blocks until shutdown).
    Daemon(DaemonArgs),
    /// Read one IOA's current value.
    Get(GetArgs),
    /// Set one IOA's value.
    Set(SetArgs),
    /// List configured IOAs.
    List(ListArgs),
    /// Simulator sub-commands.
    Sim(SimArgs),
    /// Deadband sub-commands.
    Deadband(DeadbandArgs),
    /// Stream events as NDJSON.
    Events,
    /// Show daemon status.
    Status,
    /// Stop the daemon.
    Shutdown,
}

#[derive(Args, Debug)]
struct DaemonArgs {
    #[command(flatten)]
    transport: TransportArgs,

    /// Directory to serve files from (104 only).
    #[arg(long)]
    files_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct GetArgs {
    /// IOA to read.
    #[arg(long)]
    ioa: u32,
}

#[derive(Args, Debug)]
struct SetArgs {
    /// IOA to update.
    #[arg(long)]
    ioa: u32,

    /// Point kind: sp-na, dp-na, me-na, me-nb, me-nc, sp-tb, dp-tb, me-td, me-te, me-tf.
    #[arg(long = "kind")]
    kind: String,

    /// Value to set (true/false for single/double, number for measured).
    #[arg(long)]
    value: String,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Filter by TypeID (optional).
    #[arg(long)]
    type_id: Option<u8>,
}

#[derive(Args, Debug)]
struct SimArgs {
    #[command(subcommand)]
    command: SimSubcommand,
}

#[derive(Subcommand, Debug)]
enum SimSubcommand {
    /// Read one IOA's simulator schedule.
    Get(SimGetArgs),
    /// Set one IOA's simulator schedule.
    Set(SimSetArgs),
}

#[derive(Args, Debug)]
struct SimGetArgs {
    #[arg(long)]
    ioa: u32,
}

#[derive(Args, Debug)]
struct SimSetArgs {
    #[arg(long)]
    ioa: u32,
    /// JSON schedule (e.g. `{"kind":"toggle","interval_ms":5000}`).
    #[arg(long)]
    schedule: String,
}

#[derive(Args, Debug)]
struct DeadbandArgs {
    #[command(subcommand)]
    command: DeadbandSubcommand,
}

#[derive(Subcommand, Debug)]
enum DeadbandSubcommand {
    /// Read one IOA's deadband policy.
    Get(DeadbandGetArgs),
    /// Set one IOA's deadband policy.
    Set(DeadbandSetArgs),
}

#[derive(Args, Debug)]
struct DeadbandGetArgs {
    #[arg(long)]
    ioa: u32,
}

#[derive(Args, Debug)]
struct DeadbandSetArgs {
    #[arg(long)]
    ioa: u32,
    /// JSON policy, e.g.:
    ///   `{"kind":"none"}`
    ///   `{"kind":"absolute","delta":0.5}`
    ///   `{"kind":"percent","pct":5.0,"floor":0.001}`
    #[arg(long)]
    policy: String,
}

// ---------------------------------------------------------------------------
// Daemon shared state
// ---------------------------------------------------------------------------

/// Connected peer entry in the 104 sender map.
#[derive(Debug, Clone)]
struct PeerEntry {
    sender: ServerSender,
}

/// All mutable daemon state behind a single `RwLock`.
struct DaemonState {
    image: ProcessImage,
    /// Per-IOA deadband state for gating spontaneous emissions.
    tracker: iec60870::DeadbandTracker,
    /// 104 peers keyed by `SocketAddr`. Empty for 101.
    peers: HashMap<SocketAddr, PeerEntry>,
    /// 101 send channel. `None` for 104.
    outstation_tx: Option<mpsc::Sender<Vec<u8>>>,
    start_time: Instant,
    transport_kind: TransportKind,
    coa: CommonAddress,
}

impl DaemonState {
    fn new(kind: TransportKind, coa: u16) -> Self {
        let mut image = ProcessImage::new();
        populate_default(&mut image);
        Self {
            image,
            tracker: iec60870::DeadbandTracker::new(),
            peers: HashMap::new(),
            outstation_tx: None,
            start_time: Instant::now(),
            transport_kind: kind,
            coa: CommonAddress(coa),
        }
    }

    /// Broadcast raw bytes to all connected 104 peers (or push to 101 channel).
    async fn broadcast(&self, bytes: Vec<u8>) {
        if let Some(tx) = &self.outstation_tx {
            let _ = tx.send(bytes).await;
        } else {
            for entry in self.peers.values() {
                let _ = entry.sender.send_asdu(bytes.clone()).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Control handler
// ---------------------------------------------------------------------------

struct ServerHandler {
    state: Arc<RwLock<DaemonState>>,
    event_tx: broadcast::Sender<Event>,
    shutdown_tx: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ControlHandler for ServerHandler {
    async fn handle(&self, req: Request) -> Response {
        match req {
            Request::Get { ioa } => self.handle_get(ioa).await,
            Request::Set {
                ioa,
                value,
                quality,
            } => self.handle_set(ioa, value, quality).await,
            Request::List { type_id } => self.handle_list(type_id).await,
            Request::SimGet { ioa } => self.handle_sim_get(ioa).await,
            Request::SimSet { ioa, schedule } => self.handle_sim_set(ioa, schedule).await,
            Request::Status => self.handle_status().await,
            Request::Shutdown => {
                self.shutdown_tx.notify_one();
                Response::ok_empty()
            }
            // Client-only ops.
            Request::Interrogate { .. }
            | Request::CmdSingle { .. }
            | Request::CmdDouble { .. }
            | Request::CmdRegulating { .. }
            | Request::CmdSetpoint { .. }
            | Request::Read { .. }
            | Request::FileGet { .. }
            | Request::FilePut { .. } => Response::err("not a server op"),
            Request::Events => Response::err("use subscribe_events"),
            Request::SetDeadband { ioa, policy } => self.handle_set_deadband(ioa, policy).await,
            Request::GetDeadband { ioa } => self.handle_get_deadband(ioa).await,
        }
    }

    async fn subscribe_events(&self) -> Option<mpsc::Receiver<Event>> {
        let (tx, rx) = mpsc::channel(256);
        let mut bcast_rx = self.event_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match bcast_rx.recv().await {
                    Ok(evt) => {
                        if tx.send(evt).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Some(rx)
    }
}

impl ServerHandler {
    async fn handle_get(&self, ioa: u32) -> Response {
        let state = self.state.read().await;
        match state.image.get(ioa) {
            None => Response::err(format!("IOA {ioa} not found")),
            Some(entry) => {
                let data = serde_json::json!({
                    "ioa": ioa,
                    "kind": serde_json::to_value(entry.kind).unwrap_or_default(),
                    "value": serde_json::to_value(&entry.value).unwrap_or_default(),
                    "quality": serde_json::to_value(entry.quality).unwrap_or_default(),
                });
                Response::ok(data)
            }
        }
    }

    async fn handle_set(
        &self,
        ioa: u32,
        value: PointValue,
        quality: Option<QualityWire>,
    ) -> Response {
        let bytes = {
            let mut state = self.state.write().await;
            if !state.image.set(ioa, value, quality) {
                return Response::err(format!("IOA {ioa} not found"));
            }
            let entry = state.image.get(ioa).unwrap().clone();
            let coa = state.coa;
            let bytes = encode_point(ioa, &entry, Cot::with(Cause::SPONTANEOUS), coa);
            // Baseline tracks any outgoing ASDU carrying the value.
            let (val, qds) = iec60870_test_tools::points::entry_to_monitored(&entry);
            if let Err(e) = state
                .tracker
                .observe(iec60870_proto::asdu::Ioa(ioa), val, qds)
            {
                tracing::error!(?e, ioa, "deadband observe error in handle_set");
            }
            bytes
        };

        if let Some(bytes) = bytes {
            let state = self.state.read().await;
            state.broadcast(bytes).await;
        }

        Response::ok_empty()
    }

    async fn handle_list(&self, type_id: Option<u8>) -> Response {
        let state = self.state.read().await;
        let filter_kind = type_id.and_then(PointKind::from_type_id);

        let mut ioas: Vec<u32> = state
            .image
            .iter()
            .filter(|(_, e)| filter_kind.is_none_or(|k| e.kind == k))
            .map(|(ioa, _)| ioa)
            .collect();
        ioas.sort_unstable();

        let mut points = Vec::new();
        for ioa in ioas {
            if let Some(entry) = state.image.get(ioa) {
                points.push(serde_json::json!({
                    "ioa": ioa,
                    "type_id": entry.kind.type_id(),
                    "kind": serde_json::to_value(entry.kind).unwrap_or_default(),
                    "value": serde_json::to_value(&entry.value).unwrap_or_default(),
                    "quality": serde_json::to_value(entry.quality).unwrap_or_default(),
                }));
            }
        }

        Response::ok(serde_json::json!({ "points": points }))
    }

    async fn handle_sim_get(&self, ioa: u32) -> Response {
        let state = self.state.read().await;
        match state.image.get(ioa) {
            None => Response::err(format!("IOA {ioa} not found")),
            Some(entry) => Response::ok(serde_json::json!({
                "ioa": ioa,
                "schedule": serde_json::to_value(&entry.schedule).unwrap_or_default(),
            })),
        }
    }

    async fn handle_sim_set(&self, ioa: u32, schedule: SimSchedule) -> Response {
        let mut state = self.state.write().await;
        match state.image.get_mut(ioa) {
            None => Response::err(format!("IOA {ioa} not found")),
            Some(entry) => {
                entry.schedule = schedule;
                Response::ok_empty()
            }
        }
    }

    async fn handle_status(&self) -> Response {
        let state = self.state.read().await;
        let uptime_s = state.start_time.elapsed().as_secs();
        let peers = state.peers.len();
        let points = state.image.len();
        let transport = match state.transport_kind {
            TransportKind::Tcp => "tcp",
            TransportKind::Serial => "serial",
        };
        Response::ok(serde_json::json!({
            "transport": transport,
            "peers": peers,
            "points": points,
            "uptime_s": uptime_s,
        }))
    }

    async fn handle_set_deadband(
        &self,
        ioa: u32,
        policy: iec60870_test_tools::wire::DeadbandPolicyWire,
    ) -> Response {
        let mut state = self.state.write().await;
        state
            .tracker
            .set_policy(iec60870_proto::asdu::Ioa(ioa), policy.into_policy());
        Response::ok_empty()
    }

    async fn handle_get_deadband(&self, ioa: u32) -> Response {
        let state = self.state.read().await;
        let policy = state.tracker.policy(iec60870_proto::asdu::Ioa(ioa));
        let wire = iec60870_test_tools::wire::DeadbandPolicyWire::from_policy(policy);
        let data = serde_json::json!({ "ioa": ioa, "policy": wire });
        Response::ok(data)
    }
}

// ---------------------------------------------------------------------------
// ASDU encoding helpers
// ---------------------------------------------------------------------------

/// Encode an ASDU payload (single-count) to bytes, IEC104 addressing.
fn encode_asdu_bytes<P: AsduPayload>(payload: &P, cot: Cot, ca: CommonAddress) -> Vec<u8> {
    let vsq = Vsq::single(1);
    let asdu = Asdu::from_payload(cot, ca, vsq, payload, AsduAddressing::IEC104);
    let mut buf = BytesMut::new();
    asdu.encode(&mut buf, AsduAddressing::IEC104);
    buf.to_vec()
}

// ---------------------------------------------------------------------------
// Interrogation responders
// ---------------------------------------------------------------------------

async fn respond_interrogation_general(
    state: &mut DaemonState,
    send: &dyn AsduSender,
    event_tx: &broadcast::Sender<Event>,
) {
    let ca = state.coa;

    // ACK.
    let ack = C_IC_NA_1 {
        objects: vec![(Ioa(0), iec60870::proto::asdu::types::Qoi::GENERAL)],
    };
    let bytes = encode_asdu_bytes_list(&ack, Cot::with(Cause::ACTIVATION_CON), ca);
    let _ = send.send(bytes).await;

    // Emit all points sorted by IOA.
    let mut ioas: Vec<u32> = state.image.iter().map(|(ioa, _)| ioa).collect();
    ioas.sort_unstable();
    for ioa in ioas {
        // Snapshot what we need from the image, then drop the immutable
        // borrow so we can mutate `state.tracker` afterwards.
        let snapshot = state.image.get(ioa).map(|entry| {
            let bytes = encode_point(ioa, entry, Cot::with(Cause::INTERROGATED_GENERAL), ca);
            let (val, qds) = iec60870_test_tools::points::entry_to_monitored(entry);
            (bytes, val, qds)
        });
        if let Some((bytes_opt, val, qds)) = snapshot {
            if let Err(e) = state
                .tracker
                .observe(iec60870_proto::asdu::Ioa(ioa), val, qds)
            {
                tracing::error!(?e, ioa, "deadband observe error in GI");
            }
            if let Some(bytes) = bytes_opt {
                let _ = send.send(bytes).await;
            }
        }
    }

    // Termination.
    let term = C_IC_NA_1 {
        objects: vec![(Ioa(0), iec60870::proto::asdu::types::Qoi::GENERAL)],
    };
    let bytes = encode_asdu_bytes_list(&term, Cot::with(Cause::ACTIVATION_TERMINATION), ca);
    let _ = send.send(bytes).await;

    let _ = event_tx.send(Event::AsduSent {
        cot: format!("{}", Cause::INTERROGATED_GENERAL.raw()),
        type_id: C_IC_NA_1::TYPE_ID,
        ioa: 0,
        value: serde_json::Value::Null,
    });
}

async fn respond_interrogation_group(
    state: &mut DaemonState,
    send: &dyn AsduSender,
    group: u8,
    _event_tx: &broadcast::Sender<Event>,
) {
    let ca = state.coa;
    let qoi = iec60870::proto::asdu::types::Qoi::group(group);

    // ACK.
    let ack = C_IC_NA_1 {
        objects: vec![(Ioa(0), qoi)],
    };
    let bytes = encode_asdu_bytes_list(&ack, Cot::with(Cause::ACTIVATION_CON), ca);
    let _ = send.send(bytes).await;

    if let Some(kind) = kind_for_group(group) {
        let cot_group = Cot::with(Cause::interrogated_group(group));
        let mut ioas: Vec<u32> = state.image.iter_kind(kind).map(|(ioa, _)| ioa).collect();
        ioas.sort_unstable();
        for ioa in ioas {
            let snapshot = state.image.get(ioa).map(|entry| {
                let bytes = encode_point(ioa, entry, cot_group, ca);
                let (val, qds) = iec60870_test_tools::points::entry_to_monitored(entry);
                (bytes, val, qds)
            });
            if let Some((bytes_opt, val, qds)) = snapshot {
                if let Err(e) = state
                    .tracker
                    .observe(iec60870_proto::asdu::Ioa(ioa), val, qds)
                {
                    tracing::error!(?e, ioa, "deadband observe error in group interrogation");
                }
                if let Some(bytes) = bytes_opt {
                    let _ = send.send(bytes).await;
                }
            }
        }
    }

    // Termination.
    let term = C_IC_NA_1 {
        objects: vec![(Ioa(0), qoi)],
    };
    let bytes = encode_asdu_bytes_list(&term, Cot::with(Cause::ACTIVATION_TERMINATION), ca);
    let _ = send.send(bytes).await;
}

/// Encode a list-based payload (like C_IC_NA_1) using its object count.
fn encode_asdu_bytes_list<P: AsduPayload + HasObjectCount>(
    payload: &P,
    cot: Cot,
    ca: CommonAddress,
) -> Vec<u8> {
    let vsq = Vsq::single(payload.object_count() as u8);
    let asdu = Asdu::from_payload(cot, ca, vsq, payload, AsduAddressing::IEC104);
    let mut buf = BytesMut::new();
    asdu.encode(&mut buf, AsduAddressing::IEC104);
    buf.to_vec()
}

trait HasObjectCount {
    fn object_count(&self) -> usize;
}

impl HasObjectCount for C_IC_NA_1 {
    fn object_count(&self) -> usize {
        self.objects.len()
    }
}

// ---------------------------------------------------------------------------
// Abstract sender (works for both 104 and 101 paths)
// ---------------------------------------------------------------------------

#[async_trait]
trait AsduSender: Send + Sync {
    async fn send(&self, bytes: Vec<u8>) -> bool;
}

struct Sender104(ServerSender);

#[async_trait]
impl AsduSender for Sender104 {
    async fn send(&self, bytes: Vec<u8>) -> bool {
        self.0.send_asdu(bytes).await.is_ok()
    }
}

struct Sender101(mpsc::Sender<Vec<u8>>);

#[async_trait]
impl AsduSender for Sender101 {
    async fn send(&self, bytes: Vec<u8>) -> bool {
        self.0.send(bytes).await.is_ok()
    }
}

// ---------------------------------------------------------------------------
// Per-connection ASDU handler
// ---------------------------------------------------------------------------

async fn handle_incoming_asdu(
    bytes: &[u8],
    send: &dyn AsduSender,
    state: Arc<RwLock<DaemonState>>,
    event_tx: &broadcast::Sender<Event>,
) {
    let asdu = match Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(?e, "failed to decode incoming asdu");
            return;
        }
    };

    let type_id = asdu.type_id();
    let cot = asdu.cot();
    let ca = {
        let s = state.read().await;
        s.coa
    };

    // Extract first IOA from header bytes (bytes 6..9 in IEC104).
    let ioa_raw: u32 = if bytes.len() >= 9 {
        u32::from(bytes[6]) | (u32::from(bytes[7]) << 8) | (u32::from(bytes[8]) << 16)
    } else {
        0
    };

    // Emit received event.
    let _ = event_tx.send(Event::AsduReceived {
        cot: format!("{}", cot.cause().raw()),
        type_id,
        ioa: ioa_raw,
        value: serde_json::Value::Null,
    });

    match type_id {
        // C_IC_NA_1 — interrogation (type 100)
        100 => {
            if let Ok(ic) = asdu.decode_payload::<C_IC_NA_1>(AsduAddressing::IEC104) {
                let qoi = ic
                    .objects
                    .first()
                    .map(|(_, q)| *q)
                    .unwrap_or(iec60870::proto::asdu::types::Qoi::GENERAL);
                let mut s = state.write().await;
                if qoi == iec60870::proto::asdu::types::Qoi::GENERAL {
                    respond_interrogation_general(&mut s, send, event_tx).await;
                } else {
                    let group = qoi.0.saturating_sub(20);
                    respond_interrogation_group(&mut s, send, group, event_tx).await;
                }
            }
        }
        // C_CI_NA_1 — counter interrogation (type 101)
        101 => {
            if let Ok(ci) = asdu.decode_payload::<C_CI_NA_1>(AsduAddressing::IEC104) {
                let ack = C_CI_NA_1 {
                    ioa: ci.ioa,
                    qcc: ci.qcc,
                };
                let bytes = encode_asdu_bytes(&ack, Cot::with(Cause::ACTIVATION_CON), ca);
                let _ = send.send(bytes).await;
            }
        }
        // C_RD_NA_1 — read command (type 102)
        102 => {
            if let Ok(rd) = asdu.decode_payload::<C_RD_NA_1>(AsduAddressing::IEC104) {
                let ack = C_RD_NA_1 { ioa: rd.ioa };
                let bytes = encode_asdu_bytes(&ack, Cot::with(Cause::REQUEST), ca);
                let _ = send.send(bytes).await;
            }
        }
        // C_CS_NA_1 — clock sync (type 103)
        103 => {
            if let Ok(cs) = asdu.decode_payload::<C_CS_NA_1>(AsduAddressing::IEC104) {
                let ack = C_CS_NA_1 {
                    ioa: cs.ioa,
                    time: cs.time,
                };
                let bytes = encode_asdu_bytes(&ack, Cot::with(Cause::ACTIVATION_CON), ca);
                let _ = send.send(bytes).await;
            }
        }
        // C_RP_NA_1 — reset process (type 105)
        105 => {
            if let Ok(rp) = asdu.decode_payload::<C_RP_NA_1>(AsduAddressing::IEC104) {
                let ack = C_RP_NA_1 {
                    ioa: rp.ioa,
                    qrp: rp.qrp,
                };
                let bytes = encode_asdu_bytes(&ack, Cot::with(Cause::ACTIVATION_CON), ca);
                let _ = send.send(bytes).await;
            }
        }
        // Command types — ACK → 50 ms → TERMINATION
        45 => {
            handle_command_list::<C_SC_NA_1>(asdu, send, ca, event_tx).await;
        }
        46 => {
            handle_command_list::<C_DC_NA_1>(asdu, send, ca, event_tx).await;
        }
        47 => {
            handle_command_list::<C_RC_NA_1>(asdu, send, ca, event_tx).await;
        }
        48 => {
            handle_command_list::<C_SE_NA_1>(asdu, send, ca, event_tx).await;
        }
        49 => {
            handle_command_list::<C_SE_NB_1>(asdu, send, ca, event_tx).await;
        }
        50 => {
            handle_command_list::<C_SE_NC_1>(asdu, send, ca, event_tx).await;
        }
        58 => {
            handle_command_list::<C_SC_TA_1>(asdu, send, ca, event_tx).await;
        }
        61 => {
            handle_command_list::<C_SE_TA_1>(asdu, send, ca, event_tx).await;
        }
        62 => {
            handle_command_list::<C_SE_TB_1>(asdu, send, ca, event_tx).await;
        }
        63 => {
            handle_command_list::<C_SE_TC_1>(asdu, send, ca, event_tx).await;
        }
        other => {
            tracing::debug!(type_id = other, "unhandled incoming type_id");
        }
    }
}

/// List-based command types (those with `objects: Vec<(Ioa, V)>`).
trait ListPayload: AsduPayload + Clone + std::fmt::Debug {
    fn object_count(&self) -> usize;
}

macro_rules! impl_list_payload {
    ($t:ty) => {
        impl ListPayload for $t {
            fn object_count(&self) -> usize {
                self.objects.len()
            }
        }
    };
}

impl_list_payload!(C_SC_NA_1);
impl_list_payload!(C_DC_NA_1);
impl_list_payload!(C_RC_NA_1);
impl_list_payload!(C_SE_NA_1);
impl_list_payload!(C_SE_NB_1);
impl_list_payload!(C_SE_NC_1);
impl_list_payload!(C_SC_TA_1);
impl_list_payload!(C_SE_TA_1);
impl_list_payload!(C_SE_TB_1);
impl_list_payload!(C_SE_TC_1);

/// Generic command-direction responder for list-based payloads.
/// ACK (ACTIVATION_CON) → 50 ms sleep → TERMINATION.
async fn handle_command_list<P>(
    asdu: Asdu,
    send: &dyn AsduSender,
    ca: CommonAddress,
    event_tx: &broadcast::Sender<Event>,
) where
    P: ListPayload,
{
    let cot_in = asdu.cot();
    let type_id = P::TYPE_ID;

    let payload = match asdu.decode_payload::<P>(AsduAddressing::IEC104) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(?e, "failed to decode command payload");
            return;
        }
    };

    let count = payload.object_count();
    let vsq = Vsq::single(count as u8);

    // ACTIVATION_CON.
    let ack_asdu = Asdu::from_payload(
        Cot::with(Cause::ACTIVATION_CON),
        ca,
        vsq,
        &payload,
        AsduAddressing::IEC104,
    );
    let mut buf = BytesMut::new();
    ack_asdu.encode(&mut buf, AsduAddressing::IEC104);
    let _ = send.send(buf.to_vec()).await;

    let _ = event_tx.send(Event::AsduReceived {
        cot: format!("{}", cot_in.cause().raw()),
        type_id,
        ioa: 0,
        value: serde_json::Value::Null,
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // ACTIVATION_TERMINATION.
    let term_asdu = Asdu::from_payload(
        Cot::with(Cause::ACTIVATION_TERMINATION),
        ca,
        vsq,
        &payload,
        AsduAddressing::IEC104,
    );
    let mut buf = BytesMut::new();
    term_asdu.encode(&mut buf, AsduAddressing::IEC104);
    let _ = send.send(buf.to_vec()).await;
}

// ---------------------------------------------------------------------------
// 104 peer loop
// ---------------------------------------------------------------------------

async fn run_104_peer(
    conn: iec60870::ServerConnection,
    state: Arc<RwLock<DaemonState>>,
    event_tx: broadcast::Sender<Event>,
) {
    let peer = conn.peer();
    let (sender, mut events) = conn.split();

    // Register sender.
    {
        let mut s = state.write().await;
        s.peers.insert(
            peer,
            PeerEntry {
                sender: sender.clone(),
            },
        );
    }
    let _ = event_tx.send(Event::Connected);

    let send = Sender104(sender);

    loop {
        match events.recv().await {
            Some(ServerEvent::Asdu(bytes)) => {
                handle_incoming_asdu(&bytes, &send, Arc::clone(&state), &event_tx).await;
            }
            Some(ServerEvent::StateChanged(st)) => {
                let _ = event_tx.send(Event::StateChanged {
                    state: format!("{st:?}"),
                });
            }
            Some(ServerEvent::Closed(reason)) => {
                tracing::info!(?reason, %peer, "peer disconnected");
                break;
            }
            Some(_) => {}
            None => break,
        }
    }

    // Unregister.
    {
        let mut s = state.write().await;
        s.peers.remove(&peer);
    }
    let _ = event_tx.send(Event::Disconnected { reason: None });
}

// ---------------------------------------------------------------------------
// 101 outstation loop
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_101_outstation(
    path: PathBuf,
    baud: u32,
    link_addr: u16,
    link_addr_size: iec60870_test_tools::transport::LinkAddrSize,
    state: Arc<RwLock<DaemonState>>,
    event_tx: broadcast::Sender<Event>,
    outstation_asdu_tx: mpsc::Sender<Vec<u8>>,
    mut outstation_asdu_rx: mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    use iec60870::proto::frame101::frame::LinkAddress;
    use iec60870::proto::frame101::link::Config as LinkConfig;
    use iec60870::serial::SerialSettings;
    use iec60870::{Outstation101, Outstation101Event};

    let cfg = LinkConfig {
        link_address: LinkAddress(link_addr),
        addr_size: link_addr_size.to_proto(),
        ..LinkConfig::default()
    };

    tracing::info!(port = %path.display(), baud, "outstation opening serial port");
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("serial path is not valid UTF-8"))?;
    let mut outstation = Outstation101::open(
        path_str,
        SerialSettings {
            baud,
            ..SerialSettings::default()
        },
        cfg,
    )
    .await?;

    // Register the outstation_tx so spontaneous sends can reach the outstation.
    {
        let mut s = state.write().await;
        s.outstation_tx = Some(outstation_asdu_tx);
    }

    let _ = event_tx.send(Event::Connected);

    loop {
        tokio::select! {
            evt = outstation.recv() => {
                match evt {
                    Some(Outstation101Event::Asdu(bytes)) => {
                        let tx = {
                            let s = state.read().await;
                            s.outstation_tx.as_ref().unwrap().clone()
                        };
                        let send = Sender101(tx);
                        handle_incoming_asdu(&bytes, &send, Arc::clone(&state), &event_tx).await;
                    }
                    Some(Outstation101Event::LinkStateChanged(ls)) => {
                        let _ = event_tx.send(Event::StateChanged { state: format!("{ls:?}") });
                    }
                    Some(Outstation101Event::Closed(r)) => {
                        tracing::info!(?r, "101 link closed");
                        break;
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            Some(bytes) = outstation_asdu_rx.recv() => {
                let _ = outstation.send_asdu(bytes).await;
            }
        }
    }

    let _ = event_tx.send(Event::Disconnected { reason: None });
    Ok(())
}

// ---------------------------------------------------------------------------
// Value advancement (inline, avoids circular dep with sim.rs)
// ---------------------------------------------------------------------------

fn advance_value(
    schedule: &SimSchedule,
    current: Option<&PointValue>,
    elapsed_ticks: u64,
) -> Option<PointValue> {
    use rand::Rng as _;
    match schedule {
        SimSchedule::None => None,
        SimSchedule::Toggle { .. } => {
            let on = match current {
                Some(PointValue::Single(b)) => !b,
                _ => true,
            };
            Some(PointValue::Single(on))
        }
        SimSchedule::Rotate { .. } => {
            let next = match current {
                Some(PointValue::Double(DoublePointWire::Off)) => DoublePointWire::On,
                Some(PointValue::Double(DoublePointWire::On)) => DoublePointWire::Off,
                _ => DoublePointWire::Off,
            };
            Some(PointValue::Double(next))
        }
        SimSchedule::RandomWalk { step, min, max, .. } => {
            let current_f = match current {
                Some(PointValue::Normalized(f)) => *f,
                Some(PointValue::Float(f)) => *f,
                _ => 0.0_f32,
            };
            let delta = if rand::thread_rng().gen_bool(0.5) {
                *step
            } else {
                -*step
            };
            let new_val = (current_f + delta).clamp(*min, *max);
            match current {
                Some(PointValue::Normalized(_)) => Some(PointValue::Normalized(new_val)),
                _ => Some(PointValue::Float(new_val)),
            }
        }
        SimSchedule::StepUp { step, wrap_at, .. } => {
            let current_i = match current {
                Some(PointValue::Scaled(s)) => i32::from(*s),
                _ => 0_i32,
            };
            let new_val = (current_i + step).rem_euclid(*wrap_at);
            #[allow(clippy::cast_possible_truncation)]
            Some(PointValue::Scaled(
                new_val.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
            ))
        }
        SimSchedule::Sine {
            period_ms,
            interval_ms,
            amplitude,
            offset,
            ..
        } => {
            let ticks_per_period = (*period_ms).max(1) / (*interval_ms).max(1);
            let phase = if ticks_per_period == 0 {
                0.0_f64
            } else {
                (elapsed_ticks % ticks_per_period) as f64 / ticks_per_period as f64
            };
            let val = (f64::from(*amplitude) * (2.0 * std::f64::consts::PI * phase).sin()
                + f64::from(*offset)) as f32;
            match current {
                Some(PointValue::Normalized(_)) => {
                    Some(PointValue::Normalized(val.clamp(-1.0, 1.0)))
                }
                _ => Some(PointValue::Float(val)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon entry point
// ---------------------------------------------------------------------------

async fn run_daemon(args: DaemonArgs, control_socket: PathBuf) -> anyhow::Result<()> {
    let transport = args.transport.resolve()?;
    let (transport_kind, coa) = match &transport {
        TransportChoice::Tcp { coa, .. } => (TransportKind::Tcp, *coa),
        TransportChoice::Serial { coa, .. } => (TransportKind::Serial, *coa),
    };

    let state = Arc::new(RwLock::new(DaemonState::new(transport_kind, coa)));
    let (event_tx, _) = broadcast::channel::<Event>(256);
    let shutdown = Arc::new(tokio::sync::Notify::new());

    // Collect IOA entries before spawning tasks.
    let ioa_entries: Vec<(u32, SimSchedule)> = {
        let s = state.read().await;
        s.image
            .iter()
            .filter_map(|(ioa, e)| {
                if matches!(&e.schedule, SimSchedule::None) {
                    None
                } else {
                    Some((ioa, e.schedule.clone()))
                }
            })
            .collect()
    };

    // Spawn one interval task per scheduled IOA.
    for (ioa, schedule) in ioa_entries {
        let interval_ms = match &schedule {
            SimSchedule::Toggle { interval_ms, .. }
            | SimSchedule::Rotate { interval_ms }
            | SimSchedule::RandomWalk { interval_ms, .. }
            | SimSchedule::StepUp { interval_ms, .. }
            | SimSchedule::Sine { interval_ms, .. } => *interval_ms,
            SimSchedule::None => continue,
        };
        let phase_ms = match &schedule {
            SimSchedule::Toggle { phase_ms, .. } => *phase_ms,
            _ => 0,
        };

        let state_tick = Arc::clone(&state);
        let event_tx_tick = event_tx.clone();

        tokio::spawn(async move {
            if phase_ms > 0 {
                tokio::time::sleep(Duration::from_millis(phase_ms)).await;
            }
            let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut elapsed: u64 = 0;

            loop {
                ticker.tick().await;
                elapsed = elapsed.wrapping_add(1);

                let (bytes, kind_str) = {
                    let mut s = state_tick.write().await;
                    let ca = s.coa;
                    let schedule = match s.image.get(ioa) {
                        Some(e) => e.schedule.clone(),
                        None => return,
                    };
                    let current = s.image.get(ioa).map(|e| e.value.clone());
                    if let Some(v) = advance_value(&schedule, current.as_ref(), elapsed) {
                        s.image.set(ioa, v, None);
                    }
                    // Read updated entry + decide via deadband.
                    let (val, qds) = match s.image.get(ioa) {
                        Some(e) => iec60870_test_tools::points::entry_to_monitored(e),
                        None => return,
                    };
                    let decision =
                        match s.tracker.evaluate(iec60870_proto::asdu::Ioa(ioa), val, qds) {
                            Ok(d) => d,
                            Err(e) => {
                                tracing::error!(?e, ioa, "deadband evaluate error");
                                iec60870::EmitDecision::Suppress
                            }
                        };
                    let kind_str = s
                        .image
                        .get(ioa)
                        .map(|e| format!("{:?}", e.kind))
                        .unwrap_or_default();
                    let bytes = if matches!(decision, iec60870::EmitDecision::Emit) {
                        s.image
                            .get(ioa)
                            .and_then(|e| encode_point(ioa, e, Cot::with(Cause::SPONTANEOUS), ca))
                    } else {
                        None
                    };
                    (bytes, kind_str)
                };

                if let Some(bytes) = bytes {
                    let s = state_tick.read().await;
                    s.broadcast(bytes).await;
                }

                // Always emit the SimTick event so observers see ticks even
                // when the value channel is suppressed by deadband.
                let _ = event_tx_tick.send(Event::SimTick {
                    ioa,
                    kind: kind_str,
                });
            }
        });
    }

    // Build the control handler.
    let handler = Arc::new(ServerHandler {
        state: Arc::clone(&state),
        event_tx: event_tx.clone(),
        shutdown_tx: Arc::clone(&shutdown),
    });

    // Spawn control socket listener.
    {
        let handler = Arc::clone(&handler);
        let sock = control_socket.clone();
        tokio::spawn(async move {
            if let Err(e) = control::serve(&sock, handler).await {
                tracing::error!(?e, "control socket error");
            }
        });
    }

    // Transport-specific loop.
    match transport {
        TransportChoice::Tcp { addr, .. } => {
            run_tcp_daemon(args.files_dir, addr, state, event_tx, shutdown).await?;
        }
        TransportChoice::Serial {
            path,
            baud,
            link_addr,
            link_addr_size,
            ..
        } => {
            run_serial_daemon(
                args.files_dir,
                path,
                baud,
                link_addr,
                link_addr_size,
                state,
                event_tx,
                shutdown,
            )
            .await?;
        }
    }

    Ok(())
}

async fn run_tcp_daemon(
    files_dir: Option<PathBuf>,
    addr: SocketAddr,
    state: Arc<RwLock<DaemonState>>,
    event_tx: broadcast::Sender<Event>,
    shutdown: Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    let server = Server104::bind(addr, iec60870::proto::frame104::Config::default()).await?;

    let server = if let Some(dir) = files_dir {
        if dir.exists() {
            let provider = iec60870::file_transfer::FsFileTransferProvider::new(&dir)?;
            tracing::info!(dir = %dir.display(), "file-transfer provider active");
            server.with_file_provider(provider)
        } else {
            tracing::warn!(dir = %dir.display(), "files-dir does not exist, skipping file transfer");
            server
        }
    } else {
        server
    };

    tracing::info!(addr = ?server.local_addr()?, "104 server listening");

    loop {
        tokio::select! {
            result = server.accept_with(DefaultLoggingHandler) => {
                match result {
                    Ok(conn) => {
                        let state = Arc::clone(&state);
                        let event_tx = event_tx.clone();
                        tokio::spawn(run_104_peer(conn, state, event_tx));
                    }
                    Err(e) => {
                        tracing::error!(?e, "accept error");
                    }
                }
            }
            _ = shutdown.notified() => {
                tracing::info!("shutdown requested");
                break;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_serial_daemon(
    files_dir: Option<PathBuf>,
    path: PathBuf,
    baud: u32,
    link_addr: u16,
    link_addr_size: iec60870_test_tools::transport::LinkAddrSize,
    state: Arc<RwLock<DaemonState>>,
    event_tx: broadcast::Sender<Event>,
    shutdown: Arc<tokio::sync::Notify>,
) -> anyhow::Result<()> {
    if files_dir.is_some() {
        tracing::warn!("--files-dir is not supported for serial (101) transport; ignoring");
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);

    tokio::select! {
        r = run_101_outstation(path, baud, link_addr, link_addr_size, Arc::clone(&state), event_tx, tx, rx) => {
            if let Err(e) = r {
                tracing::error!(?e, "101 outstation error");
            }
        }
        _ = shutdown.notified() => {
            tracing::info!("shutdown requested");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client-mode subcommands
// ---------------------------------------------------------------------------

async fn client_call(socket: &std::path::Path, req: &Request) -> anyhow::Result<()> {
    let resp = control::call(socket, req).await?;
    println!("{}", serde_json::to_string(&resp)?);
    Ok(())
}

fn parse_point_value(kind: &str, value: &str) -> anyhow::Result<PointValue> {
    match kind.to_lowercase().replace('-', "_").as_str() {
        "sp_na" | "sp_tb" => {
            let b: bool = value
                .parse()
                .map_err(|_| anyhow::anyhow!("expected true/false"))?;
            Ok(PointValue::Single(b))
        }
        "dp_na" | "dp_tb" => {
            let s = match value.to_lowercase().as_str() {
                "off" | "false" | "0" => DoublePointWire::Off,
                "on" | "true" | "1" => DoublePointWire::On,
                "intermediate" => DoublePointWire::Intermediate,
                "indeterminate" => DoublePointWire::Indeterminate,
                _ => anyhow::bail!("expected off/on/intermediate/indeterminate"),
            };
            Ok(PointValue::Double(s))
        }
        "me_na" | "me_td" => {
            let f: f32 = value
                .parse()
                .map_err(|_| anyhow::anyhow!("expected float"))?;
            Ok(PointValue::Normalized(f))
        }
        "me_nb" | "me_te" => {
            let i: i16 = value.parse().map_err(|_| anyhow::anyhow!("expected i16"))?;
            Ok(PointValue::Scaled(i))
        }
        "me_nc" | "me_tf" => {
            let f: f32 = value
                .parse()
                .map_err(|_| anyhow::anyhow!("expected float"))?;
            Ok(PointValue::Float(f))
        }
        _ => anyhow::bail!("unknown kind: {kind}"),
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info,iec_server=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let socket = cli.control.clone();

    match cli.command {
        CliCommand::Daemon(args) => run_daemon(args, socket).await?,
        CliCommand::Get(args) => {
            client_call(&socket, &Request::Get { ioa: args.ioa }).await?;
        }
        CliCommand::Set(args) => {
            let value = parse_point_value(&args.kind, &args.value)?;
            client_call(
                &socket,
                &Request::Set {
                    ioa: args.ioa,
                    value,
                    quality: None,
                },
            )
            .await?;
        }
        CliCommand::List(args) => {
            client_call(
                &socket,
                &Request::List {
                    type_id: args.type_id,
                },
            )
            .await?;
        }
        CliCommand::Sim(sim_args) => match sim_args.command {
            SimSubcommand::Get(a) => {
                client_call(&socket, &Request::SimGet { ioa: a.ioa }).await?;
            }
            SimSubcommand::Set(a) => {
                let schedule: SimSchedule = serde_json::from_str(&a.schedule)
                    .map_err(|e| anyhow::anyhow!("invalid schedule JSON: {e}"))?;
                client_call(
                    &socket,
                    &Request::SimSet {
                        ioa: a.ioa,
                        schedule,
                    },
                )
                .await?;
            }
        },
        CliCommand::Deadband(args) => match args.command {
            DeadbandSubcommand::Get(g) => {
                client_call(&socket, &Request::GetDeadband { ioa: g.ioa }).await?;
            }
            DeadbandSubcommand::Set(s) => {
                let policy: iec60870_test_tools::wire::DeadbandPolicyWire =
                    serde_json::from_str(&s.policy)
                        .map_err(|e| anyhow::anyhow!("invalid policy JSON: {e}"))?;
                client_call(&socket, &Request::SetDeadband { ioa: s.ioa, policy }).await?;
            }
        },
        CliCommand::Events => {
            control::follow_events(&socket, |evt| {
                if let Ok(s) = serde_json::to_string(&evt) {
                    println!("{s}");
                }
            })
            .await?;
        }
        CliCommand::Status => {
            client_call(&socket, &Request::Status).await?;
        }
        CliCommand::Shutdown => {
            client_call(&socket, &Request::Shutdown).await?;
        }
    }

    Ok(())
}

//! IEC 60870-5 master test daemon (`iec-client`).
//!
//! Long-running daemon that maintains an open IEC-104 (TCP) or IEC-101
//! (serial) connection to an outstation, pumps incoming ASDUs into a
//! last-value cache, and serves a JSON-over-Unix-socket control plane so
//! agent subcommands can issue interrogations, commands, file transfers, reads,
//! and status queries without restarting the daemon.
//!
//! # Usage
//!
//! ```text
//! # start the daemon
//! iec-client daemon --transport tcp --addr 127.0.0.1:2404
//!
//! # send a single command
//! iec-client cmd single --ioa 2100 --on
//!
//! # read the most recent value for an IOA
//! iec-client read --ioa 100
//!
//! # shut down gracefully
//! iec-client shutdown
//! ```

#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Args, Parser, Subcommand};
use iec60870::file_transfer::FsFileTransferProvider;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{DoublePoint, Nva, Qds, Diq, R32, Siq, Sva};
use iec60870::proto::asdu::types::{
    Qoi, C_DC_NA_1, C_IC_NA_1, C_RC_NA_1, C_SC_NA_1, C_SE_NA_1, C_SE_NB_1, C_SE_NC_1,
    M_DP_NA_1, M_DP_TB_1, M_ME_NA_1, M_ME_NB_1, M_ME_NC_1, M_ME_TD_1, M_ME_TE_1, M_ME_TF_1,
    M_SP_NA_1, M_SP_TB_1,
};
use iec60870::proto::asdu::types::{Dco, Qos, Rco, Sco, StepDirection};
use iec60870::proto::asdu::types::file::NameOfFile;
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame101::frame::LinkAddress;
use iec60870::proto::frame101::link::{Config as LinkConfig, LinkState};
use iec60870::proto::frame104::Config as Config104;
use iec60870::{Client104, ClientEvent, DefaultLoggingHandler, Master101, Master101Event, Transport};
use iec60870_test_tools::cache::PointCache;
use iec60870_test_tools::control::{self, ControlHandler};
use iec60870_test_tools::transport::{TransportArgs, TransportChoice};
use iec60870_test_tools::wire::{
    DoublePointWire, Event, PointKind, PointValue, QualityWire, Request, Response, SetpointKind,
    StepDir,
};
use serde_json::json;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "iec-client", about = "IEC 60870-5 master test daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the long-running daemon (blocks until shutdown).
    Daemon(DaemonArgs),

    /// Send a general or group interrogation and collect responses.
    Interrogate {
        /// Group number (1-16). Omit for general interrogation.
        #[arg(long)]
        group: Option<u8>,
        /// Common address (overrides daemon default).
        #[arg(long)]
        ca: Option<u16>,
        /// Timeout in milliseconds (default 5000).
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// Control socket path.
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },

    /// Issue a command.
    #[command(subcommand)]
    Cmd(CmdCommands),

    /// Read the most recent cached value for an IOA.
    Read {
        /// Information object address.
        #[arg(long)]
        ioa: u32,
        /// Type ID filter (optional).
        #[arg(long)]
        type_id: Option<u8>,
        /// Control socket path.
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },

    /// File-transfer operations.
    #[command(subcommand)]
    File(FileCommands),

    /// Stream events as JSON until interrupted.
    Events {
        /// Control socket path.
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },

    /// Show daemon status.
    Status {
        /// Control socket path.
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },

    /// Stop the daemon.
    Shutdown {
        /// Control socket path.
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },
}

#[derive(Args, Debug)]
struct DaemonArgs {
    /// Control socket path.
    #[arg(long, default_value_os_t = control::default_client_socket())]
    control: PathBuf,

    /// Directory to write fetched files to (and read files for push).
    #[arg(long, default_value = "/tmp/iec-client-files")]
    files_dir: PathBuf,

    #[command(flatten)]
    transport: TransportArgs,
}

#[derive(Subcommand, Debug)]
enum CmdCommands {
    /// Issue `C_SC_NA_1` (single command).
    Single {
        #[arg(long)]
        ioa: u32,
        #[arg(long)]
        on: bool,
        #[arg(long)]
        ca: Option<u16>,
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },
    /// Issue `C_DC_NA_1` (double command).
    Double {
        #[arg(long)]
        ioa: u32,
        #[arg(long)]
        on: bool,
        #[arg(long)]
        ca: Option<u16>,
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },
    /// Issue `C_RC_NA_1` (regulating-step command).
    Regulating {
        #[arg(long)]
        ioa: u32,
        #[arg(long, value_enum)]
        step: StepDirArg,
        #[arg(long)]
        ca: Option<u16>,
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },
    /// Issue `C_SE_NA_1`/`NB_1`/`NC_1` (set-point command).
    Setpoint {
        #[arg(long)]
        ioa: u32,
        #[arg(long, value_enum)]
        kind: SetpointKindArg,
        #[arg(long)]
        value: f64,
        #[arg(long)]
        ca: Option<u16>,
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum StepDirArg {
    Lower,
    Higher,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum SetpointKindArg {
    Normalized,
    Scaled,
    Float,
}

#[derive(Subcommand, Debug)]
enum FileCommands {
    /// Pull a file from the outstation.
    Get {
        /// Name of file (decimal or 0xNNNN hex).
        #[arg(long)]
        nof: String,
        /// Output directory (default `--files-dir` of daemon).
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        ca: Option<u16>,
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },
    /// Push a file to the outstation.
    Put {
        /// Name of file (decimal or 0xNNNN hex).
        #[arg(long)]
        nof: String,
        /// Local path to the file to push.
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        ca: Option<u16>,
        #[arg(long, default_value_os_t = control::default_client_socket())]
        socket: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// IE conversion helpers (top-level so they don't trigger pedantic warnings)
// ---------------------------------------------------------------------------

fn qds_to_wire(qds: Qds) -> QualityWire {
    QualityWire {
        overflow: qds.overflow,
        blocked: qds.quality.blocked,
        substituted: qds.quality.substituted,
        not_topical: qds.quality.not_topical,
        invalid: qds.quality.invalid,
    }
}

fn siq_to_wire(siq: Siq) -> QualityWire {
    QualityWire {
        overflow: false,
        blocked: siq.quality.blocked,
        substituted: siq.quality.substituted,
        not_topical: siq.quality.not_topical,
        invalid: siq.quality.invalid,
    }
}

fn diq_to_wire(diq: Diq) -> QualityWire {
    QualityWire {
        overflow: false,
        blocked: diq.quality.blocked,
        substituted: diq.quality.substituted,
        not_topical: diq.quality.not_topical,
        invalid: diq.quality.invalid,
    }
}

fn dp_to_wire(dp: DoublePoint) -> DoublePointWire {
    match dp {
        DoublePoint::Intermediate => DoublePointWire::Intermediate,
        DoublePoint::Off => DoublePointWire::Off,
        DoublePoint::On => DoublePointWire::On,
        DoublePoint::Indeterminate => DoublePointWire::Indeterminate,
    }
}

// ---------------------------------------------------------------------------
// Daemon state
// ---------------------------------------------------------------------------

/// Pending interrogation collector: keyed by `(ca, qoi-raw-byte)`.
///
/// Each entry holds a sender channel that the event pump pushes decoded points
/// into, plus a signal for `ACTTERM`.
struct InterrogateCollector {
    /// Receives `(PointKind, ioa, PointValue, QualityWire)` tuples.
    tx: mpsc::Sender<(PointKind, u32, PointValue, QualityWire)>,
    /// Set to true when the pump sees `ACTIVATION_TERMINATION`.
    done_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Pending command ACK waiter.
///
/// The event pump pushes `is_negative` once it sees `ACTIVATION_CON` for the
/// right `(type_id, ioa)` pair.
struct CmdWaiter {
    tx: tokio::sync::oneshot::Sender<bool>,
}

struct DaemonState {
    /// Common address configured for this daemon instance.
    coa: u16,
    /// ASDU addressing — IEC-104 fixed; could be extended for 101.
    addressing: AsduAddressing,
    /// Last-value cache.
    cache: PointCache,
    /// In-flight interrogation collectors keyed by `(ca_raw, qoi_raw)`.
    interrogations: HashMap<(u16, u8), InterrogateCollector>,
    /// In-flight command ACK waiters keyed by `(type_id, ioa)`.
    cmd_waiters: HashMap<(u8, u32), CmdWaiter>,
    /// Broadcast sender for the events subscription channel.
    event_tx: broadcast::Sender<Event>,
    /// Shutdown signal — set when Shutdown is requested.
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl DaemonState {
    fn new(coa: u16) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (event_tx, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let state = Self {
            coa,
            addressing: AsduAddressing::IEC104,
            cache: PointCache::new(),
            interrogations: HashMap::new(),
            cmd_waiters: HashMap::new(),
            event_tx,
            shutdown_tx: Some(shutdown_tx),
        };
        (state, shutdown_rx)
    }

    fn broadcast(&self, evt: Event) {
        // Ignore send errors — no active subscribers is normal.
        let _ = self.event_tx.send(evt);
    }
}

// ---------------------------------------------------------------------------
// ASDU event pump
// ---------------------------------------------------------------------------

/// Decode one ASDU and dispatch updates to the cache, interrogation
/// collectors, and command waiters. Returns a list of events to broadcast.
fn process_asdu(asdu: &Asdu, state: &mut DaemonState) -> Vec<Event> {
    let cot = asdu.cot();
    let cause = cot.cause();
    let ca = asdu.ca().0;
    let addressing = state.addressing;
    let type_id = asdu.type_id();
    let mut events: Vec<Event> = Vec::new();

    macro_rules! push_event {
        ($ioa:expr, $value:expr) => {
            events.push(Event::AsduReceived {
                cot: format!("{}", cause.raw()),
                type_id,
                ioa: $ioa,
                value: serde_json::to_value(&$value).unwrap_or(serde_json::Value::Null),
            });
        };
    }

    // `ACTIVATION_TERMINATION` for `C_IC`: signal the interrogation collector.
    if cause == Cause::ACTIVATION_TERMINATION && type_id == C_IC_NA_1::TYPE_ID {
        let keys: Vec<_> = state
            .interrogations
            .keys()
            .filter(|&&(k_ca, _)| k_ca == ca)
            .copied()
            .collect();
        for key in keys {
            if let Some(col) = state.interrogations.get_mut(&key) {
                if let Some(done) = col.done_tx.take() {
                    let _ = done.send(());
                }
            }
        }
        return events;
    }

    // `ACTIVATION_CON` for command types — signal waiter.
    if cause == Cause::ACTIVATION_CON {
        let cmd_type_ids: &[u8] = &[
            C_SC_NA_1::TYPE_ID,
            C_DC_NA_1::TYPE_ID,
            C_RC_NA_1::TYPE_ID,
            C_SE_NA_1::TYPE_ID,
            C_SE_NB_1::TYPE_ID,
            C_SE_NC_1::TYPE_ID,
        ];
        if cmd_type_ids.contains(&type_id) {
            // All command types start with a 3-byte IOA under IEC104 addressing.
            let payload = asdu.payload_bytes();
            if payload.len() >= 3 {
                let ioa_raw = u32::from(payload[0])
                    | (u32::from(payload[1]) << 8)
                    | (u32::from(payload[2]) << 16);
                let is_negative = cot.is_negative();
                if let Some(waiter) = state.cmd_waiters.remove(&(type_id, ioa_raw)) {
                    let _ = waiter.tx.send(is_negative);
                }
            }
            return events;
        }
    }

    // Monitor-direction ASDUs — decode, cache, forward to interrogation.
    match type_id {
        M_SP_NA_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_SP_NA_1>(addressing) {
                for (ioa, siq) in p.objects {
                    let v = PointValue::Single(siq.on);
                    let q = siq_to_wire(siq);
                    state.cache.update(PointKind::SpNa, ioa.0, v.clone(), q, None);
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::SpNa, ioa.0);
                }
            }
        }
        M_DP_NA_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_DP_NA_1>(addressing) {
                for (ioa, diq) in p.objects {
                    let v = PointValue::Double(dp_to_wire(diq.state));
                    let q = diq_to_wire(diq);
                    state.cache.update(PointKind::DpNa, ioa.0, v.clone(), q, None);
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::DpNa, ioa.0);
                }
            }
        }
        M_ME_NA_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_ME_NA_1>(addressing) {
                for (ioa, (nva, qds)) in p.objects {
                    let v = PointValue::Normalized(nva.as_f32());
                    let q = qds_to_wire(qds);
                    state.cache.update(PointKind::MeNa, ioa.0, v.clone(), q, None);
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::MeNa, ioa.0);
                }
            }
        }
        M_ME_NB_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_ME_NB_1>(addressing) {
                for (ioa, (sva, qds)) in p.objects {
                    let v = PointValue::Scaled(sva.0);
                    let q = qds_to_wire(qds);
                    state.cache.update(PointKind::MeNb, ioa.0, v.clone(), q, None);
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::MeNb, ioa.0);
                }
            }
        }
        M_ME_NC_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_ME_NC_1>(addressing) {
                for (ioa, (r32, qds)) in p.objects {
                    let v = PointValue::Float(r32.0);
                    let q = qds_to_wire(qds);
                    state.cache.update(PointKind::MeNc, ioa.0, v.clone(), q, None);
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::MeNc, ioa.0);
                }
            }
        }
        M_SP_TB_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_SP_TB_1>(addressing) {
                for (ioa, (siq, ts)) in p.objects {
                    let v = PointValue::Single(siq.on);
                    let q = siq_to_wire(siq);
                    state
                        .cache
                        .update(PointKind::SpTb, ioa.0, v.clone(), q, Some(ts));
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::SpTb, ioa.0);
                }
            }
        }
        M_DP_TB_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_DP_TB_1>(addressing) {
                for (ioa, (diq, ts)) in p.objects {
                    let v = PointValue::Double(dp_to_wire(diq.state));
                    let q = diq_to_wire(diq);
                    state
                        .cache
                        .update(PointKind::DpTb, ioa.0, v.clone(), q, Some(ts));
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::DpTb, ioa.0);
                }
            }
        }
        M_ME_TD_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_ME_TD_1>(addressing) {
                for (ioa, (nva, qds, ts)) in p.objects {
                    let v = PointValue::Normalized(nva.as_f32());
                    let q = qds_to_wire(qds);
                    state
                        .cache
                        .update(PointKind::MeTd, ioa.0, v.clone(), q, Some(ts));
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::MeTd, ioa.0);
                }
            }
        }
        M_ME_TE_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_ME_TE_1>(addressing) {
                for (ioa, (sva, qds, ts)) in p.objects {
                    let v = PointValue::Scaled(sva.0);
                    let q = qds_to_wire(qds);
                    state
                        .cache
                        .update(PointKind::MeTe, ioa.0, v.clone(), q, Some(ts));
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::MeTe, ioa.0);
                }
            }
        }
        M_ME_TF_1::TYPE_ID => {
            if let Ok(p) = asdu.decode_payload::<M_ME_TF_1>(addressing) {
                for (ioa, (r32, qds, ts)) in p.objects {
                    let v = PointValue::Float(r32.0);
                    let q = qds_to_wire(qds);
                    state
                        .cache
                        .update(PointKind::MeTf, ioa.0, v.clone(), q, Some(ts));
                    push_event!(ioa.0, v);
                    feed_interrogation(state, ca, &v, q, PointKind::MeTf, ioa.0);
                }
            }
        }
        _ => {}
    }

    events
}

/// Push a decoded point to any active interrogation collector for this CA.
fn feed_interrogation(
    state: &mut DaemonState,
    ca: u16,
    value: &PointValue,
    quality: QualityWire,
    kind: PointKind,
    ioa: u32,
) {
    let keys: Vec<_> = state
        .interrogations
        .keys()
        .filter(|&&(k_ca, _)| k_ca == ca)
        .copied()
        .collect();
    for key in keys {
        if let Some(col) = state.interrogations.get(&key) {
            let _ = col.tx.try_send((kind, ioa, value.clone(), quality));
        }
    }
}

// ---------------------------------------------------------------------------
// Connection abstraction
// ---------------------------------------------------------------------------

/// Commands sent to the IEC connection task.
enum ConnCmd {
    SendAsdu(Vec<u8>),
}

/// A handle to send ASDUs over whichever transport is active.
#[derive(Clone)]
struct ConnSender {
    tx: mpsc::Sender<ConnCmd>,
}

impl ConnSender {
    async fn send_asdu(&self, bytes: Vec<u8>) -> Result<()> {
        self.tx
            .send(ConnCmd::SendAsdu(bytes))
            .await
            .map_err(|_| anyhow::anyhow!("connection task gone"))
    }
}

// ---------------------------------------------------------------------------
// Handler implementation
// ---------------------------------------------------------------------------

struct ClientHandler {
    state: Arc<Mutex<DaemonState>>,
    conn: ConnSender,
    files_dir: PathBuf,
    ft_handle: Option<iec60870::file_transfer::FileTransferHandle>,
}

#[async_trait::async_trait]
impl ControlHandler for ClientHandler {
    async fn handle(&self, req: Request) -> Response {
        match req {
            Request::Interrogate {
                group,
                ca,
                timeout_ms,
            } => self.handle_interrogate(group, ca, timeout_ms).await,
            Request::CmdSingle { ioa, on, ca } => self.handle_cmd_single(ioa, on, ca).await,
            Request::CmdDouble { ioa, on, ca } => self.handle_cmd_double(ioa, on, ca).await,
            Request::CmdRegulating { ioa, step, ca } => {
                self.handle_cmd_regulating(ioa, step, ca).await
            }
            Request::CmdSetpoint { ioa, kind, value, ca } => {
                self.handle_cmd_setpoint(ioa, kind, value, ca).await
            }
            Request::Read { ioa, type_id } => self.handle_read(ioa, type_id).await,
            Request::FileGet { nof, out, ca } => self.handle_file_get(nof, out, ca).await,
            Request::FilePut { nof, input, ca } => self.handle_file_put(nof, input, ca).await,
            Request::Status => self.handle_status().await,
            Request::Shutdown => self.handle_shutdown().await,
            // Server-side operations: not applicable on the client.
            Request::Get { .. }
            | Request::Set { .. }
            | Request::List { .. }
            | Request::SimGet { .. }
            | Request::SimSet { .. } => Response::err("not a client op"),
            Request::Events => {
                // Handled by the control::serve loop directly.
                Response::err("internal: events handled by serve loop")
            }
        }
    }

    async fn subscribe_events(&self) -> Option<mpsc::Receiver<Event>> {
        let state = self.state.lock().await;
        let mut bcast_rx = state.event_tx.subscribe();
        drop(state);

        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                match bcast_rx.recv().await {
                    Ok(evt) => {
                        if tx.send(evt).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(n, "event subscriber lagged, dropping events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Some(rx)
    }
}

impl ClientHandler {
    async fn handle_interrogate(
        &self,
        group: Option<u8>,
        ca: Option<u16>,
        timeout_ms: Option<u64>,
    ) -> Response {
        let qoi = group.map_or(Qoi::GENERAL, Qoi::group);
        let ca_val = {
            let st = self.state.lock().await;
            ca.unwrap_or(st.coa)
        };
        let ms = timeout_ms.unwrap_or(5_000);

        // Register an interrogation collector.
        let (pt_tx, mut pt_rx) = mpsc::channel::<(PointKind, u32, PointValue, QualityWire)>(256);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        {
            let mut st = self.state.lock().await;
            st.interrogations.insert(
                (ca_val, qoi.0),
                InterrogateCollector {
                    tx: pt_tx,
                    done_tx: Some(done_tx),
                },
            );
        }

        // Build and send `C_IC_NA_1`.
        let payload = C_IC_NA_1 {
            objects: vec![(Ioa(0), qoi)],
        };
        let bytes = {
            let st = self.state.lock().await;
            Asdu::from_payload(
                Cot::with(Cause::ACTIVATION),
                CommonAddress(ca_val),
                Vsq::single(1),
                &payload,
                st.addressing,
            )
            .encode_to_vec(st.addressing)
        };
        if let Err(e) = self.conn.send_asdu(bytes).await {
            let mut st = self.state.lock().await;
            st.interrogations.remove(&(ca_val, qoi.0));
            return Response::err(format!("send failed: {e}"));
        }

        // Wait for ACTTERM (done_rx) or the overall deadline, whichever comes first.
        // Points arrive concurrently and are buffered in `pt_rx` (capacity 256).
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(ms)) => {}
            _ = done_rx => {}
        }

        // Drain any remaining buffered points.
        let mut collected: Vec<(PointKind, u32, PointValue, QualityWire)> = Vec::new();
        let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        while let Ok(Some(item)) =
            tokio::time::timeout_at(drain_deadline, pt_rx.recv()).await
        {
            collected.push(item);
        }

        // Clean up the collector entry.
        {
            let mut st = self.state.lock().await;
            st.interrogations.remove(&(ca_val, qoi.0));
        }

        // Deduplicate: keep the last entry per (kind, ioa).
        let mut seen: HashMap<(u8, u32), (PointKind, u32, PointValue, QualityWire)> =
            HashMap::new();
        for (kind, ioa, value, quality) in collected {
            seen.insert((kind.type_id(), ioa), (kind, ioa, value, quality));
        }

        let mut points_json: Vec<serde_json::Value> = seen
            .into_values()
            .map(|(kind, ioa, value, quality)| {
                json!({
                    "ioa": ioa,
                    "kind": serde_json::to_value(kind).unwrap_or(serde_json::Value::Null),
                    "value": serde_json::to_value(&value).unwrap_or(serde_json::Value::Null),
                    "quality": serde_json::to_value(quality).unwrap_or(serde_json::Value::Null),
                })
            })
            .collect();
        points_json.sort_by_key(|v| {
            v.get("ioa")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX)
        });
        let count = points_json.len();
        Response::ok(json!({ "points": points_json, "count": count }))
    }

    async fn handle_cmd_single(&self, ioa: u32, on: bool, ca: Option<u16>) -> Response {
        let ca_val = self.resolve_ca(ca).await;
        let payload = C_SC_NA_1 {
            objects: vec![(
                Ioa(ioa),
                Sco {
                    on,
                    qualifier: 0,
                    select: false,
                },
            )],
        };
        self.send_command_and_wait(C_SC_NA_1::TYPE_ID, ioa, ca_val, &payload)
            .await
    }

    async fn handle_cmd_double(&self, ioa: u32, on: bool, ca: Option<u16>) -> Response {
        let ca_val = self.resolve_ca(ca).await;
        let state = if on { DoublePoint::On } else { DoublePoint::Off };
        let payload = C_DC_NA_1 {
            objects: vec![(
                Ioa(ioa),
                Dco {
                    state,
                    qualifier: 0,
                    select: false,
                },
            )],
        };
        self.send_command_and_wait(C_DC_NA_1::TYPE_ID, ioa, ca_val, &payload)
            .await
    }

    async fn handle_cmd_regulating(&self, ioa: u32, step: StepDir, ca: Option<u16>) -> Response {
        let ca_val = self.resolve_ca(ca).await;
        let direction = match step {
            StepDir::Lower => StepDirection::Lower,
            StepDir::Higher => StepDirection::Higher,
        };
        let payload = C_RC_NA_1 {
            objects: vec![(
                Ioa(ioa),
                Rco {
                    direction,
                    qualifier: 0,
                    select: false,
                },
            )],
        };
        self.send_command_and_wait(C_RC_NA_1::TYPE_ID, ioa, ca_val, &payload)
            .await
    }

    async fn handle_cmd_setpoint(
        &self,
        ioa: u32,
        kind: SetpointKind,
        value: f64,
        ca: Option<u16>,
    ) -> Response {
        let ca_val = self.resolve_ca(ca).await;
        let qos = Qos {
            qualifier: 0,
            select: false,
        };
        #[allow(clippy::cast_possible_truncation)]
        match kind {
            SetpointKind::Normalized => {
                let payload = C_SE_NA_1 {
                    objects: vec![(Ioa(ioa), (Nva::from_f32(value as f32), qos))],
                };
                self.send_command_and_wait(C_SE_NA_1::TYPE_ID, ioa, ca_val, &payload)
                    .await
            }
            SetpointKind::Scaled => {
                let payload = C_SE_NB_1 {
                    objects: vec![(Ioa(ioa), (Sva(value as i16), qos))],
                };
                self.send_command_and_wait(C_SE_NB_1::TYPE_ID, ioa, ca_val, &payload)
                    .await
            }
            SetpointKind::Float => {
                let payload = C_SE_NC_1 {
                    objects: vec![(Ioa(ioa), (R32(value as f32), qos))],
                };
                self.send_command_and_wait(C_SE_NC_1::TYPE_ID, ioa, ca_val, &payload)
                    .await
            }
        }
    }

    async fn send_command_and_wait<P: AsduPayload>(
        &self,
        type_id: u8,
        ioa: u32,
        ca: u16,
        payload: &P,
    ) -> Response {
        // Register ACK waiter.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<bool>();
        {
            let mut st = self.state.lock().await;
            st.cmd_waiters
                .insert((type_id, ioa), CmdWaiter { tx: ack_tx });
        }

        let bytes = {
            let st = self.state.lock().await;
            Asdu::from_payload(
                Cot::with(Cause::ACTIVATION),
                CommonAddress(ca),
                Vsq::single(1),
                payload,
                st.addressing,
            )
            .encode_to_vec(st.addressing)
        };

        if let Err(e) = self.conn.send_asdu(bytes).await {
            let mut st = self.state.lock().await;
            st.cmd_waiters.remove(&(type_id, ioa));
            return Response::err(format!("send failed: {e}"));
        }

        match timeout(Duration::from_secs(5), ack_rx).await {
            Ok(Ok(is_negative)) => {
                if is_negative {
                    Response::err("negative ACTIVATION_CON")
                } else {
                    Response::ok(json!({
                        "cot": "activation_con",
                        "negative": false,
                    }))
                }
            }
            Ok(Err(_)) => Response::err("command waiter dropped"),
            Err(_) => {
                let mut st = self.state.lock().await;
                st.cmd_waiters.remove(&(type_id, ioa));
                Response::err("timeout waiting for ACTIVATION_CON")
            }
        }
    }

    async fn handle_read(&self, ioa: u32, type_id: Option<u8>) -> Response {
        let st = self.state.lock().await;
        match st.cache.get_by_ioa(ioa, type_id) {
            Some((kind, pt)) => {
                let age_ms = pt.received_at.elapsed().as_millis();
                let ts_str = pt.timestamp.as_ref().map(|t| {
                    format!(
                        "20{:02}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                        t.year,
                        t.month,
                        t.day,
                        t.hour,
                        t.minute,
                        t.milliseconds / 1000,
                        t.milliseconds % 1000,
                    )
                });
                let value_json =
                    serde_json::to_value(&pt.value).unwrap_or(serde_json::Value::Null);
                let quality_json =
                    serde_json::to_value(pt.quality).unwrap_or(serde_json::Value::Null);
                let kind_json =
                    serde_json::to_value(kind).unwrap_or(serde_json::Value::Null);
                let mut data = json!({
                    "ioa": ioa,
                    "kind": kind_json,
                    "value": value_json,
                    "quality": quality_json,
                    "age_ms": age_ms,
                });
                if let Some(ts) = ts_str {
                    data["timestamp"] = json!(ts);
                }
                Response::ok(data)
            }
            None => Response::err(format!("no cached value for ioa={ioa}")),
        }
    }

    async fn handle_file_get(&self, nof: u16, out: PathBuf, ca: Option<u16>) -> Response {
        let ca_val = self.resolve_ca(ca).await;
        let ft = match &self.ft_handle {
            Some(h) => h.clone(),
            None => {
                return Response::err(
                    "file-transfer not available (daemon not started with file provider)",
                );
            }
        };

        let out_dir = if out.as_os_str().is_empty() {
            self.files_dir.clone()
        } else {
            out
        };

        if let Err(e) = tokio::fs::create_dir_all(&out_dir).await {
            return Response::err(format!("cannot create output directory: {e}"));
        }

        let nof_val = NameOfFile(nof);
        match ft.fetch(CommonAddress(ca_val), nof_val).await {
            Ok(bytes) => {
                let path = out_dir.join(format!("upload_{nof:04X}.bin"));
                Response::ok(json!({
                    "bytes": bytes,
                    "path": path.display().to_string(),
                }))
            }
            Err(e) => Response::err(format!("file fetch failed: {e:?}")),
        }
    }

    async fn handle_file_put(&self, nof: u16, input: PathBuf, ca: Option<u16>) -> Response {
        let ca_val = self.resolve_ca(ca).await;
        let ft = match &self.ft_handle {
            Some(h) => h.clone(),
            None => {
                return Response::err(
                    "file-transfer not available (daemon not started with file provider)",
                );
            }
        };

        // Stage the input file at the path `FsFileTransferProvider` expects.
        let target_name = format!("upload_{nof:04X}.bin");
        let target = self.files_dir.join(&target_name);

        if let Err(e) = tokio::fs::copy(&input, &target).await {
            return Response::err(format!("failed to stage file for push: {e}"));
        }

        // Rescan so the provider picks up the newly staged file.
        let rescan_provider = match FsFileTransferProvider::new(&self.files_dir) {
            Ok(p) => p,
            Err(e) => return Response::err(format!("provider rescan failed: {e:?}")),
        };
        if let Err(e) = rescan_provider.rescan().await {
            return Response::err(format!("rescan failed: {e:?}"));
        }

        let nof_val = NameOfFile(nof);
        match ft.push(CommonAddress(ca_val), nof_val).await {
            Ok(bytes) => Response::ok(json!({ "bytes": bytes })),
            Err(e) => {
                // Best-effort cleanup.
                let _ = tokio::fs::remove_file(&target).await;
                Response::err(format!("file push failed: {e:?}"))
            }
        }
    }

    async fn handle_status(&self) -> Response {
        let st = self.state.lock().await;
        let cached = st.cache.list_all().len();
        drop(st);
        Response::ok(json!({
            "status": "running",
            "cached_points": cached,
        }))
    }

    async fn handle_shutdown(&self) -> Response {
        let mut st = self.state.lock().await;
        if let Some(tx) = st.shutdown_tx.take() {
            let _ = tx.send(());
        }
        Response::ok_empty()
    }

    /// Resolve the common address: use caller-supplied `ca` or fall back to
    /// the daemon's configured `coa`.
    async fn resolve_ca(&self, ca: Option<u16>) -> u16 {
        let st = self.state.lock().await;
        ca.unwrap_or(st.coa)
    }
}

// ---------------------------------------------------------------------------
// Daemon startup
// ---------------------------------------------------------------------------

async fn run_daemon(args: DaemonArgs) -> Result<()> {
    let transport = args.transport.resolve()?;
    let coa = match &transport {
        TransportChoice::Tcp { coa, .. } | TransportChoice::Serial { coa, .. } => *coa,
    };

    tokio::fs::create_dir_all(&args.files_dir)
        .await
        .with_context(|| {
            format!("creating files directory {}", args.files_dir.display())
        })?;

    let (state, mut shutdown_rx) = DaemonState::new(coa);
    let state = Arc::new(Mutex::new(state));

    let (conn_cmd_tx, conn_cmd_rx) = mpsc::channel::<ConnCmd>(64);
    let conn_sender = ConnSender { tx: conn_cmd_tx };
    let (asdu_tx, mut asdu_rx) = mpsc::channel::<(Vec<u8>, AsduAddressing)>(256);

    let ft_handle = connect_transport(
        &transport,
        &args.files_dir,
        conn_cmd_rx,
        asdu_tx,
        state.clone(),
    )
    .await?;

    // Spawn the ASDU dispatch task.
    {
        let state_clone = state.clone();
        tokio::spawn(async move {
            while let Some((bytes, addressing)) = asdu_rx.recv().await {
                let parsed = match Asdu::decode(&mut bytes.as_slice(), addressing) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!(?e, "failed to decode asdu");
                        continue;
                    }
                };
                let mut st = state_clone.lock().await;
                let events = process_asdu(&parsed, &mut st);
                for evt in events {
                    st.broadcast(evt);
                }
            }
        });
    }

    let handler = Arc::new(ClientHandler {
        state: state.clone(),
        conn: conn_sender,
        files_dir: args.files_dir.clone(),
        ft_handle,
    });

    let socket_path = args.control.clone();
    let serve_fut = control::serve(&socket_path, handler);

    tracing::info!(socket = %socket_path.display(), "control socket ready");
    tracing::info!("daemon running — waiting for shutdown");

    tokio::select! {
        res = serve_fut => {
            res.context("control socket serve error")?;
        }
        _ = &mut shutdown_rx => {
            tracing::info!("shutdown signal received");
        }
    }

    Ok(())
}

/// Connect to the configured transport, spawn the event pump task, and return
/// the optional file-transfer handle.
async fn connect_transport(
    transport: &TransportChoice,
    files_dir: &Path,
    conn_cmd_rx: mpsc::Receiver<ConnCmd>,
    asdu_tx: mpsc::Sender<(Vec<u8>, AsduAddressing)>,
    state: Arc<Mutex<DaemonState>>,
) -> Result<Option<iec60870::file_transfer::FileTransferHandle>> {
    match transport {
        TransportChoice::Tcp { addr, .. } => {
            let provider = FsFileTransferProvider::new(files_dir)
                .with_context(|| format!("building file provider at {}", files_dir.display()))?;

            let client = Client104::connect_with_file_provider(
                Transport::tcp(*addr),
                Config104::default(),
                provider,
                DefaultLoggingHandler,
            )
            .await
            .with_context(|| format!("connecting to {addr}"))?;

            let ft = client.file_transfer().cloned();

            tokio::spawn(run_client104(client, conn_cmd_rx, asdu_tx, state));

            Ok(ft)
        }
        TransportChoice::Serial {
            path,
            baud,
            link_addr,
            link_addr_size,
            ..
        } => {
            use iec60870::serial::SerialSettings;

            let serial_settings = SerialSettings {
                baud: *baud,
                ..SerialSettings::default()
            };
            let link_cfg = LinkConfig {
                link_address: LinkAddress(*link_addr),
                addr_size: link_addr_size.to_proto(),
                ..LinkConfig::default()
            };
            let path_str = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid serial path"))?;

            let mut master = Master101::open(path_str, serial_settings, link_cfg)
                .await
                .with_context(|| format!("opening serial port {}", path.display()))?;

            master.reset_link().await.context("reset_link failed")?;
            tracing::info!("reset_link sent, waiting for LinkState::Ready");

            // Wait up to 5 s for Ready.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                match tokio::time::timeout_at(deadline, master.recv()).await {
                    Ok(Some(Master101Event::LinkStateChanged(LinkState::Ready))) => {
                        tracing::info!("link is Ready");
                        break;
                    }
                    Ok(Some(Master101Event::Closed(r))) => {
                        anyhow::bail!("link closed before Ready: {r:?}");
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        anyhow::bail!("timed out waiting for link Ready");
                    }
                }
            }

            tokio::spawn(run_master101(master, conn_cmd_rx, asdu_tx, state));

            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// IEC-104 event pump task
// ---------------------------------------------------------------------------

async fn run_client104(
    mut client: Client104,
    mut cmd_rx: mpsc::Receiver<ConnCmd>,
    asdu_tx: mpsc::Sender<(Vec<u8>, AsduAddressing)>,
    state: Arc<Mutex<DaemonState>>,
) {
    loop {
        tokio::select! {
            evt = client.recv() => {
                match evt {
                    Some(ClientEvent::Asdu(bytes)) => {
                        let _ = asdu_tx.send((bytes, AsduAddressing::IEC104)).await;
                    }
                    Some(ClientEvent::StateChanged(s)) => {
                        let state_str = format!("{s:?}");
                        let st = state.lock().await;
                        st.broadcast(Event::StateChanged { state: state_str });
                    }
                    Some(ClientEvent::Closed(reason)) => {
                        let st = state.lock().await;
                        st.broadcast(Event::Disconnected {
                            reason: reason.map(|r| format!("{r:?}")),
                        });
                        break;
                    }
                    None => break,
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ConnCmd::SendAsdu(bytes)) => {
                        if let Err(e) = client.send_asdu(bytes).await {
                            tracing::warn!(?e, "send_asdu failed");
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IEC-101 event pump + poll task
// ---------------------------------------------------------------------------

async fn run_master101(
    mut master: Master101,
    mut cmd_rx: mpsc::Receiver<ConnCmd>,
    asdu_tx: mpsc::Sender<(Vec<u8>, AsduAddressing)>,
    state: Arc<Mutex<DaemonState>>,
) {
    let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                if let Err(e) = master.request_class1().await {
                    tracing::warn!(?e, "request_class1 failed");
                }
            }
            evt = master.recv() => {
                match evt {
                    Some(Master101Event::Asdu(bytes)) => {
                        let _ = asdu_tx.send((bytes, AsduAddressing::IEC104)).await;
                    }
                    Some(Master101Event::LinkStateChanged(ls)) => {
                        let state_str = format!("{ls:?}");
                        let st = state.lock().await;
                        st.broadcast(Event::StateChanged { state: state_str });
                    }
                    Some(Master101Event::Closed(r)) => {
                        let st = state.lock().await;
                        st.broadcast(Event::Disconnected {
                            reason: r.map(|x| format!("{x:?}")),
                        });
                        break;
                    }
                    None => break,
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ConnCmd::SendAsdu(bytes)) => {
                        if let Err(e) = master.send_asdu(bytes).await {
                            tracing::warn!(?e, "send_asdu (101) failed");
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Short-lived subcommand helpers
// ---------------------------------------------------------------------------

async fn send_request(socket: &Path, req: &Request) -> Result<()> {
    let resp = control::call(socket, req)
        .await
        .with_context(|| format!("calling daemon at {}", socket.display()))?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

fn parse_nof(s: &str) -> Result<u16> {
    if let Some(hex) = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16).context("invalid hex NOF")
    } else {
        s.parse::<u16>().context("invalid NOF")
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info,iec60870_test_tools=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon(args) => run_daemon(args).await,

        Commands::Interrogate {
            group,
            ca,
            timeout_ms,
            socket,
        } => {
            send_request(
                &socket,
                &Request::Interrogate {
                    group,
                    ca,
                    timeout_ms,
                },
            )
            .await
        }

        Commands::Cmd(cmd) => match cmd {
            CmdCommands::Single {
                ioa,
                on,
                ca,
                socket,
            } => send_request(&socket, &Request::CmdSingle { ioa, on, ca }).await,
            CmdCommands::Double {
                ioa,
                on,
                ca,
                socket,
            } => send_request(&socket, &Request::CmdDouble { ioa, on, ca }).await,
            CmdCommands::Regulating {
                ioa,
                step,
                ca,
                socket,
            } => {
                let step = match step {
                    StepDirArg::Lower => StepDir::Lower,
                    StepDirArg::Higher => StepDir::Higher,
                };
                send_request(&socket, &Request::CmdRegulating { ioa, step, ca }).await
            }
            CmdCommands::Setpoint {
                ioa,
                kind,
                value,
                ca,
                socket,
            } => {
                let kind = match kind {
                    SetpointKindArg::Normalized => SetpointKind::Normalized,
                    SetpointKindArg::Scaled => SetpointKind::Scaled,
                    SetpointKindArg::Float => SetpointKind::Float,
                };
                send_request(
                    &socket,
                    &Request::CmdSetpoint {
                        ioa,
                        kind,
                        value,
                        ca,
                    },
                )
                .await
            }
        },

        Commands::Read { ioa, type_id, socket } => {
            send_request(&socket, &Request::Read { ioa, type_id }).await
        }

        Commands::File(fcmd) => match fcmd {
            FileCommands::Get {
                nof,
                out,
                ca,
                socket,
            } => {
                let nof_val = parse_nof(&nof)?;
                let out_path = out.unwrap_or_default();
                send_request(&socket, &Request::FileGet { nof: nof_val, out: out_path, ca })
                    .await
            }
            FileCommands::Put {
                nof,
                input,
                ca,
                socket,
            } => {
                let nof_val = parse_nof(&nof)?;
                send_request(&socket, &Request::FilePut { nof: nof_val, input, ca }).await
            }
        },

        Commands::Events { socket } => {
            control::follow_events(socket.as_path(), |evt| {
                println!(
                    "{}",
                    serde_json::to_string(&evt).unwrap_or_else(|_| "{}".into())
                );
            })
            .await
        }

        Commands::Status { socket } => send_request(&socket, &Request::Status).await,

        Commands::Shutdown { socket } => send_request(&socket, &Request::Shutdown).await,
    }
}

//! IEC 60870-5-104 outstation example with per-point deadband.
//!
//! Binds 0.0.0.0:2404 and, on each connection, spawns:
//!
//! * an **inbound task** that answers `C_IC_NA_1` general interrogation
//!   by sending one measured value per IOA (float, normalized, scaled)
//!   and then calls [`DeadbandTracker::observe`] for each — so the
//!   tracker's baseline reflects what the master just received;
//! * a **simulator task** that drifts those three IOAs every second,
//!   passes each candidate through [`DeadbandTracker::evaluate`], and
//!   only sends a spontaneous ASDU when the decision is
//!   [`EmitDecision::Emit`]. The decision is logged either way, so the
//!   suppress side of the contract is visible even without a connected
//!   master.
//!
//! Run with:
//!
//! ```text
//! RUST_LOG=iec60870=info,server_104_deadband=info \
//!     cargo run --example server_104_deadband
//! ```
//!
//! Then point a master at `127.0.0.1:2404`. Compare the spontaneous
//! ASDUs the master receives against the `decision=Emit` / `Suppress`
//! lines in the outstation log — every Emit, and only those, should
//! reach the wire.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{info, warn};

use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{Nva, Qds, R32, Sva};
use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_ME_NA_1, M_ME_NB_1, M_ME_NC_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame104::Config;
use iec60870::{
    DeadbandPolicy, DeadbandTracker, DefaultLoggingHandler, EmitDecision, IpFilter,
    MonitoredValue, Server104, ServerEvent, ServerSender,
};

const COA: CommonAddress = CommonAddress(1);
const IOA_FLOAT: Ioa = Ioa(200);
const IOA_NORM: Ioa = Ioa(201);
const IOA_SCALED: Ioa = Ioa(202);

/// In-memory process image with one value per demonstrated TypeID plus
/// the deadband tracker that gates spontaneous emissions for all three.
struct Image {
    float_val: f32,
    norm_val: f32,
    scaled_val: i16,
    tracker: DeadbandTracker,
}

impl Image {
    fn new() -> Self {
        let mut tracker = DeadbandTracker::new();
        tracker.set_policy(IOA_FLOAT, DeadbandPolicy::Absolute { delta: 0.5 });
        tracker.set_policy(IOA_NORM, DeadbandPolicy::Absolute { delta: 0.02 });
        tracker.set_policy(
            IOA_SCALED,
            DeadbandPolicy::Percent {
                pct: 5.0,
                floor: 100.0,
            },
        );
        Self {
            float_val: 50.0,
            norm_val: 0.0,
            scaled_val: 1000,
            tracker,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info,server_104_deadband=info".into()),
        )
        .init();

    let bind = (Ipv4Addr::UNSPECIFIED, 2404).into();
    let server =
        Server104::bind_with_security(bind, Config::default(), IpFilter::allow_all()).await?;
    info!(addr = ?server.local_addr()?, "deadband outstation listening");

    loop {
        let conn = server.accept_with(DefaultLoggingHandler).await?;
        info!(peer = ?conn.peer(), "client connected");

        let (sender, mut events) = conn.split();
        let state = Arc::new(Mutex::new(Image::new()));

        // Inbound: answer GI and call observe() after each value.
        let inbound_sender = sender.clone();
        let inbound_state = state.clone();
        tokio::spawn(async move {
            while let Some(evt) = events.recv().await {
                match evt {
                    ServerEvent::Asdu(bytes) => {
                        let parsed = match Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104) {
                            Ok(a) => a,
                            Err(e) => {
                                warn!(?e, "failed to decode incoming asdu");
                                continue;
                            }
                        };
                        if parsed.type_id() == C_IC_NA_1::TYPE_ID {
                            if let Err(e) = respond_to_gi(&inbound_sender, &inbound_state).await {
                                warn!(?e, "GI response failed");
                            }
                        }
                    }
                    ServerEvent::StateChanged(s) => info!(state = ?s, "state changed"),
                    ServerEvent::Closed(reason) => {
                        info!(?reason, "connection closed");
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Simulator: drift values, evaluate(), emit on Emit.
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            let mut tick: u32 = 0;
            loop {
                ticker.tick().await;
                tick = tick.wrapping_add(1);
                if let Err(e) = simulate_tick(&sender, &state, tick).await {
                    info!(?e, "simulator stopping (peer gone)");
                    break;
                }
            }
        });
    }
}

/// Answer a general interrogation: ACK → one ASDU per IOA → ActTerm.
///
/// After each value ASDU goes out, `observe()` refreshes the deadband
/// baseline. The next spontaneous candidate then compares against the
/// freshly-emitted value rather than whatever was stored before GI.
async fn respond_to_gi(
    sender: &ServerSender,
    state: &Arc<Mutex<Image>>,
) -> iec60870::Result<()> {
    sender
        .send(
            Cot::with(Cause::ACTIVATION_CON),
            COA,
            Vsq::single(1),
            &C_IC_NA_1 {
                objects: vec![(Ioa(0), Qoi::GENERAL)],
            },
        )
        .await?;

    let mut img = state.lock().await;
    let qds = Qds::default();

    let float = img.float_val;
    sender
        .send(
            Cot::with(Cause::INTERROGATED_GENERAL),
            COA,
            Vsq::single(1),
            &M_ME_NC_1 {
                objects: vec![(IOA_FLOAT, (R32(float), qds))],
            },
        )
        .await?;
    let _ = img
        .tracker
        .observe(IOA_FLOAT, MonitoredValue::Float(float), qds);

    let norm = img.norm_val;
    sender
        .send(
            Cot::with(Cause::INTERROGATED_GENERAL),
            COA,
            Vsq::single(1),
            &M_ME_NA_1 {
                objects: vec![(IOA_NORM, (Nva::from_f32(norm), qds))],
            },
        )
        .await?;
    let _ = img
        .tracker
        .observe(IOA_NORM, MonitoredValue::Normalized(norm), qds);

    let scaled = img.scaled_val;
    sender
        .send(
            Cot::with(Cause::INTERROGATED_GENERAL),
            COA,
            Vsq::single(1),
            &M_ME_NB_1 {
                objects: vec![(IOA_SCALED, (Sva(scaled), qds))],
            },
        )
        .await?;
    let _ = img
        .tracker
        .observe(IOA_SCALED, MonitoredValue::Scaled(scaled), qds);

    sender
        .send(
            Cot::with(Cause::ACTIVATION_TERMINATION),
            COA,
            Vsq::single(1),
            &C_IC_NA_1 {
                objects: vec![(Ioa(0), Qoi::GENERAL)],
            },
        )
        .await
}

/// One simulator step: advance all three values, then evaluate() each
/// and send a spontaneous ASDU only on Emit. The pattern is deliberately
/// chosen so each IOA produces a visible mix of Suppress (small drift
/// within threshold) and Emit (periodic larger jump that crosses it).
async fn simulate_tick(
    sender: &ServerSender,
    state: &Arc<Mutex<Image>>,
    tick: u32,
) -> iec60870::Result<()> {
    let mut img = state.lock().await;
    let qds = Qds::default();

    // Float: drift by +0.1 each tick, +1.0 jump every 6th tick.
    // Threshold is Absolute 0.5 → drift suppresses, jump emits.
    img.float_val += 0.1;
    if tick % 6 == 0 {
        img.float_val += 1.0;
    }

    // Normalized: small drift, periodic larger nudge; wrap to stay in [0, 1).
    // Threshold is Absolute 0.02 → 0.005 drift suppresses, 0.1 nudge emits.
    img.norm_val = (img.norm_val + 0.005).rem_euclid(1.0);
    if tick % 5 == 0 {
        img.norm_val = (img.norm_val + 0.1).rem_euclid(1.0);
    }

    // Scaled: +1 each tick, +60 every 7th tick.
    // Threshold is Percent 5% of max(|baseline|, 100) ≈ 50 at value ~1000 →
    // +1 suppresses, +60 emits.
    img.scaled_val = img.scaled_val.wrapping_add(1);
    if tick % 7 == 0 {
        img.scaled_val = img.scaled_val.wrapping_add(60);
    }

    let float = img.float_val;
    let norm = img.norm_val;
    let scaled = img.scaled_val;
    let spontaneous = Cot::with(Cause::SPONTANEOUS);

    let decision = decide(&mut img.tracker, IOA_FLOAT, MonitoredValue::Float(float), qds);
    info!(ioa = ?IOA_FLOAT, value = float, ?decision, "tick");
    if matches!(decision, EmitDecision::Emit) {
        sender
            .send(
                spontaneous,
                COA,
                Vsq::single(1),
                &M_ME_NC_1 {
                    objects: vec![(IOA_FLOAT, (R32(float), qds))],
                },
            )
            .await?;
    }

    let decision = decide(
        &mut img.tracker,
        IOA_NORM,
        MonitoredValue::Normalized(norm),
        qds,
    );
    info!(ioa = ?IOA_NORM, value = norm, ?decision, "tick");
    if matches!(decision, EmitDecision::Emit) {
        sender
            .send(
                spontaneous,
                COA,
                Vsq::single(1),
                &M_ME_NA_1 {
                    objects: vec![(IOA_NORM, (Nva::from_f32(norm), qds))],
                },
            )
            .await?;
    }

    let decision = decide(
        &mut img.tracker,
        IOA_SCALED,
        MonitoredValue::Scaled(scaled),
        qds,
    );
    info!(ioa = ?IOA_SCALED, value = scaled, ?decision, "tick");
    if matches!(decision, EmitDecision::Emit) {
        sender
            .send(
                spontaneous,
                COA,
                Vsq::single(1),
                &M_ME_NB_1 {
                    objects: vec![(IOA_SCALED, (Sva(scaled), qds))],
                },
            )
            .await?;
    }

    Ok(())
}

/// Thin wrapper that converts a tracker error into Suppress and logs it.
/// Kind mismatches are programmer bugs — in a real outstation you'd
/// propagate them; here we keep the simulator running so the example
/// stays online.
fn decide(
    tracker: &mut DeadbandTracker,
    ioa: Ioa,
    value: MonitoredValue,
    qds: Qds,
) -> EmitDecision {
    match tracker.evaluate(ioa, value, qds) {
        Ok(d) => d,
        Err(e) => {
            warn!(?e, ?ioa, "deadband evaluate error");
            EmitDecision::Suppress
        }
    }
}

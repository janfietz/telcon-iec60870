//! Shared driver task — owns the state machine + a transport stream and
//! shuttles APDUs in both directions. Used by both the client and server
//! flavours of the public API.

use std::time::Duration;

use bytes::{Buf, BytesMut};
use iec60870_proto::asdu::{Asdu, AsduAddressing, CommonAddress};
use iec60870_proto::frame104::{
    Action, Codec, Config, Connection, DisconnectReason, Input, Role, State,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use crate::error::{Error, Result};
use crate::handler::EventHandler;
use crate::policy::AsduPolicy;

/// Upper bound on bytes buffered while waiting for one IEC 60870-5-104 APDU
/// to be fully received.
///
/// The APDU itself is at most 255 octets (`0x68 length-byte body` with the
/// length byte ≤ 253). 4 KiB is comfortably above that, so a hostile peer
/// that dribbles header bytes without ever finishing a frame is detected
/// quickly and disconnected.
const MAX_RX_BUFFERED: usize = 4 * 1024;

/// Commands the user-facing handle sends to the driver task.
#[derive(Debug)]
pub(crate) enum Command {
    SendAsdu(Vec<u8>),
    StartDt,
    StopDt,
}

/// Inbound events the driver pushes to the user-facing handle.
#[derive(Debug)]
pub(crate) enum DriverEvent {
    Asdu(Vec<u8>),
    StateChanged(State),
    Closed(Option<DisconnectReason>),
}

/// Run the driver until the underlying stream closes or the state machine
/// asks for a disconnect. `cmd_rx` receives user commands; `evt_tx` carries
/// inbound ASDUs and lifecycle events back out.
///
/// When `ft_tx` is `Some`, ASDUs whose Type ID is in the file-transfer range
/// (120..=126) are routed to it instead of `evt_tx` — this lets the
/// file-transfer service consume them without polluting the user-facing
/// event stream.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run<S, H>(
    stream: S,
    role: Role,
    config: Config,
    policy: AsduPolicy,
    handler: H,
    mut cmd_rx: mpsc::Receiver<Command>,
    evt_tx: mpsc::Sender<DriverEvent>,
    ft_tx: Option<mpsc::Sender<(CommonAddress, Asdu)>>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    H: EventHandler,
{
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let mut conn = Connection::new(role, config);
    let mut rx_buf = BytesMut::with_capacity(2048);
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Server side: nothing to do up-front. Client side: the application
    // initiates StartDt explicitly via the handle.

    loop {
        tokio::select! {
            biased;

            // Outbound user commands
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                let now = std::time::Instant::now();
                let actions = match cmd {
                    Command::SendAsdu(asdu) => conn.handle(Input::SendAsdu(asdu), now),
                    Command::StartDt => conn.handle(Input::StartDt, now),
                    Command::StopDt => conn.handle(Input::StopDt, now),
                };
                if !apply_actions(actions, &policy, &handler, &mut write_half, &evt_tx, &ft_tx).await? {
                    return Ok(());
                }
            }

            // Periodic timer kick
            _ = tick.tick() => {
                let now = std::time::Instant::now();
                let actions = conn.handle(Input::Tick, now);
                if !apply_actions(actions, &policy, &handler, &mut write_half, &evt_tx, &ft_tx).await? {
                    return Ok(());
                }
            }

            // Inbound bytes from the peer
            res = read_half.read_buf(&mut rx_buf) => {
                let n = res?;
                if n == 0 {
                    let _ = evt_tx.send(DriverEvent::Closed(None)).await;
                    return Ok(());
                }
                while let Some((apdu, consumed)) = Codec::decode_slice(&rx_buf)? {
                    rx_buf.advance(consumed);
                    handler.on_frame_received(&apdu);
                    let now = std::time::Instant::now();
                    let actions = conn.handle(Input::Apdu(apdu), now);
                    if !apply_actions(actions, &policy, &handler, &mut write_half, &evt_tx, &ft_tx).await? {
                        return Ok(());
                    }
                }
                // After draining complete frames, anything still buffered is
                // an in-flight prefix. A peer that holds the buffer above the
                // cap (without ever finishing a frame) is either broken or
                // malicious; disconnect rather than keep growing.
                if rx_buf.len() > MAX_RX_BUFFERED {
                    handler.on_protocol_error(&format!(
                        "rx buffer exceeded {MAX_RX_BUFFERED} bytes ({} buffered) without a complete APDU",
                        rx_buf.len()
                    ));
                    let _ = evt_tx.send(DriverEvent::Closed(None)).await;
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "iec104 rx buffer overflow without a complete APDU",
                    )));
                }
            }
        }

        // Workaround for the deadline of tokio::time::interval not advancing
        // when no I/O has happened; explicitly mark the wall-clock baseline.
        let _ = TokioInstant::now();
    }
    Ok(())
}

/// Quick policy-only header peek for IEC-60870-5-104-addressed ASDUs.
///
/// Layout: `TypeID(1) | VSQ(1) | COT lo,hi(2) | CA lo,hi(2) | …`. Returns
/// `true` (= allow) when the buffer is too short to even read the header,
/// so the underlying ASDU decoder (called by user code) produces the
/// meaningful error instead of the policy filter silently dropping a
/// malformed frame.
fn asdu_passes_policy(asdu: &[u8], policy: &AsduPolicy) -> bool {
    if asdu.len() < 6 {
        return true;
    }
    let type_id = asdu[0];
    let cause_raw = asdu[2] & 0x3F;
    let ca = u16::from_le_bytes([asdu[4], asdu[5]]);
    policy.allows(type_id, cause_raw, ca)
}

/// Apply the action list. Returns `Ok(false)` if the driver should exit.
async fn apply_actions<W, H>(
    actions: Vec<Action>,
    policy: &AsduPolicy,
    handler: &H,
    write: &mut W,
    evt_tx: &mpsc::Sender<DriverEvent>,
    ft_tx: &Option<mpsc::Sender<(CommonAddress, Asdu)>>,
) -> Result<bool>
where
    W: AsyncWrite + Unpin,
    H: EventHandler,
{
    let mut keep_going = true;
    for action in actions {
        match action {
            Action::SendApdu(apdu) => {
                handler.on_frame_sent(&apdu);
                let mut buf = BytesMut::new();
                Codec::encode(&apdu, &mut buf)?;
                write.write_all(&buf).await?;
                write.flush().await?;
            }
            Action::DeliverAsdu(asdu) => {
                if policy.is_restrictive() && !asdu_passes_policy(&asdu, policy) {
                    tracing::warn!(
                        target: "iec60870::policy",
                        len = asdu.len(),
                        "incoming asdu rejected by AsduPolicy",
                    );
                    continue;
                }
                handler.on_asdu_received(&asdu);
                // Route file-transfer ASDUs (Type ID 120..=126) to the FT
                // service when one is wired up; otherwise fall through to
                // the user event channel.
                if let Some(tx) = ft_tx {
                    if matches!(asdu.first(), Some(120..=126)) {
                        let mut slice: &[u8] = &asdu;
                        if let Ok(parsed) = Asdu::decode(&mut slice, AsduAddressing::IEC104) {
                            let ca = parsed.ca();
                            let _ = tx.send((ca, parsed)).await;
                        }
                        continue;
                    }
                }
                if evt_tx.send(DriverEvent::Asdu(asdu)).await.is_err() {
                    keep_going = false;
                }
            }
            Action::StateChanged(state) => {
                handler.on_state_changed(state);
                if evt_tx.send(DriverEvent::StateChanged(state)).await.is_err() {
                    keep_going = false;
                }
            }
            Action::Disconnect(reason) => {
                handler.on_protocol_error(&format!("disconnect: {reason:?}"));
                let _ = evt_tx.send(DriverEvent::Closed(Some(reason))).await;
                keep_going = false;
            }
        }
    }
    Ok(keep_going)
}

//! Async driver task for IEC 60870-5-101 (FT 1.2 framing).
//!
//! Mirrors the structure of [`crate::driver`] for IEC 60870-5-104, adapted
//! for the FT 1.2 link layer. The driver is generic over any stream that
//! implements `AsyncRead + AsyncWrite + Unpin` so it works with
//! [`tokio_serial::SerialStream`], plain TCP (useful for tests), and
//! [`tokio::io::DuplexStream`] used by the integration tests.
//!
//! ## EventHandler choice
//!
//! Option C was chosen: the existing [`crate::handler::EventHandler`] trait
//! is left 104-only. The 101 driver logs all frame events via `tracing` at
//! appropriate levels internally. Adding 101 hooks to `EventHandler` would
//! introduce a 104-specific type (`Apdu`) and a 101-specific type (`Frame101`)
//! into the same trait, muddying the surface. A separate trait would be clean
//! but adds API surface with no current user. Logging via tracing is
//! sufficient for observability and keeps the public API surface minimal.

use std::time::Duration;

use bytes::{Buf, BytesMut};
use iec60870_proto::frame101::codec::Codec;
use iec60870_proto::frame101::link::{Action, Config, Connection, Input, LinkState, Mode, Role};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::error::Result;

/// Commands the user-facing handle sends to the driver task.
#[derive(Debug)]
pub(crate) enum Command101 {
    /// Send an ASDU as a USER_DATA_CONFIRMED frame.
    SendAsdu(Vec<u8>),
    /// Issue a RESET_REMOTE_LINK request (master only).
    ResetLink,
    /// Request class-1 data from the outstation (master only).
    RequestClass1,
    /// Request class-2 data from the outstation (master only).
    RequestClass2,
}

/// Inbound events the driver pushes to the user-facing handle.
#[derive(Debug)]
pub(crate) enum DriverEvent101 {
    /// An ASDU was delivered from the link layer.
    Asdu(Vec<u8>),
    /// The link state changed.
    LinkStateChanged(LinkState),
    /// The link closed (with optional error reason).
    Closed(Option<iec60870_proto::frame101::link::Reason>),
}

/// Run the 101 driver until the stream closes or the link layer declares
/// failure. `cmd_rx` receives user commands; `evt_tx` carries inbound ASDUs
/// and lifecycle events back out.
///
/// The `addr_size` parameter must match the system-wide link-address width
/// configured on both ends.
pub(crate) async fn run<S>(
    stream: S,
    role: Role,
    config: Config,
    mut cmd_rx: mpsc::Receiver<Command101>,
    evt_tx: mpsc::Sender<DriverEvent101>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let addr_size = config.addr_size;
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let mut conn = Connection::new(role, Mode::Unbalanced, config);
    let mut rx_buf = BytesMut::with_capacity(2048);

    // Tick every 250 ms — well below the default 1 s ACK timeout so the
    // timer fires promptly on the first missed beat.
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            // Outbound user commands
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                let now = std::time::Instant::now();
                let actions = match cmd {
                    Command101::SendAsdu(asdu) => conn.handle(Input::SendUserData(asdu), now),
                    Command101::ResetLink      => conn.handle(Input::ResetRemoteLink, now),
                    Command101::RequestClass1  => conn.handle(Input::RequestUserDataClass1, now),
                    Command101::RequestClass2  => conn.handle(Input::RequestUserDataClass2, now),
                };
                if !apply_actions101(actions, addr_size, &mut write_half, &evt_tx).await? {
                    return Ok(());
                }
            }

            // Periodic timer kick
            _ = tick.tick() => {
                let now = std::time::Instant::now();
                let actions = conn.handle(Input::Tick, now);
                if !apply_actions101(actions, addr_size, &mut write_half, &evt_tx).await? {
                    return Ok(());
                }
            }

            // Inbound bytes from the peer
            res = read_half.read_buf(&mut rx_buf) => {
                let n = res?;
                if n == 0 {
                    let _ = evt_tx.send(DriverEvent101::Closed(None)).await;
                    return Ok(());
                }
                loop {
                    match Codec::decode_slice(&rx_buf, addr_size) {
                        Ok(Some((frame, consumed))) => {
                            rx_buf.advance(consumed);
                            tracing::trace!(target: "iec60870_101::rx", ?frame, "frame received");
                            let now = std::time::Instant::now();
                            let actions = conn.handle(Input::FrameReceived(frame), now);
                            if !apply_actions101(actions, addr_size, &mut write_half, &evt_tx).await? {
                                return Ok(());
                            }
                        }
                        Ok(None) => break, // need more bytes
                        Err(e) => {
                            tracing::warn!(target: "iec60870_101::rx", err = %e, "frame decode error; discarding buffer");
                            // Discard the offending byte and try to resync.
                            if !rx_buf.is_empty() {
                                rx_buf.advance(1);
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Apply the action list. Returns `Ok(false)` if the driver should exit.
async fn apply_actions101<W>(
    actions: Vec<Action>,
    addr_size: iec60870_proto::frame101::LinkAddressSize,
    write: &mut W,
    evt_tx: &mpsc::Sender<DriverEvent101>,
) -> Result<bool>
where
    W: AsyncWrite + Unpin,
{
    let mut keep_going = true;
    for action in actions {
        match action {
            Action::SendFrame(frame) => {
                tracing::trace!(target: "iec60870_101::tx", ?frame, "sending frame");
                let mut buf = BytesMut::new();
                Codec::encode(&frame, &mut buf, addr_size);
                write.write_all(&buf).await?;
                write.flush().await?;
            }
            Action::DeliverAsdu(asdu) => {
                tracing::debug!(target: "iec60870_101::asdu", len = asdu.len(), "asdu delivered");
                if evt_tx.send(DriverEvent101::Asdu(asdu)).await.is_err() {
                    keep_going = false;
                }
            }
            Action::LinkStateChanged(state) => {
                tracing::info!(target: "iec60870_101::state", ?state, "link state changed");
                if evt_tx
                    .send(DriverEvent101::LinkStateChanged(state))
                    .await
                    .is_err()
                {
                    keep_going = false;
                }
            }
            Action::LinkError(reason) => {
                tracing::warn!(target: "iec60870_101::error", ?reason, "link error");
                let _ = evt_tx.send(DriverEvent101::Closed(Some(reason))).await;
                keep_going = false;
            }
        }
    }
    Ok(keep_going)
}

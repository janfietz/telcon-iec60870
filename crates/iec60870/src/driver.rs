//! Shared driver task — owns the state machine + a transport stream and
//! shuttles APDUs in both directions. Used by both the client and server
//! flavours of the public API.

use std::time::Duration;

use bytes::{Buf, BytesMut};
use iec60870_proto::frame104::{
    Action, Codec, Config, Connection, DisconnectReason, Input, Role, State,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant as TokioInstant;

use crate::error::Result;
use crate::handler::EventHandler;

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
pub(crate) async fn run<S, H>(
    stream: S,
    role: Role,
    config: Config,
    handler: H,
    mut cmd_rx: mpsc::Receiver<Command>,
    evt_tx: mpsc::Sender<DriverEvent>,
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
                if !apply_actions(actions, &handler, &mut write_half, &evt_tx).await? {
                    return Ok(());
                }
            }

            // Periodic timer kick
            _ = tick.tick() => {
                let now = std::time::Instant::now();
                let actions = conn.handle(Input::Tick, now);
                if !apply_actions(actions, &handler, &mut write_half, &evt_tx).await? {
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
                    if !apply_actions(actions, &handler, &mut write_half, &evt_tx).await? {
                        return Ok(());
                    }
                }
            }
        }

        // Workaround for the deadline of tokio::time::interval not advancing
        // when no I/O has happened; explicitly mark the wall-clock baseline.
        let _ = TokioInstant::now();
    }
    Ok(())
}

/// Apply the action list. Returns `Ok(false)` if the driver should exit.
async fn apply_actions<W, H>(
    actions: Vec<Action>,
    handler: &H,
    write: &mut W,
    evt_tx: &mpsc::Sender<DriverEvent>,
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
                handler.on_asdu_received(&asdu);
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

//! Async IEC 60870-5-104 client.

use std::time::Duration;

use iec60870_proto::frame104::apdu::MAX_ASDU_LEN;
use iec60870_proto::frame104::{Config, DisconnectReason, Role, State};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::driver::{self, Command, DriverEvent};
use crate::error::{Error, Result};
use crate::handler::{DefaultLoggingHandler, EventHandler};
use crate::transport::Transport;

/// IEC 60870-5-104 client over TCP (and optional TLS).
///
/// The client owns an internal task that drives the connection. Drop the
/// `Client104` to terminate it.
#[derive(Debug)]
pub struct Client104 {
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<DriverEvent>,
    _task: tokio::task::JoinHandle<Result<()>>,
}

impl Client104 {
    /// Connect using the default logging handler.
    pub async fn connect(transport: Transport, config: Config) -> Result<Self> {
        Self::connect_with(transport, config, DefaultLoggingHandler).await
    }

    /// Connect with a custom event handler.
    pub async fn connect_with<H: EventHandler>(
        transport: Transport,
        config: Config,
        handler: H,
    ) -> Result<Self> {
        let stream = open_stream(&transport).await?;
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (evt_tx, evt_rx) = mpsc::channel(64);
        let task = tokio::spawn(driver::run(
            stream,
            Role::Client,
            config,
            handler,
            cmd_rx,
            evt_tx,
        ));
        // Issue STARTDT automatically; the typical caller wants data
        // transfer enabled as soon as the connection is up.
        cmd_tx
            .send(Command::StartDt)
            .await
            .map_err(|_| Error::DriverGone)?;
        Ok(Self {
            cmd_tx,
            evt_rx,
            _task: task,
        })
    }

    /// Send an ASDU (raw header + info-objects bytes) over the connection.
    /// Returns [`iec60870_proto::Error::AsduTooLong`] (wrapped in
    /// [`Error::Protocol`]) if the payload exceeds the wire-format cap of
    /// [`MAX_ASDU_LEN`] bytes — the IEC 60870-5-104 APDU length octet can
    /// not represent anything larger.
    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        if asdu.len() > MAX_ASDU_LEN {
            return Err(iec60870_proto::Error::AsduTooLong {
                len: asdu.len(),
                max: MAX_ASDU_LEN,
            }
            .into());
        }
        self.cmd_tx
            .send(Command::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Receive the next event from the connection. Returns `None` when the
    /// driver has shut down.
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        match self.evt_rx.recv().await? {
            DriverEvent::Asdu(bytes) => Some(ClientEvent::Asdu(bytes)),
            DriverEvent::StateChanged(state) => Some(ClientEvent::StateChanged(state)),
            DriverEvent::Closed(reason) => Some(ClientEvent::Closed(reason)),
        }
    }

    /// Convenience helper: drain events until the next ASDU arrives.
    pub async fn recv_asdu(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.recv().await? {
                ClientEvent::Asdu(bytes) => return Some(bytes),
                ClientEvent::Closed(_) => return None,
                _ => continue,
            }
        }
    }

    /// Issue a STOPDT_act. The connection will close once outstanding
    /// frames have been acknowledged.
    pub async fn stop(&self) -> Result<()> {
        self.cmd_tx
            .send(Command::StopDt)
            .await
            .map_err(|_| Error::DriverGone)
    }
}

/// Events surfaced to the application by [`Client104::recv`].
#[derive(Debug, Clone)]
pub enum ClientEvent {
    Asdu(Vec<u8>),
    StateChanged(State),
    Closed(Option<DisconnectReason>),
}

async fn open_stream(transport: &Transport) -> Result<TcpStream> {
    match transport {
        Transport::Tcp { addr } => {
            let stream = tokio::time::timeout(Duration::from_secs(30), TcpStream::connect(addr))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")
                })??;
            stream.set_nodelay(true)?;
            Ok(stream)
        }
        #[cfg(feature = "tls")]
        Transport::Tls { .. } => {
            // Implemented in tls.rs; the public connect_with path forwards
            // through Transport::Tls and is handled by a separate code path.
            unreachable!("Tls is handled by the tls module")
        }
    }
}

//! Async IEC 60870-5-104 server.

use std::net::SocketAddr;

use iec60870_proto::frame104::{Config, DisconnectReason, Role, State};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::driver::{self, Command, DriverEvent};
use crate::error::{Error, Result};
use crate::handler::{DefaultLoggingHandler, EventHandler};

/// IEC 60870-5-104 server. Bind a port, then `accept().await` returns a
/// [`ServerConnection`] for each new peer.
#[derive(Debug)]
pub struct Server104 {
    listener: TcpListener,
    config: Config,
}

impl Server104 {
    pub async fn bind(addr: SocketAddr, config: Config) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener, config })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept one connection. Spawns the driver task internally.
    pub async fn accept(&self) -> Result<ServerConnection> {
        self.accept_with(DefaultLoggingHandler).await
    }

    pub async fn accept_with<H: EventHandler>(&self, handler: H) -> Result<ServerConnection> {
        let (stream, peer) = self.listener.accept().await?;
        stream.set_nodelay(true)?;
        Ok(spawn_connection(stream, peer, self.config, handler))
    }
}

#[derive(Debug)]
pub struct ServerConnection {
    peer: SocketAddr,
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<DriverEvent>,
    _task: tokio::task::JoinHandle<Result<()>>,
}

impl ServerConnection {
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(Command::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }

    pub async fn recv(&mut self) -> Option<ServerEvent> {
        match self.evt_rx.recv().await? {
            DriverEvent::Asdu(bytes) => Some(ServerEvent::Asdu(bytes)),
            DriverEvent::StateChanged(state) => Some(ServerEvent::StateChanged(state)),
            DriverEvent::Closed(reason) => Some(ServerEvent::Closed(reason)),
        }
    }

    pub async fn recv_asdu(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.recv().await? {
                ServerEvent::Asdu(bytes) => return Some(bytes),
                ServerEvent::Closed(_) => return None,
                _ => continue,
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum ServerEvent {
    Asdu(Vec<u8>),
    StateChanged(State),
    Closed(Option<DisconnectReason>),
}

pub(crate) fn spawn_connection<H: EventHandler>(
    stream: TcpStream,
    peer: SocketAddr,
    config: Config,
    handler: H,
) -> ServerConnection {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (evt_tx, evt_rx) = mpsc::channel(64);
    let task = tokio::spawn(driver::run(
        stream,
        Role::Server,
        config,
        handler,
        cmd_rx,
        evt_tx,
    ));
    ServerConnection {
        peer,
        cmd_tx,
        evt_rx,
        _task: task,
    }
}

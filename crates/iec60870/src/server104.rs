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

/// A single connected peer.
///
/// `ServerConnection` is a thin facade over two halves: a cloneable
/// [`ServerSender`] for outbound ASDUs and a [`ServerEvents`] event consumer.
/// Use [`ServerConnection::split`] when you need to fan out events while
/// sending into the same connection from multiple call sites — that's the
/// shape [`RedundancyServer`](crate::RedundancyServer) is built on.
#[derive(Debug)]
pub struct ServerConnection {
    sender: ServerSender,
    events: ServerEvents,
}

/// Cloneable send-side handle. Holding any clone keeps the underlying driver
/// task alive; dropping the last clone closes the TCP connection cleanly.
#[derive(Debug, Clone)]
pub struct ServerSender {
    peer: SocketAddr,
    cmd_tx: mpsc::Sender<Command>,
}

/// Exclusive event-receive half. Yields [`ServerEvent`]s in the order the
/// driver task emitted them, returning `None` once the driver has exited.
#[derive(Debug)]
pub struct ServerEvents {
    peer: SocketAddr,
    evt_rx: mpsc::Receiver<DriverEvent>,
}

impl ServerConnection {
    pub fn peer(&self) -> SocketAddr {
        self.sender.peer
    }

    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.sender.send_asdu(asdu).await
    }

    pub async fn recv(&mut self) -> Option<ServerEvent> {
        self.events.recv().await
    }

    pub async fn recv_asdu(&mut self) -> Option<Vec<u8>> {
        self.events.recv_asdu().await
    }

    /// Decompose into independent send and event halves. The cloneable
    /// sender can be shared across tasks while a single owner drives the
    /// event stream.
    pub fn split(self) -> (ServerSender, ServerEvents) {
        (self.sender, self.events)
    }
}

impl ServerSender {
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(Command::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }
}

impl ServerEvents {
    pub fn peer(&self) -> SocketAddr {
        self.peer
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
    // Driver task is detached; it exits when either channel side closes,
    // which keeps connection teardown driven by the user-facing handles.
    tokio::spawn(driver::run(
        stream,
        Role::Server,
        config,
        handler,
        cmd_rx,
        evt_tx,
    ));
    ServerConnection {
        sender: ServerSender { peer, cmd_tx },
        events: ServerEvents { peer, evt_rx },
    }
}

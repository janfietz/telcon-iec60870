//! TLS support via `tokio-rustls`. Available behind the `tls` cargo feature.
//!
//! IEC 62351-3 recommends TLS for IEC 60870-5-104 over public networks. The
//! conventional port is 19998 (see [`crate::DEFAULT_TLS_PORT`]).

use std::net::SocketAddr;
use std::sync::Arc;

use iec60870_proto::frame104::{Config, Role};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::driver::{self, Command, DriverEvent};
use crate::error::{Error, Result};
use crate::handler::{DefaultLoggingHandler, EventHandler};
use crate::server104::ServerEvent;
use crate::transport::Transport;

/// Re-export of the underlying `rustls::ClientConfig` for convenience.
pub type TlsConfig = Arc<ClientConfig>;

/// Build a `ClientConfig` that trusts the given list of root certificate
/// authorities. This is sufficient for most internal-CA-signed peers.
pub fn client_config_with_roots(roots: RootCertStore) -> TlsConfig {
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Connect an IEC 60870-5-104 client over TLS. Behaves like
/// [`Client104::connect_with`] but expects a [`Transport::Tls`] variant.
pub async fn tls_client_connect<H: EventHandler>(
    transport: Transport,
    config: Config,
    handler: H,
) -> Result<TlsClient> {
    let Transport::Tls {
        addr,
        server_name,
        client_config,
    } = transport
    else {
        return Err(Error::Tls("transport is not TLS".into()));
    };
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    let connector = TlsConnector::from(client_config);
    let name = ServerName::try_from(server_name.clone())
        .map_err(|e| Error::Tls(format!("invalid server name: {e}")))?;
    let stream = connector
        .connect(name, stream)
        .await
        .map_err(|e| Error::Tls(format!("tls handshake: {e}")))?;

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
    cmd_tx
        .send(Command::StartDt)
        .await
        .map_err(|_| Error::DriverGone)?;
    Ok(TlsClient {
        cmd_tx,
        evt_rx,
        _task: task,
    })
}

/// TLS-secured IEC 60870-5-104 client handle.
#[derive(Debug)]
pub struct TlsClient {
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<DriverEvent>,
    _task: tokio::task::JoinHandle<Result<()>>,
}

impl TlsClient {
    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(Command::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }

    pub async fn recv(&mut self) -> Option<crate::client104::ClientEvent> {
        match self.evt_rx.recv().await? {
            DriverEvent::Asdu(bytes) => Some(crate::client104::ClientEvent::Asdu(bytes)),
            DriverEvent::StateChanged(state) => {
                Some(crate::client104::ClientEvent::StateChanged(state))
            }
            DriverEvent::Closed(reason) => Some(crate::client104::ClientEvent::Closed(reason)),
        }
    }

    pub async fn stop(&self) -> Result<()> {
        self.cmd_tx
            .send(Command::StopDt)
            .await
            .map_err(|_| Error::DriverGone)
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Accept one inbound TLS connection, perform the handshake, and spawn the
/// IEC 60870-5-104 driver task. Returns a [`TlsServerConnection`] handle.
///
/// Uses [`DefaultLoggingHandler`]; call [`tls_server_accept_with`] for a
/// custom handler.
pub async fn tls_server_accept(
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    config: Config,
) -> Result<TlsServerConnection> {
    tls_server_accept_with(stream, peer, acceptor, config, DefaultLoggingHandler).await
}

/// Accept one inbound TLS connection with a custom event handler.
///
/// Performs the TLS handshake, then spawns `driver::run` with the resulting
/// `TlsStream<TcpStream>` as the transport and `Role::Server`.
pub async fn tls_server_accept_with<H: EventHandler>(
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    config: Config,
    handler: H,
) -> Result<TlsServerConnection> {
    stream.set_nodelay(true)?;
    let tls_stream = acceptor
        .accept(stream)
        .await
        .map_err(|e| Error::Tls(format!("tls accept: {e}")))?;

    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (evt_tx, evt_rx) = mpsc::channel(64);
    let task = tokio::spawn(driver::run(
        tls_stream,
        Role::Server,
        config,
        handler,
        cmd_rx,
        evt_tx,
    ));
    Ok(TlsServerConnection {
        peer,
        cmd_tx,
        evt_rx,
        _task: task,
    })
}

/// Handle to a TLS-secured inbound IEC 60870-5-104 server connection.
///
/// Has the same interface as [`crate::ServerConnection`] so the same
/// application logic can handle both plain-TCP and TLS connections.
#[derive(Debug)]
pub struct TlsServerConnection {
    peer: SocketAddr,
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<DriverEvent>,
    _task: tokio::task::JoinHandle<Result<()>>,
}

impl TlsServerConnection {
    /// Remote address of the connected peer.
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Send an ASDU (raw header + info-objects bytes) to the peer.
    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(Command::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Receive the next event from the connection. Returns `None` when the
    /// driver has shut down.
    pub async fn recv(&mut self) -> Option<ServerEvent> {
        match self.evt_rx.recv().await? {
            DriverEvent::Asdu(bytes) => Some(ServerEvent::Asdu(bytes)),
            DriverEvent::StateChanged(state) => Some(ServerEvent::StateChanged(state)),
            DriverEvent::Closed(reason) => Some(ServerEvent::Closed(reason)),
        }
    }

    /// Convenience helper: drain events until the next ASDU arrives.
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

// ---------------------------------------------------------------------------
// TlsServer convenience wrapper
// ---------------------------------------------------------------------------

/// Convenience wrapper that holds a [`TcpListener`], a shared
/// [`rustls::ServerConfig`], and an IEC 60870-5-104 [`Config`]. Exposes
/// `bind` + `accept_with` so callers don't have to manage the listener and
/// acceptor separately.
pub struct TlsServer {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    config: Config,
}

impl std::fmt::Debug for TlsServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsServer")
            .field("listener", &self.listener)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TlsServer {
    /// Bind a `TlsServer` on `addr` using the given rustls `ServerConfig`.
    pub async fn bind(
        addr: SocketAddr,
        server_config: Arc<ServerConfig>,
        config: Config,
    ) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let acceptor = TlsAcceptor::from(server_config);
        Ok(Self {
            listener,
            acceptor,
            config,
        })
    }

    /// Return the local socket address the listener is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept one connection using the [`DefaultLoggingHandler`].
    pub async fn accept(&self) -> Result<TlsServerConnection> {
        self.accept_with(DefaultLoggingHandler).await
    }

    /// Accept one connection with a custom event handler.
    pub async fn accept_with<H: EventHandler>(&self, handler: H) -> Result<TlsServerConnection> {
        let (stream, peer) = self.listener.accept().await?;
        tls_server_accept_with(stream, peer, self.acceptor.clone(), self.config, handler).await
    }
}

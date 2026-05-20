//! TLS support via `tokio-rustls`. Available behind the `tls` cargo feature.
//!
//! IEC 62351-3 recommends TLS for IEC 60870-5-104 over public networks. The
//! conventional port is 19998 (see [`crate::DEFAULT_TLS_PORT`]).

use std::sync::Arc;

use iec60870_proto::frame104::{Config, Role};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::client104::ClientEvent;
use crate::driver::{self, Command, DriverEvent};
use crate::error::{Error, Result};
use crate::handler::EventHandler;
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

    pub async fn recv(&mut self) -> Option<ClientEvent> {
        match self.evt_rx.recv().await? {
            DriverEvent::Asdu(bytes) => Some(ClientEvent::Asdu(bytes)),
            DriverEvent::StateChanged(state) => Some(ClientEvent::StateChanged(state)),
            DriverEvent::Closed(reason) => Some(ClientEvent::Closed(reason)),
        }
    }

    pub async fn stop(&self) -> Result<()> {
        self.cmd_tx
            .send(Command::StopDt)
            .await
            .map_err(|_| Error::DriverGone)
    }
}

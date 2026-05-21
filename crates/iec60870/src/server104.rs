//! Async IEC 60870-5-104 server.

use std::net::SocketAddr;

use iec60870_proto::asdu::{Asdu, AsduPayload, AsduAddressing, CommonAddress, Cot, Vsq};
use iec60870_proto::frame104::{Config, DisconnectReason, Role, State};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::driver::{self, Command, DriverEvent};
use crate::error::{Error, Result};
use crate::file_transfer::service::{self as ft_service, ProviderObject};
use crate::file_transfer::{FileTransferConfig, FileTransferHandle, FileTransferProvider};
use crate::handler::{DefaultLoggingHandler, EventHandler};
use crate::policy::AsduPolicy;

/// IEC 60870-5-104 server. Bind a port, then `accept().await` returns a
/// [`ServerConnection`] for each new peer.
#[derive(Debug)]
pub struct Server104 {
    listener: TcpListener,
    config: Config,
    file_provider: Option<(ProviderObject, FileTransferConfig)>,
}

impl Server104 {
    pub async fn bind(addr: SocketAddr, config: Config) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self {
            listener,
            config,
            file_provider: None,
        })
    }

    /// Install a file-transfer provider for every connection accepted after
    /// this call. The provider serves outbound transfers (master pulls) and
    /// receives inbound transfers (master pushes) automatically.
    pub fn with_file_provider<P: FileTransferProvider>(mut self, provider: P) -> Self {
        self.file_provider = Some((ProviderObject::new(provider), FileTransferConfig::default()));
        self
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept one connection. Spawns the driver task internally.
    pub async fn accept(&self) -> Result<ServerConnection> {
        self.accept_with(DefaultLoggingHandler).await
    }

    pub async fn accept_with<H: EventHandler>(&self, handler: H) -> Result<ServerConnection> {
        self.accept_with_policy_and_handler(AsduPolicy::default(), handler)
            .await
    }

    /// Accept one connection with a restrictive [`AsduPolicy`] and a custom
    /// event handler. Rejected ASDUs are dropped and logged at warn level.
    pub async fn accept_with_policy_and_handler<H: EventHandler>(
        &self,
        mut policy: AsduPolicy,
        handler: H,
    ) -> Result<ServerConnection> {
        let (stream, peer) = self.listener.accept().await?;
        stream.set_nodelay(true)?;
        // If a provider is configured, widen the policy so FT ASDUs aren't dropped.
        if self.file_provider.is_some() && policy.is_restrictive() {
            for tid in 120u8..=126 {
                policy = policy.allow_type_id(tid);
            }
        }
        Ok(ServerConnection::spawn_with_ft(
            stream,
            peer,
            self.config,
            policy,
            handler,
            self.file_provider.clone(),
        ))
    }
}

#[derive(Debug)]
pub struct ServerConnection {
    peer: SocketAddr,
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<DriverEvent>,
    ft: Option<FileTransferHandle>,
    _task: tokio::task::JoinHandle<Result<()>>,
    _ft_task: Option<tokio::task::JoinHandle<()>>,
}

impl ServerConnection {
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// File-transfer handle if a provider was installed via
    /// [`Server104::with_file_provider`].
    pub fn file_transfer(&self) -> Option<&FileTransferHandle> {
        self.ft.as_ref()
    }

    pub async fn send_asdu(&self, asdu: Vec<u8>) -> Result<()> {
        self.cmd_tx
            .send(Command::SendAsdu(asdu))
            .await
            .map_err(|_| Error::DriverGone)
    }

    /// Send a typed ASDU payload using IEC-60870-5-104 addressing.
    ///
    /// See [`Client104::send`](crate::Client104::send) for the analogous
    /// helper on the client side.
    pub async fn send<P: AsduPayload>(
        &self,
        cot: Cot,
        ca: CommonAddress,
        vsq: Vsq,
        payload: &P,
    ) -> Result<()> {
        let bytes = Asdu::from_payload(cot, ca, vsq, payload, AsduAddressing::IEC104)
            .encode_to_vec(AsduAddressing::IEC104);
        self.send_asdu(bytes).await
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

impl ServerConnection {
    /// Spawn a server-side driver around an already-established stream
    /// (plain TCP or TLS). Used internally so the plain-TCP and TLS paths
    /// produce the same handle type.
    pub(crate) fn spawn<S, H>(
        stream: S,
        peer: SocketAddr,
        config: Config,
        policy: AsduPolicy,
        handler: H,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        H: EventHandler,
    {
        Self::spawn_with_ft(stream, peer, config, policy, handler, None)
    }

    pub(crate) fn spawn_with_ft<S, H>(
        stream: S,
        peer: SocketAddr,
        config: Config,
        policy: AsduPolicy,
        handler: H,
        ft: Option<(ProviderObject, FileTransferConfig)>,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        H: EventHandler,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (evt_tx, evt_rx) = mpsc::channel(64);
        let (ft_tx, ft_handle, ft_task) = match ft {
            Some((provider, cfg)) => {
                let (asdu_tx, asdu_rx) = mpsc::channel(32);
                let (handle, service) =
                    ft_service::build(provider, cfg, cmd_tx.clone(), asdu_rx);
                let task = tokio::spawn(service.run());
                (Some(asdu_tx), Some(handle), Some(task))
            }
            None => (None, None, None),
        };
        let task = tokio::spawn(driver::run(
            stream,
            Role::Server,
            config,
            policy,
            handler,
            cmd_rx,
            evt_tx,
            ft_tx,
        ));
        Self {
            peer,
            cmd_tx,
            evt_rx,
            ft: ft_handle,
            _task: task,
            _ft_task: ft_task,
        }
    }
}

//! Async IEC 60870-5-104 client.

use std::time::Duration;

use iec60870_proto::asdu::{Asdu, AsduAddressing, AsduPayload, CommonAddress, Cot, Vsq};
use iec60870_proto::frame104::apdu::MAX_ASDU_LEN;
use iec60870_proto::frame104::{Config, DisconnectReason, Role, State};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::driver::{self, Command, DriverEvent};
use crate::error::{Error, Result};
use crate::file_transfer::service::{self as ft_service, ProviderObject};
use crate::file_transfer::{FileTransferConfig, FileTransferHandle, FileTransferProvider};
use crate::handler::{DefaultLoggingHandler, EventHandler};
use crate::policy::AsduPolicy;
use crate::transport::Transport;

/// IEC 60870-5-104 client over TCP (and optional TLS).
///
/// The client owns an internal task that drives the connection. Drop the
/// `Client104` to terminate it.
#[derive(Debug)]
pub struct Client104 {
    cmd_tx: mpsc::Sender<Command>,
    evt_rx: mpsc::Receiver<DriverEvent>,
    ft: Option<FileTransferHandle>,
    _task: tokio::task::JoinHandle<Result<()>>,
    _ft_task: Option<tokio::task::JoinHandle<()>>,
}

impl Client104 {
    /// Connect using the default logging handler and a fully permissive
    /// ASDU policy.
    pub async fn connect(transport: Transport, config: Config) -> Result<Self> {
        Self::connect_with_policy_and_handler(
            transport,
            config,
            AsduPolicy::default(),
            DefaultLoggingHandler,
        )
        .await
    }

    /// Connect with a custom event handler and a fully permissive policy.
    pub async fn connect_with<H: EventHandler>(
        transport: Transport,
        config: Config,
        handler: H,
    ) -> Result<Self> {
        Self::connect_with_policy_and_handler(transport, config, AsduPolicy::default(), handler)
            .await
    }

    /// Connect with both a custom event handler and a restrictive
    /// [`AsduPolicy`]. The policy is applied to every decoded ASDU before
    /// it is surfaced to user code (or [`EventHandler::on_asdu_received`]).
    /// Rejected ASDUs are dropped and logged at warn level.
    pub async fn connect_with_policy_and_handler<H: EventHandler>(
        transport: Transport,
        config: Config,
        policy: AsduPolicy,
        handler: H,
    ) -> Result<Self> {
        let stream = open_stream(&transport).await?;
        Self::spawn(stream, config, policy, handler).await
    }

    /// Connect and additionally wire up a file-transfer provider. The
    /// returned client exposes [`Client104::file_transfer`] for high-level
    /// fetch / push operations and routes inbound FT ASDUs to the provider
    /// automatically (peer-initiated transfers).
    pub async fn connect_with_file_provider<P, H>(
        transport: Transport,
        config: Config,
        provider: P,
        handler: H,
    ) -> Result<Self>
    where
        P: FileTransferProvider,
        H: EventHandler,
    {
        let stream = open_stream(&transport).await?;
        // Default policy is fully permissive; FT ASDUs flow through. Callers
        // who want to restrict should use [`Client104::connect_with_file_provider_and_policy`].
        Self::spawn_with_ft(
            stream,
            config,
            AsduPolicy::default(),
            handler,
            Some((ProviderObject::new(provider), FileTransferConfig::default())),
        )
        .await
    }

    /// Same as [`Client104::connect_with_file_provider`] but with a custom
    /// [`AsduPolicy`]. If the policy is restrictive, FT TypeIDs 120-126 are
    /// added to its allow-list automatically so the provider can do its job;
    /// the rest of the policy is left untouched.
    pub async fn connect_with_file_provider_and_policy<P, H>(
        transport: Transport,
        config: Config,
        provider: P,
        mut policy: AsduPolicy,
        handler: H,
    ) -> Result<Self>
    where
        P: FileTransferProvider,
        H: EventHandler,
    {
        let stream = open_stream(&transport).await?;
        if policy.is_restrictive() {
            for tid in 120u8..=126 {
                policy = policy.allow_type_id(tid);
            }
        }
        Self::spawn_with_ft(
            stream,
            config,
            policy,
            handler,
            Some((ProviderObject::new(provider), FileTransferConfig::default())),
        )
        .await
    }

    /// Spawn the client driver around an already-established stream. Used
    /// both by [`Client104::connect`] (plain TCP) and by the TLS entry
    /// point in [`crate::tls`] so both paths produce the same handle type.
    pub(crate) async fn spawn<S, H>(
        stream: S,
        config: Config,
        policy: AsduPolicy,
        handler: H,
    ) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        H: EventHandler,
    {
        Self::spawn_with_ft(stream, config, policy, handler, None).await
    }

    pub(crate) async fn spawn_with_ft<S, H>(
        stream: S,
        config: Config,
        policy: AsduPolicy,
        handler: H,
        ft: Option<(ProviderObject, FileTransferConfig)>,
    ) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        H: EventHandler,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (evt_tx, evt_rx) = mpsc::channel(64);
        let (ft_tx, ft_handle, ft_task) = match ft {
            Some((provider, cfg)) => {
                let (asdu_tx, asdu_rx) = mpsc::channel(32);
                let (handle, service) = ft_service::build(provider, cfg, cmd_tx.clone(), asdu_rx);
                let task = tokio::spawn(service.run());
                (Some(asdu_tx), Some(handle), Some(task))
            }
            None => (None, None, None),
        };
        let task = tokio::spawn(driver::run(
            stream,
            Role::Client,
            config,
            policy,
            handler,
            cmd_rx,
            evt_tx,
            ft_tx,
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
            ft: ft_handle,
            _task: task,
            _ft_task: ft_task,
        })
    }

    /// Returns the file-transfer handle if the client was constructed with a
    /// provider via [`Client104::connect_with_file_provider`].
    pub fn file_transfer(&self) -> Option<&FileTransferHandle> {
        self.ft.as_ref()
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

    /// Send a typed ASDU payload. Encodes the header (Type ID, VSQ, COT, CA)
    /// plus the information-objects section in IEC-60870-5-104 addressing
    /// (two-octet COT, two-octet CA, three-octet IOA) and dispatches the
    /// resulting bytes through the driver.
    ///
    /// Prefer this over manually constructing an [`Asdu`] and calling
    /// [`send_asdu`](Self::send_asdu) when the payload type is known at
    /// compile time.
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
#[non_exhaustive]
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
        Transport::Tls { .. } => Err(Error::InvalidTransport(
            "Transport::Tls must be opened via tls_client_connect, not Client104::connect",
        )),
    }
}

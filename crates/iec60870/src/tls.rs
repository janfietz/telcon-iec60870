//! TLS support via `tokio-rustls`. Available behind the `tls` cargo feature.
//!
//! ## IEC 62351-3 deployment notes
//!
//! IEC 62351-3 specifies TLS for IEC 60870-5-104 over untrusted or shared
//! networks. The conventional port is **19998** (see
//! [`crate::DEFAULT_TLS_PORT`]) and the standard **expects mutual TLS** —
//! both controlling and controlled stations are authenticated with X.509
//! certificates issued by an organisation-controlled CA.
//!
//! Helpers in this module:
//!
//! | Helper | Use for |
//! | --- | --- |
//! | [`client_config_with_roots`] | Server-authenticated TLS (server proves identity, client does not). Acceptable for **lab / development** only. |
//! | [`client_config_with_client_cert`] | **Recommended** for production — client also presents a certificate, satisfying mTLS expectations. |
//! | [`server_config_single_cert`] | Server-only authentication. Same caveat as `client_config_with_roots`. |
//! | [`server_config_requiring_client_cert`] | **Recommended** for production — verifies the client certificate chain against the given root store and rejects unauthenticated peers. |
//!
//! `rustls` is configured with the `ring` provider; cipher suites and TLS
//! versions follow `rustls` defaults (TLS 1.2 and 1.3 only, no SSL/early TLS).

use std::net::SocketAddr;
use std::sync::Arc;

use iec60870_proto::frame104::Config;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::client104::Client104;
use crate::error::{Error, Result};
use crate::handler::{DefaultLoggingHandler, EventHandler};
use crate::policy::AsduPolicy;
use crate::security::{IpFilter, TlsSecurityConfig};
use crate::server104::ServerConnection;
use crate::transport::Transport;

/// Re-export of the underlying `rustls::ClientConfig` for convenience.
pub type TlsConfig = Arc<ClientConfig>;

/// Build a `ClientConfig` that trusts the given list of root certificate
/// authorities. **The client does not authenticate itself** — use
/// [`client_config_with_client_cert`] for IEC 62351-3 deployments.
pub fn client_config_with_roots(roots: RootCertStore) -> TlsConfig {
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Build a `ClientConfig` for **mutual TLS**: the client trusts peers signed
/// by `roots`, and presents `cert_chain` + `key` when the server requests a
/// client certificate.
///
/// This is the configuration IEC 62351-3 expects for production deployments.
///
/// Returns an error if rustls cannot consume the given key (e.g. malformed
/// PKCS#8).
pub fn client_config_with_client_cert(
    roots: RootCertStore,
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<TlsConfig> {
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| Error::Tls(format!("invalid client key/cert: {e}")))?;
    Ok(Arc::new(cfg))
}

/// Build a `ServerConfig` that presents `cert_chain` + `key` and does **not**
/// request a client certificate. Acceptable for lab use; use
/// [`server_config_requiring_client_cert`] for IEC 62351-3 deployments.
pub fn server_config_single_cert(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>> {
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| Error::Tls(format!("invalid server key/cert: {e}")))?;
    Ok(Arc::new(cfg))
}

/// Build a `ServerConfig` for **mutual TLS**: presents `cert_chain` + `key`
/// to the peer, and *requires* the peer to present a client certificate
/// signed by one of the roots in `client_roots`. Connections from
/// unauthenticated peers are rejected during the TLS handshake.
///
/// This is the configuration IEC 62351-3 expects for production deployments.
pub fn server_config_requiring_client_cert(
    cert_chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    client_roots: RootCertStore,
) -> Result<Arc<ServerConfig>> {
    let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
        .build()
        .map_err(|e| Error::Tls(format!("building client cert verifier: {e}")))?;
    let cfg = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key)
        .map_err(|e| Error::Tls(format!("invalid server key/cert: {e}")))?;
    Ok(Arc::new(cfg))
}

// ---------------------------------------------------------------------------
// Type aliases — preserved for source compatibility with the pre-unification
// API. The underlying handles are the same `Client104` / `ServerConnection`
// types used for plain TCP, since the driver is generic over the stream.
// ---------------------------------------------------------------------------

/// Alias for the unified [`Client104`] handle. TLS connections produce the
/// same client type as plain TCP — only the constructor differs.
pub type TlsClient = Client104;

/// Alias for the unified [`ServerConnection`] handle. TLS-accepted
/// connections produce the same type as plain-TCP-accepted ones.
pub type TlsServerConnection = ServerConnection;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Connect an IEC 60870-5-104 client over TLS. Behaves like
/// [`Client104::connect_with`] but expects a [`Transport::Tls`] variant.
pub async fn tls_client_connect<H: EventHandler>(
    transport: Transport,
    config: Config,
    handler: H,
) -> Result<Client104> {
    tls_client_connect_with_policy(transport, config, AsduPolicy::default(), handler).await
}

/// Connect an IEC 60870-5-104 client over TLS with a restrictive
/// [`AsduPolicy`].
pub async fn tls_client_connect_with_policy<H: EventHandler>(
    transport: Transport,
    config: Config,
    policy: AsduPolicy,
    handler: H,
) -> Result<Client104> {
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

    Client104::spawn(stream, config, policy, handler).await
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Accept one inbound TLS connection, perform the handshake, and spawn the
/// IEC 60870-5-104 driver task. Returns a [`ServerConnection`] handle.
///
/// Uses [`DefaultLoggingHandler`]; call [`tls_server_accept_with`] for a
/// custom handler.
pub async fn tls_server_accept(
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    config: Config,
) -> Result<ServerConnection> {
    tls_server_accept_with(stream, peer, acceptor, config, DefaultLoggingHandler).await
}

/// Accept one inbound TLS connection with a custom event handler.
///
/// Performs the TLS handshake, then spawns the driver with the resulting
/// `TlsStream<TcpStream>` as the transport and `Role::Server`.
pub async fn tls_server_accept_with<H: EventHandler>(
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    config: Config,
    handler: H,
) -> Result<ServerConnection> {
    tls_server_accept_with_policy(
        stream,
        peer,
        acceptor,
        config,
        AsduPolicy::default(),
        handler,
    )
    .await
}

/// Accept one inbound TLS connection with a custom event handler and a
/// restrictive [`AsduPolicy`].
pub async fn tls_server_accept_with_policy<H: EventHandler>(
    stream: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    config: Config,
    policy: AsduPolicy,
    handler: H,
) -> Result<ServerConnection> {
    stream.set_nodelay(true)?;
    let tls_stream = acceptor
        .accept(stream)
        .await
        .map_err(|e| Error::Tls(format!("tls accept: {e}")))?;
    Ok(ServerConnection::spawn_with_ft(
        tls_stream, peer, config, policy, handler, None,
    ))
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
    ip_filter: IpFilter,
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
        Self::bind_with(addr, server_config, config, IpFilter::allow_all()).await
    }

    /// Bind a `TlsServer` from a [`TlsSecurityConfig`], applying an
    /// [`IpFilter`] to every accept *before* the TLS handshake runs.
    ///
    /// Use this entry point for production deployments: it bundles the
    /// server cert/key, the trusted client-CA roots, optional cipher and
    /// signature-scheme allowlists, and the [`ClientCertPolicy`](crate::ClientCertPolicy)
    /// in one place.
    pub async fn bind_with_security(
        addr: SocketAddr,
        config: Config,
        tls_security: TlsSecurityConfig,
        ip_filter: IpFilter,
    ) -> Result<Self> {
        let server_config = tls_security.build_server_config()?;
        Self::bind_with(addr, server_config, config, ip_filter).await
    }

    /// Bind a `TlsServer` from a pre-built rustls `ServerConfig`, also
    /// applying an [`IpFilter`]. Use this when you already have a custom
    /// `ServerConfig` (e.g. legacy code, raw rustls integration) but still
    /// want pre-handshake IP allow-listing.
    pub async fn bind_with_filter(
        addr: SocketAddr,
        server_config: Arc<ServerConfig>,
        config: Config,
        ip_filter: IpFilter,
    ) -> Result<Self> {
        Self::bind_with(addr, server_config, config, ip_filter).await
    }

    async fn bind_with(
        addr: SocketAddr,
        server_config: Arc<ServerConfig>,
        config: Config,
        ip_filter: IpFilter,
    ) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let acceptor = TlsAcceptor::from(server_config);
        Ok(Self {
            listener,
            acceptor,
            config,
            ip_filter,
        })
    }

    /// Return the local socket address the listener is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept one connection using the [`DefaultLoggingHandler`].
    pub async fn accept(&self) -> Result<ServerConnection> {
        self.accept_with(DefaultLoggingHandler).await
    }

    /// Accept one connection with a custom event handler.
    pub async fn accept_with<H: EventHandler>(&self, handler: H) -> Result<ServerConnection> {
        self.accept_with_policy_and_handler(AsduPolicy::default(), handler)
            .await
    }

    /// Accept one connection with both a custom handler and a restrictive
    /// [`AsduPolicy`].
    pub async fn accept_with_policy_and_handler<H: EventHandler>(
        &self,
        policy: AsduPolicy,
        handler: H,
    ) -> Result<ServerConnection> {
        let (stream, peer) = loop {
            let (stream, peer) = self.listener.accept().await?;
            if self.ip_filter.contains(peer) {
                break (stream, peer);
            }
            tracing::warn!(%peer, "iec60870: peer rejected by ip filter (pre-tls)");
            drop(stream);
        };
        tls_server_accept_with_policy(
            stream,
            peer,
            self.acceptor.clone(),
            self.config,
            policy,
            handler,
        )
        .await
    }
}

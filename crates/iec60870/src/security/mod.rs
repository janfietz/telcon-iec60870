//! Connection-level security for IEC 60870-5-104 servers.
//!
//! Two orthogonal layers are exposed:
//!
//! - [`IpFilter`] — peer IP allow-listing. Applies to both plain TCP
//!   ([`Server104`](crate::Server104)) and TLS ([`TlsServer`](crate::TlsServer))
//!   accept loops, runs *before* any handshake, and silently drops rejected
//!   peers.
//! - [`TlsSecurityConfig`] (under the `tls` feature) — declarative TLS
//!   hardening: server cert/key, explicit CA root store (no OS fallback),
//!   cipher-suite allow-list, signature-scheme allow-list, and a
//!   [`ClientCertPolicy`] (none / chain-only / pinned SHA-256 fingerprints /
//!   custom verifier closure).
//!
//! See [`Server104::bind_with_security`](crate::Server104::bind_with_security)
//! and [`TlsServer::bind_with_security`](crate::TlsServer::bind_with_security).

mod ip_filter;

pub use ip_filter::{IpFilter, IpFilterParseError};

#[cfg(feature = "tls")]
mod tls_config;
#[cfg(feature = "tls")]
mod verifier;

#[cfg(feature = "tls")]
pub use tls_config::{
    fingerprint_sha256_of_pem_file, CertificateChain, ClientCertPolicy, CustomVerifierFn,
    TlsSecurityConfig, VerifyError,
};

/// Aggregate security configuration. Reserved for future composition (e.g.
/// passing a single struct that bundles IP filtering and TLS settings). At the
/// moment the `bind_with_security` entry points take the constituent parts
/// directly for clarity; `SecurityConfig` is a convenience aggregate for
/// callers building deployments from configuration files.
#[derive(Debug, Default)]
pub struct SecurityConfig {
    /// Peer IP allow-list. Defaults to [`IpFilter::allow_all`].
    pub ip_filter: IpFilter,
    /// Optional TLS configuration. `None` means plain TCP. `TlsSecurityConfig`
    /// is not `Clone` because the rustls `PrivateKeyDer` is intentionally
    /// non-clonable (zero-on-drop key material).
    #[cfg(feature = "tls")]
    pub tls: Option<TlsSecurityConfig>,
}

//! Declarative TLS hardening for [`TlsServer`](crate::TlsServer).
//!
//! A [`TlsSecurityConfig`] bundles everything needed to bring up a server
//! that:
//!
//! - presents a fixed certificate chain and key,
//! - trusts client certificates only against an explicit [`RootCertStore`]
//!   (the system trust store is never consulted),
//! - optionally restricts the cipher-suite and signature-scheme sets,
//! - applies an application-level [`ClientCertPolicy`] on top of webpki
//!   chain validation (pinned SHA-256 fingerprints or a custom closure).
//!
//! The configuration is consumed by
//! [`TlsServer::bind_with_security`](crate::TlsServer::bind_with_security).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use tokio_rustls::rustls::crypto::{ring, CryptoProvider};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{
    RootCertStore, ServerConfig, SignatureScheme, SupportedCipherSuite,
};

use crate::error::{Error, Result};

use super::verifier::{sha256_fingerprint, PolicyClientVerifier};

/// Closure type for application-defined client-certificate validation.
///
/// Invoked after the cert chain has already passed webpki validation against
/// [`TlsSecurityConfig::client_roots`]. Returning `Err(VerifyError(_))` aborts
/// the handshake.
pub type CustomVerifierFn =
    Arc<dyn Fn(&CertificateChain<'_>) -> std::result::Result<(), VerifyError> + Send + Sync>;

/// A validated client certificate chain handed to a [`CustomVerifierFn`].
///
/// `leaf` is the end-entity certificate (the one the client owns the private
/// key for); `intermediates` is the list of issuer certificates the client
/// presented to bridge to a trusted root.
#[derive(Debug)]
pub struct CertificateChain<'a> {
    pub leaf: &'a CertificateDer<'a>,
    pub intermediates: &'a [CertificateDer<'a>],
}

/// Returned from a [`CustomVerifierFn`] to reject a client.
#[derive(Debug, Clone, thiserror::Error)]
#[error("client certificate rejected: {0}")]
pub struct VerifyError(pub String);

/// What the server requires from the client's certificate.
///
/// Layered on top of [`TlsSecurityConfig::client_roots`] — every variant
/// except [`Self::None`] still requires a valid chain to a trusted root
/// **and** then runs the policy on the validated leaf.
pub enum ClientCertPolicy {
    /// Server-authenticated TLS only. The client does not present a
    /// certificate. Acceptable for lab use; **not** IEC 62351-3 compliant.
    None,
    /// Chain validation only — every client presenting a cert that chains
    /// to `client_roots` is accepted.
    TrustChain,
    /// Chain validation **and** the SHA-256 fingerprint of the leaf DER
    /// must be in this list. Use [`fingerprint_sha256_of_pem_file`] to
    /// compute pins from PEM files at startup.
    PinnedFingerprints(Vec<[u8; 32]>),
    /// Chain validation **and** the closure must return `Ok(())` for the
    /// presented chain.
    CustomVerifier(CustomVerifierFn),
}

impl std::fmt::Debug for ClientCertPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::TrustChain => f.write_str("TrustChain"),
            Self::PinnedFingerprints(p) => f
                .debug_tuple("PinnedFingerprints")
                .field(&format_args!("[{} pin(s)]", p.len()))
                .finish(),
            Self::CustomVerifier(_) => f.write_str("CustomVerifier(<closure>)"),
        }
    }
}

/// Declarative TLS configuration for [`TlsServer::bind_with_security`](crate::TlsServer::bind_with_security).
///
/// All fields are public to keep the type a simple data record — callers can
/// build it field-by-field or via the [`from_pem_paths`](Self::from_pem_paths)
/// helper.
pub struct TlsSecurityConfig {
    /// Server certificate chain (leaf first).
    pub server_chain: Vec<CertificateDer<'static>>,
    /// Server private key for `server_chain`.
    pub server_key: PrivateKeyDer<'static>,
    /// Trusted client-CA roots. **The OS trust store is never consulted.**
    /// IEC 62351-3 deployments use an organisation-controlled CA here.
    pub client_roots: RootCertStore,
    /// Restrict the cipher-suite set. `None` uses rustls defaults; an empty
    /// `Some(vec![])` is rejected at `build_server_config` time.
    pub cipher_suites: Option<Vec<SupportedCipherSuite>>,
    /// Restrict the signature schemes advertised to clients during the
    /// handshake. `None` uses rustls defaults.
    pub signature_schemes: Option<Vec<SignatureScheme>>,
    /// Policy applied to the client certificate after webpki validation.
    pub client_cert_policy: ClientCertPolicy,
}

impl std::fmt::Debug for TlsSecurityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSecurityConfig")
            .field("server_chain", &format_args!("[{} cert(s)]", self.server_chain.len()))
            .field("client_roots", &self.client_roots)
            .field(
                "cipher_suites",
                &self.cipher_suites.as_ref().map(|c| c.len()),
            )
            .field("signature_schemes", &self.signature_schemes)
            .field("client_cert_policy", &self.client_cert_policy)
            .finish_non_exhaustive()
    }
}

impl TlsSecurityConfig {
    /// Load `server_chain` + `server_key` + `client_roots` from PEM files on
    /// disk. Cipher/scheme allowlists default to `None`; client cert policy
    /// defaults to [`ClientCertPolicy::TrustChain`] (mTLS enforced against
    /// the loaded CAs, no extra pinning).
    pub fn from_pem_paths(
        cert: &Path,
        key: &Path,
        client_ca: &[&Path],
    ) -> Result<Self> {
        let server_chain = load_certs(cert)?;
        let server_key = load_private_key(key)?;

        let mut client_roots = RootCertStore::empty();
        for path in client_ca {
            for cert in load_certs(path)? {
                client_roots
                    .add(cert)
                    .map_err(|e| Error::Tls(format!("adding CA from {}: {e}", path.display())))?;
            }
        }

        Ok(Self {
            server_chain,
            server_key,
            client_roots,
            cipher_suites: None,
            signature_schemes: None,
            client_cert_policy: ClientCertPolicy::TrustChain,
        })
    }

    /// Build the rustls `ServerConfig` described by this struct.
    pub fn build_server_config(self) -> Result<Arc<ServerConfig>> {
        let TlsSecurityConfig {
            server_chain,
            server_key,
            client_roots,
            cipher_suites,
            signature_schemes,
            client_cert_policy,
        } = self;

        // Build a CryptoProvider with the requested cipher suites.
        let mut provider: CryptoProvider = ring::default_provider();
        if let Some(suites) = cipher_suites {
            if suites.is_empty() {
                return Err(Error::Tls("cipher_suites allowlist is empty".into()));
            }
            provider.cipher_suites = suites;
        }
        let provider = Arc::new(provider);

        let builder = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Tls(format!("protocol versions: {e}")))?;

        let cfg = match client_cert_policy {
            ClientCertPolicy::None => builder
                .with_no_client_auth()
                .with_single_cert(server_chain, server_key)
                .map_err(|e| Error::Tls(format!("invalid server key/cert: {e}")))?,
            policy => {
                let inner = WebPkiClientVerifier::builder_with_provider(
                    Arc::new(client_roots),
                    provider,
                )
                .build()
                .map_err(|e| Error::Tls(format!("building client cert verifier: {e}")))?;
                let policy_verifier = Arc::new(PolicyClientVerifier::new(
                    inner,
                    policy,
                    signature_schemes,
                ));
                builder
                    .with_client_cert_verifier(policy_verifier)
                    .with_single_cert(server_chain, server_key)
                    .map_err(|e| Error::Tls(format!("invalid server key/cert: {e}")))?
            }
        };

        Ok(Arc::new(cfg))
    }
}

/// SHA-256 fingerprint of the first certificate in a PEM file. Use this at
/// startup to derive a pin from a known-good client cert PEM rather than
/// hand-typing 32 bytes.
pub fn fingerprint_sha256_of_pem_file(path: &Path) -> Result<[u8; 32]> {
    let certs = load_certs(path)?;
    let leaf = certs
        .into_iter()
        .next()
        .ok_or_else(|| Error::Tls(format!("no certificate in {}", path.display())))?;
    Ok(sha256_fingerprint(leaf.as_ref()))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path)
        .map_err(|e| Error::Tls(format!("opening cert file {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    let certs: std::result::Result<Vec<_>, _> = rustls_pemfile::certs(&mut reader).collect();
    certs.map_err(|e| Error::Tls(format!("parsing certs from {}: {e}", path.display())))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let file = File::open(path)
        .map_err(|e| Error::Tls(format!("opening key file {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| Error::Tls(format!("parsing key from {}: {e}", path.display())))?
        .ok_or_else(|| Error::Tls(format!("no private key in {}", path.display())))
}

//! `ClientCertVerifier` adapter that layers an application-level
//! [`ClientCertPolicy`] on top of standard webpki chain validation.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio_rustls::rustls::pki_types::{CertificateDer, UnixTime};
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, SignatureScheme,
};
use tokio_rustls::rustls::client::danger::HandshakeSignatureValid;

use super::tls_config::{CertificateChain, ClientCertPolicy};

/// Wraps an inner `ClientCertVerifier` (typically `WebPkiClientVerifier`) and
/// runs a [`ClientCertPolicy`] check after webpki chain validation succeeds.
/// Optionally restricts the set of advertised signature schemes.
pub(crate) struct PolicyClientVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    policy: ClientCertPolicy,
    signature_schemes: Option<Vec<SignatureScheme>>,
}

impl PolicyClientVerifier {
    pub(crate) fn new(
        inner: Arc<dyn ClientCertVerifier>,
        policy: ClientCertPolicy,
        signature_schemes: Option<Vec<SignatureScheme>>,
    ) -> Self {
        Self {
            inner,
            policy,
            signature_schemes,
        }
    }
}

impl std::fmt::Debug for PolicyClientVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyClientVerifier")
            .field("policy", &self.policy)
            .field("signature_schemes", &self.signature_schemes)
            .finish_non_exhaustive()
    }
}

impl ClientCertVerifier for PolicyClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn offer_client_auth(&self) -> bool {
        // Any non-`None` policy means we want the client to present a cert.
        !matches!(self.policy, ClientCertPolicy::None)
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        // 1. Standard chain validation first.
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;

        // 2. Application-level policy check on the validated chain.
        match &self.policy {
            ClientCertPolicy::None | ClientCertPolicy::TrustChain => Ok(verified),
            ClientCertPolicy::PinnedFingerprints(pins) => {
                let fp = sha256_fingerprint(end_entity.as_ref());
                if pins.contains(&fp) {
                    Ok(verified)
                } else {
                    Err(Error::InvalidCertificate(
                        CertificateError::ApplicationVerificationFailure,
                    ))
                }
            }
            ClientCertPolicy::CustomVerifier(f) => {
                let chain = CertificateChain {
                    leaf: end_entity,
                    intermediates,
                };
                f(&chain).map(|()| verified).map_err(|_e| {
                    Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
                })
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        let inner = self.inner.supported_verify_schemes();
        match &self.signature_schemes {
            None => inner,
            Some(allow) => inner.into_iter().filter(|s| allow.contains(s)).collect(),
        }
    }
}

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
pub(crate) fn sha256_fingerprint(der: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(der);
    h.finalize().into()
}

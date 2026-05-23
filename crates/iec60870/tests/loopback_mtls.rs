//! End-to-end mTLS smoke test.
//!
//! Generates a tiny in-memory PKI (CA + client cert + server cert), wires
//! it through [`server_config_requiring_client_cert`] and
//! [`client_config_with_client_cert`], and verifies the IEC 60870-5-104
//! STARTDT handshake + interrogation roundtrip succeeds when both peers
//! authenticate. Also verifies that a client missing its certificate is
//! refused.

#![cfg(feature = "tls")]

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1};
use iec60870::proto::asdu::{CommonAddress, Cot, Ioa, Vsq};
use iec60870::proto::frame104::Config;
use iec60870::{
    client_config_with_client_cert, client_config_with_roots,
    server_config_requiring_client_cert, tls_client_connect, ClientCertPolicy, ClientEvent,
    IpFilter, NoopHandler, TlsSecurityConfig, Transport, TlsServer, VerifyError,
};
use rcgen::{CertificateParams, DistinguishedName, DnValue, KeyPair, SanType};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer,
};
use tokio_rustls::rustls::RootCertStore;

struct Pki {
    ca_cert: CertificateDer<'static>,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
    client_chain: Vec<CertificateDer<'static>>,
    client_key: PrivateKeyDer<'static>,
}

fn build_pki() -> Pki {
    // 1. Generate a self-signed CA.
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(
        rcgen::DnType::CommonName,
        DnValue::Utf8String("iec60870-test-ca".into()),
    );
    ca_params.distinguished_name = ca_dn;
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    // 2. Issue server leaf.
    let mut server_params = CertificateParams::default();
    server_params.subject_alt_names = vec![
        SanType::DnsName("server.test".try_into().unwrap()),
        SanType::IpAddress(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)),
    ];
    let mut server_dn = DistinguishedName::new();
    server_dn.push(
        rcgen::DnType::CommonName,
        DnValue::Utf8String("iec60870-test-server".into()),
    );
    server_params.distinguished_name = server_dn;
    let server_key = KeyPair::generate().unwrap();
    let server_cert = server_params.signed_by(&server_key, &ca_cert, &ca_key).unwrap();

    // 3. Issue client leaf.
    let mut client_params = CertificateParams::default();
    let mut client_dn = DistinguishedName::new();
    client_dn.push(
        rcgen::DnType::CommonName,
        DnValue::Utf8String("iec60870-test-client".into()),
    );
    client_params.distinguished_name = client_dn;
    let client_key = KeyPair::generate().unwrap();
    let client_cert = client_params.signed_by(&client_key, &ca_cert, &ca_key).unwrap();

    fn key_to_pkcs8(key: KeyPair) -> PrivateKeyDer<'static> {
        let der = key.serialize_der();
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der))
    }

    Pki {
        ca_cert: ca_cert.der().clone(),
        server_chain: vec![server_cert.der().clone()],
        server_key: key_to_pkcs8(server_key),
        client_chain: vec![client_cert.der().clone()],
        client_key: key_to_pkcs8(client_key),
    }
}

fn roots_from(cert: &CertificateDer<'static>) -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add(cert.clone()).unwrap();
    roots
}

fn sha256(der: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(der);
    h.finalize().into()
}

/// Build a `TlsSecurityConfig` for the given PKI with the supplied policy.
/// `cipher_suites` / `signature_schemes` follow rustls defaults.
fn security_for(pki: &Pki, policy: ClientCertPolicy) -> TlsSecurityConfig {
    TlsSecurityConfig {
        server_chain: pki.server_chain.clone(),
        server_key: pki.server_key.clone_key(),
        client_roots: roots_from(&pki.ca_cert),
        cipher_suites: None,
        signature_schemes: None,
        client_cert_policy: policy,
    }
}

/// Issue a fresh CA + client leaf that is *not* signed by `pki.ca_cert` —
/// used to verify the custom-root-store path rejects unrelated authorities.
fn issue_unrelated_client() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let mut other_ca_params = CertificateParams::default();
    other_ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let mut dn = DistinguishedName::new();
    dn.push(
        rcgen::DnType::CommonName,
        DnValue::Utf8String("iec60870-test-other-ca".into()),
    );
    other_ca_params.distinguished_name = dn;
    let other_ca_key = KeyPair::generate().unwrap();
    let other_ca = other_ca_params.self_signed(&other_ca_key).unwrap();

    let mut leaf_params = CertificateParams::default();
    let mut leaf_dn = DistinguishedName::new();
    leaf_dn.push(
        rcgen::DnType::CommonName,
        DnValue::Utf8String("iec60870-test-other-client".into()),
    );
    leaf_params.distinguished_name = leaf_dn;
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = leaf_params
        .signed_by(&leaf_key, &other_ca, &other_ca_key)
        .unwrap();

    let der = leaf_key.serialize_der();
    (
        vec![leaf.der().clone()],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der)),
    )
}

/// Run `accept_with` in a background task and return the join handle.
/// The accept future is wrapped in a short timeout so the test can assert
/// rejection via the client side without leaking tasks.
fn spawn_accept(server: TlsServer) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(3), server.accept()).await;
    })
}

/// Attempt a TLS connect from `client_chain` + `client_key`, then drain a few
/// events. Returns true if the connection was rejected (either the connect
/// returned an error, or the driver immediately reported `Closed`).
async fn expect_rejection(
    addr: std::net::SocketAddr,
    ca: &CertificateDer<'static>,
    client_chain: Vec<CertificateDer<'static>>,
    client_key: PrivateKeyDer<'static>,
) -> bool {
    let client_cfg =
        client_config_with_client_cert(roots_from(ca), client_chain, client_key).expect("client cfg");
    let transport = Transport::Tls {
        addr,
        server_name: "server.test".into(),
        client_config: client_cfg,
    };

    let connect = tokio::time::timeout(
        Duration::from_secs(3),
        tls_client_connect(transport, Config::default(), NoopHandler),
    )
    .await
    .expect("connect attempt should not hang");

    match connect {
        Err(_) => true,
        Ok(mut client) => {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            loop {
                match tokio::time::timeout_at(deadline, client.recv()).await {
                    Ok(None) => return true,
                    Ok(Some(ClientEvent::Closed(_))) => return true,
                    Ok(Some(_)) => continue,
                    Err(_) => return false,
                }
            }
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pinned_fingerprint_accepts_matching_client() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let pki = build_pki();

    let pin = sha256(pki.client_chain[0].as_ref());
    let security = security_for(&pki, ClientCertPolicy::PinnedFingerprints(vec![pin]));

    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = TlsServer::bind_with_security(
        bind,
        Config::default(),
        security,
        IpFilter::allow_all(),
    )
    .await
    .expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let server_task = spawn_accept(server);

    let client_cfg = client_config_with_client_cert(
        roots_from(&pki.ca_cert),
        pki.client_chain,
        pki.client_key,
    )
    .expect("client cfg");
    let transport = Transport::Tls {
        addr,
        server_name: "server.test".into(),
        client_config: client_cfg,
    };

    let mut client = tls_client_connect(transport, Config::default(), NoopHandler)
        .await
        .expect("matching pin must connect");

    client
        .send(
            Cot::act(),
            CommonAddress(1),
            Vsq::single(1),
            &C_IC_NA_1 {
                objects: vec![(Ioa(0), Qoi::GENERAL)],
            },
        )
        .await
        .expect("send");

    let _ = tokio::time::timeout(Duration::from_secs(2), client.recv()).await;
    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn pinned_fingerprint_rejects_wrong_pin() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let pki = build_pki();

    let security = security_for(&pki, ClientCertPolicy::PinnedFingerprints(vec![[0u8; 32]]));
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = TlsServer::bind_with_security(
        bind,
        Config::default(),
        security,
        IpFilter::allow_all(),
    )
    .await
    .expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let server_task = spawn_accept(server);

    let rejected = expect_rejection(addr, &pki.ca_cert, pki.client_chain, pki.client_key).await;
    assert!(rejected, "wrong fingerprint pin must be rejected");
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn custom_verifier_can_reject() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let pki = build_pki();

    let policy = ClientCertPolicy::CustomVerifier(Arc::new(|_chain| {
        Err(VerifyError("policy denied".into()))
    }));
    let security = security_for(&pki, policy);

    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = TlsServer::bind_with_security(
        bind,
        Config::default(),
        security,
        IpFilter::allow_all(),
    )
    .await
    .expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let server_task = spawn_accept(server);

    let rejected = expect_rejection(addr, &pki.ca_cert, pki.client_chain, pki.client_key).await;
    assert!(rejected, "custom verifier denial must be propagated");
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn custom_root_store_rejects_other_ca() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let pki = build_pki();

    let security = security_for(&pki, ClientCertPolicy::TrustChain);
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = TlsServer::bind_with_security(
        bind,
        Config::default(),
        security,
        IpFilter::allow_all(),
    )
    .await
    .expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let server_task = spawn_accept(server);

    // Build a client whose cert is signed by an *unrelated* CA, but advertise
    // trust for the *server* CA so the client itself completes its half of
    // the handshake.
    let (other_chain, other_key) = issue_unrelated_client();
    let rejected = expect_rejection(addr, &pki.ca_cert, other_chain, other_key).await;
    assert!(rejected, "client cert from unknown CA must be rejected");
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cipher_allowlist_takes_effect() {
    use tokio_rustls::rustls::crypto::{ring, CryptoProvider};
    use tokio_rustls::rustls::ClientConfig;

    let _ = ring::default_provider().install_default();
    let pki = build_pki();

    // Server allows only TLS_CHACHA20_POLY1305_SHA256.
    let mut security = security_for(&pki, ClientCertPolicy::TrustChain);
    security.cipher_suites = Some(vec![ring::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256]);

    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = TlsServer::bind_with_security(
        bind,
        Config::default(),
        security,
        IpFilter::allow_all(),
    )
    .await
    .expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let server_task = spawn_accept(server);

    // Client offers only TLS_AES_256_GCM_SHA384 — no intersection.
    let mut provider: CryptoProvider = ring::default_provider();
    provider.cipher_suites = vec![ring::cipher_suite::TLS13_AES_256_GCM_SHA384];
    let provider = Arc::new(provider);

    let client_cfg = Arc::new(
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots_from(&pki.ca_cert))
            .with_client_auth_cert(pki.client_chain, pki.client_key)
            .expect("client auth"),
    );

    let transport = Transport::Tls {
        addr,
        server_name: "server.test".into(),
        client_config: client_cfg,
    };
    let connect = tokio::time::timeout(
        Duration::from_secs(3),
        tls_client_connect(transport, Config::default(), NoopHandler),
    )
    .await
    .expect("connect must not hang");

    assert!(
        connect.is_err(),
        "handshake must fail when cipher suites do not intersect"
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn mtls_handshake_and_interrogation_roundtrip() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let pki = build_pki();

    let server_cfg = server_config_requiring_client_cert(
        pki.server_chain.clone(),
        pki.server_key.clone_key(),
        roots_from(&pki.ca_cert),
    )
    .expect("server cfg");

    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = TlsServer::bind(bind, server_cfg, Config::default())
        .await
        .expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_handle = tokio::spawn(async move {
        let mut conn = server.accept().await.expect("accept");
        // Drain a few events to ensure the driver is alive after the
        // handshake; the test only needs the connection to come up.
        for _ in 0..3 {
            match tokio::time::timeout(Duration::from_secs(3), conn.recv()).await {
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
    });

    let client_cfg = client_config_with_client_cert(
        roots_from(&pki.ca_cert),
        pki.client_chain,
        pki.client_key,
    )
    .expect("client cfg");

    let transport = Transport::Tls {
        addr,
        server_name: "server.test".into(),
        client_config: client_cfg,
    };

    let mut client = tls_client_connect(transport, Config::default(), NoopHandler)
        .await
        .expect("tls_client_connect (mTLS)");

    // Fire one interrogation to prove the data path works after handshake.
    let interrogation = C_IC_NA_1 {
        objects: vec![(Ioa(0), Qoi::GENERAL)],
    };
    client
        .send(
            Cot::act(),
            CommonAddress(1),
            Vsq::single(1),
            &interrogation,
        )
        .await
        .expect("send interrogation");

    // Wait until we observe at least one ClientEvent (Active state change
    // is the natural first one).
    let evt = tokio::time::timeout(Duration::from_secs(3), client.recv())
        .await
        .expect("client recv timed out")
        .expect("client recv None");
    assert!(matches!(
        evt,
        ClientEvent::StateChanged(_) | ClientEvent::Asdu(_)
    ));

    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

#[tokio::test(flavor = "current_thread")]
async fn mtls_rejects_client_without_certificate() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let pki = build_pki();

    let server_cfg = server_config_requiring_client_cert(
        pki.server_chain.clone(),
        pki.server_key.clone_key(),
        roots_from(&pki.ca_cert),
    )
    .expect("server cfg");

    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = TlsServer::bind(bind, server_cfg, Config::default())
        .await
        .expect("bind");
    let addr = server.local_addr().expect("local_addr");

    // Accept attempt runs in the background; it should fail because the
    // client never presents a certificate.
    let server_handle = tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(2), server.accept()).await;
    });

    // No-client-auth config: this is what an attacker without a valid
    // client cert would send.
    let client_cfg = client_config_with_roots(roots_from(&pki.ca_cert));
    let transport = Transport::Tls {
        addr,
        server_name: "server.test".into(),
        client_config: client_cfg,
    };

    // TLS 1.3 lets the client finish its half of the handshake before the
    // server's "missing client cert" alert arrives, so `tls_client_connect`
    // may return Ok and the rejection only surfaces when the driver tries
    // to use the (closed) stream. Either outcome — failed connect, or a
    // driver that immediately reports `Closed` — counts as the server
    // refusing the unauthenticated peer.
    let connect = tokio::time::timeout(
        Duration::from_secs(3),
        tls_client_connect(transport, Config::default(), NoopHandler),
    )
    .await
    .expect("connect attempt should not hang");

    match connect {
        Err(_) => { /* server rejected during handshake — pass */ }
        Ok(mut client) => {
            // Server must close the connection promptly because the TLS
            // session is unusable. Drain a few events; the first one is
            // typically the local StateChanged(Starting) from the driver
            // queueing STARTDT_act before the TCP/TLS stream errors out.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let mut closed_or_dropped = false;
            loop {
                let evt =
                    match tokio::time::timeout_at(deadline, client.recv()).await {
                        Ok(e) => e,
                        Err(_) => break,
                    };
                match evt {
                    None => {
                        closed_or_dropped = true;
                        break;
                    }
                    Some(ClientEvent::Closed(_)) => {
                        closed_or_dropped = true;
                        break;
                    }
                    _ => continue,
                }
            }
            assert!(
                closed_or_dropped,
                "mTLS server must reject a client that does not present a certificate"
            );
        }
    }

    let _ = server_handle.await;
}

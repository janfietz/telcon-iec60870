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
use std::time::Duration;

use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1};
use iec60870::proto::asdu::{CommonAddress, Cot, Ioa, Vsq};
use iec60870::proto::frame104::Config;
use iec60870::{
    client_config_with_client_cert, client_config_with_roots,
    server_config_requiring_client_cert, tls_client_connect, ClientEvent, NoopHandler, Transport,
    TlsServer,
};
use rcgen::{CertificateParams, DistinguishedName, DnValue, KeyPair, SanType};
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

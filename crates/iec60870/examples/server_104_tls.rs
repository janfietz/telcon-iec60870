//! IEC 60870-5-104 TLS outstation example.
//!
//! Binds 0.0.0.0:19998 (the IEC 62351-3 TLS port), accepts connections over
//! TLS, and answers general interrogations with synthetic measurements.
//!
//! A self-signed certificate for `iec60870.local` is generated at startup and
//! written to:
//!   target/iec60870_demo_cert.pem
//!   target/iec60870_demo_key.pem
//!
//! The client example reads those files to trust the server.
//!
//! ```text
//! RUST_LOG=iec60870=debug cargo run --example server_104_tls --features tls
//! ```

use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{Qds, Quality, Siq, R32};
use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_ME_NC_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame104::Config;
use iec60870::{DefaultLoggingHandler, ServerEvent, TlsServer};
use rcgen::{CertificateParams, DistinguishedName, DnValue, KeyPair, SanType};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;

const CERT_PATH: &str = "target/iec60870_demo_cert.pem";
const KEY_PATH: &str = "target/iec60870_demo_key.pem";

fn encode<P: AsduPayload>(payload: &P, cot: Cot, vsq: Vsq) -> Vec<u8> {
    let asdu = Asdu::from_payload(cot, CommonAddress(1), vsq, payload, AsduAddressing::IEC104);
    let mut buf = BytesMut::new();
    asdu.encode(&mut buf, AsduAddressing::IEC104);
    buf.to_vec()
}

/// Load or generate a self-signed certificate for `iec60870.local`.
///
/// Writes PEM-encoded cert + key to `target/` for the client to consume.
fn load_or_create_cert(
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivatePkcs8KeyDer<'static>)> {
    // Re-use existing files if they are already present.
    if Path::new(CERT_PATH).exists() && Path::new(KEY_PATH).exists() {
        tracing::info!("reusing existing demo cert at {CERT_PATH}");
        let cert_pem = fs::read_to_string(CERT_PATH)?;
        let key_pem = fs::read_to_string(KEY_PATH)?;

        let cert_der =
            rustls_pemfile::certs(&mut cert_pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
        let key_der = rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_bytes())
            .next()
            .ok_or_else(|| anyhow::anyhow!("no private key in {KEY_PATH}"))??;
        return Ok((cert_der, key_der));
    }

    tracing::info!("generating self-signed demo certificate for iec60870.local");
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![
        SanType::DnsName("iec60870.local".try_into()?),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    ];
    let mut dn = DistinguishedName::new();
    dn.push(
        rcgen::DnType::CommonName,
        DnValue::Utf8String("iec60870.local".into()),
    );
    params.distinguished_name = dn;

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    // Ensure target/ exists.
    fs::create_dir_all("target")?;
    fs::write(CERT_PATH, cert.pem())?;
    fs::write(KEY_PATH, key_pair.serialize_pem())?;
    tracing::info!("cert written to {CERT_PATH}, key to {KEY_PATH}");

    let cert_der = vec![cert.der().clone()];
    let key_pem = key_pair.serialize_pem();
    let key_der = rustls_pemfile::pkcs8_private_keys(&mut key_pem.as_bytes())
        .next()
        .ok_or_else(|| anyhow::anyhow!("could not re-parse generated key"))??;

    Ok((cert_der, key_der))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info".into()),
        )
        .init();

    let (certs, key) = load_or_create_cert()?;

    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key.into())?;

    let bind = (Ipv4Addr::UNSPECIFIED, 19998).into();
    let server = TlsServer::bind(bind, Arc::new(server_config), Config::default()).await?;
    tracing::info!(addr = ?server.local_addr()?, "TLS server listening");

    loop {
        let mut conn = server.accept_with(DefaultLoggingHandler).await?;
        tracing::info!(peer = ?conn.peer(), "TLS client connected");

        tokio::spawn(async move {
            while let Some(evt) = conn.recv().await {
                match evt {
                    ServerEvent::Asdu(bytes) => {
                        let parsed = match Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104) {
                            Ok(a) => a,
                            Err(e) => {
                                tracing::warn!(?e, "failed to decode incoming asdu");
                                continue;
                            }
                        };
                        tracing::info!(type_id = parsed.type_id, "incoming asdu");

                        if parsed.type_id == C_IC_NA_1::TYPE_ID {
                            let ack = C_IC_NA_1 {
                                objects: vec![(Ioa(0), Qoi::GENERAL)],
                            };
                            let _ = conn
                                .send_asdu(encode(
                                    &ack,
                                    Cot::with(Cause::ACTIVATION_CON),
                                    Vsq::single(1),
                                ))
                                .await;

                            let sp = M_SP_NA_1 {
                                objects: vec![(
                                    Ioa(100),
                                    Siq {
                                        on: true,
                                        quality: Quality::default(),
                                    },
                                )],
                            };
                            let _ = conn
                                .send_asdu(encode(
                                    &sp,
                                    Cot::with(Cause::INTERROGATED_GENERAL),
                                    Vsq::single(1),
                                ))
                                .await;

                            let me = M_ME_NC_1 {
                                objects: vec![
                                    (Ioa(200), (R32(50.0), Qds::default())),
                                    (Ioa(201), (R32(51.5), Qds::default())),
                                ],
                            };
                            let _ = conn
                                .send_asdu(encode(
                                    &me,
                                    Cot::with(Cause::INTERROGATED_GENERAL),
                                    Vsq::single(2),
                                ))
                                .await;

                            let term = C_IC_NA_1 {
                                objects: vec![(Ioa(0), Qoi::GENERAL)],
                            };
                            let _ = conn
                                .send_asdu(encode(
                                    &term,
                                    Cot::with(Cause::ACTIVATION_TERMINATION),
                                    Vsq::single(1),
                                ))
                                .await;
                        }
                    }
                    ServerEvent::StateChanged(state) => {
                        tracing::info!(?state, "state changed");
                    }
                    ServerEvent::Closed(reason) => {
                        tracing::info!(?reason, "connection closed");
                        break;
                    }
                }
            }
        });
    }
}

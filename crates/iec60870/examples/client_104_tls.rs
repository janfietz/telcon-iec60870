//! IEC 60870-5-104 TLS client example.
//!
//! Connects to the TLS outstation (default 127.0.0.1:19998), sends a general
//! interrogation, and prints the resulting ASDUs. Reads the self-signed
//! demo certificate written by `server_104_tls` from
//! `target/iec60870_demo_cert.pem`.
//!
//! ```text
//! RUST_LOG=iec60870=debug cargo run --example client_104_tls --features tls
//! ```

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_ME_NC_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame104::Config;
use iec60870::{tls_client_connect, ClientEvent, Transport};
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

const CERT_PATH: &str = "target/iec60870_demo_cert.pem";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info".into()),
        )
        .init();

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:19998".into())
        .parse()?;

    // Load the demo certificate written by server_104_tls.
    let cert_pem = fs::read_to_string(CERT_PATH)
        .map_err(|e| anyhow::anyhow!("cannot read {CERT_PATH}: {e}  — run server_104_tls first"))?;
    let cert_ders: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;

    let mut roots = RootCertStore::empty();
    for cert in cert_ders {
        roots.add(cert)?;
    }

    let client_config = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );

    let transport = Transport::Tls {
        addr,
        server_name: "iec60870.local".into(),
        client_config,
    };

    tracing::info!(%addr, "connecting over TLS, sending general interrogation");
    let mut client = tls_client_connect(
        transport,
        Config::default(),
        iec60870::DefaultLoggingHandler,
    )
    .await?;

    let interrogation = C_IC_NA_1 {
        objects: vec![(Ioa(0), Qoi::GENERAL)],
    };
    let mut buf = BytesMut::new();
    Asdu::from_payload(
        Cot::with(Cause::ACTIVATION),
        CommonAddress(1),
        Vsq::single(1),
        &interrogation,
        AsduAddressing::IEC104,
    )
    .encode(&mut buf, AsduAddressing::IEC104);
    client.send_asdu(buf.to_vec()).await?;

    // Print responses for up to 10 seconds, then exit cleanly.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let evt = match tokio::time::timeout_at(deadline, client.recv()).await {
            Ok(Some(e)) => e,
            _ => break,
        };
        match evt {
            ClientEvent::Asdu(bytes) => {
                let parsed = Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104)?;
                match parsed.type_id {
                    M_SP_NA_1::TYPE_ID => {
                        let p: M_SP_NA_1 = parsed.decode_payload(AsduAddressing::IEC104)?;
                        for (ioa, siq) in &p.objects {
                            println!("[SP] ioa={} on={} quality={:?}", ioa.0, siq.on, siq.quality);
                        }
                    }
                    M_ME_NC_1::TYPE_ID => {
                        let p: M_ME_NC_1 = parsed.decode_payload(AsduAddressing::IEC104)?;
                        for (ioa, (value, qds)) in &p.objects {
                            println!("[ME] ioa={} value={} quality={:?}", ioa.0, value.0, qds);
                        }
                    }
                    other => println!("[??] type_id={other} cot={:?}", parsed.cot),
                }
            }
            ClientEvent::StateChanged(state) => tracing::info!(?state, "state changed"),
            ClientEvent::Closed(reason) => {
                tracing::info!(?reason, "connection closed");
                break;
            }
        }
    }
    Ok(())
}

//! IEC 60870-5-104 client example.
//!
//! Connects to 127.0.0.1:2404, sends a general interrogation, and prints
//! the resulting ASDUs as the outstation responds. Run with:
//!
//! ```text
//! RUST_LOG=iec60870=debug cargo run --example client_104
//! ```

use std::time::Duration;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_ME_NC_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame104::Config;
use iec60870::{Client104, ClientEvent, Transport};

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
        .unwrap_or_else(|| "127.0.0.1:2404".into())
        .parse()?;

    let mut client = Client104::connect(Transport::tcp(addr), Config::default()).await?;
    tracing::info!(%addr, "connected, sending general interrogation");

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
                match parsed.type_id() {
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
                    other => println!("[??] type_id={other} cot={:?}", parsed.cot()),
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

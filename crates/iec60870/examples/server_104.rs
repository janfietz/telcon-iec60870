//! IEC 60870-5-104 outstation example.
//!
//! Binds 0.0.0.0:2404, accepts one connection, and answers a general
//! interrogation with a few synthetic measurements. Run with:
//!
//! ```text
//! RUST_LOG=iec60870=debug,iec60870::state=info cargo run --example server_104
//! ```

use std::net::Ipv4Addr;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{Qds, Quality, Siq, R32};
use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_ME_NC_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame104::Config;
use iec60870::{DefaultLoggingHandler, Server104, ServerEvent};

fn encode<P: AsduPayload>(payload: &P, cot: Cot, vsq: Vsq) -> Vec<u8> {
    let asdu = Asdu::from_payload(cot, CommonAddress(1), vsq, payload, AsduAddressing::IEC104);
    let mut buf = BytesMut::new();
    asdu.encode(&mut buf, AsduAddressing::IEC104);
    buf.to_vec()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info".into()),
        )
        .init();

    let bind = (Ipv4Addr::UNSPECIFIED, 2404).into();
    let server = Server104::bind(bind, Config::default()).await?;
    tracing::info!(addr = ?server.local_addr()?, "server listening");

    loop {
        let mut conn = server.accept_with(DefaultLoggingHandler).await?;
        tracing::info!(peer = ?conn.peer(), "client connected");

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
                        tracing::info!(type_id = parsed.type_id(), "incoming asdu");

                        // Answer a general interrogation with a few synthetic
                        // measurements (one single-point + two float values).
                        if parsed.type_id() == C_IC_NA_1::TYPE_ID {
                            // ACK the interrogation with COT = ActivationCon.
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

                            // Activation termination.
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

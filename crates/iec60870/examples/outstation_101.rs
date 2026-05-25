//! IEC 60870-5-101 outstation (secondary station) example.
//!
//! Opens a serial port (path from argv1 or a platform default), waits for
//! the master to reset the link, then serves a small synthetic measurement
//! bundle whenever the master polls class-1 data. The measurement bundle is
//! identical in spirit to `server_104`'s interrogation handler.
//!
//! # Pairing with `master_101`
//!
//! Use `socat` to create a pseudo-terminal pair:
//!
//! ```text
//! socat -d -d pty,raw,echo=0 pty,raw,echo=0
//! # socat prints two PTY paths, e.g. /dev/pts/3 and /dev/pts/4
//! cargo run --example outstation_101 --features serial -- /dev/pts/3 &
//! cargo run --example master_101 --features serial -- /dev/pts/4
//! ```
//!
//! Run with:
//!
//! ```text
//! RUST_LOG=iec60870=info cargo run --example outstation_101 --features serial -- /dev/pts/3
//! ```

use std::time::Duration;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{Qds, Quality, Siq, R32};
use iec60870::proto::asdu::types::{C_IC_NA_1, M_ME_NC_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame101::frame::{LinkAddress, LinkAddressSize};
use iec60870::proto::frame101::link::{Config as LinkConfig, LinkState};
use iec60870::serial::SerialSettings;
use iec60870::{Outstation101, Outstation101Event};

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

    let path = std::env::args().nth(1).unwrap_or_else(|| {
        #[cfg(windows)]
        return "COM3".into();
        #[cfg(not(windows))]
        "/dev/ttyUSB0".into()
    });

    let config = LinkConfig {
        link_address: LinkAddress(1),
        addr_size: LinkAddressSize::One,
        ..LinkConfig::default()
    };

    tracing::info!(port = %path, "outstation opening serial port");
    let mut outstation = Outstation101::open(&path, SerialSettings::default(), config).await?;

    // Wait up to 30 s for the master to reset us to Ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout_at(deadline, outstation.recv()).await {
            Ok(Some(Outstation101Event::LinkStateChanged(LinkState::Ready))) => {
                tracing::info!("link is Ready — master has reset us");
                break;
            }
            Ok(Some(Outstation101Event::Closed(r))) => {
                tracing::error!(?r, "link closed before Ready");
                return Ok(());
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => {
                tracing::error!("timed out waiting for master reset");
                return Ok(());
            }
        }
    }

    // Pre-load one round of synthetic measurements into the send queue.
    // The link layer will transmit them when the master next polls class-1.
    let enqueue_bundle = |_outstation: &Outstation101| {
        // Single-point: IOA 100, ON.
        let sp = M_SP_NA_1 {
            objects: vec![(
                Ioa(100),
                Siq {
                    on: true,
                    quality: Quality::default(),
                },
            )],
        };
        let sp_bytes = encode(&sp, Cot::with(Cause::SPONTANEOUS), Vsq::single(1));

        // Float measurements: IOA 200 = 50.0, IOA 201 = 51.5.
        let me = M_ME_NC_1 {
            objects: vec![
                (Ioa(200), (R32(50.0), Qds::default())),
                (Ioa(201), (R32(51.5), Qds::default())),
            ],
        };
        let me_bytes = encode(&me, Cot::with(Cause::SPONTANEOUS), Vsq::single(2));

        (sp_bytes, me_bytes)
    };

    let (sp_bytes, me_bytes) = enqueue_bundle(&outstation);
    outstation.send_asdu(sp_bytes).await?;
    outstation.send_asdu(me_bytes).await?;
    tracing::info!("synthetic measurements queued");

    // Event loop — handle incoming ASDUs from the master (e.g. interrogation)
    // and re-queue measurements after each poll is consumed.
    loop {
        match tokio::time::timeout(Duration::from_secs(30), outstation.recv()).await {
            Ok(Some(Outstation101Event::Asdu(bytes))) => {
                match Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104) {
                    Ok(parsed) => {
                        tracing::info!(type_id = parsed.type_id(), "received asdu from master");
                        if parsed.type_id() == C_IC_NA_1::TYPE_ID {
                            // Master sent a general interrogation — respond.
                            let (sp_bytes, me_bytes) = enqueue_bundle(&outstation);
                            outstation.send_asdu(sp_bytes).await?;
                            outstation.send_asdu(me_bytes).await?;
                            tracing::info!("re-queued measurements in response to interrogation");
                        }
                    }
                    Err(e) => tracing::warn!(?e, "failed to decode master asdu"),
                }
            }
            Ok(Some(Outstation101Event::LinkStateChanged(state))) => {
                tracing::info!(?state, "link state changed");
            }
            Ok(Some(Outstation101Event::Closed(reason))) => {
                tracing::info!(?reason, "link closed");
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                tracing::info!("idle timeout — exiting");
                break;
            }
        }
    }

    Ok(())
}

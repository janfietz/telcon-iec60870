//! IEC 60870-5-101 master (primary station) example.
//!
//! Opens a serial port (path taken from argv1 or a platform default),
//! resets the link, polls the outstation for class-1 data and prints any
//! received ASDUs using the same `[SP] / [ME] / [??]` format as `client_104`.
//!
//! # Pairing with `outstation_101`
//!
//! Use `socat` to create a pseudo-terminal pair and run both examples in
//! separate shells:
//!
//! ```text
//! socat -d -d pty,raw,echo=0 pty,raw,echo=0
//! # socat will print two PTY paths, e.g. /dev/pts/3 and /dev/pts/4
//! cargo run --example outstation_101 --features serial -- /dev/pts/3 &
//! cargo run --example master_101 --features serial -- /dev/pts/4
//! ```
//!
//! Without `socat` on the path the example falls back to `/dev/ttyUSB0`
//! (Linux) / `COM3` (Windows) — change as needed for your hardware.
//!
//! Run with:
//!
//! ```text
//! RUST_LOG=iec60870=info cargo run --example master_101 --features serial -- /dev/pts/4
//! ```

use std::time::Duration;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_ME_NC_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame101::frame::{LinkAddress, LinkAddressSize};
use iec60870::proto::frame101::link::{Config as LinkConfig, LinkState};
use iec60870::serial::SerialSettings;
use iec60870::{Master101, Master101Event};

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

    tracing::info!(port = %path, "opening serial port");
    let mut master = Master101::open(&path, SerialSettings::default(), config).await?;

    // Reset the remote link — required before data transfer.
    master.reset_link().await?;
    tracing::info!("reset_link sent, waiting for Ready…");

    // Wait up to 5 s for the link to become Ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, master.recv()).await {
            Ok(Some(Master101Event::LinkStateChanged(LinkState::Ready))) => {
                tracing::info!("link is Ready");
                break;
            }
            Ok(Some(Master101Event::Closed(r))) => {
                tracing::error!(?r, "link closed before Ready");
                return Ok(());
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => {
                tracing::error!("timed out waiting for link Ready");
                return Ok(());
            }
        }
    }

    // Send a general interrogation encoded as ASDU (mirrors client_104 flow).
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
    master.send_asdu(buf.to_vec()).await?;
    tracing::info!("general interrogation sent");

    // Poll class-1 data from the outstation for up to 10 s.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut polls_remaining = 5u32;

    loop {
        // Periodically poll the outstation.
        if polls_remaining > 0 {
            master.request_class1().await?;
            polls_remaining -= 1;
        }

        let evt = match tokio::time::timeout_at(deadline, master.recv()).await {
            Ok(Some(e)) => e,
            _ => break,
        };

        match evt {
            Master101Event::Asdu(bytes) => {
                match Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104) {
                    Ok(parsed) => match parsed.type_id() {
                        M_SP_NA_1::TYPE_ID => {
                            let p: M_SP_NA_1 =
                                parsed.decode_payload(AsduAddressing::IEC104).unwrap();
                            for (ioa, siq) in &p.objects {
                                println!(
                                    "[SP] ioa={} on={} quality={:?}",
                                    ioa.0, siq.on, siq.quality
                                );
                            }
                        }
                        M_ME_NC_1::TYPE_ID => {
                            let p: M_ME_NC_1 =
                                parsed.decode_payload(AsduAddressing::IEC104).unwrap();
                            for (ioa, (value, qds)) in &p.objects {
                                println!("[ME] ioa={} value={} quality={:?}", ioa.0, value.0, qds);
                            }
                        }
                        other => println!("[??] type_id={other} cot={:?}", parsed.cot()),
                    },
                    Err(e) => tracing::warn!(?e, "failed to decode asdu"),
                }
            }
            Master101Event::LinkStateChanged(state) => {
                tracing::info!(?state, "link state changed");
            }
            Master101Event::Closed(reason) => {
                tracing::info!(?reason, "link closed");
                break;
            }
            _ => {}
        }

        // Small delay between polls to avoid flooding the bus.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Ok(())
}

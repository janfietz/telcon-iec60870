//! End-to-end integration tests for the IEC 60870-5-101 async layer.
//!
//! Uses `tokio::io::duplex` as a pseudo serial pair — no real hardware
//! needed. Both ends are fed into the 101 driver which is generic over any
//! `AsyncRead + AsyncWrite + Unpin` stream.
//!
//! Two scenarios are tested:
//!
//! 1. Master resets the link, outstation ACKs, master sends
//!    `USER_DATA_CONFIRMED` with a synthetic ASDU, outstation delivers it.
//! 2. Master polls class-1 data, outstation responds with an ASDU, master
//!    delivers it.

use std::time::Duration;

use iec60870::proto::frame101::frame::{LinkAddress, LinkAddressSize};
use iec60870::proto::frame101::link::{Config as LinkConfig, LinkState};
use iec60870::{Master101, Master101Event, Outstation101, Outstation101Event};

fn master_config() -> LinkConfig {
    LinkConfig {
        link_address: LinkAddress(1),
        addr_size: LinkAddressSize::One,
        timeout: Duration::from_secs(2),
        max_retries: 3,
    }
}

fn outstation_config() -> LinkConfig {
    LinkConfig {
        link_address: LinkAddress(1),
        addr_size: LinkAddressSize::One,
        timeout: Duration::from_secs(2),
        max_retries: 3,
    }
}

/// Test 1: master resets link, outstation ACKs and becomes Ready, master
/// sends a USER_DATA_CONFIRMED ASDU, outstation delivers it.
#[tokio::test(flavor = "current_thread")]
async fn loopback_reset_and_user_data_confirmed() {
    // Create a duplex pair — master writes to `master_stream`, outstation
    // reads from `outstation_stream` and vice versa.
    let (master_stream, outstation_stream) = tokio::io::duplex(4096);

    let outstation_handle = tokio::spawn(async move {
        let mut outstation = Outstation101::open_stream(outstation_stream, outstation_config());

        // Expect: LinkStateChanged(Ready) after the reset
        let evt = tokio::time::timeout(Duration::from_secs(3), outstation.recv())
            .await
            .expect("timeout waiting for outstation state change")
            .expect("outstation driver closed unexpectedly");
        assert!(
            matches!(evt, Outstation101Event::LinkStateChanged(LinkState::Ready)),
            "expected LinkStateChanged(Ready), got {evt:?}"
        );

        // Expect the ASDU delivery
        let asdu = tokio::time::timeout(Duration::from_secs(3), outstation.recv_asdu())
            .await
            .expect("timeout waiting for outstation asdu")
            .expect("outstation never got an asdu");
        assert_eq!(asdu, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    });

    let mut master = Master101::open_stream(master_stream, master_config());

    // Issue RESET_REMOTE_LINK
    master.reset_link().await.expect("reset_link failed");

    // Expect: LinkStateChanged(Ready) on the master side
    let state_evt = tokio::time::timeout(Duration::from_secs(3), master.recv())
        .await
        .expect("timeout waiting for master state change")
        .expect("master driver closed unexpectedly");
    assert!(
        matches!(
            state_evt,
            Master101Event::LinkStateChanged(LinkState::Ready)
        ),
        "expected master LinkStateChanged(Ready), got {state_evt:?}"
    );

    // Send an ASDU as USER_DATA_CONFIRMED
    master
        .send_asdu(vec![0xDE, 0xAD, 0xBE, 0xEF])
        .await
        .expect("send_asdu failed");

    // Wait for the outstation task to complete its assertions.
    outstation_handle.await.expect("outstation task panicked");
}

/// Test 2: master polls class-1 data, outstation responds with user data,
/// master delivers the ASDU to its receive channel.
#[tokio::test(flavor = "current_thread")]
async fn loopback_class1_poll_delivers_asdu_to_master() {
    let (master_stream, outstation_stream) = tokio::io::duplex(4096);

    let synthetic_asdu = vec![0x01, 0x02, 0x03, 0x04];
    let expected = synthetic_asdu.clone();

    let outstation_handle = tokio::spawn(async move {
        let mut outstation = Outstation101::open_stream(outstation_stream, outstation_config());

        // Wait for the outstation to come up (reset handled by master first).
        let evt = tokio::time::timeout(Duration::from_secs(3), outstation.recv())
            .await
            .expect("timeout waiting for outstation state change")
            .expect("outstation driver closed unexpectedly");
        assert!(
            matches!(evt, Outstation101Event::LinkStateChanged(LinkState::Ready)),
            "expected Ready, got {evt:?}"
        );

        // Enqueue an ASDU for the master's next poll.
        outstation
            .send_asdu(synthetic_asdu)
            .await
            .expect("outstation send_asdu failed");

        // Keep the outstation alive while the master polls.
        // Drain events until we see Closed (master stops) or timeout.
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match outstation.recv().await {
                    Some(Outstation101Event::Closed(_)) | None => break,
                    _ => {}
                }
            }
        })
        .await;
    });

    let mut master = Master101::open_stream(master_stream, master_config());

    // Reset the link first so the outstation transitions to Ready.
    master.reset_link().await.expect("reset_link failed");

    // Wait for master's LinkStateChanged(Ready).
    let _ = tokio::time::timeout(Duration::from_secs(3), master.recv())
        .await
        .expect("timeout on master ready");

    // Give the outstation a moment to enqueue its data.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Poll class-1 data.
    master
        .request_class1()
        .await
        .expect("request_class1 failed");

    // Master should receive the ASDU.
    let asdu = tokio::time::timeout(Duration::from_secs(3), master.recv_asdu())
        .await
        .expect("timeout waiting for master to receive class1 asdu")
        .expect("master never got an asdu");
    assert_eq!(asdu, expected);

    outstation_handle.await.expect("outstation task panicked");
}

//! Oversized-ASDU rejection across the async API.
//!
//! The IEC 60870-5-104 APDU length field is a single octet, capping the
//! ASDU portion at 249 bytes. Earlier revisions of this crate silently
//! truncated oversized ASDUs at encode time, putting a corrupt frame on
//! the wire. These tests verify the user-facing send paths reject the
//! call up-front with `Error::Protocol(AsduTooLong)` and never produce
//! a partial frame.

use std::net::Ipv4Addr;
use std::time::Duration;

use iec60870::proto::frame104::{apdu::MAX_ASDU_LEN, Config};
use iec60870::{Client104, Error, RedundancyServer, Server104, Transport};
use tokio::time::sleep;

fn is_asdu_too_long(err: &Error) -> bool {
    matches!(
        err,
        Error::Protocol(iec60870::proto::Error::AsduTooLong { .. }),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn client_send_rejects_oversized_asdu() {
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let _server_task = tokio::spawn(async move {
        let _ = server.accept().await;
    });

    let client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    let oversized = vec![0u8; MAX_ASDU_LEN + 1];
    let err = client
        .send_asdu(oversized)
        .await
        .expect_err("oversized send must error");
    assert!(is_asdu_too_long(&err), "expected AsduTooLong, got {err:?}",);
}

#[tokio::test(flavor = "current_thread")]
async fn server_send_rejects_oversized_asdu() {
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("accept");
        let oversized = vec![0u8; MAX_ASDU_LEN + 1];
        conn.send_asdu(oversized).await
    });

    let _client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    let result = tokio::time::timeout(Duration::from_secs(3), server_task)
        .await
        .expect("server task timed out")
        .expect("server task panicked");
    let err = result.expect_err("oversized send must error");
    assert!(is_asdu_too_long(&err), "expected AsduTooLong, got {err:?}",);
}

#[tokio::test(flavor = "current_thread")]
async fn redundancy_send_active_rejects_oversized_asdu() {
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let rs = RedundancyServer::spawn(server);

    let _client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    // Wait until a peer is active so we exercise the routing path, not the
    // NoActivePeer short-circuit (which would mask the size check).
    for _ in 0..100 {
        if rs.active_peer().await.is_some() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    assert!(rs.active_peer().await.is_some(), "no active peer");

    let oversized = vec![0u8; MAX_ASDU_LEN + 1];
    let err = rs
        .send_active(oversized)
        .await
        .expect_err("oversized send must error");
    assert!(is_asdu_too_long(&err), "expected AsduTooLong, got {err:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn boundary_size_is_accepted() {
    // Exactly MAX_ASDU_LEN must still encode successfully — the rejection
    // is for > MAX_ASDU_LEN, not >=.
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let _server_task = tokio::spawn(async move {
        let _ = server.accept().await;
    });

    let client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    let at_max = vec![0u8; MAX_ASDU_LEN];
    client
        .send_asdu(at_max)
        .await
        .expect("size at the limit must encode");
}

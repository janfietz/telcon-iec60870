//! Integration tests for [`iec60870::RedundancyServer`].
//!
//! Mirrors IEC 60870-5-104 §5.1: many TCP sessions are accepted in parallel
//! but at most one peer is in data-transfer state at any moment, and the
//! redundancy manager surfaces that single "active" peer to the application.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{Quality, Siq};
use iec60870::proto::asdu::types::M_SP_NA_1;
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame104::Config;
use iec60870::{Client104, RedundancyConfig, RedundancyServer, Server104, Transport};
use std::sync::Arc;
use tokio::time::sleep;

async fn bind_redundancy() -> (RedundancyServer, SocketAddr) {
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();
    (RedundancyServer::spawn(server), addr)
}

/// Poll `active_peer()` until it transitions to `Some(_)`. Panics on timeout.
async fn wait_until_active(rs: &RedundancyServer) -> SocketAddr {
    for _ in 0..100 {
        if let Some(p) = rs.active_peer().await {
            return p;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("no peer became active within 2s");
}

/// Poll `active_peer()` until it differs from `prev` and is `Some`.
async fn wait_for_failover(rs: &RedundancyServer, prev: SocketAddr) -> SocketAddr {
    for _ in 0..100 {
        let cur = rs.active_peer().await;
        if let Some(p) = cur {
            if p != prev {
                return p;
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("failover from {prev} never happened within 2s");
}

#[tokio::test(flavor = "current_thread")]
async fn connected_client_with_startdt_becomes_active() {
    let (rs, addr) = bind_redundancy().await;
    let _client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");
    let _peer = wait_until_active(&rs).await;
}

#[tokio::test(flavor = "current_thread")]
async fn second_client_startdt_demotes_first() {
    let (rs, addr) = bind_redundancy().await;

    let _c1 = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("c1 connect");
    let first = wait_until_active(&rs).await;

    let _c2 = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("c2 connect");
    let second = wait_for_failover(&rs, first).await;

    assert_ne!(
        first, second,
        "second STARTDT should take over the active slot"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_active_client_clears_active_slot() {
    let (rs, addr) = bind_redundancy().await;

    let client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");
    let _peer = wait_until_active(&rs).await;
    drop(client);

    for _ in 0..100 {
        if rs.active_peer().await.is_none() {
            return;
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("active slot was not cleared after client disconnect");
}

#[tokio::test(flavor = "current_thread")]
async fn send_active_routes_to_current_active_peer() {
    let (rs, addr) = bind_redundancy().await;
    let mut client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");
    let _peer = wait_until_active(&rs).await;

    let payload = M_SP_NA_1 {
        objects: vec![(
            Ioa(7),
            Siq {
                on: true,
                quality: Quality::default(),
            },
        )],
    };
    let asdu = Asdu::from_payload(
        Cot::with(Cause::SPONTANEOUS),
        CommonAddress(1),
        Vsq::single(1),
        &payload,
        AsduAddressing::IEC104,
    );
    let mut buf = BytesMut::new();
    asdu.encode(&mut buf, AsduAddressing::IEC104);

    rs.send_active(buf.to_vec())
        .await
        .expect("send_active routes to active peer");

    let bytes = tokio::time::timeout(Duration::from_secs(3), client.recv_asdu())
        .await
        .expect("client recv timed out")
        .expect("client did not receive ASDU");
    let parsed = Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104).unwrap();
    assert_eq!(parsed.type_id, M_SP_NA_1::TYPE_ID);
}

#[tokio::test(flavor = "current_thread")]
async fn send_active_errors_when_no_peer_active() {
    let (rs, _addr) = bind_redundancy().await;
    let err = rs
        .send_active(vec![0u8; 6])
        .await
        .expect_err("send_active without an active peer must error");
    // Concrete variant check via Display; full Error import would need a
    // pub re-export of the variant, which isn't worth doing just for this.
    let msg = format!("{err}");
    assert!(
        msg.contains("no active peer"),
        "unexpected error message: {msg}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn demoted_peer_observes_disconnect() {
    // Demoting is implemented by closing the prior active peer's TCP
    // session (the spec forbids server-initiated STOPDT). Verify the
    // demoted client side actually observes that close.
    let (rs, addr) = bind_redundancy().await;

    let mut c1 = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("c1 connect");
    let first = wait_until_active(&rs).await;

    let _c2 = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("c2 connect");
    let _second = wait_for_failover(&rs, first).await;

    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match c1.recv().await {
                None => return true,
                Some(iec60870::ClientEvent::Closed(_)) => return true,
                Some(_) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "demoted client did not observe a connection close");
}

#[tokio::test(flavor = "current_thread")]
async fn max_peers_caps_concurrent_connections() {
    // Cap = 1. Connect a first client and let it become active. A second
    // accept must be refused: the manager drops its handle, which closes
    // the TCP session, so the second Client104 either fails to complete
    // STARTDT or observes a close shortly after connecting.
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let rs = RedundancyServer::spawn_with(
        server,
        RedundancyConfig {
            max_peers: 1,
            ..Default::default()
        },
    );

    let _c1 = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("c1 connect");
    wait_until_active(&rs).await;

    let mut c2 = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("c2 tcp connect");

    // c2 must close shortly: either StartDt never completes or Closed is
    // surfaced. Bounded wait to keep the test deterministic.
    let observed_close = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match c2.recv().await {
                None => return true,
                Some(iec60870::ClientEvent::Closed(_)) => return true,
                Some(_) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        observed_close,
        "second client should be refused when max_peers cap is reached"
    );

    // Manager's view: still exactly one peer.
    assert_eq!(rs.peers().await.len(), 1, "cap should keep peers ≤ 1");
}

#[tokio::test(flavor = "current_thread")]
async fn promote_filter_rejects_disallowed_peer() {
    // Filter rejects every promotion. The connecting client issues
    // STARTDT, the manager refuses to promote, and disconnects the peer.
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let rs = RedundancyServer::spawn_with(
        server,
        RedundancyConfig {
            promote_filter: Some(Arc::new(|_peer| false)),
            ..Default::default()
        },
    );

    let mut client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    // active_peer must stay None and the client must observe a close.
    let observed_close = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match client.recv().await {
                None => return true,
                Some(iec60870::ClientEvent::Closed(_)) => return true,
                Some(_) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(observed_close, "rejected peer should be disconnected");
    assert!(
        rs.active_peer().await.is_none(),
        "rejected peer must not become active"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn promote_filter_allows_matching_peer() {
    // Filter accepts only loopback addresses (which is what the test uses),
    // so the peer is promoted normally.
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();
    let rs = RedundancyServer::spawn_with(
        server,
        RedundancyConfig {
            promote_filter: Some(Arc::new(|peer: SocketAddr| peer.ip().is_loopback())),
            ..Default::default()
        },
    );

    let _client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");
    let _peer = wait_until_active(&rs).await;
}

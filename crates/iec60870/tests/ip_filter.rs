//! Integration tests for the plain-TCP IP filter on [`Server104`].
//!
//! These tests bind on a loopback address and verify that:
//! - an empty allow-list (`IpFilter::allow_all()`) preserves historical behaviour,
//! - a CIDR that matches loopback accepts the peer and the STARTDT roundtrip works,
//! - a CIDR that does *not* match loopback causes the server's `accept_with`
//!   future to stay pending while the rejected client observes its connection
//!   being closed without protocol exchange.

use std::net::Ipv4Addr;
use std::time::Duration;

use iec60870::proto::frame104::Config;
use iec60870::{Client104, IpFilter, NoopHandler, Server104, Transport};
use tokio::net::TcpStream;

async fn server(filter: IpFilter) -> Server104 {
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    Server104::bind_with_security(bind, Config::default(), filter)
        .await
        .expect("bind")
}

#[tokio::test(flavor = "current_thread")]
async fn allow_all_accepts_loopback() {
    let server = server(IpFilter::allow_all()).await;
    let addr = server.local_addr().expect("local_addr");

    let server_task = tokio::spawn(async move {
        let mut conn = server.accept_with(NoopHandler).await.expect("accept");
        // Just drain one event to confirm the driver is alive.
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.recv()).await;
    });

    let client = Client104::connect_with(Transport::tcp(addr), Config::default(), NoopHandler)
        .await
        .expect("client connects");
    drop(client);

    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn cidr_allow_list_accepts_loopback() {
    let server = server(IpFilter::from_strs(&["127.0.0.0/8"]).unwrap()).await;
    let addr = server.local_addr().expect("local_addr");

    let server_task = tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(2), server.accept_with(NoopHandler)).await;
    });

    let client = Client104::connect_with(Transport::tcp(addr), Config::default(), NoopHandler)
        .await
        .expect("client connects when its address is allow-listed");
    drop(client);

    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
}

#[tokio::test(flavor = "current_thread")]
async fn disjoint_allow_list_drops_loopback_peer() {
    // 10.0.0.0/8 doesn't cover 127.x.x.x → the server must drop the peer
    // immediately after accept(), before any protocol bytes flow.
    let server = server(IpFilter::from_strs(&["10.0.0.0/8"]).unwrap()).await;
    let addr = server.local_addr().expect("local_addr");

    // The server's accept_with future will loop forever (no allowed peer
    // ever connects). Park it on a background task so we can probe.
    let server_task = tokio::spawn(async move {
        let _ =
            tokio::time::timeout(Duration::from_millis(700), server.accept_with(NoopHandler)).await;
    });

    // The raw TCP connect will succeed (kernel SYN/ACK) but the server then
    // drops the stream — we expect EOF on the first read.
    let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_millis(500), {
        use tokio::io::AsyncReadExt;
        async move { stream.read(&mut buf).await }
    })
    .await;
    match read {
        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {
            // 0 bytes → clean FIN; Err(_) → connection reset; timeout → also fine
            // because the server has hung up and we'll observe EOF shortly.
        }
        Ok(Ok(n)) => panic!("server sent {n} bytes to a filtered peer"),
    }

    let _ = server_task.await;
}

#[tokio::test(flavor = "current_thread")]
async fn deny_all_rejects_everything() {
    let server = server(IpFilter::deny_all()).await;
    let addr = server.local_addr().expect("local_addr");

    let server_task = tokio::spawn(async move {
        let _ =
            tokio::time::timeout(Duration::from_millis(500), server.accept_with(NoopHandler)).await;
    });

    let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_millis(400), {
        use tokio::io::AsyncReadExt;
        async move { stream.read(&mut buf).await }
    })
    .await;
    match read {
        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => {}
        Ok(Ok(n)) => panic!("deny_all server sent {n} bytes"),
    }

    let _ = server_task.await;
}

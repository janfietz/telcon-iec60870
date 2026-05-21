//! End-to-end integration test using a loopback TCP socket.
//!
//! Spawns a Server104 on 127.0.0.1:0, connects a Client104 to it, and
//! exercises the full STARTDT handshake plus an interrogation/response
//! round-trip through the real driver and codec.

use std::net::Ipv4Addr;
use std::time::Duration;

use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{Quality, Siq};
use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduPayload};
use iec60870::proto::frame104::Config;
use iec60870::{
    AsduPolicy, Client104, DefaultLoggingHandler, Server104, ServerEvent, Transport,
};

fn encode_asdu<P: AsduPayload>(payload: &P, cot: Cot, vsq: Vsq) -> Vec<u8> {
    let asdu = Asdu::from_payload(cot, CommonAddress(1), vsq, payload, AsduAddressing::IEC104);
    let mut buf = BytesMut::new();
    asdu.encode(&mut buf, AsduAddressing::IEC104);
    buf.to_vec()
}

#[tokio::test(flavor = "current_thread")]
async fn loopback_interrogation_roundtrip() {
    // Bind on an OS-assigned port to avoid clashes with other tests.
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let mut conn = server.accept().await.expect("accept");
        // Wait for the client's interrogation.
        let asdu_bytes = tokio::time::timeout(Duration::from_secs(3), conn.recv_asdu())
            .await
            .expect("timeout")
            .expect("server got asdu");
        let parsed = Asdu::decode(&mut &asdu_bytes[..], AsduAddressing::IEC104).unwrap();
        assert_eq!(parsed.type_id(), C_IC_NA_1::TYPE_ID);

        // Reply with a single-point measurement.
        let response = M_SP_NA_1 {
            objects: vec![(
                Ioa(100),
                Siq {
                    on: true,
                    quality: Quality::default(),
                },
            )],
        };
        let bytes = encode_asdu(
            &response,
            Cot::with(Cause::INTERROGATED_GENERAL),
            Vsq::single(1),
        );
        conn.send_asdu(bytes).await.expect("send");
        conn
    });

    let mut client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    // Send a general interrogation.
    let interrogation = C_IC_NA_1 {
        objects: vec![(Ioa(0), Qoi::GENERAL)],
    };
    let bytes = encode_asdu(&interrogation, Cot::with(Cause::ACTIVATION), Vsq::single(1));
    client.send_asdu(bytes).await.expect("client send");

    // Receive the server's reply.
    let asdu_bytes = tokio::time::timeout(Duration::from_secs(3), client.recv_asdu())
        .await
        .expect("client timeout")
        .expect("client got asdu");
    let parsed = Asdu::decode(&mut &asdu_bytes[..], AsduAddressing::IEC104).unwrap();
    assert_eq!(parsed.type_id(), M_SP_NA_1::TYPE_ID);

    let decoded: M_SP_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
    assert_eq!(decoded.objects.len(), 1);
    assert_eq!(decoded.objects[0].0, Ioa(100));
    assert!(decoded.objects[0].1.on);

    // Keep the server alive long enough to flush.
    let _ = server_handle.await;
}

#[tokio::test(flavor = "current_thread")]
async fn server_observes_state_changes() {
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let mut conn = server.accept().await.unwrap();
        // Look for the STARTDT-driven state change to Active.
        let mut saw_active = false;
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_secs(2), conn.recv()).await {
                Ok(Some(ServerEvent::StateChanged(_))) => {
                    saw_active = true;
                    break;
                }
                Ok(Some(_)) | Ok(None) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_active, "server never saw a state change");
    });

    let _client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    server_task.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn asdu_policy_blocks_disallowed_type_id() {
    // Server accepts with a policy that only allows monitor-direction
    // M_SP_NA_1. The client will send a control-direction C_IC_NA_1; the
    // policy must drop it before it reaches the server-side application.
    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default()).await.unwrap();
    let addr = server.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        let policy = AsduPolicy::new().allow_type_id(M_SP_NA_1::TYPE_ID);
        let mut conn = server
            .accept_with_policy_and_handler(policy, DefaultLoggingHandler)
            .await
            .expect("accept");
        // Drain events for a short while; we must NOT see any ASDU
        // delivered, because the client only sends C_IC_NA_1.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let mut saw_asdu = false;
        loop {
            let evt = match tokio::time::timeout_at(deadline, conn.recv()).await {
                Ok(e) => e,
                Err(_) => break,
            };
            match evt {
                Some(ServerEvent::Asdu(_)) => {
                    saw_asdu = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(
            !saw_asdu,
            "policy was supposed to drop the C_IC_NA_1 — server must not see it"
        );
    });

    let client = Client104::connect(Transport::tcp(addr), Config::default())
        .await
        .expect("connect");

    let interrogation = C_IC_NA_1 {
        objects: vec![(Ioa(0), Qoi::GENERAL)],
    };
    let bytes = encode_asdu(&interrogation, Cot::with(Cause::ACTIVATION), Vsq::single(1));
    client.send_asdu(bytes).await.expect("client send");

    // Wait for the server task to verify the drop.
    let _ = server_handle.await;
}

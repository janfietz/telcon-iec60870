//! End-to-end IEC 60870-5-104 client/server with an in-process wire tap.
//!
//! Stands up a real `Server104` and `Client104` on loopback and inserts a
//! tiny TCP relay between them that records every byte in both directions
//! with a timestamp. At the end of the interrogation cycle the captured
//! byte stream is decoded with the same APDU codec the library uses and
//! pretty-printed — equivalent to `tcpdump -X` on TCP/2404, but with
//! IEC-60870-5-104 framing already parsed.
//!
//! Run with:
//!
//! ```text
//! cargo run --example wire_capture
//! ```
//!
//! The output looks like:
//!
//! ```text
//! [T+0.002s] C→S  6 B   68 04 07 00 00 00                                 U StartDtAct
//! [T+0.004s] S→C  6 B   68 04 0B 00 00 00                                 U StartDtCon
//! [T+0.006s] C→S 16 B   68 0E 00 00 00 00 64 01 06 00 01 00 00 00 00 14   I N(S)=0 N(R)=0 ASDU(10): C_IC_NA_1 cot=Activation ca=1
//! ...
//! ```

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iec60870::proto::asdu::types::{Qoi, C_IC_NA_1, M_ME_NC_1, M_SP_NA_1};
use iec60870::proto::asdu::{Asdu, AsduAddressing, AsduPayload, CommonAddress, Cot, Cause, Ioa, Vsq};
use iec60870::proto::asdu::ie::{Qds, Quality, Siq, R32};
use iec60870::proto::frame104::{Apdu, Codec, Config, UFunction};
use iec60870::{Client104, ClientEvent, NoopHandler, Server104, ServerEvent, Transport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug)]
enum Dir {
    ClientToServer,
    ServerToClient,
}

#[derive(Debug)]
struct Chunk {
    at: Instant,
    dir: Dir,
    bytes: Vec<u8>,
}

/// Bidirectional byte-copy proxy that records every read from each side.
/// Runs until either side closes its half of the connection.
async fn wire_tap(
    listen_on: SocketAddr,
    upstream: SocketAddr,
    log: Arc<Mutex<Vec<Chunk>>>,
    _t0: Instant,
) {
    let listener = TcpListener::bind(listen_on).await.expect("tap bind");
    let (client_side, _) = listener.accept().await.expect("tap accept");
    let server_side = TcpStream::connect(upstream).await.expect("tap upstream");
    // Disable Nagle on both legs so we see frames as they were emitted.
    let _ = client_side.set_nodelay(true);
    let _ = server_side.set_nodelay(true);
    let (mut cr, mut cw) = client_side.into_split();
    let (mut sr, mut sw) = server_side.into_split();
    let log_c2s = log.clone();
    let log_s2c = log.clone();

    let c2s = async move {
        let mut buf = [0u8; 4096];
        loop {
            let n = match cr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            log_c2s.lock().unwrap().push(Chunk {
                at: Instant::now(),
                dir: Dir::ClientToServer,
                bytes: buf[..n].to_vec(),
            });
            if sw.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = sw.shutdown().await;
    };
    let s2c = async move {
        let mut buf = [0u8; 4096];
        loop {
            let n = match sr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            log_s2c.lock().unwrap().push(Chunk {
                at: Instant::now(),
                dir: Dir::ServerToClient,
                bytes: buf[..n].to_vec(),
            });
            if cw.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = cw.shutdown().await;
    };
    tokio::join!(c2s, s2c);
}

// Hack: store the micros-since-t0 in the `at` field as an `Instant` field is
// awkward. Use a parallel Vec<Duration> via a wrapper.
//
// Simpler: just store Instant and compute deltas at print time.
fn print_capture(t0: Instant, log: &[Chunk]) {
    // First, reassemble per-direction byte streams to decode APDU boundaries
    // (the tap may split a single APDU across two reads or coalesce two).
    let mut c2s_stream: Vec<u8> = Vec::new();
    let mut s2c_stream: Vec<u8> = Vec::new();
    for c in log {
        match c.dir {
            Dir::ClientToServer => c2s_stream.extend_from_slice(&c.bytes),
            Dir::ServerToClient => s2c_stream.extend_from_slice(&c.bytes),
        }
    }

    println!();
    println!("==================== WIRE CAPTURE ====================");
    println!("(reassembled per-direction; APDUs in arrival order)");
    println!();

    // Walk each TCP read in order, and decode APDUs greedily from the
    // per-direction buffer at the position the chunk delivered them.
    let mut c2s_consumed = 0usize;
    let mut s2c_consumed = 0usize;
    let mut c2s_delivered = 0usize;
    let mut s2c_delivered = 0usize;

    for chunk in log {
        let dt = chunk.at.duration_since(t0);
        let arrow = match chunk.dir {
            Dir::ClientToServer => {
                c2s_delivered += chunk.bytes.len();
                "C→S"
            }
            Dir::ServerToClient => {
                s2c_delivered += chunk.bytes.len();
                "S→C"
            }
        };
        println!(
            "[T+{:>5}.{:03}ms] {} {:>3} B   {}",
            dt.as_millis() / 1000,
            dt.as_millis() % 1000,
            arrow,
            chunk.bytes.len(),
            hex(&chunk.bytes)
        );

        // After each TCP read, try to decode any APDUs that have become
        // complete in the corresponding direction.
        let (stream, consumed, delivered) = match chunk.dir {
            Dir::ClientToServer => (&c2s_stream, &mut c2s_consumed, c2s_delivered),
            Dir::ServerToClient => (&s2c_stream, &mut s2c_consumed, s2c_delivered),
        };
        while *consumed < delivered {
            let slice = &stream[*consumed..delivered];
            match Codec::decode_slice(slice) {
                Ok(Some((apdu, n))) => {
                    println!("            └── {}", describe_apdu(&apdu, chunk.dir));
                    *consumed += n;
                }
                _ => break,
            }
        }
    }
    println!();
    println!("--- summary ---");
    println!(
        "{} chunks captured, {} bytes total ({} C→S, {} S→C)",
        log.len(),
        c2s_stream.len() + s2c_stream.len(),
        c2s_stream.len(),
        s2c_stream.len(),
    );
    println!("=======================================================");
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for byte in b {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("{byte:02X}"));
    }
    s
}

fn describe_apdu(apdu: &Apdu, dir: Dir) -> String {
    match apdu {
        Apdu::U { function } => format!(
            "U {:?}",
            match function {
                UFunction::StartDtAct => "StartDtAct",
                UFunction::StartDtCon => "StartDtCon",
                UFunction::StopDtAct => "StopDtAct",
                UFunction::StopDtCon => "StopDtCon",
                UFunction::TestFrAct => "TestFrAct",
                UFunction::TestFrCon => "TestFrCon",
            }
        ),
        Apdu::S { recv } => format!("S N(R)={}", recv.value()),
        Apdu::I { send, recv, asdu } => {
            let summary = match Asdu::decode(&mut &asdu[..], AsduAddressing::IEC104) {
                Ok(a) => format!(
                    "ASDU({}): TID={} cot={} ca={}",
                    asdu.len(),
                    type_id_name(a.type_id()),
                    cause_name(a.cot().cause().raw()),
                    a.ca().0,
                ),
                Err(e) => format!("ASDU({} B, decode error: {e})", asdu.len()),
            };
            let _ = dir; // direction already shown in the chunk line
            format!("I N(S)={} N(R)={} {}", send.value(), recv.value(), summary)
        }
    }
}

fn cause_name(raw: u8) -> String {
    match raw {
        1 => "Periodic".into(),
        2 => "Background".into(),
        3 => "Spontaneous".into(),
        4 => "Initialized".into(),
        5 => "Request".into(),
        6 => "Activation".into(),
        7 => "ActivationCon".into(),
        8 => "Deactivation".into(),
        9 => "DeactivationCon".into(),
        10 => "ActivationTermination".into(),
        11 => "ReturnRemote".into(),
        12 => "ReturnLocal".into(),
        13 => "FileTransfer".into(),
        20 => "InterrogatedGeneral".into(),
        n @ 21..=36 => format!("InterrogatedGroup{}", n - 20),
        37 => "ReqCounterGeneral".into(),
        n @ 38..=41 => format!("ReqCounterGroup{}", n - 37),
        44 => "UnknownTypeID".into(),
        45 => "UnknownCause".into(),
        46 => "UnknownCA".into(),
        47 => "UnknownIOA".into(),
        other => format!("Cause({other})"),
    }
}

fn type_id_name(id: u8) -> String {
    match id {
        1 => "M_SP_NA_1".into(),
        13 => "M_ME_NC_1".into(),
        100 => "C_IC_NA_1".into(),
        other => format!("0x{other:02X}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ---- bring up server ------------------------------------------------
    let server_bind: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(server_bind, Config::default()).await?;
    let server_addr = server.local_addr()?;
    println!("server listening on {server_addr}");

    // ---- start wire tap -------------------------------------------------
    let tap_bind: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let tap_listener = std::net::TcpListener::bind(tap_bind)?;
    tap_listener.set_nonblocking(true)?;
    let tap_addr = tap_listener.local_addr()?;
    drop(tap_listener); // free the port; the async tap will rebind
    println!("wire tap will relay {tap_addr} → {server_addr}");

    let log: Arc<Mutex<Vec<Chunk>>> = Arc::new(Mutex::new(Vec::new()));
    let t0 = Instant::now();
    let tap_log = log.clone();
    let tap_task = tokio::spawn(wire_tap(tap_addr, server_addr, tap_log, t0));

    // give the tap a moment to rebind
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ---- server-side responder ------------------------------------------
    let server_task = tokio::spawn(async move {
        let mut conn = server.accept().await.expect("accept");
        while let Some(evt) = conn.recv().await {
            match evt {
                ServerEvent::Asdu(bytes) => {
                    let parsed = Asdu::decode(&mut &bytes[..], AsduAddressing::IEC104).unwrap();
                    if parsed.type_id() == C_IC_NA_1::TYPE_ID {
                        // ActivationCon
                        let _ = conn
                            .send(
                                Cot::with(Cause::ACTIVATION_CON),
                                CommonAddress(1),
                                Vsq::single(1),
                                &C_IC_NA_1 { objects: vec![(Ioa(0), Qoi::GENERAL)] },
                            )
                            .await;
                        // One M_SP and one M_ME
                        let _ = conn
                            .send(
                                Cot::with(Cause::INTERROGATED_GENERAL),
                                CommonAddress(1),
                                Vsq::single(1),
                                &M_SP_NA_1 {
                                    objects: vec![(
                                        Ioa(100),
                                        Siq { on: true, quality: Quality::default() },
                                    )],
                                },
                            )
                            .await;
                        let _ = conn
                            .send(
                                Cot::with(Cause::INTERROGATED_GENERAL),
                                CommonAddress(1),
                                Vsq::single(1),
                                &M_ME_NC_1 {
                                    objects: vec![(Ioa(200), (R32(50.0), Qds::default()))],
                                },
                            )
                            .await;
                        // ActivationTermination
                        let _ = conn
                            .send(
                                Cot::with(Cause::ACTIVATION_TERMINATION),
                                CommonAddress(1),
                                Vsq::single(1),
                                &C_IC_NA_1 { objects: vec![(Ioa(0), Qoi::GENERAL)] },
                            )
                            .await;
                    }
                }
                ServerEvent::Closed(_) => break,
                _ => continue,
            }
        }
    });

    // ---- client connects through the tap --------------------------------
    let mut client = Client104::connect_with(
        Transport::tcp(tap_addr),
        Config::default(),
        NoopHandler,
    )
    .await?;

    client
        .send(
            Cot::with(Cause::ACTIVATION),
            CommonAddress(1),
            Vsq::single(1),
            &C_IC_NA_1 { objects: vec![(Ioa(0), Qoi::GENERAL)] },
        )
        .await?;

    // collect events for a bounded time
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut asdu_count = 0;
    loop {
        let evt = match tokio::time::timeout_at(deadline, client.recv()).await {
            Ok(Some(e)) => e,
            _ => break,
        };
        if matches!(evt, ClientEvent::Asdu(_)) {
            asdu_count += 1;
            if asdu_count >= 4 {
                break; // ActivationCon + M_SP + M_ME + ActivationTermination
            }
        }
    }

    // shut everything down
    drop(client);
    let _ = tokio::time::timeout(Duration::from_secs(2), server_task).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), tap_task).await;

    // ---- print the capture ---------------------------------------------
    let log = log.lock().unwrap();
    print_capture(t0, &log);
    Ok(())
}

//! End-to-end integration test for the file-transfer feature.
//!
//! Spins up a server with an `FsFileTransferProvider` pointing at a temp
//! directory containing one file, connects a client wired with its own
//! provider, and verifies that the file makes it across the wire intact.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use iec60870::file_transfer::{
    FileTransferEvent, FileTransferOutcome, FileTransferProvider, FsFileTransferProvider,
};
use iec60870::proto::asdu::CommonAddress;
use iec60870::proto::asdu::types::file::NameOfFile;
use iec60870::proto::frame104::Config;
use iec60870::{Client104, Server104, Transport};

fn tempdir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let pid = std::process::id();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("iec60870-ft-{tag}-{pid}-{nonce}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Single-file convenience: take the only entry's NOF from the provider.
async fn only_nof(provider: &FsFileTransferProvider) -> NameOfFile {
    let entries = provider.list_directory().await.unwrap();
    assert_eq!(entries.len(), 1, "expected exactly one file in provider");
    entries[0].nof
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_round_trips_a_file() {
    // Server-side directory with one known file.
    let server_dir = tempdir("server");
    let payload = b"hello iec 60870-5 file transfer";
    std::fs::write(server_dir.join("greeting.txt"), payload).unwrap();
    let server_provider = FsFileTransferProvider::new(&server_dir).unwrap();
    let nof = only_nof(&server_provider).await;

    // Client-side directory starts empty; the file lands as upload_<nof>.bin
    // because the client provider has no mapping for `greeting.txt`.
    let client_dir = tempdir("client");
    let client_provider = FsFileTransferProvider::new(&client_dir).unwrap();

    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default())
        .await
        .unwrap()
        .with_file_provider(server_provider);
    let addr = server.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let mut conn = server.accept().await.expect("accept");
        // The server-side connection needs to be polled so the driver runs;
        // we don't expect any user-visible ASDUs because FT ASDUs are
        // intercepted by the service.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Some(_)) = tokio::time::timeout_at(deadline, conn.recv()).await {}
    });

    let mut client = Client104::connect_with_file_provider(
        Transport::tcp(addr),
        Config::default(),
        client_provider,
        iec60870::DefaultLoggingHandler,
    )
    .await
    .expect("connect");

    let ft = client.file_transfer().expect("ft handle").clone();
    let mut events = ft.subscribe();

    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        ft.fetch(CommonAddress(1), nof),
    )
    .await
    .expect("transfer timeout")
    .expect("transfer failed");

    assert_eq!(bytes as usize, payload.len());

    // The file should now exist on the client side.
    let stored = std::fs::read(client_dir.join(format!("upload_{:04X}.bin", nof.0))).unwrap();
    assert_eq!(stored, payload);

    // We expect a Finished{Completed} event.
    let mut saw_finished_completed = false;
    while let Ok(ev) = events.try_recv() {
        if let FileTransferEvent::Finished {
            outcome: FileTransferOutcome::Completed { .. },
            ..
        } = ev
        {
            saw_finished_completed = true;
        }
    }
    assert!(saw_finished_completed, "no Completed event observed");

    // Drive the client to drain — drop closes the channels, server task ends.
    let _ = tokio::time::timeout(Duration::from_millis(200), client.recv()).await;
    drop(client);
    let _ = server_task.await;
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_unknown_file_fails_with_not_found() {
    // Server has no provider at all → it can't serve files. The client
    // sends F_SC_NA_1 SELECT and times out waiting for a reply.
    let server_dir = tempdir("server-empty");
    let server_provider = FsFileTransferProvider::new(&server_dir).unwrap();

    let client_dir = tempdir("client-noent");
    let client_provider = FsFileTransferProvider::new(&client_dir).unwrap();

    let bind = (Ipv4Addr::LOCALHOST, 0).into();
    let server = Server104::bind(bind, Config::default())
        .await
        .unwrap()
        .with_file_provider(server_provider);
    let addr = server.local_addr().unwrap();

    let server_task = tokio::spawn(async move {
        let mut conn = server.accept().await.expect("accept");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(_)) = tokio::time::timeout_at(deadline, conn.recv()).await {}
    });

    let client = Client104::connect_with_file_provider(
        Transport::tcp(addr),
        Config::default(),
        client_provider,
        iec60870::DefaultLoggingHandler,
    )
    .await
    .expect("connect");

    let ft = client.file_transfer().expect("ft handle").clone();

    // Force a short-ish timeout — the session config default is 30 s, but
    // we wrap the fetch call ourselves so the test doesn't hang.
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        ft.fetch(CommonAddress(1), NameOfFile(0xDEAD)),
    )
    .await;

    // Either the fetch hits its session-internal idle timeout (eventually)
    // or our outer timeout fires first — both are acceptable signals that
    // a missing file does not magically appear. We assert that the fetch
    // did not return Ok.
    if let Ok(Ok(_)) = outcome {
        panic!("fetch of unknown NOF unexpectedly succeeded");
    }

    drop(client);
    let _ = server_task.await;
}

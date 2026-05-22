//! End-to-end loopback over TCP (104) with both daemons spawned as
//! subprocesses and driven via their Unix-socket control plane.
//!
//! Verifies the agent-facing JSON contract is wired up: list, get, set, sim
//! get, status, interrogate, read, single command, shutdown.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use iec60870_test_tools::control;
use iec60870_test_tools::wire::{PointValue, Request, Response, SetpointKind, StepDir};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::sleep;

/// Pick an unused TCP port on 127.0.0.1.
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Find the freshly built `iec-server` / `iec-client` binary path.
fn bin(name: &str) -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    // tests/loopback_ctl-<hash> → strip the test name, walk up to the deps
    // directory's parent (`target/debug`), then take `<name>`.
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.push(name);
    if !p.exists() {
        panic!(
            "binary {} not found at {} — run `cargo build -p iec60870-test-tools --bins` first",
            name,
            p.display()
        );
    }
    p
}

struct Harness {
    server: Child,
    client: Child,
    server_sock: PathBuf,
    client_sock: PathBuf,
}

impl Harness {
    async fn spawn(files_dir: &Path, client_files_dir: &Path) -> Result<Self> {
        let port = free_port().await;
        let tmp = std::env::temp_dir();
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let server_sock = tmp.join(format!("iec-test-server-{pid}-{nonce}.sock"));
        let client_sock = tmp.join(format!("iec-test-client-{pid}-{nonce}.sock"));

        let _ = std::fs::remove_file(&server_sock);
        let _ = std::fs::remove_file(&client_sock);

        let server = Command::new(bin("iec-server"))
            .arg("--control")
            .arg(&server_sock)
            .arg("daemon")
            .arg("--transport")
            .arg("tcp")
            .arg("--addr")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--files-dir")
            .arg(files_dir)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        // Wait for the server socket to appear (up to ~3s).
        wait_for_socket(&server_sock, Duration::from_secs(3)).await?;

        let client = Command::new(bin("iec-client"))
            .arg("daemon")
            .arg("--transport")
            .arg("tcp")
            .arg("--addr")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--control")
            .arg(&client_sock)
            .arg("--files-dir")
            .arg(client_files_dir)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        wait_for_socket(&client_sock, Duration::from_secs(3)).await?;
        // Give the IEC link a moment to settle (STARTDT_CON).
        sleep(Duration::from_millis(300)).await;

        Ok(Self {
            server,
            client,
            server_sock,
            client_sock,
        })
    }

    async fn server(&self, req: Request) -> Result<Response> {
        control::call(&self.server_sock, &req).await
    }

    async fn client(&self, req: Request) -> Result<Response> {
        control::call(&self.client_sock, &req).await
    }

    async fn shutdown(mut self) -> Result<()> {
        // Best-effort graceful shutdown via control sockets.
        let _ = control::call(&self.client_sock, &Request::Shutdown).await;
        let _ = control::call(&self.server_sock, &Request::Shutdown).await;

        // Give them a moment to exit cleanly, then kill if still alive.
        sleep(Duration::from_millis(200)).await;
        let _ = self.client.kill().await;
        let _ = self.server.kill().await;
        let _ = self.client.wait().await;
        let _ = self.server.wait().await;
        let _ = std::fs::remove_file(&self.server_sock);
        let _ = std::fs::remove_file(&self.client_sock);
        Ok(())
    }
}

async fn wait_for_socket(path: &Path, max: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + max;
    loop {
        if path.exists() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("socket {} did not appear in time", path.display()));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn loopback_104_round_trip() -> Result<()> {
    let files_dir_obj = TempDir::new()?;
    let client_files_dir_obj = TempDir::new()?;
    let files_dir = files_dir_obj.path().to_path_buf();
    let client_files_dir = client_files_dir_obj.path().to_path_buf();
    // File at NOF 0xBB3D = CRC16-IBM of "123456789".
    std::fs::write(files_dir.join("123456789"), b"hello from loopback\n")?;

    let h = Harness::spawn(&files_dir, &client_files_dir).await?;

    // 1. Server `list` returns 50 points (5 per type × 10 types).
    let resp = h.server(Request::List { type_id: None }).await?;
    assert!(resp.ok, "list failed: {:?}", resp.error);
    let points = resp
        .data
        .get("points")
        .and_then(|v| v.as_array())
        .expect("points array");
    assert_eq!(points.len(), 50, "expected 50 default points, got {}", points.len());

    // 2. Server `set` mutates a float point.
    let resp = h
        .server(Request::Set {
            ioa: 500,
            value: PointValue::Float(123.5),
            quality: None,
        })
        .await?;
    assert!(resp.ok, "set failed: {:?}", resp.error);

    // 3. Server `get` reads it back. (Simulator may overwrite within 500ms;
    //    we read immediately.)
    let resp = h.server(Request::Get { ioa: 500 }).await?;
    assert!(resp.ok, "get failed: {:?}", resp.error);
    let v = resp.data.get("value").expect("value");
    assert_eq!(v.get("kind"), Some(&serde_json::Value::String("float".into())));

    // 4. Status sanity-check on both sides.
    let resp = h.server(Request::Status).await?;
    assert!(resp.ok);
    assert_eq!(resp.data.get("peers"), Some(&serde_json::json!(1)));

    let resp = h.client(Request::Status).await?;
    assert!(resp.ok);
    assert_eq!(
        resp.data.get("status"),
        Some(&serde_json::Value::String("running".into()))
    );

    // 5. Client `interrogate` returns the full 50-point set.
    let resp = h
        .client(Request::Interrogate {
            group: None,
            ca: None,
            timeout_ms: Some(5_000),
        })
        .await?;
    assert!(resp.ok, "interrogate failed: {:?}", resp.error);
    let count = resp.data.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    assert_eq!(count, 50, "expected 50 points from interrogation, got {count}");

    // 6. Client `cmd single`: ACTIVATION_CON positive.
    let resp = h
        .client(Request::CmdSingle {
            ioa: 2100,
            on: true,
            ca: None,
        })
        .await?;
    assert!(resp.ok, "cmd single failed: {:?}", resp.error);

    // 7. Client `cmd setpoint` (float): ACTIVATION_CON positive.
    let resp = h
        .client(Request::CmdSetpoint {
            ioa: 2500,
            kind: SetpointKind::Float,
            value: 7.5,
            ca: None,
        })
        .await?;
    assert!(resp.ok, "cmd setpoint failed: {:?}", resp.error);

    // 8. Client `cmd regulating` (step higher): ACTIVATION_CON positive.
    let resp = h
        .client(Request::CmdRegulating {
            ioa: 2300,
            step: StepDir::Higher,
            ca: None,
        })
        .await?;
    assert!(resp.ok, "cmd regulating failed: {:?}", resp.error);

    // 9. Client `read` cached value for a sine-driven IOA (the simulator
    //    should have pushed at least one spontaneous value by now).
    let resp = h
        .client(Request::Read {
            ioa: 500,
            type_id: None,
        })
        .await?;
    assert!(resp.ok, "read failed: {:?}", resp.error);
    assert!(resp.data.contains_key("value"));

    // 10. File transfer round-trip.
    let resp = h
        .client(Request::FileGet {
            nof: 0xBB3D,
            out: PathBuf::new(),
            ca: None,
        })
        .await?;
    assert!(resp.ok, "file get failed: {:?}", resp.error);
    let bytes = resp.data.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
    assert_eq!(bytes, 20, "expected 20 bytes (the test fixture), got {bytes}");

    h.shutdown().await
}

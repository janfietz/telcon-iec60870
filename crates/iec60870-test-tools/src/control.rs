//! NDJSON-over-Unix-socket control plane shared by both daemons.
//!
//! The daemon binds a [`tokio::net::UnixListener`] at a path of its choosing
//! and serves incoming connections with one request/response round-trip per
//! line (or a long-lived `Event` stream for `Request::Events`).
//!
//! Short-lived CLI subcommands open a fresh connection, write one
//! [`Request`], read one [`Response`], and close. The `events --follow`
//! subcommand opens a connection, writes `Request::Events`, and keeps
//! reading [`Event`] lines until the daemon closes the socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::wire::{Event, Request, Response};

/// Default control-socket path for the server daemon.
pub fn default_server_socket() -> PathBuf {
    PathBuf::from("/tmp/iec-test-server.sock")
}

/// Default control-socket path for the client daemon.
pub fn default_client_socket() -> PathBuf {
    PathBuf::from("/tmp/iec-test-client.sock")
}

/// Handler trait for daemon-side request dispatch.
#[async_trait]
pub trait ControlHandler: Send + Sync + 'static {
    /// Dispatch a single request.
    async fn handle(&self, req: Request) -> Response;

    /// Subscribe to the daemon's event stream. Returning `None` rejects the
    /// `Events` op with `"events not supported"`.
    async fn subscribe_events(&self) -> Option<mpsc::Receiver<Event>> {
        None
    }
}

/// Run the daemon control loop. Binds the Unix socket (replacing any stale
/// file at the path), accepts connections in a loop, and dispatches each
/// line to the supplied handler. Returns only on listener error.
pub async fn serve<H: ControlHandler>(socket: &Path, handler: Arc<H>) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(socket)
            .with_context(|| format!("removing stale control socket {}", socket.display()))?;
    }
    if let Some(parent) = socket.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
    }
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("binding control socket {}", socket.display()))?;
    tracing::info!(path = %socket.display(), "control socket bound");

    loop {
        let (stream, _addr) = listener.accept().await?;
        let h = handler.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, h).await {
                tracing::debug!(?e, "control connection ended");
            }
        });
    }
}

async fn handle_connection<H: ControlHandler>(stream: UnixStream, handler: Arc<H>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                write_response(&mut write_half, &Response::err(format!("parse: {e}"))).await?;
                continue;
            }
        };

        match req {
            Request::Events => {
                let Some(mut rx) = handler.subscribe_events().await else {
                    write_response(&mut write_half, &Response::err("events not supported")).await?;
                    continue;
                };
                write_response(
                    &mut write_half,
                    &Response::ok(serde_json::json!({"subscribed": true})),
                )
                .await?;
                while let Some(evt) = rx.recv().await {
                    let bytes = match serde_json::to_vec(&evt) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(?e, "failed to encode event");
                            continue;
                        }
                    };
                    if write_half.write_all(&bytes).await.is_err() {
                        break;
                    }
                    if write_half.write_all(b"\n").await.is_err() {
                        break;
                    }
                }
                return Ok(());
            }
            Request::Shutdown => {
                let resp = handler.handle(Request::Shutdown).await;
                write_response(&mut write_half, &resp).await?;
                return Ok(());
            }
            other => {
                let resp = handler.handle(other).await;
                write_response(&mut write_half, &resp).await?;
            }
        }
    }
}

async fn write_response<W: AsyncWriteExt + Unpin>(write: &mut W, resp: &Response) -> Result<()> {
    let bytes = serde_json::to_vec(resp)?;
    write.write_all(&bytes).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;
    Ok(())
}

/// CLI-side: open the socket, send one request, read one response, close.
pub async fn call(socket: &Path, req: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to control socket {}", socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();

    let payload = serde_json::to_vec(req)?;
    write_half.write_all(&payload).await?;
    write_half.write_all(b"\n").await?;
    write_half.shutdown().await.ok();

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        anyhow::bail!("daemon closed the connection without responding");
    }
    let resp: Response = serde_json::from_str(trimmed)
        .with_context(|| format!("parsing daemon response: {trimmed}"))?;
    Ok(resp)
}

/// CLI-side: open the socket, request an event subscription, and invoke
/// `on_event` for every line. Returns when the daemon closes the stream.
pub async fn follow_events<F>(socket: &Path, mut on_event: F) -> Result<()>
where
    F: FnMut(Event),
{
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to control socket {}", socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();

    let payload = serde_json::to_vec(&Request::Events)?;
    write_half.write_all(&payload).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    // Discard the initial acknowledgement (`{"ok": true, ...}`).
    reader.read_line(&mut line).await?;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(trimmed) {
            Ok(evt) => on_event(evt),
            Err(e) => tracing::warn!(line = trimmed, ?e, "failed to parse event line"),
        }
    }
}

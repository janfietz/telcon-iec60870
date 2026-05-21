//! IEC 60870-5-104 outstation that serves files from a host directory.
//!
//! ```text
//! mkdir -p /tmp/iec104-files
//! echo "hello iec" > /tmp/iec104-files/greeting.txt
//! cargo run --example file_serve -- /tmp/iec104-files
//! ```
//!
//! Then in another terminal, run `cargo run --example file_get -- <NOF>` to
//! pull the file across the wire.

use std::net::Ipv4Addr;
use std::path::PathBuf;

use iec60870::file_transfer::{FileTransferProvider, FsFileTransferProvider};
use iec60870::proto::frame104::Config;
use iec60870::{DefaultLoggingHandler, Server104};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info".into()),
        )
        .init();

    let base_dir: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("./fixtures/files"));
    if !base_dir.exists() {
        std::fs::create_dir_all(&base_dir)?;
    }

    let provider = FsFileTransferProvider::new(&base_dir)?;
    tracing::info!(
        base_dir = %provider.base_dir().display(),
        "file-transfer provider ready",
    );

    let entries = provider.list_directory().await?;
    for entry in &entries {
        tracing::info!(
            nof = format!("{:#06X}", entry.nof.0),
            length = entry.meta.length,
            "serving file",
        );
    }

    let bind = (Ipv4Addr::UNSPECIFIED, 2404).into();
    let server = Server104::bind(bind, Config::default())
        .await?
        .with_file_provider(provider);
    tracing::info!(addr = ?server.local_addr()?, "server listening");

    loop {
        let mut conn = server.accept_with(DefaultLoggingHandler).await?;
        tracing::info!(peer = ?conn.peer(), "client connected");
        tokio::spawn(async move {
            // We don't need to handle non-FT ASDUs — but recv() must be
            // polled so the driver keeps running.
            while let Some(evt) = conn.recv().await {
                tracing::debug!(?evt, "non-ft event");
            }
        });
    }
}

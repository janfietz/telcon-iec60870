//! IEC 60870-5-104 master that fetches a single file from a remote
//! outstation.
//!
//! ```text
//! cargo run --example file_get -- 127.0.0.1:2404 0xBB3D
//! ```
//!
//! The fetched bytes land in a temporary directory; the path is printed on
//! success.

use std::net::SocketAddr;
use std::path::PathBuf;

use iec60870::file_transfer::FsFileTransferProvider;
use iec60870::proto::asdu::types::file::NameOfFile;
use iec60870::proto::asdu::CommonAddress;
use iec60870::proto::frame104::Config;
use iec60870::{Client104, DefaultLoggingHandler, Transport};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "iec60870=info".into()),
        )
        .init();

    let addr: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:2404".into())
        .parse()?;
    let nof_arg = std::env::args().nth(2).unwrap_or_else(|| "0xBB3D".into());
    let nof_value = if let Some(hex) = nof_arg
        .strip_prefix("0x")
        .or_else(|| nof_arg.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16)?
    } else {
        nof_arg.parse()?
    };
    let nof = NameOfFile(nof_value);

    let sink_dir: PathBuf = std::env::var("FILE_GET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("iec60870-file-get"));
    std::fs::create_dir_all(&sink_dir)?;
    let provider = FsFileTransferProvider::new(&sink_dir)?;

    let client = Client104::connect_with_file_provider(
        Transport::tcp(addr),
        Config::default(),
        provider,
        DefaultLoggingHandler,
    )
    .await?;

    let ft = client.file_transfer().expect("ft handle").clone();
    let bytes = ft.fetch(CommonAddress(1), nof).await?;
    tracing::info!(bytes, "fetch complete");
    println!(
        "wrote {bytes} bytes to {}",
        sink_dir.join(format!("upload_{:04X}.bin", nof.0)).display(),
    );
    Ok(())
}

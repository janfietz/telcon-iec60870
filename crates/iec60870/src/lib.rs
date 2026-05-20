//! Async client and server for IEC 60870-5-101 and IEC 60870-5-104.
//!
//! Built on `tokio`. The pure protocol logic lives in [`iec60870_proto`],
//! which this crate drives over TCP, optional TLS, and serial transports.
//!
//! Quickstart (client):
//!
//! ```ignore
//! use iec60870::{Client104, Transport};
//! use iec60870::proto::frame104::Config;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut client = Client104::connect(
//!         Transport::tcp("127.0.0.1:2404".parse()?),
//!         Config::default(),
//!     ).await?;
//!     // Send a raw ASDU (encode via iec60870_proto::asdu::Asdu::encode).
//!     client.send_asdu(vec![0x64, 0x01, 0x06, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x14]).await?;
//!     while let Some(asdu) = client.recv_asdu().await {
//!         println!("received {} bytes", asdu.len());
//!     }
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

pub use iec60870_proto as proto;

mod client104;
mod driver;
mod driver101;
mod error;
mod handler;
mod master101;
mod outstation101;
mod redundancy;
mod server104;
mod transport;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "serial")]
pub mod serial;

pub use client104::{Client104, ClientEvent};
pub use error::{Error, Result};
pub use handler::{DefaultLoggingHandler, EventHandler, NoopHandler};
pub use redundancy::RedundancyServer;
pub use server104::{Server104, ServerConnection, ServerEvent, ServerEvents, ServerSender};
pub use transport::{Transport, DEFAULT_PORT, DEFAULT_TLS_PORT};

#[cfg(feature = "tls")]
pub use tls::{
    client_config_with_roots, tls_client_connect, tls_server_accept, tls_server_accept_with,
    TlsClient, TlsConfig, TlsServer, TlsServerConnection,
};

pub use master101::{Master101, Master101Event};
pub use outstation101::{Outstation101, Outstation101Event};

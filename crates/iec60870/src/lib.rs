//! Async client and server for IEC 60870-5-101 and IEC 60870-5-104.
//!
//! Built on `tokio`. The pure protocol logic lives in [`iec60870_proto`],
//! which this crate drives over TCP, optional TLS, and serial transports.
//!
//! Quickstart (client):
//!
//! ```ignore
//! use iec60870::{Client104, Transport};
//! use iec60870::proto::asdu::{CommonAddress, Cot, Cause, Ioa, Vsq};
//! use iec60870::proto::asdu::types::{C_IC_NA_1, Qoi};
//! use iec60870::proto::frame104::Config;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut client = Client104::connect(
//!         Transport::tcp("127.0.0.1:2404".parse()?),
//!         Config::default(),
//!     ).await?;
//!
//!     let interrogation = C_IC_NA_1 { objects: vec![(Ioa(0), Qoi::GENERAL)] };
//!     client.send(
//!         Cot::with(Cause::ACTIVATION),
//!         CommonAddress(1),
//!         Vsq::single(1),
//!         &interrogation,
//!     ).await?;
//!
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
mod deadband;
mod driver;
mod driver101;
mod error;
pub mod file_transfer;
mod handler;
mod master101;
mod outstation101;
mod policy;
mod redundancy;
pub mod security;
mod server104;
mod transport;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "serial")]
pub mod serial;

pub use client104::{Client104, ClientEvent};
pub use deadband::{
    DeadbandError, DeadbandPolicy, DeadbandTracker, EmitDecision, MonitoredValue, ValueKind,
};
pub use error::{Error, Result};
pub use handler::{DefaultLoggingHandler, EventHandler, NoopHandler};
pub use policy::AsduPolicy;
pub use redundancy::{RedundancyConfig, RedundancyServer};
pub use security::{IpFilter, IpFilterParseError, SecurityConfig};
pub use server104::{Server104, ServerConnection, ServerEvent, ServerEvents, ServerSender};
pub use transport::{Transport, DEFAULT_PORT, DEFAULT_TLS_PORT};

#[cfg(feature = "tls")]
pub use security::{
    fingerprint_sha256_of_pem_file, CertificateChain, ClientCertPolicy, CustomVerifierFn,
    TlsSecurityConfig, VerifyError,
};

#[cfg(feature = "tls")]
pub use tls::{
    client_config_with_client_cert, client_config_with_roots, server_config_requiring_client_cert,
    server_config_single_cert, tls_client_connect, tls_client_connect_with_policy,
    tls_server_accept, tls_server_accept_with, tls_server_accept_with_policy, TlsClient,
    TlsConfig, TlsServer, TlsServerConnection,
};

pub use master101::{Master101, Master101Event};
pub use outstation101::{Outstation101, Outstation101Event};

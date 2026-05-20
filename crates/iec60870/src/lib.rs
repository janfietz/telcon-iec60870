//! Async client and server for IEC 60870-5-101 and IEC 60870-5-104.
//!
//! Built on `tokio`. The pure protocol logic lives in
//! [`iec60870-proto`](https://docs.rs/iec60870-proto), which this crate drives over
//! TCP, optional TLS, and serial transports.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

pub use iec60870_proto as proto;

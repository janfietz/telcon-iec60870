//! Sans-I/O protocol layer for IEC 60870-5-101 and IEC 60870-5-104.
//!
//! This crate contains no `async`, no sockets and no clocks. It exposes:
//!
//! * [`asdu`] — the Application Service Data Unit codec (shared between 101 and 104)
//! * [`frame101`] — FT 1.2 framing and link-layer state machine for IEC 60870-5-101
//! * [`frame104`] — APCI / APDU framing and connection state machine for IEC 60870-5-104
//!
//! The companion crate `iec60870` ties these into a `tokio`-based client and server.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations, rust_2018_idioms)]

pub mod asdu;
pub mod error;
pub mod frame101;
pub mod frame104;

pub use error::{Error, Result};

//! Built-in typed ASDU payloads.
//!
//! Each submodule implements one Type ID per IEC 60870-5-101 §7.3 by providing
//! a struct that implements [`crate::asdu::AsduPayload`].

mod monitor;

pub use monitor::M_SP_NA_1;

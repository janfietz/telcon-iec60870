//! Shared types and helpers for the IEC 60870-5 test server/client daemons.
//!
//! The two binaries (`iec-server` and `iec-client`) are long-running
//! processes that speak either IEC 60870-5-104 (TCP) or IEC 60870-5-101
//! (serial), and expose a JSON request/response control plane on a Unix
//! domain socket so agents can drive them with short-lived CLI invocations.

pub mod cache;
pub mod control;
pub mod points;
pub mod sim;
pub mod transport;
pub mod wire;

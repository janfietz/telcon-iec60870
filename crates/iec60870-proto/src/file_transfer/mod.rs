//! Sans-I/O file-transfer session state machine.
//!
//! This module orchestrates the multi-step IEC 60870-5-5 file-transfer
//! dialogue. It is layered above the ASDU codec (defined in
//! [`crate::asdu::types::file`]) and below the async driver / provider in the
//! companion `iec60870` crate.
//!
//! A [`Session`] owns the per-transfer state (current step, accumulated
//! checksum, bytes transferred, timeouts) for one file. The driver feeds it
//! inputs ([`SessionInput`] — received ASDUs, segment data from the provider,
//! tick) and applies the resulting [`SessionAction`] list (typed ASDUs to
//! send, segment data to deliver, completion / failure signals).
//!
//! There are two roles: [`Role::Receiver`] (we want a file) and
//! [`Role::Sender`] (we have a file). Both can be initiated either side:
//! a [`SessionInput::Start`] kicks the session off locally, or an incoming
//! ASDU drives a reactive session.
//!
//! The state machine is deterministic and clock-driven: every public entry
//! takes the current [`Instant`] explicitly.

pub mod session;

pub use session::{
    FailureReason, Role, Session, SessionAction, SessionConfig, SessionInput, SessionState,
};

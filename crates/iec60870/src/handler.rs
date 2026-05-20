//! Connection event hooks.
//!
//! Every async client and server in this crate is generic over an
//! [`EventHandler`]. The handler is invoked at well-defined points in the
//! connection lifecycle (frame received, frame sent, ASDU delivered, state
//! changed, protocol error) with default no-op implementations, so users
//! only need to override the events they care about.
//!
//! The library ships [`DefaultLoggingHandler`] which routes everything to
//! the `tracing` crate at appropriate levels.

use iec60870_proto::frame104::{Apdu, State};

/// Trait implemented by types that want to observe connection events.
///
/// All methods have empty defaults; implement only what you need. The trait
/// requires `Send + Sync + 'static` so handlers can be shared across the
/// driver task and the user-facing API.
pub trait EventHandler: Send + Sync + 'static {
    /// Called after an APDU is successfully decoded from the wire.
    fn on_frame_received(&self, _apdu: &Apdu) {}

    /// Called just before an APDU is written to the wire.
    fn on_frame_sent(&self, _apdu: &Apdu) {}

    /// Called when the state machine delivers an ASDU to the application.
    fn on_asdu_received(&self, _asdu: &[u8]) {}

    /// Called when the connection state machine changes state.
    fn on_state_changed(&self, _state: State) {}

    /// Called when a protocol-level error caused or will cause a disconnect.
    fn on_protocol_error(&self, _message: &str) {}
}

/// A handler that swallows every event. Useful as a generic default when the
/// caller hasn't supplied one.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHandler;

impl EventHandler for NoopHandler {}

/// A handler that emits structured `tracing` events at appropriate levels:
///
/// * `trace` — every received and sent APDU (with format and sequence numbers)
/// * `debug` — ASDU deliveries (length only, not contents)
/// * `info`  — state transitions
/// * `warn`  — protocol errors
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultLoggingHandler;

impl EventHandler for DefaultLoggingHandler {
    fn on_frame_received(&self, apdu: &Apdu) {
        tracing::trace!(target: "iec60870::rx", ?apdu, "apdu received");
    }
    fn on_frame_sent(&self, apdu: &Apdu) {
        tracing::trace!(target: "iec60870::tx", ?apdu, "apdu sent");
    }
    fn on_asdu_received(&self, asdu: &[u8]) {
        tracing::debug!(target: "iec60870::asdu", len = asdu.len(), "asdu received");
    }
    fn on_state_changed(&self, state: State) {
        tracing::info!(target: "iec60870::state", ?state, "connection state changed");
    }
    fn on_protocol_error(&self, message: &str) {
        tracing::warn!(target: "iec60870::error", "protocol error: {message}");
    }
}

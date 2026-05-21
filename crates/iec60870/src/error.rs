//! Error type for the async layer.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(#[from] iec60870_proto::Error),

    #[error("connection closed by state machine ({0:?})")]
    ConnectionClosed(iec60870_proto::frame104::DisconnectReason),

    #[error("driver shut down")]
    DriverGone,

    #[error("no active peer is currently in data-transfer state")]
    NoActivePeer,

    #[cfg(feature = "tls")]
    #[error("tls error: {0}")]
    Tls(String),
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

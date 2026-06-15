//! Observability events emitted by the file-transfer service.

use iec60870_proto::asdu::types::file::NameOfFile;
use iec60870_proto::asdu::CommonAddress;
use iec60870_proto::file_transfer::{FailureReason, Role};

/// One file-transfer lifecycle event.
#[derive(Debug, Clone)]
pub enum FileTransferEvent {
    /// A new session began (either locally requested or driven by an
    /// incoming FT ASDU from the peer).
    Started {
        peer: CommonAddress,
        nof: NameOfFile,
        role: Role,
    },
    /// One segment was successfully transferred.
    SegmentTransferred {
        peer: CommonAddress,
        nof: NameOfFile,
        bytes_total: u32,
    },
    /// Transfer completed (success or failure).
    Finished {
        peer: CommonAddress,
        nof: NameOfFile,
        outcome: FileTransferOutcome,
    },
}

/// Outcome of a finished transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransferOutcome {
    Completed { bytes: u32 },
    Failed(FailureReason),
}

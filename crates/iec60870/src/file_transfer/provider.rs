//! Storage abstraction for file transfers.
//!
//! Implement [`FileTransferProvider`] (or use the bundled
//! [`super::FsFileTransferProvider`]) to make files available to a connected
//! peer, or to receive uploads. The trait is async so providers can be backed
//! by remote stores, databases, or in-memory caches; the [`tokio::fs`]-based
//! default fits the most common "files on a host directory" use case.
//!
//! # Caveats
//!
//! * The 16-bit `NameOfFile` is a *protocol identifier*, not a filename.
//!   Providers map between NOF and storage. The default implementation
//!   derives NOF from a stable CRC-16 hash of the relative path.
//! * Methods are called from the driver task, not a user task. They must
//!   avoid blocking — use `tokio::fs` or `spawn_blocking` for synchronous
//!   I/O.

use std::time::SystemTime;

use async_trait::async_trait;
use iec60870_proto::asdu::types::file::{LengthOfFile, NameOfFile, Sof};
use iec60870_proto::asdu::ie::Cp56Time2a;
use thiserror::Error;

/// Tunables that affect every transfer driven by a provider-backed service.
///
/// The defaults are conservative: capped concurrent sessions and a hard
/// upper bound on inbound file size. Loosen them only if you trust the peer.
#[derive(Debug, Clone, Copy)]
pub struct FileTransferConfig {
    /// Maximum segment payload in bytes. Clamped to
    /// [`iec60870_proto::asdu::types::file::MAX_SEGMENT_BYTES`] (255).
    pub max_segment_bytes: usize,
    /// Reserved for a future multi-section extension. Currently every file
    /// transfers as a single section.
    pub max_section_bytes: u32,
    /// Per-session inactivity timeout.
    pub idle_timeout: std::time::Duration,
    /// Maximum number of concurrent active sessions. A peer cannot create
    /// more than this; once the cap is reached new transfers are dropped
    /// silently. Caps memory + file-descriptor use under a misbehaving peer.
    pub max_concurrent_sessions: usize,
    /// Maximum advertised `LengthOfFile` accepted for an inbound transfer.
    /// `F_FR_NA_1` ASDUs carrying a larger LOF are refused at session
    /// creation time. Caps disk consumption under a hostile peer.
    pub max_inbound_file_bytes: u32,
}

impl Default for FileTransferConfig {
    fn default() -> Self {
        Self {
            max_segment_bytes: 240,
            max_section_bytes: u32::MAX,
            idle_timeout: std::time::Duration::from_secs(30),
            max_concurrent_sessions: 8,
            // 16 MiB — comfortably larger than typical event logs / firmware
            // chunks shipped over IEC 60870-5, well below "fill the disk".
            max_inbound_file_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Provider-level errors. Surfaced both to local callers and the peer (where
/// possible via negative FRQ / SRQ / AFQ qualifiers).
#[derive(Debug, Error)]
pub enum FileTransferError {
    #[error("file not found for nof {nof:?}")]
    NotFound { nof: NameOfFile },
    #[error("permission denied")]
    PermissionDenied,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid state: {0}")]
    InvalidState(String),
    #[error("checksum mismatch on inbound file")]
    ChecksumMismatch,
    #[error("nof collision: {0:?} maps to multiple files")]
    Collision(NameOfFile),
    #[error("{0}")]
    Other(String),
}

/// Async pluggable storage provider.
#[async_trait]
pub trait FileTransferProvider: Send + Sync + 'static {
    /// Enumerate all files known to this provider. Used to answer directory
    /// requests; an empty list is acceptable.
    async fn list_directory(&self) -> Result<Vec<DirectoryEntry>, FileTransferError>;

    /// Look up metadata for one file. Returns `None` for unknown NOFs.
    async fn lookup(
        &self,
        nof: NameOfFile,
    ) -> Result<Option<FileMetadata>, FileTransferError>;

    /// Open a file for reading. The returned reader is driven segment-by-
    /// segment until it yields `Ok(None)` (EOF).
    async fn open_read(
        &self,
        nof: NameOfFile,
    ) -> Result<Box<dyn FileReader + Send>, FileTransferError>;

    /// Open a file for writing. The provider chooses storage semantics
    /// (overwrite vs append, atomic-rename, …). `expected_length` is the
    /// advertised `LengthOfFile` from the sender.
    async fn open_write(
        &self,
        nof: NameOfFile,
        expected_length: u32,
    ) -> Result<Box<dyn FileWriter + Send>, FileTransferError>;
}

/// One entry returned by [`FileTransferProvider::list_directory`].
#[derive(Debug, Clone, Copy)]
pub struct DirectoryEntry {
    pub nof: NameOfFile,
    pub meta: FileMetadata,
}

impl DirectoryEntry {
    pub fn into_wire(self) -> iec60870_proto::asdu::types::file::DirectoryEntry {
        iec60870_proto::asdu::types::file::DirectoryEntry {
            nof: self.nof,
            lof: LengthOfFile(self.meta.length),
            sof: self.meta.status,
            time: self.meta.modified_cp56,
        }
    }
}

/// File metadata exposed to the protocol layer.
#[derive(Debug, Clone, Copy)]
pub struct FileMetadata {
    pub length: u32,
    pub status: Sof,
    pub modified: SystemTime,
    /// Pre-rendered CP56Time2a — providers may set this directly if they
    /// already have it; otherwise [`super::FsFileTransferProvider`] computes
    /// it from `modified`.
    pub modified_cp56: Cp56Time2a,
}

impl Default for FileMetadata {
    fn default() -> Self {
        Self {
            length: 0,
            status: Sof::default(),
            modified: SystemTime::UNIX_EPOCH,
            modified_cp56: Cp56Time2a::default(),
        }
    }
}

/// Async file reader produced by [`FileTransferProvider::open_read`].
///
/// The driver invokes `read_segment` repeatedly until it returns `Ok(None)`
/// (EOF). The provider chooses any segment size up to `max_bytes`; smaller
/// is fine, larger is truncated.
#[async_trait]
pub trait FileReader: Send {
    async fn read_segment(
        &mut self,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, FileTransferError>;
}

/// Async file writer produced by [`FileTransferProvider::open_write`].
///
/// `write_segment` is invoked once per received segment. `finalize` is
/// invoked exactly once at transfer end with `success = true` on a
/// checksum-verified transfer or `success = false` when the transfer
/// failed.
#[async_trait]
pub trait FileWriter: Send {
    async fn write_segment(&mut self, data: &[u8]) -> Result<(), FileTransferError>;
    async fn finalize(self: Box<Self>, success: bool) -> Result<(), FileTransferError>;
}

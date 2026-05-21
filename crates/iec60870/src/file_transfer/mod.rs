//! Async file-transfer driver and pluggable storage providers.
//!
//! The crate ships a default [`FsFileTransferProvider`] that maps IEC 60870-5
//! `NameOfFile` identifiers to files on a host-system directory. Users with
//! more involved needs (object storage, in-memory test fixtures, database
//! tables, …) implement the [`FileTransferProvider`] / [`FileReader`] /
//! [`FileWriter`] traits themselves.
//!
//! The protocol-level state machine driving each transfer lives in
//! [`iec60870_proto::file_transfer`]. This module wraps it with a tokio
//! task that:
//!
//! * receives FT ASDUs decoded from the wire,
//! * pulls bytes from a `FileReader` or pushes them into a `FileWriter`,
//! * emits outgoing FT ASDUs back to the connection driver,
//! * surfaces high-level [`FileTransferEvent`]s for observability.
//!
//! ```ignore
//! use iec60870::file_transfer::FsFileTransferProvider;
//! let provider = FsFileTransferProvider::new("/var/lib/iec104/files")?;
//! // Then hand the provider to Server104::accept_with_file_provider(...) or
//! // Client104::connect_with_file_provider(...).
//! ```

pub mod events;
pub mod fs;
pub mod provider;
pub mod service;

pub use events::{FileTransferEvent, FileTransferOutcome};
pub use fs::{CollisionStrategy, FsFileTransferProvider};
pub use provider::{
    DirectoryEntry, FileMetadata, FileReader, FileTransferConfig, FileTransferError,
    FileTransferProvider, FileWriter,
};
pub use service::{FileTransferHandle, ProviderObject};

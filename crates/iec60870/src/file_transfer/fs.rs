//! Default filesystem-backed [`FileTransferProvider`].
//!
//! Maps a `NameOfFile` to a path under a fixed base directory using a
//! CRC-16/IBM hash of the relative path. The mapping is computed once at
//! construction; new files dropped into the directory show up on the next
//! [`FileTransferProvider::list_directory`] call but only become *fetchable
//! by NOF* after a re-scan via [`FsFileTransferProvider::rescan`].
//!
//! # Path safety
//!
//! Every path access is canonicalised and rejected if it escapes the base
//! directory. Symlinks pointing outside are denied. The provider never reads
//! or writes anything outside `base_dir`.
//!
//! # NOF collisions
//!
//! CRC-16/IBM is 16-bit so collisions are possible (one in 65 536 pairs in
//! the worst case). By default the constructor returns
//! [`FileTransferError::Collision`] when two files hash to the same NOF.
//! Use [`FsFileTransferProvider::with_collision_strategy`] to pick `KeepFirst`
//! or `KeepLast` instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use iec60870_proto::asdu::ie::Cp56Time2a;
use iec60870_proto::asdu::types::file::{NameOfFile, Sof};
use tokio::fs as tfs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;

use super::provider::{
    DirectoryEntry, FileMetadata, FileReader, FileTransferError, FileTransferProvider, FileWriter,
};

/// Policy for handling duplicate NOF hashes during a directory scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CollisionStrategy {
    /// Refuse to construct or rescan when two files map to the same NOF.
    #[default]
    Error,
    /// Keep the first file encountered, drop subsequent collisions.
    KeepFirst,
    /// Keep the most recent file encountered, drop earlier collisions.
    KeepLast,
}

/// Filesystem-backed provider.
#[derive(Debug, Clone)]
pub struct FsFileTransferProvider {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    base_dir: PathBuf,
    collision: CollisionStrategy,
    /// nof -> absolute path. Held under RwLock so `rescan` can refresh it
    /// without breaking concurrent reads.
    index: RwLock<HashMap<NameOfFile, PathBuf>>,
}

impl FsFileTransferProvider {
    /// Build a provider rooted at `base_dir` with the default collision
    /// strategy ([`CollisionStrategy::Error`]).
    ///
    /// Performs a blocking directory scan to build the NOF index.
    pub fn new(base_dir: impl Into<PathBuf>) -> Result<Self, FileTransferError> {
        Self::with_collision_strategy(base_dir, CollisionStrategy::Error)
    }

    /// Build a provider with an explicit collision strategy.
    pub fn with_collision_strategy(
        base_dir: impl Into<PathBuf>,
        collision: CollisionStrategy,
    ) -> Result<Self, FileTransferError> {
        let base_dir = base_dir.into();
        let base_abs = std::fs::canonicalize(&base_dir).map_err(|e| {
            FileTransferError::Other(format!(
                "cannot canonicalise base_dir {}: {e}",
                base_dir.display()
            ))
        })?;
        if !base_abs.is_dir() {
            return Err(FileTransferError::Other(format!(
                "base_dir is not a directory: {}",
                base_abs.display()
            )));
        }
        let index = scan_dir(&base_abs, collision)?;
        Ok(Self {
            inner: Arc::new(Inner {
                base_dir: base_abs,
                collision,
                index: RwLock::new(index),
            }),
        })
    }

    /// Base directory that all file operations are anchored to.
    pub fn base_dir(&self) -> &Path {
        &self.inner.base_dir
    }

    /// Re-walk the base directory and rebuild the NOF index. Returns the
    /// number of files now indexed.
    pub async fn rescan(&self) -> Result<usize, FileTransferError> {
        let base = self.inner.base_dir.clone();
        let collision = self.inner.collision;
        let new_index = tokio::task::spawn_blocking(move || scan_dir(&base, collision))
            .await
            .map_err(|e| FileTransferError::Other(format!("rescan join error: {e}")))??;
        let mut guard = self.inner.index.write().await;
        let count = new_index.len();
        *guard = new_index;
        Ok(count)
    }

    async fn resolve(&self, nof: NameOfFile) -> Result<PathBuf, FileTransferError> {
        let guard = self.inner.index.read().await;
        match guard.get(&nof) {
            Some(p) => Ok(p.clone()),
            None => Err(FileTransferError::NotFound { nof }),
        }
    }

    /// Compute the path used for incoming uploads under a fresh NOF. The
    /// path is `<base>/upload_<nof_hex>.bin` while the transfer is in
    /// progress (suffixed `.partial`) and is renamed on successful finalize.
    fn upload_target(&self, nof: NameOfFile) -> PathBuf {
        self.inner
            .base_dir
            .join(format!("upload_{:04X}.bin", nof.0))
    }
}

#[async_trait]
impl FileTransferProvider for FsFileTransferProvider {
    async fn list_directory(&self) -> Result<Vec<DirectoryEntry>, FileTransferError> {
        let guard = self.inner.index.read().await;
        let mut out = Vec::with_capacity(guard.len());
        // Sort by NOF for stable output ordering.
        let mut entries: Vec<(NameOfFile, PathBuf)> =
            guard.iter().map(|(k, v)| (*k, v.clone())).collect();
        drop(guard);
        entries.sort_by_key(|(k, _)| k.0);
        let total = entries.len();
        for (i, (nof, path)) in entries.into_iter().enumerate() {
            let meta = match tfs::metadata(&path).await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let last = i + 1 == total;
            let entry = DirectoryEntry {
                nof,
                meta: FileMetadata {
                    length: clamp_u32(meta.len()),
                    status: Sof {
                        status: 0,
                        last_file: last,
                        sub_directory: false,
                        active: false,
                    },
                    modified,
                    modified_cp56: system_time_to_cp56(modified),
                },
            };
            out.push(entry);
        }
        Ok(out)
    }

    async fn lookup(&self, nof: NameOfFile) -> Result<Option<FileMetadata>, FileTransferError> {
        let path = match self.resolve(nof).await {
            Ok(p) => p,
            Err(FileTransferError::NotFound { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let meta = tfs::metadata(&path).await?;
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        Ok(Some(FileMetadata {
            length: clamp_u32(meta.len()),
            status: Sof::default(),
            modified,
            modified_cp56: system_time_to_cp56(modified),
        }))
    }

    async fn open_read(
        &self,
        nof: NameOfFile,
    ) -> Result<Box<dyn FileReader + Send>, FileTransferError> {
        let path = self.resolve(nof).await?;
        // Defence in depth: refuse to follow a symlink that may have been
        // dropped into the base dir between scan and now. `symlink_metadata`
        // does NOT traverse the final component.
        let lmeta = tfs::symlink_metadata(&path).await?;
        if lmeta.file_type().is_symlink() {
            return Err(FileTransferError::PermissionDenied);
        }
        let file = tfs::File::open(&path).await?;
        Ok(Box::new(FsFileReader { file }))
    }

    async fn open_write(
        &self,
        nof: NameOfFile,
        _expected_length: u32,
    ) -> Result<Box<dyn FileWriter + Send>, FileTransferError> {
        // Determine final path: if NOF is already mapped, overwrite it;
        // otherwise mint a synthetic path under the base directory.
        let final_path = match self.resolve(nof).await {
            Ok(p) => p,
            Err(FileTransferError::NotFound { .. }) => self.upload_target(nof),
            Err(e) => return Err(e),
        };
        // Block writing through a pre-existing symlink at either the final or
        // partial path; an attacker who can drop a symlink into base_dir
        // (e.g. a less-privileged service sharing the directory) could
        // otherwise redirect our writes elsewhere.
        if let Ok(m) = tfs::symlink_metadata(&final_path).await {
            if m.file_type().is_symlink() {
                return Err(FileTransferError::PermissionDenied);
            }
        }
        let partial = with_partial_suffix(&final_path);
        if let Ok(m) = tfs::symlink_metadata(&partial).await {
            if m.file_type().is_symlink() {
                return Err(FileTransferError::PermissionDenied);
            }
        }
        let file = tfs::File::create(&partial).await?;
        Ok(Box::new(FsFileWriter {
            file: Some(file),
            partial,
            final_path,
            provider: Some(self.clone()),
        }))
    }
}

/// Reader implementation streaming bytes from a [`tokio::fs::File`].
struct FsFileReader {
    file: tfs::File,
}

#[async_trait]
impl FileReader for FsFileReader {
    async fn read_segment(
        &mut self,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, FileTransferError> {
        if max_bytes == 0 {
            return Ok(Some(Vec::new()));
        }
        let mut buf = vec![0u8; max_bytes];
        let n = self.file.read(&mut buf).await?;
        if n == 0 {
            Ok(None)
        } else {
            buf.truncate(n);
            Ok(Some(buf))
        }
    }
}

/// Writer implementation that streams into a `*.partial` sibling and
/// atomically renames on success.
struct FsFileWriter {
    file: Option<tfs::File>,
    partial: PathBuf,
    final_path: PathBuf,
    provider: Option<FsFileTransferProvider>,
}

#[async_trait]
impl FileWriter for FsFileWriter {
    async fn write_segment(&mut self, data: &[u8]) -> Result<(), FileTransferError> {
        let f = self
            .file
            .as_mut()
            .ok_or_else(|| FileTransferError::InvalidState("writer already finalized".into()))?;
        f.write_all(data).await?;
        Ok(())
    }

    async fn finalize(mut self: Box<Self>, success: bool) -> Result<(), FileTransferError> {
        if let Some(mut f) = self.file.take() {
            f.flush().await?;
            drop(f);
        }
        if success {
            tfs::rename(&self.partial, &self.final_path).await?;
            if let Some(p) = self.provider.take() {
                // Best-effort: re-index so the file becomes fetchable
                // immediately. Ignore errors — at worst a rescan picks it up.
                let _ = p.rescan().await;
            }
        } else {
            let _ = tfs::remove_file(&self.partial).await;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn with_partial_suffix(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

fn clamp_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

fn scan_dir(
    base: &Path,
    collision: CollisionStrategy,
) -> Result<HashMap<NameOfFile, PathBuf>, FileTransferError> {
    let mut idx: HashMap<NameOfFile, PathBuf> = HashMap::new();
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        let path = entry.path();
        // Skip subdirectories (non-recursive) and `.partial` uploads in progress.
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("partial") {
            continue;
        }
        // Reject symlinks that resolve outside the base dir.
        let canon = std::fs::canonicalize(&path)?;
        if !canon.starts_with(base) {
            continue;
        }
        // The CRC input is the relative-path bytes.
        let rel = match canon.strip_prefix(base) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let nof = NameOfFile(crc16_ibm(rel.to_string_lossy().as_bytes()));
        match (idx.contains_key(&nof), collision) {
            (false, _) => {
                idx.insert(nof, canon);
            }
            (true, CollisionStrategy::Error) => {
                return Err(FileTransferError::Collision(nof));
            }
            (true, CollisionStrategy::KeepFirst) => {}
            (true, CollisionStrategy::KeepLast) => {
                idx.insert(nof, canon);
            }
        }
    }
    Ok(idx)
}

/// CRC-16/IBM (a.k.a. ARC) — polynomial `0xA001`, initial value `0x0000`,
/// no reflection beyond the polynomial choice. 16-bit output is the NOF.
///
/// Embedded as a const lookup table to avoid a runtime dependency on a
/// dedicated CRC crate.
fn crc16_ibm(data: &[u8]) -> u16 {
    let mut crc: u16 = 0x0000;
    for &b in data {
        crc = (crc >> 8) ^ CRC16_IBM_TABLE[((crc as u8) ^ b) as usize];
    }
    crc
}

const CRC16_IBM_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xA001
            } else {
                c >> 1
            };
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// Convert a `SystemTime` to a CP56Time2a tag (UTC, year mod 100).
fn system_time_to_cp56(t: SystemTime) -> Cp56Time2a {
    let dur = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return Cp56Time2a::default(),
    };
    let total_secs = dur.as_secs();
    let millis_in_sec = dur.subsec_millis() as u16;
    let days = total_secs / 86_400;
    let secs_of_day = total_secs % 86_400;
    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    let second = (secs_of_day % 60) as u8;
    let millis = second as u16 * 1000 + millis_in_sec;
    let (year, month, day, day_of_week) = days_to_ymd(days as i64);
    Cp56Time2a {
        milliseconds: millis,
        minute,
        hour,
        day,
        day_of_week,
        month,
        year: (year as u32 % 100) as u8,
        ..Default::default()
    }
}

/// Convert days-since-1970-01-01 into (year, month, day, day_of_week).
fn days_to_ymd(days: i64) -> (i32, u8, u8, u8) {
    // Day-of-week: 1970-01-01 was Thursday = 4. IEC indexes 1..=7 with
    // Monday = 1.
    let dow = (((days + 3).rem_euclid(7)) + 1) as u8;
    // Civil-from-days algorithm by Howard Hinnant.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d, dow)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_known_vector_for_empty_input() {
        assert_eq!(crc16_ibm(b""), 0x0000);
    }

    #[test]
    fn crc16_for_123456789_is_bb3d() {
        // "123456789" -> CRC-16/ARC = 0xBB3D (well-known reference).
        assert_eq!(crc16_ibm(b"123456789"), 0xBB3D);
    }

    #[tokio::test]
    async fn fs_provider_lists_files() {
        let tmp = tempdir();
        std::fs::write(tmp.join("alpha.bin"), b"hello").unwrap();
        std::fs::write(tmp.join("beta.bin"), b"world!").unwrap();
        let p = FsFileTransferProvider::new(&tmp).unwrap();
        let entries = p.list_directory().await.unwrap();
        assert_eq!(entries.len(), 2);
        // Last entry must carry the "last_file" flag.
        assert!(entries.last().unwrap().meta.status.last_file);
    }

    #[tokio::test]
    async fn fs_provider_read_then_eof() {
        let tmp = tempdir();
        std::fs::write(tmp.join("blob.bin"), b"hello world").unwrap();
        let p = FsFileTransferProvider::new(&tmp).unwrap();
        let entries = p.list_directory().await.unwrap();
        let nof = entries[0].nof;
        let mut r = p.open_read(nof).await.unwrap();
        let chunk = r.read_segment(64).await.unwrap().unwrap();
        assert_eq!(chunk, b"hello world");
        assert!(r.read_segment(64).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fs_provider_write_finalize_renames_partial() {
        let tmp = tempdir();
        let p = FsFileTransferProvider::new(&tmp).unwrap();
        let nof = NameOfFile(0xABCD);
        let mut w = p.open_write(nof, 5).await.unwrap();
        w.write_segment(b"hello").await.unwrap();
        w.finalize(true).await.unwrap();
        let final_name = tmp.join("upload_ABCD.bin");
        assert!(final_name.exists());
        assert!(!tmp.join("upload_ABCD.bin.partial").exists());
        assert_eq!(std::fs::read(&final_name).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn fs_provider_write_failure_deletes_partial() {
        let tmp = tempdir();
        let p = FsFileTransferProvider::new(&tmp).unwrap();
        let nof = NameOfFile(0x1234);
        let mut w = p.open_write(nof, 0).await.unwrap();
        w.write_segment(b"x").await.unwrap();
        w.finalize(false).await.unwrap();
        // Neither the partial nor the final file should remain.
        assert!(!tmp.join("upload_1234.bin").exists());
        assert!(!tmp.join("upload_1234.bin.partial").exists());
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nonce: u64 = std::time::SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        p.push(format!("iec60870-fsfp-{pid}-{nonce}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

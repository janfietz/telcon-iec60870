//! Client-side last-value cache.
//!
//! The master daemon's event pump decodes incoming ASDUs from the outstation
//! and calls [`PointCache::update`] for every information object. Callers can
//! then query the freshest seen value for a given IOA — with or without a
//! specific Type ID — without issuing a new poll.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use iec60870::proto::asdu::ie::Cp56Time2a;

use crate::wire::{PointKind, PointValue, QualityWire};

/// A single cached point observation.
#[derive(Debug, Clone)]
pub struct CachedPoint {
    /// Decoded value.
    pub value: PointValue,
    /// Quality bits at the time of the last update.
    pub quality: QualityWire,
    /// Optional time-tag embedded in the ASDU itself.
    pub timestamp: Option<Cp56Time2a>,
    /// Wall-clock instant at which this update was received.
    pub received_at: Instant,
}

/// Composite cache key: `(PointKind, ioa)`.
///
/// Using `PointKind` (rather than the raw `u8` Type ID) keeps the key
/// bounded to the ten types we actually decode.
type CacheKey = (PointKind, u32);

/// Shared last-value store.
///
/// Cheaply cloneable; all clones share the same underlying lock.
#[derive(Debug, Clone)]
pub struct PointCache {
    inner: Arc<RwLock<HashMap<CacheKey, CachedPoint>>>,
}

impl PointCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Insert or overwrite the cached value for `(kind, ioa)`.
    pub fn update(
        &self,
        kind: PointKind,
        ioa: u32,
        value: PointValue,
        quality: QualityWire,
        timestamp: Option<Cp56Time2a>,
    ) {
        let key = (kind, ioa);
        let entry = CachedPoint {
            value,
            quality,
            timestamp,
            received_at: Instant::now(),
        };
        self.inner
            .write()
            .expect("cache lock poisoned")
            .insert(key, entry);
    }

    /// Look up a specific `(kind, ioa)` pair.
    pub fn get(&self, kind: PointKind, ioa: u32) -> Option<CachedPoint> {
        self.inner
            .read()
            .expect("cache lock poisoned")
            .get(&(kind, ioa))
            .cloned()
    }

    /// Return the most-recently-updated entry for `ioa`, searching across all
    /// known `PointKind`s. If `type_id` is `Some`, only that type is searched.
    pub fn get_by_ioa(&self, ioa: u32, type_id: Option<u8>) -> Option<(PointKind, CachedPoint)> {
        let guard = self.inner.read().expect("cache lock poisoned");
        let mut best: Option<(PointKind, CachedPoint)> = None;
        for (&(kind, k_ioa), point) in guard.iter() {
            if k_ioa != ioa {
                continue;
            }
            if let Some(tid) = type_id {
                if kind.type_id() != tid {
                    continue;
                }
            }
            let is_newer = best
                .as_ref()
                .is_none_or(|(_, b)| point.received_at > b.received_at);
            if is_newer {
                best = Some((kind, point.clone()));
            }
        }
        best
    }

    /// Return all cached entries sorted by IOA, then by `PointKind` ordinal.
    /// Useful for the interrogation collector result set.
    pub fn list_all(&self) -> Vec<(PointKind, u32, CachedPoint)> {
        let guard = self.inner.read().expect("cache lock poisoned");
        let mut entries: Vec<(PointKind, u32, CachedPoint)> = guard
            .iter()
            .map(|(&(kind, ioa), pt)| (kind, ioa, pt.clone()))
            .collect();
        entries.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.0.type_id().cmp(&b.0.type_id()))
        });
        entries
    }
}

impl Default for PointCache {
    fn default() -> Self {
        Self::new()
    }
}

//! Access observation for the namespace provider (issue #43, design §4).
//!
//! §4's one non-negotiable for the FUSE provider is that every open, read and close
//! updates `lifecycle` directly, so "usage" stops being an atime that `noatime`/ZFS never
//! advance. This module is that bookkeeping, split from `fuse.rs` so it can be tested
//! against a real catalog without a mount — CI has no usable `/dev/fuse`.
//!
//! # What is counted
//!
//! `lifecycle.accesses` counts **opens**: one open is one use of the object. A read or a
//! close refreshes `lifecycle.last_access` but does not add to the counter, because the
//! number of `read` calls a consumer makes is a function of its buffer size, not of how
//! much the object is used — counting them would let `cat` and `dd bs=1` disagree by six
//! orders of magnitude about the same access.
//!
//! # The loss bound, named
//!
//! Observations are buffered in memory and written to the catalog in one transaction per
//! flush, so a read-heavy consumer does not turn every `read` into an SQLite write. A
//! flush happens:
//!
//! * on every **close** (`release`) — the access is complete, so it is recorded;
//! * on any open or read arriving at least [`FLUSH_INTERVAL`] after the last flush;
//! * when the filesystem is unmounted (`destroy`).
//!
//! A daemon killed with `SIGKILL` therefore loses **at most the observations made since the
//! last flush**: the opens and reads of files still open at the kill, plus at most
//! [`FLUSH_INTERVAL`] of activity on files that were never closed. A completed access —
//! one whose close reached the daemon — is never lost. A failed flush keeps its batch
//! pending and retries on the next trigger rather than dropping it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::catalog::{Catalog, CatalogError};

/// The longest a buffered observation waits for a flush while its file stays open.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Observations not yet written to the catalog, keyed by namespace path.
#[derive(Debug)]
pub struct AccessLog {
    /// path -> (latest access, unix seconds; opens to add).
    pending: BTreeMap<String, (i64, u64)>,
    last_flush: Instant,
}

impl Default for AccessLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessLog {
    pub fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            last_flush: Instant::now(),
        }
    }

    /// An open through the provider: one access, stamped `at`.
    pub fn opened(&mut self, path: &str, at: SystemTime) {
        let entry = self.entry(path, at);
        entry.1 += 1;
    }

    /// A read or close through the provider: refreshes the stamp, adds no access.
    pub fn touched(&mut self, path: &str, at: SystemTime) {
        self.entry(path, at);
    }

    fn entry(&mut self, path: &str, at: SystemTime) -> &mut (i64, u64) {
        let seconds = at
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or(0);
        let entry = self.pending.entry(path.to_string()).or_insert((seconds, 0));
        entry.0 = entry.0.max(seconds);
        entry
    }

    /// True once [`FLUSH_INTERVAL`] has passed since the last flush and something waits.
    pub fn due(&self, now: Instant) -> bool {
        !self.pending.is_empty() && now.duration_since(self.last_flush) >= FLUSH_INTERVAL
    }

    /// Paths with an observation not yet in the catalog.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Write every pending observation in one transaction. On error nothing is dropped:
    /// the batch stays pending for the next flush, so a transient `SQLITE_BUSY` delays an
    /// observation instead of losing it. Returns the number of lifecycle rows written
    /// (a name the catalog does not know — created through the mount, not yet synced —
    /// writes none).
    pub fn flush(&mut self, catalog: &mut Catalog) -> Result<usize, CatalogError> {
        self.last_flush = Instant::now();
        if self.pending.is_empty() {
            return Ok(0);
        }
        let batch: Vec<(String, i64, u64)> = self
            .pending
            .iter()
            .map(|(path, (at, opens))| (path.clone(), *at, *opens))
            .collect();
        let written = catalog.record_observed_accesses(&batch)?;
        self.pending.clear();
        Ok(written)
    }
}

//! Cache overlays: a promotion target in front of a tier of record (§2.1, issue #46).
//!
//! An [`Overlay`] holds *copies*. Promoting a file copies its bytes into the overlay and
//! changes nothing else: the file's home, its catalog `location` rows, its copy floor and
//! its scrub state are untouched, because a promotion is not a move. That one property is
//! what lets the rest of this module be careless in the way a cache is allowed to be:
//!
//! * **No journal, no fsync.** Losing an overlay entry is not data loss — the home still
//!   has the bytes — so a crash mid-promotion needs no recovery, only a cleanup, and an
//!   unsynced copy that vanishes on power loss is simply a cache miss.
//! * **Empty on restart.** [`Overlay::open`] discards whatever a previous process left in
//!   the overlay's own directory instead of re-deriving it. Re-deriving would mean
//!   trusting bytes nothing was watching while the process was down: a home rewritten
//!   while the mount was gone would be served stale from RAM. Empty is always correct.
//! * **Recency, not policy.** Eviction is LRU against `max_size`. Lifecycle rules (§5)
//!   never see an overlay, and an overlay never sees a rule.
//!
//! # Coherency: write-invalidate, checked twice
//!
//! A write, truncate or rename through the namespace lands on the home tier and calls
//! [`Overlay::invalidate`]. That alone is not enough, because a home can also be changed
//! by something that is not the mount (a direct edit of the pool, `catalog sync`'s own
//! reads are harmless but a `restore` is not). So every entry also remembers the home's
//! size and mtime at promotion, and [`Overlay::read`] drops an entry whose home no longer
//! matches before serving it. A promotion that raced a write — the home changed while the
//! copy was being taken — is discarded the same way, never published.
//!
//! # Where the bytes go
//!
//! Inside `<cache.path>/.just_cache-overlay-<name>/`, a directory the overlay owns
//! outright. The configured `path` itself must already exist (a missing ramdisk is the
//! same "unmounted disk silently becomes a directory" hazard as invariant 1), and nothing
//! outside the owned directory is ever written or removed.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::tiers::CacheConfig;

/// The prefix of the directory an overlay owns under its configured path.
pub const OVERLAY_DIR_PREFIX: &str = ".just_cache-overlay-";

/// Why an overlay could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The configured path is not an existing directory. It is never created: a ramdisk
    /// that is not mounted must not quietly become a directory on the root filesystem.
    #[error("cache `{name}`: path {path} is not an existing directory (it is never created)")]
    MissingPath { name: String, path: PathBuf },
    #[error("cache `{name}`: {source}")]
    Io {
        name: String,
        #[source]
        source: io::Error,
    },
}

/// The identity of a home file's bytes, as far as a cache can cheaply tell. A mismatch is
/// treated as "the home changed", which is the safe direction: a false mismatch is only a
/// cache miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HomeStamp {
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ino: u64,
}

impl HomeStamp {
    fn of(path: &Path) -> io::Result<HomeStamp> {
        let md = fs::metadata(path)?;
        Ok(HomeStamp {
            len: md.len(),
            mtime: md.mtime(),
            mtime_nsec: md.mtime_nsec(),
            ino: md.ino(),
        })
    }
}

#[derive(Debug, Clone)]
struct Entry {
    object: String,
    file: PathBuf,
    size: u64,
    home: PathBuf,
    stamp: HomeStamp,
    last_used: u64,
}

/// One resident entry, as reported for observability. Ephemeral by construction: it
/// describes a copy that may be gone by the time it is read, and never a home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residency {
    pub key: String,
    pub object: String,
    pub size: u64,
}

/// What one read did to the overlay, so a caller that mirrors residency (the catalog's
/// `cache_residency` table) can follow along without the overlay touching the catalog.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReadOutcome {
    /// The file to serve the read from: the overlay copy when resident, `None` meaning
    /// "read the home".
    pub serve: Option<PathBuf>,
    /// The key was promoted by this read.
    pub promoted: bool,
    /// Keys that stopped being resident during this read (evicted to make room, or
    /// dropped because their home changed).
    pub dropped: Vec<String>,
}

/// A live overlay in front of one tier of record.
#[derive(Debug)]
pub struct Overlay {
    config: CacheConfig,
    /// The canonical root of the tier this overlay accelerates. Only bytes under it are
    /// ever promoted: an overlay `over = "ssd"` must not quietly cache the HDD pool.
    home_root: PathBuf,
    dir: PathBuf,
    entries: HashMap<String, Entry>,
    /// Access times (unix seconds) inside the `promote_on` window, per key, for keys not
    /// yet resident.
    accesses: HashMap<String, VecDeque<u64>>,
    used: u64,
    clock: u64,
    next_file: u64,
}

impl Overlay {
    /// Open an overlay for `config`, in front of the tier rooted at `home_root`. Any
    /// contents a previous process left in the owned directory are discarded (see the
    /// module docs: empty is the only state that cannot be stale).
    pub fn open(config: CacheConfig, home_root: &Path) -> Result<Overlay, CacheError> {
        let io_err = |source| CacheError::Io {
            name: config.name.clone(),
            source,
        };
        if !config.path.is_dir() {
            return Err(CacheError::MissingPath {
                name: config.name.clone(),
                path: config.path.clone(),
            });
        }
        let dir = config
            .path
            .join(format!("{OVERLAY_DIR_PREFIX}{}", config.name));
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_err(err)),
        }
        fs::create_dir(&dir).map_err(io_err)?;
        let home_root = home_root
            .canonicalize()
            .unwrap_or_else(|_| home_root.to_path_buf());
        Ok(Overlay {
            config,
            home_root,
            dir,
            entries: HashMap::new(),
            accesses: HashMap::new(),
            used: 0,
            clock: 0,
            next_file: 0,
        })
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    /// The directory this overlay owns.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Bytes currently held.
    pub fn used(&self) -> u64 {
        self.used
    }

    /// True when `home` lives on the tier this overlay sits in front of.
    pub fn covers(&self, home: &Path) -> bool {
        let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        home.starts_with(&self.home_root)
    }

    pub fn is_resident(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Every resident entry, most recently used first.
    pub fn residency(&self) -> Vec<Residency> {
        let mut entries: Vec<(&String, &Entry)> = self.entries.iter().collect();
        entries.sort_by_key(|(_, entry)| std::cmp::Reverse(entry.last_used));
        entries
            .into_iter()
            .map(|(key, entry)| Residency {
                key: key.clone(),
                object: entry.object.clone(),
                size: entry.size,
            })
            .collect()
    }

    /// Record a read of namespace path `key`, whose home bytes are at `home`, at unix
    /// time `now`. Serves from the overlay when resident and still coherent; promotes
    /// when this read meets `promote_on`. A failure to promote is never an error for the
    /// read — the home is always there to serve it — so I/O problems here degrade to a
    /// miss.
    pub fn read(&mut self, key: &str, object: &str, home: &Path, now: u64) -> ReadOutcome {
        let mut outcome = ReadOutcome::default();
        if !self.covers(home) {
            return outcome;
        }
        let current = HomeStamp::of(home).ok();

        if let Some(entry) = self.entries.get(key) {
            if current == Some(entry.stamp) && entry.home == home && entry.object == object {
                self.clock += 1;
                let clock = self.clock;
                let entry = self.entries.get_mut(key).expect("checked above");
                entry.last_used = clock;
                outcome.serve = Some(entry.file.clone());
                return outcome;
            }
            // The home moved on without telling us. Drop, then fall through and count
            // this read towards a fresh promotion of the new bytes.
            self.drop_entry(key);
            outcome.dropped.push(key.to_string());
        }

        let Some(stamp) = current else {
            return outcome;
        };
        let window = self.config.promote_on.window.as_secs();
        let needed = self.config.promote_on.accesses as usize;
        let seen = self.accesses.entry(key.to_string()).or_default();
        seen.push_back(now);
        while seen
            .front()
            .is_some_and(|first| now.saturating_sub(*first) >= window)
        {
            seen.pop_front();
        }
        if seen.len() < needed {
            return outcome;
        }
        self.accesses.remove(key);

        if stamp.len > self.config.max_size {
            // Larger than the whole overlay: promoting it would evict everything and
            // still not fit. Not a candidate, ever, at this size.
            return outcome;
        }
        outcome.dropped.extend(self.evict_until_fits(stamp.len));
        if let Ok(Some(file)) = self.copy_in(home, stamp) {
            self.clock += 1;
            self.used += stamp.len;
            self.entries.insert(
                key.to_string(),
                Entry {
                    object: object.to_string(),
                    file: file.clone(),
                    size: stamp.len,
                    home: home.to_path_buf(),
                    stamp,
                    last_used: self.clock,
                },
            );
            outcome.promoted = true;
            outcome.serve = Some(file);
        }
        outcome
    }

    /// Drop the copy of `key`, if any: a write, truncate or rename through the namespace
    /// is about to change (or has changed) the home. Returns whether a copy was dropped.
    /// Pending access counts are forgotten too, so the next promotion is earned by reads
    /// of the new bytes.
    pub fn invalidate(&mut self, key: &str) -> bool {
        self.accesses.remove(key);
        self.drop_entry(key)
    }

    fn drop_entry(&mut self, key: &str) -> bool {
        match self.entries.remove(key) {
            Some(entry) => {
                self.used = self.used.saturating_sub(entry.size);
                // A copy that cannot be removed is orphaned bytes in the owned directory,
                // reclaimed on the next open; it is no longer reachable, so it is no
                // longer served.
                let _ = fs::remove_file(&entry.file);
                true
            }
            None => false,
        }
    }

    /// Evict least-recently-used entries until `incoming` more bytes fit in `max_size`.
    fn evict_until_fits(&mut self, incoming: u64) -> Vec<String> {
        let mut evicted = Vec::new();
        while self.used + incoming > self.config.max_size {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.drop_entry(&victim);
            evicted.push(victim);
        }
        evicted
    }

    /// Copy `home` into the owned directory. `Ok(None)` when the home changed during the
    /// copy: a copy of bytes that were being rewritten is not published.
    fn copy_in(&mut self, home: &Path, before: HomeStamp) -> io::Result<Option<PathBuf>> {
        self.next_file += 1;
        let file = self.dir.join(format!("{:016x}", self.next_file));
        let copied = fs::copy(home, &file)?;
        let after = HomeStamp::of(home).ok();
        if after != Some(before) || copied != before.len {
            let _ = fs::remove_file(&file);
            return Ok(None);
        }
        Ok(Some(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiers::PromoteOn;
    use std::time::Duration;

    fn config(path: &Path, max_size: u64, accesses: u32) -> CacheConfig {
        CacheConfig {
            name: "ram".into(),
            over: "ssd".into(),
            kind: "fs".into(),
            path: path.to_path_buf(),
            max_size,
            promote_on: PromoteOn {
                accesses,
                window: Duration::from_secs(3600),
            },
        }
    }

    #[test]
    fn a_missing_overlay_path_is_never_created() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("ramdisk");
        let err = Overlay::open(config(&missing, 10, 1), tmp.path()).unwrap_err();
        assert!(matches!(err, CacheError::MissingPath { .. }), "{err}");
        assert!(!missing.exists());
    }

    #[test]
    fn accesses_outside_the_window_do_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("ssd");
        let ram = tmp.path().join("ram");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&ram).unwrap();
        fs::write(home.join("a"), b"aaaa").unwrap();
        let mut overlay = Overlay::open(config(&ram, 100, 2), &home).unwrap();
        assert!(!overlay.read("a", "id", &home.join("a"), 0).promoted);
        // An hour later the first access has aged out, so this is one access, not two.
        assert!(!overlay.read("a", "id", &home.join("a"), 3600).promoted);
        assert!(overlay.read("a", "id", &home.join("a"), 3601).promoted);
    }

    #[test]
    fn bytes_outside_the_over_tier_are_never_promoted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("ssd");
        let other = tmp.path().join("hdd");
        let ram = tmp.path().join("ram");
        for dir in [&home, &other, &ram] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(other.join("a"), b"aaaa").unwrap();
        let mut overlay = Overlay::open(config(&ram, 100, 1), &home).unwrap();
        let outcome = overlay.read("a", "id", &other.join("a"), 0);
        assert!(!outcome.promoted && outcome.serve.is_none());
    }
}

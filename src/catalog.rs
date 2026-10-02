//! The catalog: SQLite as the source of truth for where files live.
//!
//! The tree used to *be* the database — where a file lives was discovered by walking
//! symlinks, and whether it was healthy by walking both sides. That works for one host
//! with two disks and stops working the moment a tier is not mounted, a copy is one of
//! two, or a file needs restoring after its path moved. This module makes the mover's
//! state *readable*: a content-addressed catalog (`docs/design.md` §3) whose identity is
//! the BLAKE3 hash of the bytes and whose `name` table is the namespace.
//!
//! ## What `sync` is, and what it deliberately is not
//!
//! `sync` **ingests the current state** of a tree the mover has already been running
//! against. It is not a migration: the mover stays the only writer of the filesystem
//! (issue #16), and a file moved before the catalog existed is picked up by the first
//! sync exactly like one moved after, because both leave the same auditable pattern.
//!
//! It is also not an overwrite. A tree that was edited by hand is a tree whose catalog
//! is now wrong, and silently rewriting the catalog to match would destroy the one thing
//! the catalog is for: a record that is trustworthy because it is *about* something. So
//! the pass splits observed facts in two:
//!
//! * facts nothing contradicts — a new object, a new name, a new location — are ingested;
//! * facts the tree contradicts — a name the catalog had is gone or now holds different
//!   bytes, a location's file vanished or was replaced — are **reported**, and the
//!   catalog rows are left exactly as they were. A human decides what the move means.
//!
//! ## Transactionality, and why it is the whole point
//!
//! Every write of one sync happens inside one SQLite transaction, committed only at the
//! end. "A catalog that disagrees with the tree is worse than none" is the issue's
//! warning and it is right: a half-applied ingest would look like a complete one to
//! everything downstream that trusts the catalog. An interrupted `sync` therefore leaves
//! the catalog byte-for-byte as it was, which is the same guarantee `journal.rs` gives a
//! move.
//!
//! ## Cache residency is not a location (§2.1)
//!
//! A copy in a promotion target is re-derivable and volatility is its nature; recording
//! it in `location` would let a restart promote a RAM copy to data of record. Nothing in
//! this module ever writes a cache to `location` — the only tiers written are the watch
//! root and the `--dest` roots — and the `cache_residency` table exists for observability
//! at most and is never a durability input.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::digest;
use crate::disk_management::{self, DiskError};

/// Name of the catalog inside (beside) the watched root. It shares the journal's
/// `.just_cache` prefix so the walk skips it: the catalog must never become a move
/// candidate, and an interrupted write leaves a `-journal` sibling with the same prefix.
pub const CATALOG_NAME: &str = ".just_cache-catalog.sqlite";

/// Bytes are in the tree at this name. The readable state of a file nobody has moved.
pub const STATE_PRESENT: &str = "present";
/// Bytes are only on a cold tier; the name is a symlink into it.
pub const STATE_OFFLOADED: &str = "offloaded";
/// Both a hot copy and a cold copy exist. A restore that has not finished, or a source
/// removal that never happened — `audit` is what tells those apart.
pub const STATE_RESTORING: &str = "restoring";

/// A content hash, 32 BLAKE3 bytes. Identity, never a path.
type ObjectId = Vec<u8>;

/// Where a copy lives: `(tier, storage_key)`. The tier is the root the bytes sit under
/// (the watch root, or one `--dest` root); the key is the path within it.
type LocationKey = (String, String);

/// The schema from `docs/design.md` §3, plus one table (`cache_residency`) that is
/// deliberately never filled. Foreign keys are on so a name can never point at an object
/// that does not exist — the transaction test relies on exactly that refusal.
const SCHEMA: &str = "
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS object (
    id          BLOB PRIMARY KEY,      -- content hash (BLAKE3), not path
    size        INTEGER NOT NULL,
    checksum    BLOB NOT NULL,         -- verified at every ingest and scrub
    created_at  INTEGER NOT NULL,
    state       TEXT NOT NULL          -- 'present' | 'offloaded' | 'restoring'
);
CREATE TABLE IF NOT EXISTS location (
    object_id   BLOB NOT NULL REFERENCES object(id),
    tier        TEXT NOT NULL,
    storage_key TEXT NOT NULL,         -- path within the tier
    is_primary  INTEGER NOT NULL,      -- tier of record
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (tier, storage_key)
);
CREATE TABLE IF NOT EXISTS name (
    object_id   BLOB NOT NULL REFERENCES object(id),
    path        TEXT NOT NULL,         -- namespace path, provider-agnostic
    PRIMARY KEY (path)
);
CREATE TABLE IF NOT EXISTS lifecycle (
    object_id    BLOB PRIMARY KEY REFERENCES object(id),
    last_access  INTEGER NOT NULL,     -- observed by the namespace provider, or atime
    accesses     INTEGER NOT NULL,     -- observed counter since ingest
    pinned_until INTEGER,              -- pin wins over policy
    rule         TEXT                  -- which rule decided the last transition
);
CREATE TABLE IF NOT EXISTS volume (
    id    TEXT PRIMARY KEY,            -- 'drawer-07'
    state TEXT NOT NULL,               -- 'in_vault' | 'mounted' | 'loaned' | 'lost'
    note  TEXT
);
-- Ephemeral, observability only: a promotion target's contents are re-derivable and
-- MUST NOT be read as data of record (§2.1). No code path in this module writes here.
CREATE TABLE IF NOT EXISTS cache_residency (
    object_id   BLOB NOT NULL,
    cache       TEXT NOT NULL,
    key         TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (cache, key)
);

CREATE INDEX IF NOT EXISTS location_by_object ON location(object_id);
CREATE INDEX IF NOT EXISTS name_by_object ON name(object_id);
";

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("cannot open the catalog at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("catalog query failed: {0}")]
    Query(#[from] rusqlite::Error),
    #[error("cannot walk {path}: {source}")]
    Walk {
        path: PathBuf,
        #[source]
        source: Box<DiskError>,
    },
    #[error("cannot read symlink {path}: {source}")]
    ReadLink {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot checksum {path}: {source}")]
    Checksum {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// How a name or a location disagrees with what the catalog recorded.
///
/// Every one of these is "the tree changed under the catalog". Each is reported and the
/// affected rows are left alone; none of them is silently reconciled away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DifferenceKind {
    /// A name the catalog had is absent from the tree: moved or deleted by hand.
    NameVanished,
    /// A name still exists but now hashes to different bytes.
    NameReplaced,
    /// A location the catalog recorded has no file at its storage key any more.
    LocationMissing,
    /// A location's path exists but holds different bytes than the catalog recorded.
    LocationChanged,
    /// Cold bytes that no name references, so nothing in the tree will ever read them.
    OrphanedCopy,
    /// A symlink in the tree points at nothing.
    DanglingSymlink,
    /// A symlink resolves outside every configured `--dest`.
    UnexpectedTarget,
}

impl DifferenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DifferenceKind::NameVanished => "name-vanished",
            DifferenceKind::NameReplaced => "name-replaced",
            DifferenceKind::LocationMissing => "location-missing",
            DifferenceKind::LocationChanged => "location-changed",
            DifferenceKind::OrphanedCopy => "orphaned-copy",
            DifferenceKind::DanglingSymlink => "dangling-symlink",
            DifferenceKind::UnexpectedTarget => "unexpected-target",
        }
    }
}

/// One disagreement between the catalog and the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub kind: DifferenceKind,
    /// The namespace path the disagreement is about.
    pub path: PathBuf,
    pub detail: String,
}

impl Difference {
    pub fn describe(&self) -> String {
        if self.detail.is_empty() {
            format!("  {}: {}", self.kind.as_str(), self.path.display())
        } else {
            format!(
                "  {}: {} ({})",
                self.kind.as_str(),
                self.path.display(),
                self.detail
            )
        }
    }
}

/// One location row, for callers that report or test the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationRecord {
    pub tier: String,
    pub storage_key: String,
    pub is_primary: bool,
    /// The object id, hex-encoded so it is printable.
    pub object: String,
}

/// The result of one sync.
#[derive(Debug)]
pub struct SyncReport {
    pub catalog: PathBuf,
    pub watch: PathBuf,
    pub dests: Vec<PathBuf>,
    pub objects: usize,
    pub names: usize,
    pub locations: usize,
    pub objects_ingested: usize,
    pub names_ingested: usize,
    pub locations_ingested: usize,
    pub differences: Vec<Difference>,
}

impl SyncReport {
    pub fn has_differences(&self) -> bool {
        !self.differences.is_empty()
    }

    /// The readable summary. The wording says out loud that a difference was *not*
    /// reconciled, because "N differences" alone reads like the sync failed rather than
    /// like it refused to guess.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "catalog sync: {} -> {}",
            self.watch.display(),
            self.catalog.display()
        )];
        lines.push(format!(
            "  objects: {} ({} new), names: {} ({} new), locations: {} ({} new)",
            self.objects,
            self.objects_ingested,
            self.names,
            self.names_ingested,
            self.locations,
            self.locations_ingested
        ));
        if self.differences.is_empty() {
            lines.push("no differences: the catalog agrees with the tree".to_string());
            return lines;
        }
        lines.push(format!(
            "differences: {} (left unresolved; the catalog was NOT rewritten to match)",
            self.differences.len()
        ));
        for difference in &self.differences {
            lines.push(difference.describe());
        }
        lines
    }
}

/// What one pass over the tree and its tiers saw. Built before anything is written, so a
/// filesystem error cannot leave a half-applied catalog.
#[derive(Debug, Default)]
pub(crate) struct Observation {
    /// id -> size.
    pub(crate) objects: BTreeMap<ObjectId, u64>,
    /// namespace path -> object id.
    pub(crate) names: BTreeMap<String, ObjectId>,
    /// (tier, storage_key) -> object id.
    pub(crate) locations: BTreeMap<LocationKey, ObjectId>,
    /// id -> most recent observed access (atime), for `lifecycle`.
    pub(crate) accessed: BTreeMap<ObjectId, i64>,
    /// Tier string of the watched (hot) root.
    pub(crate) watch_tier: String,
    /// Structural problems found while walking, independent of the catalog.
    pub(crate) differences: Vec<Difference>,
}

/// The catalog's prior contents, loaded once per sync to compare against.
#[derive(Debug, Default)]
struct Existing {
    /// id -> state.
    objects: BTreeMap<ObjectId, String>,
    names: BTreeMap<String, ObjectId>,
    locations: BTreeMap<LocationKey, ObjectId>,
}

/// What one `apply` did, before it is folded into a [`SyncReport`].
#[derive(Debug, Default)]
struct Applied {
    objects_new: usize,
    names_new: usize,
    locations_new: usize,
    differences: Vec<Difference>,
}

/// One side of a symlink, resolved far enough to ingest.
enum LinkState {
    /// Resolves to a regular file under a configured `--dest`.
    Cold {
        target: PathBuf,
        key: String,
        tier: String,
    },
    /// Points at something that does not exist.
    Dangling { target: PathBuf },
    /// Resolves, but outside every `--dest` (or at something that is not a file).
    Outside { target: PathBuf },
}

/// The SQLite catalog, opened at a path of the caller's choosing.
pub struct Catalog {
    conn: Connection,
    path: PathBuf,
}

impl Catalog {
    /// Open (creating on first use) the catalog at `path`, applying the schema.
    ///
    /// Creating it is not a silent side effect: `--catalog` names the file, and the
    /// default is derived from `--watch`, so a user always knows where their source of
    /// truth lives. What must never happen is *creating bytes elsewhere* — a missing
    /// `--dest` is still an error, because an unmounted tier must not become a directory
    /// on the wrong filesystem (invariant 1).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CatalogError> {
        let path = path.into();
        let conn = Connection::open(&path).map_err(|source| CatalogError::Open {
            path: path.clone(),
            source,
        })?;
        conn.execute_batch(SCHEMA)
            .map_err(|source| CatalogError::Open {
                path: path.clone(),
                source,
            })?;
        Ok(Catalog { conn, path })
    }

    /// The catalog that belongs to this watched root, when `--catalog` is not given.
    pub fn default_path(watch: &Path) -> PathBuf {
        watch.join(CATALOG_NAME)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ingest the current state of `watch` and `dests`, reporting — never reconciling —
    /// anything that contradicts what the catalog already recorded.
    pub fn sync(&mut self, watch: &Path, dests: &[PathBuf]) -> Result<SyncReport, CatalogError> {
        let observation = observe(watch, dests)?;
        let existing = self.load_state()?;
        let applied = self.apply(&observation, &existing)?;
        Ok(SyncReport {
            catalog: self.path.clone(),
            watch: watch.to_path_buf(),
            dests: dests.to_vec(),
            objects: self.object_count()?,
            names: self.name_count()?,
            locations: self.location_count()?,
            objects_ingested: applied.objects_new,
            names_ingested: applied.names_new,
            locations_ingested: applied.locations_new,
            differences: applied.differences,
        })
    }

    /// Write one observation inside a single transaction. Either every fact lands or
    /// none does; a caller that sees an `Err` can trust the catalog is unchanged.
    fn apply(
        &mut self,
        observation: &Observation,
        existing: &Existing,
    ) -> Result<Applied, CatalogError> {
        let mut applied = Applied {
            differences: observation.differences.clone(),
            ..Applied::default()
        };

        // Which objects have a hot copy, and which a cold one. Besides deciding `state`,
        // this is what tells a relocation (hot location legitimately gone, cold location
        // present) from a location that simply vanished.
        let mut hot_objects: BTreeSet<ObjectId> = BTreeSet::new();
        let mut cold_objects: BTreeSet<ObjectId> = BTreeSet::new();
        for ((tier, _), id) in &observation.locations {
            if *tier == observation.watch_tier {
                hot_objects.insert(id.clone());
            } else {
                cold_objects.insert(id.clone());
            }
        }
        let state_of = |id: &ObjectId| -> &'static str {
            match (hot_objects.contains(id), cold_objects.contains(id)) {
                (true, true) => STATE_RESTORING,
                (true, false) => STATE_PRESENT,
                (false, _) => STATE_OFFLOADED,
            }
        };

        let now = now_seconds();
        let tx = self.conn.transaction()?;

        // 1. Objects. A known object keeps its identity and only has its state refreshed;
        //    a new one is created (never before its names, which reference it).
        for (id, size) in &observation.objects {
            let state = state_of(id);
            match existing.objects.get(id) {
                Some(previous) if previous.as_str() == state => {}
                Some(_) => {
                    tx.execute(
                        "UPDATE object SET state = ?1 WHERE id = ?2",
                        params![state, id],
                    )?;
                }
                None => {
                    tx.execute(
                        "INSERT INTO object (id, size, checksum, created_at, state)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![id, *size as i64, id, now, state],
                    )?;
                    let access = observation.accessed.get(id).copied().unwrap_or(now);
                    tx.execute(
                        "INSERT OR IGNORE INTO lifecycle
                             (object_id, last_access, accesses, pinned_until, rule)
                         VALUES (?1, ?2, 0, NULL, NULL)",
                        params![id, access],
                    )?;
                    applied.objects_new += 1;
                }
            }
        }

        // 2. Names nothing contradicts are ingested; a name that now holds different
        //    bytes is the tree having been edited, and is reported rather than repointed.
        for (path, id) in &observation.names {
            match existing.names.get(path) {
                Some(previous) if previous == id => {}
                Some(previous) => applied.differences.push(Difference {
                    kind: DifferenceKind::NameReplaced,
                    path: PathBuf::from(path),
                    detail: format!("was {}, now {}", hex(previous), hex(id)),
                }),
                None => {
                    tx.execute(
                        "INSERT INTO name (object_id, path) VALUES (?1, ?2)",
                        params![id, path],
                    )?;
                    applied.names_new += 1;
                }
            }
        }

        // 3. A name the catalog had that is no longer in the tree. Left in place: the
        //    bytes may still be on a tier, and only a human knows whether the path moved
        //    or the file is gone (identity is the hash, so a rename is still this object).
        for (path, id) in &existing.names {
            if !observation.names.contains_key(path) {
                applied.differences.push(Difference {
                    kind: DifferenceKind::NameVanished,
                    path: PathBuf::from(path),
                    detail: format!("catalog still records object {}", hex(id)),
                });
            }
        }

        // 4. Locations. Same rule as names: a new one is ingested, a changed one is
        //    reported and the row keeps saying what the catalog recorded.
        for (key, id) in &observation.locations {
            match existing.locations.get(key) {
                Some(previous) if previous == id => {}
                Some(previous) => applied.differences.push(Difference {
                    kind: DifferenceKind::LocationChanged,
                    path: PathBuf::from(&key.1),
                    detail: format!("on {} was {}, now {}", key.0, hex(previous), hex(id)),
                }),
                None => {
                    // A tier of record that is hot when a hot copy exists, otherwise the
                    // cold copy this row is. `fix_primaries` later repairs any object
                    // whose primary was the hot location that has just moved.
                    let primary =
                        i64::from(key.0 != observation.watch_tier && !hot_objects.contains(id));
                    tx.execute(
                        "INSERT INTO location (object_id, tier, storage_key, is_primary, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![id, key.0, key.1, primary, now],
                    )?;
                    applied.locations_new += 1;
                }
            }
        }

        // 5. Locations the catalog had that are not in the tree. A move by the mover
        //    leaves the same namespace path as a symlink to the new cold location, so
        //    when that is what we see the stale hot row is dropped — the name still
        //    references the object, and the object still has its cold location, so
        //    nothing is stranded. Anything else is reported and kept.
        for ((tier, key), id) in &existing.locations {
            let observed_key = (tier.clone(), key.clone());
            if observation.locations.contains_key(&observed_key) {
                continue;
            }
            let relocated = *tier == observation.watch_tier
                && observation.names.get(key) == Some(id)
                && observation
                    .locations
                    .iter()
                    .any(|((t, _), o)| o == id && *t != observation.watch_tier);
            if relocated {
                tx.execute(
                    "DELETE FROM location WHERE tier = ?1 AND storage_key = ?2",
                    params![tier, key],
                )?;
            } else {
                applied.differences.push(Difference {
                    kind: DifferenceKind::LocationMissing,
                    path: PathBuf::from(key),
                    detail: format!("no file at {} on {tier} any more", key),
                });
            }
        }

        // 6. Cold bytes no name references: nothing in the tree will ever read them.
        //    An object whose name vanished is already reported as such, so it is not
        //    double-counted here.
        let named: BTreeSet<ObjectId> = observation
            .names
            .values()
            .cloned()
            .chain(existing.names.values().cloned())
            .collect();
        for ((tier, key), id) in &observation.locations {
            if *tier == observation.watch_tier || named.contains(id) {
                continue;
            }
            applied.differences.push(Difference {
                kind: DifferenceKind::OrphanedCopy,
                path: PathBuf::from(key),
                detail: format!("cold bytes on {tier} that no name references"),
            });
        }

        // 7. Exactly one tier of record per object: the hot location while the file is
        //    in the tree, a cold location once it has moved.
        tx.execute(
            "UPDATE location SET is_primary = 0
             WHERE tier <> ?1
               AND object_id IN (SELECT object_id FROM location WHERE tier = ?1)",
            params![observation.watch_tier],
        )?;
        tx.execute(
            "UPDATE location SET is_primary = 1
             WHERE rowid IN (
                 SELECT MIN(rowid) FROM location
                  WHERE object_id NOT IN (SELECT object_id FROM location WHERE is_primary = 1)
                  GROUP BY object_id
             )",
            [],
        )?;

        tx.commit()?;
        Ok(applied)
    }

    fn load_state(&self) -> Result<Existing, CatalogError> {
        let mut existing = Existing::default();

        let mut objects = self.conn.prepare("SELECT id, state FROM object")?;
        let rows = objects.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, state) = row?;
            existing.objects.insert(id, state);
        }

        let mut names = self.conn.prepare("SELECT path, object_id FROM name")?;
        let rows = names.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (path, id) = row?;
            existing.names.insert(path, id);
        }

        let mut locations = self
            .conn
            .prepare("SELECT tier, storage_key, object_id FROM location")?;
        let rows = locations.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        for row in rows {
            let (tier, key, id) = row?;
            existing.locations.insert((tier, key), id);
        }

        Ok(existing)
    }

    // -- Read-only queries, for reporting and for tests that must see the real rows. --

    pub fn object_count(&self) -> Result<usize, CatalogError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM object", [], |row| {
                row.get::<_, i64>(0)
            })? as usize)
    }

    pub fn name_count(&self) -> Result<usize, CatalogError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM name", [], |row| row.get::<_, i64>(0))?
            as usize)
    }

    pub fn location_count(&self) -> Result<usize, CatalogError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM location", [], |row| {
                row.get::<_, i64>(0)
            })? as usize)
    }

    /// Every name and the object it resolves to, hex-encoded, in path order.
    pub fn all_names(&self) -> Result<Vec<(String, String)>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, object_id FROM name ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, hex(&row.get::<_, Vec<u8>>(1)?)))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every location row, in tier/key order.
    pub fn all_locations(&self) -> Result<Vec<LocationRecord>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare("SELECT tier, storage_key, is_primary, object_id FROM location ORDER BY tier, storage_key")?;
        let rows = stmt.query_map([], |row| {
            Ok(LocationRecord {
                tier: row.get::<_, String>(0)?,
                storage_key: row.get::<_, String>(1)?,
                is_primary: row.get::<_, i64>(2)? != 0,
                object: hex(&row.get::<_, Vec<u8>>(3)?),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The state of the object a namespace path names, if that path is in the catalog.
    pub fn state_for_path(&self, path: &str) -> Result<Option<String>, CatalogError> {
        self.conn
            .query_row(
                "SELECT o.state FROM name n JOIN object o ON o.id = n.object_id WHERE n.path = ?1",
                params![path],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// The object id a namespace path names, hex-encoded.
    pub fn object_for_path(&self, path: &str) -> Result<Option<String>, CatalogError> {
        self.conn
            .query_row(
                "SELECT object_id FROM name WHERE path = ?1",
                params![path],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map(|found| found.map(|id| hex(&id)))
            .map_err(Into::into)
    }
}

/// Walk the watched tree and the cold tiers and put down every fact they show.
///
/// Cold tiers are walked first so an orphaned copy is visible even when no name points
/// at it; the name pass then adds names (and re-adds the same locations, which the map
/// dedupes). Nothing here writes anything.
fn observe(watch: &Path, dests: &[PathBuf]) -> Result<Observation, CatalogError> {
    let watch_root = canonical(watch);
    let watch_tier = key_of(&watch_root);
    let dest_roots: Vec<(PathBuf, String)> = dests
        .iter()
        .map(|dest| {
            let root = canonical(dest);
            let tier = key_of(&root);
            (root, tier)
        })
        .collect();

    let mut observation = Observation {
        watch_tier: watch_tier.clone(),
        ..Observation::default()
    };
    // One hash per unique file: a symlink and the cold scan both see the same bytes, and
    // hashing a 40 GB file twice would be the tool's own worst enemy.
    let mut hashes: BTreeMap<PathBuf, ObjectId> = BTreeMap::new();

    for (root, tier) in &dest_roots {
        for entry in list(root)? {
            // A symlink that appeared on a cold tier is not the mover's output and is
            // not a copy of anything.
            if entry.is_symlink {
                continue;
            }
            let id = hash_path(&mut hashes, &entry.path)?;
            record_object(
                &mut observation,
                &id,
                entry.size,
                access_epoch(&entry.last_access),
            );
            observation
                .locations
                .insert((tier.clone(), key_of(&entry.relative)), id);
        }
    }

    for entry in list(&watch_root)? {
        let access = access_epoch(&entry.last_access);
        if entry.is_symlink {
            match resolve_link(&entry.path, &dest_roots)? {
                LinkState::Cold { target, key, tier } => {
                    let id = hash_path(&mut hashes, &target)?;
                    let size = fs::metadata(&target)
                        .map(|metadata| metadata.len())
                        .map_err(|source| CatalogError::Checksum {
                            path: target.clone(),
                            source,
                        })?;
                    record_object(&mut observation, &id, size, access);
                    observation
                        .names
                        .insert(key_of(&entry.relative), id.clone());
                    observation.locations.insert((tier, key), id);
                }
                LinkState::Dangling { target } => {
                    observation.differences.push(Difference {
                        kind: DifferenceKind::DanglingSymlink,
                        path: entry.path.clone(),
                        detail: format!("points at {}, which does not exist", target.display()),
                    });
                }
                LinkState::Outside { target } => {
                    observation.differences.push(Difference {
                        kind: DifferenceKind::UnexpectedTarget,
                        path: entry.path.clone(),
                        detail: format!("points at {}, outside every --dest", target.display()),
                    });
                }
            }
        } else {
            let id = hash_path(&mut hashes, &entry.path)?;
            record_object(&mut observation, &id, entry.size, access);
            let key = key_of(&entry.relative);
            observation.names.insert(key.clone(), id.clone());
            observation.locations.insert((watch_tier.clone(), key), id);
        }
    }

    Ok(observation)
}

fn list(root: &Path) -> Result<Vec<disk_management::FileEntry>, CatalogError> {
    disk_management::list_files_recursive(root).map_err(|source| CatalogError::Walk {
        path: root.to_path_buf(),
        source: Box::new(source),
    })
}

fn hash_path(
    hashes: &mut BTreeMap<PathBuf, ObjectId>,
    path: &Path,
) -> Result<ObjectId, CatalogError> {
    let key = canonical(path);
    if let Some(id) = hashes.get(&key) {
        return Ok(id.clone());
    }
    let hash = digest::file_digest(path).map_err(|source| CatalogError::Checksum {
        path: path.to_path_buf(),
        source,
    })?;
    let id = hash.as_bytes().to_vec();
    hashes.insert(key, id.clone());
    Ok(id)
}

fn record_object(observation: &mut Observation, id: &ObjectId, size: u64, access: i64) {
    observation.objects.entry(id.clone()).or_insert(size);
    let slot = observation.accessed.entry(id.clone()).or_insert(access);
    if access > *slot {
        *slot = access;
    }
}

/// Resolve a symlink to a cold tier, or say which kind of problem it is.
fn resolve_link(link: &Path, dests: &[(PathBuf, String)]) -> Result<LinkState, CatalogError> {
    let target = fs::read_link(link).map_err(|source| CatalogError::ReadLink {
        path: link.to_path_buf(),
        source,
    })?;
    let resolved = if target.is_absolute() {
        target.clone()
    } else {
        // A relative link is relative to the directory holding the link.
        link.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&target)
    };
    let Ok(canonical_target) = fs::canonicalize(&resolved) else {
        return Ok(LinkState::Dangling { target });
    };
    if !fs::metadata(&canonical_target).is_ok_and(|metadata| metadata.is_file()) {
        return Ok(LinkState::Outside { target });
    }
    for (root, tier) in dests {
        if let Ok(relative) = canonical_target.strip_prefix(root) {
            let key = key_of(relative);
            return Ok(LinkState::Cold {
                target: canonical_target,
                key,
                tier: tier.clone(),
            });
        }
    }
    Ok(LinkState::Outside { target })
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn key_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn access_epoch(time: &SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_in(tmp: &Path) -> Catalog {
        Catalog::open(tmp.join("catalog.sqlite")).expect("catalog opens")
    }

    #[test]
    fn a_catalog_is_created_on_first_use_with_the_design_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = catalog_in(tmp.path());
        assert!(catalog.path().exists());
        // The tables the design fixes are all there; a missing one would only surface
        // much later, as a query error in some unrelated command.
        for table in ["object", "location", "name", "lifecycle", "volume"] {
            let found: i64 = catalog
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "table {table} must exist");
        }
    }

    #[test]
    fn an_interrupted_ingest_leaves_no_partial_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let mut catalog = catalog_in(tmp.path());

        // A well-formed object, then a name pointing at an object that was never
        // inserted. Foreign keys refuse the second insert, and the whole transaction —
        // including the object written first — must roll back. This is the difference
        // between "the catalog disagrees with itself" and "nothing happened".
        let good: ObjectId = vec![7u8; 32];
        let mut observation = Observation {
            watch_tier: "/hot".to_string(),
            ..Observation::default()
        };
        observation.objects.insert(good.clone(), 3);
        observation.names.insert("a.bin".to_string(), good);
        observation
            .names
            .insert("ghost.bin".to_string(), vec![9u8; 32]);

        let existing = catalog.load_state().unwrap();
        assert!(catalog.apply(&observation, &existing).is_err());
        assert_eq!(catalog.object_count().unwrap(), 0);
        assert_eq!(catalog.name_count().unwrap(), 0);
        assert_eq!(catalog.location_count().unwrap(), 0);
    }

    #[test]
    fn a_sync_never_writes_a_cache_residency_to_location() {
        // Only the watch root and the --dest roots may ever be a location's tier, so a
        // restart can never promote a volatile copy to data of record (§2.1).
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(watch.join("live.bin"), b"hot").unwrap();
        fs::write(cold.join("moved.bin"), b"cold").unwrap();
        std::os::unix::fs::symlink("../cold/moved.bin", watch.join("moved.bin")).unwrap();

        let mut catalog = catalog_in(tmp.path());
        catalog.sync(&watch, std::slice::from_ref(&cold)).unwrap();

        let allowed = [canonical(&watch), canonical(&cold)];
        for location in catalog.all_locations().unwrap() {
            let tier = PathBuf::from(&location.tier);
            assert!(
                allowed.contains(&tier),
                "location tier {} is not the watch root or a --dest root",
                location.tier
            );
        }
    }
}

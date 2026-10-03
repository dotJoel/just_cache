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
//! A file the pass cannot read is neither: it is reported per file, with its path and the
//! error, and the rest of the tree is ingested anyway. Stopping at the first unreadable
//! file would make the whole catalog all-or-nothing, and the mover's rule 7 — one file
//! failing never stops a sweep — has no reason not to apply here (issue #53). The failure
//! is *listed*, never inferred on: nothing is recorded as gone, missing or empty because a
//! read failed, so a hole in the catalog is visible instead of a lie.
//!
//! ## Transactionality, and why it is the whole point
//!
//! Every write of one sync happens inside one SQLite transaction, committed only at the
//! end. "A catalog that disagrees with the tree is worse than none" is the issue's
//! warning and it is right: a half-applied ingest would look like a complete one to
//! everything downstream that trusts the catalog. An interrupted `sync` therefore leaves
//! the catalog byte-for-byte as it was, which is the same guarantee `journal.rs` gives a
//! move. A file that could not be read is reported and excluded from the observation
//! *before* the transaction opens, so "the readable subset committed" and "the whole thing
//! rolled back" stay the only two outcomes — never a catalog half-written because the walk
//! died in the middle.
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
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
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
    verified    INTEGER NOT NULL DEFAULT 0,  -- 1 once a digest vouched for this copy
    checksum    BLOB,                        -- digest this copy verified to; NULL = unknown
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
-- The durability floor, recorded once per tier rather than guessed from how many
-- locations happen to exist (issue #20). Sweep --copies N declares it; a missing copy
-- against it is an under-replicated finding, not silence.
CREATE TABLE IF NOT EXISTS tier (
    name   TEXT PRIMARY KEY,           -- canonical root of the tier
    copies INTEGER NOT NULL DEFAULT 1  -- the copy floor for objects that live here
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

-- Scrub state (issue #21): one row per location, recording the last time its bytes
-- hashed back to the object they claim to be. This is what lets a restart resume instead
-- of re-reading a whole tier. It is a *new table* rather than a column added to
-- `location` on purpose: `CREATE TABLE IF NOT EXISTS` is then the entire migration, and
-- a catalog written before scrub existed gains it on the next open with no `ALTER TABLE`
-- and no window in which a partially-upgraded schema can be read.
CREATE TABLE IF NOT EXISTS scrub_state (
    tier        TEXT NOT NULL,
    storage_key TEXT NOT NULL,
    object_id   BLOB NOT NULL REFERENCES object(id),  -- the id verified at verified_at
    verified_at INTEGER NOT NULL,
    PRIMARY KEY (tier, storage_key)
);
-- Corruption for which no good copy existed to repair from. The object is never deleted
-- for this: a damaged copy is still the only evidence of what the bytes were meant to
-- be, and a human decides. A clean read (a later scrub, or a restore) clears the row.
CREATE TABLE IF NOT EXISTS damage (
    object_id   BLOB NOT NULL REFERENCES object(id),
    tier        TEXT NOT NULL,
    storage_key TEXT NOT NULL,
    detected_at INTEGER NOT NULL,
    detail      TEXT NOT NULL,
    PRIMARY KEY (tier, storage_key)
);
CREATE INDEX IF NOT EXISTS damage_by_object ON damage(object_id);
-- The canonical roots the catalog was synced from: the watch root and every `--dest`
-- root. A reader joins a location's `tier` to a path only when it is one of these, so a
-- hand-edited (or restored-from-backup) catalog cannot point a scrub's replace, a
-- reconcile's create, or an audit's hash outside every destination root (issue #72).
-- `catalog sync` is the only writer, like every other table here.
CREATE TABLE IF NOT EXISTS root (
    path TEXT PRIMARY KEY
);
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
    #[error("refusing to open the catalog at {path}: it is a symbolic link")]
    Symlink { path: PathBuf },
    #[error("refusing to open the catalog at {path}: not a regular file")]
    NotRegular { path: PathBuf },
    #[error(
        "refusing to open the catalog at {path}: owned by uid {owner}, and its directory is \
         group- or world-writable so another user could have planted it"
    )]
    ForeignOwner { path: PathBuf, owner: u32 },
    #[error("cannot stat the catalog at {path}: {source}")]
    Stat {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create the catalog at {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
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
/// Most of these are "the tree changed under the catalog". Each is reported and the
/// affected rows are left alone; none of them is silently reconciled away.
///
/// [`DifferenceKind::Unreadable`] is the one entry that is not a disagreement: it is a
/// file the pass could not read, so the pass has no fact about it at all. It is reported
/// through the same channel — never silently dropped — but the code that would infer a
/// vanish or a missing location from the file's absence must not run for it (issue #53).
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
    /// Fewer verified copies than the object's tier floor requires. The detail names
    /// the disks it is on and the ones it is not.
    UnderReplicated,
    /// A location the mover wrote but no digest has vouched for. It counts toward no
    /// floor until a sync hashes it — unknown, not assumed good.
    ReplicaUnknown,
    /// A file the pass could not read, so nothing about it was ingested. The `detail` is
    /// the error. This is not a statement that the file is missing or empty — only that
    /// this run could not observe it (issue #53).
    Unreadable,
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
            DifferenceKind::UnderReplicated => "under-replicated",
            DifferenceKind::ReplicaUnknown => "replica-unknown",
            DifferenceKind::Unreadable => "unreadable",
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
    /// True when a digest has vouched for the bytes at this location; false means the
    /// mover wrote bytes it could not verify — unknown, not good.
    pub verified: bool,
    /// The digest the copy verified to, hex-encoded. `None` when unverified.
    pub checksum: Option<String>,
    /// The object id, hex-encoded so it is printable.
    pub object: String,
}

/// One object as `locate` and `explain` need to see it: identity, size, lifecycle, every
/// copy, and every name that answers to it.
///
/// Names and locations both come along because a content digest is not a path: identical
/// bytes can sit at two names, and one object can have a hot copy and a cold one at once.
/// Reporting only the first would answer a question nobody asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRecord {
    /// The object id, hex-encoded. This is the identity, not a path.
    pub id: String,
    pub size: u64,
    /// The recorded digest a copy must match to be intact, hex-encoded. Equal to `id`
    /// today (identity *is* the hash at ingest), but kept as its own field: `audit`
    /// compares a copy against `checksum`, not against the id, and a scrub may re-record a
    /// checksum of the same identity after verifying a copy.
    pub checksum: String,
    /// `present` | `offloaded` | `restoring`.
    pub state: String,
    /// Unix seconds of the last observed access, from `lifecycle`.
    pub last_access: Option<i64>,
    /// The observed-access counter, from `lifecycle`.
    pub accesses: Option<u64>,
    /// Unix seconds until which a pin wins over the idle rule, from `lifecycle`.
    pub pinned_until: Option<i64>,
    /// The lifecycle rule that decided the last transition, if any.
    pub rule: Option<String>,
    /// Every namespace path that names this object, in path order.
    pub names: Vec<String>,
    /// Every copy, primary first.
    pub locations: Vec<LocationRecord>,
}

impl ObjectRecord {
    /// The tier of record: the primary location, or the first copy when no row is marked
    /// primary (a catalog written by hand, or one whose primary was cleared).
    pub fn primary(&self) -> Option<&LocationRecord> {
        self.locations
            .iter()
            .find(|location| location.is_primary)
            .or_else(|| self.locations.first())
    }
}

/// One location to scrub, with what the catalog expects to find there.
///
/// The expected checksum is the object id itself: the identity of every object is the
/// BLAKE3 digest of its bytes (`docs/design.md` §3), which is exactly what makes a scrub
/// possible without a second column to keep in step with the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubTarget {
    /// The tier root the bytes sit under (absolute, canonical when it was ingested).
    pub tier: String,
    /// The path within that tier.
    pub storage_key: String,
    /// The object id the location claims to hold, raw BLAKE3 bytes.
    pub object: Vec<u8>,
    pub size: u64,
    /// The object id recorded the last time this location verified clean, if ever.
    /// `Some` equal to `object` means "already scrubbed"; anything else means it must be
    /// read again (a location that changed content since its last verification).
    pub verified_object: Option<Vec<u8>>,
}

impl ScrubTarget {
    /// Where the bytes are: the tier root joined with the key within it, after proving the
    /// row stays under a root this invocation trusts.
    ///
    /// This is the only way a recorded row becomes a filesystem path for a scrub. A row
    /// whose tier is not a trusted root, whose key is absolute, or whose key contains `..`
    /// is refused before any filesystem call, so a hand-edited (or restored-from-backup)
    /// catalog cannot point the tool at a file outside every destination root (issue #72,
    /// `SECURITY.md`).
    pub fn path(&self, roots: &[PathBuf]) -> Result<PathBuf, RowPathError> {
        resolve_location_path(&self.tier, &self.storage_key, roots)
    }

    /// True when this location has already been verified against the object it now
    /// claims, so a resuming scrub can skip re-reading it.
    pub fn is_already_verified(&self) -> bool {
        self.verified_object.as_deref() == Some(self.object.as_slice())
    }

    pub fn object_hex(&self) -> String {
        hex(&self.object)
    }
}

/// Why a recorded `(tier, storage_key)` is not a filesystem path the tool may touch.
///
/// A catalog row is data, and a path built from that data is one an editor of the catalog
/// controls. `Path::join` with an absolute component discards the tier, and a `..`
/// component walks out of it lexically — so a row is checked against the roots this
/// invocation trusts before it becomes a path (issue #72).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowPathError {
    /// The tier is not one of the canonical roots this invocation trusts. The catalog
    /// writer only ever recorded a watch or `--dest` root, so anything else is a row no
    /// current root explains.
    UnknownTier,
    /// The key is absolute: `Path::join` would replace the tier with it entirely.
    AbsoluteKey,
    /// The key contains a `..` component: the join walks out of the tier lexically.
    ParentDirKey,
    /// The joined path is not under the tier root (defence in depth; with the key checks
    /// above this only fires for a tier that is not itself canonical).
    Escapes,
}

impl RowPathError {
    /// The reason, for a report line.
    pub fn detail(&self) -> &'static str {
        match self {
            RowPathError::UnknownTier => "tier is not one of the current roots",
            RowPathError::AbsoluteKey => "storage_key is absolute and would replace the tier",
            RowPathError::ParentDirKey => "storage_key contains `..` and would escape the tier",
            RowPathError::Escapes => "resolved path does not sit under the tier root",
        }
    }
}

/// A catalog row the reader refused to turn into a path, reported instead of touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedRow {
    pub tier: String,
    pub storage_key: String,
    pub detail: String,
}

impl MalformedRow {
    pub fn new(tier: &str, storage_key: &str, error: &RowPathError) -> Self {
        MalformedRow {
            tier: tier.to_string(),
            storage_key: storage_key.to_string(),
            detail: error.detail().to_string(),
        }
    }

    /// One report line, in the voice of the rest of the tool's output.
    pub fn describe(&self) -> String {
        format!(
            "  malformed catalog row {}/{}: {} (no filesystem operation)",
            self.tier, self.storage_key, self.detail
        )
    }
}

/// Turn a recorded `(tier, storage_key)` into the filesystem path to touch, refusing any
/// row that does not provably stay under a trusted root.
///
/// `roots` are the canonical root paths this invocation trusts: the watch root and every
/// `--dest`, as `catalog sync` recorded them. The tier must be one of them exactly; an
/// empty set (a catalog not yet synced since root tracking was added) fails closed because
/// the reader has no independent way to tell a real root from an edited row. Everything
/// the three readers do with a row — stat, hash, create, replace — goes through here
/// first, so a refused row is only ever *reported*, never touched.
pub fn resolve_location_path(
    tier: &str,
    storage_key: &str,
    roots: &[PathBuf],
) -> Result<PathBuf, RowPathError> {
    let tier_path = Path::new(tier);
    if tier_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(RowPathError::UnknownTier);
    }

    // With no independently observed roots we cannot distinguish a legitimate absolute
    // tier from one substituted into an edited location row, so refuse it. A `catalog sync`
    // refreshes root rows from a filesystem walk before readers act on this catalog.
    let root = roots
        .iter()
        .find(|root| root.as_path() == tier_path)
        .cloned()
        .ok_or(RowPathError::UnknownTier)?;

    let key = Path::new(storage_key);
    if key.is_absolute() {
        return Err(RowPathError::AbsoluteKey);
    }
    if key
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(RowPathError::ParentDirKey);
    }

    let path = root.join(key);
    if !path.starts_with(&root) {
        return Err(RowPathError::Escapes);
    }
    Ok(path)
}

/// The counts behind "has this copy ever been scrubbed?", for `audit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrubSummary {
    /// Every location the catalog records.
    pub locations: usize,
    /// Locations whose last recorded verification matches the object they hold.
    pub verified: usize,
    /// Locations with no verification, or one that no longer matches: never scrubbed
    /// (or scrubbed before the file changed under the catalog).
    pub never_scrubbed: usize,
    /// Locations recorded damaged by a scrub that found no good copy to repair from.
    pub damaged: usize,
}

impl ScrubSummary {
    pub fn summary_lines(&self) -> Vec<String> {
        vec![format!(
            "  scrub: {} location(s): {} verified, {} never scrubbed, {} damaged",
            self.locations, self.verified, self.never_scrubbed, self.damaged
        )]
    }
}

/// One object a reconcile pass (`just_cache reconcile`, issue #24) considers, with every
/// location recorded for it and which of those carry a damage mark.
///
/// This is not [`ScrubTarget`]'s job: a reconcile decides *where a missing copy should
/// go* — which needs the object's recorded checksum, every location row, and which
/// siblings a scrub has already given up on — and it never reads bytes, so it carries no
/// scrub-state either. The checksum is what makes a rebuild safe at all: without it there
/// is no way to tell a valid sibling from a same-size stranger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileObject {
    /// The object id, raw BLAKE3 bytes.
    pub object: Vec<u8>,
    /// The recorded checksum a rebuilt copy must hash to. Equal to `object` today
    /// (identity *is* the hash at ingest), kept separate so a rebuild proves the bytes
    /// against what the catalog *recorded*, not against a tautology.
    pub checksum: Vec<u8>,
    pub size: u64,
    /// `present` | `offloaded` | `restoring`.
    pub state: String,
    /// Every location row, primary first.
    pub locations: Vec<LocationRecord>,
    /// Locations a scrub marked damaged: `(tier, storage_key)`. A rebuild may still use
    /// such a sibling, but only after re-reading it — which the pass always does — and a
    /// sibling that fails that re-read must never be copied from.
    pub damaged: BTreeSet<(String, String)>,
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
    /// like it refused to guess. Unreadable files are split out from disagreements: a
    /// disagreement is about a fact the tree contradicts, an unreadable file is a fact
    /// this pass could not obtain at all, and merging the two would blur the catalog's
    /// hole into a claim about the tree (issue #53).
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

        let (unreadable, disagreements): (Vec<&Difference>, Vec<&Difference>) = self
            .differences
            .iter()
            .partition(|difference| difference.kind == DifferenceKind::Unreadable);

        if !disagreements.is_empty() {
            lines.push(format!(
                "differences: {} (left unresolved; the catalog was NOT rewritten to match)",
                disagreements.len()
            ));
            for difference in disagreements {
                lines.push(difference.describe());
            }
        }
        if !unreadable.is_empty() {
            lines.push(format!(
                "unreadable: {} file(s) not ingested (reported, not guessed: the catalog records nothing about them)",
                unreadable.len()
            ));
            for difference in unreadable {
                lines.push(difference.describe());
            }
        }
        lines
    }
}

/// One difference a resolution pass concluded about, and — unless the pass was report-only —
/// applied. The `detail` names the evidence that supported the conclusion, because the whole
/// point of the command is that the conclusion is *earned*: a rename is only a rename when
/// the object it names still exists somewhere the tree vouches for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// What kind of difference the row was.
    pub kind: DifferenceKind,
    /// The namespace path (or, for a location, its storage key) the difference was about.
    pub path: PathBuf,
    /// The evidence and the conclusion, in one line.
    pub detail: String,
    /// False when the pass was report-only (`--apply` absent) and nothing was written.
    pub applied: bool,
}

impl Resolution {
    pub fn describe(&self) -> String {
        let verb = if self.applied {
            "resolved"
        } else {
            "would resolve"
        };
        if self.detail.is_empty() {
            format!("  {verb} {}: {}", self.kind.as_str(), self.path.display())
        } else {
            format!(
                "  {verb} {}: {} ({})",
                self.kind.as_str(),
                self.path.display(),
                self.detail
            )
        }
    }
}

/// The result of one `catalog resolve`.
///
/// Two lists, and the split is the contract: `resolutions` are differences the evidence
/// supported a conclusion for; `irreconcilable` are differences left exactly as they were,
/// with the reason. Nothing is ever deleted from the filesystem, and no row is dropped if
/// that would leave an object without its last record.
#[derive(Debug)]
pub struct ResolveReport {
    pub catalog: PathBuf,
    pub watch: PathBuf,
    pub dests: Vec<PathBuf>,
    /// True when `--apply` was given and the catalog was written.
    pub apply: bool,
    /// Differences a conclusion was drawn about, in path order.
    pub resolutions: Vec<Resolution>,
    /// Differences left alone because the evidence did not support a conclusion.
    pub irreconcilable: Vec<Difference>,
}

impl ResolveReport {
    /// True when the catalog and the tree still disagree: an irreconcilable difference is
    /// still there, or a report-only pass found something it could resolve but did not.
    /// An `--apply` run that resolved everything writes and exits clean.
    pub fn has_findings(&self) -> bool {
        !self.irreconcilable.is_empty() || (!self.apply && !self.resolutions.is_empty())
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let mode = if self.apply {
            "applied; the catalog was rewritten only where the evidence supported it"
        } else {
            "report only; nothing was written (pass --apply to resolve)"
        };
        let mut lines = vec![format!(
            "catalog resolve: {} -> {}",
            self.watch.display(),
            self.catalog.display()
        )];
        lines.push(format!("  {mode}"));
        lines.push(format!(
            "  resolved: {}, irreconcilable: {}",
            self.resolutions.len(),
            self.irreconcilable.len()
        ));
        for resolution in &self.resolutions {
            lines.push(resolution.describe());
        }
        for difference in &self.irreconcilable {
            lines.push(format!(
                "  not resolved {}",
                difference.describe().trim_start()
            ));
        }
        if self.resolutions.is_empty() && self.irreconcilable.is_empty() {
            lines.push("nothing to resolve: the catalog agrees with the tree".to_string());
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
    /// Canonical root strings this observation was taken under: the watch root and every
    /// `--dest`. Persisted so a later reader can re-check that a row's tier is real
    /// instead of trusting the stored string (issue #72).
    pub(crate) roots: BTreeSet<String>,
    /// Structural problems found while walking, independent of the catalog. Carries the
    /// per-file [`DifferenceKind::Unreadable`] reports too: a file that could not be read
    /// is a fact about this pass that must reach the operator, never a silent skip.
    pub(crate) differences: Vec<Difference>,
    /// Namespace keys whose bytes could not be read this pass. Absence alone would make
    /// the name look vanished; a read failure is not evidence of a vanish, so the
    /// vanished-name report is suppressed for these (issue #53).
    pub(crate) unreadable_names: BTreeSet<String>,
    /// Location keys whose bytes could not be read this pass, for the same reason: a
    /// location we could not read is not a location observed to be missing.
    pub(crate) unreadable_locations: BTreeSet<LocationKey>,
}

/// The catalog's prior contents, loaded once per sync to compare against.
#[derive(Debug, Default)]
struct Existing {
    /// id -> state.
    objects: BTreeMap<ObjectId, String>,
    names: BTreeMap<String, ObjectId>,
    locations: BTreeMap<LocationKey, ObjectId>,
    /// Location keys the mover wrote but no digest has vouched for yet. A sync that
    /// hashes one of these upgrades it rather than assuming it was good.
    unverified: BTreeSet<LocationKey>,
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
    ///
    /// Opening is also an untrusted-input boundary. The default path is inside the
    /// watched tree, and SQLite opens a database with a plain `open(2)` and creates
    /// predictable `-journal`/`-wal`/`-shm` siblings beside it — all of which a symlink
    /// planted in a shared tree would redirect at some other file the tool's user can
    /// write. So the name is created exclusively and privately (mode 0600, no umask
    /// leakage) when absent, refused when it is a symlink or not a regular file, and
    /// opened with `SQLITE_OPEN_NOFOLLOW`; the sibling names are checked the same way
    /// because a journal is written before the first byte of a transaction lands.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CatalogError> {
        let path = path.into();
        // Siblings first: a refusal must not leave a freshly created catalog behind when
        // the very name SQLite is about to write a journal to is the planted one.
        refuse_siblings(&path)?;
        create_or_check(&path)?;
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|source| CatalogError::Open {
            path: path.clone(),
            source,
        })?;
        conn.execute_batch(SCHEMA)
            .map_err(|source| CatalogError::Open {
                path: path.clone(),
                source,
            })?;
        migrate(&conn).map_err(|source| CatalogError::Open {
            path: path.clone(),
            source,
        })?;
        Ok(Catalog { conn, path })
    }

    /// Open the catalog only if it already exists, never creating one.
    ///
    /// This is what the mover uses to record a replica it placed: the sweep is not
    /// allowed to bring a catalog into being (invariant 9 — only `catalog sync` does
    /// that), but when one is there it is the right place to note "I wrote a copy, and
    /// here is whether a digest vouched for it".
    ///
    /// `symlink_metadata`, not `exists`: a symlink at the name is a catalog this process
    /// must refuse, and `exists` would follow it (and report a dangling one as absent).
    pub fn open_existing(path: impl Into<PathBuf>) -> Result<Option<Self>, CatalogError> {
        let path = path.into();
        match fs::symlink_metadata(&path) {
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(CatalogError::Stat { path, source }),
            Ok(_) => Self::open(path).map(Some),
        }
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
    ///
    /// A file that cannot be read does not stop the pass: it is reported per file (with
    /// its path and the error) as [`DifferenceKind::Unreadable`], the rest of the tree is
    /// ingested, and nothing is inferred about the file itself (issue #53).
    pub fn sync(&mut self, watch: &Path, dests: &[PathBuf]) -> Result<SyncReport, CatalogError> {
        let observation = observe(watch, dests)?;
        let existing = self.load_state()?;
        let applied = self.apply(&observation, &existing)?;
        // Under-replication is computed after the ingest but from the observation, so a
        // copy that vanished between the walk and the transaction is still counted as
        // absent — the floor is about what is really on the disks, not what was.
        let mut differences = applied.differences;
        differences.extend(self.under_replicated(&observation, dests)?);
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
            differences,
        })
    }

    /// Resolve the differences a `sync` reported, where the tree's own evidence supports a
    /// conclusion — and only there.
    ///
    /// `sync` is report-not-heal on purpose: a name the catalog recorded that is gone, and a
    /// location whose file vanished or was replaced, are reported and their rows left alone,
    /// so the difference repeats on every pass until a human decides. This is that human
    /// command. It re-observes the tree and the tiers exactly as `sync` does, then concludes
    /// only where the evidence is a *surviving reference the catalog already records*:
    ///
    /// * a vanished name is dropped when the object it pointed at is still named elsewhere
    ///   in the tree (the rename/duplicate case) — the recorded digest of the file now at the
    ///   new name is what the observation hashed to the same object id;
    /// * a vanished location is dropped when the object survives as another name or another
    ///   copy — the deleted replica, with a sibling left;
    /// * a name whose path now holds different bytes is repointed only when the object it
    ///   used to name still survives named elsewhere, so the old object keeps its record.
    ///
    /// Everything else is *irreconcilable* and is reported, not guessed at: an object with no
    /// surviving copy keeps its rows (invariant 6 — recovery never discards the only record
    /// of a thing), and a replacement whose old object survives nowhere is refused because
    /// repointing would drop the last record of it. No file is ever deleted; the only
    /// mutations are to catalog rows, and only for differences the evidence settled.
    ///
    /// `apply` is the whole safety switch. Without it the pass writes nothing at all — the
    /// default stays report-only, so a cron job can run `resolve` to *see* what it would do.
    ///
    /// # Why it does not ingest
    ///
    /// The command consumes the differences a `sync` already reported: the new name a rename
    /// created, and the surviving sibling a delete left, are already catalog rows by the time
    /// this runs. Requiring the surviving reference to be a recorded row is exactly what
    /// keeps this from having to write on the way to deciding — and from silently ingesting a
    /// tree it was not asked to ingest. Run `catalog sync` first; its non-zero exit is the
    /// report this command resolves.
    pub fn resolve(
        &mut self,
        watch: &Path,
        dests: &[PathBuf],
        apply: bool,
    ) -> Result<ResolveReport, CatalogError> {
        let observation = observe(watch, dests)?;
        let existing = self.load_state()?;
        let watch_tier = observation.watch_tier.clone();

        let mut resolutions: Vec<Resolution> = Vec::new();
        // The walk's own structural findings (a dangling symlink, a link outside every
        // --dest) are differences no catalog edit can settle, so they are carried through as
        // irreconcilable rather than silently dropped from the report.
        let mut irreconcilable: Vec<Difference> = observation.differences.clone();
        let mut drop_names: Vec<String> = Vec::new();
        let mut repoint_names: Vec<(String, ObjectId)> = Vec::new();
        let mut drop_locations: BTreeSet<LocationKey> = BTreeSet::new();
        let mut repoint_locations: Vec<(LocationKey, ObjectId)> = Vec::new();
        // A name and the hot location that carried it are one difference. The name pass owns
        // both so the two can never be concluded apart: dropping a name but leaving its hot
        // location row (or the reverse) would just hand `sync` a fresh difference next run.
        let mut handled: BTreeSet<LocationKey> = BTreeSet::new();

        for (path, id) in &existing.names {
            let hot = (watch_tier.clone(), path.clone());
            match observation.names.get(path) {
                // The name is gone from the tree.
                None => {
                    handled.insert(hot.clone());
                    match surviving_name(&observation, &existing, id, path) {
                        Some(survivor) => {
                            drop_names.push(path.clone());
                            if existing.locations.get(&hot) == Some(id)
                                && !observation.locations.contains_key(&hot)
                            {
                                drop_locations.insert(hot);
                            }
                            resolutions.push(Resolution {
                                kind: DifferenceKind::NameVanished,
                                path: PathBuf::from(path),
                                detail: format!(
                                    "object {} still named at {survivor}; stale name dropped",
                                    hex(id)
                                ),
                                applied: apply,
                            });
                        }
                        None => irreconcilable.push(Difference {
                            kind: DifferenceKind::NameVanished,
                            path: PathBuf::from(path),
                            detail: format!(
                                "object {} has no surviving name in the tree; left alone, nothing deleted",
                                hex(id)
                            ),
                        }),
                    }
                }
                Some(new_id) if new_id == id => {}
                // The name still exists but now hashes to different bytes.
                Some(new_id) => {
                    handled.insert(hot.clone());
                    match surviving_name(&observation, &existing, id, path) {
                        Some(survivor) => {
                            repoint_names.push((path.clone(), new_id.clone()));
                            if existing.locations.get(&hot) == Some(id)
                                && observation.locations.get(&hot) == Some(new_id)
                            {
                                repoint_locations.push((hot, new_id.clone()));
                            }
                            resolutions.push(Resolution {
                                kind: DifferenceKind::NameReplaced,
                                path: PathBuf::from(path),
                                detail: format!(
                                    "now {}; name repointed (object {} still named at {survivor})",
                                    hex(new_id),
                                    hex(id)
                                ),
                                applied: apply,
                            });
                        }
                        None => irreconcilable.push(Difference {
                            kind: DifferenceKind::NameReplaced,
                            path: PathBuf::from(path),
                            detail: format!(
                                "was {}, now {}; object {} survives nowhere, so repointing would drop its only record; left alone",
                                hex(id),
                                hex(new_id),
                                hex(id)
                            ),
                        }),
                    }
                }
            }
        }

        for (key, id) in &existing.locations {
            if handled.contains(key) {
                continue;
            }
            match observation.locations.get(key) {
                Some(new_id) if new_id == id => {}
                // A replacement at a location is never concluded here: the bytes the row
                // describes changed in place, and only a human can say whether that is the
                // object rewritten or a different file that took its place. A watch-tier
                // replacement is owned by the name pass above and never reaches this arm.
                Some(new_id) => irreconcilable.push(Difference {
                    kind: DifferenceKind::LocationChanged,
                    path: PathBuf::from(&key.1),
                    detail: format!(
                        "on {} was {}, now {}; a replacement is left for a human",
                        key.0,
                        hex(id),
                        hex(new_id)
                    ),
                }),
                None => {
                    // A mover offload leaves the namespace path a symlink into the cold
                    // tier, and `sync` drops the stale hot row itself. That is not a
                    // difference to resolve, so it is left for the next sync rather than
                    // reported as a resolution here.
                    let relocated = key.0 == watch_tier
                        && observation.names.get(&key.1) == Some(id)
                        && observation
                            .locations
                            .iter()
                            .any(|((tier, _), object)| object == id && *tier != watch_tier);
                    if relocated {
                        continue;
                    }
                    match (
                        surviving_name(&observation, &existing, id, ""),
                        surviving_location(&observation, &existing, id, key),
                    ) {
                        (Some(name), _) => {
                            drop_locations.insert(key.clone());
                            resolutions.push(Resolution {
                                kind: DifferenceKind::LocationMissing,
                                path: PathBuf::from(&key.1),
                                detail: format!(
                                    "on {}; object {} still named at {name}; stale row dropped",
                                    key.0,
                                    hex(id)
                                ),
                                applied: apply,
                            });
                        }
                        (None, Some((tier, storage_key))) => {
                            drop_locations.insert(key.clone());
                            resolutions.push(Resolution {
                                kind: DifferenceKind::LocationMissing,
                                path: PathBuf::from(&key.1),
                                detail: format!(
                                    "on {}; object {} still copied to {tier}/{storage_key}; stale row dropped",
                                    key.0,
                                    hex(id)
                                ),
                                applied: apply,
                            });
                        }
                        (None, None) => irreconcilable.push(Difference {
                            kind: DifferenceKind::LocationMissing,
                            path: PathBuf::from(&key.1),
                            detail: format!(
                                "on {}; object {} has no surviving copy; left alone, nothing deleted",
                                key.0,
                                hex(id)
                            ),
                        }),
                    }
                }
            }
        }

        // Every write is one transaction, for the same reason `sync` is: a half-applied
        // resolution would leave a catalog that is wrong in a new way rather than the old
        // one. An `Err` here leaves the file byte-for-byte as it was.
        if apply
            && !(drop_names.is_empty()
                && repoint_names.is_empty()
                && drop_locations.is_empty()
                && repoint_locations.is_empty())
        {
            let tx = self.conn.transaction()?;
            for path in &drop_names {
                tx.execute("DELETE FROM name WHERE path = ?1", params![path])?;
            }
            for (path, new_id) in &repoint_names {
                tx.execute(
                    "UPDATE name SET object_id = ?1 WHERE path = ?2",
                    params![new_id, path],
                )?;
            }
            for key in &drop_locations {
                tx.execute(
                    "DELETE FROM location WHERE tier = ?1 AND storage_key = ?2",
                    params![key.0, key.1],
                )?;
            }
            // A repointed location now holds the object the observation hashed there, so its
            // checksum and verified flag follow the row rather than describing bytes it no
            // longer contains.
            for (key, new_id) in &repoint_locations {
                tx.execute(
                    "UPDATE location SET object_id = ?1, verified = 1, checksum = ?1, updated_at = ?2
                      WHERE tier = ?3 AND storage_key = ?4",
                    params![new_id, now_seconds(), key.0, key.1],
                )?;
            }
            tx.commit()?;
        }

        resolutions.sort_by(|a, b| a.path.cmp(&b.path));
        irreconcilable.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(ResolveReport {
            catalog: self.path.clone(),
            watch: watch.to_path_buf(),
            dests: dests.to_vec(),
            apply,
            resolutions,
            irreconcilable,
        })
    }

    /// Objects that have fewer verified copies than their recorded tier floor.
    ///
    /// The floor is read from the `tier` table, which only `catalog sync --copies N`
    /// writes; a plain sync has no floors recorded and therefore reports nothing here.
    /// An object that still has a hot copy is skipped — replication is a property of
    /// offloaded bytes, and requiring two copies of a file that has not moved yet would
    /// make a fresh tree look broken.
    fn under_replicated(
        &self,
        observation: &Observation,
        dests: &[PathBuf],
    ) -> Result<Vec<Difference>, CatalogError> {
        let mut differences = Vec::new();

        let mut dest_floors: BTreeMap<String, usize> = BTreeMap::new();
        for dest in dests {
            let tier = key_of(&canonical(dest));
            if let Some(copies) = self.tier_floor(&tier)? {
                dest_floors.insert(tier, copies);
            }
        }
        if dest_floors.is_empty() {
            return Ok(differences);
        }
        let expected = dest_floors.values().copied().max().unwrap_or(1);

        for id in observation.objects.keys() {
            let has_hot = observation
                .locations
                .iter()
                .any(|((tier, _), object)| object == id && *tier == observation.watch_tier);
            if has_hot {
                continue;
            }

            let mut present: BTreeSet<String> = BTreeSet::new();
            for ((tier, _), object) in &observation.locations {
                if object == id && *tier != observation.watch_tier {
                    present.insert(tier.clone());
                }
            }
            if present.len() >= expected {
                continue;
            }

            let missing: Vec<String> = dest_floors
                .keys()
                .filter(|tier| !present.contains(*tier))
                .cloned()
                .collect();
            let path = observation
                .names
                .iter()
                .find(|(_, object)| *object == id)
                .map(|(path, _)| path.clone())
                .unwrap_or_default();
            differences.push(Difference {
                kind: DifferenceKind::UnderReplicated,
                path: PathBuf::from(path),
                detail: format!(
                    "object {} has {} verified copy/copies on [{}], floor {expected}; missing on [{}]",
                    hex(id),
                    present.len(),
                    present.into_iter().collect::<Vec<_>>().join(", "),
                    missing.join(", ")
                ),
            });
        }

        differences.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(differences)
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

        // 0. The roots this catalog was synced from. Recorded inside the one transaction so
        //    an interrupted sync leaves no half-declared root, and so a later reader can
        //    prove a row's tier is one of them instead of joining the stored string blind
        //    (issue #72).
        for root in &observation.roots {
            tx.execute(
                "INSERT OR IGNORE INTO root (path) VALUES (?1)",
                params![root],
            )?;
        }

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
        //    A name whose bytes could not be read is excluded: absence from the
        //    observation is a failed read, not a vanished file, and reporting it as
        //    vanished would invent a deletion (issue #53).
        for (path, id) in &existing.names {
            if observation.names.contains_key(path) || observation.unreadable_names.contains(path) {
                continue;
            }
            applied.differences.push(Difference {
                kind: DifferenceKind::NameVanished,
                path: PathBuf::from(path),
                detail: format!("catalog still records object {}", hex(id)),
            });
        }

        // 4. Locations. Same rule as names: a new one is ingested, a changed one is
        //    reported and the row keeps saying what the catalog recorded. A location
        //    the mover wrote but could not verify is upgraded here — this pass has now
        //    hashed the bytes, which is exactly the vouch it was missing.
        for (key, id) in &observation.locations {
            match existing.locations.get(key) {
                Some(previous) if previous == id => {
                    if existing.unverified.contains(key) {
                        tx.execute(
                            "UPDATE location SET verified = 1, checksum = ?1, updated_at = ?2
                              WHERE tier = ?3 AND storage_key = ?4",
                            params![id, now, key.0, key.1],
                        )?;
                    }
                }
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
                        "INSERT INTO location
                             (object_id, tier, storage_key, is_primary, updated_at, verified, checksum)
                         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?1)",
                        params![id, key.0, key.1, primary, now],
                    )?;
                    applied.locations_new += 1;
                }
            }
        }

        // 4b. A location the mover recorded but no digest has vouched for, still present
        //     on disk and NOT observed as a copy of this object (otherwise it was
        //     upgraded above). It counts toward no floor: unknown, not assumed good.
        for (tier, key) in &existing.unverified {
            let observed_key = (tier.clone(), key.clone());
            let observed_here = observation.locations.contains_key(&observed_key)
                // An unreadable location is not one observed to be gone: a later sync can
                // still read it and vouch for it, so recording "not found" now would be an
                // inference from a failed read (issue #53).
                || observation.unreadable_locations.contains(&observed_key);
            if !observed_here {
                applied.differences.push(Difference {
                    kind: DifferenceKind::ReplicaUnknown,
                    path: PathBuf::from(key),
                    detail: format!(
                        "on {tier}: written but never verified, and not found at its key now"
                    ),
                });
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
            // A location we could not read is not a location observed to be missing. Treat
            // the unreadable file as still there: the row is kept either way, and only the
            // report changes — it must not claim a disappearance the pass did not see
            // (issue #53).
            if observation.unreadable_locations.contains(&observed_key) {
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
            .prepare("SELECT tier, storage_key, object_id, verified FROM location")?;
        let rows = locations.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })?;
        for row in rows {
            let (tier, key, id, verified) = row?;
            if !verified {
                existing.unverified.insert((tier.clone(), key.clone()));
            }
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
        let mut stmt = self.conn.prepare(
            "SELECT tier, storage_key, is_primary, verified, checksum, object_id
             FROM location ORDER BY tier, storage_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(LocationRecord {
                tier: row.get::<_, String>(0)?,
                storage_key: row.get::<_, String>(1)?,
                is_primary: row.get::<_, i64>(2)? != 0,
                verified: row.get::<_, i64>(3)? != 0,
                checksum: row.get::<_, Option<Vec<u8>>>(4)?.map(|bytes| hex(&bytes)),
                object: hex(&row.get::<_, Vec<u8>>(5)?),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Declare the copy floor for a tier. Recorded once per tier, updated when the
    /// operator asks for a different floor; never inferred from the rows that exist.
    pub fn set_tier_floor(&self, tier: &str, copies: usize) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO tier (name, copies) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET copies = excluded.copies",
            params![tier, copies as i64],
        )?;
        Ok(())
    }

    /// The copy floor recorded for a tier, if any.
    pub fn tier_floor(&self, tier: &str) -> Result<Option<usize>, CatalogError> {
        self.conn
            .query_row(
                "SELECT copies FROM tier WHERE name = ?1",
                params![tier],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|found| found.map(|copies| copies.max(0) as usize))
            .map_err(Into::into)
    }

    /// Every recorded tier floor, as `(tier, copies)`, in tier order.
    pub fn all_tier_floors(&self) -> Result<Vec<(String, usize)>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, copies FROM tier ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The canonical roots this catalog was synced from, for a reader that must re-check a
    /// row's `tier` before joining it into a path (issue #72).
    ///
    /// Empty for a catalog written before roots were recorded; [`resolve_location_path`]
    /// then refuses rows until a successful `catalog sync` records independently observed
    /// roots.
    pub fn roots(&self) -> Result<Vec<PathBuf>, CatalogError> {
        let mut stmt = self.conn.prepare("SELECT path FROM root ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(PathBuf::from(row?));
        }
        Ok(out)
    }

    // -- Scrub state (issue #21), read and written per location. --

    /// Every location the catalog records, with the checksum it must hash to and whether
    /// it was verified before. Ordered by object, then tier/key, so a scrub can repair
    /// sibling copies without holding the whole table in a map.
    pub fn scrub_targets(&self) -> Result<Vec<ScrubTarget>, CatalogError> {
        let mut stmt = self.conn.prepare(
            "SELECT l.tier, l.storage_key, l.object_id, o.size, s.object_id
               FROM location l
               JOIN object o ON o.id = l.object_id
               LEFT JOIN scrub_state s ON s.tier = l.tier AND s.storage_key = l.storage_key
              ORDER BY l.object_id, l.tier, l.storage_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ScrubTarget {
                tier: row.get(0)?,
                storage_key: row.get(1)?,
                object: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                verified_object: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every object with the state, checksum, locations and damage marks a reconcile pass
    /// needs, in id order.
    ///
    /// One query each for objects, locations and damage, joined in Rust: the whole point
    /// of a rebuild is that a missing copy has no row of its own to read, so everything
    /// the pass can know must come from the objects that *do* have rows.
    pub fn reconcile_objects(&self) -> Result<Vec<ReconcileObject>, CatalogError> {
        let mut objects: BTreeMap<ObjectId, ReconcileObject> = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT id, size, checksum, state FROM object ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)? as u64,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (id, size, checksum, state) = row?;
            objects.insert(
                id.clone(),
                ReconcileObject {
                    object: id,
                    checksum,
                    size,
                    state,
                    locations: Vec::new(),
                    damaged: BTreeSet::new(),
                },
            );
        }

        let mut stmt = self.conn.prepare(
            "SELECT object_id, tier, storage_key, is_primary, verified, checksum
             FROM location ORDER BY is_primary DESC, tier, storage_key",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                LocationRecord {
                    tier: row.get(1)?,
                    storage_key: row.get(2)?,
                    is_primary: row.get::<_, i64>(3)? != 0,
                    verified: row.get::<_, i64>(4)? != 0,
                    checksum: row.get::<_, Option<Vec<u8>>>(5)?.map(|bytes| hex(&bytes)),
                    object: String::new(),
                },
            ))
        })?;
        for row in rows {
            let (id, location) = row?;
            if let Some(entry) = objects.get_mut(&id) {
                entry.locations.push(location);
            }
        }

        let mut stmt = self
            .conn
            .prepare("SELECT object_id, tier, storage_key FROM damage")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, tier, key) = row?;
            if let Some(entry) = objects.get_mut(&id) {
                entry.damaged.insert((tier, key));
            }
        }

        Ok(objects.into_values().collect())
    }

    /// Every object row, in id order.
    ///
    /// `audit` needs the recorded `checksum` to compare a copy against, which the other
    /// read-only accessors do not carry: `object_for_path` returns identity, and identity
    /// is not what a scrub verifies.
    pub fn all_objects(&self) -> Result<Vec<ObjectRecord>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, size, checksum, state FROM object ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(ObjectRecord {
                id: hex(&row.get::<_, Vec<u8>>(0)?),
                size: row.get::<_, i64>(1)? as u64,
                checksum: hex(&row.get::<_, Vec<u8>>(2)?),
                state: row.get::<_, String>(3)?,
                // `all_objects` reads only what an audit compares a copy against; the
                // lifecycle and namespace fields belong to `record_for_object`, which reads
                // them per object and is not used here.
                last_access: None,
                accesses: None,
                pinned_until: None,
                rule: None,
                names: Vec::new(),
                locations: Vec::new(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Record a replica the mover placed for an object the catalog already knows.
    ///
    /// Returns `Ok(false)` when the object is not in the catalog — nothing is inserted,
    /// because the mover is not allowed to ingest (that is `sync`'s job, and invariant 9
    /// keeps the sweep away from creating catalog state beyond noting a copy it made).
    /// `verified = false` records bytes the mover wrote but could not vouch for: the row
    /// exists so a later sync hashes them, but it counts toward no floor.
    pub fn record_replica(
        &self,
        object: &[u8],
        tier: &str,
        storage_key: &str,
        verified: bool,
        checksum: Option<&[u8]>,
    ) -> Result<bool, CatalogError> {
        let known: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM object WHERE id = ?1)",
            params![object],
            |row| row.get(0),
        )?;
        if !known {
            return Ok(false);
        }
        let now = now_seconds();
        self.conn.execute(
            "INSERT INTO location
                 (object_id, tier, storage_key, is_primary, updated_at, verified, checksum)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)
             ON CONFLICT(tier, storage_key) DO UPDATE SET
                 object_id = excluded.object_id,
                 updated_at = excluded.updated_at,
                 verified = excluded.verified,
                 checksum = excluded.checksum",
            params![
                object,
                tier,
                storage_key,
                now,
                i64::from(verified),
                checksum
            ],
        )?;
        Ok(true)
    }

    /// Record that a location was read back and matched the object it claims.
    ///
    /// Written per location, not once per run: a scrub killed at location 900 of 1000
    /// must resume at 901, and only a durable write at each stop gives that. The same
    /// statement retires any damage record for the location, because a clean read is
    /// exactly the evidence that the corruption is gone.
    pub fn record_verified(
        &self,
        tier: &str,
        storage_key: &str,
        object: &[u8],
    ) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO scrub_state (tier, storage_key, object_id, verified_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(tier, storage_key) DO UPDATE SET
                 object_id = excluded.object_id, verified_at = excluded.verified_at",
            params![tier, storage_key, object, now_seconds()],
        )?;
        self.conn.execute(
            "DELETE FROM damage WHERE tier = ?1 AND storage_key = ?2",
            params![tier, storage_key],
        )?;
        Ok(())
    }

    /// Record that a location's bytes did not match its object and no good copy existed.
    ///
    /// Nothing is deleted: the corrupt bytes may be the only surviving record of what the
    /// file held. Any previous verification for this location is dropped, so the next
    /// scrub reads it again rather than trusting a row that is no longer true.
    pub fn mark_damaged(
        &self,
        tier: &str,
        storage_key: &str,
        object: &[u8],
        detail: &str,
    ) -> Result<(), CatalogError> {
        self.conn.execute(
            "INSERT INTO damage (object_id, tier, storage_key, detected_at, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(tier, storage_key) DO UPDATE SET
                 object_id = excluded.object_id, detected_at = excluded.detected_at,
                 detail = excluded.detail",
            params![object, tier, storage_key, now_seconds(), detail],
        )?;
        self.conn.execute(
            "DELETE FROM scrub_state WHERE tier = ?1 AND storage_key = ?2",
            params![tier, storage_key],
        )?;
        Ok(())
    }

    /// How much of the catalog has been scrubbed, and how much is damaged. `audit` reads
    /// this to say "never scrubbed" for a copy instead of implying the catalog vouches
    /// for bytes nobody has read back.
    pub fn scrub_summary(&self) -> Result<ScrubSummary, CatalogError> {
        let locations: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM location", [], |row| row.get(0))?;
        let verified: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM location l
               JOIN scrub_state s ON s.tier = l.tier AND s.storage_key = l.storage_key
              WHERE s.object_id = l.object_id",
            [],
            |row| row.get(0),
        )?;
        let never_scrubbed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM location l
               LEFT JOIN scrub_state s ON s.tier = l.tier AND s.storage_key = l.storage_key
              WHERE s.object_id IS NULL OR s.object_id <> l.object_id",
            [],
            |row| row.get(0),
        )?;
        let damaged: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM damage", [], |row| row.get(0))?;
        Ok(ScrubSummary {
            locations: locations as usize,
            verified: verified as usize,
            never_scrubbed: never_scrubbed as usize,
            damaged: damaged as usize,
        })
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

    /// Record which lifecycle rule decided a path's transition (§5: every transition
    /// records the rule that fired). `false` when the path is not named — a sweep that
    /// moved a file the catalog has not ingested yet has nothing to attach the rule to,
    /// and the next `catalog sync` brings the name in.
    ///
    /// The object id is resolved in SQL (`name -> object_id`) so the rule is written
    /// against the object's identity, not the path a rename could move.
    pub fn record_lifecycle_rule(&self, path: &str, rule: &str) -> Result<bool, CatalogError> {
        let changed = self.conn.execute(
            "UPDATE lifecycle SET rule = ?1
             WHERE object_id = (SELECT object_id FROM name WHERE path = ?2)",
            params![rule, path],
        )?;
        Ok(changed > 0)
    }

    /// One object, by the namespace path it answers to. `None` when the path is not
    /// named — which is an answer: the catalog, not the filesystem, is the source of
    /// truth for "does this object exist" (§3).
    pub fn record_for_path(&self, path: &str) -> Result<Option<ObjectRecord>, CatalogError> {
        let Some(id) = self.object_for_path(path)? else {
            return Ok(None);
        };
        self.record_for_object(&id)
    }

    /// Every object whose hex id starts with `prefix`, in id order.
    ///
    /// A prefix is not a guess: several matches are several objects and every one is
    /// returned as its own record. Nothing here decides which match was meant.
    pub fn records_with_prefix(&self, prefix: &str) -> Result<Vec<ObjectRecord>, CatalogError> {
        // The prefix is validated hex by the caller, so it cannot smuggle a LIKE wildcard
        // in and turn a digest query into "everything".
        let mut stmt = self
            .conn
            .prepare("SELECT lower(hex(id)) FROM object WHERE lower(hex(id)) LIKE ?1 || '%' ORDER BY lower(hex(id))")?;
        let rows = stmt.query_map(params![prefix.to_ascii_lowercase()], |row| {
            row.get::<_, String>(0)
        })?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        let mut records = Vec::new();
        for id in ids {
            if let Some(record) = self.record_for_object(&id)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// One object by its hex id, with its names, every copy, and its lifecycle row.
    pub fn record_for_object(&self, id: &str) -> Result<Option<ObjectRecord>, CatalogError> {
        let id = id.to_ascii_lowercase();
        let base = self
            .conn
            .query_row(
                "SELECT size, state, lower(hex(checksum)) FROM object WHERE lower(hex(id)) = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((size, state, checksum)) = base else {
            return Ok(None);
        };

        // A lifecycle row is written at ingest, but a catalog hand-edited or written by an
        // older schema might lack one; absence is reported, not guessed.
        let (last_access, accesses, pinned_until, rule) = self
            .conn
            .query_row(
                "SELECT last_access, accesses, pinned_until, rule FROM lifecycle
                  WHERE lower(hex(object_id)) = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?
            .unwrap_or((None, None, None, None));

        let mut locations = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT tier, storage_key, is_primary, verified, checksum, lower(hex(object_id))
             FROM location
              WHERE lower(hex(object_id)) = ?1 ORDER BY is_primary DESC, tier, storage_key",
        )?;
        let rows = stmt.query_map(params![id], |row| {
            Ok(LocationRecord {
                tier: row.get::<_, String>(0)?,
                storage_key: row.get::<_, String>(1)?,
                is_primary: row.get::<_, i64>(2)? != 0,
                verified: row.get::<_, i64>(3)? != 0,
                checksum: row.get::<_, Option<Vec<u8>>>(4)?.map(|bytes| hex(&bytes)),
                object: row.get::<_, String>(5)?,
            })
        })?;
        for row in rows {
            locations.push(row?);
        }

        let mut names = Vec::new();
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM name WHERE lower(hex(object_id)) = ?1 ORDER BY path")?;
        let rows = stmt.query_map(params![id], |row| row.get::<_, String>(0))?;
        for row in rows {
            names.push(row?);
        }

        Ok(Some(ObjectRecord {
            id,
            size: size.max(0) as u64,
            checksum,
            state,
            last_access,
            accesses: accesses.map(|value| value.max(0) as u64),
            pinned_until,
            rule,
            names,
            locations,
        }))
    }
}

/// Create the catalog file if it is absent, or verify that what is there is trustworthy.
///
/// `create_new` is `O_CREAT|O_EXCL`: it either creates the file atomically with mode
/// 0600 or fails with `AlreadyExists` — which is also what an existing symlink produces,
/// dangling or not, so a planted link can never be opened *as* the catalog. 0600 is exact
/// under any umask because umask can only clear bits and this mode has none to clear.
fn create_or_check(path: &Path) -> Result<(), CatalogError> {
    match open_new_private(path) {
        Ok(_created) => Ok(()),
        // Something is already at the name. It must be a regular file this process owns
        // (or owns trust for): anything else is someone else's file at a name this tool
        // is about to write through.
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|source| CatalogError::Stat {
                path: path.to_path_buf(),
                source,
            })?;
            if metadata.file_type().is_symlink() {
                return Err(CatalogError::Symlink {
                    path: path.to_path_buf(),
                });
            }
            if !metadata.is_file() {
                return Err(CatalogError::NotRegular {
                    path: path.to_path_buf(),
                });
            }
            refuse_foreign_owner(path, &metadata)
        }
        Err(source) => Err(CatalogError::Create {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Create a file nobody else can read, at a name nobody else holds.
fn open_new_private(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Refuse a symlink or other non-regular file at a SQLite sibling name.
///
/// SQLite names the rollback journal, the WAL and the shared-memory file after the
/// database (`<db>-journal`, `-wal`, `-shm`). The journal exists before the first page of
/// a transaction lands, so it is a name an attacker who can watch the tree gets to plant
/// first. `SQLITE_OPEN_NOFOLLOW` protects the database file itself; these are checked
/// here so a link at a sibling is refused, leaving every target untouched.
fn refuse_siblings(path: &Path) -> Result<(), CatalogError> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let sibling = PathBuf::from(name);
        let Ok(metadata) = fs::symlink_metadata(&sibling) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            return Err(CatalogError::Symlink { path: sibling });
        }
        if !metadata.is_file() {
            return Err(CatalogError::NotRegular { path: sibling });
        }
    }
    Ok(())
}

/// Refuse to trust a catalog owned by another user inside a writable directory.
///
/// The catalog is the tool's source of truth, and a file another user owns in a tree that
/// user can write is a file they can also replace between syncs. The directory the
/// catalog lives in stands in for "the tree": for the default path it *is* the watched
/// root, and for `--catalog` it is the only directory this process had a say in.
#[cfg(unix)]
fn refuse_foreign_owner(path: &Path, metadata: &fs::Metadata) -> Result<(), CatalogError> {
    use std::os::unix::fs::MetadataExt;
    let owner = metadata.uid();
    if owner == rustix::process::geteuid().as_raw() {
        return Ok(());
    }
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let group_or_world_writable = fs::symlink_metadata(directory)
        .map(|dir| dir.mode() & 0o022 != 0)
        .unwrap_or(true);
    if group_or_world_writable {
        return Err(CatalogError::ForeignOwner {
            path: path.to_path_buf(),
            owner,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn refuse_foreign_owner(_path: &Path, _metadata: &fs::Metadata) -> Result<(), CatalogError> {
    Ok(())
}

/// A name the tree still holds *and* the catalog already records as holding `id`, other than
/// `exclude`: the evidence a resolution pass requires before it drops or repoints a name.
///
/// Both halves matter. The observation half says the object still exists with the recorded
/// digest (the walk hashed it); the catalog half says the surviving spelling is already
/// ingested, so dropping the stale one cannot lose the object — `resolve` is not allowed to
/// write a name into being on the way to concluding a rename (that is `sync`'s job).
fn surviving_name(
    observation: &Observation,
    existing: &Existing,
    id: &ObjectId,
    exclude: &str,
) -> Option<String> {
    observation
        .names
        .iter()
        .find(|(path, object)| {
            *object == id && path.as_str() != exclude && existing.names.get(*path) == Some(*object)
        })
        .map(|(path, _)| path.clone())
}

/// A location the tree still holds and the catalog already records as holding `id`, other
/// than `exclude`: evidence that a vanished location's object survives as another copy, so
/// its row can be dropped without losing the object's last record.
fn surviving_location(
    observation: &Observation,
    existing: &Existing,
    id: &ObjectId,
    exclude: &LocationKey,
) -> Option<LocationKey> {
    observation
        .locations
        .iter()
        .find(|(key, object)| {
            *object == id && *key != exclude && existing.locations.get(*key) == Some(*object)
        })
        .map(|(key, _)| key.clone())
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
    observation.roots.insert(watch_tier.clone());
    for (_, tier) in &dest_roots {
        observation.roots.insert(tier.clone());
    }
    // One hash per unique file: a symlink and the cold scan both see the same bytes, and
    // hashing a 40 GB file twice would be the tool's own worst enemy.
    let mut hashes: BTreeMap<PathBuf, ObjectId> = BTreeMap::new();
    // Absolute paths already reported unreadable. A cold copy reached by the tier walk and
    // again through the watched symlink that points at it is one broken file, not two.
    let mut unreadable_seen: BTreeSet<PathBuf> = BTreeSet::new();

    for (root, tier) in &dest_roots {
        for entry in list(root)? {
            // A symlink that appeared on a cold tier is not the mover's output and is
            // not a copy of anything.
            if entry.is_symlink {
                continue;
            }
            match hash_path(&mut hashes, &entry.path) {
                Ok(id) => {
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
                Err(error) => {
                    // Failure is per file: name the location, report it, keep walking. The
                    // read failure is not evidence the copy is gone, so the location is
                    // marked unreadable rather than left for the missing-report to claim.
                    observation
                        .unreadable_locations
                        .insert((tier.clone(), key_of(&entry.relative)));
                    record_unreadable(
                        &mut observation,
                        &mut unreadable_seen,
                        &entry.path,
                        &entry.relative,
                        error,
                    )?;
                }
            }
        }
    }

    for entry in list(&watch_root)? {
        let access = access_epoch(&entry.last_access);
        if entry.is_symlink {
            match resolve_link(&entry.path, &dest_roots)? {
                LinkState::Cold { target, key, tier } => {
                    // The bytes are on a cold tier; a failed read there leaves neither the
                    // name nor the cold location observable, so both are marked unreadable.
                    let read = hash_path(&mut hashes, &target).and_then(|id| {
                        let size = fs::metadata(&target)
                            .map(|metadata| metadata.len())
                            .map_err(|source| CatalogError::Checksum {
                                path: target.clone(),
                                source,
                            })?;
                        Ok((id, size))
                    });
                    match read {
                        Ok((id, size)) => {
                            record_object(&mut observation, &id, size, access);
                            observation
                                .names
                                .insert(key_of(&entry.relative), id.clone());
                            observation.locations.insert((tier, key), id);
                        }
                        Err(error) => {
                            observation.unreadable_names.insert(key_of(&entry.relative));
                            observation.unreadable_locations.insert((tier, key));
                            record_unreadable(
                                &mut observation,
                                &mut unreadable_seen,
                                &target,
                                &entry.relative,
                                error,
                            )?;
                        }
                    }
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
            match hash_path(&mut hashes, &entry.path) {
                Ok(id) => {
                    record_object(&mut observation, &id, entry.size, access);
                    let key = key_of(&entry.relative);
                    observation.names.insert(key.clone(), id.clone());
                    observation.locations.insert((watch_tier.clone(), key), id);
                }
                Err(error) => {
                    // A watched file that could not be read. Its absence from the
                    // observation must not read as a vanished name or a missing location —
                    // the read failed, and that is all this pass knows (issue #53).
                    let key = key_of(&entry.relative);
                    observation.unreadable_names.insert(key.clone());
                    observation
                        .unreadable_locations
                        .insert((watch_tier.clone(), key));
                    record_unreadable(
                        &mut observation,
                        &mut unreadable_seen,
                        &entry.path,
                        &entry.relative,
                        error,
                    )?;
                }
            }
        }
    }

    Ok(observation)
}

/// Record one file this pass could not read, and carry on.
///
/// Rule 7 for the catalog: one unreadable file is a hole the report names, not a reason to
/// leave the whole tree un-ingested. The report is deduplicated on the absolute path — a
/// cold copy and the watched symlink that points at it are two views of the same unreadable
/// bytes, and naming it twice would read as two broken files (issue #53).
///
/// Only a read failure is a per-file failure. Anything else is not something this pass can
/// carry on from, so it still propagates and aborts the sync.
fn record_unreadable(
    observation: &mut Observation,
    seen: &mut BTreeSet<PathBuf>,
    absolute: &Path,
    display: &Path,
    error: CatalogError,
) -> Result<(), CatalogError> {
    match error {
        CatalogError::Checksum { source, .. } => {
            if seen.insert(canonical(absolute)) {
                observation.differences.push(Difference {
                    kind: DifferenceKind::Unreadable,
                    path: display.to_path_buf(),
                    detail: source.to_string(),
                });
            }
            Ok(())
        }
        other => Err(other),
    }
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

/// Bring an older catalog file up to the current schema without touching its data.
///
/// `CREATE TABLE IF NOT EXISTS` covers new *tables* (the `tier` table), but a column
/// added to an existing table is invisible to an existing file. A catalog written before
/// replication existed has no `verified`/`checksum` on `location`, so this adds them with
/// defaults that mean "unknown" — never "good": an unverified pre-existing copy must be
/// hashed by a later sync before it counts toward a floor. Backward compatible in the
/// direction that matters: a new binary opens an old file, and the old rows keep saying
/// exactly what they said.
fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    let has_verified: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('location') WHERE name = 'verified'",
        [],
        |row| row.get(0),
    )?;
    if has_verified == 0 {
        conn.execute_batch(
            "ALTER TABLE location ADD COLUMN verified INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE location ADD COLUMN checksum BLOB;",
        )?;
    }
    Ok(())
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
        for table in [
            "object",
            "location",
            "name",
            "lifecycle",
            "volume",
            "scrub_state",
            "damage",
            "root",
        ] {
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

    #[test]
    fn a_sync_verified_every_copy_it_ingested() {
        // The identity is the digest, so a location observed by the walk has been hashed
        // and can be vouched for. A row that is not verified would be one nothing read.
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(cold.join("shows")).unwrap();
        fs::create_dir_all(watch.join("shows")).unwrap();
        fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
        std::os::unix::fs::symlink("../../cold/shows/moved.mkv", watch.join("shows/moved.mkv"))
            .unwrap();

        let mut catalog = catalog_in(tmp.path());
        catalog.sync(&watch, std::slice::from_ref(&cold)).unwrap();

        for location in catalog.all_locations().unwrap() {
            assert!(
                location.verified,
                "a hashed location must be verified: {location:?}"
            );
            assert_eq!(
                location.checksum.as_deref(),
                Some(location.object.as_str()),
                "the stored checksum is the object identity"
            );
        }
    }

    #[test]
    fn an_unverified_location_is_reported_as_unknown_not_counted() {
        // A mover that wrote bytes it could not vouch for leaves `verified = 0`. If the
        // file is then gone, a sync must say so — it must never quietly promote the row
        // to a good copy just because it has a row.
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&cold).unwrap();
        fs::create_dir_all(&watch).unwrap();
        fs::write(cold.join("ghost.bin"), b"maybe").unwrap();

        let mut catalog = catalog_in(tmp.path());
        catalog.sync(&watch, std::slice::from_ref(&cold)).unwrap();
        // Break the vouch by hand: the row now says "written, never verified".
        catalog
            .conn
            .execute("UPDATE location SET verified = 0, checksum = NULL", [])
            .unwrap();
        // The bytes vanish too.
        fs::remove_file(cold.join("ghost.bin")).unwrap();

        let report = catalog.sync(&watch, std::slice::from_ref(&cold)).unwrap();
        assert!(
            report
                .differences
                .iter()
                .any(|difference| difference.kind == DifferenceKind::ReplicaUnknown),
            "an unknown copy must be reported: {:?}",
            report.differences
        );
    }

    #[test]
    fn a_floor_is_recorded_once_per_tier_and_under_replication_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold_a = tmp.path().join("cold-a");
        let cold_b = tmp.path().join("cold-b");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold_a).unwrap();
        fs::create_dir_all(&cold_b).unwrap();
        fs::write(cold_a.join("movie.bin"), b"movie bytes").unwrap();

        let mut catalog = catalog_in(tmp.path());
        catalog
            .set_tier_floor(&key_of(&canonical(&cold_a)), 2)
            .unwrap();
        catalog
            .set_tier_floor(&key_of(&canonical(&cold_b)), 2)
            .unwrap();
        assert_eq!(
            catalog.tier_floor(&key_of(&canonical(&cold_a))).unwrap(),
            Some(2)
        );
        assert_eq!(catalog.tier_floor("/nowhere").unwrap(), None);

        // The copy is a cold location with a name pointing at it, but only one of the two
        // disks holds it — below the recorded floor of 2.
        std::os::unix::fs::symlink("../cold-a/movie.bin", watch.join("movie.bin")).unwrap();
        let report = catalog
            .sync(&watch, &[cold_a.clone(), cold_b.clone()])
            .unwrap();
        let finding = report
            .differences
            .iter()
            .find(|difference| difference.kind == DifferenceKind::UnderReplicated)
            .unwrap_or_else(|| panic!("expected under-replicated: {:?}", report.differences));
        assert!(
            finding.detail.contains(&key_of(&canonical(&cold_b))),
            "the missing disk must be named: {}",
            finding.detail
        );

        // Add the second copy: the floor is met and the finding disappears.
        fs::write(cold_b.join("movie.bin"), b"movie bytes").unwrap();
        let healed = catalog
            .sync(&watch, &[cold_a.clone(), cold_b.clone()])
            .unwrap();
        assert!(
            !healed
                .differences
                .iter()
                .any(|difference| difference.kind == DifferenceKind::UnderReplicated),
            "two copies meet the floor: {:?}",
            healed.differences
        );
    }

    #[test]
    fn a_replica_is_recorded_only_for_an_object_the_catalog_knows() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = catalog_in(tmp.path());
        assert!(
            !catalog
                .record_replica(&[9u8; 32], "/cold", "a.bin", true, Some(&[9u8; 32]))
                .unwrap(),
            "an unknown object must not be ingested by a replica record"
        );
        assert_eq!(catalog.location_count().unwrap(), 0);
    }

    #[test]
    fn an_old_catalog_file_without_verification_columns_still_opens() {
        // A catalog written before replication has a `location` table with no `verified`
        // column. Opening it must migrate the columns in with a default of "unknown", not
        // fail and not claim the old rows were verified.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE object (
                     id BLOB PRIMARY KEY, size INTEGER NOT NULL, checksum BLOB NOT NULL,
                     created_at INTEGER NOT NULL, state TEXT NOT NULL);
                 CREATE TABLE location (
                     object_id BLOB NOT NULL, tier TEXT NOT NULL, storage_key TEXT NOT NULL,
                     is_primary INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                     PRIMARY KEY (tier, storage_key));
                 INSERT INTO object VALUES (x'0102', 3, x'0102', 0, 'offloaded');
                 INSERT INTO location VALUES (x'0102', '/cold', 'a.bin', 1, 0);",
            )
            .unwrap();
        }

        let catalog = Catalog::open(&path).expect("an old catalog must still open");
        let locations = catalog.all_locations().unwrap();
        assert_eq!(locations.len(), 1);
        assert!(
            !locations[0].verified,
            "a pre-existing row is unknown until a sync hashes it"
        );
        assert_eq!(locations[0].checksum, None);
    }

    /// A digest prefix that matches more than one object returns all of them — a prefix is
    /// not a promise that the answer is unique, and guessing would be the one wrong move.
    /// Two synthetic objects are written directly because forcing a real 8-hex collision
    /// between BLAKE3 hashes needs an impossible amount of data.
    #[test]
    fn a_digest_prefix_returns_every_object_it_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = catalog_in(tmp.path());
        let now = now_seconds();
        for suffix in ["aa", "bb"] {
            let mut id = vec![0u8; 32];
            id[0] = 0x42;
            id[1] = 0x17;
            id[2] = u8::from_str_radix(suffix, 16).unwrap();
            catalog
                .conn
                .execute(
                    "INSERT INTO object (id, size, checksum, created_at, state)
                     VALUES (?1, 1, ?1, ?2, 'present')",
                    params![id, now],
                )
                .unwrap();
            catalog
                .conn
                .execute(
                    "INSERT INTO lifecycle (object_id, last_access, accesses, pinned_until, rule)
                     VALUES (?1, ?2, 0, NULL, NULL)",
                    params![id, now],
                )
                .unwrap();
        }

        // Six hex digits is not yet a digest query by the CLI's rule, so the method is
        // exercised directly with a prefix both objects share.
        let hits = catalog.records_with_prefix("4217").unwrap();
        assert_eq!(hits.len(), 2, "both objects must be listed");
        assert!(hits[0].id.starts_with("4217"));
        assert!(hits[1].id.starts_with("4217"));
        // A prefix that matches nothing is an empty list, not an error.
        assert!(catalog.records_with_prefix("deadbeef").unwrap().is_empty());
    }

    #[test]
    fn scrub_state_round_trips_and_a_fresh_catalog_reports_never_scrubbed() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(watch.join("a.bin"), b"payload").unwrap();

        let mut catalog = catalog_in(tmp.path());
        catalog.sync(&watch, std::slice::from_ref(&cold)).unwrap();

        // Before any scrub, every location is "never scrubbed" — the catalog says where
        // bytes are, not that anyone has read them back.
        let fresh = catalog.scrub_summary().unwrap();
        assert_eq!(fresh.locations, 1);
        assert_eq!(fresh.verified, 0);
        assert_eq!(fresh.never_scrubbed, 1);
        assert_eq!(fresh.damaged, 0);

        let target = catalog.scrub_targets().unwrap().pop().unwrap();
        assert!(!target.is_already_verified());
        let roots = catalog.roots().unwrap();
        assert_eq!(target.path(&roots).unwrap(), watch.join("a.bin"));

        catalog
            .record_verified(&target.tier, &target.storage_key, &target.object)
            .unwrap();
        let after = catalog.scrub_summary().unwrap();
        assert_eq!(after.verified, 1);
        assert_eq!(after.never_scrubbed, 0);
        // The re-read agrees, so the second scrub would skip this location.
        let target = catalog
            .scrub_targets()
            .unwrap()
            .into_iter()
            .find(|t| t.storage_key == "a.bin")
            .unwrap();
        assert!(target.is_already_verified());

        // Damage is recorded against the object, and drops the stale verification so the
        // location is read again rather than trusted.
        catalog
            .mark_damaged(
                &target.tier,
                &target.storage_key,
                &target.object,
                "test rot",
            )
            .unwrap();
        let damaged = catalog.scrub_summary().unwrap();
        assert_eq!(damaged.damaged, 1);
        assert_eq!(damaged.verified, 0);
        assert_eq!(damaged.never_scrubbed, 1);
    }

    #[test]
    fn a_recorded_row_only_becomes_a_path_under_a_trusted_root() {
        let roots = vec![PathBuf::from("/hot"), PathBuf::from("/cold")];

        // The legitimate row resolves exactly as a join would.
        assert_eq!(
            resolve_location_path("/cold", "shows/ep1.mkv", &roots).unwrap(),
            PathBuf::from("/cold/shows/ep1.mkv")
        );

        // An absolute key would have replaced the tier entirely.
        assert_eq!(
            resolve_location_path("/cold", "/etc/passwd", &roots),
            Err(RowPathError::AbsoluteKey)
        );
        // A `..` component walks out of the tier lexically.
        assert_eq!(
            resolve_location_path("/cold", "../../etc/passwd", &roots),
            Err(RowPathError::ParentDirKey)
        );
        // A tier no current root explains is refused even with a harmless key.
        assert_eq!(
            resolve_location_path("/etc", "passwd", &roots),
            Err(RowPathError::UnknownTier)
        );
        assert_eq!(
            resolve_location_path("relative/dir", "x.bin", &roots),
            Err(RowPathError::UnknownTier)
        );

        // Without any roots independently recorded from a sync, the catalog fails closed:
        // even an absolute tier cannot be distinguished from an edited row.
        assert_eq!(
            resolve_location_path("/cold", "../../etc/passwd", &[]),
            Err(RowPathError::UnknownTier)
        );
        assert_eq!(
            resolve_location_path("relative/dir", "x.bin", &[]),
            Err(RowPathError::UnknownTier)
        );
        assert_eq!(
            resolve_location_path("/cold", "shows/ep1.mkv", &[]),
            Err(RowPathError::UnknownTier)
        );
    }

    #[test]
    fn a_sync_records_the_roots_it_was_run_with() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold_a = tmp.path().join("cold-a");
        let cold_b = tmp.path().join("cold-b");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold_a).unwrap();
        fs::create_dir_all(&cold_b).unwrap();
        fs::write(watch.join("a.bin"), b"payload").unwrap();

        let mut catalog = catalog_in(tmp.path());
        catalog
            .sync(&watch, &[cold_a.clone(), cold_b.clone()])
            .unwrap();

        let roots = catalog.roots().unwrap();
        assert!(roots.contains(&canonical(&watch)), "roots: {roots:?}");
        assert!(roots.contains(&canonical(&cold_a)), "roots: {roots:?}");
        assert!(roots.contains(&canonical(&cold_b)), "roots: {roots:?}");
    }

    #[test]
    fn a_sync_reports_an_unreadable_file_instead_of_failing() {
        // The API half of #53: `sync` returns a report rather than an `Err` when a file
        // cannot be read, and the report carries the path and the error. The end-to-end
        // tests turn the report into the exit-code contract; this pins the shape.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(watch.join("readable.bin"), b"read me").unwrap();
        fs::write(watch.join("locked.bin"), b"cannot read me").unwrap();
        fs::set_permissions(watch.join("locked.bin"), fs::Permissions::from_mode(0o000)).unwrap();

        let mut catalog = catalog_in(tmp.path());
        let report = catalog
            .sync(&watch, std::slice::from_ref(&cold))
            .expect("an unreadable file must not fail the sync");
        let unreadable: Vec<&Difference> = report
            .differences
            .iter()
            .filter(|difference| difference.kind == DifferenceKind::Unreadable)
            .collect();
        assert_eq!(unreadable.len(), 1, "{:?}", report.differences);
        assert_eq!(unreadable[0].path, PathBuf::from("locked.bin"));
        assert!(
            !unreadable[0].detail.is_empty(),
            "the report must carry the error"
        );

        // The rest of the tree is in; nothing was inferred about the file that failed.
        assert!(catalog.object_for_path("readable.bin").unwrap().is_some());
        assert!(catalog.object_for_path("locked.bin").unwrap().is_none());
    }
}

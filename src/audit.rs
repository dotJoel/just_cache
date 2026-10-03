//! Audit: name every structural inconsistency in a tree, read-only unless `--repair`.
//!
//! Three modes. Two are chosen by whether a catalog exists (`--catalog <FILE>`, or the
//! default `.just_cache-catalog.sqlite` beside the watch root); the third, catalog-only, is
//! asked for outright (`--no-filesystem`):
//!
//! * **Catalog-only mode** (issue #51). The watched tree and every tier are not touched —
//!   no walk, no stat, no hash — so the answer is whatever the recorded rows say and
//!   nothing else. It reports the recorded state (object/name/location counts), a floor an
//!   object's recorded locations fall below, damage marks a scrub left, malformed rows and
//!   the scrub-state summary, and it says out loud which findings it cannot make without a
//!   tree (`missing-copy`, `unknown-path`, a checksum of record, and the rest of
//!   [`VerdictKind::filesystem_only`]). This is the mode for a catalog whose tier is not
//!   mounted: it answers "what does the catalog say" without pretending to have looked.
//!
//! * **Catalog mode** (issue #19). The catalog is the arbiter, so the audit reads it and
//!   makes a *single* filesystem pass over the primary copies instead of walking both
//!   sides and reasoning about both. It answers the §3 questions directly — a name whose
//!   primary location is gone, a copy that fails its recorded checksum, an object below
//!   its tier's copy floor, a symlink pointing at nothing or at a version the catalog does
//!   not have — plus the one thing only a walk can see: a path the catalog does not know
//!   about, which is reported and never silently adopted.
//! * **Walk mode** (the P0 audit, unchanged). No catalog file: the walk-based audit is the
//!   bootstrap and the fallback. It compares the same relative path on both sides:
//!
//!   | source path | cold copy | meaning |
//!   |---|---|---|
//!   | regular file | absent | healthy: not migrated, or not cold yet |
//!   | regular file | present | **duplicate**: the source removal never happened |
//!   | symlink, resolves under a cold tier | present | healthy: the migrated state |
//!   | symlink, resolves outside every cold tier | any | **unexpected-target** |
//!   | symlink that does not resolve | any | **dangling-symlink** |
//!   | absent | present | **orphaned-copy** |
//!
//! Both modes are read-only; classification is a pure function of what the sides look like,
//! so it can be unit-tested without touching a filesystem. Repair is the only mutating
//! path and is deliberately conservative. In walk mode it never removes bytes whose content
//! it has not hashed and matched against the copy that stays behind. In catalog mode it
//! does not touch the filesystem at all: a disagreement between the catalog and the tree is
//! a thing a human has to interpret (the catalog may record a move the walk sees as
//! unfinished), so `repair` *marks* the finding for resync rather than rewriting rows.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::catalog::{resolve_location_path, Catalog, CatalogError};
use crate::digest;
use crate::disk_management::{self, DiskError};
use crate::tiers::TierSet;

/// How many examples the readable summary lists by default.
pub const DEFAULT_EXAMPLES: usize = 10;

/// The copy floor an audit enforces for an object when the operator names none.
///
/// This is a per-*object* floor and it is not the schema's only one. `docs/design.md` §6
/// asks for a per-tier floor, and the catalog records it: the `tier` table's `copies` column
/// (written by `catalog sync` from `tiers.toml`, or by `--copies N`) is the floor a sweep
/// enforces when it decides whether a tier is under-replicated. An audit answers a different
/// question — how many locations still *exist* for one object — so its floor is per object and
/// normally comes from `--copies`; this constant is the default. **Every object must keep at
/// least one location that still exists**: an object whose every recorded copy is gone is
/// below the floor and is reported as such — the "missing copy is a repair job, reported, not
/// silent" case, made visible even when each individual copy was already reported missing.
pub const COPY_FLOOR: usize = 1;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("cannot walk {path}: {source}")]
    Walk {
        path: PathBuf,
        #[source]
        source: DiskError,
    },
    #[error("cannot read symlink {path}: {source}")]
    ReadLink {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot checksum {path}: {source}")]
    Checksum {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot repair {path}: {source}")]
    Repair {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("--dest {path} is not an existing directory")]
    MissingDest { path: PathBuf },
    #[error("catalog query failed: {0}")]
    Catalog(#[from] CatalogError),
}

/// What one side of a relative path contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideKind {
    File,
    Symlink,
}

/// The watched side of one path, resolved far enough to classify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    /// Nothing at the source path.
    Missing,
    /// A regular file (or something else that is not a symlink).
    File,
    /// A symlink. `target` is the raw link text; `resolves` says whether it currently
    /// points at something that exists; `dest_index` is the configured cold tier that
    /// resolved target sits under, if any.
    Symlink {
        target: PathBuf,
        resolves: bool,
        dest_index: Option<usize>,
    },
}

/// The classifications this audit can produce.
///
/// The first group is what the walk-based audit (walk mode) emits. The second is what the
/// catalog-backed audit (catalog mode) emits; several of them reuse the walk vocabulary
/// where the meaning is identical, and the rest are the answers only a catalog can give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictKind {
    Healthy,
    OrphanedCopy,
    DanglingSymlink,
    UnexpectedTarget,
    Duplicate,
    /// An offloaded object whose copies on disk number fewer than the floor. `missing`
    /// names the configured destinations a copy is absent from — the disks it is not on.
    ReplicaLost,
    /// Catalog mode: a name the catalog recorded is not in the tree (moved or deleted by
    /// hand, or a mover that never finished). The row is kept; the disagreement is the
    /// finding.
    NameVanished,
    /// Catalog mode: a location the catalog recorded has no file. `primary` in the verdict
    /// says whether it was the copy of record.
    MissingCopy,
    /// Catalog mode: a copy exists but its bytes do not match the recorded checksum.
    ChecksumMismatch,
    /// Catalog mode: an object has fewer than [`COPY_FLOOR`] surviving copies.
    CopyFloor,
    /// Catalog-only mode: an object's *recorded* locations number fewer than the floor
    /// recorded for its tier. Nothing was stat'ed, so this is a claim about the rows, not
    /// about bytes: a copy may have been deleted under the catalog and the row still
    /// counts here — that is exactly what the mode cannot see.
    UnderReplicated,
    /// Catalog-only mode: a location a scrub marked damaged (no good copy existed to repair
    /// from). The mark is recorded state; whether the bytes are still there is not checked.
    DamagedCopy,
    /// Catalog mode: a symlink resolves to a file the catalog has no location for — a
    /// version the catalog does not have.
    UnknownVersion,
    /// Catalog mode: a path exists in the tree that the catalog does not know about. It is
    /// reported and never adopted; ingesting it is `catalog sync`'s job, not an audit's.
    UnknownPath,
    /// Catalog mode: a recorded location was refused before it became a path — its tier is
    /// not one of the current roots, or its key would escape one. Reported, not touched
    /// (issue #72).
    MalformedCatalog,
}

impl VerdictKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VerdictKind::Healthy => "healthy",
            VerdictKind::OrphanedCopy => "orphaned-copy",
            VerdictKind::DanglingSymlink => "dangling-symlink",
            VerdictKind::UnexpectedTarget => "unexpected-target",
            VerdictKind::Duplicate => "duplicate",
            VerdictKind::ReplicaLost => "replica-lost",
            VerdictKind::NameVanished => "name-vanished",
            VerdictKind::MissingCopy => "missing-copy",
            VerdictKind::ChecksumMismatch => "checksum-mismatch",
            VerdictKind::CopyFloor => "copy-floor",
            VerdictKind::UnderReplicated => "under-replicated",
            VerdictKind::DamagedCopy => "damaged-copy",
            VerdictKind::UnknownVersion => "unknown-version",
            VerdictKind::UnknownPath => "unknown-path",
            VerdictKind::MalformedCatalog => "malformed-catalog",
        }
    }

    /// The kinds that make `audit` exit non-zero, in report order.
    ///
    /// Every problem kind is listed, in both modes, so a cron job's counts do not change
    /// shape when the catalog appears — the catalog-only kinds simply read `0` in walk mode.
    pub fn problems() -> [VerdictKind; 14] {
        [
            VerdictKind::MissingCopy,
            VerdictKind::ChecksumMismatch,
            VerdictKind::CopyFloor,
            VerdictKind::UnderReplicated,
            VerdictKind::DamagedCopy,
            VerdictKind::NameVanished,
            VerdictKind::UnknownVersion,
            VerdictKind::UnknownPath,
            VerdictKind::MalformedCatalog,
            VerdictKind::OrphanedCopy,
            VerdictKind::DanglingSymlink,
            VerdictKind::UnexpectedTarget,
            VerdictKind::Duplicate,
            VerdictKind::ReplicaLost,
        ]
    }

    /// Every kind, so a test can prove the partition below is exhaustive rather than
    /// trusting a hand-maintained list.
    pub const ALL: [VerdictKind; 15] = [
        VerdictKind::Healthy,
        VerdictKind::OrphanedCopy,
        VerdictKind::DanglingSymlink,
        VerdictKind::UnexpectedTarget,
        VerdictKind::Duplicate,
        VerdictKind::ReplicaLost,
        VerdictKind::NameVanished,
        VerdictKind::MissingCopy,
        VerdictKind::ChecksumMismatch,
        VerdictKind::CopyFloor,
        VerdictKind::UnderReplicated,
        VerdictKind::DamagedCopy,
        VerdictKind::UnknownVersion,
        VerdictKind::UnknownPath,
        VerdictKind::MalformedCatalog,
    ];

    /// The kinds a catalog-only audit (`--no-filesystem`) cannot make, because every one of
    /// them needs the tree: a location row may lie about a file that was deleted, only a
    /// `stat` sees that; a checksum needs the bytes read; a name/path disagreement needs
    /// the namespace walked. The complement — [`Healthy`](VerdictKind::Healthy),
    /// `under-replicated`, `damaged-copy` and `malformed-catalog` — is everything the mode
    /// can honestly report. Kept as a function of the enum (with [`ALL`](VerdictKind::ALL)
    /// to check the partition) so the "cannot see the filesystem" set is exactly this list,
    /// not prose that can drift from the code.
    pub fn filesystem_only() -> [VerdictKind; 11] {
        [
            VerdictKind::MissingCopy,
            VerdictKind::ChecksumMismatch,
            VerdictKind::CopyFloor,
            VerdictKind::NameVanished,
            VerdictKind::UnknownVersion,
            VerdictKind::UnknownPath,
            VerdictKind::OrphanedCopy,
            VerdictKind::DanglingSymlink,
            VerdictKind::UnexpectedTarget,
            VerdictKind::Duplicate,
            VerdictKind::ReplicaLost,
        ]
    }
}

/// One path's verdict. The extra data is what repair needs to act, and what a human
/// needs to understand the finding without opening a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Healthy,
    /// Cold bytes exist but the name at the source path is gone.
    OrphanedCopy,
    /// The symlink at the source path does not point at anything that exists.
    DanglingSymlink {
        target: PathBuf,
    },
    /// The symlink resolves, but outside every configured `--dest`.
    UnexpectedTarget {
        target: PathBuf,
    },
    /// A real file at the source path *and* a copy on the cold tier: the source was
    /// never removed after the copy landed (or a restore is in progress).
    Duplicate,
    /// An offloaded object whose copies on disk number fewer than the floor. `missing`
    /// names the configured destinations a copy is absent from — the disks it is not on.
    ReplicaLost {
        present: Vec<PathBuf>,
        missing: Vec<PathBuf>,
    },
    /// Catalog mode: the catalog records a name here, but the tree has nothing at the
    /// path. The object is named so the row can be found without guessing.
    NameVanished {
        object: String,
    },
    /// Catalog mode: a recorded location has no file. `tier` and `key` locate it; `primary`
    /// distinguishes the copy of record from a replica.
    MissingCopy {
        tier: String,
        key: String,
        primary: bool,
    },
    /// Catalog mode: a copy's bytes differ from the recorded checksum.
    ChecksumMismatch {
        tier: String,
        key: String,
        expected: String,
        found: String,
    },
    /// Catalog mode: an object has fewer surviving copies than [`COPY_FLOOR`].
    CopyFloor {
        object: String,
        copies: usize,
    },
    /// Catalog-only mode: an object's recorded locations number fewer than the floor
    /// recorded for its primary tier. `copies` counts rows, not bytes.
    UnderReplicated {
        object: String,
        copies: usize,
        floor: usize,
    },
    /// Catalog-only mode: a location a scrub marked damaged.
    DamagedCopy {
        tier: String,
        key: String,
        object: String,
    },
    /// Catalog mode: a symlink resolves to something the catalog does not hold as a
    /// location of the object the name points at.
    UnknownVersion {
        target: PathBuf,
    },
    /// Catalog mode: the catalog has no row for this path at all.
    UnknownPath,
    /// Catalog mode: a recorded location is refused as a path — its tier is not one of the
    /// current roots, or its key is absolute or walks out with `..`. Reported, and no stat
    /// or hash ran on it (issue #72).
    MalformedCatalog {
        tier: String,
        key: String,
        detail: String,
    },
}

impl Verdict {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Verdict::Healthy)
    }

    pub fn kind(&self) -> VerdictKind {
        match self {
            Verdict::Healthy => VerdictKind::Healthy,
            Verdict::OrphanedCopy => VerdictKind::OrphanedCopy,
            Verdict::DanglingSymlink { .. } => VerdictKind::DanglingSymlink,
            Verdict::UnexpectedTarget { .. } => VerdictKind::UnexpectedTarget,
            Verdict::Duplicate => VerdictKind::Duplicate,
            Verdict::ReplicaLost { .. } => VerdictKind::ReplicaLost,
            Verdict::NameVanished { .. } => VerdictKind::NameVanished,
            Verdict::MissingCopy { .. } => VerdictKind::MissingCopy,
            Verdict::ChecksumMismatch { .. } => VerdictKind::ChecksumMismatch,
            Verdict::CopyFloor { .. } => VerdictKind::CopyFloor,
            Verdict::UnderReplicated { .. } => VerdictKind::UnderReplicated,
            Verdict::DamagedCopy { .. } => VerdictKind::DamagedCopy,
            Verdict::UnknownVersion { .. } => VerdictKind::UnknownVersion,
            Verdict::UnknownPath => VerdictKind::UnknownPath,
            Verdict::MalformedCatalog { .. } => VerdictKind::MalformedCatalog,
        }
    }
}

/// One non-healthy path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The path in the watched tree (whether or not it still exists).
    pub path: PathBuf,
    pub relative: PathBuf,
    /// The cold copy behind this path, when one is known — the object of a repair.
    pub cold_copy: Option<PathBuf>,
    pub verdict: Verdict,
}

impl Finding {
    /// One line, with the cold copy when it is relevant, and the extra detail the
    /// catalog-backed verdicts carry (a tier/key, an expected digest) so a human can act
    /// without opening a shell.
    pub fn describe(&self) -> String {
        self.describe_with(None)
    }

    /// As [`describe`](Finding::describe), but a configured tier is printed by name
    /// instead of by path — the point of `tiers.toml`, applied to the findings that name a
    /// tier. With no config every tier string is exactly what it was before.
    pub fn describe_with(&self, tiers: Option<&TierSet>) -> String {
        let tier_name = |tier: &str| match tiers {
            Some(set) => set.name_for_tier_key(tier),
            None => tier.to_string(),
        };
        let root_name = |path: &Path| match tiers {
            Some(set) => set.name_for_root(path),
            None => path.display().to_string(),
        };
        let mut line = format!("{}: {}", self.verdict.kind().as_str(), self.path.display());
        match &self.verdict {
            Verdict::DanglingSymlink { target }
            | Verdict::UnexpectedTarget { target }
            | Verdict::UnknownVersion { target } => {
                line.push_str(&format!(" (points at {})", target.display()));
            }
            Verdict::NameVanished { object } => {
                line.push_str(&format!(" (catalog records object {object} here)"));
            }
            Verdict::MissingCopy { tier, key, primary } => {
                line.push_str(&format!(
                    " ({} copy {key} on {} is gone)",
                    if *primary { "primary" } else { "replica" },
                    tier_name(tier)
                ));
            }
            Verdict::ChecksumMismatch {
                tier,
                key,
                expected,
                found,
            } => {
                line.push_str(&format!(
                    " (recorded {expected}, found {found} at {}/{key})",
                    tier_name(tier)
                ));
            }
            Verdict::CopyFloor { object, copies } => {
                line.push_str(&format!(
                    " (object {object} has {copies} surviving copy/copies, floor is {COPY_FLOOR})"
                ));
            }
            Verdict::UnderReplicated {
                object,
                copies,
                floor,
            } => {
                line.push_str(&format!(
                    " (object {object} records {copies} location(s), the recorded floor is \
                     {floor}; no copy was checked, so only the rows are known)"
                ));
            }
            Verdict::DamagedCopy { tier, key, object } => {
                line.push_str(&format!(
                    " (location {} on {tier} is marked damaged for object {object}; the mark is \
                     recorded state, the bytes were not read)",
                    key
                ));
            }
            Verdict::ReplicaLost { present, missing } => {
                let list = |paths: &[PathBuf]| {
                    paths
                        .iter()
                        .map(|path| root_name(path))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                line.push_str(&format!(
                    " (present on [{}]; missing on [{}])",
                    list(present),
                    list(missing)
                ));
            }
            Verdict::UnknownPath => {
                line.push_str(" (the catalog does not know this path)");
            }
            Verdict::MalformedCatalog { tier, key, detail } => {
                line.push_str(&format!(
                    " (catalog row {}/{key}: {detail}; no filesystem operation)",
                    tier_name(tier)
                ));
            }
            Verdict::Healthy => {}
            // The walk-mode verdicts that carry a cold copy: keep the hint.
            Verdict::Duplicate | Verdict::OrphanedCopy => {
                if let Some(copy) = &self.cold_copy {
                    line.push_str(&format!(" (cold copy {})", copy.display()));
                }
            }
        }
        line
    }
}

/// Where an audit's answers came from. Carried in the report so the summary and the JSON
/// say it out loud: two ways to know the truth is exactly the confusion this issue exists
/// to remove, so a reader must never have to guess which one answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditSource {
    /// No catalog file: the audit walked both sides and compared them.
    Walk,
    /// Answered from the catalog at this path, plus one pass over the watched tree.
    Catalog { path: PathBuf },
    /// Answered from the catalog rows alone (issue #51): the watched tree and every tier
    /// were not walked, stat'ed, or hashed. The report names the findings it could not make
    /// in [`CatalogOnlySection::unchecked`].
    CatalogOnly { path: PathBuf },
}

impl AuditSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditSource::Walk => "walk",
            AuditSource::Catalog { .. } => "catalog",
            AuditSource::CatalogOnly { .. } => "catalog-only",
        }
    }

    /// The catalog file behind the answer, when a catalog answered.
    pub fn catalog_path(&self) -> Option<&Path> {
        match self {
            AuditSource::Walk => None,
            AuditSource::Catalog { path } | AuditSource::CatalogOnly { path } => Some(path),
        }
    }
}

/// What a catalog-only audit can report but cannot check: the recorded counts, the tier
/// floors, the damage marks a scrub left, and the scrub-state summary — plus the explicit
/// list of findings it could not make, so a reader never mistakes a `0` in the counts block
/// for "checked and clean".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogOnlySection {
    /// Objects the catalog records.
    pub objects: usize,
    /// Namespace paths the catalog records.
    pub names: usize,
    /// Location rows the catalog records.
    pub locations: usize,
    /// The recorded per-tier copy floor, `(tier, copies)`, in tier order.
    pub floors: Vec<(String, usize)>,
    /// Locations a scrub marked damaged.
    pub damaged: usize,
    /// The same scrub-state summary (`verified`/`never-scrubbed`/`damaged`) `audit` prints
    /// for a catalog-mode run.
    pub scrub: crate::catalog::ScrubSummary,
    /// The finding kinds this audit could not make, because each needs the tree.
    pub unchecked: Vec<VerdictKind>,
}

impl CatalogOnlySection {
    /// The recorded state a catalog-only audit could report, one line per fact. The report
    /// adds the findings it could not make after these; deliberately this says what was
    /// *not* checked, because the whole point of this mode is that a reader must not read
    /// its silence as health.
    pub fn summary_lines(&self) -> Vec<String> {
        let floors = if self.floors.is_empty() {
            "none recorded".to_string()
        } else {
            self.floors
                .iter()
                .map(|(tier, copies)| format!("{tier}={copies}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut lines = vec![
            format!(
                "  recorded state: {} object(s), {} name(s), {} location(s)",
                self.objects, self.names, self.locations
            ),
            format!("  recorded floors: {floors}"),
            format!("  damage marks: {}", self.damaged),
        ];
        lines.extend(self.scrub.summary_lines());
        lines
    }
}

/// The scrub-state counts `audit` reports alongside its structural findings.
///
/// A copy the catalog records but no scrub has read back is not *proven*: the whole point
/// of §6's scrub is that "the catalog has it" is not the same as "the bytes are good". The
/// readable output already says so, and this carries the same numbers into `--json` so a
/// machine reading the document gets the same disclaimer a human reads on the terminal.
///
/// Only a catalog-backed audit has anything to count. The field is `Option` for exactly
/// that reason: without a catalog the JSON is `null`, never a zeroed object. Four zeroes
/// read as "nothing never-scrubbed, nothing damaged" — "everything verified" — which is the
/// one thing a walk-based audit did not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrubSection {
    /// Every location the catalog records.
    pub locations: usize,
    /// Locations whose last recorded verification matches the object they hold.
    pub verified: usize,
    /// Locations with no verification, or one that no longer matches.
    pub never_scrubbed: usize,
    /// Locations a scrub marked damaged with no verified copy to repair from.
    pub damaged: usize,
}

impl ScrubSection {
    /// The readable line. Deliberately the same wording `catalog::ScrubSummary` prints, and
    /// the one place both renderings read it from, so the human line and the JSON object
    /// cannot drift apart — the test that compares them would be the only thing keeping
    /// two copies of this sentence in step, and a test is not a guarantee at run time.
    pub fn summary_lines(&self) -> Vec<String> {
        vec![format!(
            "  scrub: {} location(s): {} verified, {} never scrubbed, {} damaged",
            self.locations, self.verified, self.never_scrubbed, self.damaged
        )]
    }

    fn json(&self) -> String {
        format!(
            "{{\"locations\":{},\"verified\":{},\"never-scrubbed\":{},\"damaged\":{}}}",
            self.locations, self.verified, self.never_scrubbed, self.damaged
        )
    }
}

/// The whole result of an audit run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub watch: PathBuf,
    pub dests: Vec<PathBuf>,
    /// Which source answered (catalog or a walk), and the catalog path when there is one.
    pub source: AuditSource,
    /// Paths that exist on at least one side.
    pub scanned: usize,
    pub healthy: usize,
    /// Only the non-healthy paths, sorted by path then kind.
    pub findings: Vec<Finding>,
    /// The configured tiers (`tiers.toml`), when the invocation supplied one. It names the
    /// tiers in the output and lets a finding print a tier's name instead of its path.
    /// `None` is exactly the pre-tiers behaviour: a tier's path is its own name.
    pub tiers: Option<TierSet>,
    /// Scrub-state counts from the catalog that answered, or `None` for a walk-based audit
    /// (and for a catalog-mode run whose summary could not be read). See [`ScrubSection`].
    pub scrub: Option<ScrubSection>,
    /// Present only for a catalog-only audit (`--no-filesystem`): the recorded state it
    /// could report and the findings it could not make. `None` for every filesystem-backed
    /// mode, which checked the tree itself.
    pub catalog_only: Option<CatalogOnlySection>,
}

impl AuditReport {
    pub fn has_findings(&self) -> bool {
        !self.findings.is_empty()
    }

    /// Attach the configured tiers after the report is built. `None` leaves the report
    /// exactly as it was before tiers existed — a tier's path is its own name.
    pub fn with_tiers(mut self, tiers: Option<TierSet>) -> Self {
        self.tiers = tiers;
        self
    }

    /// Attach the catalog's scrub-state counts. `None` leaves the JSON's `scrub` key `null`
    /// (and the readable output silent): a walk-based audit read no stored bytes, and a
    /// zeroed object would claim every copy was verified.
    pub fn with_scrub(mut self, scrub: Option<ScrubSection>) -> Self {
        self.scrub = scrub;
        self
    }

    pub fn count(&self, kind: VerdictKind) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.verdict.kind() == kind)
            .count()
    }

    /// The readable summary: which source answered, counts for every class, then the first
    /// `examples` findings so a human gets the shape of the problem without the full dump
    /// (use `--json` for everything).
    ///
    /// A catalog-only audit prints only the classes it could actually check; the classes it
    /// could not are listed as such rather than shown as `0`, because a `0` for
    /// `missing-copy` would read as "checked and none missing" when nothing was stat'ed.
    pub fn summary_lines(&self, examples: usize) -> Vec<String> {
        let answered_from = match &self.source {
            AuditSource::Walk => format!(
                "{} paths scanned under {} against {} cold tier(s) (walked; no catalog)",
                self.scanned,
                self.watch.display(),
                self.dests.len()
            ),
            AuditSource::Catalog { path } => format!(
                "{} paths scanned under {} against {} cold tier(s) (from catalog {})",
                self.scanned,
                self.watch.display(),
                self.dests.len(),
                path.display()
            ),
            AuditSource::CatalogOnly { path } => format!(
                "answered from catalog {} with no filesystem access (--no-filesystem)",
                path.display()
            ),
        };
        let mut lines = vec![format!("audit: {answered_from}")];
        if let Some(section) = &self.catalog_only {
            lines.extend(section.summary_lines());
        }
        lines.push(format!("  healthy: {}", self.healthy));
        // Configured tiers are named once, up front, so a finding below can refer to a
        // tier by name and the reader has the key to what that name means.
        if let Some(tiers) = &self.tiers {
            if !tiers.is_empty() {
                lines.extend(tiers.summary_lines());
            }
        }
        // Only the kinds this source could check are counted. Walk and catalog modes check
        // the tree, so they count every problem kind as before; a catalog-only audit counts
        // the three it can derive from rows and names the rest as unchecked.
        let counted: Vec<VerdictKind> = match &self.source {
            AuditSource::CatalogOnly { .. } => vec![
                VerdictKind::UnderReplicated,
                VerdictKind::DamagedCopy,
                VerdictKind::MalformedCatalog,
            ],
            _ => VerdictKind::problems().to_vec(),
        };
        for kind in counted {
            lines.push(format!("  {}: {}", kind.as_str(), self.count(kind)));
        }
        if let Some(section) = &self.catalog_only {
            let names: Vec<&str> = section.unchecked.iter().map(|kind| kind.as_str()).collect();
            lines.push(format!(
                "  cannot be checked without the tree: {}",
                names.join(", ")
            ));
        }
        if self.findings.is_empty() {
            lines.push("no structural inconsistency found".to_string());
            return lines;
        }
        let shown = examples.min(self.findings.len());
        lines.push(format!(
            "findings (first {shown} of {}):",
            self.findings.len()
        ));
        for finding in self.findings.iter().take(shown) {
            lines.push(format!("  {}", finding.describe_with(self.tiers.as_ref())));
        }
        if shown < self.findings.len() {
            lines.push(format!(
                "  ... and {} more (use --json for the full list)",
                self.findings.len() - shown
            ));
        }
        lines
    }

    /// A stable, machine-readable document. Hand-written because the crate deliberately
    /// does not depend on a JSON library; the shape is part of the tool's contract and
    /// is covered by tests, not generated by reflection.
    pub fn to_json(&self, repairs: Option<&[RepairOutcome]>) -> String {
        let mut dest_json = Vec::new();
        for dest in &self.dests {
            dest_json.push(json_string(&dest.to_string_lossy()));
        }

        let mut findings = Vec::new();
        for finding in &self.findings {
            let cold_copy = finding
                .cold_copy
                .as_ref()
                .map(|copy| json_string(&copy.to_string_lossy()))
                .unwrap_or_else(|| "null".to_string());
            let target = match &finding.verdict {
                Verdict::DanglingSymlink { target }
                | Verdict::UnexpectedTarget { target }
                | Verdict::UnknownVersion { target } => json_string(&target.to_string_lossy()),
                _ => "null".to_string(),
            };
            findings.push(format!(
                "{{\"path\":{},\"relative\":{},\"kind\":{},\"target\":{},\"cold_copy\":{}}}",
                json_string(&finding.path.to_string_lossy()),
                json_string(&finding.relative.to_string_lossy()),
                json_string(finding.verdict.kind().as_str()),
                target,
                cold_copy
            ));
        }

        let mut counts = Vec::new();
        for kind in VerdictKind::problems() {
            counts.push(format!(
                "{}:{}",
                json_string(kind.as_str()),
                self.count(kind)
            ));
        }

        let repair_json = match repairs {
            None => "{\"enabled\":false}".to_string(),
            Some(repairs) => {
                let mut actions = Vec::new();
                for repair in repairs {
                    actions.push(format!(
                        "{{\"path\":{},\"kind\":{},\"action\":{},\"target\":{},\"reason\":{}}}",
                        json_string(&repair.path.to_string_lossy()),
                        json_string(repair.kind.as_str()),
                        json_string(repair.action.name()),
                        repair
                            .action
                            .target()
                            .map(|target| json_string(&target.to_string_lossy()))
                            .unwrap_or_else(|| "null".to_string()),
                        repair
                            .action
                            .reason()
                            .map(json_string)
                            .unwrap_or_else(|| "null".to_string())
                    ));
                }
                format!(
                    "{{\"enabled\":true,\"removed-duplicate\":{},\"restored-symlink\":{},\"refused\":{},\"not-attempted\":{},\"marked-for-resync\":{},\"actions\":[{}]}}",
                    repairs
                        .iter()
                        .filter(|r| matches!(r.action, RepairAction::RemovedDuplicate { .. }))
                        .count(),
                    repairs
                        .iter()
                        .filter(|r| matches!(r.action, RepairAction::RestoredSymlink { .. }))
                        .count(),
                    repairs
                        .iter()
                        .filter(|r| matches!(r.action, RepairAction::Refused { .. }))
                        .count(),
                    repairs
                        .iter()
                        .filter(|r| matches!(r.action, RepairAction::NotAttempted { .. }))
                        .count(),
                    repairs
                        .iter()
                        .filter(|r| matches!(r.action, RepairAction::MarkedForResync { .. }))
                        .count(),
                    actions.join(",")
                )
            }
        };

        let catalog_json = match &self.source {
            AuditSource::Catalog { path } | AuditSource::CatalogOnly { path } => {
                json_string(&path.to_string_lossy())
            }
            AuditSource::Walk => "null".to_string(),
        };

        // `null`, not a zeroed object, when no catalog answered. A consumer must be able to
        // tell "this audit read no stored bytes" from "this audit read them and all were
        // verified"; zeroes would collapse those into the reassuring one.
        let scrub_json = match &self.scrub {
            None => "null".to_string(),
            Some(scrub) => scrub.json(),
        };

        // What a catalog-only audit could report and could not check. `null` for any audit
        // that had filesystem access, so a consumer can tell "0 findings" from "0 checked":
        // the `unchecked` list names the finding kinds this run never attempted. It carries
        // its own recorded scrub summary, separate from the top-level `scrub` object that
        // names the catalog's scrub state for every mode.
        let no_filesystem_json = match &self.catalog_only {
            None => "null".to_string(),
            Some(section) => {
                let floors: Vec<String> = section
                    .floors
                    .iter()
                    .map(|(tier, copies)| {
                        format!("{{\"tier\":{},\"copies\":{copies}}}", json_string(tier))
                    })
                    .collect();
                let unchecked: Vec<String> = section
                    .unchecked
                    .iter()
                    .map(|kind| json_string(kind.as_str()))
                    .collect();
                let scrub = section.scrub;
                format!(
                    "{{\"objects\":{},\"names\":{},\"locations\":{},\"damaged\":{},\"floors\":[{}],\"scrub\":{{\"locations\":{},\"verified\":{},\"never-scrubbed\":{},\"damaged\":{}}},\"unchecked\":[{}]}}",
                    section.objects,
                    section.names,
                    section.locations,
                    section.damaged,
                    floors.join(","),
                    scrub.locations,
                    scrub.verified,
                    scrub.never_scrubbed,
                    scrub.damaged,
                    unchecked.join(",")
                )
            }
        };

        // The configured tiers, as a machine-readable list. `null` when none was given,
        // so a consumer can tell "no config" from "an empty config" the same way the CLI
        // does.
        let tiers_json = match &self.tiers {
            None => "null".to_string(),
            Some(tiers) => {
                let items: Vec<String> = tiers
                    .tiers()
                    .iter()
                    .map(|tier| {
                        format!(
                            "{{\"name\":{},\"kind\":{},\"path\":{},\"volatility\":{},\"recall\":{},\"copies\":{},\"cost\":{}}}",
                            json_string(&tier.name),
                            json_string(tier.kind.as_str()),
                            json_string(&tier.path.to_string_lossy()),
                            json_string(tier.volatility.as_str()),
                            json_string(tier.recall.as_str()),
                            tier.copies,
                            tier.cost
                                .as_ref()
                                .map(|cost| json_string(cost))
                                .unwrap_or_else(|| "null".to_string())
                        )
                    })
                    .collect();
                format!("[{}]", items.join(","))
            }
        };

        format!(
            "{{\"watch\":{},\"dest\":[{}],\"source\":{},\"catalog\":{},\"tiers\":{},\"scanned\":{},\"healthy\":{},\"scrub\":{},\"findings\":[{}],\"counts\":{{{}}},\"repair\":{},\"no-filesystem\":{}}}",
            json_string(&self.watch.to_string_lossy()),
            dest_json.join(","),
            json_string(self.source.as_str()),
            catalog_json,
            tiers_json,
            self.scanned,
            self.healthy,
            scrub_json,
            findings.join(","),
            counts.join(","),
            repair_json,
            no_filesystem_json
        )
    }
}

/// Classify one path from its two sides. Pure: no filesystem, no clock, no globals.
///
/// The order matters only where two descriptions are possible at once (a dangling
/// symlink that also has a cold copy at the mirrored path). A dangling or unexpected
/// symlink is what the name at the source path *is*, so it wins over the copy, which is
/// then only the repair hint.
pub fn classify(source: &SourceState, cold_copies: &[PathBuf]) -> Verdict {
    match source {
        SourceState::File if !cold_copies.is_empty() => Verdict::Duplicate,
        SourceState::File => Verdict::Healthy,
        SourceState::Symlink {
            target,
            resolves: false,
            ..
        } => Verdict::DanglingSymlink {
            target: target.clone(),
        },
        SourceState::Symlink {
            resolves: true,
            dest_index: Some(_),
            ..
        } => Verdict::Healthy,
        SourceState::Symlink {
            target,
            resolves: true,
            dest_index: None,
        } => Verdict::UnexpectedTarget {
            target: target.clone(),
        },
        SourceState::Missing if !cold_copies.is_empty() => Verdict::OrphanedCopy,
        SourceState::Missing => Verdict::Healthy,
    }
}

/// Walk `watch` and every `dest`, classify the union of their relative paths and return
/// every path that is not healthy. Read-only. The single-copy audit: no floor beyond the
/// one copy the mover's flat layout implies.
pub fn audit(watch: &Path, dests: &[PathBuf]) -> Result<AuditReport, AuditError> {
    audit_with_copies(watch, dests, 1)
}

/// As [`audit`], but with a copy floor: a resolving symlink into a cold tier whose object
/// is present on fewer than `copies` configured destinations is a `replica-lost` finding,
/// naming the disks it is missing from. The catalog reports the same state as
/// `under-replicated`; this is the filesystem-only view of it.
pub fn audit_with_copies(
    watch: &Path,
    dests: &[PathBuf],
    copies: usize,
) -> Result<AuditReport, AuditError> {
    let watch_side = scan(watch)?;
    let canonical_dests: Vec<PathBuf> = dests
        .iter()
        .map(|dest| dest.canonicalize().unwrap_or_else(|_| dest.clone()))
        .collect();

    let mut dest_maps = Vec::with_capacity(dests.len());
    for dest in dests {
        if !dest.is_dir() {
            return Err(AuditError::MissingDest { path: dest.clone() });
        }
        let side = scan(dest)?;
        // A cold tier is expected to hold regular files; a symlink that appeared there
        // is not a copy of anything and is not the mover's output.
        let files: BTreeMap<PathBuf, SideKind> = side
            .into_iter()
            .filter(|(_, kind)| *kind == SideKind::File)
            .collect();
        dest_maps.push(files);
    }

    let mut relatives: BTreeSet<PathBuf> = watch_side.keys().cloned().collect();
    for side in &dest_maps {
        relatives.extend(side.keys().cloned());
    }

    let mut findings = Vec::new();
    let mut healthy = 0usize;

    for relative in &relatives {
        let source_path = watch.join(relative);
        let source = source_state(
            &source_path,
            watch_side.get(relative).copied(),
            &canonical_dests,
        )?;
        let cold_copies: Vec<PathBuf> = dests
            .iter()
            .zip(&dest_maps)
            .filter(|(_, side)| side.contains_key(relative))
            .map(|(dest, _)| dest.join(relative))
            .collect();

        let verdict = match &source {
            SourceState::Symlink {
                resolves: true,
                dest_index: Some(_),
                ..
            } if copies > 1 && cold_copies.len() < copies => {
                let missing: Vec<PathBuf> = dests
                    .iter()
                    .zip(&dest_maps)
                    .filter(|(_, side)| !side.contains_key(relative))
                    .map(|(dest, _)| dest.join(relative))
                    .collect();
                Verdict::ReplicaLost {
                    present: cold_copies.clone(),
                    missing,
                }
            }
            _ => classify(&source, &cold_copies),
        };
        if verdict.is_healthy() {
            healthy += 1;
            continue;
        }
        findings.push(Finding {
            path: source_path,
            relative: relative.clone(),
            cold_copy: cold_copies.into_iter().next(),
            verdict,
        });
    }

    findings.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.verdict.kind().as_str().cmp(b.verdict.kind().as_str()))
    });

    Ok(AuditReport {
        watch: watch.to_path_buf(),
        dests: dests.to_vec(),
        source: AuditSource::Walk,
        scanned: relatives.len(),
        healthy,
        findings,
        tiers: None,
        scrub: None,
        catalog_only: None,
    })
}

/// Where a symlink resolves, as the catalog sees tiers.
enum LinkTarget {
    /// Points at something that does not exist.
    Dangling { target: PathBuf },
    /// Resolves, but outside every configured `--dest`.
    Outside { target: PathBuf },
    /// Resolves to a regular file under a `--dest`, at this relative key.
    Cold {
        target: PathBuf,
        tier: String,
        key: String,
    },
}

/// Resolve a symlink against the canonical destination tiers. A local version of
/// `catalog::resolve_link` because the audit needs the resolved tier *and* the storage key
/// together, and the catalog's copy is private to its sync pass.
fn resolve_target(link: &Path, dests: &[(PathBuf, String)]) -> Result<LinkTarget, AuditError> {
    let target = fs::read_link(link).map_err(|source| AuditError::ReadLink {
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
        return Ok(LinkTarget::Dangling { target });
    };
    if !fs::metadata(&canonical_target).is_ok_and(|metadata| metadata.is_file()) {
        return Ok(LinkTarget::Outside { target });
    }
    for (root, tier) in dests {
        let Ok(relative) = canonical_target.strip_prefix(root) else {
            continue;
        };
        let key = relative.to_string_lossy().into_owned();
        return Ok(LinkTarget::Cold {
            target: canonical_target,
            tier: tier.clone(),
            key,
        });
    }
    Ok(LinkTarget::Outside { target })
}

/// Answer from the catalog, plus one filesystem pass over the watched tree.
///
/// This is issue #19: the catalog is the arbiter, so the audit does not walk both sides and
/// compare. It reads the recorded names, objects and locations, walks the *namespace* once,
/// and asks the recorded rows whether the filesystem still agrees:
///
/// * a **name** whose path is gone is `name-vanished` (the row is kept: the bytes may be on
///   a tier, and only a human knows whether the path moved or the file is gone);
/// * a symlink that does not resolve is `dangling-symlink`, one that resolves outside the
///   tiers is `unexpected-target`, and one that resolves to a file the catalog has no
///   location for is `unknown-version` — a version the catalog does not have;
/// * a **location** with no file is `missing-copy`, and a primary location whose bytes do
///   not match the recorded checksum is `checksum-mismatch`;
/// * an object with no surviving location is `copy-floor` (§6);
/// * a path in the tree with no `name` row is `unknown-path` — reported, never adopted;
/// * cold bytes no name references are `orphaned-copy`.
///
/// Read-only. Nothing here writes the catalog or the tree; a disagreement is a finding, not
/// something to reconcile.
pub fn catalog_audit(
    catalog: &Catalog,
    watch: &Path,
    dests: &[PathBuf],
) -> Result<AuditReport, AuditError> {
    let watch_root = canonical(watch);
    let watch_tier = watch_root.to_string_lossy().into_owned();
    let dest_tiers: Vec<(PathBuf, String)> = dests
        .iter()
        .map(|dest| {
            let root = canonical(dest);
            let tier = root.to_string_lossy().into_owned();
            (root, tier)
        })
        .collect();

    // The roots a recorded tier must be one of before its key becomes a path. A row that
    // names any other tier is reported, never stat'ed or hashed (issue #72).
    let roots: Vec<PathBuf> = std::iter::once(watch_root.clone())
        .chain(dest_tiers.iter().map(|(root, _)| root.clone()))
        .collect();

    let objects = catalog.all_objects()?;
    let object_index: BTreeMap<String, crate::catalog::ObjectRecord> = objects
        .iter()
        .map(|object| (object.id.clone(), object.clone()))
        .collect();
    let names = catalog.all_names()?;
    let locations = catalog.all_locations()?;

    let walk = scan(watch)?;
    let known: BTreeSet<PathBuf> = names.iter().map(|(path, _)| PathBuf::from(path)).collect();

    let mut findings = Vec::new();
    // Paths and objects that have at least one problem, so `healthy` counts an object as
    // healthy only when neither the name nor any of its copies is in question.
    let mut suspect_names: BTreeSet<PathBuf> = BTreeSet::new();
    let mut suspect_objects: BTreeSet<String> = BTreeSet::new();

    // 1. Names: the namespace the catalog recorded, against what the tree actually has.
    //
    // Locations are indexed by (tier, key) so a symlink can ask "does the catalog hold this
    // exact version?" without a linear scan per link.
    let mut location_of: BTreeMap<(String, String), String> = BTreeMap::new();
    for location in &locations {
        location_of.insert(
            (location.tier.clone(), location.storage_key.clone()),
            location.object.clone(),
        );
    }

    for (path, object) in &names {
        let relative = PathBuf::from(path);
        let abs = watch.join(&relative);
        let verdict = match walk.get(&relative).copied() {
            None => Some(Verdict::NameVanished {
                object: object.clone(),
            }),
            Some(SideKind::File) => {
                // A real file at the path. If the object also has a cold copy, the source
                // removal never happened (or a restore is in progress): a duplicate.
                let has_cold = locations
                    .iter()
                    .any(|location| location.object == *object && location.tier != watch_tier);
                if has_cold {
                    Some(Verdict::Duplicate)
                } else {
                    None
                }
            }
            Some(SideKind::Symlink) => match resolve_target(&abs, &dest_tiers)? {
                LinkTarget::Dangling { target } => Some(Verdict::DanglingSymlink { target }),
                LinkTarget::Outside { target } => Some(Verdict::UnexpectedTarget { target }),
                LinkTarget::Cold { target, tier, key } => {
                    // The link resolves, but does the catalog hold *this version* of the
                    // object? A link to a file the catalog does not have is exactly the
                    // "version the catalog does not have" case.
                    match location_of.get(&(tier, key)) {
                        Some(holder) if holder == object => None,
                        _ => Some(Verdict::UnknownVersion { target }),
                    }
                }
            },
        };

        if let Some(verdict) = verdict {
            suspect_names.insert(relative.clone());
            suspect_objects.insert(object.clone());
            findings.push(Finding {
                path: abs,
                relative,
                cold_copy: None,
                verdict,
            });
        }
    }

    // 2. Locations: every recorded copy, against the file the catalog says is there.
    let mut by_object: BTreeMap<String, Vec<&crate::catalog::LocationRecord>> = BTreeMap::new();
    for location in &locations {
        by_object
            .entry(location.object.clone())
            .or_default()
            .push(location);
    }

    let named_objects: BTreeSet<String> = names.iter().map(|(_, object)| object.clone()).collect();

    for (object, copies) in &by_object {
        let expected = object_index
            .get(object)
            .map(|record| record.checksum.as_str())
            .unwrap_or_default();
        let mut surviving = 0usize;

        for copy in copies {
            // The row is joined only after it proves to stay under a current root; a
            // refused row is reported and no stat or hash runs on it (issue #72).
            let file = match resolve_location_path(&copy.tier, &copy.storage_key, &roots) {
                Ok(file) => file,
                Err(error) => {
                    suspect_objects.insert(object.clone());
                    findings.push(Finding {
                        path: PathBuf::from(format!("{}/{}", copy.tier, copy.storage_key)),
                        relative: PathBuf::from(&copy.storage_key),
                        cold_copy: None,
                        verdict: Verdict::MalformedCatalog {
                            tier: copy.tier.clone(),
                            key: copy.storage_key.clone(),
                            detail: error.detail().to_string(),
                        },
                    });
                    continue;
                }
            };
            if !fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file()) {
                suspect_objects.insert(object.clone());
                findings.push(Finding {
                    path: file,
                    relative: PathBuf::from(&copy.storage_key),
                    cold_copy: None,
                    verdict: Verdict::MissingCopy {
                        tier: copy.tier.clone(),
                        key: copy.storage_key.clone(),
                        primary: copy.is_primary,
                    },
                });
                continue;
            }
            surviving += 1;

            // Only the copy of record is hashed. Hashing every replica on every audit is a
            // scrub (`docs/design.md` §6), not an audit; a replica's *presence* is checked
            // here and its bytes are the scrubber's job. The primary is the copy the tool
            // would hand a reader, so its integrity is the one an audit must not miss.
            if !copy.is_primary {
                continue;
            }
            match digest::file_digest(&file) {
                Ok(hash) if hash.to_hex().as_str() == expected => {}
                Ok(hash) => {
                    suspect_objects.insert(object.clone());
                    findings.push(Finding {
                        path: file,
                        relative: PathBuf::from(&copy.storage_key),
                        cold_copy: None,
                        verdict: Verdict::ChecksumMismatch {
                            tier: copy.tier.clone(),
                            key: copy.storage_key.clone(),
                            expected: expected.to_string(),
                            found: hash.to_hex().to_string(),
                        },
                    });
                }
                Err(source) => {
                    // Present but unreadable is not intact; report it in the same shape as a
                    // mismatch rather than pretending the copy is fine.
                    suspect_objects.insert(object.clone());
                    findings.push(Finding {
                        path: file,
                        relative: PathBuf::from(&copy.storage_key),
                        cold_copy: None,
                        verdict: Verdict::ChecksumMismatch {
                            tier: copy.tier.clone(),
                            key: copy.storage_key.clone(),
                            expected: expected.to_string(),
                            found: format!("unreadable: {source}"),
                        },
                    });
                }
            }
        }

        if surviving < COPY_FLOOR {
            suspect_objects.insert(object.clone());
            findings.push(Finding {
                path: PathBuf::from(object),
                relative: PathBuf::new(),
                cold_copy: None,
                verdict: Verdict::CopyFloor {
                    object: object.clone(),
                    copies: surviving,
                },
            });
        }

        // Cold bytes with no name: nothing in the tree will ever read them.
        if !named_objects.contains(object) {
            for copy in copies {
                if copy.tier == watch_tier {
                    continue;
                }
                // A refused row was already reported in the location loop above; it never
                // becomes a path here either (issue #72).
                let Ok(cold) = resolve_location_path(&copy.tier, &copy.storage_key, &roots) else {
                    continue;
                };
                findings.push(Finding {
                    path: cold.clone(),
                    relative: PathBuf::from(&copy.storage_key),
                    cold_copy: Some(cold),
                    verdict: Verdict::OrphanedCopy,
                });
            }
        }
    }

    // 3. Paths the catalog does not know about. Reported, never adopted: ingesting a new
    //    name is `catalog sync`'s job, and an audit that silently ingested would be the
    //    catalog rewriting itself to match a tree — the exact failure the sync pass avoids.
    for relative in walk.keys() {
        if !known.contains(relative) {
            findings.push(Finding {
                path: watch.join(relative),
                relative: relative.clone(),
                cold_copy: None,
                verdict: Verdict::UnknownPath,
            });
        }
    }

    findings.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.verdict.kind().as_str().cmp(b.verdict.kind().as_str()))
    });

    let mut scanned: BTreeSet<&PathBuf> = walk.keys().collect();
    scanned.extend(known.iter());

    let healthy = names
        .iter()
        .filter(|(path, object)| {
            !suspect_names.contains(Path::new(path)) && !suspect_objects.contains(object)
        })
        .count();

    Ok(AuditReport {
        watch: watch.to_path_buf(),
        dests: dests.to_vec(),
        source: AuditSource::Catalog {
            path: catalog.path().to_path_buf(),
        },
        scanned: scanned.len(),
        healthy,
        findings,
        tiers: None,
        scrub: None,
        catalog_only: None,
    })
}

/// Answer from the catalog rows alone (issue #51): no walk, no `stat`, no hash, no tier
/// access at all.
///
/// Catalog mode above still makes one filesystem pass — it must, to tell a location row
/// that is stale from one whose file is there. This mode does not, which is the whole
/// point: it is for a tier that is not mounted (or a host where the tree is gone), and it
/// answers only what the recorded rows say. Everything it reports is therefore a claim
/// about the catalog, never about bytes, and it says which findings it could not make in
/// [`CatalogOnlySection::unchecked`] so a reader is never left to infer health from
/// silence:
///
/// * a **floor** an object's recorded locations fall below is `under-replicated` — rows
///   counted, not bytes verified;
/// * a **damage mark** a scrub left is `damaged-copy`;
/// * a **malformed row** (a tier or key that could never be joined into a path) is
///   `malformed-catalog` — the check is lexical, so it needs no filesystem;
/// * the recorded object/name/location counts, the floors, and the scrub-state summary go
///   in the report's [`CatalogOnlySection`].
///
/// `watch` and `dests` are carried only so the output can name the invocation; they are
/// never touched. Read-only, and it writes nothing to the catalog.
pub fn catalog_audit_no_filesystem(
    catalog: &Catalog,
    watch: &Path,
    dests: &[PathBuf],
) -> Result<AuditReport, AuditError> {
    // The recorded roots decide whether a row could become a path — a purely lexical check,
    // so it needs no filesystem. A catalog written before roots were recorded has none;
    // fall back to the invocation's watch/dest (the same roots catalog mode resolves
    // against) rather than declaring every row malformed.
    let mut roots = catalog.roots()?;
    if roots.is_empty() {
        roots.push(watch.to_path_buf());
        roots.extend(dests.iter().cloned());
    }
    let objects = catalog.reconcile_objects()?;
    let names = catalog.all_names()?;
    let floors = catalog.all_tier_floors()?;
    let scrub = catalog.scrub_summary()?;

    let floor_of: BTreeMap<&str, usize> = floors
        .iter()
        .map(|(tier, copies)| (tier.as_str(), *copies))
        .collect();

    let mut findings = Vec::new();
    // Objects with at least one recorded finding, so a name is called healthy only when
    // nothing about the object it points at is in question.
    let mut suspect_objects: BTreeSet<String> = BTreeSet::new();

    for object in &objects {
        let object_hex = hex_bytes(&object.object);
        for location in &object.locations {
            // A row that could never be a path is reported, never joined; the check is the
            // same lexical one catalog mode uses, minus any filesystem call after it.
            if let Err(error) = resolve_location_path(&location.tier, &location.storage_key, &roots)
            {
                suspect_objects.insert(object_hex.clone());
                findings.push(Finding {
                    path: PathBuf::from(format!("{}/{}", location.tier, location.storage_key)),
                    relative: PathBuf::from(&location.storage_key),
                    cold_copy: None,
                    verdict: Verdict::MalformedCatalog {
                        tier: location.tier.clone(),
                        key: location.storage_key.clone(),
                        detail: error.detail().to_string(),
                    },
                });
            }
            // A damage mark is recorded state: a scrub found no good copy to repair this
            // location from. Reported as such — the mark exists whether or not the bytes
            // still do, and only the tree could tell the two apart.
            if object
                .damaged
                .contains(&(location.tier.clone(), location.storage_key.clone()))
            {
                suspect_objects.insert(object_hex.clone());
                findings.push(Finding {
                    path: PathBuf::from(format!("{}/{}", location.tier, location.storage_key)),
                    relative: PathBuf::from(&location.storage_key),
                    cold_copy: None,
                    verdict: Verdict::DamagedCopy {
                        tier: location.tier.clone(),
                        key: location.storage_key.clone(),
                        object: object_hex.clone(),
                    },
                });
            }
        }

        // The floor application: the recorded floor of the tier the object's copy of record
        // sits on, defaulting to the schema-wide [`COPY_FLOOR`] when no tier floor was
        // recorded. An object with no locations is below the floor of 1, which is right —
        // its rows are gone — but whether its bytes are is unknowable here.
        let primary = object
            .locations
            .iter()
            .find(|location| location.is_primary)
            .or_else(|| object.locations.first());
        let floor = primary
            .and_then(|location| floor_of.get(location.tier.as_str()).copied())
            .unwrap_or(COPY_FLOOR);
        let copies = object.locations.len();
        if copies < floor {
            suspect_objects.insert(object_hex.clone());
            findings.push(Finding {
                path: PathBuf::from(&object_hex),
                relative: PathBuf::new(),
                cold_copy: None,
                verdict: Verdict::UnderReplicated {
                    object: object_hex.clone(),
                    copies,
                    floor,
                },
            });
        }
    }

    findings.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.verdict.kind().as_str().cmp(b.verdict.kind().as_str()))
    });

    let locations = catalog.location_count()?;
    let unchecked = VerdictKind::filesystem_only().to_vec();
    let damaged = object_damage_count(&objects);
    let section = CatalogOnlySection {
        objects: objects.len(),
        names: names.len(),
        locations,
        floors,
        damaged,
        scrub,
        unchecked,
    };

    Ok(AuditReport {
        watch: watch.to_path_buf(),
        dests: dests.to_vec(),
        source: AuditSource::CatalogOnly {
            path: catalog.path().to_path_buf(),
        },
        // Nothing was walked, so `scanned` counts recorded rows (names and locations) and
        // `healthy` counts recorded names whose object has no recorded finding. A named
        // object whose every recorded location is gone still counts as it stands; the tree
        // is what would say otherwise, and this mode cannot see it.
        scanned: names.len() + locations,
        healthy: names
            .iter()
            .filter(|(_, object)| !suspect_objects.contains(object))
            .count(),
        findings,
        tiers: None,
        scrub: None,
        catalog_only: Some(section),
    })
}

/// How many recorded locations carry a damage mark, across every object.
fn object_damage_count(objects: &[crate::catalog::ReconcileObject]) -> usize {
    objects.iter().map(|object| object.damaged.len()).sum()
}

/// Hex for an object id. The catalog's own encoder is private to it, and a report should not
/// grow a public dependency for six lines.
fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Canonical form of a root, so a tier string recorded by `catalog sync` (which uses the
/// same canonicalization) matches the one the audit computes. A path that cannot be
/// canonicalized is used as given — the same fallback the catalog makes.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn scan(root: &Path) -> Result<BTreeMap<PathBuf, SideKind>, AuditError> {
    let entries =
        disk_management::list_files_recursive(root).map_err(|source| AuditError::Walk {
            path: root.to_path_buf(),
            source,
        })?;
    let mut side = BTreeMap::new();
    for entry in entries {
        let kind = if entry.is_symlink {
            SideKind::Symlink
        } else {
            SideKind::File
        };
        side.insert(entry.relative, kind);
    }
    Ok(side)
}

/// Resolve one source path into the state the classifier understands.
fn source_state(
    path: &Path,
    kind: Option<SideKind>,
    canonical_dests: &[PathBuf],
) -> Result<SourceState, AuditError> {
    match kind {
        None => Ok(SourceState::Missing),
        Some(SideKind::File) => Ok(SourceState::File),
        Some(SideKind::Symlink) => {
            let target = fs::read_link(path).map_err(|source| AuditError::ReadLink {
                path: path.to_path_buf(),
                source,
            })?;
            let resolved = if target.is_absolute() {
                target.clone()
            } else {
                // A relative link is relative to the directory holding the link.
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(&target)
            };
            // `metadata` follows the link; an error means the target is missing, or the
            // link is part of a loop — both are "does not resolve".
            let resolves = fs::metadata(&resolved).is_ok();
            let dest_index = if resolves {
                resolved.canonicalize().ok().and_then(|real| {
                    canonical_dests
                        .iter()
                        .position(|dest| real.starts_with(dest))
                })
            } else {
                None
            };
            Ok(SourceState::Symlink {
                target,
                resolves,
                dest_index,
            })
        }
    }
}

/// What repair did to one finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// The real file at the source path was removed after matching the cold copy's
    /// checksum, and replaced by a symlink to that copy.
    RemovedDuplicate { target: PathBuf },
    /// A broken or misdirected symlink was replaced by one pointing at an existing cold
    /// copy. No bytes were deleted.
    RestoredSymlink { target: PathBuf },
    /// Repair declined to act. Never deletes anything.
    Refused { reason: String },
    /// The finding is not safely auto-repairable.
    NotAttempted { reason: String },
    /// Catalog mode: the finding is a catalog/file disagreement, so repair did not touch
    /// either side. The row is marked for resync — investigate the tree, then run
    /// `catalog sync` (or a scrub) to re-record it. This is deliberately weaker than a
    /// rewrite: the catalog may record a move the walk sees as unfinished, and guessing is
    /// how a repair deletes the wrong thing.
    MarkedForResync { reason: String },
}

impl RepairAction {
    pub fn name(&self) -> &'static str {
        match self {
            RepairAction::RemovedDuplicate { .. } => "removed-duplicate",
            RepairAction::RestoredSymlink { .. } => "restored-symlink",
            RepairAction::Refused { .. } => "refused",
            RepairAction::NotAttempted { .. } => "not-attempted",
            RepairAction::MarkedForResync { .. } => "marked-for-resync",
        }
    }

    pub fn target(&self) -> Option<PathBuf> {
        match self {
            RepairAction::RemovedDuplicate { target }
            | RepairAction::RestoredSymlink { target } => Some(target.clone()),
            _ => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            RepairAction::Refused { reason }
            | RepairAction::NotAttempted { reason }
            | RepairAction::MarkedForResync { reason } => Some(reason),
            _ => None,
        }
    }
}

/// One finding's repair attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutcome {
    pub path: PathBuf,
    pub kind: VerdictKind,
    pub action: RepairAction,
}

impl RepairOutcome {
    /// True when the finding was resolved and the path can now be healthy.
    pub fn repaired(&self) -> bool {
        matches!(
            self.action,
            RepairAction::RemovedDuplicate { .. } | RepairAction::RestoredSymlink { .. }
        )
    }
}

/// Repair what can be repaired without guessing.
///
/// - `duplicate`: hash both copies; only when the digests match is the source file
///   removed and replaced by a symlink to the cold copy. A mismatch leaves both copies
///   untouched (§6: verify before delete).
/// - `dangling-symlink`: there is no data behind the current target, so when a cold copy
///   exists at the mirrored path the link is simply re-pointed at it.
/// - `unexpected-target`: the current target *does* resolve, so re-pointing it could
///   discard a link someone created on purpose. Not attempted.
/// - `orphaned-copy`: the name is gone, so deciding between restoring it and reclaiming
///   the cold bytes needs a human. Not attempted — and never deleted, because there is
///   no source copy to checksum it against.
pub fn repair(report: &AuditReport) -> Result<Vec<RepairOutcome>, AuditError> {
    let mut outcomes = Vec::new();

    for finding in &report.findings {
        let action = match &finding.verdict {
            Verdict::Healthy => continue,
            Verdict::Duplicate => match &finding.cold_copy {
                Some(copy) if copy.is_file() => {
                    let source_digest = digest::file_digest(&finding.path).map_err(|source| {
                        AuditError::Checksum {
                            path: finding.path.clone(),
                            source,
                        }
                    })?;
                    let copy_digest = digest::file_digest(copy).map_err(|source| {
                        AuditError::Checksum {
                            path: copy.clone(),
                            source,
                        }
                    })?;
                    if source_digest != copy_digest {
                        RepairAction::Refused {
                            reason: format!(
                                "checksum mismatch: source {} is not the cold copy {}{}",
                                source_digest.to_hex(),
                                copy.display(),
                                copy_digest.to_hex()
                            ),
                        }
                    } else {
                        replace_with_symlink(&finding.path, copy)?
                    }
                }
                _ => RepairAction::NotAttempted {
                    reason: "no cold copy at the mirrored path to link to".to_string(),
                },
            },
            Verdict::DanglingSymlink { .. } => match &finding.cold_copy {
                Some(copy) if copy.is_file() => restore_symlink(&finding.path, copy)?,
                _ => RepairAction::NotAttempted {
                    reason: "no cold copy at the mirrored path to point at".to_string(),
                },
            },
            Verdict::UnexpectedTarget { .. } => RepairAction::NotAttempted {
                reason: "the target resolves outside the cold tiers; re-pointing it could discard a working link"
                    .to_string(),
            },
            Verdict::OrphanedCopy => RepairAction::NotAttempted {
                reason: "the name is gone; choosing between restoring it and reclaiming the cold bytes needs a human"
                    .to_string(),
            },
            Verdict::ReplicaLost { .. } => RepairAction::NotAttempted {
                reason: "replicating a lost copy from a surviving one is a mover pass, not audit repair"
                    .to_string(),
            },
            // Catalog-mode verdicts are never repaired by the walk repairer: they are
            // disagreements between the catalog and the tree, and `catalog_repair` marks
            // them for resync instead of guessing. Reaching one here means a caller wired
            // the wrong report to the wrong repairer, so refuse rather than act.
            Verdict::NameVanished { .. }
            | Verdict::MissingCopy { .. }
            | Verdict::ChecksumMismatch { .. }
            | Verdict::CopyFloor { .. }
            | Verdict::UnderReplicated { .. }
            | Verdict::DamagedCopy { .. }
            | Verdict::UnknownVersion { .. }
            | Verdict::MalformedCatalog { .. }
            | Verdict::UnknownPath => RepairAction::NotAttempted {
                reason: "catalog-mode finding: use catalog repair, which marks it for resync"
                    .to_string(),
            },
        };

        outcomes.push(RepairOutcome {
            path: finding.path.clone(),
            kind: finding.verdict.kind(),
            action,
        });
    }

    Ok(outcomes)
}

/// Repair a catalog-backed audit: mark, do not rewrite.
///
/// Catalog mode never mutates. The catalog is the arbiter precisely because it can remember
/// something the filesystem walk cannot see (a move recorded but not yet observable, a copy
/// on an unmounted tier), so a disagreement is not an instruction to change the tree. Each
/// finding is marked for resync — the operator investigates, then `catalog sync` re-records
/// the tree's actual state (which itself refuses to guess: see `catalog.rs`). Nothing is
/// deleted, re-pointed, or rewritten here, and the exit-code contract is unchanged: an
/// unresolved finding keeps the alert up.
pub fn catalog_repair(report: &AuditReport) -> Vec<RepairOutcome> {
    report
        .findings
        .iter()
        .map(|finding| RepairOutcome {
            path: finding.path.clone(),
            kind: finding.verdict.kind(),
            action: RepairAction::MarkedForResync {
                reason: format!(
                    "{} is a catalog/file disagreement; the row was left unchanged. \
                     Investigate, then run `just_cache catalog sync` to re-record it.",
                    finding.verdict.kind().as_str()
                ),
            },
        })
        .collect()
}

/// Remove a verified-duplicate source file and leave the symlink the mover intended.
///
/// The source is first renamed aside so there is never a moment where the path means
/// nothing: create the link, then drop the temporary copy. The bytes are on the cold
/// tier either way, and on any failure the original file is put back.
fn replace_with_symlink(source: &Path, cold_copy: &Path) -> Result<RepairAction, AuditError> {
    let target = disk_management::symlink_target(source, cold_copy);
    let temp = sibling_temp(source);

    fs::rename(source, &temp).map_err(|source_err| AuditError::Repair {
        path: source.to_path_buf(),
        source: source_err,
    })?;

    if let Err(err) = create_symlink(&target, source) {
        let _ = fs::rename(&temp, source);
        return Err(AuditError::Repair {
            path: source.to_path_buf(),
            source: err,
        });
    }

    fs::remove_file(&temp).map_err(|source_err| AuditError::Repair {
        path: temp.clone(),
        source: source_err,
    })?;
    Ok(RepairAction::RemovedDuplicate { target })
}

/// Replace a symlink whose target does not resolve with one pointing at the cold copy.
fn restore_symlink(source: &Path, cold_copy: &Path) -> Result<RepairAction, AuditError> {
    let target = disk_management::symlink_target(source, cold_copy);
    // Removing a symlink with remove_file removes the link itself, not the target.
    fs::remove_file(source).map_err(|source_err| AuditError::Repair {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    create_symlink(&target, source).map_err(|source_err| AuditError::Repair {
        path: source.to_path_buf(),
        source: source_err,
    })?;
    Ok(RepairAction::RestoredSymlink { target })
}

fn sibling_temp(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    path.with_file_name(format!(
        ".just_cache-audit-{name}-{}-{nanos}.tmp",
        std::process::id()
    ))
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    let _ = (target, link);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

/// Minimal JSON string escaping: enough for paths and our own ASCII reason strings, and
/// tested, rather than the reflection machinery of a JSON dependency.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nowhere() -> PathBuf {
        PathBuf::from("/nowhere/missing.bin")
    }

    #[test]
    fn a_plain_file_with_no_cold_copy_is_healthy() {
        assert_eq!(classify(&SourceState::File, &[]), Verdict::Healthy);
    }

    #[test]
    fn a_real_file_with_a_cold_copy_is_a_duplicate() {
        let copies = vec![PathBuf::from("/cold/a.bin")];
        assert_eq!(classify(&SourceState::File, &copies), Verdict::Duplicate);
    }

    #[test]
    fn a_symlink_that_does_not_resolve_is_dangling() {
        let state = SourceState::Symlink {
            target: nowhere(),
            resolves: false,
            dest_index: None,
        };
        assert_eq!(
            classify(&state, &[PathBuf::from("/cold/a.bin")]),
            Verdict::DanglingSymlink { target: nowhere() }
        );
    }

    #[test]
    fn a_symlink_into_a_cold_tier_is_healthy() {
        let state = SourceState::Symlink {
            target: PathBuf::from("/cold/a.bin"),
            resolves: true,
            dest_index: Some(0),
        };
        assert_eq!(
            classify(&state, &[PathBuf::from("/cold/a.bin")]),
            Verdict::Healthy
        );
    }

    #[test]
    fn a_resolving_symlink_outside_every_cold_tier_is_unexpected() {
        let state = SourceState::Symlink {
            target: PathBuf::from("/home/me/elsewhere.bin"),
            resolves: true,
            dest_index: None,
        };
        assert_eq!(
            classify(&state, &[]),
            Verdict::UnexpectedTarget {
                target: PathBuf::from("/home/me/elsewhere.bin")
            }
        );
    }

    #[test]
    fn cold_bytes_with_no_name_at_the_source_are_an_orphaned_copy() {
        let copies = vec![PathBuf::from("/cold/a.bin")];
        assert_eq!(
            classify(&SourceState::Missing, &copies),
            Verdict::OrphanedCopy
        );
        assert_eq!(classify(&SourceState::Missing, &[]), Verdict::Healthy);
    }

    #[test]
    fn every_problem_kind_is_counted_and_named() {
        let report = AuditReport {
            watch: PathBuf::from("/watch"),
            dests: vec![PathBuf::from("/cold")],
            source: AuditSource::Walk,
            scanned: 10,
            healthy: 6,
            tiers: None,
            scrub: None,
            catalog_only: None,
            findings: vec![
                Finding {
                    path: PathBuf::from("/watch/a.bin"),
                    relative: PathBuf::from("a.bin"),
                    cold_copy: Some(PathBuf::from("/cold/a.bin")),
                    verdict: Verdict::Duplicate,
                },
                Finding {
                    path: PathBuf::from("/watch/b.bin"),
                    relative: PathBuf::from("b.bin"),
                    cold_copy: None,
                    verdict: Verdict::DanglingSymlink { target: nowhere() },
                },
            ],
        };
        assert_eq!(report.count(VerdictKind::Duplicate), 1);
        assert_eq!(report.count(VerdictKind::DanglingSymlink), 1);
        assert_eq!(report.count(VerdictKind::OrphanedCopy), 0);
        assert!(report.has_findings());

        let summary = report.summary_lines(10).join("\n");
        assert!(summary.contains("duplicate: 1"));
        assert!(summary.contains("dangling-symlink: 1"));
        assert!(summary.contains("healthy: 6"));
    }

    #[test]
    fn a_symlink_into_the_cold_tier_is_a_healthy_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("watch");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(cold.join("a.bin"), b"payload").unwrap();
        create_symlink(Path::new("../cold/a.bin"), &watch.join("a.bin")).unwrap();

        // The link resolves under the cold tier, so the pair is healthy.
        let report = audit(&watch, std::slice::from_ref(&cold)).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.healthy, 1);
        assert!(!report.has_findings());
    }

    #[test]
    fn json_carries_the_counts_and_the_repair_block() {
        let report = AuditReport {
            watch: PathBuf::from("/watch"),
            dests: vec![PathBuf::from("/cold")],
            source: AuditSource::Walk,
            scanned: 1,
            healthy: 0,
            tiers: None,
            scrub: None,
            catalog_only: None,
            findings: vec![Finding {
                path: PathBuf::from("/watch/a)weird\"name"),
                relative: PathBuf::from("a)weird\"name"),
                cold_copy: Some(PathBuf::from("/cold/a)weird\"name")),
                verdict: Verdict::Duplicate,
            }],
        };
        let json = report.to_json(None);
        assert!(json.contains("\"duplicate\":1"));
        assert!(json.contains("\"enabled\":false"));
        assert!(
            json.contains("a)weird\\\"name"),
            "paths with quotes must be escaped: {json}"
        );

        let repaired = vec![RepairOutcome {
            path: PathBuf::from("/watch/a"),
            kind: VerdictKind::Duplicate,
            action: RepairAction::RemovedDuplicate {
                target: PathBuf::from("../cold/a"),
            },
        }];
        let json = report.to_json(Some(&repaired));
        assert!(json.contains("\"removed-duplicate\":1"));
        assert!(json.contains("\"enabled\":true"));
    }

    #[test]
    fn the_json_scrub_section_matches_the_readable_line_and_nulls_without_a_catalog() {
        let scrub = ScrubSection {
            locations: 7,
            verified: 4,
            never_scrubbed: 2,
            damaged: 1,
        };

        // The JSON object and the readable line are the same numbers from the same struct:
        // a consumer and a human must never be told different things about the same catalog.
        let report = AuditReport {
            watch: PathBuf::from("/watch"),
            dests: vec![PathBuf::from("/cold")],
            source: AuditSource::Catalog {
                path: PathBuf::from("/watch/.just_cache-catalog.sqlite"),
            },
            scanned: 1,
            healthy: 0,
            tiers: None,
            scrub: Some(scrub),
            catalog_only: None,
            findings: Vec::new(),
        };
        let json = report.to_json(None);
        assert!(
            json.contains(
                "\"scrub\":{\"locations\":7,\"verified\":4,\"never-scrubbed\":2,\"damaged\":1}"
            ),
            "{json}"
        );
        // The readable line is the same struct rendered for a human: one source, two forms.
        let readable = scrub.summary_lines().join("\n");
        assert!(
            readable.contains("scrub: 7 location(s): 4 verified, 2 never scrubbed, 1 damaged"),
            "{readable}"
        );

        // No catalog answered: `null`, not four zeroes. A zeroed object reads as
        // "everything verified", which a walk-based audit never established.
        let walk = report.with_scrub(None);
        assert!(
            walk.to_json(None).contains("\"scrub\":null"),
            "{}",
            walk.to_json(None)
        );
    }

    #[test]
    fn the_copy_floor_is_the_one_the_schema_can_express() {
        // A configurable per-tier floor needs a schema column the catalog does not have;
        // until then the only honest floor is "at least one copy survives". Pinning it here
        // makes a later change to that number a deliberate act, not an accident.
        assert_eq!(COPY_FLOOR, 1);
    }

    #[test]
    fn catalog_findings_render_their_tier_and_digest_and_name_their_source() {
        let report = AuditReport {
            watch: PathBuf::from("/watch"),
            dests: vec![PathBuf::from("/cold")],
            source: AuditSource::Catalog {
                path: PathBuf::from("/watch/.just_cache-catalog.sqlite"),
            },
            scanned: 3,
            healthy: 0,
            tiers: None,
            scrub: None,
            catalog_only: None,
            findings: vec![
                Finding {
                    path: PathBuf::from("/cold/a.bin"),
                    relative: PathBuf::from("a.bin"),
                    cold_copy: None,
                    verdict: Verdict::ChecksumMismatch {
                        tier: "/cold".to_string(),
                        key: "a.bin".to_string(),
                        expected: "aaaa".to_string(),
                        found: "bbbb".to_string(),
                    },
                },
                Finding {
                    path: PathBuf::from("/watch/b.bin"),
                    relative: PathBuf::from("b.bin"),
                    cold_copy: None,
                    verdict: Verdict::NameVanished {
                        object: "cccc".to_string(),
                    },
                },
                Finding {
                    path: PathBuf::from("dddd"),
                    relative: PathBuf::new(),
                    cold_copy: None,
                    verdict: Verdict::CopyFloor {
                        object: "dddd".to_string(),
                        copies: 0,
                    },
                },
            ],
        };

        let summary = report.summary_lines(10).join("\n");
        assert!(summary.contains("from catalog"), "{summary}");
        assert!(summary.contains("checksum-mismatch: 1"), "{summary}");
        assert!(summary.contains("recorded aaaa, found bbbb"), "{summary}");
        assert!(summary.contains("name-vanished: 1"), "{summary}");
        assert!(summary.contains("floor is 1"), "{summary}");

        let json = report.to_json(None);
        assert!(json.contains("\"source\":\"catalog\""), "{json}");
        assert!(json.contains("\"checksum-mismatch\":1"), "{json}");
        assert!(json.contains(".just_cache-catalog.sqlite"), "{json}");
    }

    #[test]
    fn the_filesystem_only_set_is_exactly_every_kind_that_needs_a_tree() {
        // The findings a catalog-only audit can make, from rows alone. Everything else must
        // be in `filesystem_only`; anything missing from both would be a kind the mode
        // silently never reports, and anything in both would be a contradiction.
        let producible = [
            VerdictKind::Healthy,
            VerdictKind::MalformedCatalog,
            VerdictKind::UnderReplicated,
            VerdictKind::DamagedCopy,
        ];
        for kind in VerdictKind::ALL {
            let needs_tree = !producible.contains(&kind);
            assert_eq!(
                VerdictKind::filesystem_only().contains(&kind),
                needs_tree,
                "{kind:?} is on the wrong side of the filesystem boundary"
            );
        }
        // And the unchecked list is never empty: a catalog-only audit always has something
        // it could not check, or it would not be a catalog-only audit.
        assert_eq!(VerdictKind::filesystem_only().len(), 11);
    }

    #[test]
    fn a_catalog_only_report_counts_only_what_it_checked_and_names_the_rest() {
        let report = AuditReport {
            watch: PathBuf::from("/gone"),
            dests: vec![PathBuf::from("/gone-cold")],
            source: AuditSource::CatalogOnly {
                path: PathBuf::from("/catalog.sqlite"),
            },
            scanned: 4,
            healthy: 3,
            tiers: None,
            scrub: None,
            catalog_only: Some(CatalogOnlySection {
                objects: 2,
                names: 2,
                locations: 4,
                floors: vec![("/cold".to_string(), 2)],
                damaged: 1,
                scrub: crate::catalog::ScrubSummary {
                    locations: 4,
                    verified: 1,
                    never_scrubbed: 3,
                    damaged: 1,
                },
                unchecked: VerdictKind::filesystem_only().to_vec(),
            }),
            findings: vec![Finding {
                path: PathBuf::from("aaaa"),
                relative: PathBuf::new(),
                cold_copy: None,
                verdict: Verdict::UnderReplicated {
                    object: "aaaa".to_string(),
                    copies: 1,
                    floor: 2,
                },
            }],
        };

        let summary = report.summary_lines(10).join("\n");
        // The recorded state is reported...
        assert!(
            summary.contains("recorded state: 2 object(s), 2 name(s), 4 location(s)"),
            "{summary}"
        );
        assert!(summary.contains("recorded floors: /cold=2"), "{summary}");
        assert!(summary.contains("damage marks: 1"), "{summary}");
        assert!(summary.contains("4 location(s): 1 verified"), "{summary}");
        assert!(summary.contains("under-replicated: 1"), "{summary}");
        // ...and the classes it could not check are named, not left as a misleading zero.
        assert!(
            summary.contains("cannot be checked without the tree: missing-copy, checksum-mismatch"),
            "{summary}"
        );
        assert!(
            !summary.contains("missing-copy: 0"),
            "an unchecked class must not read as an explicit zero: {summary}"
        );

        let json = report.to_json(None);
        assert!(json.contains("\"source\":\"catalog-only\""), "{json}");
        assert!(json.contains("\"unchecked\":[\"missing-copy\""), "{json}");
        assert!(json.contains("\"no-filesystem\":{\"objects\":2"), "{json}");
        assert!(
            json.contains("\"floors\":[{\"tier\":\"/cold\",\"copies\":2}]"),
            "{json}"
        );
    }
}

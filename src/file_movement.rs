//! Usage tracking and the policy that decides which files count as "cold".

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::disk_management::{self, DiskError, FileEntry, MoveOutcome};
use crate::journal::Journal;
use crate::opened::{Guards, InUse};
use crate::scope::{Rejected, Scope};

/// What we know about one tracked file.
#[derive(Debug, Clone)]
struct UsageState {
    /// How many times we have watched this file's access stamp advance since the
    /// process started. This is real usage we observed, not just "the file exists".
    observed_accesses: u64,
    /// Most recent access stamp seen for the file, from the filesystem or from us.
    last_access: SystemTime,
}

/// Tracks how genuinely used each file is.
///
/// Two independent signals are kept per path:
///
/// 1. the access time reported by the filesystem (atime, or mtime where atime is
///    unavailable), which reflects every reader since the file was written; and
/// 2. accesses this process itself witnessed — each time a re-scan sees the stamp move
///    forward, that is one more use.
///
/// Only signal 1 is trusted for the initial decision, because signal 2 starts at zero
/// on every launch; signal 2 exists so a busy run pins files it just watched being read.
#[derive(Debug, Default)]
pub struct UsageTracker {
    states: HashMap<PathBuf, UsageState>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one scan's view of a file into the tracker.
    pub fn observe(&mut self, entry: &FileEntry) {
        let state = self
            .states
            .entry(entry.path.clone())
            .or_insert_with(|| UsageState {
                observed_accesses: 0,
                last_access: entry.last_access,
            });
        if entry.last_access > state.last_access {
            state.observed_accesses += 1;
        }
        state.last_access = state.last_access.max(entry.last_access);
    }

    /// Accesses witnessed during this run for `path`.
    pub fn observed_accesses(&self, path: &Path) -> u64 {
        self.states
            .get(path)
            .map(|state| state.observed_accesses)
            .unwrap_or(0)
    }

    /// Best known last-use stamp for `path`, falling back to `fallback` when the file
    /// has not been seen (or was seen with an older stamp).
    pub fn last_access(&self, path: &Path, fallback: SystemTime) -> SystemTime {
        match self.states.get(path) {
            Some(state) => state.last_access.max(fallback),
            None => fallback,
        }
    }

    /// Forget paths that are no longer in the watched tree (moved, deleted, replaced by
    /// a symlink) so the map cannot grow without bound on a long-running process.
    pub fn retain_present(&mut self, entries: &[FileEntry]) {
        let mut live: HashMap<&Path, bool> = HashMap::with_capacity(entries.len());
        for entry in entries {
            if !entry.is_symlink {
                live.insert(entry.path.as_path(), true);
            }
        }
        self.states
            .retain(|path, _| live.contains_key(path.as_path()));
    }

    pub fn tracked_paths(&self) -> usize {
        self.states.len()
    }
}

/// How aggressive a sweep should be.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Only files whose last use is at least this old are eligible.
    pub min_idle: Duration,
    /// Files accessed this many times during the run are pinned (never moved).
    pub observed_access_pin: u64,
    /// Upper bound on files moved per destination, per sweep.
    pub limit: usize,
    /// Report what would happen without touching anything.
    pub dry_run: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            min_idle: Duration::from_secs(30 * 24 * 60 * 60),
            observed_access_pin: 1,
            limit: 10,
            dry_run: false,
        }
    }
}

/// Why a file was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Outside the configured scope: not this tool's file to move.
    OutOfScope,
    TooSmall {
        size: u64,
        min: u64,
    },
    TooLarge {
        size: u64,
        max: u64,
    },
    IdleFor(Duration),
    RecentlyAccessed(u64),
    BeyondLimit,
    EmptyFile,
    /// A process has the file open right now.
    OpenElsewhere,
    /// Moving it would split a hardlinked pair across filesystems.
    Hardlinked {
        links: u64,
    },
}

impl From<InUse> for SkipReason {
    fn from(in_use: InUse) -> Self {
        match in_use {
            InUse::OpenElsewhere => SkipReason::OpenElsewhere,
            InUse::Hardlinked { links } => SkipReason::Hardlinked { links },
        }
    }
}

impl From<Rejected> for SkipReason {
    fn from(rejected: Rejected) -> Self {
        match rejected {
            Rejected::OutOfScope => SkipReason::OutOfScope,
            Rejected::TooSmall { size, min } => SkipReason::TooSmall { size, min },
            Rejected::TooLarge { size, max } => SkipReason::TooLarge { size, max },
        }
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::OutOfScope => write!(f, "outside the configured scope"),
            SkipReason::TooSmall { size, min } => write!(
                f,
                "below the {} size floor ({})",
                crate::scope::human_bytes(*min),
                crate::scope::human_bytes(*size)
            ),
            SkipReason::TooLarge { size, max } => write!(
                f,
                "above the {} size ceiling ({})",
                crate::scope::human_bytes(*max),
                crate::scope::human_bytes(*size)
            ),
            SkipReason::IdleFor(d) => write!(f, "idle for only {}h", d.as_secs() / 3600),
            SkipReason::RecentlyAccessed(n) => write!(f, "accessed {n}x this run"),
            SkipReason::BeyondLimit => write!(f, "over this sweep's move limit"),
            SkipReason::EmptyFile => write!(f, "empty file, nothing to reclaim"),
            SkipReason::OpenElsewhere => write!(f, "open by another process"),
            SkipReason::Hardlinked { links } => {
                write!(f, "hardlinked elsewhere ({links} links)")
            }
        }
    }
}

/// Everything a sweep decides with, in one place.
///
/// These four arrived one at a time — configuration, then eligibility, then live state,
/// then the record of what is in flight — and the parameter list grew with each. Grouping
/// them keeps call sites readable and, more usefully, names what a sweep actually depends
/// on: how aggressive it may be, what it may touch, what is currently in use, and what it
/// is in the middle of.
pub struct MoveContext<'a> {
    pub policy: &'a Policy,
    pub scope: &'a Scope,
    pub guards: &'a Guards,
    pub journal: &'a mut Journal,
}

/// The files a sweep would move, oldest use first, capped at [`Policy::limit`].
///
/// Already-migrated symlinks and non-regular files are excluded; the reasons for the
/// remaining exclusions are returned alongside so the run can explain itself. Scope
/// (§[`crate::scope`]) is checked first, so a file this tool is not allowed to manage
/// can never be picked up by a policy that later grows more eager.
pub fn select_candidates<'a>(
    entries: &'a [FileEntry],
    tracker: &UsageTracker,
    context: &MoveContext<'_>,
    now: SystemTime,
) -> (Vec<&'a FileEntry>, Vec<(&'a FileEntry, SkipReason)>) {
    let (policy, scope, guards) = (context.policy, context.scope, context.guards);
    let mut eager: Vec<(&FileEntry, SystemTime)> = Vec::new();
    let mut skipped: Vec<(&FileEntry, SkipReason)> = Vec::new();

    for entry in entries {
        if entry.is_symlink {
            continue;
        }
        if let Err(rejected) = scope.allows(entry) {
            skipped.push((entry, SkipReason::from(rejected)));
            continue;
        }
        if entry.size == 0 {
            skipped.push((entry, SkipReason::EmptyFile));
            continue;
        }
        // Live state beats every heuristic below: a file something is using is not cold,
        // whatever its access time says, and a hardlinked file cannot be moved without
        // breaking the pair.
        if let Err(in_use) = guards.check(entry) {
            skipped.push((entry, SkipReason::from(in_use)));
            continue;
        }

        let last_access = tracker.last_access(&entry.path, entry.last_access);
        let idle = now.duration_since(last_access).unwrap_or(Duration::ZERO);

        // The pin is checked first: a file that was read while we were watching is
        // "in use", and that is the more useful thing to report than its idle time.
        let accesses = tracker.observed_accesses(&entry.path);
        if policy.observed_access_pin > 0 && accesses >= policy.observed_access_pin {
            skipped.push((entry, SkipReason::RecentlyAccessed(accesses)));
            continue;
        }
        if idle < policy.min_idle {
            skipped.push((entry, SkipReason::IdleFor(idle)));
            continue;
        }

        eager.push((entry, last_access));
    }

    eager.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.path.cmp(&b.0.path)));

    let mut candidates = Vec::new();
    for (index, (entry, _)) in eager.into_iter().enumerate() {
        if index >= policy.limit {
            skipped.push((entry, SkipReason::BeyondLimit));
            continue;
        }
        candidates.push(entry);
    }

    (candidates, skipped)
}

/// Result of the work attempted for a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOutcome {
    /// Would be moved; only produced in dry-run mode.
    Planned,
    Moved,
    /// Destination already had the bytes; the symlink was (re)created.
    LinkedExisting,
    AlreadyLinked,
    /// The destination tier is full. Routine on a disk you keep filled, so this is
    /// reported separately from a failure.
    NoRoom,
    /// Fewer than the requested copies verified, so the source was deliberately kept.
    /// This is not a failure of the mover — it is the floor doing its job — but it is
    /// data that is not as durable as asked, so a sweep treats it as a finding.
    UnderReplicated {
        verified: usize,
        floor: usize,
    },
    Skipped(SkipReason),
    Failed(String),
}

/// Per-file record of a sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRecord {
    pub path: PathBuf,
    pub destination: Option<PathBuf>,
    pub outcome: FileOutcome,
    /// Size of the file at the time it was considered, so byte totals do not need a
    /// second look at a tree that has already changed.
    pub size: u64,
}

/// Everything a sweep did, in the order it did it.
#[derive(Debug, Default, Clone)]
pub struct MigrationReport {
    pub records: Vec<MigrationRecord>,
    /// Per-file replication detail, for a caller that records copies in a catalog. Empty
    /// on the single-copy path and in a dry run. The placements carry the digest each
    /// copy verified to, which is what a catalog needs to store.
    pub replication: Vec<ReplicationDetail>,
}

/// One candidate's replication placements, kept alongside the movement record so the CLI
/// can write them into the catalog without the policy layer depending on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationDetail {
    pub path: PathBuf,
    /// Hex-encoded BLAKE3 of the object, when it could be computed.
    pub object: Option<String>,
    pub placements: Vec<crate::replication::ReplicaPlacement>,
}

impl MigrationReport {
    pub fn count(&self, wanted: fn(&FileOutcome) -> bool) -> usize {
        self.records
            .iter()
            .filter(|record| wanted(&record.outcome))
            .count()
    }

    pub fn moved(&self) -> usize {
        self.count(|outcome| matches!(outcome, FileOutcome::Moved))
    }

    pub fn linked_existing(&self) -> usize {
        self.count(|outcome| matches!(outcome, FileOutcome::LinkedExisting))
    }

    pub fn planned(&self) -> usize {
        self.count(|outcome| matches!(outcome, FileOutcome::Planned))
    }

    pub fn failed(&self) -> usize {
        self.count(|outcome| matches!(outcome, FileOutcome::Failed(_)))
    }

    /// Files a full destination tier could not take this sweep.
    pub fn waiting_for_room(&self) -> usize {
        self.count(|outcome| matches!(outcome, FileOutcome::NoRoom))
    }

    /// Objects left below their copy floor this sweep: fewer copies verified than were
    /// asked for, so the source was kept. Counted as a finding, because the whole point
    /// of a floor is that silence about it means it was met.
    pub fn under_replicated(&self) -> usize {
        self.count(|outcome| matches!(outcome, FileOutcome::UnderReplicated { .. }))
    }

    /// Files left alone because something is using them: open, or hardlinked.
    pub fn in_use(&self) -> usize {
        self.count(|outcome| {
            matches!(
                outcome,
                FileOutcome::Skipped(SkipReason::OpenElsewhere)
                    | FileOutcome::Skipped(SkipReason::Hardlinked { .. })
            )
        })
    }

    /// Files left alone because of scope or the size window — i.e. not this tool's to
    /// move in the first place, as opposed to "not cold yet".
    pub fn excluded(&self) -> usize {
        self.count(|outcome| {
            matches!(
                outcome,
                FileOutcome::Skipped(SkipReason::OutOfScope)
                    | FileOutcome::Skipped(SkipReason::TooSmall { .. })
                    | FileOutcome::Skipped(SkipReason::TooLarge { .. })
            )
        })
    }

    pub fn bytes_moved(&self) -> u64 {
        self.records
            .iter()
            .filter(|record| {
                matches!(
                    record.outcome,
                    FileOutcome::Moved | FileOutcome::LinkedExisting
                )
            })
            .map(|record| record.size)
            .sum()
    }

    /// Paths this sweep moved off the watched tree (used to keep the next disk tier
    /// from reconsidering them).
    pub fn migrated_paths(&self) -> Vec<PathBuf> {
        self.records
            .iter()
            .filter(|record| {
                matches!(
                    record.outcome,
                    FileOutcome::Moved | FileOutcome::LinkedExisting | FileOutcome::Planned
                )
            })
            .map(|record| record.path.clone())
            .collect()
    }

    /// One line per record, for the log.
    pub fn lines(&self) -> Vec<String> {
        self.records
            .iter()
            .map(|record| {
                let where_to = record
                    .destination
                    .as_ref()
                    .map(|dest| format!(" -> {}", dest.display()))
                    .unwrap_or_default();
                let what = match &record.outcome {
                    FileOutcome::Planned => "would move".to_string(),
                    FileOutcome::Moved => "moved".to_string(),
                    FileOutcome::LinkedExisting => "linked (already on disk)".to_string(),
                    FileOutcome::AlreadyLinked => "already a symlink".to_string(),
                    FileOutcome::NoRoom => "waiting for room".to_string(),
                    FileOutcome::UnderReplicated { verified, floor } => {
                        format!("UNDER-REPLICATED ({verified}/{floor} copies; source kept)")
                    }
                    FileOutcome::Skipped(reason) => format!("skipped: {reason}"),
                    FileOutcome::Failed(err) => format!("FAILED: {err}"),
                };
                format!("{} {}{}", what, record.path.display(), where_to)
            })
            .collect()
    }
}

/// Move the cold files of `entries` onto a cold tier and symlink them back.
///
/// `choose_destination` picks the tier for each file, given the tier this call is
/// filling; returning `Ok(None)` marks the file as waiting for room rather than
/// failing it.
///
/// Scope is re-checked here, immediately before any bytes move, in addition to the
/// check during candidate selection: belt and braces, so a policy that becomes dynamic
/// later cannot move something the scope protects.
pub fn migrate_least_used<F>(
    entries: &[FileEntry],
    tracker: &UsageTracker,
    context: &mut MoveContext<'_>,
    now: SystemTime,
    mut choose_destination: F,
) -> MigrationReport
where
    F: FnMut(&FileEntry) -> Result<Option<PathBuf>, DiskError>,
{
    let (candidates, skipped) = select_candidates(entries, tracker, context, now);
    // The copy fields are read out before the journal is borrowed mutably below: the
    // references are `Copy`, so this does not keep the context borrowed.
    let (policy, scope, guards) = (context.policy, context.scope, context.guards);
    let journal: &mut Journal = context.journal;
    let mut report = MigrationReport::default();

    for (entry, reason) in skipped {
        report.records.push(MigrationRecord {
            path: entry.path.clone(),
            destination: None,
            outcome: FileOutcome::Skipped(reason),
            size: entry.size,
        });
    }

    for entry in candidates {
        // Re-checked here, immediately before bytes move: the sweep's snapshot cannot see
        // a descriptor opened after it was taken, so this second open-file scan is the one
        // that catches a file opened during the walk (invariant 3).
        if let Err(rejected) = scope.allows(entry) {
            report.records.push(MigrationRecord {
                path: entry.path.clone(),
                destination: None,
                outcome: FileOutcome::Skipped(SkipReason::from(rejected)),
                size: entry.size,
            });
            continue;
        }
        if let Err(in_use) = guards.recheck(entry) {
            report.records.push(MigrationRecord {
                path: entry.path.clone(),
                destination: None,
                outcome: FileOutcome::Skipped(SkipReason::from(in_use)),
                size: entry.size,
            });
            continue;
        }

        let destination = match choose_destination(entry) {
            Ok(Some(destination)) => destination,
            Ok(None) => {
                report.records.push(MigrationRecord {
                    path: entry.path.clone(),
                    destination: None,
                    outcome: FileOutcome::NoRoom,
                    size: entry.size,
                });
                continue;
            }
            Err(err) => {
                report.records.push(MigrationRecord {
                    path: entry.path.clone(),
                    destination: None,
                    outcome: FileOutcome::Failed(err.to_string()),
                    size: entry.size,
                });
                continue;
            }
        };

        let outcome = if policy.dry_run {
            FileOutcome::Planned
        } else {
            // The intent is recorded *before* anything moves and flushed to disk, because
            // the window this covers is the machine dying mid-move. Without it, a crash
            // between removing the source and creating the symlink leaves bytes on a cold
            // tier that nothing in the namespace points at — recoverable only if we wrote
            // down that we were about to do it.
            let target = destination.join(&entry.relative);
            let journalled = journal.intent(&entry.relative, &target, entry.size).is_ok();
            let moved = match disk_management::move_file_with_symlink(&destination, entry) {
                Ok(MoveOutcome::Moved) => FileOutcome::Moved,
                Ok(MoveOutcome::LinkedExisting) => FileOutcome::LinkedExisting,
                Ok(MoveOutcome::AlreadyLinked) => FileOutcome::AlreadyLinked,
                Err(err) => FileOutcome::Failed(err.to_string()),
            };
            // Only a completed move clears the record. A failure leaves it and the sweep
            // ends, so the journal keeps describing what is on disk.
            if journalled
                && matches!(
                    moved,
                    FileOutcome::Moved | FileOutcome::LinkedExisting | FileOutcome::AlreadyLinked
                )
            {
                // Cleared, not marked: the symlink now at this path is the evidence the
                // move finished, and a daemon that kept a line per move would grow a file
                // describing nothing.
                journal.forget(&entry.relative);
            }
            if !journalled {
                // Reported, not swallowed: an unwritable journal means the crash window is
                // unguarded, and the user is the only one who can fix that.
                eprintln!(
                    "just_cache: cannot record the move of {} in the journal; a crash during \
                     this move would not be recoverable",
                    entry.path.display()
                );
            }
            moved
        };

        report.records.push(MigrationRecord {
            path: entry.path.clone(),
            destination: Some(destination.join(&entry.relative)),
            outcome,
            size: entry.size,
        });
    }

    report
}

/// Offload each candidate across `floor` distinct destinations, then retire the source.
///
/// This is the replication path behind `sweep --copies N` (N > 1). It differs from
/// [`migrate_least_used`] in the one way that matters:
///
/// * every destination is tried for the *same* file, not the first that has room, until
///   `floor` copies have their digest verified;
/// * the source is removed **only** when the floor is met
///   ([`crate::replication::ReplicationOutcome::meets_floor`]). Below the floor the file
///   is left exactly where it was and reported as [`FileOutcome::UnderReplicated`] — the
///   mover never trades the last copy away to satisfy a number;
/// * the journal record names the copy the move actually retires to — the first that
///   verified — rather than the destination tried first, so a crash in the retire window
///   sends recovery to a copy that exists instead of reporting data loss over a path that
///   never held one.
///
/// With `floor == 1` this collapses to the old behaviour, which is why the default sweep
/// still calls `migrate_least_used` and only an explicit `--copies N` reaches here.
pub fn migrate_replicated(
    entries: &[FileEntry],
    tracker: &UsageTracker,
    context: &mut MoveContext<'_>,
    now: SystemTime,
    dests: &[PathBuf],
    floor: usize,
    min_free: u64,
) -> MigrationReport {
    let (candidates, skipped) = select_candidates(entries, tracker, context, now);
    let (policy, scope, guards) = (context.policy, context.scope, context.guards);
    let journal: &mut Journal = context.journal;
    let mut report = MigrationReport::default();

    for (entry, reason) in skipped {
        report.records.push(MigrationRecord {
            path: entry.path.clone(),
            destination: None,
            outcome: FileOutcome::Skipped(reason),
            size: entry.size,
        });
    }

    for entry in candidates {
        // Same belt-and-braces re-check as the single-copy path: a fresh open-file scan
        // immediately before the copy, because the sweep's snapshot predates the walk.
        if let Err(rejected) = scope.allows(entry) {
            report.records.push(MigrationRecord {
                path: entry.path.clone(),
                destination: None,
                outcome: FileOutcome::Skipped(SkipReason::from(rejected)),
                size: entry.size,
            });
            continue;
        }
        if let Err(in_use) = guards.recheck(entry) {
            report.records.push(MigrationRecord {
                path: entry.path.clone(),
                destination: None,
                outcome: FileOutcome::Skipped(SkipReason::from(in_use)),
                size: entry.size,
            });
            continue;
        }

        // The intent names the first destination before anything moves (invariant 5). On
        // this path the copy step never touches the source, so the record only has to be
        // right for the window *after* the copies exist — where the destination the move
        // actually retires to is whichever copy verified, which need not be the first one
        // tried. It is corrected to that copy below, before the source is removed.
        let intent_destination = dests.first().map(|dest| dest.join(&entry.relative));

        if policy.dry_run {
            report.records.push(MigrationRecord {
                path: entry.path.clone(),
                destination: intent_destination,
                outcome: FileOutcome::Planned,
                size: entry.size,
            });
            continue;
        }

        let mut journalled = match &intent_destination {
            Some(target) => journal.intent(&entry.relative, target, entry.size).is_ok(),
            None => false,
        };

        let outcome = crate::replication::replicate(entry, dests, floor, min_free);

        // The copy recovery would link to is the one that verified, not necessarily the
        // destination the intent guessed at. Record it now, *before* the source is
        // removed: a record left naming a destination that never held a copy sends
        // recovery to an empty path, so a crash in this window is reported as DATA LOST
        // while verified copies exist (#78).
        let retire_to = outcome.primary().map(Path::to_path_buf);
        if let Some(target) = &retire_to {
            if intent_destination.as_deref() != Some(target.as_path()) {
                journalled = journal.intent(&entry.relative, target, entry.size).is_ok();
            }
        }

        let result = if outcome.meets_floor() {
            // The floor is met, so the source may now be retired. Removing it first and
            // linking second follows the single-copy mover's order: if the link step
            // fails, the bytes are safe on every verified replica and the journal record
            // still describes the move, so the next run's recovery creates the link.
            let primary_path = retire_to
                .as_deref()
                .expect("a met floor always has at least one verified replica");
            if !journalled {
                // Retiring over a record that does not name the destination being relied
                // on is exactly the unrecoverable window this fix closes, so keep the
                // source instead: the journal could not name the copy a crash would need.
                eprintln!(
                    "just_cache: cannot record the replicated move of {} in the journal; a \
                     crash during this move would not be recoverable, so the source is kept",
                    entry.path.display()
                );
                FileOutcome::Failed(
                    "the journal could not record the destination the move retires to".to_string(),
                )
            } else {
                match retire_source(&entry.path, primary_path) {
                    Ok(()) => {
                        journal.forget(&entry.relative);
                        FileOutcome::Moved
                    }
                    Err(err) => FileOutcome::Failed(err.to_string()),
                }
            }
        } else {
            // Belt and braces in the other direction: the source is not touched. This is
            // the branch that keeps the mover from deleting the last copy.
            FileOutcome::UnderReplicated {
                verified: outcome.verified,
                floor,
            }
        };

        if let FileOutcome::UnderReplicated { verified, floor } = &result {
            eprintln!(
                "just_cache: {} is under-replicated ({verified}/{floor} copies verified); \
                 the source is kept. {}",
                entry.path.display(),
                outcome.unmet_detail()
            );
        }

        report.replication.push(ReplicationDetail {
            path: entry.path.clone(),
            object: outcome.digest.clone(),
            placements: outcome.replicas.clone(),
        });

        report.records.push(MigrationRecord {
            path: entry.path.clone(),
            // The copy the move relies on — the one it retired to, or the first that
            // verified — never the destination that was merely tried first (#78).
            destination: retire_to.or(intent_destination),
            outcome: result,
            size: entry.size,
        });
    }

    report
}

/// Replace a source file with a symlink to a verified replica.
///
/// The removal happens second-to-last and the link last, exactly as
/// [`disk_management::move_file_with_symlink`] does it, so journal recovery sees the same
/// shape it already knows how to repair.
fn retire_source(source: &Path, primary: &Path) -> Result<(), DiskError> {
    fs::remove_file(source).map_err(|err| DiskError::MoveError {
        from: source.to_path_buf(),
        to: primary.to_path_buf(),
        source: err,
    })?;
    disk_management::link_into_place(primary, source)
}

/// Keep the movement log in one place so the wording stays consistent across runs.
pub fn log_file_movement(record: &MigrationRecord) {
    match &record.outcome {
        FileOutcome::Moved | FileOutcome::LinkedExisting => {
            if let Some(destination) = &record.destination {
                println!("{} > {}", record.path.display(), destination.display());
            }
        }
        FileOutcome::Failed(err) => {
            eprintln!("error: {}: {err}", record.path.display());
        }
        // A file kept because its copies did not verify is something the user has to
        // see, even without -v: it is the difference between "offloaded" and "offloaded
        // to as many disks as you asked for".
        FileOutcome::UnderReplicated { verified, floor } => {
            eprintln!(
                "under-replicated: {} ({verified}/{floor} copies verified; source kept)",
                record.path.display()
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn entry(path: &str, last_access_secs_ago: u64, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            relative: Path::new(path)
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_default(),
            size,
            allocated: size,
            last_access: SystemTime::now() - Duration::from_secs(last_access_secs_ago),
            is_symlink: false,
        }
    }

    /// A journal in its own temporary directory, so no test writes to a shared path.
    fn test_journal() -> (tempfile::TempDir, Journal) {
        let tmp = tempfile::tempdir().unwrap();
        let journal = Journal::at(tmp.path().join("journal")).unwrap();
        (tmp, journal)
    }

    #[test]
    fn only_files_idle_past_the_threshold_are_candidates() {
        let (_tmp, mut journal) = test_journal();
        let now = SystemTime::now();
        let entries = vec![
            entry("/watch/cold.bin", 90 * 86_400, 10),
            entry("/watch/warm.bin", 3_600, 10),
        ];
        let policy = Policy {
            min_idle: Duration::from_secs(30 * 86_400),
            limit: 10,
            ..Policy::default()
        };

        let (candidates, skipped) = select_candidates(
            &entries,
            &UsageTracker::new(),
            &MoveContext {
                policy: &policy,
                scope: &Scope::everything(),
                guards: &Guards::permissive(),
                journal: &mut journal,
            },
            now,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, Path::new("/watch/cold.bin"));
        assert_eq!(skipped.len(), 1);
        assert!(matches!(skipped[0].1, SkipReason::IdleFor(_)));
    }

    #[test]
    fn oldest_files_win_when_over_the_limit() {
        let (_tmp, mut journal) = test_journal();
        let now = SystemTime::now();
        let entries = vec![
            entry("/watch/b.bin", 10 * 86_400, 10),
            entry("/watch/a.bin", 40 * 86_400, 10),
            entry("/watch/c.bin", 20 * 86_400, 10),
        ];
        let policy = Policy {
            min_idle: Duration::from_secs(86_400),
            limit: 2,
            ..Policy::default()
        };

        let (candidates, skipped) = select_candidates(
            &entries,
            &UsageTracker::new(),
            &MoveContext {
                policy: &policy,
                scope: &Scope::everything(),
                guards: &Guards::permissive(),
                journal: &mut journal,
            },
            now,
        );

        let picked: Vec<_> = candidates.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            picked,
            vec![PathBuf::from("/watch/a.bin"), PathBuf::from("/watch/c.bin")]
        );
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].1, SkipReason::BeyondLimit);
    }

    #[test]
    fn observed_accesses_pin_a_file_even_when_its_stamp_is_old() {
        let now = SystemTime::now();
        let old = entry("/watch/used.bin", 90 * 86_400, 10);
        let entries = vec![old.clone()];

        let mut tracker = UsageTracker::new();
        tracker.observe(&old);
        // the file was read while we were running: its stamp jumped forward
        let mut touched = old.clone();
        touched.last_access = now;
        tracker.observe(&touched);

        let policy = Policy {
            min_idle: Duration::from_secs(86_400),
            observed_access_pin: 1,
            limit: 10,
            ..Policy::default()
        };

        let (_tmp, mut journal) = test_journal();
        let (candidates, skipped) = select_candidates(
            &entries,
            &tracker,
            &MoveContext {
                policy: &policy,
                scope: &Scope::everything(),
                guards: &Guards::permissive(),
                journal: &mut journal,
            },
            now,
        );
        assert!(candidates.is_empty());
        assert_eq!(skipped[0].1, SkipReason::RecentlyAccessed(1));
        assert_eq!(tracker.observed_accesses(Path::new("/watch/used.bin")), 1);
    }

    #[test]
    fn tracker_forgets_paths_that_left_the_tree() {
        let mut tracker = UsageTracker::new();
        let gone = entry("/watch/gone.bin", 100, 1);
        let here = entry("/watch/here.bin", 100, 1);
        tracker.observe(&gone);
        tracker.observe(&here);
        assert_eq!(tracker.tracked_paths(), 2);

        tracker.retain_present(std::slice::from_ref(&here));
        assert_eq!(tracker.tracked_paths(), 1);
        assert_eq!(tracker.observed_accesses(Path::new("/watch/gone.bin")), 0);
    }

    #[test]
    fn dry_run_reports_without_touching_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("watch");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(watch.join("cold.bin"), b"payload").unwrap();

        let entries = disk_management::list_files_recursive(&watch).unwrap();
        let policy = Policy {
            min_idle: Duration::ZERO,
            observed_access_pin: 0,
            limit: 10,
            dry_run: true,
        };
        let mut journal = Journal::at(tmp.path().join("journal")).unwrap();

        let report = migrate_least_used(
            &entries,
            &UsageTracker::new(),
            &mut MoveContext {
                policy: &policy,
                scope: &Scope::everything(),
                guards: &Guards::permissive(),
                journal: &mut journal,
            },
            SystemTime::now(),
            |_| Ok(Some(dest.clone())),
        );

        assert_eq!(report.planned(), 1);
        assert_eq!(report.moved(), 0);
        assert!(watch.join("cold.bin").is_file(), "source must be untouched");
        assert!(!dest.join("cold.bin").exists());
    }

    /// The window the mover's re-check exists for: the sweep snapshot is taken, a
    /// descriptor then opens, and the file is already past selection. The single-copy
    /// mover must refuse it at the pre-move re-check; reverting that call to the
    /// snapshot-only `check` makes this test move the file and fail.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_descriptor_opened_mid_sweep_is_caught_before_the_move() {
        use crate::opened::{FileId, OpenFiles};

        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("watch");
        let dest = tmp.path().join("dest");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&dest).unwrap();
        let path = watch.join("opens-late.bin");
        fs::write(&path, b"payload").unwrap();

        // The sweep's snapshot, taken while nothing holds the file.
        let guards = Guards::new(OpenFiles::snapshot(), true);

        // A real descriptor opens after it, held by another process.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exec 3< '{}'; sleep 20", path.display()))
            .spawn()
            .expect("spawn a holder process");
        let target = FileId::of(&path).unwrap();
        let mut seen = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if OpenFiles::snapshot().contains(target) {
                seen = true;
                break;
            }
        }

        let entries = disk_management::list_files_recursive(&watch).unwrap();
        let policy = Policy {
            min_idle: Duration::ZERO,
            observed_access_pin: 0,
            limit: 10,
            dry_run: false,
        };
        let mut journal = Journal::at(tmp.path().join("journal")).unwrap();

        let report = migrate_least_used(
            &entries,
            &UsageTracker::new(),
            &mut MoveContext {
                policy: &policy,
                scope: &Scope::everything(),
                guards: &guards,
                journal: &mut journal,
            },
            SystemTime::now(),
            |_| Ok(Some(dest.clone())),
        );

        let _ = child.kill();
        let _ = child.wait();

        assert!(seen, "the holder must be visible to a fresh scan");
        assert_eq!(
            report.in_use(),
            1,
            "the pre-move re-check must report the file as open"
        );
        assert_eq!(report.moved(), 0);
        assert!(path.is_file(), "the source must be left alone");
        assert!(!dest.join("opens-late.bin").exists());
    }
}

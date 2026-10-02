//! Usage tracking and the policy that decides which files count as "cold".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::disk_management::{self, DiskError, FileEntry, MoveOutcome};
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

/// The files a sweep would move, oldest use first, capped at [`Policy::limit`].
///
/// Already-migrated symlinks and non-regular files are excluded; the reasons for the
/// remaining exclusions are returned alongside so the run can explain itself. Scope
/// (§[`crate::scope`]) is checked first, so a file this tool is not allowed to manage
/// can never be picked up by a policy that later grows more eager.
pub fn select_candidates<'a>(
    entries: &'a [FileEntry],
    tracker: &UsageTracker,
    policy: &Policy,
    scope: &Scope,
    guards: &Guards,
    now: SystemTime,
) -> (Vec<&'a FileEntry>, Vec<(&'a FileEntry, SkipReason)>) {
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
    policy: &Policy,
    scope: &Scope,
    guards: &Guards,
    now: SystemTime,
    mut choose_destination: F,
) -> MigrationReport
where
    F: FnMut(&FileEntry) -> Result<Option<PathBuf>, DiskError>,
{
    let (candidates, skipped) = select_candidates(entries, tracker, policy, scope, guards, now);
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
        // Re-checked here, immediately before bytes move: a descriptor can be opened
        // between selection and the move, and on a busy box that window is real.
        if let Err(rejected) = scope.allows(entry) {
            report.records.push(MigrationRecord {
                path: entry.path.clone(),
                destination: None,
                outcome: FileOutcome::Skipped(SkipReason::from(rejected)),
                size: entry.size,
            });
            continue;
        }
        if let Err(in_use) = guards.check(entry) {
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
            match disk_management::move_file_with_symlink(&destination, entry) {
                Ok(MoveOutcome::Moved) => FileOutcome::Moved,
                Ok(MoveOutcome::LinkedExisting) => FileOutcome::LinkedExisting,
                Ok(MoveOutcome::AlreadyLinked) => FileOutcome::AlreadyLinked,
                Err(err) => FileOutcome::Failed(err.to_string()),
            }
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

    #[test]
    fn only_files_idle_past_the_threshold_are_candidates() {
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
            &policy,
            &Scope::everything(),
            &Guards::permissive(),
            now,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].path, Path::new("/watch/cold.bin"));
        assert_eq!(skipped.len(), 1);
        assert!(matches!(skipped[0].1, SkipReason::IdleFor(_)));
    }

    #[test]
    fn oldest_files_win_when_over_the_limit() {
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
            &policy,
            &Scope::everything(),
            &Guards::permissive(),
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

        let (candidates, skipped) = select_candidates(
            &entries,
            &tracker,
            &policy,
            &Scope::everything(),
            &Guards::permissive(),
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

        let report = migrate_least_used(
            &entries,
            &UsageTracker::new(),
            &policy,
            &Scope::everything(),
            &Guards::permissive(),
            SystemTime::now(),
            |_| Ok(Some(dest.clone())),
        );

        assert_eq!(report.planned(), 1);
        assert_eq!(report.moved(), 0);
        assert!(watch.join("cold.bin").is_file(), "source must be untouched");
        assert!(!dest.join("cold.bin").exists());
    }
}

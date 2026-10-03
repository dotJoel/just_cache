//! The operation journal: what the mover was in the middle of when it stopped.
//!
//! A move is three filesystem operations that cannot be made atomic together — the bytes
//! land on the cold tier, the source is removed, the symlink appears — and the window
//! between the last two is the one that hurts: the file is still on disk, but nothing in
//! the namespace points at it. The design calls this out as binding for every driver
//! (`docs/design.md` §7.1), and this is the symlink provider's answer.
//!
//! The journal holds **only unresolved intents**, and a completed move clears its record.
//! The symlink the move leaves behind is itself the evidence that it finished, so a second
//! "finished" record would be redundant — and worse than redundant: a long-running daemon
//! would accumulate one line per successful move, so the file would grow without bound while
//! describing nothing that needs doing, and a real in-flight record would be buried among
//! them.
//!
//! Nothing is replayed blindly either: recovery reads the filesystem and asks what actually
//! happened, because the journal only says what was *attempted*. A journal that overrode the
//! filesystem could talk a repair into deleting the only copy of something.
//!
//! Two rules shape every branch below:
//!
//! - **Recovery never deletes anything that might be the only copy.** Restoring a name is
//!   a symlink creation, which is reversible; deleting a partial that is the only surviving
//!   byte range of a file is not, so those are kept and reported instead.
//! - **A record survives only if something still needs a human.** Handled records are
//!   dropped, and so are ones the next sweep will resolve by itself (the mover's adoption
//!   path already checksums a same-size destination). Records describing impossible states
//!   — no source, no copy — are kept and reported every run, because nothing on disk
//!   remains for `audit` to find.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

/// Name of the journal inside the watched root. The walk skips anything starting with this
/// prefix, so the journal and any interrupted-run leftovers never become move candidates.
pub const JOURNAL_NAME: &str = ".just_cache-journal";

/// Prefix shared by the journal and by in-flight copies, for the walk to ignore.
pub const INTERNAL_PREFIX: &str = ".just_cache";

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("cannot read the journal at {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("journal {path} is damaged at line {line}: {detail}")]
    Damaged {
        path: PathBuf,
        line: usize,
        detail: String,
    },
    #[error("cannot write the journal at {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) relative: PathBuf,
    /// Where the bytes were headed. Absent on `linked` records, which carry no new
    /// information beyond "this one is done".
    pub(crate) destination: Option<PathBuf>,
    /// The size the move promised to write, used to tell a complete copy from a truncated
    /// one without hashing anything.
    pub(crate) size: u64,
    pub(crate) at: u64,
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

/// An append-only record of in-flight moves, kept beside the tree it describes.
#[derive(Debug)]
pub struct Journal {
    path: PathBuf,
    /// Latest record per relative path, in path order so compaction is deterministic.
    records: BTreeMap<PathBuf, Record>,
}

impl Journal {
    /// The journal that belongs to this watched root.
    pub fn in_tree(watch_root: &Path) -> Result<Self, JournalError> {
        Self::at(watch_root.join(JOURNAL_NAME))
    }

    /// Open (or start) a journal at an explicit path.
    ///
    /// A journal that cannot be parsed is *reported*, never guessed at: the caller is told
    /// which line is damaged so it can be looked at by hand. Continuing with an empty
    /// journal would be the dangerous option — it would look exactly like "nothing was in
    /// flight".
    pub fn at(path: PathBuf) -> Result<Self, JournalError> {
        let mut journal = Self {
            path,
            records: BTreeMap::new(),
        };
        journal.load()?;
        Ok(journal)
    }

    fn load(&mut self) -> Result<(), JournalError> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(JournalError::Read {
                    path: self.path.clone(),
                    source,
                })
            }
        };

        for (index, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record = parse_record(line).map_err(|detail| JournalError::Damaged {
                path: self.path.clone(),
                line: index + 1,
                detail,
            })?;
            self.records.insert(record.relative.clone(), record);
        }
        Ok(())
    }

    /// Path this journal is stored at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Note that a move of `relative` onto `destination` is about to start.
    pub fn intent(
        &mut self,
        relative: &Path,
        destination: &Path,
        size: u64,
    ) -> Result<(), JournalError> {
        let record = Record {
            relative: relative.to_path_buf(),
            destination: Some(destination.to_path_buf()),
            size,
            at: now_seconds(),
        };
        self.records.insert(relative.to_path_buf(), record.clone());
        self.append(&record)
    }

    /// Moves that were started and are not known to have finished, oldest first.
    pub(crate) fn unfinished(&self) -> Vec<Record> {
        let mut unfinished: Vec<Record> = self.records.values().cloned().collect();
        unfinished.sort_by_key(|record| (record.at, record.relative.clone()));
        unfinished
    }

    /// How many moves are still unaccounted for. Every record is an unfinished move: a
    /// finished one is forgotten, because the symlink it left behind says so better.
    pub fn unfinished_count(&self) -> usize {
        self.records.len()
    }

    /// Number of records currently held.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Forget a path entirely — it is finished, or the next sweep will settle it.
    pub fn forget(&mut self, relative: &Path) {
        self.records.remove(relative);
    }

    /// Keep only the given records, for the states that still need attention.
    fn retain_only(&mut self, keep: &[Record]) {
        self.records.clear();
        for record in keep {
            self.records.insert(record.relative.clone(), record.clone());
        }
    }

    /// Append one record and flush it to stable storage.
    ///
    /// The fsync is the whole point: a record that is only in the page cache when the
    /// machine dies is a record that was never written, and the window it was meant to
    /// cover is exactly the one where the power goes out mid-move.
    fn append(&self, record: &Record) -> Result<(), JournalError> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| JournalError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(encode_record(record).as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| JournalError::Write {
                path: self.path.clone(),
                source,
            })
    }

    /// Rewrite the journal with exactly the records it currently holds.
    ///
    /// Written to a sibling and renamed into place, so a crash during compaction leaves
    /// either the old journal or the new one — never a half-written file that reads as
    /// "nothing was in flight".
    pub fn compact(&self) -> Result<(), JournalError> {
        let temporary = self.path.with_extension("compacting");
        let mut body = String::new();
        for record in self.records.values() {
            body.push_str(&encode_record(record));
            body.push('\n');
        }

        let write = || -> io::Result<()> {
            let mut file = File::create(&temporary)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        };
        write().map_err(|source| JournalError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

/// What recovery found for one path, and what it did about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// The move had actually finished; the record was stale.
    AlreadyLinked,
    /// The name had vanished while the bytes were safely on the cold tier: the symlink was
    /// recreated, which is what makes the crash window survivable.
    NameRestored { destination: PathBuf },
    /// The bytes never left: nothing to do.
    NeverStarted,
    /// Both copies exist. Left exactly as they are, because the next sweep's adoption path
    /// already checksums a same-size destination and will either adopt or refuse it.
    LeftForTheSweep { destination: PathBuf },
    /// Both copies exist but disagree in size, so neither is safely removable.
    Refused {
        destination: PathBuf,
        detail: String,
    },
    /// No source and no complete copy: the only state that means real data loss.
    DataLost { destination: Option<PathBuf> },
    /// An in-flight partial beside a destination whose source is still intact, so the
    /// partial is ours and worthless.
    PartialRemoved { path: PathBuf },
    /// An in-flight partial we did not dare delete, because nothing else may be left.
    PartialKept { path: PathBuf },
    /// A record naming a path outside the watched tree, or a destination outside every
    /// `--dest` root. Attacker-controlled input, or a truncated journal: nothing was
    /// touched, and the record is kept so the operator can look at the line.
    RecordRejected { detail: String },
}

impl Recovery {
    /// One line for a human, in the same voice as the rest of the tool's output.
    pub fn describe(&self, relative: &Path) -> String {
        match self {
            Recovery::AlreadyLinked => format!(
                "  finished mid-flight, nothing to repair: {}",
                relative.display()
            ),
            Recovery::NameRestored { destination } => format!(
                "  restored the name {} -> {} (the bytes were already safe)",
                relative.display(),
                destination.display()
            ),
            Recovery::NeverStarted => {
                format!("  never started, nothing to repair: {}", relative.display())
            }
            Recovery::LeftForTheSweep { destination } => format!(
                "  both copies exist, left for the next sweep to verify: {} and {}",
                relative.display(),
                destination.display()
            ),
            Recovery::Refused {
                destination,
                detail,
            } => format!(
                "  REFUSED to touch {} and {}: {detail}",
                relative.display(),
                destination.display()
            ),
            Recovery::DataLost { destination } => match destination {
                Some(destination) => format!(
                    "  DATA LOST: {} is gone and {} holds no complete copy",
                    relative.display(),
                    destination.display()
                ),
                None => format!(
                    "  DATA LOST: {} is gone and no copy is recorded",
                    relative.display()
                ),
            },
            Recovery::PartialRemoved { path } => format!(
                "  removed an interrupted copy of {} ({})",
                relative.display(),
                path.display()
            ),
            Recovery::PartialKept { path } => format!(
                "  kept an interrupted copy {} because nothing else survived for {}",
                path.display(),
                relative.display()
            ),
            Recovery::RecordRejected { detail } => format!(
                "  REFUSED a journal record for {}: {detail}",
                relative.display()
            ),
        }
    }

    /// Whether this outcome deserves a line by default, without `-v`.
    ///
    /// Restoring a name is not an error, but it is never routine: it means a previous run
    /// died mid-move, and the file the user thought was there had vanished. Deleting an
    /// interrupted copy counts too, because a file went away, even a worthless one.
    pub fn is_notable(&self) -> bool {
        self.is_trouble()
            || matches!(
                self,
                Recovery::NameRestored { .. } | Recovery::PartialRemoved { .. }
            )
    }

    /// Whether this outcome is a problem the user has to know about.
    pub fn is_trouble(&self) -> bool {
        matches!(
            self,
            Recovery::DataLost { .. }
                | Recovery::Refused { .. }
                | Recovery::PartialKept { .. }
                | Recovery::RecordRejected { .. }
        )
    }
}

/// The result of one recovery pass.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub outcomes: Vec<(PathBuf, Recovery)>,
}

impl RecoveryReport {
    pub fn restored(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Recovery::NameRestored { .. }))
            .count()
    }

    pub fn trouble(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, outcome)| outcome.is_trouble())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }

    /// A one-line summary, or `None` when there was nothing to recover.
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(format!(
            "journal: {} unfinished move(s) found, {} name(s) restored, {} need attention",
            self.outcomes.len(),
            self.restored(),
            self.trouble()
        ))
    }
}

/// Repair whatever the journal says was in flight, and update the journal to say what is
/// still worth knowing.
///
/// Reads the filesystem rather than trusting the records: the journal knows what was
/// *attempted*, and only the filesystem knows what happened. It also does not trust the
/// records to name paths inside the tree or inside a destination root — see
/// [`check_record`] — because the journal is a file in the watched tree that anyone who
/// can write that tree can edit, and a truncated or hand-edited journal reaches the same
/// state with no attacker at all.
pub fn repair(
    journal: &mut Journal,
    watch_root: &Path,
    dests: &[PathBuf],
) -> Result<RecoveryReport, JournalError> {
    let mut report = RecoveryReport::default();
    let mut still_needs_attention = Vec::new();

    for record in journal.unfinished() {
        journal.forget(&record.relative);
        let relative = record.relative.clone();
        // The record is checked before anything is done with the paths it names. Without
        // this, `watch_root.join(absolute)` discards the root and `..` is not resolved, so
        // an unchecked `rel` would let recovery create a symlink or delete a file anywhere
        // on the filesystem — before the first move of the sweep runs.
        if let Err(detail) = check_record(&record, watch_root, dests) {
            still_needs_attention.push(record);
            report
                .outcomes
                .push((relative, Recovery::RecordRejected { detail }));
            continue;
        }
        let source = watch_root.join(&record.relative);
        let (outcome, keep) = recover_one(&source, &record);
        if keep {
            still_needs_attention.push(record);
        }
        report.outcomes.push((relative, outcome));
    }

    if !report.is_empty() {
        journal.retain_only(&still_needs_attention);
        journal.compact()?;
    }
    Ok(report)
}

/// Whether one journal record names paths this pass is allowed to touch.
///
/// Three escapes are refused, each because `repair` would otherwise act on a path the
/// operator never pointed the tool at:
///
/// - an absolute `rel`: `watch_root.join` discards the root, so the record names a path
///   anywhere on the filesystem;
/// - a `..` component in `rel`: the join does not resolve it, so the source climbs out of
///   the watched tree;
/// - a `dest` that is not lexically under one of the `--dest` roots: the `PartialOnly`
///   branch removes the partial sitting beside it and the restore branch creates a symlink
///   pointing at it, so an unchecked destination is a deletion or a link aimed outside
///   every tier.
///
/// The source containment test is the same one `restore` applies — a `strip_prefix` of the
/// watch root — with `..` rejected first, so a lexical prefix match cannot be used to
/// climb back out (`<root>/../outside` strips to `../outside`, which is not contained).
fn check_record(record: &Record, watch_root: &Path, dests: &[PathBuf]) -> Result<(), String> {
    let relative = &record.relative;
    if relative.as_os_str().is_empty() {
        return Err("the recorded path is empty".to_string());
    }
    if relative.is_absolute() {
        return Err(format!(
            "the recorded path {} is absolute, and joining it onto the watched tree would \
             name a file outside it",
            relative.display()
        ));
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "the recorded path {} contains `..`, which escapes the watched tree",
            relative.display()
        ));
    }

    let watch = absolute(watch_root);
    let source = absolute(&watch_root.join(relative));
    match source.strip_prefix(&watch) {
        Ok(rest) if !rest.as_os_str().is_empty() => {}
        _ => {
            return Err(format!(
                "the recorded path {} resolves to {}, outside the watched tree {}",
                relative.display(),
                source.display(),
                watch.display()
            ))
        }
    }

    if let Some(destination) = &record.destination {
        if !under_any_root(destination, dests) {
            return Err(format!(
                "the recorded destination {} is not under any --dest root ({})",
                destination.display(),
                display_roots(dests)
            ));
        }
    }
    Ok(())
}

/// Whether `path` sits under one of `roots`. `..` in the remainder is rejected so that a
/// lexical prefix like `<root>/../outside` cannot pass as contained.
fn under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    let path = absolute(path);
    roots
        .iter()
        .any(|root| match path.strip_prefix(absolute(root)) {
            Ok(rest) => {
                !rest.as_os_str().is_empty()
                    && !rest
                        .components()
                        .any(|component| matches!(component, Component::ParentDir))
            }
            Err(_) => false,
        })
}

fn display_roots(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        return "none given".to_string();
    }
    roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// An absolute form of `path`, without resolving symlinks. `std::path::absolute` is purely
/// lexical plus the current directory, which is what the containment checks need: a
/// symlink-resolving `canonicalize` would require the path to exist, and a record's source
/// may be gone precisely because it was moved.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Decide what became of one interrupted move.
///
/// Returns the outcome and whether the record must be kept (true only for states nothing
/// on disk can describe later, so that reporting them does not depend on the journal being
/// read again — they are kept precisely because they would otherwise be forgotten).
fn recover_one(source: &Path, record: &Record) -> (Recovery, bool) {
    let destination = record.destination.clone();
    let source_state = describe_source(source);
    let destination_state = destination
        .as_deref()
        .map(|path| describe_destination(path, record.size))
        .unwrap_or(DestinationState::Missing);

    match (source_state, destination_state) {
        // The move finished; the record was simply never updated.
        (SourceState::Link, _) => (Recovery::AlreadyLinked, false),

        // The crash window this whole module exists for: the name is gone, the bytes are
        // on the cold tier, complete. Recreate the symlink — reversible, and it makes the
        // file readable again at the path that lost it.
        (SourceState::Missing, DestinationState::Complete { .. }) => {
            let destination = destination.expect("complete destination implies a path");
            match crate::disk_management::link_into_place(&destination, source) {
                Ok(()) => (
                    Recovery::NameRestored {
                        destination: destination.clone(),
                    },
                    false,
                ),
                Err(err) => (
                    Recovery::Refused {
                        destination,
                        detail: format!("could not recreate the symlink: {err}"),
                    },
                    true,
                ),
            }
        }

        // The bytes never left the hot side.
        (SourceState::Missing, DestinationState::Missing) => {
            (Recovery::DataLost { destination }, true)
        }
        (SourceState::Missing, DestinationState::PartialOnly { .. }) => {
            let partial = destination_partials(destination.as_deref());
            let kept = partial.first().cloned();
            (
                match kept {
                    Some(path) => Recovery::PartialKept { path },
                    // A partial was reported but is gone by the time we look: nothing left
                    // to keep, and the file itself is still missing.
                    None => Recovery::DataLost { destination },
                },
                true,
            )
        }
        (SourceState::Missing, DestinationState::Incomplete { .. }) => {
            let destination = destination.expect("incomplete destination implies a path");
            let detail = match describe_destination(destination.as_path(), record.size) {
                DestinationState::Incomplete { size } => format!(
                    "the cold copy is {size} bytes, but the move promised {}",
                    record.size
                ),
                _ => "the cold copy does not match the move that was in flight".to_string(),
            };
            (
                Recovery::Refused {
                    destination,
                    detail,
                },
                true,
            )
        }

        // Both copies are present, so the source was never removed. The mover already
        // checksums a same-size destination and adopts or refuses it, so the next sweep is
        // both cheaper and better informed than anything we could do here.
        (SourceState::File { size }, DestinationState::Complete { .. }) if size == record.size => (
            Recovery::LeftForTheSweep {
                destination: destination.expect("complete destination implies a path"),
            },
            false,
        ),
        (SourceState::File { size }, DestinationState::Complete { .. }) => (
            Recovery::Refused {
                destination: destination.unwrap_or_default(),
                detail: format!(
                    "the hot file is {size} bytes but the move promised {}",
                    record.size
                ),
            },
            true,
        ),
        (SourceState::File { .. }, DestinationState::Missing) => (Recovery::NeverStarted, false),
        (SourceState::File { .. }, DestinationState::PartialOnly { .. }) => {
            // The source is the truth here, so its half-written copy on the cold side is
            // ours to clean up.
            let partials = destination_partials(destination.as_deref());
            match partials.first() {
                Some(path) => match fs::remove_file(path) {
                    Ok(()) => (Recovery::PartialRemoved { path: path.clone() }, false),
                    Err(err) => (
                        Recovery::PartialKept { path: path.clone() },
                        matches!(err.kind(), io::ErrorKind::PermissionDenied),
                    ),
                },
                None => (Recovery::NeverStarted, false),
            }
        }
        (SourceState::File { .. }, DestinationState::Incomplete { .. }) => (
            Recovery::Refused {
                destination: destination.unwrap_or_default(),
                detail: "the cold copy is shorter than the move promised, while the hot file \
                         is whole"
                    .to_string(),
            },
            false,
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceState {
    Missing,
    File { size: u64 },
    Link,
}

fn describe_source(source: &Path) -> SourceState {
    match fs::symlink_metadata(source) {
        Ok(metadata) if metadata.is_symlink() => SourceState::Link,
        Ok(metadata) if metadata.is_file() => SourceState::File {
            size: metadata.len(),
        },
        Ok(_) => SourceState::Missing,
        Err(_) => SourceState::Missing,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationState {
    Missing,
    /// The final name exists with the promised size — the copy completed and was renamed
    /// into place.
    Complete {
        size: u64,
    },
    /// Only `.just_cache-partial-*` files exist: the copy died mid-write.
    PartialOnly {
        size: u64,
    },
    /// The final name exists, but not with the size the move promised.
    Incomplete {
        size: u64,
    },
}

/// What is at the final destination name, judged against the size the move promised.
///
/// The comparison is the point: a file sitting at that name with the *wrong* size must
/// never be treated as the copy a move completed, or recovery would restore a name
/// pointing at bytes nobody promised.
fn describe_destination(destination: &Path, promised: u64) -> DestinationState {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_file() && metadata.len() == promised => {
            DestinationState::Complete {
                size: metadata.len(),
            }
        }
        Ok(metadata) if metadata.is_file() => DestinationState::Incomplete {
            size: metadata.len(),
        },
        _ => {
            let partials = destination_partials(Some(destination));
            match partials.first().and_then(|path| fs::metadata(path).ok()) {
                Some(metadata) => DestinationState::PartialOnly {
                    size: metadata.len(),
                },
                None => DestinationState::Missing,
            }
        }
    }
}

/// In-flight copies sitting beside a destination, newest first.
fn destination_partials(destination: Option<&Path>) -> Vec<PathBuf> {
    let Some(destination) = destination else {
        return Vec::new();
    };
    let Some(directory) = destination.parent() else {
        return Vec::new();
    };
    let prefix = crate::disk_management::PARTIAL_PREFIX;
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect();
    found.sort();
    found
}

// ---------------------------------------------------------------------------------------
// Encoding. JSON lines, written and read by hand because this crate carries no JSON
// dependency; the escape handling is tested with paths that contain the awkward characters,
// since a journal that mangles a path would send recovery to the wrong file.
// ---------------------------------------------------------------------------------------

fn encode_record(record: &Record) -> String {
    let mut line = format!(
        "{{\"stage\":\"intent\",\"rel\":{}",
        escape(&record.relative)
    );
    if let Some(destination) = &record.destination {
        line.push_str(&format!(",\"dest\":{}", escape(destination)));
    }
    line.push_str(&format!(",\"size\":{},\"at\":{}}}", record.size, record.at));
    line
}

fn escape(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn unescape(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let digits: String = characters.by_ref().take(4).collect();
                let code = u32::from_str_radix(&digits, 16)
                    .map_err(|_| format!("bad unicode escape \\u{digits}"))?;
                let decoded = char::from_u32(code)
                    .ok_or_else(|| format!("\\u{digits} is not a character"))?;
                out.push(decoded);
            }
            other => return Err(format!("bad escape \\{}", other.unwrap_or(' '))),
        }
    }
    Ok(out)
}

fn parse_record(line: &str) -> Result<Record, String> {
    match field(line, "stage")?.as_str() {
        "intent" => {}
        // Written by an earlier version of this module, when a finished move recorded
        // itself as well. Read as a record rather than an error so an upgrade does not
        // strand a journal mid-flight; recovery will resolve it from the filesystem.
        "linked" => {}
        other => return Err(format!("unknown stage {other:?}")),
    }
    let relative = PathBuf::from(field(line, "rel")?);
    let destination = optional_field(line, "dest").map(PathBuf::from);
    let size = optional_field(line, "size")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|err| format!("bad size: {err}"))
        })
        .transpose()?
        .unwrap_or(0);
    let at = optional_field(line, "at")
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|err| format!("bad timestamp: {err}"))
        })
        .transpose()?
        .unwrap_or(0);

    Ok(Record {
        relative,
        destination,
        size,
        at,
    })
}

/// Pull one string field out of a record line. Deliberately not a general JSON parser.
fn field(line: &str, key: &str) -> Result<String, String> {
    optional_field(line, key).ok_or_else(|| format!("missing field {key:?}"))
}

fn optional_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    if let Some(quoted) = rest.strip_prefix('"') {
        let mut escaped = false;
        let mut raw = String::new();
        for character in quoted.chars() {
            if escaped {
                raw.push('\\');
                raw.push(character);
                escaped = false;
                continue;
            }
            match character {
                '\\' => escaped = true,
                '"' => return unescape(&raw).ok(),
                other => raw.push(other),
            }
        }
        return None;
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal() -> (tempfile::TempDir, Journal) {
        let tmp = tempfile::tempdir().unwrap();
        let journal = Journal::in_tree(tmp.path()).expect("journal opens");
        (tmp, journal)
    }

    #[test]
    fn an_empty_journal_reports_nothing_unfinished() {
        let (_tmp, journal) = temp_journal();
        assert!(journal.unfinished().is_empty());
        assert!(journal.is_empty());
    }

    #[test]
    fn records_survive_a_reopen() {
        let (tmp, mut journal) = temp_journal();
        journal
            .intent(
                Path::new("sub/a.bin"),
                &tmp.path().join("cold/sub/a.bin"),
                4096,
            )
            .unwrap();

        let reopened = Journal::in_tree(tmp.path()).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.unfinished().len(), 1);
        assert_eq!(reopened.unfinished()[0].size, 4096);
    }

    /// A finished move must leave nothing behind: the symlink is the record that it
    /// finished, and a daemon that wrote a line per move would bury the real in-flight ones.
    #[test]
    fn forgetting_a_finished_move_leaves_an_empty_journal() {
        let (tmp, mut journal) = temp_journal();
        journal
            .intent(Path::new("a.bin"), &tmp.path().join("cold/a.bin"), 10)
            .unwrap();
        journal.forget(Path::new("a.bin"));
        journal.compact().unwrap();

        let reopened = Journal::in_tree(tmp.path()).unwrap();
        assert!(reopened.is_empty());
        assert_eq!(
            fs::read_to_string(tmp.path().join(JOURNAL_NAME)).unwrap(),
            "",
            "and nothing is left on disk"
        );
    }

    #[test]
    fn awkward_paths_round_trip() {
        let (tmp, mut journal) = temp_journal();
        let awkward = Path::new("a \"quoted\"/tab\there/new\nline.bin");
        journal
            .intent(awkward, &tmp.path().join("cold/x.bin"), 7)
            .unwrap();

        let reopened = Journal::in_tree(tmp.path()).unwrap();
        let unfinished = reopened.unfinished();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].relative, awkward);
        assert_eq!(unfinished[0].size, 7);
    }

    #[test]
    fn a_damaged_line_is_reported_rather_than_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(JOURNAL_NAME),
            "{\"stage\":\"intent\",\"rel\":\"a.bin\"}\nnot a record at all\n",
        )
        .unwrap();
        match Journal::in_tree(tmp.path()) {
            Err(JournalError::Damaged { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected a damaged-line error, got {other:?}"),
        }
    }

    #[test]
    fn compaction_leaves_only_the_records_that_remain() {
        let (tmp, mut journal) = temp_journal();
        for name in ["a.bin", "b.bin", "c.bin"] {
            journal
                .intent(Path::new(name), &tmp.path().join("cold").join(name), 10)
                .unwrap();
        }
        journal.forget(Path::new("b.bin"));
        journal.forget(Path::new("c.bin"));
        journal.compact().unwrap();

        let reopened = Journal::in_tree(tmp.path()).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.unfinished()[0].relative, PathBuf::from("a.bin"));
    }

    #[test]
    fn the_journal_file_is_not_mistaken_for_a_record_holder() {
        let (tmp, mut journal) = temp_journal();
        journal
            .intent(Path::new("x.bin"), &tmp.path().join("cold/x.bin"), 1)
            .unwrap();
        assert!(tmp.path().join(JOURNAL_NAME).is_file());
        assert!(
            JOURNAL_NAME.starts_with(INTERNAL_PREFIX),
            "the walk skips this prefix, so the journal can never be moved onto a cold tier"
        );
    }
}

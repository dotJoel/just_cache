//! Audit: compare the watched tree with the cold tiers and name every structural
//! inconsistency between them.
//!
//! The mover leaves one of four states behind, and each is detectable by looking at the
//! two sides of a path at the same time (the destination mirrors a file's path relative
//! to the watched root, so a relative path is the join key):
//!
//! | source path | cold copy | meaning |
//! |---|---|---|
//! | regular file | absent | healthy: not migrated, or not cold yet |
//! | regular file | present | **duplicate**: the source removal never happened |
//! | symlink, resolves under a cold tier | present | healthy: the migrated state |
//! | symlink, resolves outside every cold tier | any | **unexpected-target** |
//! | symlink that does not resolve | any | **dangling-symlink** |
//! | absent | present | **orphaned-copy** |
//!
//! The walk is read-only; classification is a pure function of what the two sides look
//! like, so it can be unit-tested without touching a filesystem. Repair is the only
//! mutating path and is deliberately conservative — it never removes bytes whose
//! content it has not hashed and matched against the copy that stays behind.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::digest;
use crate::disk_management::{self, DiskError};

/// How many examples the readable summary lists by default.
pub const DEFAULT_EXAMPLES: usize = 10;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictKind {
    Healthy,
    OrphanedCopy,
    DanglingSymlink,
    UnexpectedTarget,
    Duplicate,
    /// An offloaded object with fewer copies on disk than its floor requires.
    ReplicaLost,
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
        }
    }

    /// The kinds that make `audit` exit non-zero, in report order.
    pub fn problems() -> [VerdictKind; 5] {
        [
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
    /// never removed after the copy landed.
    Duplicate,
    /// An offloaded object whose copies on disk number fewer than the floor. `missing`
    /// names the configured destinations a copy is absent from — the disks it is not on.
    ReplicaLost {
        present: Vec<PathBuf>,
        missing: Vec<PathBuf>,
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
    /// One line, with the cold copy when it is relevant.
    pub fn describe(&self) -> String {
        if let Verdict::ReplicaLost { present, missing } = &self.verdict {
            let list = |paths: &[PathBuf]| {
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return format!(
                "replica-lost: {} (present on [{}]; missing on [{}])",
                self.path.display(),
                list(present),
                list(missing)
            );
        }
        let cold = self
            .cold_copy
            .as_ref()
            .map(|copy| format!(" (cold copy {})", copy.display()))
            .unwrap_or_default();
        format!(
            "{}: {}{cold}",
            self.verdict.kind().as_str(),
            self.path.display()
        )
    }
}

/// The whole result of an audit run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    pub watch: PathBuf,
    pub dests: Vec<PathBuf>,
    /// Paths that exist on at least one side.
    pub scanned: usize,
    pub healthy: usize,
    /// Only the non-healthy paths, sorted by path then kind.
    pub findings: Vec<Finding>,
}

impl AuditReport {
    pub fn has_findings(&self) -> bool {
        !self.findings.is_empty()
    }

    pub fn count(&self, kind: VerdictKind) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.verdict.kind() == kind)
            .count()
    }

    /// The readable summary: counts for every class, then the first `examples` findings
    /// so a human gets the shape of the problem without the full dump (use `--json` for
    /// everything).
    pub fn summary_lines(&self, examples: usize) -> Vec<String> {
        let mut lines = vec![format!(
            "audit: {} paths scanned under {} against {} cold tier(s)",
            self.scanned,
            self.watch.display(),
            self.dests.len()
        )];
        lines.push(format!("  healthy: {}", self.healthy));
        for kind in VerdictKind::problems() {
            lines.push(format!("  {}: {}", kind.as_str(), self.count(kind)));
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
            lines.push(format!("  {}", finding.describe()));
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
                Verdict::DanglingSymlink { target } | Verdict::UnexpectedTarget { target } => {
                    json_string(&target.to_string_lossy())
                }
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
                    "{{\"enabled\":true,\"removed-duplicate\":{},\"restored-symlink\":{},\"refused\":{},\"not-attempted\":{},\"actions\":[{}]}}",
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
                    actions.join(",")
                )
            }
        };

        format!(
            "{{\"watch\":{},\"dest\":[{}],\"scanned\":{},\"healthy\":{},\"findings\":[{}],\"counts\":{{{}}},\"repair\":{}}}",
            json_string(&self.watch.to_string_lossy()),
            dest_json.join(","),
            self.scanned,
            self.healthy,
            findings.join(","),
            counts.join(","),
            repair_json
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
        scanned: relatives.len(),
        healthy,
        findings,
    })
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
}

impl RepairAction {
    pub fn name(&self) -> &'static str {
        match self {
            RepairAction::RemovedDuplicate { .. } => "removed-duplicate",
            RepairAction::RestoredSymlink { .. } => "restored-symlink",
            RepairAction::Refused { .. } => "refused",
            RepairAction::NotAttempted { .. } => "not-attempted",
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
            RepairAction::Refused { reason } | RepairAction::NotAttempted { reason } => {
                Some(reason)
            }
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
        };

        outcomes.push(RepairOutcome {
            path: finding.path.clone(),
            kind: finding.verdict.kind(),
            action,
        });
    }

    Ok(outcomes)
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
            scanned: 10,
            healthy: 6,
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
            scanned: 1,
            healthy: 0,
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
}

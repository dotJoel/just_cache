//! Garbage collection for tier bytes no catalog row references (issue #144).
//!
//! A delete (issue #128) releases the bytes the catalog vouches for, but nothing removes
//! bytes no *row* ever reaches: an export that aborted before a location was recorded, a
//! copy whose row a reconcile concluded away, a retired `.just_cache-partial-*`. This
//! module finds them and — only under `--apply` — removes them.
//!
//! # Report first, act on request
//!
//! Nothing here runs as part of a read or a sweep. `just_cache gc` reports the bytes it
//! would remove and exits `1` as a finding; `--apply` removes them and exits `0` when it
//! removed everything it named. That is the tool's posture everywhere else (§9): a pass
//! that deletes is one an operator asked for.
//!
//! # What counts as garbage
//!
//! Inside a **configured tier root** (a root the catalog recorded at `sync`, plus — when a
//! `tiers.toml` is beside the catalog — an `offline` tier's mount), a regular file or a
//! `.just_cache-partial-*` file is garbage exactly when no `location` row resolves to it,
//! and a directory is garbage exactly when nothing in its subtree is kept. Two
//! consequences are load bearing:
//!
//! * A row whose copy is **unverified** still claims its bytes. Unknown is not none: a
//!   file a row points at is kept whether or not a scrub ever confirmed it, because the
//!   catalog is what decides, not a scrub verdict it does not carry.
//! * A row that does *not* resolve under its tier root is a **finding**, never a licence:
//!   the walk does not delete anything on the strength of a row it could not read.
//!
//! # What it never touches
//!
//! Nothing outside a configured tier root: every candidate comes from a walk of one, the
//! walk never follows a symlink and never crosses a device boundary, and the tool's own
//! files (the catalog and its SQLite siblings, the move journal, this pass's journal, the
//! configs) are excluded by name even when a tier root is also the watch root. The scan
//! is the inverse of the mover's placement (§9's invariant 4 posture, applied to deletion).
//!
//! # Crash safety (§9 invariant 5)
//!
//! Before each unlink an `--apply` pass writes the file's intent to a journal beside the
//! catalog (`GC_JOURNAL_NAME`), flushed to stable storage, and forgets it once the unlink
//! succeeds — the same write-ahead shape the mover uses, in a file of its own so the
//! mover's recovery (which reads every record as a move) never sees a GC record. A crash
//! after the nth unlink leaves both the file gone and its record behind; the next pass
//! drops every record whose file is already gone, adopts nothing, and reports nothing.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::catalog::{self, resolve_location_path, Catalog, CatalogError};
use crate::disk_management::PARTIAL_PREFIX;
use crate::journal::{Journal, JOURNAL_NAME};
use crate::offline;
use crate::policy::POLICY_FILE_NAME;
use crate::schedule::{SCHEDULE_FILE_NAME, SCHEDULE_STATE_NAME};
use crate::tiers::{TierKind, TierSet, TIERS_FILE_NAME};

/// The GC's write-ahead journal, beside the catalog. A name of its own keeps its records
/// out of the mover's journal, whose recovery interprets every record as a move.
pub const GC_JOURNAL_NAME: &str = ".just_cache-gc-journal";

/// Everything one `gc` pass needs.
pub struct GcRequest<'a> {
    pub catalog: &'a mut Catalog,
    /// Tier config, when one is beside the catalog. Needed only to reach an `offline`
    /// tier's mount; a filesystem tier is described by the catalog's own roots.
    pub tiers: Option<&'a TierSet>,
    /// Restrict the pass to one configured tier, by name or by root path.
    pub tier: Option<String>,
    /// Remove what the pass found. Without it, the pass reports and changes no byte.
    pub apply: bool,
}

#[derive(Debug, Error)]
pub enum GcError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("the catalog records no roots; run `just_cache catalog sync` before gc")]
    NoRoots,
    #[error("--tier {tier}: no configured tier matches (roots or names: {known})")]
    UnknownTier { tier: String, known: String },
    #[error("journal: {0}")]
    Journal(String),
}

/// What kind of thing a garbage entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GarbageKind {
    /// A regular file no row references.
    File,
    /// A `.just_cache-partial-*` file no row references: an aborted copy or a retired
    /// partial. Never adopted as a copy, so it is garbage by construction.
    Partial,
    /// A directory whose whole subtree is garbage (or already empty).
    EmptyDir,
}

impl GarbageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GarbageKind::File => "file",
            GarbageKind::Partial => "partial",
            GarbageKind::EmptyDir => "empty-dir",
        }
    }
}

/// One garbage entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Garbage {
    pub path: PathBuf,
    pub size: u64,
    pub kind: GarbageKind,
}

/// A row the pass could not read as pointing at bytes it is allowed to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcFinding {
    pub tier: String,
    pub storage_key: String,
    pub detail: String,
}

impl GcFinding {
    pub fn describe(&self) -> String {
        format!(
            "  unresolved row {}/{}: {} (nothing removed on its account)",
            self.tier, self.storage_key, self.detail
        )
    }
}

/// Garbage found under one configured tier root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierGarbage {
    /// The tier's name when the config names it, otherwise its root path.
    pub tier: String,
    pub root: PathBuf,
    pub entries: Vec<Garbage>,
}

impl TierGarbage {
    pub fn bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|entry| entry.kind != GarbageKind::EmptyDir)
            .map(|entry| entry.size)
            .sum()
    }
}

/// The result of one GC pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    /// Whether the pass removed anything (`--apply`).
    pub applied: bool,
    pub tiers: Vec<TierGarbage>,
    pub findings: Vec<GcFinding>,
    pub removed: Vec<PathBuf>,
    pub failures: Vec<(PathBuf, String)>,
    /// Journal records whose file was already gone and were dropped this pass.
    pub journal_recovered: Vec<PathBuf>,
}

impl GcReport {
    pub fn files(&self) -> usize {
        self.tiers
            .iter()
            .flat_map(|tier| &tier.entries)
            .filter(|entry| entry.kind != GarbageKind::EmptyDir)
            .count()
    }

    pub fn empty_dirs(&self) -> usize {
        self.tiers
            .iter()
            .flat_map(|tier| &tier.entries)
            .filter(|entry| entry.kind == GarbageKind::EmptyDir)
            .count()
    }

    pub fn bytes(&self) -> u64 {
        self.tiers.iter().map(TierGarbage::bytes).sum()
    }

    /// True when the pass leaves the operator something to act on: garbage it only
    /// reported (`--apply` was not given), a failure, or a row it could not read.
    pub fn has_findings(&self) -> bool {
        !self.failures.is_empty()
            || !self.findings.is_empty()
            || (!self.applied && (self.files() > 0 || self.empty_dirs() > 0))
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.files() == 0 && self.empty_dirs() == 0 {
            lines.push(if self.applied {
                "gc: no garbage; nothing removed".to_string()
            } else {
                "gc: no garbage; nothing to remove".to_string()
            });
        } else {
            for tier in &self.tiers {
                if tier.entries.is_empty() {
                    continue;
                }
                let files = tier
                    .entries
                    .iter()
                    .filter(|entry| entry.kind != GarbageKind::EmptyDir)
                    .count();
                let dirs = tier.entries.len() - files;
                lines.push(format!(
                    "gc: tier {} holds {} unreferenced file(s) and {} empty director(ies), {}",
                    tier.tier,
                    files,
                    dirs,
                    human_bytes(tier.bytes())
                ));
            }
            lines.push(format!(
                "gc: {} file(s), {} empty director(ies), {} {}",
                self.files(),
                self.empty_dirs(),
                human_bytes(self.bytes()),
                if self.applied {
                    "removed"
                } else {
                    "to remove (--apply)"
                }
            ));
        }
        for path in &self.journal_recovered {
            lines.push(format!(
                "  dropped a stale journal record for {} (already gone)",
                path.display()
            ));
        }
        lines
    }

    pub fn finding_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for tier in &self.tiers {
            for entry in &tier.entries {
                lines.push(format!(
                    "  {} {} ({}, {})",
                    if self.applied {
                        "removed"
                    } else {
                        "would remove"
                    },
                    entry.path.display(),
                    entry.kind.as_str(),
                    human_bytes(entry.size)
                ));
            }
        }
        for (path, reason) in &self.failures {
            lines.push(format!("  could not remove {}: {reason}", path.display()));
        }
        for finding in &self.findings {
            lines.push(finding.describe());
        }
        lines
    }

    /// The report as JSON, hand-rolled like the rest of the tool's `--json` output.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        out.push_str(&format!("\"applied\":{},", self.applied));
        out.push_str(&format!("\"files\":{},", self.files()));
        out.push_str(&format!("\"empty_dirs\":{},", self.empty_dirs()));
        out.push_str(&format!("\"bytes\":{},", self.bytes()));
        out.push_str(&format!(
            "\"removed\":[{}],",
            self.removed
                .iter()
                .map(|path| json_string(&path.display().to_string()))
                .collect::<Vec<_>>()
                .join(",")
        ));
        out.push_str("\"failures\":[");
        out.push_str(
            &self
                .failures
                .iter()
                .map(|(path, reason)| {
                    format!(
                        "{{\"path\":{},\"reason\":{}}}",
                        json_string(&path.display().to_string()),
                        json_string(reason)
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push_str("],");
        out.push_str("\"tiers\":[");
        out.push_str(
            &self
                .tiers
                .iter()
                .map(|tier| {
                    let entries = tier
                        .entries
                        .iter()
                        .map(|entry| {
                            format!(
                                "{{\"path\":{},\"size\":{},\"kind\":{}}}",
                                json_string(&entry.path.display().to_string()),
                                entry.size,
                                json_string(entry.kind.as_str())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(
                        "{{\"tier\":{},\"root\":{},\"bytes\":{},\"entries\":[{entries}]}}",
                        json_string(&tier.tier),
                        json_string(&tier.root.display().to_string()),
                        tier.bytes()
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push_str("],");
        out.push_str("\"findings\":[");
        out.push_str(
            &self
                .findings
                .iter()
                .map(|finding| {
                    format!(
                        "{{\"tier\":{},\"storage_key\":{},\"detail\":{}}}",
                        json_string(&finding.tier),
                        json_string(&finding.storage_key),
                        json_string(&finding.detail)
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push(']');
        out.push('}');
        out
    }
}

/// Find the bytes no catalog row references, across the configured tiers, and remove them
/// when the request says to. See the module docs for the rules.
pub fn gc(request: GcRequest<'_>) -> Result<GcReport, GcError> {
    let GcRequest {
        catalog,
        tiers,
        tier,
        apply,
    } = request;
    let roots = catalog.roots()?;
    if roots.is_empty() {
        return Err(GcError::NoRoots);
    }

    // The roots this pass will walk: every recorded root (optionally just one), plus an
    // `offline` tier's mount when a config describes it. Deduplicated by path, so a mount
    // that is also a recorded root is walked once.
    let mut scan: Vec<(String, PathBuf)> = Vec::new();
    let mut known: Vec<String> = Vec::new();
    for root in &roots {
        let name = tiers
            .map(|set| set.name_for_root(root))
            .unwrap_or_else(|| root.display().to_string());
        known.push(name.clone());
        if let Some(only) = &tier {
            if *only != name && Path::new(only.as_str()) != root.as_path() {
                continue;
            }
        }
        scan.push((name, root.clone()));
    }
    if let Some(set) = tiers {
        for configured in set.tiers() {
            if configured.kind != TierKind::Offline {
                continue;
            }
            let mount = configured.path.clone();
            known.push(configured.name.clone());
            if let Some(only) = &tier {
                if *only != configured.name && Path::new(only.as_str()) != mount.as_path() {
                    continue;
                }
            }
            if !mount.is_dir() || scan.iter().any(|(_, path)| path == &mount) {
                continue;
            }
            scan.push((configured.name.clone(), mount));
        }
    }
    known.sort();
    known.dedup();
    if scan.is_empty() {
        return Err(GcError::UnknownTier {
            tier: tier.unwrap_or_default(),
            known: known.join(", "),
        });
    }

    let mut report = GcReport {
        applied: apply,
        ..GcReport::default()
    };
    // A GC and a delete are complements: an `--apply` pass finishes any released bytes the
    // catalog already owes an unlink before judging what no row references, so it never
    // removes a file the deliberate path is about to remove anyway. A report-only pass
    // changes nothing at all — a released file is then just another unreferenced file the
    // scan reports. A released copy whose tier is not reachable stays pending, and is
    // named as a finding rather than silently held.
    if apply {
        for outcome in catalog.finish_pending_removals(true)? {
            if let catalog::RemovalOutcome::Deferred { path, reason } = outcome {
                report.findings.push(GcFinding {
                    tier: "pending".to_string(),
                    storage_key: path.display().to_string(),
                    detail: reason,
                });
            }
        }
    }

    // Which bytes a row still claims. A row's tier must be a recorded root (or, for an
    // `offline` row, name a volume); anything else is a finding, not a licence.
    let mut referenced: BTreeSet<PathBuf> = BTreeSet::new();
    for location in catalog.all_locations()? {
        let tier = location.tier.clone();
        let key = location.storage_key.clone();
        if roots.iter().any(|root| root.as_path() == Path::new(&tier)) {
            match resolve_location_path(&tier, &key, &roots) {
                Ok(path) => {
                    referenced.insert(path);
                }
                Err(err) => report.findings.push(GcFinding {
                    tier,
                    storage_key: key,
                    detail: err.detail().to_string(),
                }),
            }
        } else if let Some((_, relative)) = offline::parse_storage_key(&key) {
            // An offline row: its bytes live under the tier's mount. Without a config the
            // mount is unknown — and then it is not walked either, so nothing is at risk.
            if let Some(mount) = tiers.and_then(|set| set.get(&tier)).map(|t| t.path.clone()) {
                referenced.insert(mount.join(&relative));
            }
        } else {
            report.findings.push(GcFinding {
                tier,
                storage_key: key,
                detail: "not a recorded root and not a volume storage key".to_string(),
            });
        }
    }

    let catalog_path = catalog.path().to_path_buf();
    for (label, root) in scan {
        let tier_garbage = walk_tier(
            &label,
            &root,
            &referenced,
            &catalog_path,
            &mut report.findings,
        );
        report.tiers.push(tier_garbage);
    }

    if apply {
        apply_garbage(catalog, &mut report)?;
    }
    Ok(report)
}

/// Walk one tier root and collect what no row references. Symlinks are never followed and
/// never candidates; a different device is a boundary the walk does not cross.
fn walk_tier(
    label: &str,
    root: &Path,
    referenced: &BTreeSet<PathBuf>,
    catalog_path: &Path,
    findings: &mut Vec<GcFinding>,
) -> TierGarbage {
    let mut entries: Vec<Garbage> = Vec::new();
    let root_dev = fs::symlink_metadata(root)
        .map(|metadata| device_of(&metadata))
        .unwrap_or(0);
    scan_dir(
        root,
        root_dev,
        referenced,
        catalog_path,
        findings,
        &mut entries,
        true,
    );
    // Deepest directories first, so a parent is only removed once its garbage children
    // are gone. Emptied files stay in the list; the removal path orders across both.
    TierGarbage {
        tier: label.to_string(),
        root: root.to_path_buf(),
        entries,
    }
}

/// Returns true when something in `dir`'s subtree must be kept (so `dir` is not garbage).
fn scan_dir(
    dir: &Path,
    root_dev: u64,
    referenced: &BTreeSet<PathBuf>,
    catalog_path: &Path,
    findings: &mut Vec<GcFinding>,
    entries: &mut Vec<Garbage>,
    is_root: bool,
) -> bool {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) => {
            findings.push(GcFinding {
                tier: "walk".to_string(),
                storage_key: dir.display().to_string(),
                detail: format!("cannot read directory: {err}"),
            });
            return true;
        }
    };
    let mut keep = false;
    let mut children: Vec<(PathBuf, std::ffi::OsString)> = Vec::new();
    for child in listing {
        let child = match child {
            Ok(child) => child,
            Err(err) => {
                findings.push(GcFinding {
                    tier: "walk".to_string(),
                    storage_key: dir.display().to_string(),
                    detail: format!("cannot read a directory entry: {err}"),
                });
                keep = true;
                continue;
            }
        };
        children.push((child.path(), child.file_name()));
    }
    for (path, name) in children {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                findings.push(GcFinding {
                    tier: "walk".to_string(),
                    storage_key: path.display().to_string(),
                    detail: format!("cannot stat: {err}"),
                });
                keep = true;
                continue;
            }
        };
        // A symlink is never followed and never removed: it is a name, not bytes.
        if metadata.file_type().is_symlink() {
            keep = true;
            continue;
        }
        if metadata.is_dir() {
            // A mount point inside the root is another filesystem: not this tier's bytes.
            if device_of(&metadata) != root_dev {
                keep = true;
                continue;
            }
            if scan_dir(
                &path,
                root_dev,
                referenced,
                catalog_path,
                findings,
                entries,
                false,
            ) {
                keep = true;
            } else {
                entries.push(Garbage {
                    path,
                    size: 0,
                    kind: GarbageKind::EmptyDir,
                });
            }
        } else if metadata.is_file() {
            if referenced.contains(&path) || is_tool_file(&name, &path, catalog_path) {
                keep = true;
                continue;
            }
            let kind = if name.to_string_lossy().starts_with(PARTIAL_PREFIX) {
                GarbageKind::Partial
            } else {
                GarbageKind::File
            };
            entries.push(Garbage {
                path,
                size: metadata.len(),
                kind,
            });
        } else {
            // A socket, fifo or device node: not a copy the mover made.
            keep = true;
        }
    }
    let _ = is_root;
    keep
}

/// The tool's own files, which a tier root may also contain: the catalog and its SQLite
/// siblings, the journals, and the config files. A partial is data the pass *does* judge.
fn is_tool_file(name: &std::ffi::OsStr, path: &Path, catalog_path: &Path) -> bool {
    if path == catalog_path {
        return true;
    }
    let Some(name) = name.to_str() else {
        return true; // a non-UTF-8 name is not something the mover writes
    };
    if name.starts_with(PARTIAL_PREFIX) {
        return false;
    }
    if name.starts_with(crate::journal::INTERNAL_PREFIX) {
        return true;
    }
    matches!(
        name,
        JOURNAL_NAME
            | GC_JOURNAL_NAME
            | catalog::CATALOG_NAME
            | TIERS_FILE_NAME
            | POLICY_FILE_NAME
            | SCHEDULE_FILE_NAME
            | SCHEDULE_STATE_NAME
    )
}

/// Remove every file the pass named, journalling each intent before its unlink, then the
/// emptied directories deepest-first.
fn apply_garbage(catalog: &Catalog, report: &mut GcReport) -> Result<(), GcError> {
    let dir = catalog
        .path()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let journal_path = dir.join(GC_JOURNAL_NAME);
    let mut journal = Journal::at(journal_path).map_err(|err| GcError::Journal(err.to_string()))?;

    // Drop every record whose file is already gone: a crash after an unlink leaves one,
    // and it is stale — the file it named is removed, not adopted.
    let mut stale = false;
    for record in journal.unfinished() {
        let target = record
            .destination
            .clone()
            .unwrap_or(record.relative.clone());
        if fs::symlink_metadata(&target).is_err() {
            journal.forget(&record.relative);
            report.journal_recovered.push(target);
            stale = true;
        }
    }
    if stale {
        journal
            .compact()
            .map_err(|err| GcError::Journal(err.to_string()))?;
    }

    let fault = crate::faults::Fault::from_env();
    let mut removed = 0usize;
    for tier in &report.tiers {
        for entry in &tier.entries {
            if entry.kind == GarbageKind::EmptyDir {
                continue;
            }
            if journal
                .intent(&entry.path, &entry.path, entry.size)
                .is_err()
            {
                report.failures.push((
                    entry.path.clone(),
                    "cannot record the intent in the GC journal".to_string(),
                ));
                continue;
            }
            match fs::remove_file(&entry.path) {
                Ok(()) => {
                    journal.forget(&entry.path);
                    report.removed.push(entry.path.clone());
                    removed += 1;
                    if let Some(fault) = fault {
                        if fault.mode == crate::faults::FaultMode::GcAfterDelete
                            && removed == fault.at
                        {
                            std::process::abort();
                        }
                    }
                }
                Err(err) => report.failures.push((entry.path.clone(), err.to_string())),
            }
        }
    }

    // Then directories, deepest first, so removing a child never strands a parent.
    let mut dirs: Vec<PathBuf> = report
        .tiers
        .iter()
        .flat_map(|tier| &tier.entries)
        .filter(|entry| entry.kind == GarbageKind::EmptyDir)
        .map(|entry| entry.path.clone())
        .collect();
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in dirs {
        let _ = fs::remove_dir(&dir);
    }
    Ok(())
}

#[cfg(unix)]
fn device_of(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(not(unix))]
fn device_of(_metadata: &fs::Metadata) -> u64 {
    0
}

fn human_bytes(bytes: u64) -> String {
    crate::scope::human_bytes(bytes)
}

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

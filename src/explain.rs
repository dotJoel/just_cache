//! `explain`: why is this file where it is, and what would happen next.
//!
//! A sweep answers in aggregate — "12 skipped, 3 moved" — and the per-file reason only
//! appears under `-v`, buried among every other file. This module answers for one path,
//! and it answers in the order the mover evaluates (`docs/design.md` §5, §5.1):
//!
//! 1. **scope** — is the path the engine's to manage at all, and which `--include` /
//!    `--exclude` pattern or size bound decided it? An out-of-scope answer is a real
//!    answer, and it names the rule rather than stopping at "outside the configured
//!    scope".
//! 2. **guards** — is something holding it open (and, when the `/proc` scan could see it,
//!    which pid), does it have more than one link, did it change since this scan?
//! 3. **policy** — the current last-use stamp and where that stamp came from (atime, or
//!    the mtime fallback), the idle duration against `--min-idle-days`, whether the path
//!    is already a symlink, and the access pin.
//! 4. **verdict** — would move now / would move in N days / would never move, and why.
//!
//! The ordering is the substance, not decoration. A path that is both out of scope and
//! too warm must report the scope reason, because that is the gate the mover reaches
//! first — and the later stages are marked *not evaluated* rather than filled in with a
//! plausible-looking "not yet cold", which would be an answer the mover never computed.
//!
//! The command is read-only: it stats, it never opens the file for reading (which would
//! bump the very atime it is reporting), and it moves nothing.
//!
//! # The catalog seam
//!
//! Issue #16 builds a catalog that will answer "where does this object live and when was
//! it last accessed" as a query. `explain` must prefer it when one exists and fall back to
//! the filesystem when it does not, and must *report* a disagreement rather than resolve it
//! silently. [`catalog_answer`] is the single place that preference is expressed; see its
//! docs for exactly what the catalog has to provide. Today there is no catalog, so it
//! returns `None` and the filesystem is the source of truth — the same place a sweep reads
//! from, which is why the explanation cannot drift from the mover's behaviour.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::Catalog;
use crate::disk_management::{self, AccessSource, FileEntry};
use crate::file_movement::{Policy, UsageTracker};
use crate::opened::{self, Coverage, FileId, Guards};
use crate::policy::{DownDecision, Lifecycle};
use crate::scope::{self, Scope, ScopeRefusal};

/// The exit code for a path the engine manages: a sweep run now would move it, or it is
/// already a symlink into a configured cold tier.
pub const EXIT_MANAGED: u8 = 0;
/// The exit code for a path it would not move: out of scope, guarded, too warm, already
/// elsewhere, or absent. Deliberately not an error — "would not be moved" is an answer.
pub const EXIT_NOT_MOVED: u8 = 1;

/// Everything `explain` decides with. Grouped for the same reason [`MoveContext`] is: the
/// list is otherwise long enough that a call site stops being readable.
///
/// [`MoveContext`]: crate::file_movement::MoveContext
pub struct ExplainContext<'a> {
    /// The path being asked about. Absolute, or relative to `watch`.
    pub path: &'a Path,
    pub watch: &'a Path,
    /// Cold tiers, fastest first — the same list a sweep fills.
    pub dests: &'a [PathBuf],
    pub scope: &'a Scope,
    pub policy: &'a Policy,
    /// The same snapshot and rules a sweep would use.
    pub guards: &'a Guards,
    /// A sweep's tracker. `explain` never observes anything, so this is empty and the
    /// observed-access pin can never fire during an explain.
    pub tracker: &'a UsageTracker,
    /// The free-space floor a sweep applies to a tier before moving onto it.
    pub min_free: u64,
    /// The catalog to consult for "where does this object live", when one exists.
    ///
    /// `None` is the honest default: with no catalog the filesystem is the only source of
    /// truth, exactly where a sweep reads, and the report says so rather than inventing an
    /// answer. `main` sets this only for a file that already exists — `explain` must never
    /// conjure an empty catalog beside a healthy tree.
    pub catalog: Option<PathBuf>,
    /// The lifecycle rules (`policy.toml`), when one governs this tree. `None` is the
    /// fallback the report names: the flag-driven decision, exactly what the tool did
    /// before rules existed.
    pub lifecycle: Option<&'a Lifecycle<'a>>,
}

/// What the engine concluded about one path.
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    pub path: PathBuf,
    pub watch: PathBuf,
    /// Path relative to `watch`, when the path is under it.
    pub relative: Option<PathBuf>,
    pub exists: bool,
    pub scope: ScopeReport,
    pub guards: GuardsReport,
    pub policy: PolicyReport,
    pub verdict: Verdict,
    pub catalog: CatalogReport,
    /// What `policy.toml` decided, or that there is none. This is the "which rule fired,
    /// or which exclusion stopped it" answer §5 requires of `explain`.
    pub rules: RuleReport,
}

impl Explanation {
    /// True when the engine manages this path: it would move it now, or it has already
    /// migrated it. This is the `0` in the exit-code contract, exposed so callers and
    /// tests do not have to re-derive it from the verdict.
    pub fn is_managed(&self) -> bool {
        matches!(
            self.verdict,
            Verdict::WouldMoveNow { .. } | Verdict::AlreadyMigrated { tier: Some(_), .. }
        )
    }

    pub fn exit_code(&self) -> u8 {
        if self.is_managed() {
            EXIT_MANAGED
        } else {
            EXIT_NOT_MOVED
        }
    }

    /// The four ordered sections, one per line, plus the catalog note when there is one.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("explain: {}", self.path.display())];
        lines.push(format!("  scope:   {}", self.scope.describe()));
        lines.push(format!("  guards:  {}", self.guards.describe()));
        lines.push(format!("  policy:  {}", self.policy.describe()));
        lines.push(format!("  rules:   {}", self.rules.describe()));
        lines.push(format!("  verdict: {}", self.verdict.describe()));
        lines.push(format!("  catalog: {}", self.catalog.describe()));
        for disagreement in &self.catalog.disagreements {
            lines.push(format!(
                "  WARNING: catalog and filesystem disagree: {disagreement}"
            ));
        }
        lines
    }

    /// A stable, machine-readable document, hand-written for the same reason `audit`'s is:
    /// the crate carries no JSON dependency, so the shape is a contract covered by tests
    /// rather than generated by reflection.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        out.push_str(&format!(
            "\"path\":{},",
            json_string(&self.path.to_string_lossy())
        ));
        out.push_str(&format!(
            "\"watch\":{},",
            json_string(&self.watch.to_string_lossy())
        ));
        out.push_str(&format!(
            "\"relative\":{},",
            self.relative
                .as_ref()
                .map(|rel| json_string(&rel.to_string_lossy()))
                .unwrap_or_else(|| "null".to_string())
        ));
        out.push_str(&format!("\"exists\":{},", self.exists));
        out.push_str(&format!("\"managed\":{},", self.is_managed()));
        out.push_str(&format!("\"exit_code\":{},", self.exit_code()));
        out.push_str(&format!("\"scope\":{},", self.scope.to_json()));
        out.push_str(&format!("\"guards\":{},", self.guards.to_json()));
        out.push_str(&format!("\"policy\":{},", self.policy.to_json()));
        out.push_str(&format!("\"rules\":{},", self.rules.to_json()));
        out.push_str(&format!("\"verdict\":{},", self.verdict.to_json()));
        out.push_str(&format!("\"catalog\":{}", self.catalog.to_json()));
        out.push('}');
        out
    }
}

/// Which gate decided, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeReport {
    /// In scope. Names the `--include` that matched, when there is one.
    In {
        matched_include: Option<String>,
        note: String,
    },
    /// Refused by scope (or by the empty-file check the mover runs immediately after it).
    Out(ScopeRefusal),
    /// The path is not under the watched tree at all.
    OutsideWatch { path: PathBuf, watch: PathBuf },
    /// Nothing to evaluate: already a symlink, or nothing at the path.
    NotApplicable { reason: String },
}

impl ScopeReport {
    pub fn decision(&self) -> &'static str {
        match self {
            ScopeReport::In { .. } => "in",
            ScopeReport::Out(_) => "out",
            ScopeReport::OutsideWatch { .. } => "out",
            ScopeReport::NotApplicable { .. } => "not-applicable",
        }
    }

    fn describe(&self) -> String {
        match self {
            ScopeReport::In {
                matched_include,
                note,
            } => match matched_include {
                Some(pattern) => format!("managed: matched --include '{pattern}'; {note}"),
                None => format!(
                    "managed: no --include set, so every path under the watched tree is fair \
                     game; {note}"
                ),
            },
            ScopeReport::Out(refusal) => format!("not managed: {}", refusal.describe()),
            ScopeReport::OutsideWatch { path, watch } => format!(
                "not managed: {} is not under the watched tree {}",
                path.display(),
                watch.display()
            ),
            ScopeReport::NotApplicable { reason } => format!("not applicable: {reason}"),
        }
    }

    fn to_json(&self) -> String {
        match self {
            ScopeReport::In {
                matched_include,
                note,
            } => format!(
                "{{\"decision\":\"in\",\"kind\":\"included\",\"matched_include\":{},\"rule\":{}}}",
                matched_include
                    .as_ref()
                    .map(|pattern| json_string(pattern))
                    .unwrap_or_else(|| "null".to_string()),
                json_string(note)
            ),
            ScopeReport::Out(refusal) => {
                let (pattern, size, bound) = match refusal {
                    ScopeRefusal::NotIncluded { .. } | ScopeRefusal::Empty => {
                        ("null".to_string(), "null".to_string(), "null".to_string())
                    }
                    ScopeRefusal::Excluded { pattern } => {
                        (json_string(pattern), "null".to_string(), "null".to_string())
                    }
                    ScopeRefusal::TooSmall { size, min } => {
                        ("null".to_string(), size.to_string(), min.to_string())
                    }
                    ScopeRefusal::TooLarge { size, max } => {
                        ("null".to_string(), size.to_string(), max.to_string())
                    }
                };
                format!(
                    "{{\"decision\":\"out\",\"kind\":{},\"rule\":{},\"pattern\":{},\"size\":{},\
                     \"bound\":{}}}",
                    json_string(refusal.kind()),
                    json_string(&refusal.describe()),
                    pattern,
                    size,
                    bound
                )
            }
            ScopeReport::OutsideWatch { path, watch } => format!(
                "{{\"decision\":\"out\",\"kind\":\"outside-watch\",\"rule\":{},\"pattern\":null,\
                 \"size\":null,\"bound\":null,\"watch\":{},\"path\":{}}}",
                json_string(&format!(
                    "{} is not under the watched tree {}",
                    path.display(),
                    watch.display()
                )),
                json_string(&watch.to_string_lossy()),
                json_string(&path.to_string_lossy())
            ),
            ScopeReport::NotApplicable { reason } => format!(
                "{{\"decision\":\"not-applicable\",\"kind\":\"not-applicable\",\"rule\":{},\
                 \"pattern\":null,\"size\":null,\"bound\":null}}",
                json_string(reason)
            ),
        }
    }
}

/// A process holding the file open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenHold {
    /// Pids the `/proc` scan saw holding it. Empty when the file is open in a process the
    /// scan could not inspect — the guard still fires, but "by whom" is not visible.
    pub pids: Vec<u32>,
}

/// How the guard result reads: the whole point is to be honest about a guard that is
/// weaker than it looks (see [`Coverage`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardsReport {
    /// False when an earlier gate already refused, so guards were never reached.
    pub evaluated: bool,
    pub skipped_reason: Option<String>,
    pub coverage: Coverage,
    pub coverage_note: Option<String>,
    /// Link count, always read: it is one stat and the answer matters even when
    /// `--allow-hardlinked` means it will not block.
    pub links: Option<u64>,
    pub hardlink_enforced: bool,
    pub open: Option<OpenHold>,
    pub changed: Option<ChangedSinceScan>,
    /// The gate's own sentence when it blocked, for the verdict.
    pub decisive: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedSinceScan {
    pub expected: u64,
    pub actual: u64,
}

impl GuardsReport {
    fn not_evaluated(reason: &str, guards: &Guards) -> Self {
        let open_files = guards.open_files();
        Self {
            evaluated: false,
            skipped_reason: Some(reason.to_string()),
            coverage: open_files.coverage(),
            coverage_note: open_files.coverage_note(),
            links: None,
            hardlink_enforced: guards.hardlink_check(),
            open: None,
            changed: None,
            decisive: None,
        }
    }

    /// Re-run the in-use checks in the mover's order, but keep the detail a sweep throws
    /// away: the pid, the link count, and whether the size moved between the stat that
    /// built the entry and this one.
    fn evaluate(guards: &Guards, entry: &FileEntry) -> Self {
        let open_files = guards.open_files();
        let mut decisive = None;

        let mut open = None;
        if let Some(id) = FileId::of_entry(entry) {
            if open_files.contains(id) {
                let pids = open_files.holder_pids(id).to_vec();
                decisive = Some(match pids.first() {
                    Some(pid) => format!("open by pid {pid}"),
                    None => "open by another process (pid not visible)".to_string(),
                });
                open = Some(OpenHold { pids });
            }
        }

        // The hardlink count is read even when the guard is off, because "2 links, but
        // --allow-hardlinked is set" is a different answer from "1 link".
        let links = opened::link_count(&entry.path);
        if guards.hardlink_check() {
            if let Some(links) = links {
                if links > 1 && decisive.is_none() {
                    decisive = Some(format!("hardlinked elsewhere ({links} links)"));
                }
            }
        }

        // "Changed since a scan" is the mover's `SourceChanged` guard: the file must still
        // be the one the policy chose. For a one-shot read-only command the scan and this
        // check are microseconds apart, so this can only fire under a genuine race — which
        // is exactly when a sweep would refuse too, and worth naming.
        let mut changed = None;
        if let Ok(live) = fs::metadata(&entry.path) {
            if live.len() != entry.size {
                changed = Some(ChangedSinceScan {
                    expected: entry.size,
                    actual: live.len(),
                });
                if decisive.is_none() {
                    decisive = Some(format!(
                        "changed since this scan: expected {} bytes, found {}",
                        entry.size,
                        live.len()
                    ));
                }
            }
        }

        Self {
            evaluated: true,
            skipped_reason: None,
            coverage: open_files.coverage(),
            coverage_note: open_files.coverage_note(),
            links,
            hardlink_enforced: guards.hardlink_check(),
            open,
            changed,
            decisive,
        }
    }

    fn describe(&self) -> String {
        if !self.evaluated {
            return format!(
                "not evaluated: {}",
                self.skipped_reason
                    .as_deref()
                    .unwrap_or("an earlier gate decided")
            );
        }
        let mut parts = Vec::new();
        parts.push(match &self.open {
            Some(hold) if !hold.pids.is_empty() => format!(
                "open by pid {}",
                hold.pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Some(_) => "open by a process whose pid is not visible without privileges".to_string(),
            None => "not open by another process".to_string(),
        });
        parts.push(match self.links {
            Some(links) if self.hardlink_enforced => format!("{links} link(s), hardlinks refused"),
            Some(links) => format!("{links} link(s), --allow-hardlinked permits moving it"),
            None => "link count unavailable".to_string(),
        });
        parts.push(match &self.changed {
            Some(changed) => format!(
                "CHANGED since this scan: {} bytes, now {}",
                changed.expected, changed.actual
            ),
            None => "size unchanged since this scan".to_string(),
        });
        let mut line = format!(
            "{}: {}",
            if self.decisive.is_some() {
                "blocking"
            } else {
                "clear"
            },
            parts.join("; ")
        );
        if let Some(note) = &self.coverage_note {
            line.push_str(&format!(" ({note})"));
        }
        line
    }

    fn to_json(&self) -> String {
        let open = match &self.open {
            Some(hold) => format!(
                "{{\"pids\":[{}],\"pid_visible\":{}}}",
                hold.pids
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                !hold.pids.is_empty()
            ),
            None => "null".to_string(),
        };
        let changed = match &self.changed {
            Some(changed) => format!(
                "{{\"expected\":{},\"actual\":{}}}",
                changed.expected, changed.actual
            ),
            None => "null".to_string(),
        };
        format!(
            "{{\"evaluated\":{},\"coverage\":{},\"coverage_note\":{},\"open\":{},\"links\":{},\
             \"hardlink_enforced\":{},\"changed_since_scan\":{},\"decisive\":{}}}",
            self.evaluated,
            json_string(coverage_name(self.coverage)),
            self.coverage_note
                .as_ref()
                .map(|note| json_string(note))
                .unwrap_or_else(|| "null".to_string()),
            open,
            self.links
                .map(|links| links.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.hardlink_enforced,
            changed,
            self.decisive
                .as_ref()
                .map(|reason| json_string(reason))
                .unwrap_or_else(|| "null".to_string())
        )
    }
}

/// The policy's view: how cold, according to which stamp, against which threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyReport {
    pub evaluated: bool,
    pub skipped_reason: Option<String>,
    pub last_access: Option<SystemTime>,
    pub access_source: AccessSource,
    pub idle: Option<Duration>,
    pub min_idle: Duration,
    pub is_symlink: bool,
    pub observed_accesses: u64,
    pub access_pin: u64,
    pub pinned: bool,
    /// True when atime and mtime are identical. On a `noatime`/`relatime` mount the atime
    /// is never advanced, so it can equal mtime even after real reads; the stamp is still
    /// what the mover trusts, but the reader deserves to know it may be the write time.
    pub atime_matches_mtime: bool,
}

impl PolicyReport {
    fn not_evaluated(reason: &str, policy: &Policy) -> Self {
        Self {
            evaluated: false,
            skipped_reason: Some(reason.to_string()),
            last_access: None,
            access_source: AccessSource::Atime,
            idle: None,
            min_idle: policy.min_idle,
            is_symlink: false,
            observed_accesses: 0,
            access_pin: policy.observed_access_pin,
            pinned: false,
            atime_matches_mtime: false,
        }
    }

    fn min_idle_days(&self) -> f64 {
        self.min_idle.as_secs_f64() / 86_400.0
    }

    fn describe(&self) -> String {
        if !self.evaluated {
            return format!(
                "not evaluated: {}",
                self.skipped_reason
                    .as_deref()
                    .unwrap_or("an earlier gate decided")
            );
        }
        let last = match self.last_access {
            Some(stamp) => format!("{} ({})", unix_seconds(stamp), format_time(stamp)),
            None => "unknown".to_string(),
        };
        let idle = match self.idle {
            Some(idle) => format!("{:.1} d", idle.as_secs_f64() / 86_400.0),
            None => "unknown".to_string(),
        };
        let atime_note = if self.atime_matches_mtime {
            "; NOTE atime equals mtime, so this may be the last write, not the last read \
             (noatime/relatime)"
        } else {
            ""
        };
        format!(
            "last use {last} via {}{atime_note}; idle {idle} against --min-idle-days {:.1}; {}; \
             {} witnessed access(es) in this run (pin {})",
            self.access_source.as_str(),
            self.min_idle_days(),
            if self.is_symlink {
                "already a symlink"
            } else {
                "not a symlink"
            },
            self.observed_accesses,
            self.access_pin
        )
    }

    fn to_json(&self) -> String {
        format!(
            "{{\"evaluated\":{},\"last_access\":{},\"last_access_iso\":{},\"access_source\":{},\
             \"idle_seconds\":{},\"min_idle_days\":{},\"is_symlink\":{},\"observed_accesses\":{},\
             \"access_pin\":{},\"pinned\":{},\"atime_matches_mtime\":{}}}",
            self.evaluated,
            self.last_access
                .map(unix_seconds)
                .map(|secs| secs.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.last_access
                .map(format_time)
                .map(|text| json_string(&text))
                .unwrap_or_else(|| "null".to_string()),
            json_string(self.access_source.as_str()),
            self.idle
                .map(|idle| idle.as_secs().to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.min_idle_days(),
            self.is_symlink,
            self.observed_accesses,
            self.access_pin,
            self.pinned,
            self.atime_matches_mtime
        )
    }
}

/// The answer, in the mover's vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// A sweep run now would move it onto `destination`.
    WouldMoveNow {
        destination: PathBuf,
        tier_index: usize,
    },
    /// In scope and unguarded, but not idle long enough yet.
    WouldMoveInDays { days: f64, min_idle_days: f64 },
    /// A lifecycle rule applies, but the file has not been idle past its `after_idle`.
    WouldMoveInDaysByRule {
        rule: String,
        days: f64,
        after_idle_days: f64,
    },
    /// The outermost gate refuses it; `reason` is that gate's sentence.
    WouldNeverMove { reason: String },
    /// Cold bytes are already in place: the path is a symlink into a configured tier.
    AlreadyMigrated {
        target: PathBuf,
        tier: Option<PathBuf>,
        tier_index: Option<usize>,
        resolves: bool,
    },
    /// A symlink that does not point into any configured cold tier.
    ForeignSymlink { target: PathBuf, resolves: bool },
    /// In scope and idle enough, but every tier is below its free-space floor this run.
    WaitingForRoom { reason: String },
    /// Nothing at the path.
    NotFound,
}

impl Verdict {
    pub fn outcome(&self) -> &'static str {
        match self {
            Verdict::WouldMoveNow { .. } => "would-move-now",
            Verdict::WouldMoveInDays { .. } => "would-move-in-days",
            Verdict::WouldMoveInDaysByRule { .. } => "would-move-in-days",
            Verdict::WouldNeverMove { .. } => "would-never-move",
            Verdict::AlreadyMigrated { .. } => "already-migrated",
            Verdict::ForeignSymlink { .. } => "foreign-symlink",
            Verdict::WaitingForRoom { .. } => "waiting-for-room",
            Verdict::NotFound => "not-found",
        }
    }

    fn describe(&self) -> String {
        match self {
            Verdict::WouldMoveNow {
                destination,
                tier_index,
            } => format!(
                "would move now -> {} (tier {tier_index})",
                destination.display()
            ),
            Verdict::WouldMoveInDays {
                days,
                min_idle_days,
            } => format!(
                "would move in {days:.1} days (idle {days:.1} d of the {min_idle_days:.1} d \
                 --min-idle-days requires)"
            ),
            Verdict::WouldMoveInDaysByRule {
                rule,
                days,
                after_idle_days,
            } => format!(
                "would move in {days:.1} days by rule `{rule}` (idle {days:.1} d of the \
                 {after_idle_days:.1} d after_idle it requires)"
            ),
            Verdict::WouldNeverMove { reason } => format!("would never move: {reason}"),
            Verdict::AlreadyMigrated {
                target,
                tier,
                tier_index,
                resolves,
            } => {
                let dangling = if *resolves {
                    ""
                } else {
                    " (target is DANGLING)"
                };
                match (tier, tier_index) {
                    (Some(tier), Some(index)) => format!(
                        "already migrated -> {} in tier {index} ({}){dangling}",
                        target.display(),
                        tier.display()
                    ),
                    _ => format!("already migrated -> {}{dangling}", target.display()),
                }
            }
            Verdict::ForeignSymlink { target, resolves } => format!(
                "would never move: symlink -> {} {} outside every --dest",
                target.display(),
                if *resolves { "resolves" } else { "dangles" }
            ),
            Verdict::WaitingForRoom { reason } => {
                format!("would move now, but {reason}")
            }
            Verdict::NotFound => "not found: nothing at this path".to_string(),
        }
    }

    fn to_json(&self) -> String {
        let (destination, tier, tier_index, in_days, reason) = match self {
            Verdict::WouldMoveNow {
                destination,
                tier_index,
            } => (
                json_string(&destination.to_string_lossy()),
                "null".to_string(),
                tier_index.to_string(),
                "null".to_string(),
                "a sweep run now would move it".to_string(),
            ),
            Verdict::WouldMoveInDays {
                days,
                min_idle_days,
            } => (
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                format!("{days}"),
                format!("idle {days:.1} d of the {min_idle_days:.1} d required"),
            ),
            Verdict::WouldMoveInDaysByRule {
                rule,
                days,
                after_idle_days,
            } => (
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                format!("{days}"),
                format!(
                    "rule `{rule}`: idle {days:.1} d of the {after_idle_days:.1} d after_idle \
                     required"
                ),
            ),
            Verdict::WouldNeverMove { reason } => (
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                reason.clone(),
            ),
            Verdict::AlreadyMigrated {
                target,
                tier,
                tier_index,
                ..
            } => (
                json_string(&target.to_string_lossy()),
                tier.as_ref()
                    .map(|tier| json_string(&tier.to_string_lossy()))
                    .unwrap_or_else(|| "null".to_string()),
                tier_index
                    .map(|index| index.to_string())
                    .unwrap_or_else(|| "null".to_string()),
                "null".to_string(),
                "the cold copy is already in place".to_string(),
            ),
            Verdict::ForeignSymlink { target, resolves } => (
                json_string(&target.to_string_lossy()),
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                format!("symlink resolves={resolves} outside every configured --dest"),
            ),
            Verdict::WaitingForRoom { reason } => (
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                reason.clone(),
            ),
            Verdict::NotFound => (
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                "null".to_string(),
                "nothing at this path".to_string(),
            ),
        };
        format!(
            "{{\"outcome\":{},\"destination\":{},\"tier\":{},\"tier_index\":{},\"in_days\":{},\
             \"reason\":{}}}",
            json_string(self.outcome()),
            destination,
            tier,
            tier_index,
            in_days,
            json_string(&reason)
        )
    }
}

/// What the catalog said, or why the filesystem was asked instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogReport {
    pub consulted: bool,
    pub source: &'static str,
    pub note: String,
    pub disagreements: Vec<String>,
    /// `lifecycle.accesses`, when the catalog answered. The access counter the mover's
    /// observed-access pin is really about; `None` when there is no catalog.
    pub accesses: Option<u64>,
    /// `lifecycle.pinned_until` (unix seconds), when the catalog answered.
    pub pinned_until: Option<u64>,
    /// True while that pin still holds. Reported separately from `pinned_until` so a machine
    /// reader does not have to re-derive "expired" from a clock it may not share.
    pub pin_live: bool,
}

impl CatalogReport {
    fn filesystem_only() -> Self {
        Self {
            consulted: false,
            source: "filesystem",
            note: "no catalog is configured, so these answers come from the filesystem — the \
                   same place a sweep reads."
                .to_string(),
            disagreements: Vec::new(),
            accesses: None,
            pinned_until: None,
            pin_live: false,
        }
    }

    fn describe(&self) -> String {
        if self.consulted {
            format!("consulted ({}); {}", self.source, self.note)
        } else {
            self.note.clone()
        }
    }

    fn to_json(&self) -> String {
        let mut disagreements = Vec::new();
        for item in &self.disagreements {
            disagreements.push(json_string(item));
        }
        format!(
            "{{\"consulted\":{},\"source\":{},\"note\":{},\"accesses\":{},\"pinned_until\":{},\
             \"pin_live\":{},\
             \"disagreements\":[{}]}}",
            self.consulted,
            json_string(self.source),
            json_string(&self.note),
            self.accesses
                .map(|accesses| accesses.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.pinned_until
                .map(|until| until.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.pin_live,
            disagreements.join(",")
        )
    }
}

/// What `policy.toml` decided for this path — the rule that fired, or the exclusion that
/// stopped it (§5, §5.1), or that no policy is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleReport {
    /// False when an earlier gate refused, so the rules were never reached.
    pub evaluated: bool,
    pub skipped_reason: Option<String>,
    /// The rule whose `match` covered the path, when one did.
    pub matched_rule: Option<String>,
    /// A machine-stable word for the decision: `fired`, `too-warm`, `pinned`, `no-rule`,
    /// `not-managed-tier`, `no-down-from`, `up`, `fallback`, `not-applicable`.
    pub decision: &'static str,
    /// The rule that decided or blocked, when a named rule did.
    pub rule: Option<String>,
    /// One sentence a human can act on.
    pub detail: String,
}

impl RuleReport {
    /// No `policy.toml`: the flag-driven decision, exactly what the tool did before rules.
    fn fallback() -> Self {
        Self {
            evaluated: true,
            skipped_reason: None,
            matched_rule: None,
            decision: "fallback",
            rule: None,
            detail: "no policy.toml is configured, so the flag-driven decision \
                     (--min-idle-days) applies"
                .to_string(),
        }
    }

    fn not_evaluated(reason: &str) -> Self {
        Self {
            evaluated: false,
            skipped_reason: Some(reason.to_string()),
            matched_rule: None,
            decision: "not-applicable",
            rule: None,
            detail: reason.to_string(),
        }
    }

    fn from_down(decision: &DownDecision) -> Self {
        let (word, detail) = match decision {
            DownDecision::NoRule => ("no-rule", decision.describe()),
            DownDecision::Pinned { .. } => ("pinned", decision.describe()),
            DownDecision::NotOnManagedTier => ("not-managed-tier", decision.describe()),
            DownDecision::NoDownFrom { .. } => ("no-down-from", decision.describe()),
            DownDecision::TooWarm { .. } => ("too-warm", decision.describe()),
            DownDecision::Down { .. } => ("fired", decision.describe()),
        };
        Self {
            evaluated: true,
            skipped_reason: None,
            matched_rule: decision.rule().map(str::to_string),
            decision: word,
            rule: decision.rule().map(str::to_string),
            detail,
        }
    }

    fn from_up(rule: &str, from: &str, to: &str) -> Self {
        Self {
            evaluated: true,
            skipped_reason: None,
            matched_rule: Some(rule.to_string()),
            decision: "up",
            rule: Some(rule.to_string()),
            detail: format!(
                "rule `{rule}`: accessed again on `{to}`, a read promotes it back to `{from}` \
                 (recall is the namespace provider's job, §8 P2)"
            ),
        }
    }

    fn describe(&self) -> String {
        if !self.evaluated {
            return format!(
                "not evaluated: {}",
                self.skipped_reason
                    .as_deref()
                    .unwrap_or("an earlier gate decided")
            );
        }
        match &self.rule {
            Some(rule) => format!("rule `{rule}` ({}) — {}", self.decision, self.detail),
            None => format!("{} — {}", self.decision, self.detail),
        }
    }

    fn to_json(&self) -> String {
        format!(
            "{{\"evaluated\":{},\"decision\":{},\"matched_rule\":{},\"rule\":{},\"detail\":{}}}",
            self.evaluated,
            json_string(self.decision),
            self.matched_rule
                .as_ref()
                .map(|rule| json_string(rule))
                .unwrap_or_else(|| "null".to_string()),
            self.rule
                .as_ref()
                .map(|rule| json_string(rule))
                .unwrap_or_else(|| "null".to_string()),
            json_string(&self.detail)
        )
    }
}

/// What the catalog would answer for one namespace path. Nothing constructs this yet —
/// it exists so the seam below has a shape to slot into, and so the disagreement path can
/// be tested before the catalog exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAnswer {
    /// Unix seconds of the last observed access.
    pub last_access: u64,
    /// `present` | `offloaded` | `restoring`, per `docs/design.md` §3.
    pub state: String,
    /// The tier of record, when the object is offloaded.
    pub tier: Option<String>,
    /// The key the bytes live under on that tier (path on fs, key on object storage).
    pub storage_key: Option<PathBuf>,
    /// Which lifecycle rule last decided a transition, if any.
    pub rule: Option<String>,
    /// `lifecycle.accesses`: the observed counter since ingest.
    pub accesses: u64,
    /// `lifecycle.pinned_until` (unix seconds): a pin wins over the idle rule.
    pub pinned_until: Option<u64>,
}

/// The single seam where the catalog (issue #16) answers "where does this object live
/// and when was it last accessed".
///
/// When a catalog file exists it is consulted, and its answer is compared with the
/// filesystem: on disagreement `explain` reports the disagreement (see
/// [`CatalogReport::disagreements`]) rather than resolving it silently, because a silent
/// resolution is how a UI ends up trusting a number nothing on disk agrees with. When no
/// catalog exists this returns `None` and the filesystem is the only source — the same
/// source the mover acts on, which is what keeps the explanation honest.
///
/// What the catalog provides, and this returns:
///
/// * **existence** — `name(path) -> object_id`: `None` when the path is not named.
/// * **managed state** — `object.state` (`present` / `offloaded` / `restoring`).
/// * **location** — the primary `location` row: `{ tier, storage_key }`.
/// * **last access** — `lifecycle.last_access` (unix seconds).
/// * **access count** — `lifecycle.accesses`.
/// * **pin** — `lifecycle.pinned_until`.
/// * **rule** — `lifecycle.rule`, the rule that last fired.
fn catalog_answer(context: &ExplainContext<'_>) -> Option<CatalogAnswer> {
    let catalog_path = context.catalog.as_deref()?;
    // Names are stored relative to the watch root — the same key `catalog sync` ingested.
    // A path outside the tree has no namespace key and therefore no catalog answer.
    let relative = context.path.strip_prefix(context.watch).ok()?;
    // Never create a catalog at query time: an empty one would answer "not in the
    // catalog" for a tree that is fine. `main` only sets `Some` for a file that already
    // exists; this is belt-and-braces against a library caller.
    if !catalog_path.is_file() {
        return None;
    }
    let catalog = Catalog::open(catalog_path).ok()?;
    let record = catalog
        .record_for_path(&relative.to_string_lossy())
        .ok()??;
    let primary = record.primary();
    Some(CatalogAnswer {
        last_access: record.last_access.unwrap_or(0).max(0) as u64,
        state: record.state.clone(),
        tier: primary.map(|location| location.tier.clone()),
        storage_key: primary.map(|location| PathBuf::from(&location.storage_key)),
        rule: record.rule.clone(),
        accesses: record.accesses.unwrap_or(0),
        pinned_until: record.pinned_until.map(|until| until.max(0) as u64),
    })
}

/// Explain one path, consulting the catalog when one is configured.
pub fn explain(context: &ExplainContext<'_>) -> Explanation {
    let answer = catalog_answer(context);
    explain_with(context, answer.as_ref())
}

/// Explain one path, with an explicit catalog answer injected. The public entry point is
/// [`explain`]; this exists so the feature tests — including the one that proves a
/// disagreement is reported — can run without a catalog process.
pub fn explain_with(context: &ExplainContext<'_>, catalog: Option<&CatalogAnswer>) -> Explanation {
    let path = context.path;
    let watch = context.watch;
    let relative = path.strip_prefix(watch).ok().map(Path::to_path_buf);

    let metadata = fs::symlink_metadata(path).ok();
    let filesystem_last_access = metadata
        .as_ref()
        .filter(|metadata| metadata.is_file() && !metadata.is_symlink())
        .map(|metadata| disk_management::last_use(metadata).0);
    let catalog_report = catalog_report(
        catalog,
        metadata.as_ref(),
        filesystem_last_access,
        unix_seconds(SystemTime::now()),
    );

    let Some(metadata) = metadata else {
        return Explanation {
            path: path.to_path_buf(),
            watch: watch.to_path_buf(),
            relative,
            exists: false,
            scope: ScopeReport::NotApplicable {
                reason: "nothing at this path".to_string(),
            },
            guards: GuardsReport::not_evaluated("nothing at this path", context.guards),
            policy: PolicyReport::not_evaluated("nothing at this path", context.policy),
            verdict: Verdict::NotFound,
            catalog: catalog_report,
            rules: RuleReport::not_evaluated("nothing at this path"),
        };
    };

    // A symlink is the mover's *output*. It never reaches scope, guards or policy — the
    // walk skips it first — so the explanation short-circuits here and resolves where the
    // bytes actually live.
    if metadata.is_symlink() {
        return explain_symlink(context, path, watch, relative, catalog_report);
    }

    // Directories, sockets, fifos and devices are not move candidates either.
    if !metadata.is_file() {
        return Explanation {
            path: path.to_path_buf(),
            watch: watch.to_path_buf(),
            relative,
            exists: true,
            scope: ScopeReport::NotApplicable {
                reason: "not a regular file".to_string(),
            },
            guards: GuardsReport::not_evaluated("not a regular file", context.guards),
            policy: PolicyReport::not_evaluated("not a regular file", context.policy),
            verdict: Verdict::WouldNeverMove {
                reason: "not a regular file; the mover only moves regular files".to_string(),
            },
            catalog: catalog_report,
            rules: RuleReport::not_evaluated("not a regular file"),
        };
    }

    let (last_access, access_source) = disk_management::last_use(&metadata);
    let entry = FileEntry {
        path: path.to_path_buf(),
        relative: relative.clone().unwrap_or_default(),
        size: metadata.len(),
        allocated: disk_management::allocated_bytes(&metadata),
        last_access,
        is_symlink: false,
    };

    // ---- 1. scope ----------------------------------------------------------------
    if relative.is_none() {
        let refusal = ScopeReport::OutsideWatch {
            path: path.to_path_buf(),
            watch: watch.to_path_buf(),
        };
        let reason = format!(
            "{} is not under the watched tree {}",
            path.display(),
            watch.display()
        );
        return Explanation {
            path: path.to_path_buf(),
            watch: watch.to_path_buf(),
            relative,
            exists: true,
            scope: refusal,
            guards: GuardsReport::not_evaluated("scope refused first", context.guards),
            policy: PolicyReport::not_evaluated("scope refused first", context.policy),
            verdict: Verdict::WouldNeverMove { reason },
            catalog: catalog_report,
            rules: RuleReport::not_evaluated("scope refused first"),
        };
    }

    // Empty file is raised by the mover immediately after scope and before the guards, so
    // it belongs to this stage rather than a new one.
    let refusal = context
        .scope
        .refusal(&entry)
        .or_else(|| (entry.size == 0).then_some(ScopeRefusal::Empty));
    if let Some(refusal) = refusal {
        return Explanation {
            path: path.to_path_buf(),
            watch: watch.to_path_buf(),
            relative,
            exists: true,
            scope: ScopeReport::Out(refusal.clone()),
            guards: GuardsReport::not_evaluated("scope refused first", context.guards),
            policy: PolicyReport::not_evaluated("scope refused first", context.policy),
            verdict: Verdict::WouldNeverMove {
                reason: refusal.describe(),
            },
            catalog: catalog_report,
            rules: RuleReport::not_evaluated("scope refused first"),
        };
    }
    let scope = ScopeReport::In {
        matched_include: context.scope.matched_include(&entry),
        note: window_note(context.scope, entry.size),
    };

    // ---- 2. guards ---------------------------------------------------------------
    let guards = GuardsReport::evaluate(context.guards, &entry);
    if let Some(reason) = guards.decisive.clone() {
        return Explanation {
            path: path.to_path_buf(),
            watch: watch.to_path_buf(),
            relative,
            exists: true,
            scope,
            guards,
            policy: PolicyReport::not_evaluated("guards refused first", context.policy),
            verdict: Verdict::WouldNeverMove { reason },
            catalog: catalog_report,
            rules: RuleReport::not_evaluated("guards refused first"),
        };
    }

    // ---- 3. policy ---------------------------------------------------------------
    let now = SystemTime::now();
    let last_access = context.tracker.last_access(path, entry.last_access);
    let idle = now.duration_since(last_access).unwrap_or(Duration::ZERO);
    let observed_accesses = context.tracker.observed_accesses(path);
    let pinned = context.policy.observed_access_pin > 0
        && observed_accesses >= context.policy.observed_access_pin;
    let atime_matches_mtime = matches!(
        (metadata.accessed(), metadata.modified()),
        (Ok(accessed), Ok(modified)) if accessed == modified
    );
    let policy = PolicyReport {
        evaluated: true,
        skipped_reason: None,
        last_access: Some(last_access),
        access_source,
        idle: Some(idle),
        min_idle: context.policy.min_idle,
        is_symlink: false,
        observed_accesses,
        access_pin: context.policy.observed_access_pin,
        pinned,
        atime_matches_mtime,
    };

    // ---- 4. lifecycle rules ------------------------------------------------------
    // A rule decides the destination as well as the idle gate, so the verdict below is
    // the rule's when one governs this tree. With no `policy.toml` this is `None` and the
    // flag-driven path stands, exactly as before rules existed.
    let current_tier = context.lifecycle.and_then(|lc| lc.current_tier(path));
    let (rules, rule_decision) = match context.lifecycle {
        None => (RuleReport::fallback(), None),
        Some(lifecycle) => {
            let decision = lifecycle.evaluate_down(&entry.relative, current_tier, idle);
            (RuleReport::from_down(&decision), Some(decision))
        }
    };

    // ---- 5. verdict --------------------------------------------------------------
    // A live catalog pin comes first: it is the operator's explicit "not this one", it wins
    // over policy (§5), and the verdict says so with the instant it lapses. An *expired* pin
    // does not appear here — policy governs again — but the catalog note still names it, so
    // the expiry is shown rather than silently honoured as "no pin".
    let live_pin = catalog
        .and_then(|answer| answer.pinned_until)
        .filter(|until| *until > unix_seconds(now));
    let verdict = if let Some(until) = live_pin {
        Verdict::WouldNeverMove {
            reason: format!(
                "pinned in the catalog until {} — a pin wins over policy (§5)",
                format_rfc3339(until as i64)
            ),
        }
    } else if pinned {
        Verdict::WouldNeverMove {
            reason: format!(
                "accessed {observed_accesses} time(s) during this run, at or above the pin of \
                 {} — a file just seen being used is not cold",
                context.policy.observed_access_pin
            ),
        }
    } else if let Some(decision) = &rule_decision {
        match decision {
            DownDecision::Down { rule, to, .. } => {
                match tier_index_for_name(context, to).and_then(|index| {
                    disk_management::destination_with_room(
                        &context.dests[index],
                        entry.allocated,
                        context.min_free,
                    )
                    .map(|destination| (index, destination))
                }) {
                    Some((tier_index, destination)) => Verdict::WouldMoveNow {
                        destination: destination.join(&entry.relative),
                        tier_index,
                    },
                    None => Verdict::WaitingForRoom {
                        reason: format!(
                            "rule `{rule}` targets tier `{to}`, but no --dest for it has room \
                             for {} above the {} free-space floor",
                            scope::human_bytes(entry.allocated),
                            scope::human_bytes(context.min_free)
                        ),
                    },
                }
            }
            DownDecision::TooWarm {
                rule,
                idle,
                after_idle,
                ..
            } => Verdict::WouldMoveInDaysByRule {
                rule: rule.clone(),
                days: idle.as_secs_f64() / 86_400.0,
                after_idle_days: after_idle.as_secs_f64() / 86_400.0,
            },
            other => Verdict::WouldNeverMove {
                reason: other.describe(),
            },
        }
    } else if idle < context.policy.min_idle {
        Verdict::WouldMoveInDays {
            days: idle.as_secs_f64() / 86_400.0,
            min_idle_days: context.policy.min_idle.as_secs_f64() / 86_400.0,
        }
    } else {
        match tier_with_room(context.dests, entry.allocated, context.min_free) {
            Some((tier_index, destination)) => Verdict::WouldMoveNow {
                destination: destination.join(&entry.relative),
                tier_index,
            },
            None => Verdict::WaitingForRoom {
                reason: format!(
                    "no cold tier has room for {} above the {} free-space floor",
                    scope::human_bytes(entry.allocated),
                    scope::human_bytes(context.min_free)
                ),
            },
        }
    };

    Explanation {
        path: path.to_path_buf(),
        watch: watch.to_path_buf(),
        relative,
        exists: true,
        scope,
        guards,
        policy,
        verdict,
        catalog: catalog_report,
        rules,
    }
}

/// The index of the `--dest` root that is configured as tier `name`, if any. A rule names
/// a tier, and the mover can only act on a tier it was given a root for.
fn tier_index_for_name(context: &ExplainContext<'_>, name: &str) -> Option<usize> {
    let lifecycle = context.lifecycle?;
    context.dests.iter().position(|dest| {
        lifecycle
            .tiers()
            .tier_for_root(dest)
            .is_some_and(|tier| tier.name == name)
    })
}

/// A path that is already a symlink: the mover's output, resolved back to its cold home.
fn explain_symlink(
    context: &ExplainContext<'_>,
    path: &Path,
    watch: &Path,
    relative: Option<PathBuf>,
    catalog: CatalogReport,
) -> Explanation {
    let not_applicable = |reason: &str| ScopeReport::NotApplicable {
        reason: reason.to_string(),
    };
    let base = |verdict: Verdict, rules: RuleReport| Explanation {
        path: path.to_path_buf(),
        watch: watch.to_path_buf(),
        relative: relative.clone(),
        exists: true,
        scope: not_applicable("already a symlink; the mover skips symlinks before scope"),
        guards: GuardsReport::not_evaluated("already a symlink", context.guards),
        policy: PolicyReport::not_evaluated("already a symlink", context.policy),
        verdict,
        catalog: catalog.clone(),
        rules,
    };

    let target = match fs::read_link(path) {
        Ok(target) => target,
        Err(err) => {
            return base(
                Verdict::WouldNeverMove {
                    reason: format!("symlink could not be read: {err}"),
                },
                RuleReport::not_evaluated("the symlink could not be read"),
            );
        }
    };
    // A relative link is relative to the directory holding the link.
    let resolved = if target.is_absolute() {
        target.clone()
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&target)
    };
    let resolves = fs::metadata(&resolved).is_ok();
    if !resolves {
        return base(
            Verdict::WouldNeverMove {
                reason: format!(
                    "dangling symlink -> {} (the cold copy is missing)",
                    target.display()
                ),
            },
            RuleReport::not_evaluated("the cold copy is missing"),
        );
    }

    // The bytes live on a tier now: if a rule promotes on access, name it. Without a
    // policy this is the fallback line, exactly as a hot file gets.
    let resolved_tier = context.lifecycle.and_then(|lc| lc.current_tier(&resolved));
    let rules = match (context.lifecycle, relative.as_deref()) {
        (Some(lifecycle), Some(relative)) => match lifecycle.evaluate_up(relative, resolved_tier) {
            Some(rule) => {
                // The `from` of the transition that put the bytes there — the tier an `up`
                // promotes back to. `evaluate_up` only returns a rule with a `down` whose
                // `to` is the current tier, so one of the entries always names it.
                let from = resolved_tier
                    .and_then(|tier| rule.down_from(tier))
                    .map(|down| down.from.as_str())
                    .or_else(|| rule.downs.first().map(|down| down.from.as_str()))
                    .unwrap_or("its home tier");
                RuleReport::from_up(&rule.name, from, resolved_tier.unwrap_or("its cold tier"))
            }
            None => RuleReport::not_evaluated(
                "the cold copy is in place; no up rule promotes it on access",
            ),
        },
        _ => RuleReport::fallback(),
    };

    match tier_containing(context.dests, &resolved) {
        Some((tier_index, tier)) => base(
            Verdict::AlreadyMigrated {
                target,
                tier: Some(tier),
                tier_index: Some(tier_index),
                resolves: true,
            },
            rules,
        ),
        None => base(
            Verdict::ForeignSymlink {
                target,
                resolves: true,
            },
            rules,
        ),
    }
}

/// First cold tier with room for `needed` above `min_free`, and its index.
fn tier_with_room(dests: &[PathBuf], needed: u64, min_free: u64) -> Option<(usize, PathBuf)> {
    dests.iter().enumerate().find_map(|(index, dest)| {
        disk_management::destination_with_room(dest, needed, min_free).map(|tier| (index, tier))
    })
}

/// The configured tier a resolved path lives under, if any.
fn tier_containing(dests: &[PathBuf], resolved: &Path) -> Option<(usize, PathBuf)> {
    let real = resolved.canonicalize().ok()?;
    dests.iter().enumerate().find_map(|(index, dest)| {
        let canonical = dest.canonicalize().unwrap_or_else(|_| dest.clone());
        if real.starts_with(&canonical) {
            Some((index, dest.clone()))
        } else {
            None
        }
    })
}

/// Compare a catalog answer with the filesystem, if one was given.
///
/// `now` is the clock a pin's liveness is decided against; it is passed in rather than read
/// here so a test can drive both the answer and the instant that judges it.
fn catalog_report(
    catalog: Option<&CatalogAnswer>,
    metadata: Option<&fs::Metadata>,
    filesystem_last_access: Option<SystemTime>,
    now: u64,
) -> CatalogReport {
    let Some(answer) = catalog else {
        return CatalogReport::filesystem_only();
    };
    let mut disagreements = Vec::new();
    if let Some(stamp) = filesystem_last_access {
        let filesystem = unix_seconds(stamp) as i64;
        let catalog_secs = answer.last_access as i64;
        // One second of slack: the two clocks stamp the same read and need not agree to
        // the sub-second. A real disagreement is hundreds of seconds, not one.
        if (catalog_secs - filesystem).abs() > 1 {
            disagreements.push(format!(
                "catalog says last access {catalog_secs} but the filesystem says {filesystem}"
            ));
        }
    }
    if answer.tier.is_some() {
        if let Some(metadata) = metadata {
            if metadata.is_file() && !metadata.is_symlink() {
                disagreements.push(
                    "catalog places this object on a tier, but the filesystem has a regular \
                     file at this path (not migrated)"
                        .to_string(),
                );
            }
        }
    }
    // The pin is stated with its expiry *and* whether that expiry has passed: "pinned until
    // <instant>" without "in force"/"expired" would leave the reader to compare clocks, and
    // §9 asks for the lapse to be visible rather than implied.
    let pin_live = answer.pinned_until.is_some_and(|until| until > now);
    let pin_note = match answer.pinned_until {
        Some(until) if pin_live => {
            format!(", pinned until {} (in force)", format_rfc3339(until as i64))
        }
        Some(until) => format!(
            ", pin expired {} (blocks nothing)",
            format_rfc3339(until as i64)
        ),
        None => String::new(),
    };
    CatalogReport {
        consulted: true,
        source: "catalog",
        note: format!(
            "catalog state {:?}, {} observed access(es){}; {}",
            answer.state,
            answer.accesses,
            pin_note,
            answer
                .rule
                .as_ref()
                .map(|rule| format!("last rule {rule:?}"))
                .unwrap_or_else(|| "no rule recorded".to_string())
        ),
        disagreements,
        accesses: Some(answer.accesses),
        pinned_until: answer.pinned_until,
        pin_live,
    }
}

/// Human wording for the size window, naming the bound that lets the path through.
fn window_note(scope: &Scope, size: u64) -> String {
    match scope.max_size() {
        Some(max) => format!(
            "{} is within the [{}, {}] size window",
            scope::human_bytes(size),
            scope::human_bytes(scope.min_size()),
            scope::human_bytes(max)
        ),
        None if scope.min_size() == 0 => "no size bounds configured".to_string(),
        None => format!(
            "{} is within the [{}, unlimited) size window",
            scope::human_bytes(size),
            scope::human_bytes(scope.min_size())
        ),
    }
}

fn coverage_name(coverage: Coverage) -> &'static str {
    match coverage {
        Coverage::Complete => "complete",
        Coverage::OwnProcessesOnly => "own-processes-only",
        Coverage::Unsupported => "unsupported",
    }
}

fn unix_seconds(stamp: SystemTime) -> u64 {
    stamp
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// RFC 3339 in UTC, without a date crate: the tool has no business growing a dependency
/// just to print a timestamp, and the arithmetic is small enough to test directly.
fn format_time(stamp: SystemTime) -> String {
    match stamp.duration_since(UNIX_EPOCH) {
        Ok(since) => format_rfc3339(since.as_secs() as i64),
        Err(_) => "before 1970".to_string(),
    }
}

fn format_rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// Days from the Unix epoch to a civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Minimal JSON string escaping — enough for paths and our own ASCII reason strings, and
/// tested, rather than the reflection machinery of a JSON dependency. Matches `audit`'s.
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
    use crate::opened::OpenFiles;
    use std::fs;

    fn context<'a>(
        path: &'a Path,
        watch: &'a Path,
        dests: &'a [PathBuf],
        scope: &'a Scope,
        policy: &'a Policy,
        guards: &'a Guards,
        tracker: &'a UsageTracker,
    ) -> ExplainContext<'a> {
        ExplainContext {
            path,
            watch,
            dests,
            scope,
            policy,
            guards,
            tracker,
            min_free: 0,
            catalog: None,
            lifecycle: None,
        }
    }

    fn policy(min_idle_days: f64) -> Policy {
        Policy {
            min_idle: Duration::from_secs_f64(min_idle_days * 86_400.0),
            observed_access_pin: 1,
            limit: 10,
            dry_run: true,
        }
    }

    fn set_times(path: &Path, when: SystemTime) {
        let times = fs::FileTimes::new().set_accessed(when).set_modified(when);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(times)
            .unwrap();
    }

    fn days_ago(days: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(days * 86_400)
    }

    #[test]
    fn a_cold_in_scope_file_would_move_now() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let path = watch.join("cold.bin");
        fs::write(&path, b"payload").unwrap();
        set_times(&path, days_ago(90));

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];

        let explanation = explain(&context(
            &path, &watch, &dests, &scope, &policy, &guards, &tracker,
        ));

        match &explanation.verdict {
            Verdict::WouldMoveNow {
                destination,
                tier_index,
            } => {
                assert_eq!(*tier_index, 0);
                assert_eq!(destination, &cold.join("cold.bin"));
            }
            other => panic!("expected would-move-now, got {other:?}"),
        }
        assert!(explanation.is_managed());
        assert_eq!(explanation.exit_code(), EXIT_MANAGED);
    }

    #[test]
    fn a_warm_file_would_move_in_days() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let path = watch.join("warm.bin");
        fs::write(&path, b"payload").unwrap();
        set_times(&path, days_ago(5));

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];

        let explanation = explain(&context(
            &path, &watch, &dests, &scope, &policy, &guards, &tracker,
        ));

        assert!(
            matches!(explanation.verdict, Verdict::WouldMoveInDays { .. }),
            "got {:?}",
            explanation.verdict
        );
        assert_eq!(explanation.policy.access_source, AccessSource::Atime);
        assert!(!explanation.is_managed());
        assert_eq!(explanation.exit_code(), EXIT_NOT_MOVED);
    }

    /// The ordering guarantee: a file that is both out of scope and too warm reports the
    /// *outermost* reason — scope — and the policy stage is marked not evaluated, because
    /// that is the gate the mover never reaches.
    #[test]
    fn scope_is_reported_ahead_of_a_warm_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(watch.join("node_modules")).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let path = watch.join("node_modules/left-pad.js");
        fs::write(&path, b"module").unwrap();
        // Warm: a sweep would never reach the idle check, but if it did, it would say warm.
        set_times(&path, SystemTime::now());

        let scope = Scope::build(&[], &["node_modules".to_string()], 0, None).unwrap();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];

        let explanation = explain(&context(
            &path, &watch, &dests, &scope, &policy, &guards, &tracker,
        ));

        match &explanation.scope {
            ScopeReport::Out(ScopeRefusal::Excluded { pattern }) => {
                assert_eq!(pattern, "node_modules");
            }
            other => panic!("expected the exclude rule, got {other:?}"),
        }
        match &explanation.verdict {
            Verdict::WouldNeverMove { reason } => {
                assert!(
                    reason.contains("node_modules"),
                    "reason must name the rule: {reason}"
                );
            }
            other => panic!("expected would-never-move, got {other:?}"),
        }
        assert!(
            !explanation.guards.evaluated,
            "guards must not be evaluated once scope refused"
        );
        assert!(
            !explanation.policy.evaluated,
            "policy must not be evaluated once scope refused"
        );
        assert_eq!(explanation.exit_code(), EXIT_NOT_MOVED);
    }

    /// A path that is out of scope *and* too small reports the un-included rule, not the
    /// size floor: include is the first gate.
    #[test]
    fn include_is_reported_ahead_of_the_size_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        fs::create_dir_all(&watch).unwrap();
        let path = watch.join("tiny.bin");
        fs::write(&path, b"hi").unwrap();

        let scope = Scope::build(&["media/**".to_string()], &[], 1 << 30, None).unwrap();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let explanation = explain(&context(
            &path,
            &watch,
            &[],
            &scope,
            &policy,
            &guards,
            &tracker,
        ));

        match &explanation.scope {
            ScopeReport::Out(ScopeRefusal::NotIncluded { patterns }) => {
                assert_eq!(patterns, &["media/**".to_string()]);
            }
            other => panic!("expected the include rule, got {other:?}"),
        }
    }

    #[test]
    fn a_size_floor_is_named_with_both_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        fs::create_dir_all(&watch).unwrap();
        let path = watch.join("small.bin");
        fs::write(&path, b"12345").unwrap();

        let scope = Scope::build(&[], &[], 1 << 20, None).unwrap();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let explanation = explain(&context(
            &path,
            &watch,
            &[],
            &scope,
            &policy,
            &guards,
            &tracker,
        ));

        match explanation.scope {
            ScopeReport::Out(refusal) => {
                assert_eq!(refusal.kind(), "too-small");
                assert!(
                    refusal.describe().contains("1.0 MiB") && refusal.describe().contains("5 B"),
                    "the floor and the size must both appear: {}",
                    refusal.describe()
                );
            }
            other => panic!("expected too-small, got {other:?}"),
        }
    }

    #[test]
    fn an_open_file_is_blocked_and_the_guard_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let path = watch.join("held.bin");
        fs::write(&path, b"payload").unwrap();
        set_times(&path, days_ago(90));

        let scope = Scope::everything();
        let policy = policy(30.0);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];

        // A file nothing holds is clear and would move.
        let clear_guards = Guards::new(OpenFiles::default(), true);
        let clear = explain(&context(
            &path,
            &watch,
            &dests,
            &scope,
            &policy,
            &clear_guards,
            &tracker,
        ));
        assert!(clear.guards.evaluated);
        assert!(matches!(clear.verdict, Verdict::WouldMoveNow { .. }));
        assert!(clear.guards.decisive.is_none());

        // Now pretend a process holds it open, and that the scan could name that pid.
        let id = FileId::of(&path).unwrap();
        let mut open = OpenFiles::default();
        open.hold_for_test(id, Some(4242));
        let held_guards = Guards::new(open, true);
        let held = explain(&context(
            &path,
            &watch,
            &dests,
            &scope,
            &policy,
            &held_guards,
            &tracker,
        ));

        match &held.verdict {
            Verdict::WouldNeverMove { reason } => {
                assert!(reason.contains("4242"), "the pid must be named: {reason}");
            }
            other => panic!("expected an open-file refusal, got {other:?}"),
        }
        assert!(held.guards.decisive.is_some());
        assert!(
            !held.policy.evaluated,
            "policy must not run after a guard blocks"
        );
        assert_eq!(held.exit_code(), EXIT_NOT_MOVED);
    }

    #[test]
    fn a_hardlinked_file_is_blocked_unless_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let path = watch.join("pair.bin");
        let other = tmp.path().join("other-link.bin");
        fs::write(&path, b"payload").unwrap();
        fs::hard_link(&path, &other).unwrap();
        set_times(&path, days_ago(90));

        let scope = Scope::everything();
        let policy = policy(30.0);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];

        let refusing = Guards::new(OpenFiles::default(), true);
        let explanation = explain(&context(
            &path, &watch, &dests, &scope, &policy, &refusing, &tracker,
        ));
        assert_eq!(explanation.guards.links, Some(2));
        match &explanation.verdict {
            Verdict::WouldNeverMove { reason } => assert!(reason.contains("2 links"), "{reason}"),
            other => panic!("expected hardlink refusal, got {other:?}"),
        }
        assert!(!explanation.policy.evaluated);
        assert_eq!(explanation.exit_code(), EXIT_NOT_MOVED);

        let allowing = Guards::new(OpenFiles::default(), false);
        let explanation = explain(&context(
            &path, &watch, &dests, &scope, &policy, &allowing, &tracker,
        ));
        assert!(matches!(explanation.verdict, Verdict::WouldMoveNow { .. }));
        assert_eq!(explanation.guards.links, Some(2));
    }

    #[test]
    fn a_migrated_path_answers_with_the_cold_location_and_tier() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(cold.join("moved.bin"), b"payload").unwrap();
        let link = watch.join("moved.bin");
        std::os::unix::fs::symlink(Path::new("../cold/moved.bin"), &link).unwrap();

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];
        let explanation = explain(&context(
            &link, &watch, &dests, &scope, &policy, &guards, &tracker,
        ));

        match &explanation.verdict {
            Verdict::AlreadyMigrated {
                tier,
                tier_index,
                resolves,
                ..
            } => {
                assert_eq!(tier.as_deref(), Some(cold.as_path()));
                assert_eq!(*tier_index, Some(0));
                assert!(resolves);
            }
            other => panic!("expected already-migrated, got {other:?}"),
        }
        assert!(explanation.is_managed());
        assert_eq!(explanation.exit_code(), EXIT_MANAGED);
    }

    #[test]
    fn a_dangling_symlink_is_not_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let link = watch.join("gone.bin");
        std::os::unix::fs::symlink(Path::new("../cold/gone.bin"), &link).unwrap();

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];
        let explanation = explain(&context(
            &link, &watch, &dests, &scope, &policy, &guards, &tracker,
        ));

        match &explanation.verdict {
            Verdict::WouldNeverMove { reason } => assert!(reason.contains("dangling"), "{reason}"),
            other => panic!("expected a dangling refusal, got {other:?}"),
        }
        assert_eq!(explanation.exit_code(), EXIT_NOT_MOVED);
    }

    #[test]
    fn a_nonexistent_path_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        fs::create_dir_all(&watch).unwrap();
        let path = watch.join("never-existed.bin");

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let explanation = explain(&context(
            &path,
            &watch,
            &[],
            &scope,
            &policy,
            &guards,
            &tracker,
        ));

        assert!(!explanation.exists);
        assert_eq!(explanation.verdict, Verdict::NotFound);
        assert_eq!(explanation.exit_code(), EXIT_NOT_MOVED);
    }

    #[test]
    fn a_path_outside_the_watched_tree_is_not_managed() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        fs::create_dir_all(&watch).unwrap();
        let path = tmp.path().join("elsewhere.bin");
        fs::write(&path, b"payload").unwrap();

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let explanation = explain(&context(
            &path,
            &watch,
            &[],
            &scope,
            &policy,
            &guards,
            &tracker,
        ));

        assert!(matches!(
            explanation.scope,
            ScopeReport::OutsideWatch { .. }
        ));
        assert_eq!(explanation.exit_code(), EXIT_NOT_MOVED);
    }

    #[test]
    fn a_catalog_disagreement_is_reported_not_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let path = watch.join("a.bin");
        fs::write(&path, b"payload").unwrap();
        set_times(&path, days_ago(90));

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];
        let answer = CatalogAnswer {
            last_access: unix_seconds(days_ago(1)),
            state: "present".to_string(),
            tier: None,
            storage_key: None,
            rule: Some("intelligent-tiering".to_string()),
            accesses: 3,
            pinned_until: None,
        };
        let explanation = explain_with(
            &context(&path, &watch, &dests, &scope, &policy, &guards, &tracker),
            Some(&answer),
        );

        assert!(explanation.catalog.consulted);
        assert_eq!(explanation.catalog.disagreements.len(), 1);
        assert!(
            explanation.catalog.disagreements[0].contains("last access"),
            "{}",
            explanation.catalog.disagreements[0]
        );
        // The verdict still comes from the filesystem, the source the mover acts on.
        assert!(matches!(explanation.verdict, Verdict::WouldMoveNow { .. }));
    }

    #[test]
    fn json_carries_the_four_stages_and_escapes_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        let path = watch.join("we\"ird.bin");
        fs::write(&path, b"payload").unwrap();
        set_times(&path, days_ago(90));

        let scope = Scope::everything();
        let policy = policy(30.0);
        let guards = Guards::new(OpenFiles::default(), true);
        let tracker = UsageTracker::new();
        let dests = vec![cold.clone()];
        let explanation = explain(&context(
            &path, &watch, &dests, &scope, &policy, &guards, &tracker,
        ));
        let json = explanation.to_json();

        assert!(json.contains("\"scope\":{"));
        assert!(json.contains("\"guards\":{"));
        assert!(json.contains("\"policy\":{"));
        assert!(json.contains("\"verdict\":{"));
        assert!(json.contains("\"outcome\":\"would-move-now\""));
        assert!(json.contains("\"exit_code\":0"));
        assert!(
            json.contains("we\\\"ird.bin"),
            "quotes in a path must be escaped: {json}"
        );
    }

    /// The two clock conversions are the only place a hand-rolled date can be quietly
    /// wrong, so they are checked against known epochs.
    #[test]
    fn timestamps_format_as_utc() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}

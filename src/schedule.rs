//! `schedule.toml`: when the maintenance passes (`scrub`, `reconcile`) run on their own.
//!
//! `docs/design.md` §9 carried two admissions — "scrub has no schedule of its own" and
//! "reconcile is on demand" — and §10 said "deciding *when* to reconcile is not this
//! feature's job... scheduling re-scan and repair passes is P2". This module is that
//! decision, made explicit: a small config file names a cadence for each pass and the
//! budget that makes it safe, `just_cache schedule` answers what runs next, and
//! `just_cache schedule --run` runs whatever is due.
//!
//! # The mechanism: a config file plus a cron contract, not a daemon
//!
//! The tool does not grow a background process. `just_cache` is already run from cron for
//! sweeps (readme, "Running it continuously"), and a second long-lived process would need
//! supervising — a restart policy, log rotation, a pid file — for a job that is a single
//! command. So the contract is inverted: **cron runs `just_cache schedule --run`, and the
//! tool decides whether anything is due.** `every` in `schedule.toml` is how often a pass
//! *wants* to run; cron's own period is how often it *checks*. A check that finds nothing
//! due is quiet and exits `0`.
//!
//! # Why last-run state is a file, not the catalog
//!
//! "When did this pass last run" is not a fact the catalog records: the catalog is the
//! source of truth for *what exists*, and scrub's `verified_at` is per location, not per
//! run (a run that skips every already-verified location would move no timestamp at all).
//! So the schedule keeps its own small state file ([`SCHEDULE_STATE_NAME`]) beside the
//! catalog. Its name starts with the `.just_cache` prefix the sweep's walk already skips
//! (invariant 8), so it is internal bookkeeping and never a move candidate, and it is
//! created only by an explicit `schedule --run` — never by a sweep or a read-only command
//! (invariant 9).
//!
//! # A pass is only safe if it carries the pass's own budget
//!
//! `scrub --rate` exists so a scrub of a media tier does not contend with playback
//! (`scrub.rs`, "The I/O budget"); a scheduled scrub that dropped the budget would
//! silently be the unthrottled command the flag exists to replace. And a scheduled pass
//! can *write*: a scrub writes a repair, and a reconcile writes rebuilds. So a pass also
//! carries a `min_free_gb` floor and is **held back** — never started — while any root the
//! catalog records is below it. Held back, not failed: the pass stays due, the next check
//! reports the disk again, and it runs as soon as there is room. The floor is checked at
//! the schedule level rather than inside `scrub`/`reconcile`, because "is this a safe
//! moment to start a writer" is a scheduling decision, and both commands already refuse a
//! destination they cannot write.
//!
//! # With nothing configured, nothing runs
//!
//! `schedule.toml` is open-if-present, exactly like `tiers.toml` and `policy.toml`: a
//! missing default file is *no schedule*, `just_cache schedule` says so and exits `0`, and
//! no pass is ever run implicitly — today's on-demand behaviour is the default. A file that
//! *is* there but malformed is an error that names its line, because a schedule that
//! quietly stopped scheduling is the one failure a scheduler must not have.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::disk_management::available_space;
use crate::policy::parse_duration;

/// The default schedule file, beside the catalog (and so beside the watch root for the
/// default catalog path) — the same placement `tiers.toml` and `policy.toml` use.
pub const SCHEDULE_FILE_NAME: &str = "schedule.toml";

/// Where last-run times are kept, beside the catalog. The `.just_cache` prefix keeps it
/// internal to the tool: the walk skips it (invariant 8), so it is never a move candidate.
pub const SCHEDULE_STATE_NAME: &str = ".just_cache-schedule.state";

/// The two maintenance passes a schedule can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    Scrub,
    Reconcile,
}

impl Pass {
    /// The declaration order, which is also the run order: a scrub verifies (and repairs)
    /// the copies a reconcile then trusts as rebuild sources, so verifying first is the
    /// cheaper order to be wrong in.
    pub const ALL: [Pass; 2] = [Pass::Scrub, Pass::Reconcile];

    /// The name as written in `schedule.toml` and printed to the operator.
    pub fn name(self) -> &'static str {
        match self {
            Pass::Scrub => "scrub",
            Pass::Reconcile => "reconcile",
        }
    }
}

/// One pass's schedule: how often it wants to run, and the budget that makes it safe.
#[derive(Debug, Clone, PartialEq)]
pub struct PassSchedule {
    /// The minimum wall-clock gap between runs.
    pub every: Duration,
    /// Read budget in KiB/s (scrub only), or `None` for unlimited.
    pub rate_kib_per_sec: Option<u64>,
    /// Free-space floor in GiB below which the pass is held back, or `None` for no floor.
    pub min_free_gb: Option<f64>,
}

impl PassSchedule {
    /// The floor as bytes, saturating like the mover's `--min-free-gb` conversion.
    pub fn min_free_bytes(&self) -> u64 {
        match self.min_free_gb {
            Some(gb) if gb.is_finite() && gb > 0.0 => (gb * 1_073_741_824.0) as u64,
            _ => 0,
        }
    }
}

/// Why a `schedule.toml` could not be read or trusted. Mirrors [`crate::policy::PolicyError`].
#[derive(Debug, Error)]
pub enum ScheduleError {
    #[error("cannot read the schedule config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// An explicitly named file that is not there. Never a fallback: the operator asked for
    /// this schedule, and running by an empty one instead would quietly stop the passes.
    #[error("--config {path} does not exist")]
    Missing { path: PathBuf },
    #[error("malformed schedule config {path}: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// A syntactically valid file that says something impossible. Names the pass, and the
    /// line when the offending field's span is known.
    #[error("invalid schedule config {path}: {detail}")]
    Invalid { path: PathBuf, detail: String },
}

/// A parsed `schedule.toml`: the passes it configures, in declaration order.
#[derive(Debug, Clone, Default)]
pub struct ScheduleSet {
    scrub: Option<PassSchedule>,
    reconcile: Option<PassSchedule>,
    path: PathBuf,
}

impl ScheduleSet {
    /// The default file beside a directory.
    pub fn default_path(dir: &Path) -> PathBuf {
        dir.join(SCHEDULE_FILE_NAME)
    }

    /// Read and parse an explicitly named config. A missing file is an error.
    pub fn load(path: &Path) -> Result<ScheduleSet, ScheduleError> {
        let text = fs::read_to_string(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                ScheduleError::Missing {
                    path: path.to_path_buf(),
                }
            } else {
                ScheduleError::Read {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
        ScheduleSet::parse(&text, path)
    }

    /// Read the default config beside `dir`, or `None` when it is not there. Open-if-present:
    /// it never creates the file, and a file that *is* there but malformed is still an error
    /// (a broken schedule is not the same as no schedule).
    pub fn load_beside(dir: &Path) -> Result<Option<ScheduleSet>, ScheduleError> {
        let path = dir.join(SCHEDULE_FILE_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        ScheduleSet::load(&path).map(Some)
    }

    /// Parse the TOML text. Split out from [`load`] so structural errors are unit-testable
    /// without a filesystem.
    pub fn parse(text: &str, path: &Path) -> Result<ScheduleSet, ScheduleError> {
        let raw: RawFile = toml::from_str(text).map_err(|source| ScheduleError::Malformed {
            path: path.to_path_buf(),
            source,
        })?;
        let invalid = |detail: String| ScheduleError::Invalid {
            path: path.to_path_buf(),
            detail,
        };
        let scrub = match raw.scrub {
            Some(pass) => Some(parse_pass(text, Pass::Scrub, pass, &invalid)?),
            None => None,
        };
        let reconcile = match raw.reconcile {
            Some(pass) => Some(parse_pass(text, Pass::Reconcile, pass, &invalid)?),
            None => None,
        };
        Ok(ScheduleSet {
            scrub,
            reconcile,
            path: path.to_path_buf(),
        })
    }

    /// True when the file configures no pass at all.
    pub fn is_empty(&self) -> bool {
        self.scrub.is_none() && self.reconcile.is_none()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The configured pass, if the file names it.
    pub fn get(&self, pass: Pass) -> Option<&PassSchedule> {
        match pass {
            Pass::Scrub => self.scrub.as_ref(),
            Pass::Reconcile => self.reconcile.as_ref(),
        }
    }

    /// Every configured pass in run order.
    pub fn passes(&self) -> impl Iterator<Item = (Pass, &PassSchedule)> {
        Pass::ALL
            .into_iter()
            .filter_map(|pass| self.get(pass).map(|config| (pass, config)))
    }
}

/// The single decision this module exists to make: is `pass` due at `now`?
///
/// Pure in `now` on purpose — the caller injects the clock, so a test can ask about any
/// instant without waiting for a schedule window.
pub fn is_due(last_run: Option<SystemTime>, every: Duration, now: SystemTime) -> bool {
    match last_run {
        // Never run: due as soon as the operator has configured it.
        None => true,
        Some(last) => match now.duration_since(last) {
            Ok(since) => since >= every,
            // The clock moved backwards, or the state names a time still in the future.
            // Running once is the safe answer: a maintenance pass that never runs because
            // a clock was corrected is worse than one extra pass.
            Err(_) => true,
        },
    }
}

/// The instant a pass is next planned to run: its last run plus `every`, or `now` when it
/// is already due (so the operator reads "due now" rather than a past timestamp).
pub fn next_run(last_run: Option<SystemTime>, every: Duration, now: SystemTime) -> SystemTime {
    match last_run {
        Some(last) if !is_due(Some(last), every, now) => last + every,
        _ => now,
    }
}

/// The recorded roots currently below a free-space floor, in the order given.
///
/// A root whose free space is unknown (an unmounted disk, an odd filesystem) is **not**
/// below the floor: the pass proceeds, and the pass itself reports a tier it cannot write.
/// That mirrors `disk_management::destination_with_room`, which also reads "unknown" as
/// "try", rather than inventing a refusal the operator cannot act on.
pub fn roots_below_floor(roots: &[PathBuf], min_free_bytes: u64) -> Vec<PathBuf> {
    if min_free_bytes == 0 {
        return Vec::new();
    }
    roots
        .iter()
        .filter(|root| matches!(available_space(root), Some(free) if free < min_free_bytes))
        .cloned()
        .collect()
}

/// Last-run times, per pass. Tolerant to read and explicit to write.
#[derive(Debug, Default, Clone)]
pub struct ScheduleState {
    last_run: BTreeMap<Pass, SystemTime>,
}

impl ScheduleState {
    /// Read the state file, treating anything unreadable or malformed as "never run".
    ///
    /// Unlike the config (where a broken file is an error), a state file is bookkeeping:
    /// re-running a maintenance pass is safe, and refusing to schedule anything at all
    /// because a bookkeeping file has a typo would be the tool's failure, not the
    /// operator's. The cost of the tolerant read is at most one extra pass.
    pub fn load(path: &Path) -> ScheduleState {
        let Ok(text) = fs::read_to_string(path) else {
            return ScheduleState::default();
        };
        let Ok(raw) = toml::from_str::<RawState>(&text) else {
            return ScheduleState::default();
        };
        let mut last_run = BTreeMap::new();
        if let Some(pass) = raw.scrub {
            last_run.insert(Pass::Scrub, UNIX_EPOCH + Duration::from_secs(pass.last_run));
        }
        if let Some(pass) = raw.reconcile {
            last_run.insert(
                Pass::Reconcile,
                UNIX_EPOCH + Duration::from_secs(pass.last_run),
            );
        }
        ScheduleState { last_run }
    }

    pub fn last_run(&self, pass: Pass) -> Option<SystemTime> {
        self.last_run.get(&pass).copied()
    }

    pub fn set_last_run(&mut self, pass: Pass, when: SystemTime) {
        self.last_run.insert(pass, when);
    }

    /// Write the state back. A failure is reported by the caller and never fatal: the pass
    /// that just ran did its work, and the worst a lost timestamp costs is one extra pass.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let raw = RawState {
            scrub: self.last_run.get(&Pass::Scrub).map(|when| RawPassState {
                last_run: unix_seconds(*when),
            }),
            reconcile: self
                .last_run
                .get(&Pass::Reconcile)
                .map(|when| RawPassState {
                    last_run: unix_seconds(*when),
                }),
        };
        let text = toml::to_string(&raw).map_err(std::io::Error::other)?;
        fs::write(path, text)
    }
}

/// Seconds since the Unix epoch, saturating before 1970.
pub fn unix_seconds(stamp: SystemTime) -> u64 {
    stamp
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// A compact duration for the operator, e.g. `7d`, `1h 30m`, `45s`.
pub fn human_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs == 0 {
        return "0s".to_string();
    }
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (hours, minutes, seconds) = (rest / 3_600, (rest % 3_600) / 60, rest % 60);
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    // Seconds are noise once a larger unit is present ("7d 3s" reads wrong), but are the
    // whole answer for a cadence under a minute.
    if seconds > 0 && days == 0 && hours == 0 {
        parts.push(format!("{seconds}s"));
    }
    parts.join(" ")
}

/// RFC 3339 in UTC, without a date crate — the same arithmetic `explain` prints, kept here
/// so the schedule module reads its own timestamps.
pub fn format_time(stamp: SystemTime) -> String {
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

/// Parse and validate one pass table: cadence required, budget optional and bounded.
fn parse_pass(
    text: &str,
    pass: Pass,
    raw: RawPass,
    invalid: &impl Fn(String) -> ScheduleError,
) -> Result<PassSchedule, ScheduleError> {
    // The same duration parser `policy.toml` uses, so `every` accepts what `after_idle`
    // accepts and a bad unit names its line the same way.
    let every = parse_duration(raw.every.get_ref()).map_err(|reason| {
        invalid(format!(
            "line {}: `{}` every {:?} is not a duration ({reason}); use a number with a unit, \
             e.g. \"7d\" or \"12h\"",
            line_at(text, raw.every.span().start),
            pass.name(),
            raw.every.get_ref()
        ))
    })?;
    if every.is_zero() {
        return Err(invalid(format!(
            "line {}: `{}` every must be at least 1s; a pass that should run on every cron \
             tick wants cron's own cadence, not a zero interval",
            line_at(text, raw.every.span().start),
            pass.name()
        )));
    }

    let rate_kib_per_sec = match raw.rate {
        Some(rate) => {
            if pass != Pass::Scrub {
                return Err(invalid(format!(
                    "line {}: `{}` sets `rate`, but only the scrub pass has a read budget \
                     (`scrub --rate`); a reconcile read is unthrottled",
                    line_at(text, rate.span().start),
                    pass.name()
                )));
            }
            let value = *rate.get_ref();
            if value == 0 {
                return Err(invalid(format!(
                    "line {}: `{}` rate must be at least 1 KiB/s",
                    line_at(text, rate.span().start),
                    pass.name()
                )));
            }
            Some(value)
        }
        None => None,
    };

    let min_free_gb = match raw.min_free_gb {
        Some(gb) => {
            let value = *gb.get_ref();
            if !value.is_finite() || value < 0.0 {
                return Err(invalid(format!(
                    "line {}: `{}` min_free_gb must be a finite, non-negative number of GiB",
                    line_at(text, gb.span().start),
                    pass.name()
                )));
            }
            Some(value)
        }
        None => None,
    };

    Ok(PassSchedule {
        every,
        rate_kib_per_sec,
        min_free_gb,
    })
}

/// The raw serde shape. Scalars are `Spanned` so a bad value can name its line.
///
/// Deliberately two named tables rather than a map: a map would let a typo (`[scub]`) parse
/// as a pass nobody scheduled. `deny_unknown_fields` turns that typo into an error instead
/// of a schedule that silently never runs. No `#[serde(untagged)]` anywhere — serde's
/// `Content` buffering loses toml's spans, and every parse then fails (the repo pitfall).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    scrub: Option<RawPass>,
    #[serde(default)]
    reconcile: Option<RawPass>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPass {
    every: toml::Spanned<String>,
    #[serde(default)]
    rate: Option<toml::Spanned<u64>>,
    #[serde(default)]
    min_free_gb: Option<toml::Spanned<f64>>,
}

/// The state file's shape. Plain scalars, no spans: nothing reads it for line numbers.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RawState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scrub: Option<RawPassState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reconcile: Option<RawPassState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawPassState {
    last_run: u64,
}

/// The 1-based line number holding a byte offset, exactly as `policy.rs` and `tiers.rs` do it.
fn line_at(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    const SAMPLE: &str = r#"
[scrub]
every = "7d"
rate = 4096
min_free_gb = 1.0

[reconcile]
every = "1h"
min_free_gb = 0.5
"#;

    #[test]
    fn a_schedule_parses_every_field_for_each_pass() {
        let dir = tmp();
        let set = ScheduleSet::parse(SAMPLE, &dir.path().join("schedule.toml")).unwrap();
        assert!(!set.is_empty());
        let scrub = set.get(Pass::Scrub).expect("scrub");
        assert_eq!(scrub.every, Duration::from_secs(7 * 86_400));
        assert_eq!(scrub.rate_kib_per_sec, Some(4096));
        assert_eq!(scrub.min_free_gb, Some(1.0));
        assert_eq!(scrub.min_free_bytes(), 1_073_741_824);

        let reconcile = set.get(Pass::Reconcile).expect("reconcile");
        assert_eq!(reconcile.every, Duration::from_secs(3_600));
        assert_eq!(reconcile.rate_kib_per_sec, None);
        // Reconcile has no read budget, so it was not refused for the missing `rate`; the
        // floor is a whole GiB fraction, which the byte conversion floors.
        assert_eq!(reconcile.min_free_bytes(), 536_870_912);
    }

    #[test]
    fn passes_come_back_in_run_order() {
        let dir = tmp();
        let set = ScheduleSet::parse(SAMPLE, &dir.path().join("schedule.toml")).unwrap();
        let names: Vec<&str> = set.passes().map(|(pass, _)| pass.name()).collect();
        assert_eq!(names, vec!["scrub", "reconcile"]);
    }

    #[test]
    fn only_the_named_passes_are_configured() {
        let dir = tmp();
        let set = ScheduleSet::parse(
            "[scrub]\nevery = \"1h\"\n",
            &dir.path().join("schedule.toml"),
        )
        .unwrap();
        assert!(set.get(Pass::Scrub).is_some());
        assert!(set.get(Pass::Reconcile).is_none());
    }

    #[test]
    fn a_bad_cadence_names_the_line_and_the_pass() {
        let dir = tmp();
        let text = "[scrub]\nevery = \"soon\"\n";
        let err = ScheduleSet::parse(text, &dir.path().join("schedule.toml")).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("line 2"), "{rendered}");
        assert!(rendered.contains("scrub"), "{rendered}");
        assert!(rendered.contains("every"), "{rendered}");
    }

    #[test]
    fn a_zero_cadence_is_refused() {
        let dir = tmp();
        let err =
            ScheduleSet::parse("[scrub]\nevery = \"0\"\n", &dir.path().join("s.toml")).unwrap_err();
        assert!(err.to_string().contains("at least 1s"), "{err}");
    }

    #[test]
    fn a_rate_on_reconcile_is_refused_not_ignored() {
        let dir = tmp();
        let err = ScheduleSet::parse(
            "[reconcile]\nevery = \"1h\"\nrate = 512\n",
            &dir.path().join("s.toml"),
        )
        .unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("reconcile"), "{rendered}");
        assert!(rendered.contains("only the scrub pass"), "{rendered}");
    }

    #[test]
    fn a_zero_or_negative_rate_or_floor_is_refused() {
        let dir = tmp();
        let path = dir.path().join("s.toml");
        let zero = ScheduleSet::parse("[scrub]\nevery = \"1h\"\nrate = 0\n", &path).unwrap_err();
        assert!(zero.to_string().contains("at least 1 KiB/s"), "{zero}");

        let negative =
            ScheduleSet::parse("[scrub]\nevery = \"1h\"\nmin_free_gb = -1.0\n", &path).unwrap_err();
        assert!(
            negative.to_string().contains("finite, non-negative"),
            "{negative}"
        );

        let nan =
            ScheduleSet::parse("[scrub]\nevery = \"1h\"\nmin_free_gb = nan\n", &path).unwrap_err();
        assert!(nan.to_string().contains("finite, non-negative"), "{nan}");
    }

    #[test]
    fn an_unknown_key_is_an_error_not_a_silent_no_op() {
        let dir = tmp();
        // A typo in a table name would otherwise parse as a pass nobody scheduled.
        let err =
            ScheduleSet::parse("[scub]\nevery = \"1h\"\n", &dir.path().join("s.toml")).unwrap_err();
        assert!(matches!(err, ScheduleError::Malformed { .. }), "{err}");

        let err = ScheduleSet::parse(
            "[scrub]\nevery = \"1h\"\nmin_free = 1.0\n",
            &dir.path().join("s.toml"),
        )
        .unwrap_err();
        assert!(matches!(err, ScheduleError::Malformed { .. }), "{err}");
    }

    #[test]
    fn a_default_file_that_is_absent_is_no_schedule() {
        let dir = tmp();
        assert!(ScheduleSet::load_beside(dir.path()).unwrap().is_none());
        assert!(!ScheduleSet::default_path(dir.path()).exists());
    }

    #[test]
    fn an_explicit_missing_file_is_an_error_not_an_empty_set() {
        let dir = tmp();
        let err = ScheduleSet::load(&dir.path().join("nope.toml")).unwrap_err();
        assert!(matches!(err, ScheduleError::Missing { .. }), "{err}");
    }

    #[test]
    fn due_and_next_run_are_pure_decisions_in_now() {
        let every = Duration::from_secs(3_600);
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);

        // Never run: due, and the next run is now (the operator sees "due now").
        assert!(is_due(None, every, now));
        assert_eq!(next_run(None, every, now), now);

        // Last run just under the cadence ago: not due, and the next run is exact.
        let last = now - Duration::from_secs(3_599);
        assert!(!is_due(Some(last), every, now));
        assert_eq!(next_run(Some(last), every, now), last + every);

        // Exactly on the boundary is due.
        let last = now - every;
        assert!(is_due(Some(last), every, now));
        assert_eq!(next_run(Some(last), every, now), now);

        // Long past: due, next run now.
        let last = now - Duration::from_secs(10 * 86_400);
        assert!(is_due(Some(last), every, now));
        assert_eq!(next_run(Some(last), every, now), now);
    }

    #[test]
    fn a_run_recorded_in_the_future_is_due_rather_than_starved() {
        let every = Duration::from_secs(3_600);
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let last = now + Duration::from_secs(600);
        assert!(is_due(Some(last), every, now));
    }

    #[test]
    fn a_floor_of_zero_never_holds_a_pass_back_and_a_huge_one_always_does() {
        let dir = tmp();
        let roots = vec![dir.path().to_path_buf()];
        assert!(roots_below_floor(&roots, 0).is_empty());
        // No real filesystem has u64::MAX bytes free, so this is deterministic.
        assert_eq!(roots_below_floor(&roots, u64::MAX), roots);
    }

    #[test]
    fn roots_that_do_not_exist_are_never_below_the_floor() {
        let dir = tmp();
        let missing = dir.path().join("not-mounted");
        assert!(roots_below_floor(&[missing], u64::MAX).is_empty());
    }

    #[test]
    fn state_round_trips_through_a_file() {
        let dir = tmp();
        let path = dir.path().join(SCHEDULE_STATE_NAME);
        let when = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut state = ScheduleState::default();
        assert_eq!(state.last_run(Pass::Scrub), None);
        state.set_last_run(Pass::Scrub, when);
        state.save(&path).unwrap();

        let reloaded = ScheduleState::load(&path);
        assert_eq!(reloaded.last_run(Pass::Scrub), Some(when));
        assert_eq!(reloaded.last_run(Pass::Reconcile), None);
    }

    #[test]
    fn a_corrupt_state_file_reads_as_never_run() {
        let dir = tmp();
        let path = dir.path().join(SCHEDULE_STATE_NAME);
        fs::write(&path, "this is not toml = = =").unwrap();
        let state = ScheduleState::load(&path);
        assert_eq!(state.last_run(Pass::Scrub), None);
    }

    #[test]
    fn durations_read_as_humans_write_them() {
        assert_eq!(human_duration(Duration::from_secs(7 * 86_400)), "7d");
        assert_eq!(human_duration(Duration::from_secs(5_400)), "1h 30m");
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn timestamps_format_as_utc() {
        assert_eq!(format_time(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_time(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
    }
}

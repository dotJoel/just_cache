//! `policy.toml`: the lifecycle rules that decide which tier a file belongs on.
//!
//! `docs/design.md` §5: rules are evaluated per file, and they are **explainable** — every
//! transition records which rule decided it, and `just_cache explain <path>` answers
//! "where is this file and why", naming either the rule that fired or the exclusion that
//! stopped it (§5.1). A rule has a `match` glob, a `down` transition (`after_idle`, `from`,
//! `to`), an optional `up` promotion (`on_access`), and `pins` — patterns that always win
//! over the rule's own idle gate.
//!
//! ## Rules govern tiers only
//!
//! §2.1 draws the line the mover must not cross: a cache overlay is a promotion target,
//! never the place a file lives. A rule that names one as a `from`, a `to`, or a `down`
//! target is refused when the policy is loaded, naming the rule — a config that could make
//! a volatile copy a home is not a config this engine will run.
//!
//! ## A rule that cannot be satisfied is said so
//!
//! An unconfigured tier, an impossible `from`/`to` pair (the same tier, or a `down` that
//! moves to a *faster* one — that is an `up`), and an unparseable duration are all usage
//! errors that name the rule. The alternative — silently skipping the rule — turns a typo
//! into a tree that quietly stops tiering, which is exactly the failure a lifecycle engine
//! must not have.
//!
//! ## The fallback is what the tool already did
//!
//! With no `policy.toml` there are no rules and the sweep's decision is the flag-driven one
//! (`--min-idle-days`), exactly as before. The file is read from `--policy <FILE>`, or from
//! `policy.toml` beside the watch root **only when it is already there** (invariant 9: a
//! command never creates anything in the watched tree it was not asked to).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use thiserror::Error;

use crate::tiers::{Tier, TierSet};

/// The default file name, beside the watch root.
pub const POLICY_FILE_NAME: &str = "policy.toml";

/// One `down` transition: move a file on tier `from` to tier `to` once it has been idle
/// for `after_idle`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownRule {
    pub after_idle: Duration,
    pub from: String,
    pub to: String,
}

/// The promotion half. `on_access` is the only knob today: a read of a file on the `down`
/// rule's `to` tier promotes it back to `from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpRule {
    pub on_access: bool,
}

/// One `[[rule]]` table, with its globs compiled.
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    /// The `match` glob as written, kept for messages.
    pub match_pattern: String,
    /// The `down` transitions, in declaration order. A rule may declare several — the
    /// design's "move down when idle" chain (`ssd → hdd_parked → offsite`), one entry per
    /// `from` tier — so the file's current tier selects which one applies.
    pub downs: Vec<DownRule>,
    pub up: Option<UpRule>,
    /// Pins as written, kept for messages.
    pub pins: Vec<String>,
    matcher: GlobMatcher,
    /// One compiled matcher per pin, plus the pattern it came from (expanded variants
    /// share their source, so a message names what the operator typed).
    pin_matchers: Vec<(String, GlobMatcher)>,
}

impl Rule {
    /// True when this rule's `match` covers the watch-relative path. Ancestor directories
    /// count, so `media/**` covers the whole subtree and a bare `video` covers its
    /// contents — the same reading `--include` uses.
    pub fn matches(&self, relative: &Path) -> bool {
        relative
            .ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .any(|ancestor| self.matcher.is_match(ancestor))
    }

    /// The `down` transition this rule declares for a file currently on `tier`, if any.
    pub fn down_from(&self, tier: &str) -> Option<&DownRule> {
        self.downs.iter().find(|down| down.from == tier)
    }

    /// True when the rule declares any `down` at all.
    pub fn has_down(&self) -> bool {
        !self.downs.is_empty()
    }

    /// The first pin pattern that matches the path, if any. A pin matches the path, any
    /// ancestor, or the file's own name, so a bare `*.drp` protects project files at any
    /// depth — the design's intent (§5).
    fn pinned_by(&self, relative: &Path) -> Option<&str> {
        let name = relative.file_name();
        for (source, matcher) in &self.pin_matchers {
            if matcher.is_match(relative)
                || name.is_some_and(|name| matcher.is_match(Path::new(name)))
                || relative
                    .ancestors()
                    .filter(|ancestor| !ancestor.as_os_str().is_empty())
                    .any(|ancestor| matcher.is_match(ancestor))
            {
                return Some(source);
            }
        }
        None
    }
}

/// Why a `policy.toml` could not be read or trusted.
#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("cannot read the policy config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// An explicitly named file that is not there. Never a fallback: the operator asked
    /// for this file, and sweeping by flags instead would run a different policy.
    #[error("--policy {path} does not exist")]
    Missing { path: PathBuf },
    #[error("malformed policy config {path}: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// A syntactically valid file that says something impossible. Names the rule, and the
    /// line when the offending field's span is known.
    #[error("invalid policy config {path}: {detail}")]
    Invalid { path: PathBuf, detail: String },
}

/// A parsed `policy.toml`: every rule, in declaration order.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    path: PathBuf,
}

impl RuleSet {
    /// The default file beside a watch root.
    pub fn default_path(watch: &Path) -> PathBuf {
        watch.join(POLICY_FILE_NAME)
    }

    /// Read and parse an explicitly named config. A missing file is an error.
    pub fn load(path: &Path) -> Result<RuleSet, PolicyError> {
        let text = fs::read_to_string(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                PolicyError::Missing {
                    path: path.to_path_buf(),
                }
            } else {
                PolicyError::Read {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
        RuleSet::parse(&text, path)
    }

    /// Read the default config beside `dir`, or `None` when it is not there. Open-if-present:
    /// it never creates the file, and a file that *is* there but malformed is still an error
    /// (a broken policy is not the same as no policy).
    pub fn load_beside(dir: &Path) -> Result<Option<RuleSet>, PolicyError> {
        let path = dir.join(POLICY_FILE_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        RuleSet::load(&path).map(Some)
    }

    /// Parse the TOML text. Split out from [`load`] so structural errors are unit-testable
    /// without a filesystem. Tier references are checked separately by [`validate_tiers`],
    /// which needs the tier config.
    pub fn parse(text: &str, path: &Path) -> Result<RuleSet, PolicyError> {
        let raw: RawFile = toml::from_str(text).map_err(|source| PolicyError::Malformed {
            path: path.to_path_buf(),
            source,
        })?;

        let invalid = |detail: String| PolicyError::Invalid {
            path: path.to_path_buf(),
            detail,
        };
        let mut rules = Vec::with_capacity(raw.rule.len());
        let mut names = HashSet::new();
        for raw_rule in raw.rule {
            let name = raw_rule.name.get_ref().trim().to_string();
            if name.is_empty() {
                return Err(invalid(format!(
                    "line {}: a rule must have a non-empty `name`",
                    line_at(text, raw_rule.name.span().start)
                )));
            }
            if !names.insert(name.clone()) {
                return Err(invalid(format!(
                    "line {}: rule `{name}` is declared more than once; a rule name is how \
                     explain names what decided, so it must be unique",
                    line_at(text, raw_rule.name.span().start)
                )));
            }

            let match_pattern = raw_rule.match_pattern.get_ref().trim().to_string();
            if match_pattern.is_empty() {
                return Err(invalid(format!(
                    "line {}: rule `{name}` has an empty `match`; use `**` to match everything",
                    line_at(text, raw_rule.match_pattern.span().start)
                )));
            }
            let matcher = Glob::new(&match_pattern)
                .map_err(|source| {
                    invalid(format!(
                        "line {}: rule `{name}` has a bad `match` glob {match_pattern:?}: {source}",
                        line_at(text, raw_rule.match_pattern.span().start)
                    ))
                })?
                .compile_matcher();

            let downs = match raw_rule.down {
                Some(one) => parse_downs(text, &name, vec![one], &invalid)?,
                None => Vec::new(),
            };

            if let Some(up) = &raw_rule.up {
                if up.on_access && downs.is_empty() {
                    return Err(invalid(format!(
                        "rule `{name}` sets `up.on_access` but has no `down` to promote back \
                         to; an up transition is the inverse of a down"
                    )));
                }
            }

            let mut pin_matchers = Vec::new();
            for pin in &raw_rule.pins {
                let pattern = pin.get_ref().trim().to_string();
                if pattern.is_empty() {
                    return Err(invalid(format!(
                        "line {}: rule `{name}` has an empty pin pattern",
                        line_at(text, pin.span().start)
                    )));
                }
                for variant in pin_variants(&pattern) {
                    let glob = Glob::new(&variant).map_err(|source| {
                        invalid(format!(
                            "line {}: rule `{name}` has a bad pin glob {pattern:?}: {source}",
                            line_at(text, pin.span().start)
                        ))
                    })?;
                    pin_matchers.push((pattern.clone(), glob.compile_matcher()));
                }
            }

            rules.push(Rule {
                name,
                match_pattern,
                downs,
                up: raw_rule.up.map(|up| UpRule {
                    on_access: up.on_access,
                }),
                pins: raw_rule
                    .pins
                    .iter()
                    .map(|pin| pin.get_ref().trim().to_string())
                    .collect(),
                matcher,
                pin_matchers,
            });
        }

        Ok(RuleSet {
            rules,
            path: path.to_path_buf(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, name: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.name == name)
    }

    /// Check every tier a rule names against the tier config, and refuse a rule that could
    /// not be satisfied. This is the §2.1 / §5 line: rules govern tiers, a cache overlay is
    /// never one of them, and a `down` must actually go *down*.
    pub fn validate_tiers(&self, tiers: &TierSet) -> Result<(), PolicyError> {
        let invalid = |detail: String| PolicyError::Invalid {
            path: self.path.clone(),
            detail,
        };
        for rule in &self.rules {
            for down in &rule.downs {
                let from = tiers.get(&down.from).ok_or_else(|| {
                    invalid(format!(
                        "rule `{}`: `down.from` names tier `{}`, which is not configured in \
                         tiers.toml",
                        rule.name, down.from
                    ))
                })?;
                let to = tiers.get(&down.to).ok_or_else(|| {
                    invalid(format!(
                        "rule `{}`: `down.to` names tier `{}`, which is not configured in \
                         tiers.toml",
                        rule.name, down.to
                    ))
                })?;
                refuse_cache_overlay(&rule.name, "down.from", from, &invalid)?;
                refuse_cache_overlay(&rule.name, "down.to", to, &invalid)?;
                if from.name == to.name {
                    return Err(invalid(format!(
                        "rule `{}`: `down.from` and `down.to` are both tier `{}`; a transition \
                         must change tiers",
                        rule.name, from.name
                    )));
                }
                if to.recall < from.recall {
                    return Err(invalid(format!(
                        "rule `{}`: `down` moves from `{}` (recall {}) to `{}` (recall {}), which \
                         is *faster* — a down transition must not move data up; use an `up` rule",
                        rule.name,
                        from.name,
                        from.recall.as_str(),
                        to.name,
                        to.recall.as_str()
                    )));
                }
            }
        }
        // `up` names no tier directly: it promotes to the `down` rule's `from`, which the
        // loop above already checked. A rule with `up` and no `down` was refused at parse.
        Ok(())
    }
}

/// §2.1: anything volatile is a mirror, never a home, so it can be neither end of a rule.
fn refuse_cache_overlay(
    rule: &str,
    field: &str,
    tier: &Tier,
    invalid: &impl Fn(String) -> PolicyError,
) -> Result<(), PolicyError> {
    if tier.is_home() {
        return Ok(());
    }
    Err(invalid(format!(
        "rule `{rule}`: `{field}` names volatile tier `{}` ({}); a cache overlay is a \
         promotion target, never a lifecycle tier (docs/design.md §2.1)",
        tier.name,
        tier.path.display()
    )))
}

/// What the rules decide for one file, positioned on one tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownDecision {
    /// No rule's `match` covers the path. The file is outside every policy.
    NoRule,
    /// A rule matched and one of its pins protects the file. Pin wins over policy.
    Pinned { rule: String, pattern: String },
    /// The path is not on any configured tier, so no `from` can match.
    NotOnManagedTier,
    /// The matching rule has no `down` from the tier the file is on.
    NoDownFrom { rule: String, from: String },
    /// The rule applies, but the file is not idle enough yet.
    TooWarm {
        rule: String,
        from: String,
        to: String,
        idle: Duration,
        after_idle: Duration,
    },
    /// The rule fired: move the file from `from` to `to`.
    Down {
        rule: String,
        from: String,
        to: String,
        after_idle: Duration,
    },
}

impl DownDecision {
    /// The rule that decided, for `lifecycle.rule`. `None` for "no rule matched".
    pub fn rule(&self) -> Option<&str> {
        match self {
            DownDecision::NoRule => None,
            DownDecision::Pinned { rule, .. }
            | DownDecision::NoDownFrom { rule, .. }
            | DownDecision::TooWarm { rule, .. }
            | DownDecision::Down { rule, .. } => Some(rule),
            DownDecision::NotOnManagedTier => None,
        }
    }

    /// One sentence a human can act on — the rule named, or the exclusion.
    pub fn describe(&self) -> String {
        match self {
            DownDecision::NoRule => "no policy rule matches this path".to_string(),
            DownDecision::Pinned { rule, pattern } => {
                format!("pinned by rule `{rule}` (pattern `{pattern}`)")
            }
            DownDecision::NotOnManagedTier => {
                "this path is not on any configured tier, so no rule `from` can match".to_string()
            }
            DownDecision::NoDownFrom { rule, from } => {
                format!("rule `{rule}` has no down transition from tier `{from}`")
            }
            DownDecision::TooWarm {
                rule,
                from,
                to,
                idle,
                after_idle,
            } => format!(
                "rule `{rule}`: idle {:.1} d of the {:.1} d after_idle it requires before \
                 {} -> {}",
                idle.as_secs_f64() / 86_400.0,
                after_idle.as_secs_f64() / 86_400.0,
                from,
                to
            ),
            DownDecision::Down {
                rule,
                from,
                to,
                after_idle,
            } => format!(
                "rule `{rule}`: idle past after_idle {:.1} d, {} -> {}",
                after_idle.as_secs_f64() / 86_400.0,
                from,
                to
            ),
        }
    }
}

/// The lifecycle engine: the parsed rules plus the tiers they govern, both borrowed.
///
/// This is the single place a rule is evaluated, so the sweep and `explain` can never
/// disagree about which rule fired — the same reason [`crate::scope`] has one gate.
pub struct Lifecycle<'a> {
    rules: &'a RuleSet,
    tiers: &'a TierSet,
}

impl<'a> Lifecycle<'a> {
    pub fn new(rules: &'a RuleSet, tiers: &'a TierSet) -> Self {
        Self { rules, tiers }
    }

    pub fn rules(&self) -> &'a RuleSet {
        self.rules
    }

    pub fn tiers(&self) -> &'a TierSet {
        self.tiers
    }

    /// True when `path` is one of the config files this engine was built from.
    ///
    /// `policy.toml` and `tiers.toml` default to sitting *beside the watch root* — that is,
    /// inside the watched tree — so a sweep would otherwise treat its own rules as an idle
    /// file and move them on a later pass, silently changing the policy that governs it.
    /// A config is something the sweep reads, never something it moves.
    pub fn is_config_file(&self, path: &Path) -> bool {
        [self.rules.path(), self.tiers.path()]
            .iter()
            .any(|configured| same_file(configured, path))
    }

    /// The configured tier a path lives on, when one contains it.
    pub fn tier_of(&self, path: &Path) -> Option<&'a Tier> {
        self.tiers.tier_containing(path)
    }

    /// The name of the tier a path lives on, when one contains it.
    pub fn current_tier(&self, path: &Path) -> Option<&'a str> {
        self.tier_of(path).map(|tier| tier.name.as_str())
    }

    /// Rules whose `match` covers the path, in declaration order. Later rules override
    /// earlier ones — that is what makes a per-path rule an override of a catch-all `**`.
    pub fn matching_rules(&self, relative: &Path) -> Vec<&'a Rule> {
        self.rules
            .rules()
            .iter()
            .filter(|rule| rule.matches(relative))
            .collect()
    }

    /// The rule a pin protects a path with, if any. A pin wins over policy, so this is
    /// checked before any idle decision.
    pub fn pinned_by(&self, relative: &Path) -> Option<(&'a str, &'a str)> {
        for rule in self.matching_rules(relative).into_iter().rev() {
            if let Some(pattern) = rule.pinned_by(relative) {
                return Some((rule.name.as_str(), pattern));
            }
        }
        None
    }

    /// The down decision for a path on `current_tier`, given how long it has been idle.
    ///
    /// Precedence is the design's, not a convenience: pins win over policy, a rule that
    /// does not name the file's tier cannot move it, and only then does the idle gate
    /// apply. The *last* matching rule that names the file's tier wins, so a per-path rule
    /// declared after a catch-all overrides it.
    pub fn evaluate_down(
        &self,
        relative: &Path,
        current_tier: Option<&str>,
        idle: Duration,
    ) -> DownDecision {
        let matching = self.matching_rules(relative);
        if matching.is_empty() {
            return DownDecision::NoRule;
        }
        if let Some((rule, pattern)) = self.pinned_by(relative) {
            return DownDecision::Pinned {
                rule: rule.to_string(),
                pattern: pattern.to_string(),
            };
        }
        let Some(current) = current_tier else {
            return DownDecision::NotOnManagedTier;
        };
        let chosen = matching
            .iter()
            .rev()
            .find(|rule| rule.down_from(current).is_some());
        let Some(rule) = chosen else {
            // Name the last matching rule: that is the one the operator was looking at.
            let last = matching.last().expect("non-empty");
            return DownDecision::NoDownFrom {
                rule: last.name.clone(),
                from: current.to_string(),
            };
        };
        let down = rule.down_from(current).expect("chosen for its down");
        if idle >= down.after_idle {
            DownDecision::Down {
                rule: rule.name.clone(),
                from: down.from.clone(),
                to: down.to.clone(),
                after_idle: down.after_idle,
            }
        } else {
            DownDecision::TooWarm {
                rule: rule.name.clone(),
                from: down.from.clone(),
                to: down.to.clone(),
                idle,
                after_idle: down.after_idle,
            }
        }
    }

    /// The up rule that would promote a path, when it sits on the `to` tier of a rule with
    /// `up.on_access`.
    ///
    /// The symlink mover cannot perform a promotion — recall is the namespace provider's
    /// job (§8, P2) — so this is reported, not acted on, and named as such in §9/§10.
    pub fn evaluate_up(&self, relative: &Path, current_tier: Option<&str>) -> Option<&'a Rule> {
        let current = current_tier?;
        self.matching_rules(relative)
            .into_iter()
            .rev()
            .find(|rule| {
                rule.up.as_ref().is_some_and(|up| up.on_access)
                    && rule.downs.iter().any(|down| down.to == current)
            })
    }
}

/// The raw serde shape. Scalars are `Spanned` so a bad value can name its line.
#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    name: toml::Spanned<String>,
    #[serde(rename = "match")]
    match_pattern: toml::Spanned<String>,
    #[serde(default)]
    down: Option<RawDown>,
    #[serde(default)]
    up: Option<RawUp>,
    #[serde(default)]
    pins: Vec<toml::Spanned<String>>,
}

#[derive(Debug, Deserialize)]
struct RawDown {
    after_idle: toml::Spanned<String>,
    from: toml::Spanned<String>,
    to: toml::Spanned<String>,
}

/// Parse and structurally validate a rule's `down` entries. Split out so one or many take
/// the identical path, and so a bad entry names its line.
fn parse_downs(
    text: &str,
    rule: &str,
    list: Vec<RawDown>,
    invalid: &impl Fn(String) -> PolicyError,
) -> Result<Vec<DownRule>, PolicyError> {
    let mut downs = Vec::with_capacity(list.len());
    for raw_down in list {
        let after_idle = parse_duration(raw_down.after_idle.get_ref()).map_err(|reason| {
            invalid(format!(
                "line {}: rule `{rule}` down.after_idle {:?} is not a duration \
                 ({reason}); use a number with a unit, e.g. \"30d\" or \"12h\"",
                line_at(text, raw_down.after_idle.span().start),
                raw_down.after_idle.get_ref()
            ))
        })?;
        let from = raw_down.from.get_ref().trim().to_string();
        let to = raw_down.to.get_ref().trim().to_string();
        if from.is_empty() || to.is_empty() {
            return Err(invalid(format!(
                "line {}: rule `{rule}` down must name both `from` and `to` tiers",
                line_at(text, raw_down.from.span().start)
            )));
        }
        downs.push(DownRule {
            after_idle,
            from,
            to,
        });
    }
    Ok(downs)
}

#[derive(Debug, Deserialize)]
struct RawUp {
    #[serde(default = "default_true")]
    on_access: bool,
}

fn default_true() -> bool {
    true
}

/// Compile a pin as written, plus any-depth variants when it reads as a bare name. A pin
/// like `*.drp` is meant to protect project files anywhere, which is why the file's own
/// name is matched too ([`Rule::pinned_by`]).
fn pin_variants(pattern: &str) -> Vec<String> {
    let mut variants = vec![pattern.to_string()];
    if !pattern.contains('/') {
        variants.push(format!("**/{pattern}"));
        variants.push(format!("**/{pattern}/**"));
    }
    variants
}

/// Parse a duration like `30d`, `12h`, `90m`, `45s`, `2w`.
///
/// A unit is required in spirit; a bare number is read as seconds only because a config
/// that writes `"0"` for "immediately" should mean what it says. Anything else — a missing
/// unit, an unparseable number, a negative or non-finite value — is an error, because a
/// rule that silently never fires is worse than one that refuses to load.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("empty".to_string());
    }
    let split_at = trimmed
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.' && ch != '_')
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split_at);
    let number = number.replace('_', "");
    if number.is_empty() {
        return Err("no number".to_string());
    }
    let value: f64 = number.parse().map_err(|_| "not a number".to_string())?;
    if !value.is_finite() || value < 0.0 {
        return Err("not a finite, non-negative number".to_string());
    }
    let seconds = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "s" | "sec" | "secs" => 1.0,
        "m" | "min" | "mins" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "week" | "weeks" => 604_800.0,
        other => return Err(format!("unknown unit {other:?}")),
    };
    let total = value * seconds;
    if total > Duration::MAX.as_secs_f64() {
        return Err("too large for a duration".to_string());
    }
    Ok(Duration::from_secs_f64(total))
}

/// The 1-based line number holding a byte offset, exactly as `tiers.rs` does it.
fn line_at(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

/// Lexical equality, with a canonical fallback so a symlinked config path, a `..` or a
/// trailing difference still matches the file the engine actually loaded.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn tiers_text(ssd: &Path, hdd: &Path) -> String {
        format!(
            "[tiers.ssd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n\n\
             [tiers.hdd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"s\"\ncopies = 1\n",
            ssd.display(),
            hdd.display()
        )
    }

    fn sample() -> &'static str {
        r#"
[[rule]]
name = "intelligent-tiering"
match = "**"
down = { after_idle = "30d", from = "ssd", to = "hdd" }
up = { on_access = true }
pins = ["*.drp"]
"#
    }

    #[test]
    fn a_rule_parses_with_every_field() {
        let dir = tmp();
        let set = RuleSet::parse(sample(), &dir.path().join("policy.toml")).unwrap();
        assert_eq!(set.rules().len(), 1);
        let rule = set.get("intelligent-tiering").expect("rule");
        assert_eq!(rule.match_pattern, "**");
        assert_eq!(rule.downs.len(), 1);
        let down = &rule.downs[0];
        assert_eq!(down.from, "ssd");
        assert_eq!(down.to, "hdd");
        assert_eq!(down.after_idle, Duration::from_secs(30 * 86_400));
        assert!(rule.up.as_ref().unwrap().on_access);
        assert_eq!(rule.pins, vec!["*.drp".to_string()]);
        assert!(rule.matches(Path::new("a/b/c.bin")));
    }

    #[test]
    fn a_bad_duration_names_the_line_and_the_rule() {
        let dir = tmp();
        let text = "[[rule]]\nname = \"bad\"\nmatch = \"**\"\ndown = { after_idle = \"soon\", from = \"ssd\", to = \"hdd\" }\n";
        let err = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("line 4"), "{rendered}");
        assert!(rendered.contains("bad"), "{rendered}");
        assert!(rendered.contains("after_idle"), "{rendered}");
    }

    #[test]
    fn an_unconfigured_tier_is_refused_by_rule_name() {
        let dir = tmp();
        let set = RuleSet::parse(sample(), &dir.path().join("policy.toml")).unwrap();
        let ssd = dir.path().join("ssd");
        let hdd = dir.path().join("hdd");
        fs::create_dir_all(&ssd).unwrap();
        fs::create_dir_all(&hdd).unwrap();
        let tiers =
            TierSet::parse(&tiers_text(&ssd, &hdd), &dir.path().join("tiers.toml")).unwrap();
        assert!(set.validate_tiers(&tiers).is_ok());

        // `to` names a tier that is not configured.
        let text = "[[rule]]\nname = \"gone\"\nmatch = \"**\"\ndown = { after_idle = \"30d\", from = \"ssd\", to = \"archive\" }\n";
        let bad = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap();
        let err = bad.validate_tiers(&tiers).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("gone"), "{rendered}");
        assert!(rendered.contains("archive"), "{rendered}");
    }

    #[test]
    fn a_volatile_cache_overlay_is_never_a_lifecycle_tier() {
        let dir = tmp();
        let ssd = dir.path().join("ssd");
        let ram = dir.path().join("ram");
        fs::create_dir_all(&ssd).unwrap();
        fs::create_dir_all(&ram).unwrap();
        let tiers = TierSet::parse(
            &format!(
                "[tiers.ssd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n\n\
                 [tiers.ram]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"volatile\"\nrecall = \"us\"\ncopies = 1\n",
                ssd.display(),
                ram.display()
            ),
            &dir.path().join("tiers.toml"),
        )
        .unwrap();
        let text = "[[rule]]\nname = \"ram-run\"\nmatch = \"**\"\ndown = { after_idle = \"1d\", from = \"ssd\", to = \"ram\" }\n";
        let set = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap();
        let err = set.validate_tiers(&tiers).unwrap_err();
        assert!(err.to_string().contains("volatile"), "{err}");
        assert!(err.to_string().contains("ram-run"), "{err}");
    }

    #[test]
    fn an_impossible_down_pair_is_refused() {
        let dir = tmp();
        let ssd = dir.path().join("ssd");
        let hdd = dir.path().join("hdd");
        fs::create_dir_all(&ssd).unwrap();
        fs::create_dir_all(&hdd).unwrap();
        let tiers =
            TierSet::parse(&tiers_text(&ssd, &hdd), &dir.path().join("tiers.toml")).unwrap();

        // Same tier on both ends.
        let same = RuleSet::parse(
            "[[rule]]\nname = \"stay\"\nmatch = \"**\"\ndown = { after_idle = \"1d\", from = \"ssd\", to = \"ssd\" }\n",
            &dir.path().join("policy.toml"),
        )
        .unwrap();
        assert!(same.validate_tiers(&tiers).is_err());

        // A down that goes to a faster tier (hdd -> ssd) is an up, not a down.
        let upward = RuleSet::parse(
            "[[rule]]\nname = \"wrong-way\"\nmatch = \"**\"\ndown = { after_idle = \"1d\", from = \"hdd\", to = \"ssd\" }\n",
            &dir.path().join("policy.toml"),
        )
        .unwrap();
        let err = upward.validate_tiers(&tiers).unwrap_err();
        assert!(err.to_string().contains("wrong-way"), "{err}");
        assert!(err.to_string().contains("faster"), "{err}");
    }

    #[test]
    fn an_up_rule_with_no_down_is_refused() {
        let dir = tmp();
        let text = "[[rule]]\nname = \"only-up\"\nmatch = \"**\"\nup = { on_access = true }\n";
        let err = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap_err();
        assert!(err.to_string().contains("only-up"), "{err}");
    }

    #[test]
    fn a_duplicate_rule_name_is_refused() {
        let dir = tmp();
        let text = "[[rule]]\nname = \"twice\"\nmatch = \"a/**\"\ndown = { after_idle = \"1d\", from = \"ssd\", to = \"hdd\" }\n\n\
                    [[rule]]\nname = \"twice\"\nmatch = \"b/**\"\ndown = { after_idle = \"1d\", from = \"ssd\", to = \"hdd\" }\n";
        let err = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap_err();
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    #[test]
    fn a_default_file_that_is_absent_is_no_policy() {
        let dir = tmp();
        assert!(RuleSet::load_beside(dir.path()).unwrap().is_none());
        assert!(!RuleSet::default_path(dir.path()).exists());
    }

    #[test]
    fn an_explicit_missing_file_is_an_error_not_an_empty_set() {
        let dir = tmp();
        let err = RuleSet::load(&dir.path().join("nope.toml")).unwrap_err();
        assert!(matches!(err, PolicyError::Missing { .. }), "{err}");
    }

    #[test]
    fn a_later_matching_rule_overrides_an_earlier_one() {
        let dir = tmp();
        let text = "[[rule]]\nname = \"catch-all\"\nmatch = \"**\"\ndown = { after_idle = \"30d\", from = \"ssd\", to = \"hdd\" }\n\n\
                    [[rule]]\nname = \"camcorder-raw\"\nmatch = \"video/raw/**\"\ndown = { after_idle = \"3d\", from = \"ssd\", to = \"hdd\" }\n";
        let rules = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap();
        let ssd = dir.path().join("ssd");
        let hdd = dir.path().join("hdd");
        fs::create_dir_all(&ssd).unwrap();
        fs::create_dir_all(&hdd).unwrap();
        let tiers =
            TierSet::parse(&tiers_text(&ssd, &hdd), &dir.path().join("tiers.toml")).unwrap();
        let engine = Lifecycle::new(&rules, &tiers);

        let five_days = Duration::from_secs(5 * 86_400);
        // A 5-day idle file in video/raw is past the override's 3d.
        match engine.evaluate_down(Path::new("video/raw/clip.drp"), Some("ssd"), five_days) {
            DownDecision::Down { rule, to, .. } => {
                assert_eq!(rule, "camcorder-raw");
                assert_eq!(to, "hdd");
            }
            other => panic!("expected the override to fire, got {other:?}"),
        }
        // The same age outside video/raw is not cold under the catch-all's 30d.
        match engine.evaluate_down(Path::new("docs/readme.md"), Some("ssd"), five_days) {
            DownDecision::TooWarm { rule, .. } => assert_eq!(rule, "catch-all"),
            other => panic!("expected too-warm under the catch-all, got {other:?}"),
        }
    }

    #[test]
    fn a_pin_wins_over_the_idle_rule() {
        let dir = tmp();
        let rules = RuleSet::parse(sample(), &dir.path().join("policy.toml")).unwrap();
        let ssd = dir.path().join("ssd");
        let hdd = dir.path().join("hdd");
        fs::create_dir_all(&ssd).unwrap();
        fs::create_dir_all(&hdd).unwrap();
        let tiers =
            TierSet::parse(&tiers_text(&ssd, &hdd), &dir.path().join("tiers.toml")).unwrap();
        let engine = Lifecycle::new(&rules, &tiers);

        let ninety = Duration::from_secs(90 * 86_400);
        match engine.evaluate_down(Path::new("projects/active.drp"), Some("ssd"), ninety) {
            DownDecision::Pinned { rule, pattern } => {
                assert_eq!(rule, "intelligent-tiering");
                assert_eq!(pattern, "*.drp");
            }
            other => panic!("expected a pin, got {other:?}"),
        }
    }

    #[test]
    fn an_up_rule_fires_on_the_to_tier() {
        let dir = tmp();
        let rules = RuleSet::parse(sample(), &dir.path().join("policy.toml")).unwrap();
        let ssd = dir.path().join("ssd");
        let hdd = dir.path().join("hdd");
        fs::create_dir_all(&ssd).unwrap();
        fs::create_dir_all(&hdd).unwrap();
        let tiers =
            TierSet::parse(&tiers_text(&ssd, &hdd), &dir.path().join("tiers.toml")).unwrap();
        let engine = Lifecycle::new(&rules, &tiers);

        let rule = engine.evaluate_up(Path::new("a.bin"), Some("hdd")).unwrap();
        assert_eq!(rule.name, "intelligent-tiering");
        // On the hot tier there is nothing to promote.
        assert!(engine
            .evaluate_up(Path::new("a.bin"), Some("ssd"))
            .is_none());
    }

    #[test]
    fn no_matching_rule_is_reported_as_such() {
        let dir = tmp();
        let text = "[[rule]]\nname = \"only-media\"\nmatch = \"media/**\"\ndown = { after_idle = \"30d\", from = \"ssd\", to = \"hdd\" }\n";
        let rules = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap();
        let ssd = dir.path().join("ssd");
        let hdd = dir.path().join("hdd");
        fs::create_dir_all(&ssd).unwrap();
        fs::create_dir_all(&hdd).unwrap();
        let tiers =
            TierSet::parse(&tiers_text(&ssd, &hdd), &dir.path().join("tiers.toml")).unwrap();
        let engine = Lifecycle::new(&rules, &tiers);
        assert_eq!(
            engine.evaluate_down(Path::new("docs/a.md"), Some("ssd"), Duration::ZERO),
            DownDecision::NoRule
        );
    }

    #[test]
    fn durations_parse_in_units() {
        assert_eq!(
            parse_duration("30d").unwrap(),
            Duration::from_secs(30 * 86_400)
        );
        assert_eq!(
            parse_duration("12h").unwrap(),
            Duration::from_secs(12 * 3_600)
        );
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(90 * 60));
        assert_eq!(
            parse_duration("2w").unwrap(),
            Duration::from_secs(2 * 604_800)
        );
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("10 fortnights").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("-3d").is_err());
    }

    #[test]
    fn a_chain_is_one_rule_per_step_and_each_step_selects_by_current_tier() {
        let dir = tmp();
        let ssd = dir.path().join("ssd");
        let hdd = dir.path().join("hdd");
        let off = dir.path().join("off");
        for root in [&ssd, &hdd, &off] {
            fs::create_dir_all(root).unwrap();
        }
        let tiers = TierSet::parse(
            &format!(
                "[tiers.ssd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n\n\
                 [tiers.hdd_parked]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"s\"\ncopies = 1\n\n\
                 [tiers.offsite]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"min\"\ncopies = 1\n",
                ssd.display(),
                hdd.display(),
                off.display()
            ),
            &dir.path().join("tiers.toml"),
        )
        .unwrap();
        // A chain is one rule per step: the design's §5 snippet repeats the `down` key, which
        // TOML forbids, so each transition is its own `[[rule]]`. The tier the file is on
        // selects the step that applies.
        let text = "[[rule]]\nname = \"tiering-hot-to-mid\"\nmatch = \"**\"\n\
                    down = { after_idle = \"30d\", from = \"ssd\", to = \"hdd_parked\" }\n\
                    up = { on_access = true }\n\n\
                    [[rule]]\nname = \"tiering-mid-to-cold\"\nmatch = \"**\"\n\
                    down = { after_idle = \"180d\", from = \"hdd_parked\", to = \"offsite\" }\n";
        let rules = RuleSet::parse(text, &dir.path().join("policy.toml")).unwrap();
        rules.validate_tiers(&tiers).unwrap();
        let engine = Lifecycle::new(&rules, &tiers);

        match engine.evaluate_down(
            Path::new("a.bin"),
            Some("ssd"),
            Duration::from_secs(90 * 86_400),
        ) {
            DownDecision::Down { rule, to, .. } => {
                assert_eq!(rule, "tiering-hot-to-mid");
                assert_eq!(to, "hdd_parked");
            }
            other => panic!("expected ssd -> hdd_parked, got {other:?}"),
        }
        // On the middle tier the second step governs: 90d is not enough…
        match engine.evaluate_down(
            Path::new("a.bin"),
            Some("hdd_parked"),
            Duration::from_secs(90 * 86_400),
        ) {
            DownDecision::TooWarm { rule, to, .. } => {
                assert_eq!(rule, "tiering-mid-to-cold");
                assert_eq!(to, "offsite");
            }
            other => panic!("expected hdd_parked to be too warm, got {other:?}"),
        }
        // …and 200d is.
        match engine.evaluate_down(
            Path::new("a.bin"),
            Some("hdd_parked"),
            Duration::from_secs(200 * 86_400),
        ) {
            DownDecision::Down { rule, to, .. } => {
                assert_eq!(rule, "tiering-mid-to-cold");
                assert_eq!(to, "offsite");
            }
            other => panic!("expected hdd_parked -> offsite, got {other:?}"),
        }
    }
}

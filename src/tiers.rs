//! `tiers.toml`: the configured description of every tier of storage.
//!
//! A tier in `docs/design.md` §2 is *configured*, not discovered: a place where bytes
//! live, described by the driver that moves them (`kind`), the latency class a read pays
//! (`recall`), whether it survives a reboot (`volatility`), the durability floor within it
//! (`copies`), and what it costs (`cost`). The mover's filesystem roots are one driver's
//! view of that; this module is the config half — it gives those roots names, and answers
//! what class of tier a root is, so commands can speak in tier names instead of raw paths.
//!
//! ## Open-if-present, never created
//!
//! The default file lives beside the watch root. It is consulted only when it is already
//! there: a sweep must never create anything in the watched tree it was not asked to
//! (invariant 9), and a config conjured empty would answer "no tiers" for a tree whose
//! operator meant something. An explicitly named `--tiers` file is different — the operator
//! asked for *that* file, so a missing or malformed one is a usage error, never a silent
//! fallback to path-as-tier-name. Every malformed-config error names the line, because a
//! config error you cannot locate is indistinguishable from a tool that ignored you.
//!
//! ## Volatile tiers are mirrors, never homes
//!
//! §2.1 is explicit: anything volatile is a read cache in front of a durable tier, never
//! the place a file lives. [`TierSet`] carries the volatility so the CLI can refuse a
//! volatile tier as a `--dest` root (a destination is a home) and as a policy target;
//! everything else — treating a volatile tier as a copy location, a floor, a scrub source
//! — is later work, and named as out of scope rather than half-built here.
//!
//! ## `[[cache]]` blocks are overlays, not tiers
//!
//! A `[[cache]]` table (§2.1, issue #46) describes a promotion target in front of one
//! tier of record (`over`). It is parsed here, beside the tiers, but deliberately kept in
//! a separate list: [`TierSet::tiers`] never yields a cache, so nothing that walks tiers —
//! the catalog's locations, a copy floor, scrub, policy — can mistake one for a home. The
//! overlay's runtime behaviour lives in [`crate::cache`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// The default file name, beside the watch root.
pub const TIERS_FILE_NAME: &str = "tiers.toml";

/// Whether a tier survives a reboot and a disk swap. §2.1: only a `persistent` tier can be
/// a file's home; a `volatile` one is a promotion target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Volatility {
    Persistent,
    Volatile,
}

impl Volatility {
    pub fn as_str(self) -> &'static str {
        match self {
            Volatility::Persistent => "persistent",
            Volatility::Volatile => "volatile",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "persistent" => Some(Volatility::Persistent),
            "volatile" => Some(Volatility::Volatile),
            _ => None,
        }
    }
}

/// The recall-latency class of a tier: how long serving one read takes. Ordered from
/// fastest to slowest, so a policy can compare two tiers without a lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Recall {
    Us,
    Ms,
    S,
    Min,
    Hours,
}

impl Recall {
    pub fn as_str(self) -> &'static str {
        match self {
            Recall::Us => "us",
            Recall::Ms => "ms",
            Recall::S => "s",
            Recall::Min => "min",
            Recall::Hours => "hours",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "us" => Some(Recall::Us),
            "ms" => Some(Recall::Ms),
            "s" => Some(Recall::S),
            "min" => Some(Recall::Min),
            "hours" => Some(Recall::Hours),
            _ => None,
        }
    }
}

/// Which transport driver serves a tier: §2's `kind` field, "which transport driver serves
/// this tier". The variants are the ones the design names, so the type can answer the two
/// questions the later drivers need — whether a driver exists yet, and whether the tier's
/// bytes leave this machine — without every caller re-deriving them from a string.
///
/// Until now `kind` was parsed as free text and never read again: only `fs` existed, and a
/// tier written `kind = "object"` with a `path` was silently served as an ordinary local
/// directory — the mover treating a destination as something it had not understood
/// (invariant 3). The parse below refuses that instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierKind {
    /// A local filesystem root: copy + fsync across a tier edge, `EXDEV` and all.
    Fs,
    /// A cloud object store: chunked upload with resumable state.
    Object,
    /// Another host's tier, reached over the LAN.
    Peer,
    /// A volume a person inserts: export to it plus a catalog handshake.
    Offline,
}

impl TierKind {
    /// Every kind the design names, in the order a refusal lists them.
    pub const ALL: [TierKind; 4] = [
        TierKind::Fs,
        TierKind::Object,
        TierKind::Peer,
        TierKind::Offline,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TierKind::Fs => "fs",
            TierKind::Object => "object",
            TierKind::Peer => "peer",
            TierKind::Offline => "offline",
        }
    }

    /// The supported set as a refusal message spells it: `fs, object, peer, offline`.
    pub fn names() -> String {
        TierKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "fs" => Some(TierKind::Fs),
            "object" => Some(TierKind::Object),
            "peer" => Some(TierKind::Peer),
            "offline" => Some(TierKind::Offline),
            _ => None,
        }
    }

    /// True when a transport driver for this kind exists. `fs`, `object` and `peer` are
    /// served; `offline` is the one kind the design names that no driver serves yet. The
    /// parse refuses a kind without one, so this is the single place to flip as drivers
    /// land — and the tests that pin the refusals are the reminder to flip it.
    pub fn is_served(self) -> bool {
        matches!(self, TierKind::Fs | TierKind::Object | TierKind::Peer)
    }

    /// §2 rule 2: "anything crossing the machine boundary is encrypted first." The envelope
    /// is required exactly where this is true, which is what the seam has to answer before a
    /// driver places any bytes.
    ///
    /// `Object` and `Peer` cross because the bytes leave the host. `Offline` crosses because
    /// the volume leaves it: a drawer is further out of reach than a bucket, not less, so the
    /// export path is the same requirement.
    pub fn crosses_machine_boundary(self) -> bool {
        match self {
            TierKind::Fs => false,
            TierKind::Object | TierKind::Peer | TierKind::Offline => true,
        }
    }
}

impl std::fmt::Display for TierKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One configured tier: §2's fields, with the name it was declared under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    /// The table key: `[tiers.ssd]` gives the name `ssd`. This is what commands print.
    pub name: String,
    /// Which transport driver serves this tier: `fs`, `object` and `peer` are served;
    /// a config naming another kind (`offline`) is refused rather than served as a
    /// local directory.
    pub kind: TierKind,
    /// The filesystem root this tier's bytes sit under. For `kind = "fs"` this is the
    /// tier root; for `kind = "object"` or `kind = "peer"` this is a local scratch
    /// directory for downloads and partial upload buffers.
    pub path: PathBuf,
    pub volatility: Volatility,
    pub recall: Recall,
    /// Durability floor within the tier (§6): the copy count the engine maintains.
    pub copies: usize,
    /// Optional cost model ($/GB-month, a W-idle figure, …), kept as written. Nothing
    /// acts on it yet — placement decisions are P4 — but it is part of the tier's honest
    /// description and printing it is how a reader checks it was parsed.
    pub cost: Option<String>,
    /// The object-store configuration, present when `kind == Object` or `kind == Peer`.
    /// Both kinds are served by the same S3-compatible driver (a peer runs the
    /// object-server, so its client config is the object-store config). Kept here so
    /// callers that match on `kind` can reach the config without a second lookup.
    pub object_config: Option<crate::object_store::ObjectTierConfig>,
}

impl Tier {
    /// True when this tier is a home in the §2.1 sense: a destination root a file may
    /// actually live on.
    pub fn is_home(&self) -> bool {
        self.volatility == Volatility::Persistent
    }
}

/// Why a tier config could not be read.
#[derive(Debug, Error)]
pub enum TiersError {
    #[error("cannot read the tier config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// An explicitly named file that is not there. Never a fallback: the operator asked
    /// for this file, and silently answering from paths instead would answer a different
    /// question.
    #[error("--tiers {path} does not exist")]
    Missing { path: PathBuf },
    /// A syntax or type error. `toml`'s own rendering names the line and column, so the
    /// message is passed through rather than replaced with a vaguer one.
    #[error("malformed tier config {path}: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// A syntactically valid file that says something impossible. The detail begins with
    /// `line N` when the offending field's span is known.
    #[error("invalid tier config {path}: {detail}")]
    Invalid { path: PathBuf, detail: String },
}

/// A parsed `tiers.toml`: every configured tier, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TierSet {
    tiers: Vec<Tier>,
    /// `[[cache]]` overlays, kept apart from `tiers` so no tier walk can yield one.
    caches: Vec<CacheConfig>,
    /// The file this set was read from. `tiers.toml` may live inside the watched tree
    /// (the default is beside the watch root), so a sweep must be able to recognise its
    /// own config and leave it alone — see `Lifecycle::is_config_file`.
    path: PathBuf,
}

impl TierSet {
    /// The file this set was read from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The default file beside a watch root.
    pub fn default_path(watch: &Path) -> PathBuf {
        watch.join(TIERS_FILE_NAME)
    }

    /// Read and parse an explicitly named config. A missing file is an error.
    pub fn load(path: &Path) -> Result<TierSet, TiersError> {
        let text = fs::read_to_string(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                TiersError::Missing {
                    path: path.to_path_buf(),
                }
            } else {
                TiersError::Read {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
        TierSet::parse(&text, path)
    }

    /// Read the default config beside `dir`, or `None` when it is not there. This is the
    /// open-if-present path: it never creates the file, and a file that *is* there but is
    /// malformed is still an error (a broken config is not the same as no config).
    pub fn load_beside(dir: &Path) -> Result<Option<TierSet>, TiersError> {
        let path = dir.join(TIERS_FILE_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        TierSet::load(&path).map(Some)
    }

    /// Parse the TOML text. Split out from [`load`] so the error paths are unit-testable
    /// without a filesystem.
    pub fn parse(text: &str, path: &Path) -> Result<TierSet, TiersError> {
        let raw: RawFile = toml::from_str(text).map_err(|source| TiersError::Malformed {
            path: path.to_path_buf(),
            source,
        })?;

        let mut tiers = Vec::with_capacity(raw.tiers.len());
        for (name, tier) in raw.tiers {
            tiers.push(tier.into_tier(&name, text, path)?);
        }
        let mut caches: Vec<CacheConfig> = Vec::with_capacity(raw.cache.len());
        for cache in raw.cache {
            let cache = cache.into_cache(&tiers, &caches, text, path)?;
            caches.push(cache);
        }
        Ok(TierSet {
            tiers,
            caches,
            path: path.to_path_buf(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    pub fn tiers(&self) -> &[Tier] {
        &self.tiers
    }

    /// Every `[[cache]]` overlay, in declaration order. Never part of [`TierSet::tiers`].
    pub fn caches(&self) -> &[CacheConfig] {
        &self.caches
    }

    /// The overlay declared under `name`, if any.
    pub fn cache(&self, name: &str) -> Option<&CacheConfig> {
        self.caches.iter().find(|cache| cache.name == name)
    }

    /// The overlay whose `path` is `root`, contains it, or sits inside it. A destination
    /// root that overlaps an overlay in either direction would make cache bytes a home, so
    /// the overlap — not only equality — is what the refusal is made of.
    pub fn cache_overlapping(&self, root: &Path) -> Option<&CacheConfig> {
        let wanted = canonical_or(root);
        self.caches.iter().find(|cache| {
            let path = canonical_or(&cache.path);
            wanted.starts_with(&path) || path.starts_with(&wanted)
        })
    }

    pub fn get(&self, name: &str) -> Option<&Tier> {
        self.tiers.iter().find(|tier| tier.name == name)
    }

    /// The configured tier whose `path` is `root`, if any. Both sides are canonicalized
    /// when they can be (a `--dest` root exists by the time this is asked; a tier's disk
    /// may be unmounted, in which case the lexical path is the best answer available).
    pub fn tier_for_root(&self, root: &Path) -> Option<&Tier> {
        let wanted = canonical_or(root);
        self.tiers
            .iter()
            .find(|tier| canonical_or(&tier.path) == wanted)
    }

    /// The configured tier whose `path` *contains* a file, deepest first. Unlike
    /// [`TierSet::tier_for_root`] this is a prefix match, because a file is not a root: a
    /// watched tree is usually a subtree of a tier's pool, and a rule's `from` must name
    /// the tier the file actually lives on, not the root the operator spelled on the CLI.
    pub fn tier_containing(&self, path: &Path) -> Option<&Tier> {
        let wanted = canonical_or(path);
        self.tiers
            .iter()
            .filter(|tier| wanted.starts_with(canonical_or(&tier.path)))
            .max_by_key(|tier| canonical_or(&tier.path).components().count())
    }

    /// The name to print for a tier root: the configured name when the root is a tier,
    /// and the raw path otherwise. With no config this is exactly the path-as-tier-name
    /// behaviour the tool had before tiers existed.
    pub fn name_for_root(&self, root: &Path) -> String {
        match self.tier_for_root(root) {
            Some(tier) => tier.name.clone(),
            None => root.to_string_lossy().into_owned(),
        }
    }

    /// The name to print for a *recorded* tier string (a catalog `location.tier`, which is
    /// a canonical path). The `file://`-free spelling is the same comparison as
    /// [`name_for_root`]; a string that is not a configured path is returned unchanged.
    pub fn name_for_tier_key(&self, key: &str) -> String {
        match self.tier_for_root(Path::new(key)) {
            Some(tier) => tier.name.clone(),
            None => key.to_string(),
        }
    }

    /// The recall class recorded for a tier key, if it is a configured tier.
    pub fn recall_for_tier_key(&self, key: &str) -> Option<Recall> {
        self.tier_for_root(Path::new(key)).map(|tier| tier.recall)
    }

    /// Every tier configured as volatile. The CLI refuses these as destination roots; this
    /// is the set that refusal is made of.
    pub fn volatile(&self) -> impl Iterator<Item = &Tier> {
        self.tiers.iter().filter(|tier| !tier.is_home())
    }

    /// The readable description of the configured tiers, for `locate`/`audit` output.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("tiers: {} configured", self.tiers.len())];
        for tier in &self.tiers {
            lines.push(format!("  {}", tier.describe()));
        }
        if !self.caches.is_empty() {
            lines.push(format!(
                "caches: {} configured (ephemeral overlays, never data of record)",
                self.caches.len()
            ));
            for cache in &self.caches {
                lines.push(format!("  {}", cache.describe()));
            }
        }
        lines
    }
}

impl Tier {
    /// One line: name, driver, root, volatility, recall, floor, and cost when given.
    pub fn describe(&self) -> String {
        let mut line = format!(
            "{}: kind={} path={} volatility={} recall={} copies={}",
            self.name,
            self.kind,
            self.path.display(),
            self.volatility.as_str(),
            self.recall.as_str(),
            self.copies
        );
        if let Some(cost) = &self.cost {
            line.push_str(&format!(" cost={cost}"));
        }
        if let Some(ref obj) = self.object_config {
            use std::fmt::Write;
            let _ = write!(line, " endpoint={} bucket={}", obj.endpoint, obj.bucket);
        }
        line
    }
}

/// The raw serde shape. Every scalar is wrapped in `toml::Spanned` so a semantic error
/// (an unknown `volatility`, a `copies` of zero) can name the line it sits on — a config
/// error that does not say where it is costs the reader the search that the file's whole
/// point is to avoid.
#[derive(Debug, Deserialize)]
struct RawFile {
    tiers: BTreeMap<String, RawTier>,
    #[serde(default)]
    cache: Vec<RawCache>,
}

/// How often a file must be read inside a window before it earns a cache copy:
/// `promote_on = "2 accesses / 24h"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromoteOn {
    pub accesses: u32,
    pub window: std::time::Duration,
}

impl PromoteOn {
    /// Parse `"N accesses / DURATION"` (`access` is accepted for N = 1).
    pub fn parse(text: &str) -> Result<PromoteOn, String> {
        let (count, window) = text
            .split_once('/')
            .ok_or_else(|| format!("`{text}` must be `N accesses / DURATION`"))?;
        let mut words = count.split_whitespace();
        let number = words.next().unwrap_or("");
        let unit = words.next().unwrap_or("");
        if words.next().is_some() || !matches!(unit, "access" | "accesses") {
            return Err(format!("`{text}` must be `N accesses / DURATION`"));
        }
        let accesses: u32 = number
            .parse()
            .map_err(|_| format!("`{number}` in `{text}` is not a whole number of accesses"))?;
        if accesses == 0 {
            // Zero would promote every file ever opened, which is a full mirror, not a
            // cache — and almost certainly a typo.
            return Err(format!("`{text}` needs at least 1 access"));
        }
        let window = crate::policy::parse_duration(window)
            .map_err(|err| format!("window in `{text}`: {err}"))?;
        if window.is_zero() {
            return Err(format!(
                "`{text}` has a zero window, which no access can meet"
            ));
        }
        Ok(PromoteOn { accesses, window })
    }
}

/// One `[[cache]]` overlay (§2.1): a promotion target in front of tier `over`.
///
/// Only the values the design names are accepted: `kind = "fs"`, `evict = "lru"`,
/// `write_policy = "write-invalidate"`. A writeback cache is refused rather than parsed,
/// because RAM has no battery and a writeback overlay on a storage server is a data-loss
/// design (§2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    pub name: String,
    /// The tier of record this overlay accelerates. Always a configured persistent tier.
    pub over: String,
    pub kind: String,
    pub path: PathBuf,
    pub max_size: u64,
    pub promote_on: PromoteOn,
}

impl CacheConfig {
    pub fn describe(&self) -> String {
        format!(
            "{}: over={} kind={} path={} max_size={} promote_on={} accesses / {}s evict=lru \
             write_policy=write-invalidate (ephemeral)",
            self.name,
            self.over,
            self.kind,
            self.path.display(),
            crate::scope::human_bytes(self.max_size),
            self.promote_on.accesses,
            self.promote_on.window.as_secs()
        )
    }
}

#[derive(Debug, Deserialize)]
struct RawCache {
    name: toml::Spanned<String>,
    over: toml::Spanned<String>,
    kind: toml::Spanned<String>,
    path: toml::Spanned<String>,
    max_size: toml::Spanned<String>,
    promote_on: toml::Spanned<String>,
    evict: toml::Spanned<String>,
    write_policy: toml::Spanned<String>,
}

impl RawCache {
    fn into_cache(
        self,
        tiers: &[Tier],
        earlier: &[CacheConfig],
        text: &str,
        file: &Path,
    ) -> Result<CacheConfig, TiersError> {
        let invalid = |span: std::ops::Range<usize>, detail: String| TiersError::Invalid {
            path: file.to_path_buf(),
            detail: format!("line {}: {detail}", line_at(text, span.start)),
        };
        let name = self.name.get_ref().trim().to_string();
        if name.is_empty() {
            return Err(invalid(
                self.name.span(),
                "a cache has an empty `name`".into(),
            ));
        }
        // A cache sharing a tier's name would let a policy rule or a `--dest` lookup by
        // name resolve to the overlay, so the namespaces are kept disjoint.
        if tiers.iter().any(|tier| tier.name == name) || earlier.iter().any(|c| c.name == name) {
            return Err(invalid(
                self.name.span(),
                format!("cache `{name}` reuses a name already given to a tier or cache"),
            ));
        }
        let over_name = self.over.get_ref().trim();
        let over =
            tiers
                .iter()
                .find(|tier| tier.name == over_name)
                .ok_or_else(|| {
                    invalid(
                self.over.span(),
                format!("cache `{name}`: `over` names tier `{over_name}`, which is not configured"),
            )
                })?;
        if !over.is_home() {
            return Err(invalid(
                self.over.span(),
                format!(
                    "cache `{name}`: `over` names volatile tier `{over_name}`; an overlay sits in \
                     front of a tier of record"
                ),
            ));
        }
        let kind = self.kind.get_ref().trim().to_string();
        if kind != "fs" {
            return Err(invalid(
                self.kind.span(),
                format!("cache `{name}`: kind `{kind}` is not supported; only `fs` exists"),
            ));
        }
        let raw_path = self.path.get_ref().trim();
        let path = PathBuf::from(raw_path);
        if raw_path.is_empty() || !path.is_absolute() {
            return Err(invalid(
                self.path.span(),
                format!("cache `{name}`: path `{raw_path}` must be a non-empty absolute path"),
            ));
        }
        // An overlay inside (or around) a tier root would put cache bytes where the walk,
        // the catalog sync and scrub read data of record — exactly the confusion §2.1
        // forbids. Refuse the overlap rather than hope every reader skips it.
        let canonical = canonical_or(&path);
        if let Some(tier) = tiers.iter().find(|tier| {
            let root = canonical_or(&tier.path);
            canonical.starts_with(&root) || root.starts_with(&canonical)
        }) {
            return Err(invalid(
                self.path.span(),
                format!(
                    "cache `{name}`: path {} overlaps tier `{}` ({}); an overlay must live \
                     outside every tier of record",
                    path.display(),
                    tier.name,
                    tier.path.display()
                ),
            ));
        }
        let max_size = crate::scope::parse_size(self.max_size.get_ref()).map_err(|err| {
            invalid(
                self.max_size.span(),
                format!("cache `{name}`: max_size: {err}"),
            )
        })?;
        if max_size == 0 {
            return Err(invalid(
                self.max_size.span(),
                format!("cache `{name}`: max_size must be greater than zero"),
            ));
        }
        let promote_on = PromoteOn::parse(self.promote_on.get_ref()).map_err(|err| {
            invalid(
                self.promote_on.span(),
                format!("cache `{name}`: promote_on {err}"),
            )
        })?;
        if self.evict.get_ref().trim() != "lru" {
            return Err(invalid(
                self.evict.span(),
                format!(
                    "cache `{name}`: evict `{}` is not supported; only `lru` exists",
                    self.evict.get_ref()
                ),
            ));
        }
        if self.write_policy.get_ref().trim() != "write-invalidate" {
            return Err(invalid(
                self.write_policy.span(),
                format!(
                    "cache `{name}`: write_policy `{}` is refused; only `write-invalidate` is \
                     safe — a writeback overlay in RAM loses acknowledged writes on power loss \
                     (docs/design.md §2.1)",
                    self.write_policy.get_ref()
                ),
            ));
        }
        Ok(CacheConfig {
            name,
            over: over.name.clone(),
            kind,
            path,
            max_size,
            promote_on,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawTier {
    kind: toml::Spanned<String>,
    path: toml::Spanned<String>,
    volatility: toml::Spanned<String>,
    recall: toml::Spanned<String>,
    copies: toml::Spanned<i64>,
    #[serde(default)]
    cost: Option<toml::Spanned<toml::Value>>,
    // Object-store fields (#141): required when kind = "object", ignored otherwise.
    #[serde(default)]
    endpoint: Option<toml::Spanned<String>>,
    #[serde(default)]
    bucket: Option<toml::Spanned<String>>,
    #[serde(default)]
    prefix: Option<toml::Spanned<String>>,
    #[serde(default)]
    region: Option<toml::Spanned<String>>,
    /// Path to a credentials file or an environment variable name. For an env var,
    /// the value must start with `$` (e.g. `$S3_CREDS`).
    #[serde(default)]
    credential_source: Option<toml::Spanned<String>>,
    /// Envelope encryption key: a file path or `$ENV_VAR`. Required for `kind = "object"`
    /// (bytes cross the machine boundary, §2 rule 2). Same format as `credential_source`.
    #[serde(default)]
    encryption_key: Option<toml::Spanned<String>>,
    /// When true, use plain HTTP instead of TLS. Exposed as a tiers.toml field for
    /// loopback testing; a real prod config must never set this.
    #[serde(default)]
    insecure: bool,
    #[serde(default)]
    chunk_size: Option<toml::Spanned<String>>,
}

impl RawTier {
    fn into_tier(self, name: &str, text: &str, file: &Path) -> Result<Tier, TiersError> {
        let invalid = |detail: String| TiersError::Invalid {
            path: file.to_path_buf(),
            detail,
        };

        let raw_kind = self.kind.get_ref().trim();
        if raw_kind.is_empty() {
            return Err(invalid(format!(
                "line {}: tier `{name}` has an empty `kind`",
                line_at(text, self.kind.span().start)
            )));
        }
        let kind = TierKind::parse(raw_kind).ok_or_else(|| {
            invalid(format!(
                "line {}: tier `{name}` kind `{raw_kind}` is not one of {}; a tier's `kind` \
                 names the transport driver that serves it",
                line_at(text, self.kind.span().start),
                TierKind::names()
            ))
        })?;
        // A kind the design names but no driver serves yet. Refused rather than accepted,
        // because the alternative is what happened before this seam existed: the tier parsed
        // and then behaved as a local directory, so the mover would have written a file to a
        // root it had not understood (invariant 3). Flipping `is_served` is what a driver
        // landing changes — and the test that pins this refusal is the reminder.
        if !kind.is_served() {
            return Err(invalid(format!(
                "line {}: tier `{name}` kind `{}` has no transport driver yet; `fs`, \
                 `object` and `peer` are served, so this tier cannot be a file's home",
                line_at(text, self.kind.span().start),
                kind
            )));
        }

        let raw_path = self.path.get_ref().trim();
        if raw_path.is_empty() {
            return Err(invalid(format!(
                "line {}: tier `{name}` has an empty `path`",
                line_at(text, self.path.span().start)
            )));
        }
        let path = PathBuf::from(raw_path);
        if !path.is_absolute() {
            return Err(invalid(format!(
                "line {}: tier `{name}` path `{raw_path}` must be absolute; catalogue roots \
                 are absolute paths",
                line_at(text, self.path.span().start)
            )));
        }

        let volatility = Volatility::parse(self.volatility.get_ref()).ok_or_else(|| {
            invalid(format!(
                "line {}: tier `{name}` volatility `{}` must be `persistent` or `volatile`",
                line_at(text, self.volatility.span().start),
                self.volatility.get_ref()
            ))
        })?;

        let recall = Recall::parse(self.recall.get_ref()).ok_or_else(|| {
            invalid(format!(
                "line {}: tier `{name}` recall `{}` must be one of us, ms, s, min, hours",
                line_at(text, self.recall.span().start),
                self.recall.get_ref()
            ))
        })?;

        let copies = *self.copies.get_ref();
        if copies < 1 {
            return Err(invalid(format!(
                "line {}: tier `{name}` copies must be at least 1, got {copies}",
                line_at(text, self.copies.span().start)
            )));
        }

        let cost = self.cost.map(|cost| match cost.into_inner() {
            // A quoted string is the value, not its TOML spelling: `"$0.02"` means the
            // string `$0.02`, and printing the quotes would misreport the config.
            toml::Value::String(text) => text,
            other => other.to_string(),
        });
        let cost = cost.filter(|cost| !cost.trim().is_empty());

        // Object-tier validation (#141, extended by #142): require the remote-store
        // fields when kind is Object or Peer (a peer runs the object-server, so it is
        // the same S3-compatible driver and the same config shape), and leave
        // object_config as None for fs tiers.
        let object_config = if matches!(kind, TierKind::Object | TierKind::Peer) {
            let endpoint = self.endpoint.ok_or_else(|| {
                invalid(format!(
                    "line {}: tier `{name}` kind `{kind}` requires `endpoint` (the \
                     S3-compatible endpoint host the driver connects to)",
                    line_at(text, self.kind.span().start)
                ))
            })?;
            let endpoint_str = endpoint.get_ref().trim().to_string();
            if endpoint_str.is_empty() {
                return Err(invalid(format!(
                    "line {}: tier `{name}` endpoint is empty",
                    line_at(text, endpoint.span().start)
                )));
            }

            let bucket = self.bucket.ok_or_else(|| {
                invalid(format!(
                    "line {}: tier `{name}` kind `{kind}` requires `bucket`",
                    line_at(text, self.kind.span().start)
                ))
            })?;
            let bucket_str = bucket.get_ref().trim().to_string();
            if bucket_str.is_empty() {
                return Err(invalid(format!(
                    "line {}: tier `{name}` bucket is empty",
                    line_at(text, bucket.span().start)
                )));
            }

            // A region is required for a cloud bucket (it signs every request); a peer
            // has no meaningful region — the object-server derives the signing region
            // from the request's credential scope, so the value only has to be
            // self-consistent, and the driver defaults it to `peer`.
            let region_str = if kind == TierKind::Object {
                let region = self.region.ok_or_else(|| {
                    invalid(format!(
                        "line {}: tier `{name}` kind `object` requires `region` (the AWS \
                         region for SigV4 signing)",
                        line_at(text, self.kind.span().start)
                    ))
                })?;
                let region_str = region.get_ref().trim().to_string();
                if region_str.is_empty() {
                    return Err(invalid(format!(
                        "line {}: tier `{name}` region is empty",
                        line_at(text, region.span().start)
                    )));
                }
                region_str
            } else {
                self.region
                    .map(|r| r.get_ref().trim().to_string())
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| "peer".to_string())
            };

            let credential_source = self.credential_source.ok_or_else(|| {
                invalid(format!(
                    "line {}: tier `{name}` kind `{kind}` requires `credential_source` \
                     (a file path or an environment variable prefixed with `$`)",
                    line_at(text, self.kind.span().start)
                ))
            })?;
            let cred_str = credential_source.get_ref().trim();
            let cred = if let Some(var) = cred_str.strip_prefix('$') {
                crate::object_store::CredentialSource::Env(var.to_string())
            } else {
                crate::object_store::CredentialSource::File(PathBuf::from(cred_str))
            };

            let encryption_key = self.encryption_key.ok_or_else(|| {
                invalid(format!(
                    "line {}: tier `{name}` kind `{kind}` requires `encryption_key` \
                         (a file path or an environment variable prefixed with `$`; the key is \
                         64 hex characters, 32 bytes — docs/design.md §2 rule 2)",
                    line_at(text, self.kind.span().start)
                ))
            })?;
            let enc_key_str = encryption_key.get_ref().trim();
            let enc_key = if let Some(var) = enc_key_str.strip_prefix('$') {
                crate::object_store::CredentialSource::Env(var.to_string())
            } else {
                crate::object_store::CredentialSource::File(PathBuf::from(enc_key_str))
            };

            let prefix = self
                .prefix
                .map(|p| p.into_inner().trim().to_string())
                .filter(|p| !p.is_empty());

            let chunk_size = match self.chunk_size {
                Some(ref cs) => {
                    let size_str = cs.get_ref().trim();
                    crate::scope::parse_size(size_str).map_err(|err| {
                        invalid(format!(
                            "line {}: tier `{name}` chunk_size {err}",
                            line_at(text, cs.span().start)
                        ))
                    })?
                }
                None => crate::object_store::CHUNK_SIZE,
            };

            let remote_kind = match kind {
                TierKind::Object => crate::object_store::RemoteKind::S3,
                TierKind::Peer => crate::object_store::RemoteKind::Peer,
                _ => unreachable!("object_config is built only for object and peer tiers"),
            };

            Some(crate::object_store::ObjectTierConfig {
                name: name.to_string(),
                remote_kind,
                endpoint: endpoint_str,
                bucket: bucket_str,
                prefix,
                region: region_str,
                insecure: self.insecure,
                credentials: cred,
                chunk_size,
                encryption_key: enc_key,
            })
        } else {
            None
        };

        Ok(Tier {
            name: name.to_string(),
            kind,
            path,
            volatility,
            recall,
            copies: copies as usize,
            cost,
            object_config,
        })
    }
}

/// Canonicalize when the path exists, and fall back to the lexical path when it does not
/// (an unmounted tier's disk is exactly when the config still has to mean something).
fn canonical_or(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The 1-based line number holding a byte offset. `toml`'s spans are byte offsets into the
/// text that was parsed, so counting newlines is exact.
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

    fn sample() -> &'static str {
        r#"
[tiers.ssd]
kind = "fs"
path = "/mnt/nvme-pool"
volatility = "persistent"
recall = "ms"
copies = 1

[tiers.hdd]
kind = "fs"
path = "/mnt/hdd-pool"
volatility = "persistent"
recall = "s"
copies = 2
cost = "$0.02"
"#
    }

    #[test]
    fn two_tiers_parse_with_every_field() {
        let dir = tmp();
        let set = TierSet::parse(sample(), &dir.path().join("tiers.toml")).unwrap();
        assert_eq!(set.tiers().len(), 2);
        let ssd = set.get("ssd").expect("ssd");
        assert_eq!(ssd.kind, TierKind::Fs);
        assert_eq!(ssd.path, PathBuf::from("/mnt/nvme-pool"));
        assert_eq!(ssd.volatility, Volatility::Persistent);
        assert_eq!(ssd.recall, Recall::Ms);
        assert_eq!(ssd.copies, 1);
        assert_eq!(ssd.cost, None);
        let hdd = set.get("hdd").expect("hdd");
        assert_eq!(hdd.recall, Recall::S);
        assert_eq!(hdd.copies, 2);
        assert_eq!(hdd.cost.as_deref(), Some("$0.02"));
    }

    #[test]
    fn a_volatile_tier_parses_and_is_not_a_home() {
        let dir = tmp();
        let text = r#"
[tiers.ram]
kind = "fs"
path = "/mnt/ramdisk"
volatility = "volatile"
recall = "us"
copies = 1
"#;
        let set = TierSet::parse(text, &dir.path().join("tiers.toml")).unwrap();
        let ram = set.get("ram").unwrap();
        assert_eq!(ram.volatility, Volatility::Volatile);
        assert!(!ram.is_home());
        assert_eq!(set.volatile().count(), 1);
    }

    /// A malformed config names the line. This is the acceptance criterion made concrete:
    /// a syntax error the reader can go straight to.
    #[test]
    fn a_syntax_error_names_its_line() {
        let dir = tmp();
        // `copies` is unclosed on line 6.
        let text = "[tiers.ssd]\nkind = \"fs\"\npath = \"/mnt/ssd\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = \n";
        let err = TierSet::parse(text, &dir.path().join("tiers.toml")).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("line 6"), "{rendered}");
    }

    /// A semantically impossible value names its line too — a bad enum is not a syntax
    /// error, and the search it would otherwise cost is the whole reason for the span.
    #[test]
    fn a_bad_enum_names_its_line() {
        let dir = tmp();
        let text = "[tiers.ssd]\nkind = \"fs\"\npath = \"/mnt/ssd\"\nvolatility = \"sometimes\"\nrecall = \"ms\"\ncopies = 1\n";
        let err = TierSet::parse(text, &dir.path().join("tiers.toml")).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("line 4"), "{rendered}");
        assert!(rendered.contains("volatility"), "{rendered}");
    }

    #[test]
    fn a_zero_copy_floor_is_refused_by_line() {
        let dir = tmp();
        let text = "[tiers.ssd]\nkind = \"fs\"\npath = \"/mnt/ssd\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 0\n";
        let err = TierSet::parse(text, &dir.path().join("tiers.toml")).unwrap_err();
        assert!(err.to_string().contains("line 6"), "{err}");
    }

    #[test]
    fn a_relative_path_is_refused_by_line() {
        let dir = tmp();
        let text = "[tiers.ssd]\nkind = \"fs\"\npath = \"nvme-pool\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n";
        let err = TierSet::parse(text, &dir.path().join("tiers.toml")).unwrap_err();
        assert!(err.to_string().contains("line 3"), "{err}");
    }

    #[test]
    fn an_explicit_missing_file_is_an_error_not_an_empty_set() {
        let dir = tmp();
        let err = TierSet::load(&dir.path().join("nope.toml")).unwrap_err();
        assert!(matches!(err, TiersError::Missing { .. }), "{err}");
    }

    #[test]
    fn a_default_file_that_is_absent_is_no_config() {
        let dir = tmp();
        assert!(TierSet::load_beside(dir.path()).unwrap().is_none());
        assert!(!TierSet::default_path(dir.path()).exists());
    }

    #[test]
    fn a_tier_root_is_named_and_falls_back_to_its_path() {
        let dir = tmp();
        let root = dir.path().join("cold");
        fs::create_dir_all(&root).unwrap();
        let text = format!(
            "[tiers.cold-disk]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n",
            root.display()
        );
        let set = TierSet::parse(&text, &dir.path().join("tiers.toml")).unwrap();
        assert_eq!(set.name_for_root(&root), "cold-disk");
        assert_eq!(
            set.recall_for_tier_key(&root.to_string_lossy()),
            Some(Recall::Ms)
        );
        // A root the config does not name keeps the path-as-tier-name behaviour.
        let other = dir.path().join("elsewhere");
        assert_eq!(set.name_for_root(&other), other.to_string_lossy());
        assert_eq!(set.recall_for_tier_key("/nowhere"), None);
    }
}

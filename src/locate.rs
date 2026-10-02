//! `locate`: find an object by namespace path or by content digest, and report every copy.
//!
//! The mover answers "where is this file" by walking symlinks, which stops working the
//! moment a tier is not mounted. The catalog (`docs/design.md` §3) answers it as a query,
//! and this command is that query. Given a path it reads the `name` table; given a content
//! digest it searches the `object` table by id prefix. Either way the answer is the same
//! shape: every copy the catalog records, which one is the tier of record, and the state
//! the object is in — `present`, `offloaded`, or `restoring`.
//!
//! ## Path or digest, and how the two are told apart
//!
//! A query of at least [`MIN_DIGEST_PREFIX`] hex characters is read as a digest prefix,
//! because a BLAKE3 id *is* hex and a full one is 64 characters; anything else is a path.
//! The two spaces overlap only in theory — a namespace path that is also eight hex
//! characters would be read as a digest — and the ambiguity is reported (`kind` in the
//! JSON), not hidden. A digest query is never a guess: a prefix can match several objects
//! and all of them are listed, because "which one did you mean" is the user's answer to
//! give, not this command's to assume.
//!
//! Nothing here opens a file or reads a byte: the answer is the catalog's, and it stays
//! answerable when the bytes themselves are on an unmounted disk. This is deliberately the
//! read-only half of the recall story; bringing the bytes back is `restore`.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::catalog::{Catalog, CatalogError, ObjectRecord};

/// Exit code for a query that matched at least one object.
pub const EXIT_FOUND: u8 = 0;
/// Exit code for a query that matched nothing. Deliberately not an error: "not here" is
/// an answer a script needs, not a failure.
pub const EXIT_NOT_FOUND: u8 = 1;

/// The shortest hex run read as a digest prefix rather than a path. Eight hex characters
/// is 32 bits of a BLAKE3 id — long enough that a path which is *also* eight hex
/// characters is a coincidence worth naming, not the common case.
pub const MIN_DIGEST_PREFIX: usize = 8;

/// How a query was read. The caller gets to see which, so the overlap above is never a
/// silent reinterpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// A namespace path, looked up in `name`.
    Path,
    /// A full or prefixed BLAKE3 hex id, searched in `object`.
    DigestPrefix,
}

impl QueryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryKind::Path => "path",
            QueryKind::DigestPrefix => "digest-prefix",
        }
    }
}

/// Read a query as a digest prefix when it is at least eight hex characters, and as a path
/// otherwise.
pub fn classify(query: &str) -> QueryKind {
    let hex_like =
        query.len() >= MIN_DIGEST_PREFIX && query.bytes().all(|byte| byte.is_ascii_hexdigit());
    if hex_like {
        QueryKind::DigestPrefix
    } else {
        QueryKind::Path
    }
}

/// What to look up, and in which catalog.
pub struct LocateRequest<'a> {
    /// A namespace path, or a full/prefixed BLAKE3 hex id.
    pub query: &'a str,
    /// The catalog file. It must exist: `locate` never creates one, because a catalog
    /// conjured empty at query time would answer "nothing found" for a tree that is fine.
    pub catalog: &'a Path,
}

#[derive(Debug, Error)]
pub enum LocateError {
    #[error("catalog {path} does not exist; run `just_cache catalog sync` first to build one")]
    MissingCatalog { path: PathBuf },
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

/// What one query found.
#[derive(Debug)]
pub struct LocateReport {
    pub catalog: PathBuf,
    pub query: String,
    pub kind: QueryKind,
    /// Every object that matched. Empty is a real result, not an error.
    pub objects: Vec<ObjectRecord>,
}

impl LocateReport {
    pub fn found(&self) -> bool {
        !self.objects.is_empty()
    }

    pub fn exit_code(&self) -> u8 {
        if self.found() {
            EXIT_FOUND
        } else {
            EXIT_NOT_FOUND
        }
    }

    /// The readable summary. Copy lines name the tier of record and where every copy
    /// lives, because an object with a hot and a cold copy has two true answers and both
    /// are printed.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "locate: {:?} (by {}) in {}",
            self.query,
            self.kind.as_str(),
            self.catalog.display()
        )];
        if self.objects.is_empty() {
            lines.push("  nothing found".to_string());
            return lines;
        }
        lines.push(format!("  {} object(s) matched", self.objects.len()));
        for object in &self.objects {
            lines.push(format!(
                "object {} ({}) {}",
                object.id,
                object.state,
                human_bytes(object.size)
            ));
            for name in &object.names {
                lines.push(format!("  name: {name}"));
            }
            if object.locations.is_empty() {
                lines.push("  copy: none recorded".to_string());
            }
            for location in &object.locations {
                lines.push(format!(
                    "  copy: {}{} (tier {})",
                    location.storage_key,
                    if location.is_primary {
                        " [PRIMARY]"
                    } else {
                        ""
                    },
                    location.tier
                ));
            }
            lines.push(format!("  lifecycle: {}", describe_lifecycle(object)));
            if object.state != crate::catalog::STATE_PRESENT {
                lines.push(format!(
                    "  offline: state is {}, so the bytes need the tier mounted to read \
                     (mounting is not this command's job)",
                    object.state
                ));
            }
        }
        lines
    }

    /// A stable, hand-written document, matching `audit`'s and `explain`'s: the crate
    /// carries no JSON dependency, so the shape is a contract held down by tests.
    pub fn to_json(&self) -> String {
        let mut objects = Vec::new();
        for object in &self.objects {
            let primary = object.primary();
            objects.push(format!(
                "{{\"object\":{},\"size\":{},\"state\":{},\"names\":[{}],\"locations\":[{}],\
                 \"primary\":{},\"last_access\":{},\"last_access_iso\":{},\"accesses\":{},\
                 \"pinned_until\":{},\"rule\":{}}}",
                json_string(&object.id),
                object.size,
                json_string(&object.state),
                object
                    .names
                    .iter()
                    .map(|name| json_string(name))
                    .collect::<Vec<_>>()
                    .join(","),
                object
                    .locations
                    .iter()
                    .map(|location| format!(
                        "{{\"tier\":{},\"storage_key\":{},\"is_primary\":{}}}",
                        json_string(&location.tier),
                        json_string(&location.storage_key),
                        location.is_primary
                    ))
                    .collect::<Vec<_>>()
                    .join(","),
                primary
                    .map(|location| format!(
                        "{{\"tier\":{},\"storage_key\":{}}}",
                        json_string(&location.tier),
                        json_string(&location.storage_key)
                    ))
                    .unwrap_or_else(|| "null".to_string()),
                json_or_null(object.last_access),
                object
                    .last_access
                    .map(format_epoch)
                    .map(|text| json_string(&text))
                    .unwrap_or_else(|| "null".to_string()),
                json_or_null(object.accesses),
                json_or_null(object.pinned_until),
                object
                    .rule
                    .as_ref()
                    .map(|rule| json_string(rule))
                    .unwrap_or_else(|| "null".to_string()),
            ));
        }
        format!(
            "{{\"catalog\":{},\"query\":{},\"kind\":{},\"found\":{},\"objects\":[{}]}}",
            json_string(&self.catalog.to_string_lossy()),
            json_string(&self.query),
            json_string(self.kind.as_str()),
            self.found(),
            objects.join(",")
        )
    }
}

/// Look up one query against one catalog.
pub fn locate(request: &LocateRequest<'_>) -> Result<LocateReport, LocateError> {
    if !request.catalog.is_file() {
        return Err(LocateError::MissingCatalog {
            path: request.catalog.to_path_buf(),
        });
    }
    let catalog = Catalog::open(request.catalog)?;
    let kind = classify(request.query);
    let objects = match kind {
        // The path is the namespace key: names are stored relative to the watch root, so
        // the query is used exactly as given, the same string `catalog sync` ingested.
        QueryKind::Path => catalog
            .record_for_path(request.query)?
            .into_iter()
            .collect(),
        QueryKind::DigestPrefix => catalog.records_with_prefix(request.query)?,
    };
    Ok(LocateReport {
        catalog: request.catalog.to_path_buf(),
        query: request.query.to_string(),
        kind,
        objects,
    })
}

fn describe_lifecycle(object: &ObjectRecord) -> String {
    let last_access = object
        .last_access
        .map(format_epoch)
        .unwrap_or_else(|| "never".to_string());
    let pin = object
        .pinned_until
        .map(|until| format!("pinned until {}", format_epoch(until)))
        .unwrap_or_else(|| "no pin".to_string());
    format!(
        "last access {last_access}, {} access(es), {pin}, rule {}",
        object.accesses.unwrap_or(0),
        object.rule.as_deref().unwrap_or("none")
    )
}

fn json_or_null<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string())
}

/// RFC 3339 in UTC, matching `explain`'s hand-rolled conversion — the tool still has no
/// date dependency, and the arithmetic is small enough to test.
fn format_epoch(seconds: i64) -> String {
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

fn human_bytes(bytes: u64) -> String {
    crate::scope::human_bytes(bytes)
}

/// Minimal JSON string escaping, matching `explain`'s.
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

    #[test]
    fn a_query_is_a_digest_only_when_it_is_long_hex() {
        assert_eq!(classify("shows/moved.mkv"), QueryKind::Path);
        assert_eq!(classify("live.bin"), QueryKind::Path);
        // A plain relative path that is short hex is still a path: below the threshold.
        assert_eq!(classify("ab12"), QueryKind::Path);
        // Eight characters that are all hex is the shortest digest prefix.
        assert_eq!(classify("4217aabb"), QueryKind::DigestPrefix);
        // A full BLAKE3 id is 64 hex characters.
        assert_eq!(classify(&"a".repeat(64)), QueryKind::DigestPrefix);
        // Uppercase is still hex; a path with a non-hex letter is a path.
        assert_eq!(classify("DEADBEEF"), QueryKind::DigestPrefix);
        assert_eq!(classify("deadbeef.txt"), QueryKind::Path);
    }

    #[test]
    fn json_lists_every_copy_and_escapes_text() {
        let report = LocateReport {
            catalog: PathBuf::from("/hot/.just_cache-catalog.sqlite"),
            query: "we\"ird".to_string(),
            kind: QueryKind::DigestPrefix,
            objects: vec![ObjectRecord {
                id: "42".repeat(32),
                size: 11,
                checksum: "42".repeat(32),
                state: "restoring".to_string(),
                last_access: Some(0),
                accesses: Some(3),
                pinned_until: None,
                rule: Some("intelligent-tiering".to_string()),
                names: vec!["shows/moved.mkv".to_string()],
                locations: vec![
                    crate::catalog::LocationRecord {
                        tier: "/cold".to_string(),
                        storage_key: "shows/moved.mkv".to_string(),
                        is_primary: true,
                        verified: true,
                        checksum: Some("42".repeat(32)),
                        object: "42".repeat(32),
                    },
                    crate::catalog::LocationRecord {
                        tier: "/hot".to_string(),
                        storage_key: "shows/moved.mkv".to_string(),
                        is_primary: false,
                        verified: true,
                        checksum: Some("42".repeat(32)),
                        object: "42".repeat(32),
                    },
                ],
            }],
        };
        let json = report.to_json();
        assert!(json.contains("\"kind\":\"digest-prefix\""), "{json}");
        assert!(json.contains("\"found\":true"), "{json}");
        assert!(json.contains("\"state\":\"restoring\""), "{json}");
        assert!(json.contains("\"is_primary\":true"), "{json}");
        assert!(json.contains("\"is_primary\":false"), "{json}");
        assert!(json.contains("\"accesses\":3"), "{json}");
        assert!(json.contains("we\\\"ird"), "quotes must escape: {json}");
        assert_eq!(report.exit_code(), EXIT_FOUND);
    }

    #[test]
    fn an_empty_report_is_not_a_failure() {
        let report = LocateReport {
            catalog: PathBuf::from("c.sqlite"),
            query: "nope.bin".to_string(),
            kind: QueryKind::Path,
            objects: Vec::new(),
        };
        assert!(!report.found());
        assert_eq!(report.exit_code(), EXIT_NOT_FOUND);
        assert!(report
            .summary_lines()
            .iter()
            .any(|line| line.contains("nothing found")));
    }

    #[test]
    fn timestamps_format_as_utc() {
        assert_eq!(format_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_epoch(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}

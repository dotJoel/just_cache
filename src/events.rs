//! The append-only events file: every maintenance pass records the report it produced.
//!
//! The web dashboard (#170–#172) tails this file over SSE, so it is the one channel a
//! viewer watches. The event is **the pass's own report document** — never a second
//! record that could drift from what the CLI printed. The file is JSONL, one complete
//! JSON object per line, and a reader skips a final line that does not parse rather than
//! calling it corrupt: a crash mid-write must leave a torn tail, not a poisoned file.
//! Nothing here reads the file as a move journal or as recovery evidence.
//!
//! The file lives under the `.just_cache` prefix the walk already skips (invariant 8),
//! so a sweep never sees it as a candidate and never moves it.
//!
//! The name is fixed — [`EVENTS_NAME`] — so a dashboard and cron agree on where to look
//! without configuration, and it sits *beside the catalog*: with `--catalog` naming a
//! path the events file is that path's sibling, and with no catalog it falls back to the
//! catalog's default location beside the watch root. Either way it is one well-known
//! file, never scattered into the tree, and when `--catalog` points outside the tree the
//! events file follows it outside (docs/design.md §9 records the decision).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The events file's name. It starts with `.just_cache` so the walk skips it
/// (invariant 8); a dashboard looks for exactly this name.
pub const EVENTS_NAME: &str = ".just_cache-events.jsonl";

/// The single rotated segment a rotation renames the live file to. Keeping one segment
/// bounds the on-disk total at roughly twice the budget and drops the oldest events
/// first; the live file itself is never rewritten, only renamed aside.
pub const EVENTS_SEGMENT_NAME: &str = ".just_cache-events.jsonl.1";

/// The envelope schema version. Bumped only when the envelope's own shape changes; the
/// `report` inside is the command's own document and carries its own contract.
pub const SCHEMA_VERSION: u32 = 1;

/// Default byte budget for the live file before it is rotated. Unbounded growth is not
/// acceptable — a pass on a busy tier runs every hour and would otherwise grow forever.
pub const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Environment override for the byte budget. Inert unless set; tests point it at a tiny
/// value to exercise rotation without writing megabytes (the same seam style as
/// `JUST_CACHE_FAULT`). A set-but-unparseable value panics, so a mistyped test budget
/// cannot silently leave rotation untested.
pub const MAX_BYTES_ENV: &str = "JUST_CACHE_EVENTS_MAX_BYTES";

/// The byte budget for the live events file, from the environment or the default.
pub fn max_bytes() -> u64 {
    match std::env::var(MAX_BYTES_ENV) {
        Ok(value) => value.parse().unwrap_or_else(|_| {
            panic!("{MAX_BYTES_ENV} `{value}`: expected a byte count, e.g. `4096`")
        }),
        Err(_) => DEFAULT_MAX_BYTES,
    }
}

/// Where the events file lives for a pass.
///
/// `catalog` is the path the pass actually resolved — an explicit `--catalog`, or the
/// default beside the watch root. The events file is that path's sibling; when there is
/// no catalog path at all, it falls back to the watch root. `watch` is only consulted
/// for that fallback, which is why a pass with no catalog still writes one known file
/// rather than one per directory it touches.
pub fn events_path(catalog: Option<&Path>, watch: &Path) -> PathBuf {
    match catalog.and_then(Path::parent) {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(EVENTS_NAME),
        _ => watch.join(EVENTS_NAME),
    }
}

/// Append one event to `path`, rotating first if the line would push the live file past
/// `max_bytes`.
///
/// This is the only writer. It is deliberately infallible-by-value: the caller decides
/// whether a failure to record is fatal, and a pass that completed must not be reported
/// as failed because its event could not be written.
pub fn append_to(path: &Path, pass: &str, report_json: &str, max_bytes: u64) -> io::Result<()> {
    let time = crate::schedule::unix_seconds(std::time::SystemTime::now());
    let line = envelope(pass, time, report_json);
    rotate_if_needed(path, line.len() as u64, max_bytes)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// Append an event using the path and budget this process resolves ([`events_path`] and
/// [`max_bytes`]).
pub fn append(
    catalog: Option<&Path>,
    watch: &Path,
    pass: &str,
    report_json: &str,
) -> io::Result<()> {
    append_to(&events_path(catalog, watch), pass, report_json, max_bytes())
}

/// One event line: the routing envelope, then the pass's report document verbatim. The
/// envelope adds only what a reader needs to route an event — version, pass name, time —
/// and never restates a field the report already carries, which is what keeps the two
/// from drifting.
fn envelope(pass: &str, time: u64, report_json: &str) -> String {
    format!(
        "{{\"v\":{SCHEMA_VERSION},\"pass\":{},\"time\":{time},\"report\":{report_json}}}\n",
        json_string(pass)
    )
}

/// Rename the live file aside when the incoming line would exceed the budget.
///
/// A single event larger than the budget is written whole: splitting a line would leave
/// two non-events, and a bounded file is worth less than a complete record. Rotation
/// keeps one segment and drops the oldest, so growth is bounded rather than capped hard.
fn rotate_if_needed(path: &Path, incoming: u64, max_bytes: u64) -> io::Result<()> {
    let len = match fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta.len(),
        // Not there yet, or not a regular file: appending will surface any real problem.
        _ => return Ok(()),
    };
    if len + incoming <= max_bytes {
        return Ok(());
    }
    let segment = path.with_file_name(EVENTS_SEGMENT_NAME);
    // A leftover segment from an earlier rotation is the oldest data; drop it first so
    // the rename below cannot fail on its own target.
    let _ = fs::remove_file(&segment);
    fs::rename(path, &segment)?;
    Ok(())
}

/// Every complete event line in the file, in order.
///
/// A line that is not a complete JSON object — the torn tail a crash mid-write leaves —
/// is skipped rather than reported as corrupt. The writer escapes quotes and backslashes
/// and emits no raw newline inside a string, so "brace-balanced and begins `{`, ends
/// `}`" is an exact test for a whole line, and needs no JSON library to compute.
pub fn read_events(path: &Path) -> io::Result<Vec<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty() && is_complete_object(line))
        .map(str::to_string)
        .collect())
}

/// True when `line` is one complete, brace-balanced JSON object.
///
/// Tracks string contents (and their escapes) so a brace or quote *inside* a value does
/// not confuse the count; the tool's hand-rolled writers escape `"` and `\` exactly, so
/// this is exact for what they emit.
///
/// `pub(crate)` because the SSE tail (#170) holds a torn final line back until it is whole:
/// it must apply the same test to each line as it streams.
pub(crate) fn is_complete_object(line: &str) -> bool {
    let bytes = line.as_bytes();
    if bytes.first() != Some(&b'{') || bytes.last() != Some(&b'}') {
        return false;
    }
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_string
}

/// A JSON string literal, hand-rolled like every other `--json` in this tool. Shared
/// with the report modules so every event document quotes the same way.
pub(crate) fn json_string(text: &str) -> String {
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
    fn events_path_is_beside_the_catalog_and_falls_back_to_the_watch_root() {
        assert_eq!(
            events_path(
                Some(Path::new("/cold/data/catalog.sqlite")),
                Path::new("/hot")
            ),
            Path::new("/cold/data/.just_cache-events.jsonl")
        );
        assert_eq!(
            events_path(None, Path::new("/hot")),
            Path::new("/hot/.just_cache-events.jsonl")
        );
        // A bare catalog name has no directory of its own, so the watch root is used.
        assert_eq!(
            events_path(Some(Path::new("catalog.sqlite")), Path::new("/hot")),
            Path::new("/hot/.just_cache-events.jsonl")
        );
    }

    #[test]
    fn appends_one_complete_parseable_line_per_report() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(EVENTS_NAME);
        append_to(&path, "sweep", "{\"moved\":1}", DEFAULT_MAX_BYTES).unwrap();
        append_to(&path, "gc", "{\"files\":0}", DEFAULT_MAX_BYTES).unwrap();

        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2, "one line per report");
        assert!(events[0].contains("\"pass\":\"sweep\""));
        assert!(events[0].contains("\"report\":{\"moved\":1}"));
        assert!(events[1].contains("\"pass\":\"gc\""));
    }

    #[test]
    fn a_truncated_final_line_is_skipped_not_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(EVENTS_NAME);
        append_to(&path, "sweep", "{\"moved\":1}", DEFAULT_MAX_BYTES).unwrap();
        append_to(&path, "gc", "{\"files\":0}", DEFAULT_MAX_BYTES).unwrap();

        // Simulate a crash mid-write: cut the last line short, mid-object, leaving no
        // closing brace and no trailing newline.
        let text = fs::read_to_string(&path).unwrap();
        let cut = text.len() - 10;
        fs::write(&path, &text[..cut]).unwrap();

        let events = read_events(&path).unwrap();
        assert_eq!(
            events.len(),
            1,
            "the torn tail is skipped, the complete line survives"
        );
        assert!(events[0].contains("\"pass\":\"sweep\""));
    }

    #[test]
    fn rotation_bounds_growth_and_drops_the_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(EVENTS_NAME);
        // Each event is ~50 bytes; a 120-byte budget fits two, then rotates.
        for _ in 0..6 {
            append_to(&path, "sweep", "{\"moved\":1}", 120).unwrap();
        }
        let live = fs::read_to_string(&path).unwrap();
        assert!(
            live.len() <= 120,
            "the live file stays within the budget: {} bytes",
            live.len()
        );
        let segment = tmp.path().join(EVENTS_SEGMENT_NAME);
        assert!(segment.is_file(), "rotation keeps exactly one segment");
        assert!(
            live.len() as u64 + fs::metadata(&segment).unwrap().len() <= 240,
            "the on-disk total is bounded at roughly twice the budget"
        );
        // The dropped events are the oldest, so every surviving line still parses.
        assert!(!read_events(&path).unwrap().is_empty());
    }

    #[test]
    fn braces_and_quotes_inside_strings_do_not_confuse_completeness() {
        assert!(is_complete_object("{\"a\":\"}{ \\\" x\"}"));
        assert!(!is_complete_object("{\"a\":\"}{ \\\" x\""));
        assert!(!is_complete_object("{\"a\":1"));
    }
}

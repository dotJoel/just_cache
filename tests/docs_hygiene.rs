//! Docs and sources are parsed, compiled and exercised — but nothing ever *reads* them for
//! the one mistake a merge leaves behind: a conflict marker committed as ordinary text.
//!
//! Git carries `<<<<<<<` / `=======` / `>>>>>>>` lines as content, so a half-resolved file
//! rebases and merges *cleanly* forever after, and a markdown file is neither compiled nor
//! tested, so no build, clippy or test run can fail on it. Leftover markers reached `main`
//! in two shipped docs (the readme and `docs/design.md`) exactly that way, and were caught
//! by a person reading the rendered README rather than by any check.
//!
//! This is that check. It is a test rather than a workflow step on purpose: pushes that
//! touch `.github/workflows/` need the token's Workflows permission, and a fine-grained PAT
//! here does not have it (see `AGENTS.md`), so a CI-only guard would be stranded on the very
//! branch that added it. `cargo test` already runs in CI, so this fails the build instead.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories that never hold reviewed text: git's object store, and build output.
const SKIPPED: [&str; 2] = [".git", "target"];

/// A file bigger than this is a binary blob or a fixture, not a doc worth scanning, and
/// reading it would only make the test slow.
const MAX_BYTES: u64 = 1024 * 1024;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SKIPPED.contains(&name.as_str()) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect(&path, out);
        } else if kind.is_file() {
            out.push(path);
        }
    }
}

/// The three marker lines, built at runtime so that this file is not its own counter-example:
/// a literal seven-character marker in the source would — correctly — be found by the scan
/// it implements.
fn markers() -> [String; 3] {
    ["<".repeat(7), ">".repeat(7), "=".repeat(7)]
}

/// The 1-based line numbers that hold a conflict marker.
///
/// `<` and `>` markers carry text after them (a ref name), so those match on prefix; `=`
/// stands alone, so it matches the whole line. That exactness matters: a setext heading
/// underline is also a run of `=`, and only the seven-character form is a marker.
fn marker_lines(path: &Path) -> Vec<usize> {
    let Ok(metadata) = fs::metadata(path) else {
        return Vec::new();
    };
    if metadata.len() > MAX_BYTES {
        return Vec::new();
    }
    // A file that is not UTF-8 is a binary fixture; markers are a text problem.
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let [open, close, separator] = markers();
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            line.starts_with(&open) || line.starts_with(&close) || line == &separator
        })
        .map(|(index, _)| index + 1)
        .collect()
}

#[test]
fn no_file_carries_a_conflict_marker() {
    let root = repo_root();
    let mut files = Vec::new();
    collect(&root, &mut files);
    assert!(
        !files.is_empty(),
        "the scan walked {} and found no files at all, which cannot be right",
        root.display()
    );

    let mut found = Vec::new();
    for file in &files {
        let lines = marker_lines(file);
        if lines.is_empty() {
            continue;
        }
        let relative = file.strip_prefix(&root).unwrap_or(file);
        let numbers: Vec<String> = lines.iter().map(usize::to_string).collect();
        found.push(format!(
            "  {}: line {}",
            relative.display(),
            numbers.join(", ")
        ));
    }

    assert!(
        found.is_empty(),
        "conflict markers were committed as text and would render in the shipped docs:\n{}\n\
         Resolve the file properly (keep the side that is right, delete the marker lines) \
         rather than committing them.",
        found.join("\n")
    );
}

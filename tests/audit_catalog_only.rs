//! `just_cache audit --no-filesystem` (issue #51): the catalog-only audit.
//!
//! The point of the mode is that it does *not* touch the filesystem, so the test has to
//! prove the abstention rather than merely that a command exits 0: every catalog here is
//! built and then its watch root and every tier root are deleted, and the audit is asked
//! against those gone paths. A mode that called `validate_paths`, walked the tree, or
//! stat'ed a tier would fail with exit 2 or report the wrong thing; this one must answer
//! from the rows alone.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use just_cache::catalog::Catalog;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Sync a small healthy tree, returning `(watch, cold, catalog_path)` with the catalog
/// kept *outside* the watch root so deleting the tree cannot delete the answer.
fn build_catalog(root: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let watch = root.join("hot");
    let cold = root.join("cold");
    let catalog = root.join("answer.sqlite");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(watch.join("live.bin"), b"still hot").unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    link(
        Path::new("../../cold/shows/moved.mkv"),
        &watch.join("shows/moved.mkv"),
    );

    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .output()
        .expect("just_cache runs");
    assert!(output.status.success(), "sync failed: {}", stderr(&output));
    (watch, cold, catalog)
}

/// The acceptance test: a catalog whose recorded roots are gone still audits, and the
/// answer names its source and the findings it could not make.
#[test]
fn a_catalog_only_audit_answers_with_every_recorded_root_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (watch, cold, catalog) = build_catalog(tmp.path());

    // Remove the filesystem the audit is "for": the tree and the tier. If the mode stat'ed
    // either, there would be nothing to answer from.
    fs::remove_dir_all(&watch).unwrap();
    fs::remove_dir_all(&cold).unwrap();
    assert!(!watch.exists() && !cold.exists());

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .args(["--no-filesystem"])
        .output()
        .expect("just_cache runs");
    assert_eq!(
        output.status.code(),
        Some(0),
        "a catalog-only audit must not fail on missing roots:\n{}\n{}",
        stdout(&output),
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("no filesystem access"), "{text}");
    assert!(text.contains("answer.sqlite"), "{text}");
    assert!(text.contains("recorded state:"), "{text}");
    assert!(
        text.contains("cannot be checked without the tree: missing-copy, checksum-mismatch"),
        "{text}"
    );
    assert!(text.contains("no structural inconsistency found"), "{text}");
}

/// The machine-readable document names the source and carries the recorded state and the
/// explicit unchecked list, so a consumer cannot read a zero as "checked and clean".
#[test]
fn the_json_names_the_source_and_the_unchecked_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let (watch, cold, catalog) = build_catalog(tmp.path());
    fs::remove_dir_all(&watch).unwrap();
    fs::remove_dir_all(&cold).unwrap();

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .args(["--no-filesystem", "--json"])
        .output()
        .expect("just_cache runs");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json = stdout(&output);
    assert!(json.contains("\"source\":\"catalog-only\""), "{json}");
    assert!(json.contains("\"no-filesystem\":{\"objects\":"), "{json}");
    assert!(json.contains("\"unchecked\":["), "{json}");
    for kind in [
        "missing-copy",
        "checksum-mismatch",
        "name-vanished",
        "unknown-path",
        "dangling-symlink",
    ] {
        assert!(
            json.contains(&format!("\"{kind}\"")),
            "the unchecked list must name `{kind}`: {json}"
        );
    }
    // The counts of what it *could* check are still there and zero for a healthy catalog.
    assert!(json.contains("\"under-replicated\":0"), "{json}");
    assert!(json.contains("\"damaged-copy\":0"), "{json}");
}

/// A floor the recorded locations fall below and a damage mark a scrub left are the two
/// findings the mode can make from rows alone — and it makes them with the roots gone.
#[test]
fn floors_and_damage_marks_are_reported_from_rows_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (watch, cold, catalog) = build_catalog(tmp.path());

    // Edit the catalog directly: raise the cold tier's floor above the one recorded copy,
    // and mark a location damaged the way a scrub that found no sibling to repair from
    // would. No filesystem is involved in either.
    let db = Catalog::open(&catalog).unwrap();
    let targets = db.scrub_targets().unwrap();
    db.set_tier_floor(&targets[0].tier, 2).unwrap();
    db.mark_damaged(
        &targets[0].tier,
        &targets[0].storage_key,
        &targets[0].object,
        "test damage",
    )
    .unwrap();
    drop(db);

    fs::remove_dir_all(&watch).unwrap();
    fs::remove_dir_all(&cold).unwrap();

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .args(["--no-filesystem"])
        .output()
        .expect("just_cache runs");
    assert_eq!(
        output.status.code(),
        Some(1),
        "recorded problems must keep the alert up:\n{}",
        stdout(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("under-replicated: 1"), "{text}");
    assert!(text.contains("damaged-copy: 1"), "{text}");
    assert!(
        text.contains("records 1 location(s), the recorded floor is 2"),
        "{text}"
    );
    assert!(text.contains("marked damaged"), "{text}");
}

/// Without a catalog there is nothing to answer from: the mode refuses (exit 2) rather
/// than falling back to the walk it promised not to make.
#[test]
fn no_catalog_is_a_usage_error_not_a_silent_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .args(["--no-filesystem"])
        .output()
        .expect("just_cache runs");
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("needs a catalog"),
        "{}",
        stderr(&output)
    );
}

/// A catalog-only audit is read-only: it never creates a catalog, a journal, or anything
/// else in the tree it was pointed at.
#[test]
fn the_mode_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (watch, cold, catalog) = build_catalog(tmp.path());

    let output = bin()
        .args(["audit", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog)
        .args(["--no-filesystem"])
        .output()
        .expect("just_cache runs");
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));

    let mut names: Vec<String> = fs::read_dir(&watch)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["live.bin", "shows"],
        "the tree is untouched: {names:?}"
    );
}

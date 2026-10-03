//! End-to-end behaviour of `just_cache audit` when a catalog is present (issue #19).
//!
//! These tests run the real binary against real temporary trees: a catalog is built with
//! `catalog sync`, the tree is then edited by hand the way a user would edit it, and the
//! audit's answers, exit codes and — crucially — its *silence* (nothing on disk or in the
//! catalog changes) are asserted. The walk-based audit's own behaviour is covered by
//! `tests/audit.rs`; here the point is that the catalog, not a second walk, is the arbiter.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use just_cache::audit::{self, AuditSource, VerdictKind};
use just_cache::catalog::{Catalog, CATALOG_NAME};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn sync(watch: &Path, dest: &Path) -> Output {
    bin()
        .args(["catalog", "sync", "--watch"])
        .arg(watch)
        .arg("--dest")
        .arg(dest)
        .output()
        .expect("just_cache runs")
}

fn audit(watch: &Path, dest: &Path, extra: &[&str]) -> Output {
    let mut command = bin();
    command
        .args(["audit", "--watch"])
        .arg(watch)
        .arg("--dest")
        .arg(dest);
    for arg in extra {
        command.arg(arg);
    }
    command.output().expect("just_cache runs")
}

fn catalog_at(watch: &Path) -> Catalog {
    Catalog::open(watch.join(CATALOG_NAME)).expect("catalog opens")
}

/// The four numbers in the readable `scrub:` line — locations, verified, never scrubbed,
/// damaged — read from the text a human sees, not recomputed from the report.
fn readable_scrub_counts(text: &str) -> (usize, usize, usize, usize) {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("scrub:"))
        .unwrap_or_else(|| panic!("no `scrub:` line in:\n{text}"));
    let mut numbers = line
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty());
    let mut next = || {
        numbers
            .next()
            .unwrap_or_else(|| panic!("too few numbers in `{line}`"))
            .parse()
            .unwrap()
    };
    (next(), next(), next(), next())
}

/// The integer after `"<field>":` in the hand-written document. Field order and surrounding
/// keys are not asserted here, so the JSON can stay additive.
fn json_number(json: &str, field: &str) -> usize {
    let marker = format!("\"{field}\":");
    let (_, rest) = json
        .split_once(&marker)
        .unwrap_or_else(|| panic!("no {marker} in:\n{json}"));
    rest.chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{marker} is not an integer in:\n{json}"))
}

/// A tree the mover left healthy, then broken by hand. Every catalog-mode verdict the
/// issue asks for should be reported, and the exit code must stay `1`.
#[test]
fn a_tree_broken_after_sync_is_caught_from_the_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();

    // A plain hot file and two migrated ones, so both a primary-hot and primary-cold copy
    // are exercised, and a dangling link can be produced by removing a *catalogued* target.
    fs::write(watch.join("live.bin"), b"still hot").unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    link(
        Path::new("../../cold/shows/moved.mkv"),
        &watch.join("shows/moved.mkv"),
    );
    fs::write(cold.join("gone.bin"), b"only cold").unwrap();
    link(Path::new("../cold/gone.bin"), &watch.join("gone.bin"));

    assert!(
        sync(&watch, &cold).status.success(),
        "a healthy tree syncs clean"
    );

    // Hand-break it, exactly as issue #19 describes ("created around the tool"):
    // 1. the migrated file's primary (cold) copy is corrupted;
    fs::write(cold.join("shows/moved.mkv"), b"CORRUPT").unwrap();
    // 2. a catalogued cold copy is deleted out from under its symlink;
    fs::remove_file(cold.join("gone.bin")).unwrap();
    // 3. a path the catalog has never seen appears.
    fs::write(watch.join("hand_made.bin"), b"made by hand").unwrap();

    let output = audit(&watch, &cold, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let text = stdout(&output);
    for expected in [
        "from catalog",
        "checksum-mismatch: 1",
        "missing-copy: 1",
        "copy-floor: 1",
        "unknown-path: 1",
        "dangling-symlink: 1",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }
}

/// A symlink that resolves to a file the catalog does not hold as a location of the object
/// the name points at is a version the catalog does not have — reported, not adopted.
#[test]
fn a_symlink_to_a_version_the_catalog_lacks_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(cold.join("x.bin"), b"payload").unwrap();
    link(Path::new("../cold/x.bin"), &watch.join("x.bin"));
    assert!(sync(&watch, &cold).status.success());

    // The cold bytes move to a new key and the hot link is re-pointed at them. The
    // catalog still records the old key, so the name resolves to a version it lacks.
    fs::rename(cold.join("x.bin"), cold.join("y.bin")).unwrap();
    fs::remove_file(watch.join("x.bin")).unwrap();
    link(Path::new("../cold/y.bin"), &watch.join("x.bin"));

    let output = audit(&watch, &cold, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let text = stdout(&output);
    assert!(text.contains("unknown-version: 1"), "{text}");
    assert!(text.contains("missing-copy: 1"), "{text}");
}

/// The acceptance test: a fresh sync followed by no changes must audit clean, and the
/// catalog-backed counters must agree with what the walk-based audit computes for the same
/// tree.
#[test]
fn a_fresh_sync_audits_clean_with_the_same_counters_as_the_walk() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("a/b")).unwrap();
    fs::create_dir_all(cold.join("a/b")).unwrap();
    fs::write(watch.join("a/live.bin"), b"hot payload").unwrap();
    fs::write(cold.join("a/b/clip.mov"), b"cold payload").unwrap();
    link(
        Path::new("../../../cold/a/b/clip.mov"),
        &watch.join("a/b/clip.mov"),
    );

    assert!(sync(&watch, &cold).status.success());

    // The binary the user runs exits zero and says the catalog answered.
    let output = audit(&watch, &cold, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let text = stdout(&output);
    assert!(text.contains("from catalog"), "{text}");
    assert!(text.contains("no structural inconsistency found"), "{text}");

    // And the two sources of truth agree on the counters for this tree.
    let catalog = catalog_at(&watch);
    let from_catalog = audit::catalog_audit(&catalog, &watch, std::slice::from_ref(&cold)).unwrap();
    let from_walk = audit::audit(&watch, std::slice::from_ref(&cold)).unwrap();

    assert!(!from_catalog.has_findings(), "{:?}", from_catalog.findings);
    assert!(!from_walk.has_findings(), "{:?}", from_walk.findings);
    assert_eq!(from_catalog.scanned, from_walk.scanned);
    assert_eq!(from_catalog.healthy, from_walk.healthy);
    assert_eq!(from_catalog.scanned, 2);
    assert_eq!(from_catalog.healthy, 2);
}

/// A catalog/file disagreement is reported, never fixed — not on a read-only audit, and not
/// even under `--repair`, which in catalog mode marks the finding for resync instead of
/// rewriting a row or touching the tree.
#[test]
fn a_hand_broken_tree_is_reported_and_never_silently_fixed() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("c.bin"), b"chrome").unwrap();
    assert!(sync(&watch, &cold).status.success());

    let before = catalog_at(&watch).object_for_path("c.bin").unwrap();
    // The hand move: no tool, no journal, just a rename.
    fs::rename(watch.join("c.bin"), watch.join("d.bin")).unwrap();

    let output = audit(&watch, &cold, &[]);
    assert_eq!(output.status.code(), Some(1));
    let text = stdout(&output);
    assert!(text.contains("name-vanished: 1"), "{text}");
    assert!(text.contains("unknown-path: 1"), "{text}");

    // The catalog still says the old name pointed at the old object, and knows nothing of
    // the new name: the audit did not adopt it.
    let catalog = catalog_at(&watch);
    assert_eq!(catalog.object_for_path("c.bin").unwrap(), before);
    assert_eq!(catalog.object_for_path("d.bin").unwrap(), None);

    // `--repair` in catalog mode changes nothing — neither the tree nor the rows.
    let repaired = audit(&watch, &cold, &["--repair"]);
    assert_eq!(
        repaired.status.code(),
        Some(1),
        "an unresolved disagreement keeps the alert up"
    );
    let repaired_text = stdout(&repaired);
    assert!(
        repaired_text.contains("marked")
            && repaired_text.contains("nothing on disk or in the catalog was changed"),
        "{repaired_text}"
    );
    assert!(
        fs::read(watch.join("d.bin")).is_ok(),
        "the renamed file must survive"
    );
    assert!(
        fs::symlink_metadata(watch.join("d.bin")).unwrap().is_file(),
        "catalog repair must not turn the file into a symlink"
    );
    let after = catalog_at(&watch);
    assert_eq!(after.object_for_path("c.bin").unwrap(), before);
    assert_eq!(after.object_for_path("d.bin").unwrap(), None);
}

/// Without a catalog the walk-based audit is still the fallback, and the audit must not
/// create the catalog as a side effect (invariant 9: only `catalog sync` creates it).
#[test]
fn audit_without_a_catalog_walks_both_sides_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("live.bin"), b"hot").unwrap();

    assert!(
        !watch.join(CATALOG_NAME).exists(),
        "precondition: no catalog"
    );
    let output = audit(&watch, &cold, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let text = stdout(&output);
    assert!(text.contains("walked; no catalog"), "{text}");
    assert!(
        !watch.join(CATALOG_NAME).exists(),
        "an audit must never create the catalog"
    );
}

/// `--catalog <FILE>` names the source of truth explicitly, so a catalog may live anywhere
/// — and the audit reports which file answered.
#[test]
fn an_explicit_catalog_flag_points_at_the_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog_path = tmp.path().join("elsewhere/catalog.sqlite");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
    fs::write(watch.join("a.bin"), b"payload").unwrap();

    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&cold)
        .arg("--catalog")
        .arg(&catalog_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let read_back = audit(
        &watch,
        &cold,
        &["--catalog", catalog_path.to_str().unwrap()],
    );
    assert_eq!(read_back.status.code(), Some(0), "{}", stdout(&read_back));
    let text = stdout(&read_back);
    assert!(text.contains("from catalog"), "{text}");
    assert!(text.contains("catalog.sqlite"), "{text}");
}

/// A catalog-backed finding carries the catalog path in the report, so `--json` consumers
/// can tell which source answered without guessing.
#[test]
fn the_report_names_its_source() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("a.bin"), b"payload").unwrap();
    assert!(sync(&watch, &cold).status.success());
    fs::write(watch.join("hand.bin"), b"new").unwrap();

    let output = audit(&watch, &cold, &["--json"]);
    assert_eq!(output.status.code(), Some(1));
    let json = stdout(&output);
    assert!(json.contains("\"source\":\"catalog\""), "{json}");
    assert!(json.contains("\"kind\":\"unknown-path\""), "{json}");
    assert!(json.contains("\"unknown-path\":1"), "{json}");

    let catalog = catalog_at(&watch);
    let report = audit::catalog_audit(&catalog, &watch, std::slice::from_ref(&cold)).unwrap();
    assert_eq!(
        report.source,
        AuditSource::Catalog {
            path: watch.join(CATALOG_NAME)
        }
    );
    assert_eq!(report.count(VerdictKind::UnknownPath), 1);
}

/// The acceptance test for #55: `audit --catalog --json` carries the scrub section, and its
/// counts are exactly the ones the readable output prints for the same catalog — before a
/// scrub (nothing verified) and after one (both copies verified).
#[test]
fn json_carries_the_scrub_counts_the_readable_output_prints() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    // A hot-only file and a migrated one: three recorded locations in total is not needed —
    // two is enough to show the counts move off zero independently of the structural audit.
    fs::write(watch.join("live.bin"), b"hot payload").unwrap();
    fs::write(cold.join("moved.bin"), b"cold payload").unwrap();
    link(Path::new("../cold/moved.bin"), &watch.join("moved.bin"));
    assert!(sync(&watch, &cold).status.success());

    let catalog_path = watch.join(CATALOG_NAME);
    let catalog_arg = catalog_path.to_str().unwrap();

    // Nothing has been scrubbed yet, so the section must say so rather than implying the
    // catalog vouches for unread bytes.
    let readable = audit(&watch, &cold, &["--catalog", catalog_arg]);
    assert_eq!(readable.status.code(), Some(0), "{}", stdout(&readable));
    let readable_text = stdout(&readable);
    let (locations, verified, never, damaged) = readable_scrub_counts(&readable_text);
    assert_eq!(verified, 0, "{readable_text}");
    assert_eq!(
        never, locations,
        "every recorded copy is unverified before a scrub:\n{readable_text}"
    );
    assert_eq!(damaged, 0, "{readable_text}");

    let machine = audit(&watch, &cold, &["--catalog", catalog_arg, "--json"]);
    assert_eq!(machine.status.code(), Some(0), "{}", stdout(&machine));
    let json = stdout(&machine);
    assert_eq!(json_number(&json, "locations"), locations, "{json}");
    assert_eq!(json_number(&json, "verified"), verified, "{json}");
    assert_eq!(json_number(&json, "never-scrubbed"), never, "{json}");
    assert_eq!(json_number(&json, "damaged"), damaged, "{json}");

    // A scrub then advances the counts; the two renderings must still agree, so the
    // agreement test cannot pass on a hard-coded zero in both.
    let scrubbed = bin()
        .arg("scrub")
        .arg("--catalog")
        .arg(&catalog_path)
        .output()
        .unwrap();
    assert!(
        scrubbed.status.success(),
        "{}",
        String::from_utf8_lossy(&scrubbed.stderr)
    );

    let readable_after = stdout(&audit(&watch, &cold, &["--catalog", catalog_arg]));
    let (locations_after, verified_after, never_after, damaged_after) =
        readable_scrub_counts(&readable_after);
    let json_after = stdout(&audit(&watch, &cold, &["--catalog", catalog_arg, "--json"]));
    assert_eq!(
        json_number(&json_after, "locations"),
        locations_after,
        "{json_after}"
    );
    assert_eq!(
        json_number(&json_after, "verified"),
        verified_after,
        "{json_after}"
    );
    assert_eq!(
        json_number(&json_after, "never-scrubbed"),
        never_after,
        "{json_after}"
    );
    assert_eq!(
        json_number(&json_after, "damaged"),
        damaged_after,
        "{json_after}"
    );
    assert!(
        verified_after > 0 && never_after == 0,
        "the scrub must show up in the counts:\n{readable_after}"
    );
}

/// Without a catalog no stored bytes were read back, so the JSON carries `null` — not a
/// zeroed object that a consumer would read as "everything verified".
#[test]
fn json_scrub_is_null_for_a_walk_based_audit() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(watch.join("live.bin"), b"hot").unwrap();

    assert!(
        !watch.join(CATALOG_NAME).exists(),
        "precondition: no catalog"
    );
    let output = audit(&watch, &cold, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    let json = stdout(&output);
    assert!(json.contains("\"source\":\"walk\""), "{json}");
    assert!(json.contains("\"scrub\":null"), "{json}");
}

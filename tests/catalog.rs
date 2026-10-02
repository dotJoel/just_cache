//! End-to-end behaviour of `just_cache catalog sync` against real temporary trees, plus
//! CLI checks that pin down the exit-code contract cron depends on.
//!
//! The catalog is the thing every later P1 command trusts, so these tests exercise the
//! three states the issue calls out (a tree with migrations that predate the catalog, a
//! hand move, and a file moved clean out of the tool) and then read the real rows back to
//! prove the catalog was *not* rewritten to match. The binary is run wherever a user
//! would see the result — exit code and output lines — rather than only the library.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use just_cache::catalog::{Catalog, CATALOG_NAME};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit; stdout:\n{}\nstderr:\n{}",
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn catalog_at(watch: &Path) -> Catalog {
    Catalog::open(watch.join(CATALOG_NAME)).expect("catalog opens")
}

/// A tree the mover has already been running against, ingested by the first sync. The
/// symlink migration predates the catalog entirely, which is the migration story: ingest,
/// not migrate. The second, identical sync must be a no-op rather than a duplicate row.
#[test]
fn sync_ingests_migrations_that_predate_the_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();

    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    link(
        Path::new("../../cold/shows/moved.mkv"),
        &hot.join("shows/moved.mkv"),
    );
    fs::write(hot.join("live.bin"), b"still hot").unwrap();

    let first = sync(&hot, &cold);
    assert_exit(&first, 0);
    assert!(
        stdout(&first).contains("no differences"),
        "a healthy pre-existing migration is not a difference:\n{}",
        stdout(&first)
    );

    // The default catalog path is beside the watch root, and the walk must skip it.
    let catalog_path = hot.join(CATALOG_NAME);
    assert!(
        catalog_path.is_file(),
        "catalog created beside the watch root"
    );

    let catalog = catalog_at(&hot);
    assert_eq!(
        catalog.state_for_path("live.bin").unwrap().as_deref(),
        Some("present")
    );
    assert_eq!(
        catalog
            .state_for_path("shows/moved.mkv")
            .unwrap()
            .as_deref(),
        Some("offloaded")
    );
    // The checksum is the identity, and the identity is what both names carry.
    let live_object = catalog.object_for_path("live.bin").unwrap().unwrap();
    assert_eq!(
        live_object.len(),
        64,
        "a BLAKE3 hash in hex is 64 characters"
    );

    let locations = catalog.all_locations().unwrap();
    let cold_location = locations
        .iter()
        .find(|location| location.storage_key == "shows/moved.mkv")
        .expect("the cold copy is a location");
    assert_eq!(cold_location.tier, canonical(&cold));
    assert!(
        cold_location.is_primary,
        "offloaded bytes are the tier of record"
    );

    // Idempotent: a second sync ingests nothing new and stays quiet.
    let second = sync(&hot, &cold);
    assert_exit(&second, 0);
    assert!(
        stdout(&second).contains("(0 new)"),
        "re-ingesting the same tree adds nothing:\n{}",
        stdout(&second)
    );
    assert_eq!(catalog.object_count().unwrap(), 2);
}

/// A file renamed by hand inside the tree. The catalog must report it and keep saying
/// where the old name pointed — a silent rewrite would destroy the record's authority.
#[test]
fn sync_reports_a_hand_move_instead_of_rewriting_the_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("c.bin"), b"chrome").unwrap();

    assert_exit(&sync(&hot, &cold), 0);
    let before = catalog_at(&hot).object_for_path("c.bin").unwrap().unwrap();

    // The hand move: no tool, no journal, just a rename.
    fs::rename(hot.join("c.bin"), hot.join("d.bin")).unwrap();

    let output = sync(&hot, &cold);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(text.contains("name-vanished: c.bin"), "{text}");
    assert!(text.contains("location-missing: c.bin"), "{text}");
    assert!(
        text.contains("NOT rewritten to match"),
        "the report must say the catalog was left alone:\n{text}"
    );

    // The old facts are all still there, and the new name was ingested alongside them.
    let catalog = catalog_at(&hot);
    assert_eq!(
        catalog.object_for_path("c.bin").unwrap(),
        Some(before.clone()),
        "the vanished name still points at the object the catalog recorded"
    );
    assert_eq!(
        catalog.object_for_path("d.bin").unwrap(),
        Some(before),
        "the hand-created name is ingested, deduped onto the same content"
    );
    assert_eq!(
        catalog.state_for_path("c.bin").unwrap().as_deref(),
        Some("present")
    );
}

/// A file moved clean out of the tree — the tool never saw it go. The location row must
/// survive: the catalog is the source of truth precisely so a question about a file that
/// is not there has an answer.
#[test]
fn sync_reports_a_file_moved_outside_the_tool_and_keeps_its_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("x.bin"), b"payload").unwrap();
    assert_exit(&sync(&hot, &cold), 0);

    fs::rename(hot.join("x.bin"), tmp.path().join("elsewhere.bin")).unwrap();

    let output = sync(&hot, &cold);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(text.contains("name-vanished: x.bin"), "{text}");
    assert!(text.contains("location-missing: x.bin"), "{text}");

    let catalog = catalog_at(&hot);
    assert!(catalog.object_for_path("x.bin").unwrap().is_some());
    assert!(
        catalog
            .all_locations()
            .unwrap()
            .iter()
            .any(|location| location.storage_key == "x.bin"),
        "the location is kept, not silently dropped"
    );
}

/// The lifecycle the issue asks for: present (hot) -> offloaded (mover ran) -> restoring
/// (a copy came back while the cold copy is still there).
#[test]
fn state_moves_present_to_offloaded_to_restoring() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), b"payload\n").unwrap();

    assert_exit(&sync(&hot, &cold), 0);
    assert_eq!(
        catalog_at(&hot).state_for_path("a.bin").unwrap().as_deref(),
        Some("present")
    );

    // Let the real mover do the offload, so the transition is against its actual output.
    let sweep = bin()
        .arg("--watch")
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .args(["--min-idle-days", "0", "--once"])
        .output()
        .expect("sweep runs");
    assert!(
        sweep.status.success(),
        "sweep failed:\n{}",
        String::from_utf8_lossy(&sweep.stderr)
    );
    let moved = fs::symlink_metadata(hot.join("a.bin")).unwrap();
    assert!(moved.is_symlink(), "the mover left the symlink");
    // Hermes invariant 8/9: the catalog inside the tree is never a move candidate.
    assert!(
        !fs::symlink_metadata(hot.join(CATALOG_NAME))
            .unwrap()
            .is_symlink(),
        "the catalog must not be moved onto a cold tier"
    );

    let after_offload = sync(&hot, &cold);
    assert_exit(&after_offload, 0);
    assert!(
        stdout(&after_offload).contains("no differences"),
        "a mover offload is the expected transition, not a difference:\n{}",
        stdout(&after_offload)
    );
    let catalog = catalog_at(&hot);
    assert_eq!(
        catalog.state_for_path("a.bin").unwrap().as_deref(),
        Some("offloaded")
    );
    assert_eq!(
        catalog.all_locations().unwrap().len(),
        1,
        "the stale hot location is gone and only the cold one remains"
    );

    // A restore in progress: the bytes are back in the tree while the cold copy remains.
    fs::remove_file(hot.join("a.bin")).unwrap();
    fs::write(hot.join("a.bin"), fs::read(cold.join("a.bin")).unwrap()).unwrap();

    let after_restore = sync(&hot, &cold);
    assert_exit(&after_restore, 0);
    assert_eq!(
        catalog_at(&hot).state_for_path("a.bin").unwrap().as_deref(),
        Some("restoring"),
        "both copies present is a restore that has not finished"
    );
}

/// A cold copy removed by hand under a live symlink: reported, and the location row kept.
/// The name still references the object, so this module must not throw the location away.
#[test]
fn a_missing_cold_copy_is_reported_and_the_location_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    link(
        Path::new("../../cold/shows/moved.mkv"),
        &hot.join("shows/moved.mkv"),
    );

    assert_exit(&sync(&hot, &cold), 0);
    fs::remove_file(cold.join("shows/moved.mkv")).unwrap();

    let output = sync(&hot, &cold);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(text.contains("dangling-symlink"), "{text}");
    assert!(text.contains("location-missing: shows/moved.mkv"), "{text}");

    let catalog = catalog_at(&hot);
    assert!(
        catalog
            .all_locations()
            .unwrap()
            .iter()
            .any(|location| location.storage_key == "shows/moved.mkv"),
        "the location row survives its file: the catalog is where the answer lives"
    );
    assert_eq!(catalog.location_count().unwrap(), 1);
}

/// `--catalog` names the file explicitly, and only the watch root and the `--dest` roots
/// ever become location tiers — a volatile cache is never data of record (§2.1).
#[test]
fn a_catalog_can_live_at_an_explicit_path_and_records_only_real_tiers() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    let catalog_path = tmp.path().join("elsewhere/catalog.sqlite");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
    fs::write(hot.join("a.bin"), b"payload").unwrap();

    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&hot)
        .args(["--dest"])
        .arg(&cold)
        .args(["--catalog"])
        .arg(&catalog_path)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert!(catalog_path.is_file());

    let catalog = Catalog::open(&catalog_path).unwrap();
    let allowed = [canonical(&hot), canonical(&cold)];
    for location in catalog.all_locations().unwrap() {
        assert!(
            allowed.contains(&location.tier),
            "location tier {} is neither the watch root nor a --dest root",
            location.tier
        );
    }
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

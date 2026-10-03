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

/// A symlink planted at the catalog's own name must be refused, and the link's target —
/// any file the tool's user can write — left byte-for-byte untouched. Before the guard,
/// SQLite opened straight through the link and initialized the target as a database,
/// which grew a 0-byte victim to ~90 KiB.
#[test]
fn a_symlink_at_the_catalog_path_is_refused_and_the_target_is_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), b"payload").unwrap();

    // An empty file is the sharpest victim: SQLite treats it as a new database and
    // writes a schema into it. A refusal must leave it at zero bytes.
    let victim = tmp.path().join("victim.sqlite");
    fs::write(&victim, b"").unwrap();
    link(&victim, &hot.join(CATALOG_NAME));

    let output = sync(&hot, &cold);
    assert_ne!(
        output.status.code(),
        Some(0),
        "a symlinked catalog must be refused, not opened:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("symbolic link"),
        "the refusal must name the reason:\n{stderr}"
    );
    assert_eq!(
        fs::read(&victim).unwrap(),
        b"",
        "the symlink target must be untouched"
    );
    // The link itself still exists — the tool refused it rather than deleting it.
    assert!(fs::symlink_metadata(hot.join(CATALOG_NAME))
        .unwrap()
        .is_symlink());
}

/// The same guard for SQLite's predictable sibling names: a rollback journal is created
/// before the first page of a transaction lands, so a link at `<catalog>-journal` is a
/// name an attacker who can watch the tree gets to plant first.
#[test]
fn a_symlink_at_a_sqlite_sibling_name_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), b"payload").unwrap();

    let victim = tmp.path().join("journal-victim");
    fs::write(&victim, b"JOURNAL VICTIM").unwrap();
    let journal = hot.join(format!("{CATALOG_NAME}-journal"));
    link(&victim, &journal);

    let output = sync(&hot, &cold);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(
        stderr.contains("symbolic link"),
        "the refusal must name the reason:\n{stderr}"
    );
    assert_eq!(fs::read(&victim).unwrap(), b"JOURNAL VICTIM");
    assert!(
        fs::symlink_metadata(&journal).unwrap().is_symlink(),
        "the planted sibling is left where it was found"
    );
    // And the refusal must not have created a catalog at all.
    assert!(
        fs::symlink_metadata(hot.join(CATALOG_NAME)).is_err(),
        "a refused open must not leave a catalog behind"
    );
}

/// A catalog created by the tool is private to its user: 0600, not the process umask's
/// 0644. The catalog discloses the watched tree's whole inventory, its sizes and digests.
#[test]
fn a_freshly_created_catalog_is_private_to_its_user() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), b"payload").unwrap();

    assert_exit(&sync(&hot, &cold), 0);
    let metadata = fs::symlink_metadata(hot.join(CATALOG_NAME)).unwrap();
    assert_eq!(
        metadata.mode() & 0o777,
        0o600,
        "a newly created catalog must be mode 0600"
    );
}

/// One unreadable file among readable ones: the pass reports it and ingests everything
/// else. Before #53 the first failed read aborted the whole sync, so a single locked file
/// left the entire catalog un-ingested — the mover's rule 7 has no business stopping at
/// the catalog's door.
///
/// The unreadable file is a mode-000 regular file. The walk stats it fine (stat needs no
/// read permission), so it is the read-back hash that fails, which is exactly the window
/// this issue is about. A process with `CAP_DAC_OVERRIDE` (root) cannot be blocked this
/// way; CI runs unprivileged, and if that ever changes this test fails loudly on the
/// exit-code assertion rather than passing quietly.
#[test]
fn sync_continues_past_an_unreadable_file_and_ingests_the_rest() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("readable.bin"), b"read me").unwrap();
    fs::write(hot.join("locked.bin"), b"cannot read me").unwrap();
    fs::set_permissions(hot.join("locked.bin"), fs::Permissions::from_mode(0o000)).unwrap();

    let output = sync(&hot, &cold);
    // Non-zero: a catalog with a hole in it is a finding cron must see (rule 7 applied to
    // the catalog), even though the rest of the tree was ingested.
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("unreadable: locked.bin"),
        "the report must name the unreadable file:\n{text}"
    );
    assert!(
        text.contains("Permission denied") || text.contains("os error 13"),
        "the report must carry the error:\n{text}"
    );

    let catalog = catalog_at(&hot);
    // The readable file went in despite the failure — the whole point of the change.
    assert!(
        catalog.object_for_path("readable.bin").unwrap().is_some(),
        "a sibling failure must not cost the readable file its ingest"
    );
    assert_eq!(catalog.name_count().unwrap(), 1);
    // Nothing was inferred about the file that could not be read: no name, and it is not
    // reported as vanished or missing either.
    assert!(catalog.object_for_path("locked.bin").unwrap().is_none());
    assert!(!text.contains("name-vanished: locked.bin"), "{text}");
    assert!(!text.contains("location-missing: locked.bin"), "{text}");

    // Make it readable: the next sync ingests it and the catalog is clean again.
    fs::set_permissions(hot.join("locked.bin"), fs::Permissions::from_mode(0o644)).unwrap();
    let healed = sync(&hot, &cold);
    assert_exit(&healed, 0);
    assert!(
        stdout(&healed).contains("no differences"),
        "the hole is filled:\n{}",
        stdout(&healed)
    );
    let catalog = catalog_at(&hot);
    assert!(catalog.object_for_path("locked.bin").unwrap().is_some());
    assert_eq!(catalog.name_count().unwrap(), 2);
}

/// A file the catalog already knows, made unreadable: it must be reported as unreadable
/// and *not* re-reported as a vanished name or a missing location. A read failure is not
/// evidence of a deletion, and inventing one would corrupt the catalog's account of the
/// tree (issue #53). The rows themselves are kept either way.
#[test]
fn an_unreadable_file_is_not_recorded_as_missing() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("readable.bin"), b"read me").unwrap();
    fs::write(hot.join("locked.bin"), b"cannot read me").unwrap();

    assert_exit(&sync(&hot, &cold), 0);
    assert_eq!(catalog_at(&hot).name_count().unwrap(), 2);

    fs::set_permissions(hot.join("locked.bin"), fs::Permissions::from_mode(0o000)).unwrap();
    let output = sync(&hot, &cold);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(text.contains("unreadable: locked.bin"), "{text}");
    assert!(
        !text.contains("name-vanished"),
        "an unreadable file is not a vanished one:\n{text}"
    );
    assert!(
        !text.contains("location-missing"),
        "an unreadable file is not a missing location:\n{text}"
    );

    let catalog = catalog_at(&hot);
    assert!(
        catalog.object_for_path("locked.bin").unwrap().is_some(),
        "the row survives a failed read"
    );
    assert_eq!(catalog.name_count().unwrap(), 2);

    fs::set_permissions(hot.join("locked.bin"), fs::Permissions::from_mode(0o644)).unwrap();
    let healed = sync(&hot, &cold);
    assert_exit(&healed, 0);
    assert!(
        stdout(&healed).contains("no differences"),
        "{}",
        stdout(&healed)
    );
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

//! End-to-end behaviour of `just_cache catalog resolve` (issue #52): the human command that
//! concludes a rename, a delete or a replacement where the tree's evidence supports it, and
//! refuses to conclude anything else.
//!
//! The contract these tests pin down is a *pair* of runs: a `sync` that reports a difference
//! and leaves the rows alone, then a `resolve` that settled it, then a `sync` that has
//! nothing left to say. If the resolution logic is removed, the second `sync` keeps reporting
//! the same difference and every assertion below fails — that is the point of writing them
//! end-to-end rather than against a helper.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use just_cache::catalog::{Catalog, CATALOG_NAME};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected exit; stdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn sync(watch: &Path, dests: &[&Path]) -> Output {
    let mut command = bin();
    command.args(["catalog", "sync", "--watch"]).arg(watch);
    for dest in dests {
        command.arg("--dest").arg(dest);
    }
    command.output().expect("just_cache runs")
}

fn resolve(watch: &Path, dests: &[&Path], apply: bool) -> Output {
    let mut command = bin();
    command.args(["catalog", "resolve", "--watch"]).arg(watch);
    for dest in dests {
        command.arg("--dest").arg(dest);
    }
    if apply {
        command.arg("--apply");
    }
    command.output().expect("just_cache runs")
}

fn catalog_at(watch: &Path) -> Catalog {
    Catalog::open(watch.join(CATALOG_NAME)).expect("catalog opens")
}

/// A file renamed by hand inside the tree. `sync` reports `name-vanished` and
/// `location-missing` and keeps the old rows; `resolve --apply` concludes the rename because
/// the object is still named — and still hashes to the recorded digest — at the new name;
/// the next `sync` is quiet and the rows describe the tree.
#[test]
fn resolve_concludes_a_hand_rename_and_sync_goes_quiet() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("shows/box.mkv"), b"movie bytes").unwrap();

    assert_exit(&sync(&hot, &[&cold]), 0);
    let before = catalog_at(&hot)
        .object_for_path("shows/box.mkv")
        .unwrap()
        .unwrap();

    fs::rename(hot.join("shows/box.mkv"), hot.join("shows/moved.mkv")).unwrap();

    // The sync that reports the difference — the report `resolve` exists to consume.
    let reported = sync(&hot, &[&cold]);
    assert_exit(&reported, 1);
    let text = stdout(&reported);
    assert!(text.contains("name-vanished: shows/box.mkv"), "{text}");
    assert!(text.contains("location-missing: shows/box.mkv"), "{text}");
    assert_eq!(
        catalog_at(&hot).object_for_path("shows/box.mkv").unwrap(),
        Some(before.clone()),
        "the old row is still there after the reporting sync"
    );

    let resolved = resolve(&hot, &[&cold], true);
    assert_exit(&resolved, 0);
    let text = stdout(&resolved);
    assert!(
        text.contains("resolved name-vanished: shows/box.mkv"),
        "{text}"
    );
    assert!(
        text.contains("still named at shows/moved.mkv"),
        "the evidence must name the surviving spelling:\n{text}"
    );

    let catalog = catalog_at(&hot);
    assert_eq!(
        catalog.object_for_path("shows/box.mkv").unwrap(),
        None,
        "the stale name is gone"
    );
    assert_eq!(
        catalog.object_for_path("shows/moved.mkv").unwrap(),
        Some(before),
        "the surviving name still maps to the same object"
    );
    assert!(
        !catalog
            .all_locations()
            .unwrap()
            .iter()
            .any(|location| location.storage_key == "shows/box.mkv"),
        "the vanished name's hot location was dropped with it"
    );

    let quiet = sync(&hot, &[&cold]);
    assert_exit(&quiet, 0);
    assert!(
        stdout(&quiet).contains("no differences"),
        "the difference must not repeat after a resolution:\n{}",
        stdout(&quiet)
    );
}

/// A cold replica deleted by hand. The bytes are gone from that tier but the object survives
/// at its other location, so the stale row can be dropped — and after it is, `sync` no longer
/// reports `location-missing`.
#[test]
fn resolve_drops_a_deleted_replica_and_sync_goes_quiet() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("movie.bin"), b"movie bytes").unwrap();
    fs::write(cold.join("movie.bin"), b"movie bytes").unwrap();

    assert_exit(&sync(&hot, &[&cold]), 0);
    assert_eq!(catalog_at(&hot).location_count().unwrap(), 2);

    fs::remove_file(cold.join("movie.bin")).unwrap();

    let reported = sync(&hot, &[&cold]);
    assert_exit(&reported, 1);
    assert!(
        stdout(&reported).contains("location-missing: movie.bin"),
        "{}",
        stdout(&reported)
    );

    let resolved = resolve(&hot, &[&cold], true);
    assert_exit(&resolved, 0);
    let text = stdout(&resolved);
    assert!(
        text.contains("resolved location-missing: movie.bin"),
        "{text}"
    );
    assert!(text.contains("still named at movie.bin"), "{text}");

    let catalog = catalog_at(&hot);
    assert_eq!(
        catalog.location_count().unwrap(),
        1,
        "the stale location row is dropped, the surviving one kept"
    );
    assert_eq!(
        catalog.all_locations().unwrap()[0].storage_key,
        "movie.bin",
        "the surviving location is the hot copy"
    );

    let quiet = sync(&hot, &[&cold]);
    assert_exit(&quiet, 0);
    assert!(
        stdout(&quiet).contains("no differences"),
        "{}",
        stdout(&quiet)
    );
}

/// A replacement at a path whose object survives *nowhere* is refused, with the reason, and
/// the rows are left exactly as they were — repointing would drop the only record of the
/// bytes the catalog recorded.
#[test]
fn resolve_refuses_a_replacement_that_would_lose_the_object() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("only.bin"), b"original bytes").unwrap();

    assert_exit(&sync(&hot, &[&cold]), 0);
    let before = catalog_at(&hot)
        .object_for_path("only.bin")
        .unwrap()
        .unwrap();

    fs::write(hot.join("only.bin"), b"different bytes").unwrap();

    assert_exit(&sync(&hot, &[&cold]), 1);

    let resolved = resolve(&hot, &[&cold], true);
    assert_exit(&resolved, 1);
    let text = stdout(&resolved);
    assert!(
        text.contains("not resolved name-replaced: only.bin"),
        "{text}"
    );
    assert!(
        text.contains("survives nowhere"),
        "the refusal must name its reason:\n{text}"
    );

    assert_eq!(
        catalog_at(&hot).object_for_path("only.bin").unwrap(),
        Some(before),
        "a refused replacement leaves the recorded object alone"
    );

    let still = sync(&hot, &[&cold]);
    assert_exit(&still, 1);
    assert!(
        stdout(&still).contains("name-replaced"),
        "nothing changed, so the difference repeats:\n{}",
        stdout(&still)
    );
}

/// A replacement whose old object still survives under another name *is* concluded: the name
/// is repointed to the bytes now at the path, and the old object keeps its other name.
#[test]
fn resolve_repoints_a_replacement_when_the_old_object_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    // Two names for the same bytes: deleting or replacing one leaves the object reachable.
    fs::write(hot.join("a.bin"), b"shared bytes").unwrap();
    fs::write(hot.join("b.bin"), b"shared bytes").unwrap();

    assert_exit(&sync(&hot, &[&cold]), 0);
    let old = catalog_at(&hot).object_for_path("a.bin").unwrap().unwrap();

    fs::write(hot.join("a.bin"), b"now something else").unwrap();
    let new = just_cache::digest::file_digest(&hot.join("a.bin"))
        .unwrap()
        .to_hex()
        .to_string();
    assert_ne!(old, new);

    let reported = sync(&hot, &[&cold]);
    assert_exit(&reported, 1);
    assert!(
        stdout(&reported).contains("name-replaced: a.bin"),
        "{}",
        stdout(&reported)
    );

    let resolved = resolve(&hot, &[&cold], true);
    assert_exit(&resolved, 0);
    let text = stdout(&resolved);
    assert!(text.contains("resolved name-replaced: a.bin"), "{text}");
    assert!(text.contains("still named at b.bin"), "{text}");

    let catalog = catalog_at(&hot);
    assert_eq!(
        catalog.object_for_path("a.bin").unwrap(),
        Some(new),
        "the replaced name now points at the bytes at that path"
    );
    assert_eq!(
        catalog.object_for_path("b.bin").unwrap(),
        Some(old),
        "the old object keeps its surviving name"
    );

    let quiet = sync(&hot, &[&cold]);
    assert_exit(&quiet, 0);
    assert!(
        stdout(&quiet).contains("no differences"),
        "{}",
        stdout(&quiet)
    );
}

/// An object deleted with nothing left anywhere is reported and left alone: the catalog must
/// keep the record of bytes that no surviving copy holds, and nothing is deleted to tidy the
/// report (invariant 6).
#[test]
fn an_irreconcilable_delete_is_reported_and_the_rows_are_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("gone.bin"), b"only copy").unwrap();

    assert_exit(&sync(&hot, &[&cold]), 0);
    let before = catalog_at(&hot)
        .object_for_path("gone.bin")
        .unwrap()
        .unwrap();

    fs::remove_file(hot.join("gone.bin")).unwrap();
    assert_exit(&sync(&hot, &[&cold]), 1);

    let resolved = resolve(&hot, &[&cold], true);
    assert_exit(&resolved, 1);
    let text = stdout(&resolved);
    assert!(
        text.contains("not resolved name-vanished: gone.bin"),
        "{text}"
    );
    assert!(text.contains("no surviving name"), "{text}");

    assert_eq!(
        catalog_at(&hot).object_for_path("gone.bin").unwrap(),
        Some(before),
        "the only record of the object is not deleted to tidy the report"
    );
}

/// The default is report-only: `resolve` without `--apply` names what it would do and writes
/// nothing, so a cron job can run it to see the plan.
#[test]
fn resolve_without_apply_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("c.bin"), b"chrome").unwrap();
    assert_exit(&sync(&hot, &[&cold]), 0);
    fs::rename(hot.join("c.bin"), hot.join("d.bin")).unwrap();
    assert_exit(&sync(&hot, &[&cold]), 1);

    let dry = resolve(&hot, &[&cold], false);
    assert_exit(&dry, 1);
    let text = stdout(&dry);
    assert!(text.contains("report only; nothing was written"), "{text}");
    assert!(
        text.contains("would resolve name-vanished: c.bin"),
        "{text}"
    );

    assert!(
        catalog_at(&hot).object_for_path("c.bin").unwrap().is_some(),
        "a report-only pass must leave the stale row exactly where it was"
    );
}

/// A tree the catalog already agrees with has nothing to resolve, and says so rather than
/// exiting non-zero on an empty pass.
#[test]
fn a_clean_tree_has_nothing_to_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("fine.bin"), b"payload").unwrap();
    assert_exit(&sync(&hot, &[&cold]), 0);

    let resolved = resolve(&hot, &[&cold], true);
    assert_exit(&resolved, 0);
    assert!(
        stdout(&resolved).contains("nothing to resolve"),
        "{}",
        stdout(&resolved)
    );
}

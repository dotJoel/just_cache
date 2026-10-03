//! A catalog row is data, and the reader that joins it to a root must not let an editor of
//! that catalog point `scrub`, `reconcile` or `audit` at a file outside every destination
//! root (issue #72). These tests edit the real SQLite catalog by hand — the way a swapped
//! or restored-from-backup catalog would arrive — and assert that the escape is *reported*
//! and that nothing outside the roots is read, created or overwritten.
//!
//! They fail if the validation is reverted: without it, `scrub` overwrites the sentinel
//! file, `reconcile` creates a file outside every root, and `audit` reports neither row as
//! malformed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::CATALOG_NAME;
use rusqlite::Connection;
use tempfile::TempDir;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
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

/// A payload that is non-zero and long enough that a size difference is real.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251 + 1) as u8).collect()
}

fn sync(watch: &Path, dests: &[&Path], copies: Option<usize>) -> Output {
    let mut command = bin();
    command.args(["catalog", "sync", "--watch"]).arg(watch);
    for dest in dests {
        command.arg("--dest").arg(dest);
    }
    if let Some(copies) = copies {
        command.args(["--copies", &copies.to_string()]);
    }
    command.output().expect("just_cache runs")
}

fn scrub(catalog: &Path) -> Output {
    bin()
        .args(["scrub", "--catalog"])
        .arg(catalog)
        .output()
        .expect("just_cache runs")
}

fn reconcile(catalog: &Path) -> Output {
    bin()
        .args(["reconcile", "--catalog"])
        .arg(catalog)
        .output()
        .expect("just_cache runs")
}

fn audit(watch: &Path, dest: &Path) -> Output {
    bin()
        .args(["audit", "--watch"])
        .arg(watch)
        .arg("--dest")
        .arg(dest)
        .output()
        .expect("just_cache runs")
}

/// A symlink, as the mover would have left behind a migrated file.
fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

/// Open the catalog with a second connection so the test can edit rows by hand.
fn edit(catalog: &Path, sql: &str) {
    let conn = Connection::open(catalog).expect("catalog opens");
    conn.execute(sql, []).expect("hand edit applies");
}

/// Every distinct tier the catalog recorded, as the strings the rows actually hold.
fn tiers(catalog: &Path) -> Vec<String> {
    let conn = Connection::open(catalog).unwrap();
    let mut stmt = conn.prepare("SELECT DISTINCT tier FROM location").unwrap();
    let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

fn canonical(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// A sentinel outside every root, so any code that reaches it is unambiguously escaping.
fn sentinel(tmp: &TempDir) -> PathBuf {
    let path = tmp.path().join("sentinel.bin");
    fs::write(&path, b"SENTINEL: this file must never be touched").unwrap();
    path
}

/// `scrub` must refuse a `storage_key` that walks out of the tier with `..`, not overwrite
/// the file it resolves to.
#[test]
fn scrub_refuses_a_storage_key_that_escapes_the_tier() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    let bytes = payload(64 * 1024);
    fs::write(hot.join("a.bin"), &bytes).unwrap();
    fs::write(cold.join("a.bin"), &bytes).unwrap();
    assert_exit(&sync(&hot, &[&cold], None), 0);

    let catalog = hot.join(CATALOG_NAME);
    let cold_tier = tiers(&catalog)
        .into_iter()
        .find(|tier| *tier != canonical(&hot))
        .expect("a cold tier was recorded");
    // `cold/../sentinel.bin` is exactly `tmp/sentinel.bin`, one level above the cold root.
    edit(
        &catalog,
        &format!(
            "UPDATE location SET storage_key = '../sentinel.bin'
              WHERE tier = '{cold_tier}' AND storage_key = 'a.bin'"
        ),
    );

    let marker = sentinel(&tmp);
    let before = fs::read(&marker).unwrap();

    let output = scrub(&catalog);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("malformed catalog row"),
        "the refused row must be reported; got:\n{text}"
    );
    assert_eq!(
        fs::read(&marker).unwrap(),
        before,
        "the file outside every root must not be overwritten"
    );
}

/// `audit` must refuse a row whose tier is not a current root and one whose key is
/// absolute, reporting both as malformed rather than hashing what they point at.
#[test]
fn audit_refuses_a_tier_that_is_not_a_root_and_an_absolute_key() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::write(hot.join("a.bin"), b"payload").unwrap();
    // A cold location must exist for the catalog to record the cold tier string; a symlink
    // names it so a plain sync stays clean (an unnamed cold copy is an orphaned-copy
    // difference, which is a finding, not a zero exit).
    fs::write(cold.join("b.bin"), b"cold payload").unwrap();
    link(Path::new("../cold/b.bin"), &hot.join("b.bin"));
    assert_exit(&sync(&hot, &[&cold], None), 0);

    let catalog = hot.join(CATALOG_NAME);
    let cold_tier = tiers(&catalog)
        .into_iter()
        .find(|tier| *tier != canonical(&hot))
        .expect("a cold tier was recorded");
    let marker = sentinel(&tmp);

    let conn = Connection::open(&catalog).unwrap();
    let object_id: Vec<u8> = conn
        .query_row(
            "SELECT object_id FROM location WHERE storage_key = 'a.bin'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // 1. A tier that is not one of the roots the invocation was given.
    conn.execute(
        "INSERT INTO location (object_id, tier, storage_key, is_primary, updated_at, verified, checksum)
         VALUES (?1, ?2, 'sentinel.bin', 0, 0, 0, NULL)",
        rusqlite::params![object_id, tmp.path().to_string_lossy()],
    )
    .unwrap();
    // 2. An absolute key, which `Path::join` would use in place of the tier entirely.
    conn.execute(
        "INSERT INTO location (object_id, tier, storage_key, is_primary, updated_at, verified, checksum)
         VALUES (?1, ?2, ?3, 0, 0, 0, NULL)",
        rusqlite::params![object_id, cold_tier, marker.to_string_lossy()],
    )
    .unwrap();
    drop(conn);

    let output = audit(&hot, &cold);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("malformed-catalog: 2"),
        "both refused rows must be reported; got:\n{text}"
    );
    assert!(
        !text.contains("checksum-mismatch: /"),
        "a refused row must not be hashed and reported as a mismatch; got:\n{text}"
    );
}

/// `reconcile` must refuse a row whose key would create a file outside every root, rather
/// than creating the directory and the copy.
#[test]
fn reconcile_refuses_a_row_that_would_create_a_file_outside_the_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let hot = tmp.path().join("hot");
    let cold_a = tmp.path().join("cold-a");
    let cold_b = tmp.path().join("cold-b");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(&cold_a).unwrap();
    fs::create_dir_all(&cold_b).unwrap();
    let bytes = payload(128 * 1024);
    fs::write(hot.join("shows/ep1.mkv"), &bytes).unwrap();

    // Record the floor, offload with replication, then sync so the catalog reflects the
    // moved state — the operator's normal sequence.
    assert_exit(&sync(&hot, &[&cold_a, &cold_b], Some(2)), 0);
    let offload = bin()
        .args(["--watch"])
        .arg(&hot)
        .args(["--dest"])
        .arg(&cold_a)
        .args(["--dest"])
        .arg(&cold_b)
        .args([
            "--copies",
            "2",
            "--min-idle-days",
            "0",
            "--min-observed-accesses",
            "0",
            "--min-free-gb",
            "0",
            "--once",
        ])
        .output()
        .expect("just_cache runs");
    assert_exit(&offload, 0);
    let catalog = hot.join(CATALOG_NAME);
    assert_exit(&sync(&hot, &[&cold_a, &cold_b], Some(2)), 0);

    // The copy on cold-b is lost, then the row is edited to point outside every root.
    fs::remove_file(cold_b.join("shows/ep1.mkv")).unwrap();
    let cold_b_tier = canonical(&cold_b);
    edit(
        &catalog,
        &format!(
            "UPDATE location SET storage_key = '../escaped/created.bin'
              WHERE tier = '{cold_b_tier}' AND storage_key = 'shows/ep1.mkv'"
        ),
    );

    let escaped = tmp.path().join("escaped/created.bin");
    assert!(
        !escaped.exists(),
        "precondition: nothing outside a root yet"
    );

    let output = reconcile(&catalog);
    assert_exit(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("malformed catalog row"),
        "the refused row must be reported; got:\n{text}"
    );
    assert!(
        !escaped.exists(),
        "reconcile must not create a file outside every root"
    );
    assert!(
        !escaped.parent().unwrap().exists(),
        "reconcile must not even create the directory outside every root"
    );
}

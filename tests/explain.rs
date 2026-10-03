//! End-to-end behaviour of `just_cache explain` against a real temporary tree, and the
//! exit-code contract a script depends on.
//!
//! These drive the binary, not the library: the exit codes and the stdout a shell sees are
//! the contract, and a library test that passes while the CLI returns the wrong code is
//! exactly the kind of hollow coverage this repo has been bitten by before.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn explain(args: &[&str]) -> Output {
    bin()
        .arg("explain")
        .args(args)
        // A real tmpfs/tmp dir has plenty of room; zero keeps this independent of how
        // much free space the test machine happens to have.
        .arg("--min-free-gb")
        .arg("0")
        .output()
        .expect("the binary should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the process should exit, not be signalled")
}

/// Set both atime and mtime to `when`, which is how a test makes a file look cold (or
/// warm) without waiting.
fn set_times(path: &Path, when: SystemTime) {
    let times = fs::FileTimes::new().set_accessed(when).set_modified(when);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(times)
        .unwrap();
}

fn days_ago(days: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(days * 86_400)
}

struct Tree {
    _tmp: tempfile::TempDir,
    watch: std::path::PathBuf,
    cold: std::path::PathBuf,
}

fn tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();
    Tree {
        _tmp: tmp,
        watch,
        cold,
    }
}

#[test]
fn a_cold_in_scope_file_exits_zero() {
    let tree = tree();
    let path = tree.watch.join("cold.bin");
    fs::write(&path, b"payload").unwrap();
    set_times(&path, days_ago(90));

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
        "--min-idle-days",
        "30",
    ]);

    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("would move now"),
        "must say it would move: {text}"
    );
    assert!(text.contains("scope:   managed"), "{text}");
    assert!(text.contains("guards:  clear"), "{text}");
    // Read-only: the file is still a real file, not touched.
    assert!(fs::symlink_metadata(&path).unwrap().is_file());
}

#[test]
fn a_warm_in_scope_file_exits_one_not_an_error() {
    let tree = tree();
    let path = tree.watch.join("warm.bin");
    fs::write(&path, b"payload").unwrap();
    set_times(&path, days_ago(5));

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
        "--min-idle-days",
        "30",
    ]);

    assert_eq!(
        code(&output),
        1,
        "in scope but too warm is exit 1, not an error: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("would move in"), "{text}");
    assert!(text.contains("scope:   managed"), "{text}");
}

/// The ordering guarantee through the binary: a file that is both out of scope *and* too
/// warm reports the scope rule, and the later stages say they were never reached.
#[test]
fn scope_is_reported_ahead_of_a_warm_stamp() {
    let tree = tree();
    fs::create_dir_all(tree.watch.join("node_modules")).unwrap();
    let path = tree.watch.join("node_modules/left-pad.js");
    fs::write(&path, b"module").unwrap();
    set_times(&path, SystemTime::now());

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
        "--exclude",
        "node_modules",
        "--min-idle-days",
        "30",
    ]);

    assert_eq!(code(&output), 1);
    let text = stdout(&output);
    assert!(
        text.contains("excluded by --exclude 'node_modules'"),
        "the exclusion is the answer and must be named: {text}"
    );
    assert!(
        text.contains("guards:  not evaluated: scope refused first"),
        "{text}"
    );
    assert!(
        text.contains("policy:  not evaluated: scope refused first"),
        "{text}"
    );
    assert!(
        !text.contains("would move in"),
        "the scope reason, not the warm stamp, is the outer answer: {text}"
    );
}

/// An `--exclude` that can never match a watch-relative path protects nothing; issue #79
/// requires it to be reported rather than silently ignored.
#[test]
fn an_exclude_that_can_never_match_is_reported() {
    let tree = tree();
    let path = tree.watch.join("cold.bin");
    fs::write(&path, b"payload").unwrap();
    set_times(&path, days_ago(90));

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
        "--exclude",
        "/var/log/scratch",
        "--min-idle-days",
        "30",
    ]);

    let err = stderr(&output);
    assert!(
        err.contains("warning:") && err.contains("/var/log/scratch"),
        "a pattern that can never match must be reported, not ignored: {err}"
    );
}

#[test]
fn a_size_floor_is_named_with_both_numbers() {
    let tree = tree();
    let path = tree.watch.join("small.bin");
    fs::write(&path, b"12345").unwrap();
    set_times(&path, days_ago(90));

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
        "--min-size",
        "1MiB",
    ]);

    assert_eq!(code(&output), 1);
    let text = stdout(&output);
    assert!(
        text.contains("below the 1.0 MiB size floor (5 B)"),
        "both the bound and the size must appear: {text}"
    );
}

#[test]
fn a_missing_path_exits_one_and_says_so() {
    let tree = tree();
    let path = tree.watch.join("never-existed.bin");

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
    ]);

    assert_eq!(code(&output), 1);
    let text = stdout(&output);
    assert!(text.contains("not found"), "{text}");
    assert!(!text.contains("would move now"), "{text}");
}

#[test]
fn a_migrated_path_answers_with_the_cold_location_and_tier() {
    let tree = tree();
    fs::create_dir_all(tree.cold.join("shows")).unwrap();
    fs::write(tree.cold.join("shows/moved.mkv"), b"payload").unwrap();
    let link = tree.watch.join("shows/moved.mkv");
    fs::create_dir_all(tree.watch.join("shows")).unwrap();
    std::os::unix::fs::symlink(Path::new("../../cold/shows/moved.mkv"), &link).unwrap();

    let output = explain(&[
        link.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
    ]);

    assert_eq!(
        code(&output),
        0,
        "a migrated file is managed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("already migrated"), "{text}");
    assert!(
        text.contains("moved.mkv"),
        "the cold location must be named: {text}"
    );
    assert!(text.contains("tier 0"), "the tier must be named: {text}");
}

#[test]
fn json_carries_the_same_answer() {
    let tree = tree();
    let path = tree.watch.join("cold.bin");
    fs::write(&path, b"payload").unwrap();
    set_times(&path, days_ago(90));

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
        "--json",
    ]);

    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    assert!(text.contains("\"managed\":true"), "{text}");
    assert!(text.contains("\"exit_code\":0"), "{text}");
    assert!(text.contains("\"decision\":\"in\""), "{text}");
    assert!(text.contains("\"outcome\":\"would-move-now\""), "{text}");
    assert!(text.contains("\"access_source\":\"atime\""), "{text}");
    assert!(text.contains("\"catalog\":{"), "{text}");
    assert!(text.contains("\"disagreements\":[]"), "{text}");
}

/// The real `/proc` path, end to end: a child holds the file open, and the report names
/// its pid.
#[test]
fn a_file_held_open_by_another_process_is_reported_with_its_pid() {
    let tree = tree();
    let path = tree.watch.join("held.bin");
    fs::write(&path, b"payload").unwrap();
    set_times(&path, days_ago(90));

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 3< '{}'; sleep 30", path.display()))
        .spawn()
        .expect("spawn a holder");
    let child_pid = child.id();

    // Give the child a moment to reach the open(), then explain.
    let mut text = String::new();
    let mut seen = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        let output = explain(&[
            path.to_str().unwrap(),
            "--watch",
            tree.watch.to_str().unwrap(),
            "--dest",
            tree.cold.to_str().unwrap(),
        ]);
        text = stdout(&output);
        if text.contains(&format!("open by pid {child_pid}")) {
            seen = true;
            assert_eq!(code(&output), 1, "a held file must not be moved");
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        seen,
        "the holder's pid must be named in the report; last output:\n{text}"
    );
}

#[test]
fn a_bad_invocation_exits_two() {
    // A missing --watch is a clap error, which exits 2.
    let output = bin()
        .args(["explain", "/tmp/x", "--dest", "/tmp"])
        .output()
        .unwrap();
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));

    // A --dest that does not exist (an unmounted disk) is a bad invocation too, not a
    // "would not move" answer.
    let tree = tree();
    let path = tree.watch.join("a.bin");
    fs::write(&path, b"payload").unwrap();
    let missing = tree.watch.join("not-mounted");
    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
}

/// Build a catalog beside the watched tree, ingesting whatever the tree holds.
fn sync_catalog(tree: &Tree) {
    let output = bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&tree.watch)
        .arg("--dest")
        .arg(&tree.cold)
        .output()
        .expect("catalog sync runs");
    assert!(output.status.success(), "sync failed: {}", stderr(&output));
}

/// With no catalog, the answer is unchanged and the report says why: the filesystem is the
/// source of truth, exactly as before the catalog existed.
#[test]
fn explain_says_when_no_catalog_is_configured() {
    let tree = tree();
    let path = tree.watch.join("cold.bin");
    fs::write(&path, b"payload").unwrap();
    set_times(&path, days_ago(90));

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
    ]);

    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("catalog: no catalog is configured"),
        "the absent catalog must be stated, not implied: {text}"
    );
}

/// A catalog that exists is consulted, and when the tree was edited by hand afterwards the
/// two sources disagree. The disagreement is reported — never resolved silently, because a
/// silent resolution is how a UI ends up trusting a number nothing on disk agrees with.
#[test]
fn explain_consults_a_catalog_and_reports_a_disagreement() {
    let tree = tree();
    fs::create_dir_all(tree.cold.join("shows")).unwrap();
    fs::write(tree.cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    fs::create_dir_all(tree.watch.join("shows")).unwrap();
    let link = tree.watch.join("shows/moved.mkv");
    std::os::unix::fs::symlink(Path::new("../../cold/shows/moved.mkv"), &link).unwrap();

    // The catalog is built against the migrated tree: the object is offloaded, its cold
    // copy is the tier of record.
    sync_catalog(&tree);
    assert!(tree.watch.join(just_cache::catalog::CATALOG_NAME).is_file());

    // Hand-modify the tree after the sync: the path is a regular file now, while the
    // catalog still places the object on a cold tier.
    fs::remove_file(&link).unwrap();
    fs::write(&link, b"movie bytes").unwrap();

    let output = explain(&[
        link.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
    ]);

    let text = stdout(&output);
    assert!(
        text.contains("catalog: consulted"),
        "the catalog must be consulted when it exists: {text}"
    );
    assert!(
        text.contains("WARNING: catalog and filesystem disagree"),
        "the disagreement must be reported, not resolved: {text}"
    );
    assert!(
        text.contains("places this object on a tier"),
        "the disagreement must name what disagrees: {text}"
    );
}

/// The same consultation through the injected public entry point: a disagreement is a
/// report, and the verdict still comes from the filesystem the mover acts on.
#[test]
fn explain_json_reports_the_consulted_catalog() {
    let tree = tree();
    let path = tree.watch.join("live.bin");
    fs::write(&path, b"payload").unwrap();
    sync_catalog(&tree);
    // Stamp the idle times *after* the sync. `catalog sync` reads every file to hash it, and
    // on a `relatime` mount that read bumps atime — so a stamp set before the sync is not the
    // stamp `explain` sees. This host is `noatime`, where the ordering is invisible, which is
    // exactly why the test passed here and failed on CI.
    set_times(&path, days_ago(90));

    let output = explain(&[
        path.to_str().unwrap(),
        "--watch",
        tree.watch.to_str().unwrap(),
        "--dest",
        tree.cold.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"consulted\":true"), "{text}");
    assert!(text.contains("\"source\":\"catalog\""), "{text}");
    assert!(text.contains("\"accesses\":"), "{text}");
}

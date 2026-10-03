//! A `--dest` root nested under `--watch` (or a `--watch` nested under a `--dest`) is
//! refused, in both directions, for the sweep and for `catalog sync`.
//!
//! The old check only refused `dest == watch`, so `--watch /data --dest /data/cold` passed:
//! the sweep would copy a file into `/data/cold`, then, on its next pass, discover the copy
//! it had just made (the destination is inside the tree it scans) and tier that deeper — the
//! tool moving its own output, once per pass. The reverse nesting is the same hazard from the
//! other side. These tests drive the real binary because the contract is what a shell sees:
//! a refusal naming the direction, a nonzero exit (sweep reports validation failures as a
//! plain failure, `catalog sync` as a usage error), and nothing moved.
//!
//! The check compares canonical paths component-wise, not as strings: a destination that
//! merely shares a name prefix with the watch (`hot` vs `hot-cold`) is fine, and a symlink
//! that resolves inside the watch is caught even though its literal path is a sibling.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A watch tree and a directory next to it, plus a real cold tier already created *inside*
/// the watch — the shape that used to pass validation. The `TempDir` is returned so it
/// lives until the end of the test and cleans up on drop.
fn layout(tag: &str) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let watch = root.join(format!("{tag}-hot"));
    let sibling = root.join(format!("{tag}-cold"));
    let nested = watch.join("cold");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    (tmp, watch, sibling, nested)
}

fn sweep(watch: &Path, dest: &Path) -> Output {
    bin()
        .arg("--watch")
        .arg(watch)
        .arg("--dest")
        .arg(dest)
        .arg("--min-free-gb")
        .arg("0")
        .arg("--once")
        .output()
        .expect("just_cache runs")
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

/// Sweep reports every validation failure as a plain failure (exit 1); `catalog sync` treats
/// a bad root pairing as a usage error (exit 2). Pinning the exact code keeps a panic (101)
/// from passing as a refusal, and keeps the two subcommands' contracts from drifting apart.
fn assert_refused(output: &Output, expected_code: i32, watch: &Path, dest: &Path, expected: &str) {
    let err = stderr(output);
    assert_eq!(
        output.status.code(),
        Some(expected_code),
        "a bad --watch/--dest pairing must be refused;\nwatch {}\ndest {}\nstderr:\n{err}",
        watch.display(),
        dest.display()
    );
    assert!(
        err.contains(expected),
        "the refusal for dest {} should mention {expected:?}:\n{err}",
        dest.display()
    );
}

#[test]
fn sweep_refuses_a_destination_inside_the_watched_tree() {
    let (_tmp, watch, _sibling, nested) = layout("sweep_in");
    let output = sweep(&watch, &nested);
    assert_refused(&output, 1, &watch, &nested, "inside the watched tree");
    assert!(
        !nested.join("cold").exists(),
        "the nested destination must not be written to"
    );
}

#[test]
fn sweep_refuses_a_watched_tree_inside_a_destination() {
    let (_tmp, watch, _sibling, nested) = layout("sweep_out");
    // The watch is a subdirectory of the destination root.
    let output = sweep(&nested, &watch);
    assert_refused(&output, 1, &nested, &watch, "inside the --dest root");
}

#[test]
fn catalog_sync_refuses_a_destination_inside_the_watched_tree() {
    let (_tmp, watch, _sibling, nested) = layout("sync_in");
    let output = sync(&watch, &nested);
    assert_refused(&output, 2, &watch, &nested, "inside the watched tree");
}

#[test]
fn catalog_sync_refuses_a_watched_tree_inside_a_destination() {
    let (_tmp, watch, _sibling, nested) = layout("sync_out");
    let output = sync(&nested, &watch);
    assert_refused(&output, 2, &nested, &watch, "inside the --dest root");
}

/// Equality is still refused with its own message — nested-by-component includes it, but the
/// operator-facing wording for the exact-same-root case should stay specific.
#[test]
fn a_destination_equal_to_the_watch_is_still_refused() {
    let (_tmp, watch, _sibling, _nested) = layout("equal");
    let output = sync(&watch, &watch);
    assert_refused(&output, 2, &watch, &watch, "is the watched directory");
}

/// The guard must compare whole components: a sibling whose *name* starts with the watch's
/// name is not inside it, and a normal sync must still succeed.
#[test]
fn a_sibling_sharing_a_name_prefix_is_allowed() {
    let (_tmp, watch, sibling, _nested) = layout("hot");
    let output = sync(&watch, &sibling);
    assert!(
        output.status.success(),
        "a sibling destination must be accepted; stderr:\n{}",
        stderr(&output)
    );
}

/// A symlink that lives beside the watch but resolves inside it is still a destination
/// inside the watch: the check has to compare canonical paths, not the names on the
/// command line.
#[test]
fn a_symlinked_destination_resolving_inside_the_watch_is_refused() {
    let (_tmp, watch, _sibling, nested) = layout("link");
    let link = watch.parent().unwrap().join("link");
    std::os::unix::fs::symlink(&nested, &link).unwrap();
    let output = sync(&watch, &link);
    assert_refused(&output, 2, &watch, &link, "inside the watched tree");
}

//! End-to-end behaviour of `just_cache locate` against a real catalog and tree, plus the
//! exit-code contract a script depends on (`0` found, `1` nothing found, `2` usage).
//!
//! The binary is driven, not the library: the contract is what a shell sees, and a library
//! test that passes while the CLI returns the wrong code is hollow coverage.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use just_cache::catalog::{Catalog, CATALOG_NAME};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn symlink(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

fn sync(watch: &Path, dest: &Path) -> Output {
    bin()
        .args(["catalog", "sync", "--watch"])
        .arg(watch)
        .arg("--dest")
        .arg(dest)
        .output()
        .expect("catalog sync runs")
}

fn locate(catalog: &Path, query: &str) -> Output {
    bin()
        .args(["locate", query, "--catalog"])
        .arg(catalog)
        .output()
        .expect("locate runs")
}

fn locate_with_json(catalog: &Path, query: &str) -> Output {
    bin()
        .args(["locate", query, "--catalog"])
        .arg(catalog)
        .arg("--json")
        .output()
        .expect("locate runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the process should exit")
}

struct Tree {
    _tmp: tempfile::TempDir,
    watch: std::path::PathBuf,
}

/// A tree with one hot file, one migrated object under a symlink, and a fresh catalog that
/// knows about both.
fn synced_tree() -> Tree {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(watch.join("shows")).unwrap();
    fs::create_dir_all(cold.join("shows")).unwrap();
    fs::write(watch.join("live.bin"), b"still hot").unwrap();
    fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
    symlink(
        Path::new("../../cold/shows/moved.mkv"),
        &watch.join("shows/moved.mkv"),
    );

    let output = sync(&watch, &cold);
    assert_eq!(code(&output), 0, "sync failed: {}", stderr(&output));
    Tree { _tmp: tmp, watch }
}

fn catalog_path(tree: &Tree) -> std::path::PathBuf {
    tree.watch.join(CATALOG_NAME)
}

/// A path query answers with every copy and its tier, which copy is the tier of record,
/// and the object's state — for a hot object and for a migrated one alike.
#[test]
fn locate_by_path_reports_state_copies_and_primary() {
    let tree = synced_tree();
    let catalog = catalog_path(&tree);

    // The hot object: present, with a single primary copy.
    let hot = locate(&catalog, "live.bin");
    assert_eq!(code(&hot), 0, "stderr: {}", stderr(&hot));
    let text = stdout(&hot);
    assert!(text.contains("(by path)"), "{text}");
    assert!(text.contains("live.bin"), "{text}");
    assert!(text.contains("(present)"), "{text}");
    assert!(
        text.contains("[PRIMARY]"),
        "the tier of record must be marked: {text}"
    );
    assert!(text.contains("no pin"), "{text}");

    // The offloaded object: its cold copy is the primary, and the command says the bytes
    // need the tier if it is offline.
    let cold = locate(&catalog, "shows/moved.mkv");
    assert_eq!(code(&cold), 0, "stderr: {}", stderr(&cold));
    let text = stdout(&cold);
    assert!(text.contains("(offloaded)"), "{text}");
    assert!(text.contains("[PRIMARY]"), "{text}");
    assert!(
        text.contains("offline: state is offloaded"),
        "an offloaded copy must be named as needing its tier: {text}"
    );
}

/// A digest prefix finds the same object by content, and the full id is shown so the
/// caller can widen or narrow the query.
#[test]
fn locate_by_digest_prefix_finds_the_object() {
    let tree = synced_tree();
    let catalog = catalog_path(&tree);
    let full = Catalog::open(&catalog)
        .unwrap()
        .object_for_path("live.bin")
        .unwrap()
        .expect("the synced name has an object");
    assert_eq!(full.len(), 64, "a BLAKE3 id in hex is 64 characters");
    let prefix = &full[..8];

    let output = locate(&catalog, prefix);
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("(by digest-prefix)"), "{text}");
    assert!(text.contains(&full), "the full id must be printed: {text}");
    assert!(
        text.contains("live.bin"),
        "the object's name must be shown: {text}"
    );

    // The full id is a prefix of itself and must be just as findable.
    let full_query = locate_with_json(&catalog, &full);
    assert_eq!(code(&full_query), 0, "stderr: {}", stderr(&full_query));
    assert!(
        stdout(&full_query).contains("\"kind\":\"digest-prefix\""),
        "{}",
        stdout(&full_query)
    );
    assert!(
        stdout(&full_query).contains("\"state\":\"present\""),
        "{}",
        stdout(&full_query)
    );
}

/// JSON is the machine-readable contract a UI or test calls; it carries the same answer.
#[test]
fn locate_json_carries_every_copy_and_the_kind() {
    let tree = synced_tree();
    let catalog = catalog_path(&tree);
    let output = locate_with_json(&catalog, "shows/moved.mkv");
    assert_eq!(code(&output), 0);
    let text = stdout(&output);
    assert!(text.contains("\"found\":true"), "{text}");
    assert!(text.contains("\"kind\":\"path\""), "{text}");
    assert!(text.contains("\"state\":\"offloaded\""), "{text}");
    assert!(text.contains("\"is_primary\":true"), "{text}");
    assert!(text.contains("\"accesses\":"), "{text}");
}

/// Nothing matched is exit 1 — an answer, not an error.
#[test]
fn a_miss_is_exit_one_and_says_nothing_found() {
    let tree = synced_tree();
    let catalog = catalog_path(&tree);
    let output = locate(&catalog, "never-existed.bin");
    assert_eq!(code(&output), 1, "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("nothing found"),
        "{}",
        stdout(&output)
    );

    let json = locate_with_json(&catalog, "never-existed.bin");
    assert_eq!(code(&json), 1);
    assert!(
        stdout(&json).contains("\"found\":false"),
        "{}",
        stdout(&json)
    );
}

/// A catalog that does not exist is a bad invocation (exit 2), never a silent "nothing
/// found": a script must be able to tell a broken catalog from an empty one.
#[test]
fn a_missing_catalog_is_a_usage_error() {
    let tree = synced_tree();
    let missing = tree._tmp.path().join("nope.sqlite");
    let output = locate(&missing, "live.bin");
    assert_eq!(code(&output), 2, "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("does not exist"),
        "{}",
        stderr(&output)
    );

    // A missing required --catalog is a clap error too (exit 2).
    let clap_error = bin().args(["locate", "live.bin"]).output().unwrap();
    assert_eq!(code(&clap_error), 2, "stderr: {}", stderr(&clap_error));
}

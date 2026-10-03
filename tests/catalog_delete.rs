//! Deleting a name through the catalog (issue #128), end to end through the binary.
//!
//! Each test builds a real tree, ingests it with `catalog sync`, deletes with
//! `catalog delete`, and then asks the tools a user would ask — `locate`, `audit`, a
//! second `sync` — whether the result is what a delete promises: the name gone, copies
//! released only with the last name, nothing resurrected, nothing orphaned counted good.
//! The crash window between the commit and the unlinks is opened with
//! `JUST_CACHE_FAULT=delete-after-commit=1`, which aborts the process exactly there.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::{Catalog, CATALOG_NAME};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

fn text(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `hot/a.bin` and `hot/b.bin` hold the same bytes (one object, two names, two hot
/// copies); `hot/shows/moved.mkv` is a migrated name whose only copy is on `cold`.
struct Tree {
    _dir: tempfile::TempDir,
    hot: PathBuf,
    cold: PathBuf,
    catalog: PathBuf,
}

impl Tree {
    fn build() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let hot = dir.path().join("hot");
        let cold = dir.path().join("cold");
        fs::create_dir_all(hot.join("shows")).unwrap();
        fs::create_dir_all(cold.join("shows")).unwrap();
        fs::write(hot.join("a.bin"), b"shared bytes").unwrap();
        fs::write(hot.join("b.bin"), b"shared bytes").unwrap();
        fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
        std::os::unix::fs::symlink("../../cold/shows/moved.mkv", hot.join("shows/moved.mkv"))
            .unwrap();
        let tree = Tree {
            catalog: hot.join(CATALOG_NAME),
            hot,
            cold,
            _dir: dir,
        };
        let output = tree.sync();
        assert_eq!(output.status.code(), Some(0), "{}", text(&output));
        tree
    }

    fn sync(&self) -> Output {
        bin()
            .args(["catalog", "sync", "--watch"])
            .arg(&self.hot)
            .arg("--dest")
            .arg(&self.cold)
            .output()
            .unwrap()
    }

    fn delete(&self, path: &str) -> Output {
        bin()
            .args(["catalog", "delete", path, "--watch"])
            .arg(&self.hot)
            .output()
            .unwrap()
    }

    fn locate(&self, query: &str) -> Output {
        bin()
            .args(["locate", query, "--catalog"])
            .arg(&self.catalog)
            .output()
            .unwrap()
    }

    fn audit(&self) -> Output {
        bin()
            .args(["audit", "--watch"])
            .arg(&self.hot)
            .arg("--dest")
            .arg(&self.cold)
            .output()
            .unwrap()
    }

    fn open(&self) -> Catalog {
        Catalog::open(&self.catalog).unwrap()
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .open()
            .all_names()
            .unwrap()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        names.sort();
        names
    }

    fn counts(&self) -> (usize, usize, usize) {
        let catalog = self.open();
        (
            catalog.object_count().unwrap(),
            catalog.name_count().unwrap(),
            catalog.location_count().unwrap(),
        )
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[test]
fn deleting_one_of_two_names_keeps_the_object_and_its_other_copy() {
    let tree = Tree::build();
    let (objects, names, locations) = tree.counts();
    assert_eq!((objects, names, locations), (2, 3, 3));

    let output = tree.delete("a.bin");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 other name"));

    // Reference counting: the object survives with its other name; only the deleted
    // name's own hot copy was released.
    assert!(!exists(&tree.hot.join("a.bin")));
    assert_eq!(fs::read(tree.hot.join("b.bin")).unwrap(), b"shared bytes");
    assert_eq!(tree.counts(), (2, 2, 2));
    assert_eq!(tree.names(), vec!["b.bin", "shows/moved.mkv"]);

    assert_ne!(
        tree.locate("a.bin").status.code(),
        Some(0),
        "a.bin still locates"
    );
    let located = tree.locate("b.bin");
    assert_eq!(located.status.code(), Some(0), "{}", text(&located));

    // The next sync sees nothing to report and resurrects nothing.
    let sync = tree.sync();
    assert_eq!(sync.status.code(), Some(0), "{}", text(&sync));
    assert_eq!(tree.counts(), (2, 2, 2));
    let audit = tree.audit();
    assert_eq!(audit.status.code(), Some(0), "{}", text(&audit));
}

#[test]
fn deleting_the_last_name_releases_every_copy() {
    let tree = Tree::build();

    let output = tree.delete("shows/moved.mkv");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stdout).contains("its last name"));
    assert!(
        !exists(&tree.hot.join("shows/moved.mkv")),
        "the name's symlink is gone"
    );
    assert!(
        !exists(&tree.cold.join("shows/moved.mkv")),
        "the cold copy is released"
    );
    assert_eq!(tree.counts(), (1, 2, 2));
    assert_ne!(tree.locate("shows/moved.mkv").status.code(), Some(0));
    assert!(tree.open().pending_removals().unwrap().is_empty());

    // Both names of the shared object, one at a time: the bytes go with the second.
    assert_eq!(tree.delete("a.bin").status.code(), Some(0));
    assert_eq!(tree.counts(), (1, 1, 1));
    assert_eq!(tree.delete("b.bin").status.code(), Some(0));
    assert_eq!(tree.counts(), (0, 0, 0));
    assert!(!exists(&tree.hot.join("b.bin")));

    // Auditable: a sync and an audit of the emptied tree find nothing — no orphan copy
    // anywhere, nothing counted as good.
    let sync = tree.sync();
    assert_eq!(sync.status.code(), Some(0), "{}", text(&sync));
    assert_eq!(tree.counts(), (0, 0, 0));
    let audit = tree.audit();
    assert_eq!(audit.status.code(), Some(0), "{}", text(&audit));
}

#[test]
fn a_delete_of_an_unknown_name_is_refused() {
    let tree = Tree::build();
    let output = tree.delete("never.bin");
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not named"));
    assert_eq!(tree.counts(), (2, 3, 3));
}

/// Every refusal: named on stderr, exit 1, and not one row or byte changed.
#[test]
fn a_delete_that_cannot_complete_is_refused_whole() {
    // Pinned.
    let tree = Tree::build();
    tree.open()
        .set_pin("shows/moved.mkv", i64::from(u32::MAX))
        .unwrap();
    let output = tree.delete("shows/moved.mkv");
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("pinned"),
        "{}",
        text(&output)
    );
    assert_refused_cleanly(&tree);

    // A damaged copy.
    let tree = Tree::build();
    {
        let catalog = tree.open();
        let id = hex_to_bytes(&tree_object(&catalog, "shows/moved.mkv"));
        let cold_tier = tree.cold.canonicalize().unwrap();
        catalog
            .mark_damaged(
                &cold_tier.to_string_lossy(),
                "shows/moved.mkv",
                &id,
                "bitrot",
            )
            .unwrap();
    }
    let output = tree.delete("shows/moved.mkv");
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("damaged"),
        "{}",
        text(&output)
    );
    assert_refused_cleanly(&tree);

    // The copy's tier is not mounted.
    let tree = Tree::build();
    let away = tree.cold.with_file_name("cold-unmounted");
    fs::rename(&tree.cold, &away).unwrap();
    let output = tree.delete("shows/moved.mkv");
    fs::rename(&away, &tree.cold).unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not mounted"),
        "{}",
        text(&output)
    );
    assert_refused_cleanly(&tree);
}

fn assert_refused_cleanly(tree: &Tree) {
    assert_eq!(tree.counts(), (2, 3, 3), "a refused delete changed rows");
    assert!(exists(&tree.hot.join("shows/moved.mkv")));
    assert_eq!(
        fs::read(tree.cold.join("shows/moved.mkv")).unwrap(),
        b"movie bytes"
    );
    assert!(tree.open().pending_removals().unwrap().is_empty());
}

fn tree_object(catalog: &Catalog, path: &str) -> String {
    catalog
        .all_names()
        .unwrap()
        .into_iter()
        .find(|(name, _)| name == path)
        .map(|(_, id)| id)
        .expect("name is catalogued")
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

/// The window the design exists for: the transition committed, the process died before
/// a single byte was unlinked. The catalog must already not vouch for the bytes, and the
/// next `catalog sync` must finish the removal rather than re-ingest the leftover — which
/// it would otherwise do twice over: the symlink back as a name, the cold copy as an
/// object.
#[test]
fn a_crash_between_commit_and_unlink_is_finished_not_resurrected() {
    let tree = Tree::build();
    let output = bin()
        .args(["catalog", "delete", "shows/moved.mkv", "--watch"])
        .arg(&tree.hot)
        .env("JUST_CACHE_FAULT", "delete-after-commit=1")
        .output()
        .unwrap();
    assert_ne!(
        output.status.code(),
        Some(0),
        "the fault did not fire: {}",
        text(&output)
    );

    // Committed: no name, no location, nothing locates — yet every byte is still there.
    assert_eq!(tree.counts(), (1, 2, 2));
    assert_ne!(tree.locate("shows/moved.mkv").status.code(), Some(0));
    assert!(exists(&tree.hot.join("shows/moved.mkv")));
    assert_eq!(
        fs::read(tree.cold.join("shows/moved.mkv")).unwrap(),
        b"movie bytes"
    );
    assert_eq!(tree.open().pending_removals().unwrap().len(), 2);

    // Recovery is the next sync.
    let sync = tree.sync();
    assert_eq!(sync.status.code(), Some(0), "{}", text(&sync));
    assert!(!exists(&tree.hot.join("shows/moved.mkv")));
    assert!(!exists(&tree.cold.join("shows/moved.mkv")));
    assert_eq!(
        tree.counts(),
        (1, 2, 2),
        "the sync re-ingested released bytes"
    );
    assert!(tree.open().pending_removals().unwrap().is_empty());
    let audit = tree.audit();
    assert_eq!(audit.status.code(), Some(0), "{}", text(&audit));
}

/// Recovery may run long after the crash. A file rewritten at a released path in the
/// meantime is new data, and is kept: an old transaction is never licence to delete it.
#[test]
fn recovery_keeps_a_file_rewritten_after_the_crash() {
    let tree = Tree::build();
    let output = bin()
        .args(["catalog", "delete", "a.bin", "--watch"])
        .arg(&tree.hot)
        .env("JUST_CACHE_FAULT", "delete-after-commit=1")
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0), "{}", text(&output));
    // Same size, different bytes: only the re-hash can tell.
    fs::write(tree.hot.join("a.bin"), b"SHARED BYTES").unwrap();

    let sync = tree.sync();
    assert_eq!(sync.status.code(), Some(0), "{}", text(&sync));
    assert_eq!(fs::read(tree.hot.join("a.bin")).unwrap(), b"SHARED BYTES");
    assert!(tree.open().pending_removals().unwrap().is_empty());
    // Ingested as what it now is: a new object under the old name.
    assert_eq!(tree.counts(), (3, 3, 3));
}

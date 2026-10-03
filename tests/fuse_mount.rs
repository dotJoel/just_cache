//! The FUSE namespace provider (issue #42).
//!
//! The lookup/namespace layer takes the catalog as an input, so its tests run against a
//! real catalog with **no mount** — they build a tree, `catalog sync` it, and assert what
//! the namespace resolves a path to, including that a row it cannot prove stays a hard
//! error rather than an empty file. Those tests are the coverage.
//!
//! The mount-dependent test is a different animal. GitHub's `ubuntu-24.04` runner has no
//! usable `/dev/fuse`, so it is gated behind `JUST_CACHE_TEST_FUSE=1`: without that
//! variable it skips, and `JUST_CACHE_REQUIRE_FUSE=1` turns a missing `/dev/fuse` into a
//! panic (the same honest-skip pattern `tests/support` uses for the second filesystem).
//! **On CI the mount test does not run**, so it proves nothing there; it covers only that
//! a real mount serves bytes, accepts a write and a rename, and unmounts clean.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use just_cache::catalog::{Catalog, CATALOG_NAME};
use just_cache::namespace::{Entry, Namespace, NamespaceError};
use just_cache::{mount_serve, MountError, MountRequest};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

/// Opt in to the real-mount test. Unset (CI, and most dev boxes) means it skips.
const TEST_FUSE_ENV: &str = "JUST_CACHE_TEST_FUSE";
/// When set to anything but `0`/empty, a missing `/dev/fuse` panics instead of skipping —
/// the way CI would catch a mount that stopped working if it could mount at all.
const REQUIRE_FUSE_ENV: &str = "JUST_CACHE_REQUIRE_FUSE";

fn require_fuse() -> bool {
    std::env::var(REQUIRE_FUSE_ENV)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

fn link(target: &Path, at: &Path) {
    std::os::unix::fs::symlink(target, at).unwrap();
}

/// A watch root with one present file and one file the mover has already offloaded
/// (a relative symlink into the cold root), ingested by `catalog sync`.
struct Tree {
    _dir: tempfile::TempDir,
    hot: PathBuf,
    cold: PathBuf,
    catalog: PathBuf,
}

impl Tree {
    fn build() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let hot = dir.path().join("hot");
        let cold = dir.path().join("cold");
        fs::create_dir_all(hot.join("shows")).unwrap();
        fs::create_dir_all(cold.join("shows")).unwrap();

        fs::write(cold.join("shows/moved.mkv"), b"movie bytes").unwrap();
        link(
            Path::new("../../cold/shows/moved.mkv"),
            &hot.join("shows/moved.mkv"),
        );
        fs::write(hot.join("shows/live.bin"), b"still hot").unwrap();

        let tree = Tree {
            hot,
            cold,
            catalog: dir.path().join("hot").join(CATALOG_NAME),
            _dir: dir,
        };
        let output = bin()
            .args(["catalog", "sync", "--watch"])
            .arg(&tree.hot)
            .arg("--dest")
            .arg(&tree.cold)
            .output()
            .expect("just_cache runs");
        assert_eq!(
            output.status.code(),
            Some(0),
            "catalog sync failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        tree
    }

    fn open(&self) -> Catalog {
        Catalog::open(&self.catalog).expect("catalog opens")
    }

    fn canonical(&self, path: &Path) -> PathBuf {
        path.canonicalize().expect("path canonicalizes")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The directory listing of a namespace directory entry.
fn children(entry: &Entry) -> &[String] {
    match entry {
        Entry::Directory { children, .. } => children,
        Entry::File { path, .. } => panic!("{path} is a file, not a directory"),
    }
}

/// The bytes path a namespace file entry resolves to.
fn bytes_of(entry: &Entry) -> &Path {
    match entry {
        Entry::File { bytes, .. } => bytes,
        Entry::Directory { path, .. } => panic!("{path} is a directory, not a file"),
    }
}

// -- The namespace layer, against a real catalog, with no mount. --

#[test]
fn lookup_resolves_every_catalogued_name_to_its_tier_of_record() {
    let tree = Tree::build();
    let catalog = tree.open();
    let namespace = Namespace::new(&catalog).expect("namespace builds");

    // The root lists the top-level directory implied by the catalogued names.
    let root = namespace.lookup(&catalog, "").expect("root resolves");
    assert_eq!(children(&root), ["shows"]);

    let shows = namespace.lookup(&catalog, "shows").expect("dir resolves");
    assert_eq!(children(&shows), ["live.bin", "moved.mkv"]);

    // A present file is served from the watch root.
    let live = namespace
        .lookup(&catalog, "shows/live.bin")
        .expect("present file resolves");
    assert_eq!(
        bytes_of(&live),
        tree.canonical(&tree.hot).join("shows/live.bin")
    );
    assert_eq!(fs::read(bytes_of(&live)).unwrap(), b"still hot");

    // An offloaded file is served from the cold tier, not from the symlink the mover
    // left — which is the whole point of the FUSE provider.
    let moved = namespace
        .lookup(&catalog, "shows/moved.mkv")
        .expect("offloaded file resolves");
    assert_eq!(
        bytes_of(&moved),
        tree.canonical(&tree.cold).join("shows/moved.mkv")
    );
    assert_eq!(fs::read(bytes_of(&moved)).unwrap(), b"movie bytes");

    // Leading/trailing slashes name the same path, as the kernel's own paths do not
    // carry them but a caller may.
    let slashed = namespace
        .lookup(&catalog, "/shows/live.bin/")
        .expect("slashes fold");
    assert_eq!(bytes_of(&slashed), bytes_of(&live));
}

#[test]
fn an_unknown_path_is_not_found_and_never_an_empty_file() {
    let tree = Tree::build();
    let catalog = tree.open();
    let namespace = Namespace::new(&catalog).expect("namespace builds");

    let err = namespace
        .lookup(&catalog, "shows/nothing.bin")
        .expect_err("a path the catalog does not name must not resolve");
    assert!(
        matches!(err, NamespaceError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

#[test]
fn a_row_that_cannot_be_joined_into_a_path_fails_closed() {
    let tree = Tree::build();
    let catalog = tree.open();
    let namespace = Namespace::new(&catalog).expect("namespace builds");

    // Point the offloaded object's location at an absolute key, which
    // `resolve_location_path` refuses because `Path::join` would replace the tier with
    // it. A mount must report this as a failure to serve, not as an absent file — that
    // is the fail-closed half of the issue.
    {
        let conn = rusqlite::Connection::open(&tree.catalog).unwrap();
        conn.execute(
            "UPDATE location SET storage_key = '/etc/passwd' WHERE storage_key LIKE '%moved.mkv'",
            [],
        )
        .unwrap();
    }
    let err = namespace
        .lookup(&catalog, "shows/moved.mkv")
        .expect_err("an absolute key must not resolve");
    assert!(
        matches!(err, NamespaceError::Unresolvable { .. }),
        "expected Unresolvable (not NotFound, which would read as an absent file), got {err:?}"
    );
    assert!(
        err.to_string().contains("storage_key"),
        "the refusal should name the row's problem: {err}"
    );

    // And with the row removed entirely it is still `Unresolvable`: the name exists (the
    // catalog says so), the bytes have nowhere to come from.
    {
        let conn = rusqlite::Connection::open(&tree.catalog).unwrap();
        conn.execute(
            "DELETE FROM location WHERE storage_key LIKE '%moved.mkv'",
            [],
        )
        .unwrap();
    }
    let err = namespace
        .lookup(&catalog, "shows/moved.mkv")
        .expect_err("a name with no location row must not resolve");
    assert!(
        matches!(err, NamespaceError::Unresolvable { .. }),
        "expected Unresolvable, got {err:?}"
    );
}

#[test]
fn resolving_names_rewrites_neither_the_catalog_nor_the_trees() {
    let tree = Tree::build();

    let before = {
        let catalog = tree.open();
        (
            catalog.object_count().unwrap(),
            catalog.name_count().unwrap(),
            catalog.location_count().unwrap(),
            fs::read(tree.cold.join("shows/moved.mkv")).unwrap(),
            fs::read(tree.hot.join("shows/live.bin")).unwrap(),
        )
    };

    {
        let catalog = tree.open();
        let namespace = Namespace::new(&catalog).unwrap();
        for path in [
            "",
            "shows",
            "shows/live.bin",
            "shows/moved.mkv",
            "shows/nope",
        ] {
            let _ = namespace.lookup(&catalog, path);
        }
    }

    let after = {
        let catalog = tree.open();
        (
            catalog.object_count().unwrap(),
            catalog.name_count().unwrap(),
            catalog.location_count().unwrap(),
            fs::read(tree.cold.join("shows/moved.mkv")).unwrap(),
            fs::read(tree.hot.join("shows/live.bin")).unwrap(),
        )
    };
    assert_eq!(
        before, after,
        "a lookup must not write the catalog or the tiers"
    );
}

// -- What can be checked about `serve` without `/dev/fuse`. --

#[test]
fn a_catalog_with_no_recorded_roots_is_refused_before_any_mount() {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot");
    let cold = dir.path().join("cold");
    let mountpoint = dir.path().join("mnt");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::create_dir_all(&mountpoint).unwrap();

    // An un-synced catalog: the schema exists, but no root was ever recorded, so every
    // location row would be unresolvable. `serve` must refuse rather than mount an
    // empty-but-live namespace.
    let catalog_path = hot.join(CATALOG_NAME);
    drop(Catalog::open(&catalog_path).expect("catalog schema is created"));

    let err = mount_serve(&MountRequest {
        catalog_path,
        watch: hot,
        dests: vec![cold],
        mountpoint,
    })
    .expect_err("a rootless catalog must not mount");
    assert!(
        matches!(err, MountError::NoRoots(_)),
        "expected NoRoots, got {err:?}"
    );
}

#[test]
fn mount_refuses_a_missing_catalog_as_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot");
    let cold = dir.path().join("cold");
    let mountpoint = dir.path().join("mnt");
    fs::create_dir_all(&hot).unwrap();
    fs::create_dir_all(&cold).unwrap();
    fs::create_dir_all(&mountpoint).unwrap();

    let output = bin()
        .arg("mount")
        .arg(&mountpoint)
        .arg("--watch")
        .arg(&hot)
        .arg("--dest")
        .arg(&cold)
        .output()
        .expect("just_cache runs");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a mount over no catalog serves an empty namespace and must be a usage error:\n{}\n{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("catalog sync"),
        "the error should say how to make a catalog"
    );
}

#[test]
fn mount_refuses_a_non_empty_mountpoint_rather_than_hiding_files() {
    let tree = Tree::build();
    let dir = tempfile::tempdir().unwrap();
    let mountpoint = dir.path().join("mnt");
    fs::create_dir_all(&mountpoint).unwrap();
    fs::write(mountpoint.join("do-not-hide-me"), b"x").unwrap();

    let err = mount_serve(&MountRequest {
        catalog_path: tree.catalog.clone(),
        watch: tree.hot.clone(),
        dests: vec![tree.cold.clone()],
        mountpoint: mountpoint.clone(),
    })
    .expect_err("a non-empty mountpoint must be refused");
    assert!(
        matches!(err, MountError::MountpointNotEmpty(_)),
        "expected MountpointNotEmpty, got {err:?}"
    );
    // The file is still there: the refusal happened before anything was mounted.
    assert!(mountpoint.join("do-not-hide-me").is_file());
}

#[test]
fn mount_refuses_a_dest_the_catalog_never_recorded() {
    let tree = Tree::build();
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("some-other-disk");
    let mountpoint = dir.path().join("mnt");
    fs::create_dir_all(&other).unwrap();
    fs::create_dir_all(&mountpoint).unwrap();

    // The catalog was synced from `tree.cold`, not from `other`. Serving it while
    // naming `other` as a tier would resolve that disk's rows to nothing, so the
    // mismatch must be refused before a mount.
    let err = mount_serve(&MountRequest {
        catalog_path: tree.catalog.clone(),
        watch: tree.hot.clone(),
        dests: vec![tree.cold.clone(), other.clone()],
        mountpoint,
    })
    .expect_err("a tier the catalog does not record must be refused");
    assert!(
        matches!(err, MountError::UnknownTier(_)),
        "expected UnknownTier, got {err:?}"
    );
}

// -- The real mount. Gated: CI cannot run it. --

/// Poll `/proc/mounts` until the mountpoint appears, so the test acts only once the
/// kernel is actually serving requests.
fn wait_until_mounted(mountpoint: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let target = mountpoint.to_string_lossy().to_string();
    while Instant::now() < deadline {
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            if mounts.lines().any(|line| {
                line.split_whitespace()
                    .nth(1)
                    .map(|field| field.replace("\\040", " ") == target)
                    .unwrap_or(false)
            }) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn unmount(mountpoint: &Path) {
    // `fusermount3` is the userspace unmount helper fuser's pure-Rust path uses; a
    // plain `umount` needs privileges the test does not have.
    let _ = Command::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .status();
}

fn reap(mut child: Child) {
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A real mount: `ls`, `stat`, `read`, `write`, `rename` behave as a filesystem does,
/// and an unmount leaves no partial file behind.
///
/// **What this does not cover on CI**: it does not run there at all. `/dev/fuse` and the
/// `fusermount3` setuid helper are absent on `ubuntu-24.04`, so without
/// `JUST_CACHE_TEST_FUSE=1` this skips. Everything a mount alone could prove — that the
/// kernel is answered by the real handler, that writes go through to a tier file — is
/// therefore verified only by a human running this with the gate set (docs/design.md §9).
#[test]
fn a_real_mount_serves_bytes_and_unmounts_cleanly() {
    if std::env::var(TEST_FUSE_ENV).is_err() {
        eprintln!(
            "skipping: set {TEST_FUSE_ENV}=1 (and have /dev/fuse + fusermount3) to run the \
             real mount check"
        );
        return;
    }
    if !Path::new("/dev/fuse").exists() {
        if require_fuse() {
            panic!("{REQUIRE_FUSE_ENV} is set but /dev/fuse is missing; the mount cannot run");
        }
        eprintln!("skipping: /dev/fuse is not present");
        return;
    }

    let tree = Tree::build();
    let dir = tempfile::tempdir().unwrap();
    let mountpoint = dir.path().join("mnt");
    fs::create_dir_all(&mountpoint).unwrap();

    let child = bin()
        .arg("mount")
        .arg(&mountpoint)
        .arg("--watch")
        .arg(&tree.hot)
        .arg("--dest")
        .arg(&tree.cold)
        .spawn()
        .expect("the mount process starts");

    if !wait_until_mounted(&mountpoint, Duration::from_secs(5)) {
        reap(child);
        panic!("the mountpoint never appeared in /proc/mounts");
    }

    // ls/stat/read.
    let listing = fs::read_dir(&mountpoint).unwrap().count();
    assert!(listing >= 1, "the mount lists the catalogued namespace");
    assert_eq!(
        fs::read(mountpoint.join("shows/moved.mkv")).unwrap(),
        b"movie bytes"
    );
    assert!(fs::metadata(mountpoint.join("shows/live.bin"))
        .unwrap()
        .is_file());

    // write: lands on the tier of record's bytes, in place.
    let hot_file = tree.hot.join("shows/live.bin");
    fs::write(mountpoint.join("shows/live.bin"), b"rewritten").unwrap();
    assert_eq!(fs::read(&hot_file).unwrap(), b"rewritten");

    // create + rename within the watch root.
    fs::write(mountpoint.join("new.bin"), b"fresh").unwrap();
    assert!(tree.hot.join("new.bin").is_file());
    fs::rename(mountpoint.join("new.bin"), mountpoint.join("renamed.bin")).unwrap();
    assert!(tree.hot.join("renamed.bin").is_file());

    // Unmount is clean: the daemon exits, nothing partial is left behind.
    unmount(&mountpoint);
    reap(child);
    assert!(
        fs::read_dir(&mountpoint).unwrap().next().is_none(),
        "an unmounted mountpoint reads as the empty directory it was"
    );
    let partials: Vec<_> = fs::read_dir(&tree.hot)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".just_cache-partial-")
        })
        .collect();
    assert!(partials.is_empty(), "no partial file survived the unmount");
}

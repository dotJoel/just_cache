//! Shared helpers for integration tests that need a *genuinely different* filesystem.
//!
//! A cold tier is a different mount by definition, so the `rename` path that every
//! ordinary test hits can never prove the fallback works: `rename` across a mount point
//! fails with `EXDEV`, and `copy_then_remove` is the branch that runs instead. That
//! fallback is the only path that can leave a partial file behind, which makes it the
//! most important one to exercise and the easiest to miss.
//!
//! A test opts in by pointing `JUST_CACHE_TEST_SECOND_FS` at a directory on another
//! filesystem. The CI workflow mounts a tmpfs for exactly this; the README documents
//! how to do it locally. A sibling branch can use the same module — the env var and
//! the helpers here are the shared contract between the two sets of tests.
//!
//! Skipping has to be honest in both directions:
//!
//! * On a developer machine with no second filesystem, these tests skip. A red suite
//!   for an environment that simply cannot support the check is noise, and noise gets
//!   muted — which is how coverage rots.
//! * In CI a second filesystem is mounted on purpose. If the tests then silently skip,
//!   the coverage we added has evaporated while the job stays green. That is the worst
//!   outcome, so CI also sets `JUST_CACHE_REQUIRE_SECOND_FS=1`, which turns every skip
//!   into a panic. The two states are therefore distinguished by an explicit second
//!   variable, not by sniffing `CI`: a skip is opt-in, never the default.

#![allow(dead_code)] // shared by several test binaries; each one uses a different subset

use std::fs;
use std::path::{Path, PathBuf};

/// Directory on the second filesystem, e.g. a tmpfs mounted by CI.
pub const SECOND_FS_ENV: &str = "JUST_CACHE_TEST_SECOND_FS";

/// When set to anything but `0`/empty, a missing second filesystem panics instead of
/// skipping. CI sets this so a broken mount cannot leave the job green.
pub const REQUIRE_SECOND_FS_ENV: &str = "JUST_CACHE_REQUIRE_SECOND_FS";

/// A verified directory on a filesystem distinct from the default temp filesystem.
pub struct SecondFs {
    root: PathBuf,
}

impl SecondFs {
    /// The base directory the second filesystem was mounted at.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Device id of the filesystem holding this directory.
    pub fn device(&self) -> u64 {
        device_id(&self.root)
    }
}

/// The device id (`st_dev`) of the filesystem holding `path`.
///
/// Comparing this between source and destination is how a test proves its move really
/// crossed a mount point. `st_dev` is part of a file's identity and is the signal
/// behind `rename`'s `EXDEV`, so it is the portable way to say "these are different
/// disks" without asking the OS to fail first.
#[cfg(unix)]
pub fn device_id(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;

    fs::metadata(path)
        .unwrap_or_else(|err| panic!("cannot stat {}: {err}", path.display()))
        .dev()
}

#[cfg(not(unix))]
pub fn device_id(_path: &Path) -> u64 {
    0
}

/// True when `a` and `b` live on different filesystems.
pub fn are_on_different_filesystems(a: &Path, b: &Path) -> bool {
    device_id(a) != device_id(b)
}

/// Assert that a source/destination pair really straddles a mount point.
///
/// This is the guard the whole exercise hangs on: if a later change lands both sides on
/// one filesystem, the test must fail *here*, loudly, rather than quietly degrading
/// into a rerun of the `rename` path it already covered. Tests call it immediately
/// before the move, on the exact directories the move will use.
pub fn assert_cross_device(source: &Path, dest: &Path) {
    let (src_dev, dest_dev) = (device_id(source), device_id(dest));
    assert_ne!(
        src_dev,
        dest_dev,
        "source {} (dev {src_dev}) and destination {} (dev {dest_dev}) are on the same \
         filesystem, so the cross-device copy path is NOT being exercised; point \
         {SECOND_FS_ENV} at a directory on a different mount",
        source.display(),
        dest.display()
    );
    eprintln!(
        "cross-device check: source dev {src_dev}, destination dev {dest_dev} — the move \
         must take the copy fallback"
    );
}

/// Prove at the syscall level that `fs::rename` cannot move a file between these two
/// trees, so the production code is *forced* onto `copy_then_remove` rather than merely
/// assumed to be. Uses a throwaway probe file on the source filesystem and cleans it up.
pub fn assert_rename_is_cross_device(source_dir: &Path, dest_dir: &Path) {
    let probe = source_dir.join(".just_cache-exdev-probe");
    let target = dest_dir.join(".just_cache-exdev-probe");
    fs::write(&probe, b"probe").unwrap_or_else(|err| panic!("cannot write probe: {err}"));

    let renamed = fs::rename(&probe, &target);
    // Whether it worked or not, do not leave the probe behind.
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_file(&target);

    match renamed {
        Ok(()) => panic!("rename crossed a mount point; this pair is not cross-device"),
        Err(err) => assert_eq!(
            err.raw_os_error(),
            Some(18), // EXDEV
            "expected rename to fail with EXDEV, got {err:?}"
        ),
    }
}

/// Resolve the second filesystem, or return `None` when this machine has none.
///
/// Panics instead of returning `None` when [`REQUIRE_SECOND_FS_ENV`] is set, so a CI
/// job that mounts a filesystem but fails to hand it over cannot pass silently.
pub fn second_fs() -> Option<SecondFs> {
    match resolve_second_fs() {
        Ok(second) => Some(second),
        Err(reason) => {
            if required() {
                panic!(
                    "{REQUIRE_SECOND_FS_ENV} is set, so a second filesystem is required, but \
                     it could not be used: {reason}. The CI job that mounts one must export \
                     {SECOND_FS_ENV} as a directory on it."
                );
            }
            eprintln!("skipping cross-device test: {reason}");
            None
        }
    }
}

fn required() -> bool {
    matches!(
        std::env::var(REQUIRE_SECOND_FS_ENV),
        Ok(value) if !value.is_empty() && value != "0"
    )
}

fn resolve_second_fs() -> Result<SecondFs, String> {
    let raw = std::env::var(SECOND_FS_ENV).map_err(|_| format!("{SECOND_FS_ENV} is not set"))?;
    if raw.trim().is_empty() {
        return Err(format!("{SECOND_FS_ENV} is empty"));
    }

    let root = PathBuf::from(raw);
    let metadata = fs::metadata(&root)
        .map_err(|err| format!("{SECOND_FS_ENV}={} is not readable: {err}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "{SECOND_FS_ENV}={} is not a directory",
            root.display()
        ));
    }

    // Probe the default temp filesystem and compare. A configured path that turns out
    // to be the same filesystem is treated as "no second filesystem": creating a
    // directory is not the same as mounting one, and the common local mistake of
    // pointing at an ordinary folder must not be mistaken for working coverage.
    #[cfg(unix)]
    {
        let probe = tempfile::tempdir().map_err(|err| format!("no usable temp dir: {err}"))?;
        if !are_on_different_filesystems(probe.path(), &root) {
            return Err(format!(
                "{SECOND_FS_ENV}={} is on the same filesystem as the test temp dir ({})",
                root.display(),
                probe.path().display()
            ));
        }
    }
    #[cfg(not(unix))]
    {
        return Err("filesystem identity can only be compared on unix".to_string());
    }

    Ok(SecondFs { root })
}

/// A private directory inside the second filesystem for one test to work in.
///
/// Kept as a `TempDir` so the caller holds it alive and it is removed afterwards;
/// tests run in parallel, so each one gets its own subtree.
pub fn work_dir(second: &SecondFs, label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(label)
        .tempdir_in(second.root())
        .unwrap_or_else(|err| {
            panic!(
                "cannot create a work dir under {}: {err}",
                second.root().display()
            )
        })
}

/// Every `.just_cache-partial-*` file anywhere under `root`.
///
/// The production walker deliberately *hides* partial files, so it is not a trustworthy
/// witness for "no partial was left behind" — a leftover would be exactly the thing it
/// is built to ignore. Tests therefore look with their own eyes, by name, at every file
/// under the destination.
pub fn partial_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            let is_partial = path
                .file_name()
                .map(|name| name.to_string_lossy().starts_with(".just_cache-partial-"))
                .unwrap_or(false);
            if is_partial {
                found.push(path);
            }
        }
    }

    found.sort();
    found
}

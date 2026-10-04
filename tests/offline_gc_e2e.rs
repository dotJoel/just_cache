//! Deleting and collecting an `offline` tier's bytes when the volume is, or is not, the
//! one in the drive (issue #144), end to end through the real binary.
//!
//! An `offline` copy has no local representation: its bytes sit on a disk a person
//! inserts, and the catalog's `location` row — `tier = <configured name>`, `storage_key =
//! <volume-id>/<relative>` — is the only record. A delete of such a copy must therefore do
//! one of two honest things: when the exact volume named by the key is the one mounted,
//! resolve `<mount>/<relative>` and finish; when it is not, record the intent and report
//! it, never unlink another volume's bytes.
//!
//! The catalog and `tiers.toml` sit beside the watch root — the placement every command
//! defaults to — and the sweep excludes the config file so the tier description is not
//! itself moved to the tier.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use just_cache::catalog::{Catalog, CATALOG_NAME};

const KEY_ENV: &str = "JC_OFFLINE_GC_KEY";
const KEY_HEX: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0\
                       f0e1d2c3b4a5968778695a4b3c2d1e0f";

fn bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_just_cache"));
    command.env(KEY_ENV, KEY_HEX);
    command
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("the process should exit")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

struct Fixture {
    _dir: tempfile::TempDir,
    hot: PathBuf,
    vol: PathBuf,
    catalog: PathBuf,
}

/// A hot tree with one cold file and an empty volume mount, with `tiers.toml` beside the
/// watch root (the placement the catalog shares) naming the mount as an `offline` tier.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let hot = dir.path().join("hot");
    let vol = dir.path().join("vol");
    fs::create_dir_all(hot.join("shows")).unwrap();
    fs::create_dir_all(&vol).unwrap();
    fs::write(hot.join("shows/movie.mkv"), b"the cold movie bytes").unwrap();
    fs::write(
        hot.join("tiers.toml"),
        format!(
            "[tiers.drawer]\n\
             kind = \"offline\"\n\
             path = \"{}\"\n\
             volatility = \"persistent\"\n\
             recall = \"hours\"\n\
             copies = 1\n\
             vaults = [\"shelf-a\"]\n\
             encryption_key = \"${KEY_ENV}\"\n",
            vol.display()
        ),
    )
    .unwrap();
    Fixture {
        catalog: hot.join(CATALOG_NAME),
        hot,
        vol,
        _dir: dir,
    }
}

/// Sync the tree: records the catalog roots and the offline tier's floor.
fn sync(fixture: &Fixture) -> Output {
    bin()
        .args(["catalog", "sync", "--watch"])
        .arg(&fixture.hot)
        .arg("--dest")
        .arg(&fixture.vol)
        .output()
        .unwrap()
}

/// Sync the tree, record the volume mounted, and sweep the movie onto it. The config file
/// itself is excluded from the sweep — it is the tier description, not cold data.
fn offload(fixture: &Fixture) {
    let synced = sync(fixture);
    assert_eq!(
        code(&synced),
        0,
        "sync failed: {}\n{}",
        stderr(&synced),
        stdout(&synced)
    );

    let mount = bin()
        .args([
            "volume",
            "set",
            "drawer-01",
            "mounted",
            "--tier",
            "drawer",
            "--catalog",
        ])
        .arg(&fixture.catalog)
        .output()
        .unwrap();
    assert_eq!(code(&mount), 0, "volume set failed: {}", stderr(&mount));

    let sweep = bin()
        .args(["sweep", "--watch"])
        .arg(&fixture.hot)
        .arg("--dest")
        .arg(&fixture.vol)
        .args(["--exclude", "tiers.toml"])
        .args(["--min-idle-days", "0", "--once"])
        .output()
        .unwrap();
    assert_eq!(code(&sweep), 0, "sweep failed: {}", stderr(&sweep));
    assert!(
        !exists(&fixture.hot.join("shows/movie.mkv")),
        "the offline move retires the source"
    );
    assert!(exists(&fixture.vol.join("shows/movie.mkv")));
}

fn set_volume(fixture: &Fixture, id: &str, state: &str) {
    let output = bin()
        .args(["volume", "set", id, state])
        .args(["--tier", "drawer", "--catalog"])
        .arg(&fixture.catalog)
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "volume {id} {state}: {}", stderr(&output));
}

#[test]
fn a_released_offline_copy_waits_for_the_right_volume_then_completes() {
    let fixture = fixture();
    offload(&fixture);
    let blob = fixture.vol.join("shows/movie.mkv");

    // The wrong volume is in the drive: the delete commits, the bytes stay, the location
    // is named as deferred — never unlinked from a disk the key did not name.
    set_volume(&fixture, "drawer-01", "in_vault");
    set_volume(&fixture, "drawer-02", "mounted");

    let deleted = bin()
        .args(["catalog", "delete", "shows/movie.mkv", "--watch"])
        .arg(&fixture.hot)
        .output()
        .unwrap();
    assert_eq!(code(&deleted), 1, "{}", stderr(&deleted));
    assert!(
        stdout(&deleted).contains("deferred"),
        "the deferred location must be named: {}",
        stdout(&deleted)
    );
    assert!(
        exists(&blob),
        "bytes on the wrong volume must not be touched"
    );
    assert_eq!(
        Catalog::open(&fixture.catalog)
            .unwrap()
            .pending_removals()
            .unwrap()
            .len(),
        1
    );

    // The right volume returns; the next sync finishes the recorded release.
    set_volume(&fixture, "drawer-02", "in_vault");
    set_volume(&fixture, "drawer-01", "mounted");
    let done = sync(&fixture);
    assert_eq!(
        code(&done),
        0,
        "sync failed: {}\n{}",
        stderr(&done),
        stdout(&done)
    );
    assert!(
        !exists(&blob),
        "the released offline copy is removed once its volume is back"
    );
    assert!(Catalog::open(&fixture.catalog)
        .unwrap()
        .pending_removals()
        .unwrap()
        .is_empty());
}

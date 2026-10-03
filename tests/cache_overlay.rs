//! Cache overlays (§2.1, issue #46): promotion copies, LRU eviction at `max_size`,
//! write-invalidate, and the refusals that keep a cache from ever being a home.
//!
//! The overlay is exercised through its library API rather than a FUSE mount: a real
//! mount needs `/dev/fuse` (gated in `tests/fuse_mount.rs`), while every decision the
//! mount delegates — promote, serve, evict, invalidate — is made in `src/cache.rs` and is
//! testable here against real files. The FUSE call sites themselves (open/setattr/rename
//! calling `invalidate`) are covered only by review; see design §9.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use just_cache::cache::Overlay;
use just_cache::{CacheConfig, Catalog, PromoteOn, TierSet};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_just_cache"))
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    ram: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("ssd");
        let ram = tmp.path().join("ram");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&ram).unwrap();
        Fixture {
            home,
            ram,
            _tmp: tmp,
        }
    }

    fn config(&self, max_size: u64, accesses: u32) -> CacheConfig {
        CacheConfig {
            name: "ram".into(),
            over: "ssd".into(),
            kind: "fs".into(),
            path: self.ram.clone(),
            max_size,
            promote_on: PromoteOn {
                accesses,
                window: Duration::from_secs(86_400),
            },
        }
    }

    fn file(&self, name: &str, body: &[u8]) -> PathBuf {
        let path = self.home.join(name);
        fs::write(&path, body).unwrap();
        path
    }
}

fn dir_len(dir: &Path) -> usize {
    fs::read_dir(dir).unwrap().count()
}

#[test]
fn promotion_copies_bytes_and_moves_nothing() {
    let fx = Fixture::new();
    let home = fx.file("a.bin", b"hot bytes");
    let mut overlay = Overlay::open(fx.config(1024, 2), &fx.home).unwrap();

    // One access is below `promote_on = 2 accesses`: served from home, nothing copied.
    let first = overlay.read("a.bin", "obj-a", &home, 100);
    assert!(!first.promoted && first.serve.is_none());
    assert_eq!(dir_len(overlay.dir()), 0);

    let second = overlay.read("a.bin", "obj-a", &home, 200);
    assert!(
        second.promoted,
        "the second access within the window promotes"
    );
    let copy = second
        .serve
        .expect("a promoted read is served from the overlay");
    assert!(copy.starts_with(overlay.dir()));
    assert_eq!(fs::read(&copy).unwrap(), b"hot bytes");
    // The home is untouched: still a regular file with the same bytes, not a symlink.
    assert!(fs::symlink_metadata(&home).unwrap().file_type().is_file());
    assert_eq!(fs::read(&home).unwrap(), b"hot bytes");

    // A later read is a hit on the same copy.
    let third = overlay.read("a.bin", "obj-a", &home, 300);
    assert_eq!(third.serve.as_deref(), Some(copy.as_path()));
    assert!(!third.promoted);
}

#[test]
fn lru_eviction_holds_max_size() {
    let fx = Fixture::new();
    let a = fx.file("a", &[b'a'; 40]);
    let b = fx.file("b", &[b'b'; 40]);
    let c = fx.file("c", &[b'c'; 40]);
    let mut overlay = Overlay::open(fx.config(100, 1), &fx.home).unwrap();

    assert!(overlay.read("a", "A", &a, 1).promoted);
    assert!(overlay.read("b", "B", &b, 2).promoted);
    // Touch `a` so `b` is now the least recently used.
    assert!(overlay.read("a", "A", &a, 3).serve.is_some());

    let outcome = overlay.read("c", "C", &c, 4);
    assert!(outcome.promoted);
    assert_eq!(
        outcome.dropped,
        vec!["b".to_string()],
        "LRU evicts `b`, not `a`"
    );
    assert!(overlay.is_resident("a") && overlay.is_resident("c"));
    assert!(!overlay.is_resident("b"));
    assert!(overlay.used() <= 100, "used {} > max_size", overlay.used());
    assert_eq!(
        dir_len(overlay.dir()),
        2,
        "evicted bytes are removed, not leaked"
    );
    // Eviction never touches a home.
    assert_eq!(fs::read(&b).unwrap(), vec![b'b'; 40]);

    // A file larger than the whole overlay is never promoted and evicts nothing.
    let big = fx.file("big", &[b'x'; 101]);
    let outcome = overlay.read("big", "X", &big, 5);
    assert!(!outcome.promoted && outcome.dropped.is_empty());
    assert!(overlay.is_resident("a") && overlay.is_resident("c"));
}

#[test]
fn a_write_lands_on_home_and_drops_the_cache_copy() {
    let fx = Fixture::new();
    let home = fx.file("doc", b"version one");
    let mut overlay = Overlay::open(fx.config(1024, 1), &fx.home).unwrap();
    let copy = overlay.read("doc", "O1", &home, 1).serve.unwrap();

    // What the mount does on a write-open: invalidate, then write the home.
    assert!(overlay.invalidate("doc"));
    fs::write(&home, b"version two, longer").unwrap();
    assert!(!copy.exists(), "the stale copy is gone, not just forgotten");
    assert!(!overlay.is_resident("doc"));
    assert_eq!(overlay.used(), 0);

    // The next read re-promotes the *new* bytes.
    let fresh = overlay.read("doc", "O2", &home, 2).serve.unwrap();
    assert_eq!(fs::read(fresh).unwrap(), b"version two, longer");
}

#[test]
fn a_home_changed_behind_the_overlay_is_never_served_stale() {
    let fx = Fixture::new();
    let home = fx.file("doc", b"old");
    let mut overlay = Overlay::open(fx.config(1024, 1), &fx.home).unwrap();
    assert!(overlay.read("doc", "O", &home, 1).promoted);

    // Rewritten without going through the mount (no invalidate call). Size changes, so
    // the stamp check must catch it whatever the filesystem's mtime granularity.
    fs::write(&home, b"newer bytes").unwrap();
    let outcome = overlay.read("doc", "O", &home, 2);
    assert!(outcome.dropped.contains(&"doc".to_string()));
    let served = outcome.serve.expect("re-promoted under promote_on = 1");
    assert_eq!(fs::read(served).unwrap(), b"newer bytes");
}

#[test]
fn losing_the_overlay_is_not_data_loss_and_it_reopens_empty() {
    let fx = Fixture::new();
    let home = fx.file("a", b"bytes");
    let dir = {
        let mut overlay = Overlay::open(fx.config(1024, 1), &fx.home).unwrap();
        assert!(overlay.read("a", "A", &home, 1).promoted);
        overlay.dir().to_path_buf()
    };
    assert_eq!(dir_len(&dir), 1, "a dropped process leaves its copy behind");
    let overlay = Overlay::open(fx.config(1024, 1), &fx.home).unwrap();
    assert_eq!(
        dir_len(&dir),
        0,
        "reopening discards what nothing was watching"
    );
    assert!(overlay.residency().is_empty());
    assert_eq!(fs::read(&home).unwrap(), b"bytes");
    // Nothing else under the configured path was touched.
    assert_eq!(dir_len(&fx.ram), 1);
}

#[test]
fn residency_rows_never_become_locations() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(tmp.path().join("catalog.sqlite")).unwrap();
    let before = catalog.location_count().unwrap();
    catalog
        .record_cache_residency("ram", "a.bin", "abcd", 10)
        .unwrap();
    assert_eq!(catalog.location_count().unwrap(), before);
    assert_eq!(catalog.cache_residency().unwrap().len(), 1);
    catalog.clear_cache_residency("ram").unwrap();
    assert!(catalog.cache_residency().unwrap().is_empty());
}

fn tiers_toml(ssd: &Path, ram: &Path, write_policy: &str) -> String {
    format!(
        "[tiers.ssd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"ms\"\ncopies = 1\n\n\
         [[cache]]\nname = \"ram\"\nover = \"ssd\"\nkind = \"fs\"\npath = \"{}\"\nmax_size = \"64MiB\"\n\
         promote_on = \"2 accesses / 24h\"\nevict = \"lru\"\nwrite_policy = \"{write_policy}\"\n",
        ssd.display(),
        ram.display()
    )
}

#[test]
fn a_cache_block_parses_apart_from_the_tiers() {
    let fx = Fixture::new();
    let set = TierSet::parse(
        &tiers_toml(&fx.home, &fx.ram, "write-invalidate"),
        Path::new("tiers.toml"),
    )
    .unwrap();
    assert_eq!(set.tiers().len(), 1, "a cache is never listed as a tier");
    let cache = set.cache("ram").unwrap();
    assert_eq!(cache.max_size, 64 * 1024 * 1024);
    assert_eq!(cache.promote_on.accesses, 2);
    assert!(set.get("ram").is_none());
}

#[test]
fn writeback_and_overlapping_paths_are_refused() {
    let fx = Fixture::new();
    let err = TierSet::parse(
        &tiers_toml(&fx.home, &fx.ram, "writeback"),
        Path::new("tiers.toml"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("write-invalidate"), "{err}");

    let inside = fx.home.join("ram");
    let err = TierSet::parse(
        &tiers_toml(&fx.home, &inside, "write-invalidate"),
        Path::new("tiers.toml"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("overlaps tier"), "{err}");
}

#[test]
fn a_cache_is_refused_as_a_destination_root() {
    let fx = Fixture::new();
    let watch = fx.home.parent().unwrap().join("watch");
    fs::create_dir_all(&watch).unwrap();
    let tiers = fx.home.parent().unwrap().join("tiers.toml");
    fs::write(&tiers, tiers_toml(&fx.home, &fx.ram, "write-invalidate")).unwrap();
    fs::write(watch.join("f"), b"payload").unwrap();

    let out = bin()
        .args(["--watch"])
        .arg(&watch)
        .arg("--dest")
        .arg(&fx.ram)
        .arg("--tiers")
        .arg(&tiers)
        .args(["--min-idle-days", "0", "--once"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a cache as --dest must be refused");
    assert!(stderr.contains("cache overlay `ram`"), "{stderr}");
    assert!(watch.join("f").is_file(), "nothing moved");
    assert_eq!(dir_len(&fx.ram), 0, "nothing landed in the cache");
}

#[test]
fn a_cache_is_refused_as_a_policy_target() {
    let fx = Fixture::new();
    let root = fx.home.parent().unwrap();
    let hdd = root.join("hdd");
    fs::create_dir_all(&hdd).unwrap();
    let mut toml = tiers_toml(&fx.home, &fx.ram, "write-invalidate");
    toml.push_str(&format!(
        "\n[tiers.hdd]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\nrecall = \"s\"\ncopies = 1\n",
        hdd.display()
    ));
    let set = TierSet::parse(&toml, Path::new("tiers.toml")).unwrap();
    let rules = just_cache::RuleSet::parse(
        "[[rule]]\nname = \"warm\"\nmatch = \"**\"\ndown = { from = \"ssd\", to = \"ram\", after_idle = \"1d\" }\n",
        Path::new("policy.toml"),
    )
    .unwrap();
    let err = rules.validate_tiers(&set).unwrap_err();
    assert!(err.to_string().contains("never a policy target"), "{err}");
}

#[test]
fn cache_status_marks_residency_ephemeral() {
    let fx = Fixture::new();
    let root = fx.home.parent().unwrap();
    let watch = root.join("watch");
    fs::create_dir_all(&watch).unwrap();
    let tiers = root.join("tiers.toml");
    fs::write(&tiers, tiers_toml(&fx.home, &fx.ram, "write-invalidate")).unwrap();
    let catalog_path = root.join("catalog.sqlite");
    Catalog::open(&catalog_path)
        .unwrap()
        .record_cache_residency("ram", "a.bin", "abcd", 10)
        .unwrap();
    let out = bin()
        .args(["cache", "status", "--watch"])
        .arg(&watch)
        .arg("--tiers")
        .arg(&tiers)
        .arg("--catalog")
        .arg(&catalog_path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("EPHEMERAL"), "{stdout}");
    assert!(stdout.contains("resident (ephemeral): a.bin"), "{stdout}");
}

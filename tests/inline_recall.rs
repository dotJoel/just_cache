//! Inline recall on read (issue #44).
//!
//! The FUSE binding only translates [`Recaller::recall`] into an errno or a file to open,
//! and CI has no `/dev/fuse`, so these tests drive the recaller directly against a real
//! catalog with the cold tier on a **second filesystem** (`JUST_CACHE_TEST_SECOND_FS`):
//! the recall genuinely copies across devices, through `restore`'s verified path.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use just_cache::catalog::{Catalog, CATALOG_NAME};
use just_cache::recall::{RecallError, RecallOutcome, RecallPolicy, Recaller};
use just_cache::tiers::TierSet;

const PAYLOAD: &[u8] = b"offloaded movie bytes, recalled inline\n";

struct Tree {
    _hot_dir: tempfile::TempDir,
    _cold_dir: tempfile::TempDir,
    hot: PathBuf,
    cold: PathBuf,
    catalog: PathBuf,
}

impl Tree {
    /// A watch root on the default temp filesystem holding a symlink for an object
    /// whose only copy sits on a cold root on the second filesystem, synced.
    fn build(second: &support::SecondFs) -> Self {
        let hot_dir = tempfile::tempdir().unwrap();
        let cold_dir = support::work_dir(second, "recall");
        let hot = fs::canonicalize(hot_dir.path()).unwrap().join("hot");
        let cold = fs::canonicalize(cold_dir.path()).unwrap();
        fs::create_dir_all(hot.join("shows")).unwrap();
        fs::create_dir_all(cold.join("shows")).unwrap();
        fs::write(cold.join("shows/moved.mkv"), PAYLOAD).unwrap();
        std::os::unix::fs::symlink(cold.join("shows/moved.mkv"), hot.join("shows/moved.mkv"))
            .unwrap();
        support::assert_cross_device(&hot, &cold);

        let output = Command::new(env!("CARGO_BIN_EXE_just_cache"))
            .args(["catalog", "sync", "--watch"])
            .arg(&hot)
            .arg("--dest")
            .arg(&cold)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "sync: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Tree {
            catalog: hot.join(CATALOG_NAME),
            hot,
            cold,
            _hot_dir: hot_dir,
            _cold_dir: cold_dir,
        }
    }

    fn tiers(&self, recall: &str) -> TierSet {
        let text = format!(
            "[tiers.archive]\nkind = \"fs\"\npath = \"{}\"\nvolatility = \"persistent\"\n\
             recall = \"{recall}\"\ncopies = 1\n",
            self.cold.display()
        );
        TierSet::parse(&text, &self.hot.join("tiers.toml")).unwrap()
    }

    fn recaller(&self, recall: &str, policy: RecallPolicy) -> Recaller {
        Recaller::new(
            self.catalog.clone(),
            self.hot.clone(),
            Some(self.tiers(recall)),
            policy,
        )
    }

    fn state(&self) -> String {
        Catalog::open(&self.catalog)
            .unwrap()
            .state_for_path("shows/moved.mkv")
            .unwrap()
            .unwrap()
    }

    /// Hot-tier location rows for the object: `(storage_key, verified, primary)`.
    fn hot_rows(&self) -> Vec<(String, bool, bool)> {
        let record = Catalog::open(&self.catalog)
            .unwrap()
            .record_for_path("shows/moved.mkv")
            .unwrap()
            .unwrap();
        let hot = self.hot.to_string_lossy().into_owned();
        record
            .locations
            .into_iter()
            .filter(|location| location.tier == hot)
            .map(|location| (location.storage_key, location.verified, location.is_primary))
            .collect()
    }

    fn hot_path(&self) -> PathBuf {
        self.hot.join("shows/moved.mkv")
    }
}

fn is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

#[test]
fn a_read_of_an_offloaded_object_recalls_it_verified_from_the_second_root() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tree = Tree::build(&second);
    assert_eq!(tree.state(), "offloaded");

    let outcome = tree
        .recaller("s", RecallPolicy::Promote)
        .recall("shows/moved.mkv")
        .expect("an `s` tier recalls inline");
    match &outcome {
        RecallOutcome::Recalled { bytes, tier } => {
            assert_eq!(bytes, &tree.hot_path());
            assert_eq!(tier, "archive", "the tier is named by its configured name");
        }
        other => panic!("expected a recall, got {other:?}"),
    }

    // The caller is served the recalled bytes, and they are a real file on the hot tier.
    assert_eq!(fs::read(outcome.bytes()).unwrap(), PAYLOAD);
    assert!(is_regular(&tree.hot_path()), "the symlink was replaced");
    // The cold copy is kept: a recall is a copy, never a move.
    assert_eq!(
        fs::read(tree.cold.join("shows/moved.mkv")).unwrap(),
        PAYLOAD
    );

    // Recorded with the real checksum: verified, the tier of record, and present.
    assert_eq!(
        tree.hot_rows(),
        vec![("shows/moved.mkv".to_string(), true, true)]
    );
    assert_eq!(tree.state(), "present");
    let record = Catalog::open(&tree.catalog)
        .unwrap()
        .record_for_path("shows/moved.mkv")
        .unwrap()
        .unwrap();
    let hot = record.primary().unwrap();
    assert_eq!(hot.checksum.as_deref(), Some(record.checksum.as_str()));

    // A second read is served from the hot copy without recalling again.
    let again = tree
        .recaller("s", RecallPolicy::Promote)
        .recall("shows/moved.mkv")
        .unwrap();
    assert_eq!(
        again,
        RecallOutcome::Hot {
            bytes: tree.hot_path()
        }
    );
}

#[test]
fn a_slow_tier_refuses_inline_recall_and_names_restore() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tree = Tree::build(&second);
    for class in ["min", "hours"] {
        let err = tree
            .recaller(class, RecallPolicy::Promote)
            .recall("shows/moved.mkv")
            .expect_err("a slow tier must refuse");
        assert!(
            matches!(&err, RecallError::Refused { tier, .. } if tier == "archive"),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("`archive`"), "{message}");
        assert!(
            message.contains("just_cache restore shows/moved.mkv"),
            "{message}"
        );
    }
    // Nothing moved and nothing was recorded.
    assert!(!is_regular(&tree.hot_path()));
    assert!(tree.hot_rows().is_empty());
    assert_eq!(tree.state(), "offloaded");
}

#[test]
fn read_through_serves_the_tier_of_record_and_places_nothing() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tree = Tree::build(&second);
    let outcome = tree
        .recaller("s", RecallPolicy::ReadThrough)
        .recall("shows/moved.mkv")
        .unwrap();
    assert!(
        matches!(&outcome, RecallOutcome::ReadThrough { tier, .. } if tier == "archive"),
        "{outcome:?}"
    );
    assert_eq!(fs::read(outcome.bytes()).unwrap(), PAYLOAD);
    assert!(!is_regular(&tree.hot_path()));
    assert!(tree.hot_rows().is_empty());
    assert_eq!(tree.state(), "offloaded");
}

#[test]
fn a_failed_recall_names_the_tier_and_leaves_the_object_not_present() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tree = Tree::build(&second);
    // Same length, different bytes: only the recorded checksum can tell.
    let mut rotten = PAYLOAD.to_vec();
    rotten[0] ^= 0xff;
    fs::write(tree.cold.join("shows/moved.mkv"), &rotten).unwrap();

    let err = tree
        .recaller("s", RecallPolicy::Promote)
        .recall("shows/moved.mkv")
        .expect_err("a copy that fails verification is never served");
    assert!(
        matches!(&err, RecallError::Failed { tier, .. } if tier == "archive"),
        "{err:?}"
    );
    assert!(err.to_string().contains("`archive`"), "{err}");
    assert!(!is_regular(&tree.hot_path()), "nothing was placed");
    assert!(tree.hot_rows().is_empty(), "no location was recorded good");
    assert_eq!(tree.state(), "offloaded");
}

#[test]
fn two_concurrent_reads_place_exactly_one_copy() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tree = Tree::build(&second);
    let copies = Arc::new(AtomicUsize::new(0));
    let hook_copies = Arc::clone(&copies);
    // Hold the recalling reader inside its copy long enough that the other reader's
    // plan certainly runs while no hot copy exists yet. Without the single-flight both
    // readers reach this hook and the count is 2.
    let recaller = Arc::new(
        tree.recaller("s", RecallPolicy::Promote)
            .with_copy_hook(Arc::new(move || {
                hook_copies.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(400));
            })),
    );
    let start = Arc::new(Barrier::new(2));
    let readers: Vec<_> = (0..2)
        .map(|_| {
            let recaller = Arc::clone(&recaller);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                recaller.recall("shows/moved.mkv")
            })
        })
        .collect();
    let outcomes: Vec<RecallOutcome> = readers
        .into_iter()
        .map(|reader| reader.join().unwrap().expect("both reads are served"))
        .collect();

    assert_eq!(
        copies.load(Ordering::SeqCst),
        1,
        "exactly one recall copied"
    );
    let recalled = outcomes
        .iter()
        .filter(|o| matches!(o, RecallOutcome::Recalled { .. }))
        .count();
    assert_eq!(recalled, 1, "{outcomes:?}");
    for outcome in &outcomes {
        assert_eq!(fs::read(outcome.bytes()).unwrap(), PAYLOAD);
    }
    assert_eq!(tree.hot_rows().len(), 1);
    assert!(support::partial_files(&tree.hot).is_empty());
}

#[test]
fn a_recalled_copy_that_reads_back_wrong_is_recorded_unknown_never_good() {
    let Some(second) = support::second_fs() else {
        return;
    };
    let tree = Tree::build(&second);
    let mut catalog = Catalog::open(&tree.catalog).unwrap();
    let object = catalog.object_for_path("shows/moved.mkv").unwrap().unwrap();
    let hot = tree.hot.to_string_lossy().into_owned();
    let wrong = just_cache::bytes_digest(b"not the object").to_hex();
    for found in [Some(wrong.as_str()), None] {
        let good = catalog
            .record_recall(&object, &hot, "shows/moved.mkv", found)
            .unwrap();
        assert!(!good);
        assert_eq!(
            tree.hot_rows(),
            vec![("shows/moved.mkv".to_string(), false, false)],
            "unverified and not the tier of record"
        );
        assert_eq!(tree.state(), "offloaded");
    }
}

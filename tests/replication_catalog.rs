//! The catalog half of verify-before-delete: a copy the mover wrote but could not vouch
//! for is *unknown*, and unknown counts toward no floor.
//!
//! This is the state a crash between "bytes written" and "digest checked" leaves behind.
//! A test cannot time that crash, but it can put the catalog in exactly the state the
//! mover would have left — a location row with `verified = 0` — and then assert that the
//! next sync refuses to believe it.

use std::fs;

use just_cache::catalog::{Catalog, CATALOG_NAME};

/// A location recorded as written-but-unverified must not satisfy a floor, and a sync
/// that cannot find the bytes must say so rather than assume they were good.
#[test]
fn an_unverified_replica_is_reported_as_unknown_and_counts_toward_no_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest).unwrap();
    fs::write(watch.join("a.bin"), b"payload bytes").unwrap();

    let mut catalog = Catalog::open(watch.join(CATALOG_NAME)).unwrap();
    catalog.sync(&watch, std::slice::from_ref(&dest)).unwrap();
    let object_hex = catalog.object_for_path("a.bin").unwrap().unwrap();
    let object = decode_hex(&object_hex);

    // The mover wrote bytes to a destination but never got a digest to vouch for them.
    // (The bytes have to exist for this to be the state a crash would leave; the copy is
    // placed by hand here because `sync` is ingest-only and moves nothing.)
    let tier = fs::canonicalize(&dest)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("a.bin"), b"payload bytes").unwrap();
    assert!(catalog
        .record_replica(&object, &tier, "a.bin", false, None)
        .unwrap());

    let recorded = catalog
        .all_locations()
        .unwrap()
        .into_iter()
        .find(|location| location.tier == tier && location.storage_key == "a.bin")
        .expect("the unverified copy is recorded");
    assert!(!recorded.verified, "written is not the same as vouched for");
    assert_eq!(
        recorded.checksum, None,
        "an unverified copy has no checksum to claim"
    );

    // The bytes are gone; the sync must report the unknown copy rather than believe it.
    fs::remove_file(dest.join("a.bin")).unwrap();
    let report = catalog.sync(&watch, std::slice::from_ref(&dest)).unwrap();
    assert!(
        report
            .differences
            .iter()
            .any(|difference| difference.kind.as_str() == "replica-unknown"),
        "an unverifiable copy must be a finding: {:?}",
        report.differences
    );

    // And the row survives: the catalog is where the answer lives.
    assert!(catalog
        .all_locations()
        .unwrap()
        .iter()
        .any(|location| location.tier == tier && location.storage_key == "a.bin"));
}

/// The floor is a per-tier property recorded once, not inferred from the rows that exist.
#[test]
fn a_tier_floor_is_recorded_and_read_back_per_tier() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog_path = tmp.path().join("catalog.sqlite");
    let catalog = Catalog::open(&catalog_path).unwrap();

    assert_eq!(catalog.tier_floor("/mnt/a").unwrap(), None);
    catalog.set_tier_floor("/mnt/a", 2).unwrap();
    catalog.set_tier_floor("/mnt/b", 3).unwrap();
    assert_eq!(catalog.tier_floor("/mnt/a").unwrap(), Some(2));
    assert_eq!(catalog.tier_floor("/mnt/b").unwrap(), Some(3));

    // Re-recorded, not accumulated: the floor is a property, not a count.
    catalog.set_tier_floor("/mnt/a", 1).unwrap();
    assert_eq!(catalog.tier_floor("/mnt/a").unwrap(), Some(1));
    assert_eq!(catalog.all_tier_floors().unwrap().len(), 2);
}

/// Reopening a catalog the current code wrote must not lose rows or re-create schema:
/// the migration is a no-op on an up-to-date file.
#[test]
fn reopening_a_catalog_keeps_its_rows_and_its_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let dest = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&dest).unwrap();
    fs::write(watch.join("a.bin"), b"payload").unwrap();

    {
        let mut catalog = Catalog::open(watch.join(CATALOG_NAME)).unwrap();
        catalog.sync(&watch, std::slice::from_ref(&dest)).unwrap();
        catalog
            .set_tier_floor(&fs::canonicalize(&dest).unwrap().to_string_lossy(), 2)
            .unwrap();
    }
    let reopened = Catalog::open(watch.join(CATALOG_NAME)).unwrap();
    assert_eq!(reopened.object_count().unwrap(), 1);
    assert_eq!(reopened.all_locations().unwrap().len(), 1);
    assert_eq!(
        reopened
            .tier_floor(&fs::canonicalize(&dest).unwrap().to_string_lossy())
            .unwrap(),
        Some(2)
    );
}

fn decode_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

//! Scrubbing stored copies: read every location back and prove it still holds what the
//! catalog says it does.
//!
//! `docs/design.md` §6: "scrub: background read-through of every location, checksum
//! against catalog; corrupt copy → restore from a good copy (or mark object damaged if
//! none)." §9 names the gap this closes — the mover compares sizes unless it is
//! *adopting* a destination, so a same-size bit flip in an otherwise healthy copy is
//! invisible to `audit`, and the only detection used to be `restore`'s after-the-fact
//! check. Re-reading the bytes and hashing them is the ongoing verification between
//! operations.
//!
//! # What a scrub does, and what it refuses to do
//!
//! For each location the catalog records, the bytes there are hashed (BLAKE3, the one
//! digest the whole tool uses) and compared with the object id — which *is* the checksum
//! (§3), so there is no second copy of the truth to drift out of step. A location whose
//! checksum does not match is corruption, and one of two things happens:
//!
//! * if another location of the same object verifies clean, the corrupt copy is
//!   **repaired** from it, through `restore`'s build-a-private-copy-then-rename path: the
//!   bytes are read back and hashed before the atomic swap, so a torn repair cannot
//!   become the location's content (verify-before-delete, §6);
//! * if no verified copy exists anywhere, the object is **marked damaged** and reported.
//!   Nothing is deleted: the corrupt bytes are the only remaining record of what the file
//!   held, and freeing them is a human's file to make.
//!
//! A location whose file is simply *missing* is reported, not marked damaged: an unmounted
//! tier's bytes may be perfectly fine on the disk that is not plugged in, and calling
//! that "rot" would be a lie. A tier root that does not exist is reported once and its
//! locations are left unchecked for the same reason.
//!
//! # Resuming instead of re-reading
//!
//! The last verification of every location lives in the catalog (`scrub_state`), written
//! per location as the scrub goes. A run that is killed at location 900 of 1000 resumes at
//! 901 rather than hashing the first 899 again — and a location whose content changed
//! under the catalog (its recorded object no longer matches the row) is read again rather
//! than trusted.
//!
//! # The I/O budget
//!
//! `--rate` caps the read throughput so scrubbing a busy media tier does not contend with
//! playback. The throttle is applied inside the single digest read loop
//! (`digest::file_digest_reading`), not with a second hasher: one read loop is what keeps
//! "is this the same file" a single answer across the tool. The budget is an average over
//! the run: after each chunk the limiter sleeps for whatever wall-clock the bytes read so
//! far would have taken, so the run can never outrun the budget but a fast filesystem is
//! not slowed below it.
//!
//! # Sparse files: what a read-through actually costs
//!
//! §9 warned that "hashing a sparse file means materializing its holes". That is true of a
//! *copy* that does not preserve holes, and it is why `disk_management::copy_contents`
//! exists: it clones (FICLONE) or walks `SEEK_DATA`/`SEEK_HOLE` so a sparse 40 GB image
//! does not become 40 GB of blocks, and `restore`/`replace_from_verified` reuse it, so
//! even a scrub *repair* keeps holes as holes.
//!
//! It is **not** true of reading, which is all scrub does to verify. `read(2)` on a hole
//! returns zeroes from the page cache; it does not allocate disk blocks or unshare a
//! reflink extent on ext4/xfs/btrfs/tmpfs. The one real cost is that the zero pages pass
//! through the page cache while the file is being hashed, which is transient and evicted
//! without writeback. `tests/scrub.rs` proves this: it hashes a sparse file and asserts
//! `st_blocks` is unchanged. So the honest answer for scrub on common Linux filesystems is
//! "reads through holes, does not materialize them"; the §9 warning was about the copy
//! path, which was already fixed.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::catalog::{Catalog, CatalogError, MalformedRow, ScrubTarget};
use crate::digest;
use crate::restore;

#[derive(Debug, Error)]
pub enum ScrubError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

/// A wall-clock throttle for the digest read loop.
///
/// Deliberately an average-rate limiter rather than a per-chunk sleep: sleeping a fixed
/// time per chunk makes the budget depend on the chunk size, and `digest`'s chunk size is
/// an implementation detail this module must not have to know. `account` is called with
/// the bytes just read and sleeps for whatever time those bytes were entitled to.
#[derive(Debug)]
pub struct RateLimiter {
    /// Bytes per second allowed, or `None` for unlimited.
    budget: Option<u64>,
    start: Instant,
    consumed: u128,
}

impl RateLimiter {
    /// A limiter for `rate_kib_per_sec` KiB/s, or unlimited when `None`.
    pub fn new(rate_kib_per_sec: Option<u64>) -> Self {
        RateLimiter {
            budget: rate_kib_per_sec.map(|kib| kib.saturating_mul(1024)),
            start: Instant::now(),
            consumed: 0,
        }
    }

    /// No budget: read as fast as the disk allows.
    pub fn unlimited() -> Self {
        Self::new(None)
    }

    /// Account for `bytes` just read, sleeping if the run is ahead of its budget.
    pub fn account(&mut self, bytes: usize) {
        let Some(budget) = self.budget else {
            return;
        };
        // A zero budget would make the target time infinite; `--rate 0` is rejected at the
        // command line, and the guard here keeps a directly-constructed limiter safe.
        if budget == 0 {
            return;
        }
        self.consumed += bytes as u128;
        let target = Duration::from_secs_f64(self.consumed as f64 / budget as f64);
        if let Some(shortfall) = target.checked_sub(self.start.elapsed()) {
            std::thread::sleep(shortfall);
        }
    }
}

/// What reading one location back found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Check {
    /// The bytes hashed to the object the catalog recorded.
    Clean,
    /// Already verified in an earlier run, and the catalog still agrees, so it was not
    /// re-read.
    AlreadyVerified,
    /// The bytes hashed to something else — rot, or a hand edit of the same size.
    Corrupt,
    /// No file at the location.
    Missing,
    /// The location could not be read at all (permissions, I/O error).
    Unreadable(String),
    /// The row was refused before any filesystem call: its tier is not a trusted root, or
    /// its key would escape one. Reported, never touched (issue #72).
    Malformed,
}

/// One repair that a scrub did, or (dry run) would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairRecord {
    /// The corrupt location that was replaced.
    pub path: PathBuf,
    /// The object, hex-encoded, that both copies are supposed to hold.
    pub object: String,
    /// The verified copy the bytes came from.
    pub source: PathBuf,
    pub bytes: u64,
    /// False under `--dry-run`: reported, not written.
    pub applied: bool,
}

/// An object with corrupt bytes and no verified copy to repair from. Never deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DamageRecord {
    pub object: String,
    pub locations: Vec<PathBuf>,
    pub detail: String,
}

/// The whole result of one scrub.
#[derive(Debug, Default)]
pub struct ScrubReport {
    pub catalog: PathBuf,
    pub dry_run: bool,
    /// Every location the catalog records.
    pub locations: usize,
    /// Locations read back and verified clean this run.
    pub verified: usize,
    /// Locations skipped because an earlier run already verified them.
    pub already_verified: usize,
    pub repairs: Vec<RepairRecord>,
    pub damaged: Vec<DamageRecord>,
    pub missing: Vec<PathBuf>,
    pub unreadable: Vec<(PathBuf, String)>,
    /// Catalog rows refused as paths (issue #72): reported, and no filesystem call was
    /// made for them.
    pub malformed: Vec<MalformedRow>,
    /// Tier roots that do not exist, named once each rather than as one missing file per
    /// location underneath them.
    pub skipped_tiers: Vec<String>,
}

impl ScrubReport {
    /// Corruption, a missing copy, or a tier that is not mounted: anything that makes the
    /// stored bytes something less than proven. A repair counts even though it was
    /// resolved, because bitrot is evidence about the tier and the operator has to see
    /// that it happened at least once.
    pub fn has_findings(&self) -> bool {
        !self.repairs.is_empty()
            || !self.damaged.is_empty()
            || !self.missing.is_empty()
            || !self.unreadable.is_empty()
            || !self.malformed.is_empty()
            || !self.skipped_tiers.is_empty()
    }

    /// Only the lines that describe a problem, for `--quiet`.
    pub fn finding_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for tier in &self.skipped_tiers {
            lines.push(format!("  tier not mounted, not checked: {tier}"));
        }
        for row in &self.malformed {
            lines.push(row.describe());
        }
        for repair in &self.repairs {
            let verb = if repair.applied {
                "repaired"
            } else {
                "would repair"
            };
            lines.push(format!(
                "  {verb} {} from verified copy {} ({} bytes, object {})",
                repair.path.display(),
                repair.source.display(),
                repair.bytes,
                repair.object
            ));
        }
        for damage in &self.damaged {
            let locations: Vec<String> = damage
                .locations
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            lines.push(format!(
                "  DAMAGED object {}: {} ({}) — nothing deleted",
                damage.object,
                locations.join(", "),
                damage.detail
            ));
        }
        for path in &self.missing {
            lines.push(format!("  missing: {}", path.display()));
        }
        for (path, detail) in &self.unreadable {
            lines.push(format!("  unreadable: {} ({detail})", path.display()));
        }
        lines
    }

    /// The readable summary: the counts, then every finding.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("scrub: {}", self.catalog.display())];
        lines.push(format!(
            "  locations: {} ({} verified, {} already verified (skipped), {} repaired, {} damaged, {} missing, {} malformed)",
            self.locations,
            self.verified,
            self.already_verified,
            self.repairs.len(),
            self.damaged.len(),
            self.missing.len(),
            self.malformed.len()
        ));
        lines.extend(self.finding_lines());
        if !self.has_findings() {
            lines.push("  no corruption found".to_string());
        }
        if self.dry_run {
            lines.push("dry run: the catalog and every copy were left as they were".to_string());
        }
        lines
    }
}

/// Everything one scrub needs.
pub struct ScrubRequest<'a> {
    pub catalog: &'a Catalog,
    /// Read budget in KiB/s, or `None` for unlimited.
    pub rate_kib_per_sec: Option<u64>,
    /// Report what would be repaired, writing neither the repair nor any scrub state.
    pub dry_run: bool,
}

/// Read through every stored location and verify it against the catalog.
///
/// Repair and damage marking reuse `restore`'s verified-copy machinery and happen through
/// the catalog; this function's own contract is only that it never deletes bytes and never
/// trusts a copy it has not verified.
pub fn scrub(request: &ScrubRequest<'_>) -> Result<ScrubReport, ScrubError> {
    let catalog = request.catalog;
    let targets = catalog.scrub_targets()?;
    // The canonical roots a row's tier must be one of before its key becomes a path. Read
    // once, so a hand-edited tier cannot be joined at all (issue #72).
    let roots = catalog.roots()?;
    let mut limiter = RateLimiter::new(request.rate_kib_per_sec);

    let mut report = ScrubReport {
        catalog: catalog.path().to_path_buf(),
        dry_run: request.dry_run,
        locations: targets.len(),
        ..ScrubReport::default()
    };

    // A tier root that is not there is reported once, not as one missing file per location
    // underneath it: an unmounted disk is a different problem from rot, and its copies may
    // be perfectly intact. A row whose tier is not a trusted root is not even stat'ed here;
    // it is reported as malformed when its object is reached.
    let mut missing_tiers: BTreeSet<String> = BTreeSet::new();
    for target in &targets {
        if target.path(&roots).is_err() {
            continue;
        }
        if !Path::new(&target.tier).is_dir() {
            missing_tiers.insert(target.tier.clone());
        }
    }
    report.skipped_tiers = missing_tiers.iter().cloned().collect();

    // The query orders by object id, so every object's locations are contiguous: repairing
    // a corrupt copy needs its siblings, and this is the grouping that provides them
    // without materialising the whole table.
    let mut index = 0;
    while index < targets.len() {
        let start = index;
        while index < targets.len() && targets[index].object == targets[start].object {
            index += 1;
        }
        scrub_object(
            catalog,
            &targets[start..index],
            &roots,
            &mut limiter,
            request.dry_run,
            &missing_tiers,
            &mut report,
        )?;
    }

    Ok(report)
}

/// Verify every location of one object, then repair the corrupt ones or mark the damage.
fn scrub_object(
    catalog: &Catalog,
    group: &[ScrubTarget],
    roots: &[PathBuf],
    limiter: &mut RateLimiter,
    dry_run: bool,
    missing_tiers: &BTreeSet<String>,
    report: &mut ScrubReport,
) -> Result<(), ScrubError> {
    let Some(expected) = hash_of(&group[0].object) else {
        // A catalog row whose id is not a BLAKE3 digest cannot be verified against. Report
        // it as a catalog problem rather than calling the file corrupt.
        report.unreadable.push((
            PathBuf::from(format!("{}/{}", group[0].tier, group[0].storage_key)),
            format!(
                "catalog object id is {} bytes, not a 32-byte digest",
                group[0].object.len()
            ),
        ));
        return Ok(());
    };
    let object_hex = group[0].object_hex();

    let mut checks: Vec<Check> = Vec::with_capacity(group.len());
    for target in group {
        // Every filesystem call below is reached only after the row has proved it stays
        // under a trusted root; a refused row is reported and skipped untouched (#72).
        let path = match target.path(roots) {
            Ok(path) => path,
            Err(error) => {
                report
                    .malformed
                    .push(MalformedRow::new(&target.tier, &target.storage_key, &error));
                checks.push(Check::Malformed);
                continue;
            }
        };
        if missing_tiers.contains(&target.tier) {
            checks.push(Check::Missing);
            continue;
        }
        if target.is_already_verified() {
            checks.push(Check::AlreadyVerified);
            report.already_verified += 1;
            continue;
        }
        let check = check_location(&path, target.size, &expected, limiter);
        match &check {
            Check::Clean => {
                report.verified += 1;
                if !dry_run {
                    catalog.record_verified(&target.tier, &target.storage_key, &target.object)?;
                }
            }
            Check::Missing => report.missing.push(path),
            Check::Unreadable(detail) => report.unreadable.push((path, detail.clone())),
            Check::AlreadyVerified | Check::Corrupt | Check::Malformed => {}
        }
        checks.push(check);
    }

    // No corruption was read this run, so there is nothing to repair: every
    // already-verified location stays skipped, which is the resume contract. Only a group
    // that actually has rot needs a fallback read (issue #75).
    if !checks.contains(&Check::Corrupt) {
        return Ok(());
    }

    // A repair source is a sibling verified clean. A location cleaned *this run* is trusted
    // directly; one verified in an earlier run is re-read before it is trusted, because the
    // whole point of a scrub is to stop trusting records and start checking bytes — and
    // rot can have reached the sibling since it was last verified.
    //
    // The fallback read is a *real* verdict, not a throwaway: replacing the location's
    // entry with what the read found is what lets a candidate that has since rotted join
    // `corrupt_indices` and be marked damaged, rather than leaving a `scrub_state` row that
    // still claims its bytes were verified — a claim every later scrub would skip without
    // reading (issue #75). Every already-verified candidate is tried in turn, so a clean
    // sibling *later* in the group is still found after an earlier one has rotted; the
    // loop carries on past a failed candidate instead of stopping at the first.
    let mut source = checks.iter().position(|check| *check == Check::Clean);
    if source.is_none() {
        for (index, target) in group.iter().enumerate() {
            if checks[index] != Check::AlreadyVerified {
                continue;
            }
            // Each candidate's path comes from the validating accessor: a row that does not
            // stay under a trusted root is refused rather than read, and is reported as a
            // catalog problem here instead of being silently skipped (#72).
            let path = match target.path(roots) {
                Ok(path) => path,
                Err(error) => {
                    report.malformed.push(MalformedRow::new(
                        &target.tier,
                        &target.storage_key,
                        &error,
                    ));
                    checks[index] = Check::Malformed;
                    continue;
                }
            };
            let check = check_location(&path, target.size, &expected, limiter);
            // This location was counted as skipped during the first pass, but the fallback
            // read did real work. Move it from the skip count into the actual result count.
            report.already_verified -= 1;
            if check == Check::Clean {
                report.verified += 1;
                if !dry_run {
                    catalog.record_verified(&target.tier, &target.storage_key, &target.object)?;
                }
                checks[index] = Check::Clean;
                source = Some(index);
                break;
            }
            match &check {
                Check::Missing => report.missing.push(path),
                Check::Unreadable(detail) => report.unreadable.push((path, detail.clone())),
                // Corruption is the reason to keep looking and to mark it below; the loop
                // simply carries on to the next already-verified candidate.
                Check::Corrupt => {}
                Check::Clean | Check::AlreadyVerified | Check::Malformed => unreachable!(),
            }
            checks[index] = check;
        }
    }

    let corrupt_indices: Vec<usize> = checks
        .iter()
        .enumerate()
        .filter(|(_, check)| **check == Check::Corrupt)
        .map(|(index, _)| index)
        .collect();

    match source {
        Some(source_index) => {
            let Ok(good) = group[source_index].path(roots) else {
                return Ok(());
            };
            for i in corrupt_indices {
                let target = &group[i];
                // A Corrupt location resolved successfully above, so this is the same path
                // that was read; a refused row was never Corrupt.
                let Ok(path) = target.path(roots) else {
                    continue;
                };
                if dry_run {
                    report.repairs.push(RepairRecord {
                        path,
                        object: object_hex.clone(),
                        source: good.clone(),
                        bytes: target.size,
                        applied: false,
                    });
                    continue;
                }
                match restore::replace_from_verified(&good, &path, &expected) {
                    Ok(bytes) => {
                        catalog.record_verified(
                            &target.tier,
                            &target.storage_key,
                            &target.object,
                        )?;
                        report.repairs.push(RepairRecord {
                            path,
                            object: object_hex.clone(),
                            source: good.clone(),
                            bytes,
                            applied: true,
                        });
                    }
                    Err(error) => {
                        // The repair itself failed (a permission problem, a torn copy that
                        // did not hash back). The corrupt bytes are still the only ones, so
                        // the object is marked damaged rather than risked.
                        let detail = format!(
                            "checksum mismatch; repair from {} failed: {error}",
                            good.display()
                        );
                        catalog.mark_damaged(
                            &target.tier,
                            &target.storage_key,
                            &target.object,
                            &detail,
                        )?;
                        report.unreadable.push((path.clone(), detail.clone()));
                        report.damaged.push(DamageRecord {
                            object: object_hex.clone(),
                            locations: vec![path],
                            detail,
                        });
                    }
                }
            }
        }
        None => {
            // Corruption with nothing verified to repair from: mark it, report it, and
            // touch none of the bytes.
            let mut locations = Vec::new();
            for i in corrupt_indices {
                let target = &group[i];
                let Ok(path) = target.path(roots) else {
                    continue;
                };
                locations.push(path);
                if !dry_run {
                    catalog.mark_damaged(
                        &target.tier,
                        &target.storage_key,
                        &target.object,
                        "checksum mismatch; no verified copy to repair from",
                    )?;
                }
            }
            report.damaged.push(DamageRecord {
                object: object_hex,
                locations,
                detail: "checksum mismatch; no verified copy available".to_string(),
            });
        }
    }

    Ok(())
}

/// Read one location back and compare it with the object's checksum.
fn check_location(
    path: &Path,
    size: u64,
    expected: &blake3::Hash,
    limiter: &mut RateLimiter,
) -> Check {
    // `symlink_metadata`, not `metadata`: a symlink at a recorded location is not a copy.
    // `metadata` follows the final component, so a link to a byte-identical target would
    // stat as a regular file, hash clean, and be recorded verified — a name the tool
    // vouches for while the bytes it points at can be removed without the catalog
    // noticing, which is the invariant-6 shape.
    match fs::symlink_metadata(path) {
        Err(_) => Check::Missing,
        Ok(metadata) if !metadata.is_file() => {
            Check::Unreadable(format!("not a regular file: {}", path.display()))
        }
        // A wrong length is a mismatch on its own; there is no need to read a 40 GB file
        // to discover that a 12-byte one is not it. The digest is still the authority for
        // a same-size change, which is exactly the bit rot this exists to catch.
        Ok(metadata) if metadata.len() != size => Check::Corrupt,
        Ok(_) => match digest::file_digest_reading(path, |bytes| limiter.account(bytes)) {
            Ok(found) if found == *expected => Check::Clean,
            Ok(_) => Check::Corrupt,
            Err(error) => Check::Unreadable(error.to_string()),
        },
    }
}

fn hash_of(object: &[u8]) -> Option<blake3::Hash> {
    let bytes: [u8; 32] = object.try_into().ok()?;
    Some(blake3::Hash::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Coarse, honest timing: the limiter is a wall-clock device, so the test asserts the
    /// pace is in the right ballpark rather than an exact sleep.
    #[test]
    fn the_rate_limiter_paces_a_read_to_the_budget() {
        let mut limiter = RateLimiter::new(Some(512)); // 512 KiB/s
        let start = Instant::now();
        // 256 KiB at 512 KiB/s is half a second of budget.
        limiter.account(256 * 1024);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "the limiter must pace ~0.5s for 256 KiB at 512 KiB/s, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the limiter must not oversleep: {elapsed:?}"
        );
    }

    #[test]
    fn an_unlimited_limiter_does_not_sleep() {
        let mut limiter = RateLimiter::unlimited();
        let start = Instant::now();
        limiter.account(512 * 1024 * 1024);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "unlimited must not throttle: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_catalog_object_id_that_is_not_a_digest_is_not_a_hash() {
        assert!(hash_of(&[0u8; 32]).is_some());
        assert!(hash_of(&[0u8; 31]).is_none());
        assert!(hash_of(&[]).is_none());
    }
}

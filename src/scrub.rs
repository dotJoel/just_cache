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
//! # Offline-volume copies (#181)
//!
//! A copy on an `offline` tier has no local representation: its bytes sit on a disk a person
//! inserts, sealed in the [`crate::envelope`], and the object id is the checksum of the
//! *plaintext*. When the ledger says the volume the location's `<volume-id>/<relative>` key
//! names is the one in the drive, the copy is read back, decrypted, and hashed exactly like a
//! local one — and repaired like one, from a verified sibling, by re-exporting that sibling's
//! plaintext as a fresh envelope. When the volume is *not* in the drive (or no `tiers.toml`
//! beside the catalog describes its tier), the copy is reported **unavailable by its volume
//! identity**: never corrupt (nothing was read), never healthy (nothing was proved), and not
//! a missing file (the row names a disk, not a path that has gone). The tier config is the
//! config beside the catalog, as every other command defaults to, so the scrub names no new
//! flag.
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
use crate::envelope::Key;
use crate::offline;
use crate::restore;
use crate::tiers::TierSet;

#[derive(Debug, Error)]
pub enum ScrubError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// A refusal the offline read path raised: a row whose key names no volume, or a ledger
    /// the catalog could not answer from.
    #[error(transparent)]
    Offline(#[from] offline::OfflineError),
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
    /// An `offline` copy whose volume is not in the drive this run, so its bytes could not be
    /// reached. A verdict in its own right: never corrupt (nothing was read), never healthy
    /// (nothing was proved), and not a missing file (the row names a disk, not a path that
    /// has gone). Reported by its volume identity (issue #181).
    Unavailable(String),
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

/// A recorded `offline` copy this run could not reach, named by the volume its key names.
///
/// The scrub keeps three verdicts apart that are easy to blur: a copy that was read and
/// matched (verified), a copy that was read and did not (corrupt), and a copy that was not
/// read at all because the disk holding it is not in the drive (unavailable). Only the first
/// is health and only the second is rot; this is the third, and it is reported by the volume
/// identity the row names rather than as a missing file — an unplugged disk is not a path
/// that has vanished, and calling it either healthy or corrupt would be a claim nothing read
/// can support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableRecord {
    /// The offline tier whose drive the volume belongs in.
    pub tier: String,
    /// The volume the location's storage key names.
    pub volume: String,
    /// The storage key as recorded, `<volume-id>/<relative>`.
    pub storage_key: String,
    /// Why it could not be reached: the volume is not mounted, the tier is not described
    /// beside the catalog, or the envelope key could not be loaded.
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
    /// Recorded `offline` copies whose volume is not in the drive this run, named by their
    /// volume identity (issue #181).
    pub unavailable: Vec<UnavailableRecord>,
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
            || !self.unavailable.is_empty()
            || !self.skipped_tiers.is_empty()
    }

    /// Only the lines that describe a problem, for `--quiet`.
    pub fn finding_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for tier in &self.skipped_tiers {
            lines.push(format!("  tier not mounted, not checked: {tier}"));
        }
        for copy in &self.unavailable {
            lines.push(format!(
                "  unavailable: tier `{}` volume `{}` ({}) — {}",
                copy.tier, copy.volume, copy.storage_key, copy.detail
            ));
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
            "  locations: {} ({} verified, {} already verified (skipped), {} repaired, {} damaged, {} missing, {} malformed, {} unavailable)",
            self.locations,
            self.verified,
            self.already_verified,
            self.repairs.len(),
            self.damaged.len(),
            self.missing.len(),
            self.malformed.len(),
            self.unavailable.len()
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

    /// The scrub report as a self-contained JSON document (#169), hand-rolled like every
    /// other `--json` in this tool. A scrub appends this whatever `--quiet` says, so the
    /// event does not depend on how much stdout showed.
    pub fn to_json(&self) -> String {
        use crate::events::json_string;
        let join = |items: Vec<String>| items.join(",");
        let mut out = String::from("{");
        out.push_str(&format!(
            "\"catalog\":{},",
            json_string(&self.catalog.display().to_string())
        ));
        out.push_str(&format!("\"dry_run\":{},", self.dry_run));
        out.push_str(&format!("\"locations\":{},", self.locations));
        out.push_str(&format!("\"verified\":{},", self.verified));
        out.push_str(&format!("\"already_verified\":{},", self.already_verified));
        out.push_str(&format!("\"has_findings\":{},", self.has_findings()));
        out.push_str(&format!(
            "\"skipped_tiers\":[{}],",
            join(
                self.skipped_tiers
                    .iter()
                    .map(|tier| json_string(tier))
                    .collect()
            )
        ));
        out.push_str(&format!(
            "\"repairs\":[{}],",
            join(
                self.repairs
                    .iter()
                    .map(|repair| format!(
                        "{{\"path\":{},\"source\":{},\"object\":{},\"bytes\":{},\"applied\":{}}}",
                        json_string(&repair.path.display().to_string()),
                        json_string(&repair.source.display().to_string()),
                        json_string(&repair.object),
                        repair.bytes,
                        repair.applied
                    ))
                    .collect()
            )
        ));
        out.push_str(&format!(
            "\"damaged\":[{}],",
            join(
                self.damaged
                    .iter()
                    .map(|damage| format!(
                        "{{\"object\":{},\"locations\":[{}],\"detail\":{}}}",
                        json_string(&damage.object),
                        join(
                            damage
                                .locations
                                .iter()
                                .map(|path| json_string(&path.display().to_string()))
                                .collect()
                        ),
                        json_string(&damage.detail)
                    ))
                    .collect()
            )
        ));
        out.push_str(&format!(
            "\"missing\":[{}],",
            join(
                self.missing
                    .iter()
                    .map(|path| json_string(&path.display().to_string()))
                    .collect()
            )
        ));
        out.push_str(&format!(
            "\"unreadable\":[{}],",
            join(
                self.unreadable
                    .iter()
                    .map(|(path, detail)| format!(
                        "{{\"path\":{},\"detail\":{}}}",
                        json_string(&path.display().to_string()),
                        json_string(detail)
                    ))
                    .collect()
            )
        ));
        out.push_str(&format!(
            "\"malformed\":[{}],",
            join(
                self.malformed
                    .iter()
                    .map(|row| format!(
                        "{{\"tier\":{},\"storage_key\":{},\"detail\":{}}}",
                        json_string(&row.tier),
                        json_string(&row.storage_key),
                        json_string(&row.detail)
                    ))
                    .collect()
            )
        ));
        out.push_str(&format!(
            "\"unavailable\":[{}]",
            join(
                self.unavailable
                    .iter()
                    .map(|copy| format!(
                        "{{\"tier\":{},\"volume\":{},\"storage_key\":{},\"detail\":{}}}",
                        json_string(&copy.tier),
                        json_string(&copy.volume),
                        json_string(&copy.storage_key),
                        json_string(&copy.detail)
                    ))
                    .collect()
            )
        ));
        out.push('}');
        out
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

    // The scrub names no `--tiers`, exactly as it names no `--watch`/`--dest`: an offline
    // tier's `tiers.toml` sits beside the catalog, the placement every command defaults to,
    // and the catalog is the source of truth for which volume is in the drive (§3). A config
    // that is there but broken is not the same as none — it is carried as a refusal so an
    // offline row reports *why* it could not be read, by name.
    let offline = OfflineAccess::beside(catalog);

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
            &offline,
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
///
/// Each location is first resolved to something readable — a filesystem copy, an `offline`
/// copy whose volume is in the drive, or a refusal named for what is known — so the bytes
/// read below always belong to the copy the row names, never to another disk and never to a
/// name that escaped its tier.
#[allow(clippy::too_many_arguments)]
fn scrub_object(
    catalog: &Catalog,
    offline: &OfflineAccess,
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

    // Resolve every location before touching bytes. The verdict "cannot be reached" is
    // reached without a read, and is named by what is known (issue #181) rather than read
    // as a missing file or reported as a failure of the scrub itself.
    let probes: Vec<Probe> = group
        .iter()
        .map(|target| resolve_probe(catalog, offline, target, roots, missing_tiers, report))
        .collect::<Result<_, _>>()?;

    let mut checks: Vec<Check> = Vec::with_capacity(group.len());
    for (index, probe) in probes.iter().enumerate() {
        let target = &group[index];
        match probe {
            Probe::Unavailable { volume, detail } => {
                report.unavailable.push(UnavailableRecord {
                    tier: target.tier.clone(),
                    volume: volume.clone(),
                    storage_key: target.storage_key.clone(),
                    detail: detail.clone(),
                });
                checks.push(Check::Unavailable(detail.clone()));
                continue;
            }
            Probe::Malformed => {
                checks.push(Check::Malformed);
                continue;
            }
            Probe::SkippedTier => {
                // Reported once as a tier (its root is not there), not as a missing file per
                // location underneath it.
                checks.push(Check::Missing);
                continue;
            }
            Probe::File(_) | Probe::Offline { .. } => {}
        }
        if target.is_already_verified() {
            checks.push(Check::AlreadyVerified);
            report.already_verified += 1;
            continue;
        }
        let check = probe.verify(target.size, &expected, limiter, offline);
        match &check {
            Check::Clean => {
                report.verified += 1;
                if !dry_run {
                    catalog.record_verified(&target.tier, &target.storage_key, &target.object)?;
                }
            }
            Check::Missing => report.missing.push(probe.location()),
            Check::Unreadable(detail) => report.unreadable.push((probe.location(), detail.clone())),
            Check::Corrupt | Check::Unavailable(_) | Check::AlreadyVerified | Check::Malformed => {}
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
        for (index, probe) in probes.iter().enumerate() {
            if checks[index] != Check::AlreadyVerified {
                continue;
            }
            // The probe already proved the row is a filesystem copy under a trusted root or
            // an `offline` copy the ledger vouches for; there is no second path to refuse
            // here — an unresolvable or absent row never reached Check::AlreadyVerified.
            let check = probe.verify(group[index].size, &expected, limiter, offline);
            // This location was counted as skipped during the first pass, but the fallback
            // read did real work. Move it from the skip count into the actual result count.
            report.already_verified -= 1;
            if check == Check::Clean {
                report.verified += 1;
                if !dry_run {
                    catalog.record_verified(
                        &group[index].tier,
                        &group[index].storage_key,
                        &group[index].object,
                    )?;
                }
                checks[index] = Check::Clean;
                source = Some(index);
                break;
            }
            match &check {
                Check::Missing => report.missing.push(probe.location()),
                Check::Unreadable(detail) => {
                    report.unreadable.push((probe.location(), detail.clone()))
                }
                // Corruption is the reason to keep looking and to mark it below; the loop
                // simply carries on to the next already-verified candidate.
                Check::Corrupt => {}
                Check::Clean
                | Check::AlreadyVerified
                | Check::Malformed
                | Check::Unavailable(_) => {
                    unreachable!()
                }
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
            // The verified source's plaintext: a filesystem copy *is* plaintext; an `offline`
            // copy is decrypted to a local scratch sibling and removed once the repairs are
            // done. A source that cannot be materialized (a sealed copy that has since become
            // unreadable) is treated as no source at all, so nothing is repaired from bytes
            // that were not re-read.
            match probes[source_index].plaintext(&expected, offline) {
                Some(source) => {
                    for i in corrupt_indices {
                        let probe = &probes[i];
                        let target = &group[i];
                        let destination = probe.location();
                        if dry_run {
                            report.repairs.push(RepairRecord {
                                path: destination,
                                object: object_hex.clone(),
                                source: source.display.clone(),
                                bytes: target.size,
                                applied: false,
                            });
                            continue;
                        }
                        match probe.repair_from(&source.path, &expected, offline) {
                            Ok(bytes) => {
                                catalog.record_verified(
                                    &target.tier,
                                    &target.storage_key,
                                    &target.object,
                                )?;
                                report.repairs.push(RepairRecord {
                                    path: destination,
                                    object: object_hex.clone(),
                                    source: source.display.clone(),
                                    bytes,
                                    applied: true,
                                });
                            }
                            Err(error) => {
                                // The repair itself failed (a permission problem, a torn copy
                                // that did not hash back). The corrupt bytes are still the
                                // only ones, so the object is marked damaged rather than
                                // risked.
                                let detail = format!(
                                    "checksum mismatch; repair from {} failed: {error}",
                                    source.display.display()
                                );
                                catalog.mark_damaged(
                                    &target.tier,
                                    &target.storage_key,
                                    &target.object,
                                    &detail,
                                )?;
                                report
                                    .unreadable
                                    .push((destination.clone(), detail.clone()));
                                report.damaged.push(DamageRecord {
                                    object: object_hex.clone(),
                                    locations: vec![destination],
                                    detail,
                                });
                            }
                        }
                    }
                    source.cleanup();
                }
                None => {
                    mark_corrupt_damaged(
                        catalog,
                        group,
                        &probes,
                        &corrupt_indices,
                        dry_run,
                        &object_hex,
                        "checksum mismatch; the verified repair source could not be re-read",
                        report,
                    )?;
                }
            }
        }
        None => {
            // Corruption with nothing verified to repair from: mark it, report it, and
            // touch none of the bytes.
            mark_corrupt_damaged(
                catalog,
                group,
                &probes,
                &corrupt_indices,
                dry_run,
                &object_hex,
                "checksum mismatch; no verified copy to repair from",
                report,
            )?;
        }
    }

    Ok(())
}

/// Mark every corrupt location of one object damaged, leaving the bytes where they are.
#[allow(clippy::too_many_arguments)]
fn mark_corrupt_damaged(
    catalog: &Catalog,
    group: &[ScrubTarget],
    probes: &[Probe],
    corrupt_indices: &[usize],
    dry_run: bool,
    object_hex: &str,
    detail: &str,
    report: &mut ScrubReport,
) -> Result<(), ScrubError> {
    let mut locations = Vec::new();
    for &i in corrupt_indices {
        let target = &group[i];
        let path = probes[i].location();
        locations.push(path);
        if !dry_run {
            catalog.mark_damaged(&target.tier, &target.storage_key, &target.object, detail)?;
        }
    }
    report.damaged.push(DamageRecord {
        object: object_hex.to_string(),
        locations,
        detail: detail.to_string(),
    });
    Ok(())
}

/// What a scrub can reach beside the catalog to check an `offline` tier's copies: the tier
/// config (open-if-present, the placement `catalog sync` and every command default to), why
/// that config could not be read when it is there but broken, and a local scratch directory
/// for decrypting a copy so its plaintext can be hashed.
///
/// The scratch sits beside the catalog — never on the volume — so plaintext never lands on a
/// disk that leaves the machine, which is the whole reason an `offline` copy is sealed.
struct OfflineAccess {
    tiers: Option<TierSet>,
    /// Why the config beside the catalog could not be read, when one is there but broken.
    load_error: Option<String>,
    /// A local directory for `.just_cache-partial-*` scratch files. Beside the catalog,
    /// which the scrub already writes `scrub_state` to, so it is writable by construction.
    scratch: PathBuf,
}

impl OfflineAccess {
    /// Load the tier config beside the catalog, open-if-present. A config that is there but
    /// malformed is carried as `load_error`, not swallowed: an `offline` row then reports
    /// that it could not be read and why.
    fn beside(catalog: &Catalog) -> Self {
        let dir = catalog
            .path()
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let (tiers, load_error) = match TierSet::load_beside(&dir) {
            Ok(set) => (set, None),
            Err(error) => (None, Some(error.to_string())),
        };
        OfflineAccess {
            tiers,
            load_error,
            scratch: dir,
        }
    }

    /// A fresh scratch path for a decrypt, under the local directory (never the volume).
    fn scratch_path(&self) -> PathBuf {
        crate::disk_management::partial_sibling(&self.scratch)
    }
}

/// One location of an object, resolved to something a scrub can read — or to the refusal
/// that says why it cannot.
enum Probe {
    /// A filesystem location: plaintext bytes at this path, under a trusted root.
    File(PathBuf),
    /// An `offline` copy: the sealed envelope at `stored`, opened with `key`, on a volume
    /// the ledger records mounted.
    Offline { stored: PathBuf, key: Key },
    /// A filesystem location whose tier root is a trusted root but is not a directory:
    /// reported once as a tier, and left unchecked rather than called missing here.
    SkippedTier,
    /// An `offline` copy whose volume is not in the drive this run (or whose tier is not
    /// described beside the catalog), refused by name.
    Unavailable { volume: String, detail: String },
    /// A row refused before any filesystem call: its tier is neither a trusted root nor a
    /// volume in the ledger, or its key would escape one.
    Malformed,
}

impl Probe {
    /// The path a report names for this location: the file for a filesystem copy, the sealed
    /// file for an offline one. Empty only for a row that never reached a read.
    fn location(&self) -> PathBuf {
        match self {
            Probe::File(path) => path.clone(),
            Probe::Offline { stored, .. } => stored.clone(),
            Probe::SkippedTier | Probe::Unavailable { .. } | Probe::Malformed => PathBuf::new(),
        }
    }

    /// Read the copy back and compare it with the object's checksum.
    fn verify(
        &self,
        size: u64,
        expected: &blake3::Hash,
        limiter: &mut RateLimiter,
        offline: &OfflineAccess,
    ) -> Check {
        match self {
            Probe::File(path) => check_location(path, size, expected, limiter),
            Probe::Offline { stored, key, .. } => check_sealed_copy(key, stored, expected, offline),
            // A verdict reached without a read is never re-read here.
            Probe::SkippedTier | Probe::Unavailable { .. } | Probe::Malformed => Check::Malformed,
        }
    }

    /// Write a fresh copy of the verified source's plaintext over this location, and (for a
    /// sealed copy) read it back before trusting it. Returns the plaintext length written.
    fn repair_from(
        &self,
        source: &Path,
        expected: &blake3::Hash,
        offline: &OfflineAccess,
    ) -> Result<u64, String> {
        match self {
            Probe::File(path) => restore::replace_from_verified(source, path, expected)
                .map_err(|error| error.to_string()),
            Probe::Offline { stored, key, .. } => {
                // A repair of a sealed copy is a fresh export of the verified plaintext,
                // then a read-back decrypt+hash so a torn rewrite cannot become the copy
                // (§6) — the same verify-before-delete rule the mover runs. The read-back
                // scratch is local, never on the volume.
                offline::export(key, source, stored).map_err(|error| error.to_string())?;
                let scratch = offline.scratch_path();
                let _ = fs::remove_file(&scratch);
                let read = offline::read_verify_decrypt(key, stored, Some(expected), &scratch);
                let _ = fs::remove_file(&scratch);
                read.map_err(|error| error.to_string())
            }
            Probe::SkippedTier | Probe::Unavailable { .. } | Probe::Malformed => {
                Err("the location has no reachable copy to repair".to_string())
            }
        }
    }

    /// This location's bytes as a plaintext file a repair can read: the copy itself for a
    /// filesystem location, a decrypted local scratch sibling for an offline one. `None` when
    /// the plaintext could not be produced (a sealed copy that no longer reads back).
    fn plaintext(
        &self,
        expected: &blake3::Hash,
        offline: &OfflineAccess,
    ) -> Option<PlaintextSource> {
        match self {
            Probe::File(path) => Some(PlaintextSource {
                path: path.clone(),
                display: path.clone(),
                scratch: None,
            }),
            Probe::Offline { stored, key, .. } => {
                let scratch = offline.scratch_path();
                let _ = fs::remove_file(&scratch);
                match offline::read_verify_decrypt(key, stored, Some(expected), &scratch) {
                    Ok(_) => Some(PlaintextSource {
                        path: scratch.clone(),
                        display: stored.clone(),
                        scratch: Some(scratch),
                    }),
                    Err(_) => {
                        let _ = fs::remove_file(&scratch);
                        None
                    }
                }
            }
            Probe::SkippedTier | Probe::Unavailable { .. } | Probe::Malformed => None,
        }
    }
}

/// A verified copy's plaintext, as a file a repair reads. `display` is what the report names
/// (the copy's own location) while `path` is where the plaintext actually sits, which for an
/// offline source is a local scratch sibling rather than the sealed file.
struct PlaintextSource {
    path: PathBuf,
    display: PathBuf,
    /// A scratch file to remove once the repairs are done, when one was made.
    scratch: Option<PathBuf>,
}

impl PlaintextSource {
    fn cleanup(&self) {
        if let Some(path) = &self.scratch {
            let _ = fs::remove_file(path);
        }
    }
}

/// Resolve one recorded location to something readable, or to the refusal that names why it
/// is not.
///
/// A tier that is one of the catalog's recorded roots is a filesystem location, exactly as
/// before (`target.path` refuses a key that would escape it). A tier that is *not* a root and
/// whose storage key names a volume in the ledger is an `offline` copy (#144's rule): it
/// resolves to `<mount>/<relative>` only when the ledger says the same volume is mounted, and
/// otherwise is reported unavailable by its volume identity.
fn resolve_probe(
    catalog: &Catalog,
    offline: &OfflineAccess,
    target: &ScrubTarget,
    roots: &[PathBuf],
    missing_tiers: &BTreeSet<String>,
    report: &mut ScrubReport,
) -> Result<Probe, ScrubError> {
    let is_root = roots
        .iter()
        .any(|root| root.as_path() == Path::new(&target.tier));
    if !is_root {
        if let Some(probe) = offline_probe(catalog, offline, target)? {
            return Ok(probe);
        }
    }
    match target.path(roots) {
        Ok(path) => {
            if missing_tiers.contains(&target.tier) {
                Ok(Probe::SkippedTier)
            } else {
                Ok(Probe::File(path))
            }
        }
        Err(error) => {
            report
                .malformed
                .push(MalformedRow::new(&target.tier, &target.storage_key, &error));
            Ok(Probe::Malformed)
        }
    }
}

/// A probe for an `offline` location, or `None` when the row is not one (its tier is not a
/// root and its storage key names no volume in the ledger) — the caller then falls back to
/// the filesystem refusal, exactly as before #181.
fn offline_probe(
    catalog: &Catalog,
    offline: &OfflineAccess,
    target: &ScrubTarget,
) -> Result<Option<Probe>, ScrubError> {
    let Some((volume_id, _relative)) = offline::parse_storage_key(&target.storage_key) else {
        return Ok(None);
    };
    // The ledger vouches for the volume id; that is what makes a `<id>/<path>` key an
    // `offline` location rather than a filesystem key (the rule `catalog delete` applies).
    if catalog.volume(&volume_id)?.is_none() {
        return Ok(None);
    }
    let unavailable = |detail: String| {
        Ok(Some(Probe::Unavailable {
            volume: volume_id.clone(),
            detail,
        }))
    };

    let Some(set) = &offline.tiers else {
        return unavailable(match &offline.load_error {
            Some(error) => format!("cannot read the tiers.toml beside the catalog: {error}"),
            None => format!(
                "tier `{}` is offline and no tiers.toml beside the catalog describes its \
                 mount; place one and insert volume `{volume_id}` so the copy can be checked",
                target.tier
            ),
        });
    };
    let Some(tier) = set.get(&target.tier) else {
        return unavailable(format!(
            "tier `{}` is offline and no tier of that name is in the tiers.toml beside the \
             catalog",
            target.tier
        ));
    };
    let Some(config) = tier.offline_config.as_ref() else {
        return unavailable(format!(
            "tier `{}` is not an offline tier, so its storage key `{}` names no volume",
            target.tier, target.storage_key
        ));
    };
    let mount = &tier.path;

    // The catalog is the source of truth for which volume is in the drive (§3): the
    // filesystem cannot be asked whether a *different* disk is mounted in its place. An
    // absent (or wrong) volume is named by its identity — never called corrupt, never
    // called healthy, and not a generic missing file.
    let mounted = catalog.mounted_volume_for_tier(&target.tier)?;
    let mounted_id = mounted.as_ref().map(|row| row.id.as_str());
    if mounted_id != Some(volume_id.as_str()) {
        return unavailable(offline::insert_prompt(
            &target.tier,
            mount,
            config.vaults.as_slice(),
            &volume_id,
            mounted_id,
        ));
    }
    // The ledger says this volume is mounted, so the shared resolver joins the mount.
    let Some(stored) =
        offline::resolve_mounted_location(catalog, &target.tier, &target.storage_key, mount)?
    else {
        return unavailable(format!(
            "storage key `{}` does not resolve to a path under {}",
            target.storage_key,
            mount.display()
        ));
    };
    if !mount.is_dir() {
        // The ledger records the disk in the drive, but the mount point is not there: the
        // disk is out, or was not mounted. Name the volume and where to find it.
        return unavailable(format!(
            "volume `{volume_id}` is recorded mounted in tier `{}` but {} is not there; \
             insert it and run the next pass",
            target.tier,
            mount.display()
        ));
    }
    // The configuration supplies the envelope key (§2 rule 2); without it the copy cannot be
    // opened, and that is reported by name rather than guessed at.
    let key = match config.load_encryption_key() {
        Ok(key) => key,
        Err(error) => {
            return unavailable(format!(
                "cannot load the envelope key for tier `{}`: {error}",
                target.tier
            ))
        }
    };
    Ok(Some(Probe::Offline { stored, key }))
}

/// Read a sealed `offline` copy back, decrypt it, and compare the plaintext with the object.
///
/// The recorded digest is the plaintext BLAKE3, so a sound volume verifies exactly like a
/// local copy and "cannot decrypt" never reads as bitrot. `--rate` does not pace this read:
/// the envelope reader decrypts in its own loop and offers no hook for the limiter, so the
/// budget bounds filesystem reads only (named in docs/design.md §9).
fn check_sealed_copy(
    key: &Key,
    stored: &Path,
    expected: &blake3::Hash,
    offline: &OfflineAccess,
) -> Check {
    match fs::symlink_metadata(stored) {
        Err(_) => return Check::Missing,
        Ok(metadata) if !metadata.is_file() => {
            return Check::Unreadable(format!("not a regular file: {}", stored.display()))
        }
        Ok(_) => {}
    }
    let scratch = offline.scratch_path();
    let _ = fs::remove_file(&scratch);
    let result = offline::read_verify_decrypt(key, stored, Some(expected), &scratch);
    let _ = fs::remove_file(&scratch);
    match result {
        Ok(_) => Check::Clean,
        // A sealed copy that decrypts but hashes to something else: rot that kept the
        // envelope intact (a stale or hand-swapped plaintext), not a broken envelope.
        Err(offline::OfflineError::ChecksumMismatch { .. }) => Check::Corrupt,
        Err(error) => Check::Unreadable(error.to_string()),
    }
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

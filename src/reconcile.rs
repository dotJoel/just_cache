//! Reconciling a re-added disk: rebuild the copies that went missing while it was out.
//!
//! `docs/design.md` §6: "a missing copy is a repair job, reported, not silent." Replication
//! (`sweep --copies N`) places verified copies and [retires the source only when the floor
//! is met](crate::replication); the catalog records a floor per tier and reports an object
//! below it. What none of that did was *act* on a floor that went unmet because a disk was
//! away: an object offloaded while `/mnt/disk-b` was unmounted stayed below its floor
//! forever, with `under-replicated` / `replica-lost` as the only symptom, until a human
//! noticed and ran a fresh sweep with the disk present. This module is the missing hands.
//!
//! # Why this is its own command, not a phase of `sweep` or a mode of `scrub`
//!
//! * **Not a sweep phase.** A sweep decides *what to offload next* by walking the watched
//!   tree and applying policy. Reconcile decides *which recorded copy is absent* and never
//!   looks at the tree at all: the catalog already knows every tier, and the absence of a
//!   copy is a catalog fact. Hanging it off a sweep would make a policy run quietly write
//!   gigabytes and, worse, invite it to move files that policy had not selected.
//! * **Not `scrub`.** A scrub reads back every copy that is *present* to find rot; it is
//!   rate-limited precisely because it is a heavy read pass, and the issue that asked for
//!   reconciliation says ongoing verification of present copies is out of scope. Folding a
//!   writer into that reader would make `scrub --rate` silently place copies, and would
//!   leave no command that answers just "what is missing".
//! * **Not `audit`.** An audit is report-only by design, and its catalog-mode `--repair`
//!   deliberately marks findings for resync without touching rows or bytes: a disagreement
//!   between catalog and tree is a human's call. Rebuilding is a *mutation* with a
//!   verified-before-recording contract, which belongs in its own command with its own
//!   exit code.
//!
//! So: `just_cache reconcile --catalog <FILE>`, its own pass, its own report.
//!
//! # What it does, exactly
//!
//! The catalog's `tier` table holds the recorded floor per destination root; the objects it
//! holds are the source of truth for where copies belong. For every object that is
//! offloaded (a hot copy is not relying on its replicas — the same rule `catalog sync`'s
//! under-replication check applies) and below its floor:
//!
//! 1. every recorded location is `stat`ed. That is deliberately **not** verification — one
//!    stat per location, no bytes read — it answers the single question this pass exists
//!    for: is the copy *absent*? A copy that is present, however suspect, is scrub's
//!    business.
//! 2. a missing copy is rebuilt at its recorded `(tier, storage_key)`; a floor-recorded
//!    tier the object has no row for at all — the disk that was out during the sweep — is
//!    filled at the mirrored key a surviving copy holds, because that is where the layout
//!    says it belongs.
//! 3. the rebuild source is a sibling whose bytes are *hashed and compared to the object's
//!    recorded checksum* before a byte is copied. A sibling that only matches on size is
//!    not a source; a sibling that hashes to something else is marked damaged and skipped.
//!    The row's `verified` flag is never enough on its own — the whole point is not to
//!    trust records, and a mark-damaged sibling is re-read rather than assumed either way.
//! 4. the copy is built through [`crate::restore::build_verified_copy`]: bytes go to a
//!    private `.just_cache-partial-*` sibling, are read back and hashed there, and only a
//!    verified copy is renamed into place. A failure leaves the location *absent*, not
//!    holding bytes nobody can vouch for.
//! 5. only then is the location recorded, via [`Catalog::record_replica`] with the real
//!    checksum and `verified = true`.
//!
//! # What it will never do
//!
//! Nothing is deleted — not the source of a rebuild, not a same-size stranger that happens
//! to sit where the copy belongs, not even our own output (see above: it is never
//! published). A destination root is never created (invariant 1): a disk that is still
//! unmounted is reported, not turned into a directory on whatever filesystem its path now
//! falls through to. And with no catalog there is no object identity to rebuild *from*, so
//! a missing catalog is a usage error rather than a pass that invents one.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::catalog::{
    Catalog, CatalogError, LocationRecord, ReconcileObject, STATE_PRESENT, STATE_RESTORING,
};
use crate::digest;
use crate::restore;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

/// What one rebuild attempt at one destination did, or could not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// A missing copy was rebuilt from a verified sibling. `applied` is false under
    /// `--dry-run`, where the source was still resolved (and hashed) but nothing written.
    Rebuilt {
        source: PathBuf,
        bytes: u64,
        applied: bool,
    },
    /// The destination already held the object — a copy restored by hand — so it was
    /// hashed, adopted, and recorded verified without rewriting a byte.
    Adopted,
    /// Something is at the destination that is not the object, or is not a regular file.
    /// Left exactly as it was: a same-size stranger is the case a length-only check gets
    /// wrong, and only the catalog may say what an existing file is.
    Conflict { detail: String },
    /// The destination root is not a directory — the disk is still out. Nothing was
    /// created (invariant 1).
    TierUnavailable,
    /// No sibling verified against the recorded checksum, so the object stays
    /// under-replicated and is reported; nothing was copied and nothing deleted.
    NoSource { detail: String },
    /// The rebuild itself failed (permissions, a full disk, a torn read-back).
    Failed { detail: String },
}

impl ReconcileOutcome {
    /// The counts line's verb for this outcome.
    fn kind(&self) -> &'static str {
        match self {
            ReconcileOutcome::Rebuilt { .. } => "rebuilt",
            ReconcileOutcome::Adopted => "adopted",
            ReconcileOutcome::Conflict { .. } => "conflict",
            ReconcileOutcome::TierUnavailable => "tier-unavailable",
            ReconcileOutcome::NoSource { .. } => "no-source",
            ReconcileOutcome::Failed { .. } => "failed",
        }
    }
}

/// One thing the pass did, or refused to do, at one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileRecord {
    /// The object, hex-encoded, the record is about.
    pub object: String,
    /// Where the rebuilt copy belongs (or, for a tier that is gone, its root).
    pub path: PathBuf,
    pub outcome: ReconcileOutcome,
}

impl ReconcileRecord {
    /// One human-readable line, in the voice of the rest of the tool's output.
    pub fn describe(&self) -> String {
        match &self.outcome {
            ReconcileOutcome::Rebuilt {
                source,
                bytes,
                applied,
            } => {
                let verb = if *applied { "rebuilt" } else { "would rebuild" };
                format!(
                    "  {verb} {} from verified sibling {} ({} bytes, object {})",
                    self.path.display(),
                    source.display(),
                    bytes,
                    self.object
                )
            }
            ReconcileOutcome::Adopted => format!(
                "  adopted {} (already the object; recorded verified, nothing rewritten)",
                self.path.display()
            ),
            ReconcileOutcome::Conflict { detail } => {
                format!("  left alone {}: {detail}", self.path.display())
            }
            ReconcileOutcome::TierUnavailable => {
                format!("  tier not mounted, not rebuilt: {}", self.path.display())
            }
            ReconcileOutcome::NoSource { detail } => format!(
                "  left under-replicated {} (object {}): {detail}",
                self.path.display(),
                self.object
            ),
            ReconcileOutcome::Failed { detail } => format!(
                "  FAILED to rebuild {} (object {}): {detail}",
                self.path.display(),
                self.object
            ),
        }
    }
}

/// The whole result of one reconcile.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub catalog: PathBuf,
    pub dry_run: bool,
    /// Objects the pass considered.
    pub objects: usize,
    /// Destination tiers with a recorded floor, so an empty pass can say why it did
    /// nothing rather than look like a failure.
    pub tiers: usize,
    /// Every rebuild, adoption, refusal and failure, in object order.
    pub records: Vec<ReconcileRecord>,
}

impl ReconcileReport {
    /// A rebuild that happened is still a finding: a disk was out, and the operator has to
    /// be able to see that from cron. This is the same call `scrub` makes about a repair.
    pub fn has_findings(&self) -> bool {
        !self.records.is_empty()
    }

    /// Only the lines that describe something that happened or could not be done.
    pub fn finding_lines(&self) -> Vec<String> {
        self.records.iter().map(ReconcileRecord::describe).collect()
    }

    /// The readable summary: the counts, then every record.
    pub fn summary_lines(&self) -> Vec<String> {
        let count = |kind: &str| {
            self.records
                .iter()
                .filter(|r| r.outcome.kind() == kind)
                .count()
        };
        let mut lines = vec![format!("reconcile: {}", self.catalog.display())];
        lines.push(format!(
            "  objects: {} ({} destination tier(s) with a recorded floor); rebuilt: {}, adopted: {}, conflicts: {}, tiers unavailable: {}, no verified sibling: {}, failed: {}",
            self.objects,
            self.tiers,
            count("rebuilt"),
            count("adopted"),
            count("conflict"),
            count("tier-unavailable"),
            count("no-source"),
            count("failed")
        ));
        lines.extend(self.finding_lines());
        if self.records.is_empty() {
            lines.push("  every recorded copy floor is met or unreconcilable".to_string());
        }
        if self.dry_run {
            lines.push("dry run: the catalog and every copy were left as they were".to_string());
        }
        lines
    }
}

/// Everything one reconcile needs. Like `scrub`, it names no `--watch`/`--dest`: the
/// catalog already holds every tier root and the floor recorded for it.
pub struct ReconcileRequest<'a> {
    pub catalog: &'a Catalog,
    /// Resolve and report what would be rebuilt, writing neither a copy nor any row.
    pub dry_run: bool,
}

/// Rebuild every missing replica that a surviving verified sibling can supply.
///
/// The contract, inherited from the mover and the scrubber: nothing is deleted, a
/// destination root is never created, and no copy is recorded as good until its own bytes
/// have been hashed against the object's recorded checksum.
pub fn reconcile(request: &ReconcileRequest<'_>) -> Result<ReconcileReport, ReconcileError> {
    let catalog = request.catalog;
    let floors = catalog.all_tier_floors()?;
    let mut report = ReconcileReport {
        catalog: catalog.path().to_path_buf(),
        dry_run: request.dry_run,
        objects: catalog.object_count()?,
        tiers: floors.len(),
        records: Vec::new(),
    };
    // No recorded floor means no durability contract to maintain: a plain `catalog sync`
    // never writes one, so this is the "replication is not in use here" case, not an error.
    if floors.is_empty() {
        return Ok(report);
    }

    for object in catalog.reconcile_objects()? {
        reconcile_object(catalog, &object, &floors, request.dry_run, &mut report)?;
    }

    Ok(report)
}

/// Bring one object back to its floor, if it is below it and anything is missing.
fn reconcile_object(
    catalog: &Catalog,
    object: &ReconcileObject,
    floors: &[(String, usize)],
    dry_run: bool,
    report: &mut ReconcileReport,
) -> Result<(), ReconcileError> {
    let object_hex = hex(&object.object);

    // A hot copy is not relying on its replicas, so replication does not apply yet. This
    // is the same skip `catalog sync`'s under-replication check makes, and it is why a
    // fresh tree with a floor recorded but nothing offloaded reports nothing.
    if object.state == STATE_PRESENT || object.state == STATE_RESTORING {
        return Ok(());
    }

    // Without a recorded checksum there is nothing to hash a sibling *against*, so a
    // rebuild would be a guess. Report the catalog problem and touch nothing.
    let Some(expected) = digest_of(&object.checksum) else {
        report.records.push(ReconcileRecord {
            object: object_hex,
            path: PathBuf::new(),
            outcome: ReconcileOutcome::Failed {
                detail: format!(
                    "catalog checksum is {} bytes, not a 32-byte digest",
                    object.checksum.len()
                ),
            },
        });
        return Ok(());
    };

    let floor_tiers: BTreeSet<&str> = floors.iter().map(|(tier, _)| tier.as_str()).collect();
    let expected_floor = floors.iter().map(|(_, copies)| *copies).max().unwrap_or(1);

    // What is really on the disks, from one stat per recorded location. Not verification:
    // "is the copy absent" is the only question this pass asks, and reading every present
    // copy is exactly the heavy work the issue keeps out of scope.
    let mut present_tiers: BTreeSet<String> = BTreeSet::new();
    let mut present_keys: BTreeSet<String> = BTreeSet::new();
    let mut missing_rows: Vec<&LocationRecord> = Vec::new();
    for location in &object.locations {
        let path = Path::new(&location.tier).join(&location.storage_key);
        if fs::symlink_metadata(&path)
            .map(|m| m.is_file())
            .unwrap_or(false)
        {
            present_tiers.insert(location.tier.clone());
            present_keys.insert(location.storage_key.clone());
        } else {
            missing_rows.push(location);
        }
    }

    // At or above the floor: the disk loss this pass exists to heal did not drop the
    // object below its contract, so there is nothing to reconcile. A lost *extra* copy is
    // a scrub finding, not a rebuild.
    if present_tiers.len() >= expected_floor {
        return Ok(());
    }
    let mut present = present_tiers.len();

    // Where a copy should go: the recorded key of every missing row on a floor tier, plus
    // every sibling key on a floor tier the object has no row for at all (the disk that
    // was out during the sweep). Keys come from copies that are *present* when there are
    // any, so a rebuild never invents a namespace path that no surviving copy holds.
    let mut targets: BTreeSet<(String, String)> = BTreeSet::new();
    for location in &missing_rows {
        if floor_tiers.contains(location.tier.as_str()) {
            targets.insert((location.tier.clone(), location.storage_key.clone()));
        }
    }
    let template_keys: BTreeSet<String> = if present_keys.is_empty() {
        object
            .locations
            .iter()
            .map(|location| location.storage_key.clone())
            .collect()
    } else {
        present_keys
    };
    for (tier, _) in floors {
        if !object
            .locations
            .iter()
            .any(|location| location.tier == *tier)
        {
            for key in &template_keys {
                targets.insert((tier.clone(), key.clone()));
            }
        }
    }

    // No location at all to write to: the object is below its floor but there is no
    // missing destination root to rebuild onto (or no key to derive one from), which is a
    // reportable dead end rather than silence.
    if targets.is_empty() {
        report.records.push(ReconcileRecord {
            object: object_hex,
            path: PathBuf::new(),
            outcome: ReconcileOutcome::NoSource {
                detail: "below its floor, but no missing destination root to rebuild onto and no sibling key to derive one from".to_string(),
            },
        });
        return Ok(());
    }

    for (tier, key) in targets {
        // Stop as soon as the floor is met, exactly as a replicated sweep stops placing
        // copies: extra replicas are allowed but not this pass's job to create.
        if present >= expected_floor {
            break;
        }
        let root = Path::new(&tier);
        let dest = root.join(&key);

        // Invariant 1: a destination root is never created. A disk that is still unmounted
        // is reported, not silently turned into a directory on the wrong filesystem.
        if !root.is_dir() {
            report.records.push(ReconcileRecord {
                object: object_hex.clone(),
                path: dest,
                outcome: ReconcileOutcome::TierUnavailable,
            });
            continue;
        }

        match fs::symlink_metadata(&dest) {
            // Something is already there. Hash it: only the catalog's recorded checksum
            // can tell the object (adopt it, rewrite nothing) from a same-size stranger
            // (refuse it, touch nothing).
            Ok(metadata) if metadata.is_file() => match digest::file_digest(&dest) {
                Ok(found) if found == expected => {
                    if !dry_run {
                        catalog.record_replica(
                            &object.object,
                            &tier,
                            &key,
                            true,
                            Some(&object.checksum),
                        )?;
                    }
                    report.records.push(ReconcileRecord {
                        object: object_hex.clone(),
                        path: dest,
                        outcome: ReconcileOutcome::Adopted,
                    });
                    present += 1;
                }
                Ok(found) => report.records.push(ReconcileRecord {
                    object: object_hex.clone(),
                    path: dest,
                    outcome: ReconcileOutcome::Conflict {
                        detail: format!(
                            "holds a {}-byte file hashing to {} (recorded {}); left untouched",
                            metadata.len(),
                            found.to_hex(),
                            expected.to_hex()
                        ),
                    },
                }),
                Err(error) => report.records.push(ReconcileRecord {
                    object: object_hex.clone(),
                    path: dest.clone(),
                    outcome: ReconcileOutcome::Failed {
                        detail: format!(
                            "cannot read the existing file at {}: {error}",
                            dest.display()
                        ),
                    },
                }),
            },
            Ok(_) => report.records.push(ReconcileRecord {
                object: object_hex.clone(),
                path: dest,
                outcome: ReconcileOutcome::Conflict {
                    detail:
                        "something that is not a regular file is recorded as a copy; left untouched"
                            .to_string(),
                },
            }),
            // Absent: this is the copy to rebuild.
            Err(_) => match resolve_source(catalog, object, &expected, dry_run)? {
                SourceResolution::None(detail) => report.records.push(ReconcileRecord {
                    object: object_hex.clone(),
                    path: dest,
                    outcome: ReconcileOutcome::NoSource { detail },
                }),
                SourceResolution::Found(source) => {
                    if dry_run {
                        report.records.push(ReconcileRecord {
                            object: object_hex.clone(),
                            path: dest,
                            outcome: ReconcileOutcome::Rebuilt {
                                source,
                                bytes: object.size,
                                applied: false,
                            },
                        });
                        present += 1;
                        continue;
                    }
                    // Nested directories under an *existing* root are the mirrored layout
                    // the mover already creates; the root itself is never created.
                    if let Some(parent) = dest.parent() {
                        if let Err(error) = fs::create_dir_all(parent) {
                            report.records.push(ReconcileRecord {
                                object: object_hex.clone(),
                                path: dest.clone(),
                                outcome: ReconcileOutcome::Failed {
                                    detail: format!(
                                        "cannot create {} for the rebuilt copy: {error}",
                                        parent.display()
                                    ),
                                },
                            });
                            continue;
                        }
                    }
                    match restore::build_verified_copy(&source, &dest, &expected) {
                        Ok(bytes) => {
                            catalog.record_replica(
                                &object.object,
                                &tier,
                                &key,
                                true,
                                Some(&object.checksum),
                            )?;
                            report.records.push(ReconcileRecord {
                                object: object_hex.clone(),
                                path: dest,
                                outcome: ReconcileOutcome::Rebuilt {
                                    source,
                                    bytes,
                                    applied: true,
                                },
                            });
                            present += 1;
                        }
                        Err(error) => report.records.push(ReconcileRecord {
                            object: object_hex.clone(),
                            path: dest,
                            outcome: ReconcileOutcome::Failed {
                                detail: error.to_string(),
                            },
                        }),
                    }
                }
            },
        }
    }

    Ok(())
}

/// The outcome of looking for a sibling to rebuild from.
enum SourceResolution {
    /// A sibling whose bytes hash to the recorded checksum.
    Found(PathBuf),
    /// Nothing usable, with the reason.
    None(String),
}

/// Find a sibling that really is the object, by hashing it against the recorded checksum.
///
/// The catalog's `verified` flag is deliberately not the deciding vote: this is the one
/// place a rebuild gets its bytes, and a stale row is exactly what a scrub exists to
/// distrust. Siblings a scrub already marked damaged are tried *after* the unmarked ones
/// (and still re-read), and any sibling that hashes to something else is marked damaged —
/// never copied from, never deleted — so the next run sees the rot even if no scrub has.
fn resolve_source(
    catalog: &Catalog,
    object: &ReconcileObject,
    expected: &blake3::Hash,
    dry_run: bool,
) -> Result<SourceResolution, ReconcileError> {
    let is_damaged = |location: &LocationRecord| {
        object
            .damaged
            .iter()
            .any(|(tier, key)| tier == &location.tier && key == &location.storage_key)
    };
    // Unmarked siblings first: a copy a scrub has given up on is a last resort, and only
    // after its bytes prove themselves again.
    let ordered = object
        .locations
        .iter()
        .filter(|location| !is_damaged(location))
        .chain(
            object
                .locations
                .iter()
                .filter(|location| is_damaged(location)),
        );

    let mut rejected: Vec<String> = Vec::new();
    for location in ordered {
        let path = Path::new(&location.tier).join(&location.storage_key);
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        match digest::file_digest(&path) {
            Ok(found) if found == *expected => return Ok(SourceResolution::Found(path)),
            Ok(found) => {
                rejected.push(format!(
                    "{} hashes to {} (recorded {})",
                    path.display(),
                    found.to_hex(),
                    expected.to_hex()
                ));
                if !dry_run {
                    catalog.mark_damaged(
                        &location.tier,
                        &location.storage_key,
                        &object.object,
                        &format!(
                            "does not match the recorded checksum {} (reconcile)",
                            expected.to_hex()
                        ),
                    )?;
                }
            }
            Err(error) => rejected.push(format!("{} is unreadable ({error})", path.display())),
        }
    }

    if rejected.is_empty() {
        Ok(SourceResolution::None(
            "no surviving copy is recorded for this object".to_string(),
        ))
    } else {
        Ok(SourceResolution::None(format!(
            "no sibling matches the recorded checksum; {}",
            rejected.join("; ")
        )))
    }
}

/// A BLAKE3 checksum from the catalog's bytes, when it is a real 32-byte digest.
fn digest_of(checksum: &[u8]) -> Option<blake3::Hash> {
    let bytes: [u8; 32] = checksum.try_into().ok()?;
    Some(blake3::Hash::from_bytes(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

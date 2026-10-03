//! Restore: bring an offloaded object back to the hot path, verified.
//!
//! The mover is one-directional on its own — it puts bytes on a cold tier and leaves a
//! relative symlink in their place. This is the other half of the loop: given a path in
//! the watched tree, find the cold copy, copy it *back*, check it byte for byte, and
//! swap it into the path with an atomic rename. The cold copy stays unless
//! `--remove-copy` is given, and that only drops it after the fresh copy has been
//! verified (verify-before-delete, `docs/design.md` §6).
//!
//! # How the cold copy is found
//!
//! The copy itself is found by filesystem state (the catalog records *where* a copy is,
//! and `locate` answers that; this command still has to name the bytes it is about to
//! copy), in this order:
//!
//! 1. if the path is a symlink that resolves, its target is the copy the mover left;
//! 2. otherwise — a broken link, or no name at all — the mirrored relative path under
//!    each `--dest`, in the order given (fastest tier first, the order the mover fills).
//!
//! If several of those candidate paths hold *different* bytes for one name, that is not
//! a situation to guess through: restoring either would be a coin flip over someone's
//! data, so the command refuses and names them. If they agree, the first is used.
//!
//! # What "verified" means
//!
//! The freshly written bytes are read back and hashed (BLAKE3, the one digest the whole
//! tool uses) before they are renamed into place, so a torn copy can never become the
//! file at the path.
//!
//! When a catalog exists — `--catalog <FILE>`, or the default
//! `.just_cache-catalog.sqlite` beside the watch root — the digest those bytes are
//! compared against is the object's **recorded checksum**, not the cold copy's own bytes.
//! That is the difference between catching a torn copy and catching a cold copy that was
//! already corrupt before this command ran: with a recorded digest there is something
//! independent to compare against, and a copy that no longer matches it is refused rather
//! than restored faithfully. A catalog that is present but does not name the path is a
//! hard error too: the catalog is the source of truth for "does this object exist"
//! (`docs/design.md` §3), so silently falling back to the filesystem would skip exactly
//! the verification this command now owes. `restore` never creates a catalog (invariant
//! 9); the same open-if-present rule `explain` and `audit` use applies here.
//!
//! With no catalog, a copy that was already corrupt cannot be detected — there is no
//! independent digest — and the restored bytes are checked against the cold copy, exactly
//! as before. `docs/design.md` §9 records that the catalog digest has closed this gap for
//! the catalog path and that the no-catalog path still has no independent check.
//!
//! # What is never overwritten
//!
//! A regular file already at the path is the restored state:
//!
//! * byte-identical to the cold copy → nothing to do, a no-op (`exit 0`), which is what
//!   makes restore idempotent;
//! * *different* from the cold copy → refused. A same-size stranger is exactly the case
//!   a length-only comparison gets wrong: the hot file may be newer than the copy, and
//!   clobbering it would lose data. Restore leaves both untouched and says so. (An
//!   "already present" file therefore means one whose content matches; a divergent one is
//!   a hard error, never a silent success.)

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::catalog::{self, Catalog, LocationRecord, ObjectRecord};
use crate::digest;
use crate::disk_management;

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("{path} is not inside the watched tree {watch}")]
    OutsideWatch { path: PathBuf, watch: PathBuf },

    #[error(
        "{path} contains a `..` component; refusing to act on a path whose real location \
         this command has not resolved (name the path as the filesystem spells it, or the \
         symlink target the mover left)"
    )]
    ParentComponent { path: PathBuf },

    #[error(
        "refusing to remove the cold copy {copy} for {path}: it does not sit under any \
         configured --dest root or catalog-recorded tier root, so this command cannot vouch \
         that it is a cold copy and not a file it must leave alone"
    )]
    RemoveCopyOutsideDests { path: PathBuf, copy: PathBuf },

    #[error("no cold copy found for {path} under any of the {} destination(s)", dests.len())]
    NoColdCopy { path: PathBuf, dests: Vec<PathBuf> },

    #[error(
        "refusing to restore {path}: its cold copies disagree about the content \
         ({count} copies, digests {digests}); picking one would be a guess"
    )]
    AmbiguousCopies {
        path: PathBuf,
        count: usize,
        digests: String,
    },

    #[error(
        "refusing to overwrite {path}: it is already a regular file whose content differs \
         from the copy that would be restored from {cold} (BLAKE3 {hot_digest} vs expected \
         {expected_digest}); nothing was touched"
    )]
    HotPathMismatch {
        path: PathBuf,
        cold: PathBuf,
        hot_digest: String,
        expected_digest: String,
    },

    #[error(
        "cannot restore {path}: a catalog is configured but does not name this path; \
         nothing was touched (run `catalog sync` if the tree has changed)"
    )]
    CatalogUnknownPath { path: PathBuf },

    #[error(
        "cannot restore {path}: the catalog records a malformed checksum {checksum:?}; \
         nothing was touched"
    )]
    CatalogChecksumMalformed { path: PathBuf, checksum: String },

    #[error("the catalog could not be read: {error}")]
    Catalog { error: catalog::CatalogError },

    #[error("{path} is a directory; restore only handles a regular file or a symlink")]
    HotPathIsDirectory { path: PathBuf },

    #[error("cannot copy {from} onto {path}: {error}")]
    Copy {
        path: PathBuf,
        from: PathBuf,
        error: io::Error,
    },

    #[error(
        "verification failed: {path} was written but its content does not match the cold \
         copy (restored BLAKE3 {found_digest}, cold BLAKE3 {expected_digest}); the path was \
         left as it was"
    )]
    VerifyMismatch {
        path: PathBuf,
        expected_digest: String,
        found_digest: String,
    },

    #[error(
        "verification failed: {path} was written but its content does not match the \
         catalog's recorded checksum (restored BLAKE3 {found_digest}, recorded \
         {expected_digest}); the path was left as it was and the cold copy was not touched"
    )]
    CatalogChecksumMismatch {
        path: PathBuf,
        expected_digest: String,
        found_digest: String,
    },

    #[error("cannot inspect {path}: {error}")]
    Stat { path: PathBuf, error: io::Error },

    #[error(
        "refusing to write {path}: something is already there, and whether it is the object is the catalog's answer, not this function's"
    )]
    DestinationExists { path: PathBuf },

    #[error(
        "restored {path} but did NOT remove the cold copy {copy}: re-checking the restored \
         file failed ({detail})"
    )]
    RemoveCopyNotVerified {
        path: PathBuf,
        copy: PathBuf,
        detail: String,
    },

    #[error("cannot remove the cold copy {copy} after restoring {path}: {error}")]
    RemoveCopy {
        path: PathBuf,
        copy: PathBuf,
        error: io::Error,
    },
}

/// What a restore did. Both variants are success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The bytes were copied back and swapped into the hot path.
    Restored {
        cold_copy: PathBuf,
        bytes: u64,
        removed_copy: bool,
    },
    /// The path already held a byte-identical regular file; nothing changed.
    AlreadyPresent { cold_copy: Option<PathBuf> },
}

impl RestoreOutcome {
    /// One line for a human, in the voice of the rest of the tool's output.
    pub fn describe(&self, path: &Path) -> String {
        match self {
            RestoreOutcome::Restored {
                cold_copy,
                bytes,
                removed_copy,
            } => {
                let removal = if *removed_copy {
                    " (cold copy removed after verification)"
                } else {
                    ""
                };
                format!(
                    "restored {} from {} ({} bytes){removal}",
                    path.display(),
                    cold_copy.display(),
                    bytes
                )
            }
            RestoreOutcome::AlreadyPresent {
                cold_copy: Some(cold),
            } => format!(
                "already present and identical: {} (nothing to do; cold copy {})",
                path.display(),
                cold.display()
            ),
            RestoreOutcome::AlreadyPresent { cold_copy: None } => format!(
                "already present: {} (no cold copy; nothing to do)",
                path.display()
            ),
        }
    }
}

/// Everything one restore needs. (The catalog is not `Debug`-printed: the struct derives
/// it for the error paths, and a catalog's contents are its rows, not its handle.)
pub struct RestoreRequest<'a> {
    /// The path to bring back. May be a symlink (working or broken) or missing.
    pub path: &'a Path,
    /// The watched tree the path belongs to.
    pub watch: &'a Path,
    /// Cold roots, fastest tier first.
    pub dests: &'a [PathBuf],
    /// Drop the cold copy once the restored file has been verified.
    pub remove_copy: bool,
    /// The catalog to verify the restored bytes against, when one exists.
    ///
    /// `None` is the honest default: with no catalog the bytes are checked against the
    /// cold copy, exactly as this command always did. The caller — `main` — owns the
    /// open-if-present rule (`--catalog` must exist; the default beside the watch root is
    /// used only when it is already there) and never creates one for a restore.
    pub catalog: Option<&'a Catalog>,
    /// Object-tier configurations, so a file whose only copy is in an object store can
    /// be downloaded rather than skipped. Empty or absent means the restore falls back to
    /// filesystem-only behaviour (the no-catalog path).
    pub object_tier_configs: &'a [crate::object_store::ObjectTierConfig],
    /// The envelope encryption key for the configured object tier(s). All object tiers
    /// with this driver share one key per invocation; the key-value never appears in an
    /// error or a log line.
    pub encryption_keys: &'a [crate::envelope::Key],
}

/// Bring the object at `request.path` back from a cold tier.
///
/// Idempotent and safe to repeat: a path that already holds the right bytes is a no-op.
pub fn restore(request: &RestoreRequest<'_>) -> Result<RestoreOutcome, RestoreError> {
    let path = absolute(request.path);
    let watch = absolute(request.watch);

    let relative = match path.strip_prefix(&watch) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_path_buf(),
        // An empty relative means the path *is* the watched root: not a file to restore.
        _ => {
            return Err(RestoreError::OutsideWatch { path, watch });
        }
    };

    // `absolute()` is lexical and does not resolve `..`, so the `strip_prefix` above can be
    // satisfied by a path that leaves the tree (`T/../escaped.bin` strips to
    // `../escaped.bin`). Resolving the `..` would mean canonicalizing, which loses the
    // symlink state restore exists to see, so the only safe answer is to refuse: the
    // operator names the path as the filesystem spells it, and this never reaches a lookup,
    // a copy, or a delete.
    if relative
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(RestoreError::ParentComponent { path });
    }

    // The catalog's answer, when there is one: its record holds both the digest every check
    // below compares against *and* the recorded locations — which is how a copy on a tier
    // this invocation was not given as a `--dest` is still found. Resolved once, up front: a
    // catalog that does not name the path fails here, before anything is opened or copied.
    let record = catalog_object(request.catalog, &relative, &path)?;
    let recorded = match &record {
        Some(record) => Some(blake3::Hash::from_hex(&record.checksum).map_err(|_| {
            RestoreError::CatalogChecksumMalformed {
                path: path.clone(),
                checksum: record.checksum.clone(),
            }
        })?),
        None => None,
    };
    let recorded_locations: &[LocationRecord] = record
        .as_ref()
        .map(|record| record.locations.as_slice())
        .unwrap_or(&[]);
    // The roots a recorded location row may be joined against: the `--dest` roots this
    // invocation was given, plus every root the catalog itself recorded at sync. A tier the
    // catalog knows but this invocation was not handed is exactly the case a bare filesystem
    // search cannot serve, and `resolve_location_path` still refuses any row that does not
    // provably stay under one of these.
    let roots = trusted_roots(request.dests, request.catalog)?;
    let watch_tier = fs::canonicalize(&watch).unwrap_or_else(|_| watch.clone());

    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => Err(RestoreError::HotPathIsDirectory { path }),
        // A real file at the path is the state restore is trying to reach. Never replace
        // it: compare, and either no-op (identical) or refuse (different).
        Ok(metadata) if metadata.is_file() => match locate(
            &path,
            &relative,
            request.dests,
            recorded_locations,
            &watch_tier,
            &roots,
        )? {
            None => Ok(RestoreOutcome::AlreadyPresent { cold_copy: None }),
            Some(cold) => {
                // Without a catalog the expected digest can only come from the cold copy;
                // with one it is the recorded checksum, so a hot file that agrees with a
                // corrupt cold copy is still refused rather than mistaken for the state
                // restore was trying to reach.
                let expected_digest = match &recorded {
                    Some(hash) => *hash,
                    None => digest_of(&cold)?,
                };
                let hot_digest = digest_of(&path)?;
                if hot_digest != expected_digest {
                    return Err(RestoreError::HotPathMismatch {
                        path,
                        cold,
                        hot_digest: hex(&hot_digest),
                        expected_digest: hex(&expected_digest),
                    });
                }
                // The path already holds bytes verified identical to the recorded
                // content, so `--remove-copy` can reclaim the cold bytes now:
                // verify-before-delete is satisfied by the digest comparison just above.
                // Without it there is nothing to do at all — this is the idempotent no-op.
                if request.remove_copy {
                    // `locate` already keeps only candidates the command can vouch for, but
                    // this is the one delete that cannot be undone, so the containment is
                    // tested again here, immediately before it, rather than trusted across
                    // the distance to `locate`.
                    ensure_removable(&cold, &roots, &path)?;
                    let cold_metadata =
                        fs::metadata(&cold).map_err(|error| RestoreError::Stat {
                            path: cold.clone(),
                            error,
                        })?;
                    fs::remove_file(&cold).map_err(|error| RestoreError::RemoveCopy {
                        path: path.clone(),
                        copy: cold.clone(),
                        error,
                    })?;
                    return Ok(RestoreOutcome::Restored {
                        cold_copy: cold,
                        bytes: cold_metadata.len(),
                        removed_copy: true,
                    });
                }
                Ok(RestoreOutcome::AlreadyPresent {
                    cold_copy: Some(cold),
                })
            }
        },
        // Missing, a working symlink, or a broken one: put the object back.
        _ => {
            let cold = locate(
                &path,
                &relative,
                request.dests,
                recorded_locations,
                &watch_tier,
                &roots,
            )?
            .ok_or_else(|| RestoreError::NoColdCopy {
                path: path.clone(),
                dests: request.dests.to_vec(),
            });

            // When locate() returns None but the catalog records an object-tier
            // location, download the object from the object store instead.
            let cold = match cold {
                Ok(cold) => cold,
                Err(err) => {
                    // Try object-tier download as a fallback
                    if let Some(obj_cold) = restore_from_object_tier(
                        request,
                        &path,
                        recorded_locations,
                        recorded.as_ref(),
                    )? {
                        obj_cold
                    } else {
                        return Err(err);
                    }
                }
            };

            if request.remove_copy {
                ensure_removable(&cold, &roots, &path)?;
            }
            restore_from(&path, &cold, request.remove_copy, recorded.as_ref())
        }
    }
}

/// The catalog's record for one namespace path.
///
/// Returns `Ok(None)` only when no catalog is configured; with one, a path it does not
/// name is an error rather than a quiet fall back to filesystem-only verification. The
/// catalog is the source of truth for "does this object exist" (`docs/design.md` §3), and
/// restoring an object it has never seen would mean skipping the independent check this
/// command exists to perform (and, now, its recorded location too).
fn catalog_object(
    catalog: Option<&Catalog>,
    relative: &Path,
    path: &Path,
) -> Result<Option<ObjectRecord>, RestoreError> {
    let Some(catalog) = catalog else {
        return Ok(None);
    };
    // Names are stored relative to the watch root — the same key `catalog sync` ingested.
    let record = catalog
        .record_for_path(&relative.to_string_lossy())
        .map_err(|error| RestoreError::Catalog { error })?;
    match record {
        Some(record) => Ok(Some(record)),
        None => Err(RestoreError::CatalogUnknownPath {
            path: path.to_path_buf(),
        }),
    }
}

/// Every root a recorded `(tier, storage_key)` may be joined against: the `--dest` roots
/// this invocation was given, plus the roots `catalog sync` recorded.
///
/// The second set is what lets restore reach an object on a configured tier that is not one
/// of this invocation's `--dest` arguments. It does not widen what may be touched blindly:
/// [`catalog::resolve_location_path`] still requires the row's `tier` to be one of these
/// roots exactly, and the resulting path is checked to be a regular file before it becomes a
/// candidate.
fn trusted_roots(
    dests: &[PathBuf],
    catalog: Option<&Catalog>,
) -> Result<Vec<PathBuf>, RestoreError> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for dest in dests {
        let root = fs::canonicalize(dest).unwrap_or_else(|_| dest.clone());
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if let Some(catalog) = catalog {
        for root in catalog
            .roots()
            .map_err(|error| RestoreError::Catalog { error })?
        {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    Ok(roots)
}

/// Find the cold copy for one relative path, refusing if the candidates disagree.
///
/// `recorded_locations` are the catalog's `location` rows for the object, and `roots` are
/// the roots those rows may be joined against. They are consulted after the filesystem
/// layout because a name that still resolves names the copy the mover *actually* left, but
/// they are the only way to reach an object whose copy is on a tier this invocation was not
/// given as a `--dest` (or whose namespace path and storage key have diverged).
fn locate(
    hot: &Path,
    relative: &Path,
    dests: &[PathBuf],
    recorded_locations: &[LocationRecord],
    watch_tier: &Path,
    roots: &[PathBuf],
) -> Result<Option<PathBuf>, RestoreError> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // A resolving symlink at the path names the copy the mover left — but only when its
    // target really is a copy under a `--dest` root. A link is writable by anyone who can
    // write the tree, so an unchecked target is an arbitrary path the operator can read:
    // without this containment a `--remove-copy` restore deletes it and a plain restore
    // copies it into the watched tree. Canonicalizing before the test means a target that
    // reaches a destination through `..` or its own symlinks still counts, while one that
    // escapes every root is a foreign link and is dropped.
    if let Ok(target) = fs::read_link(hot) {
        let resolved = resolve_link(hot, &target);
        if is_regular_file(&resolved) && under_any_root(&resolved, dests) {
            candidates.push(resolved);
        }
    }

    // The mirrored layout: each tier holds the file at its path relative to the watched
    // root, so a broken link — or a name that vanished entirely — is still recoverable.
    // The containment is the same one the link target gets: a name under a tier that is
    // itself a symlink out of it is no more a cold copy than a link target that escapes.
    for dest in dests {
        let mirrored = dest.join(relative);
        if is_regular_file(&mirrored) && under_any_root(&mirrored, dests) {
            candidates.push(mirrored);
        }
    }

    // Copies the catalog recorded that the filesystem layout above cannot reach — a copy on
    // a configured tier this invocation was not given as a `--dest`, or one whose storage
    // key no longer mirrors the namespace path. A hot location is skipped: it is the file at
    // the path, not a cold copy to restore from. Every row is proved to stay under a trusted
    // root by `resolve_location_path` before it becomes a path, exactly as scrub and
    // reconcile do (issue #72), so a hand-edited row is refused rather than touched.
    for location in recorded_locations {
        if Path::new(&location.tier) == watch_tier {
            continue;
        }
        let Ok(candidate) =
            catalog::resolve_location_path(&location.tier, &location.storage_key, roots)
        else {
            continue;
        };
        if is_regular_file(&candidate) {
            candidates.push(candidate);
        }
    }

    // A link target can be the very file one of the mirrored paths resolves to; treat
    // those as one candidate rather than two.
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut unique: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let key = fs::canonicalize(&candidate).unwrap_or_else(|_| candidate.clone());
        if seen.insert(key) {
            unique.push(candidate);
        }
    }
    if unique.is_empty() {
        return Ok(None);
    }

    // Group by digest. More than one group means the tiers do not agree — see the module
    // docs; the command refuses rather than pick a winner.
    let mut by_digest: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for candidate in &unique {
        by_digest
            .entry(hex(&digest_of(candidate)?))
            .or_default()
            .push(candidate.clone());
    }
    if by_digest.len() > 1 {
        let digests: Vec<String> = by_digest.keys().cloned().collect();
        return Err(RestoreError::AmbiguousCopies {
            path: hot.to_path_buf(),
            count: unique.len(),
            digests: digests.join(", "),
        });
    }

    // Report the real path, not the raw link text with its `..` components: a caller that
    // passes a symlink target on to `--remove-copy` should be naming a real file.
    Ok(Some(
        fs::canonicalize(&unique[0]).unwrap_or_else(|_| unique[0].clone()),
    ))
}

/// Try to restore a file whose only copy is on an object tier: download from the
/// matching object store, decrypt, verify the plaintext against the recorded digest,
/// and return the path to the downloaded temporary file.
///
/// Returns `Ok(None)` when no object-tier location is found, or when the object-tier
/// config is not provided.
fn restore_from_object_tier(
    request: &RestoreRequest<'_>,
    hot: &Path,
    recorded_locations: &[LocationRecord],
    recorded: Option<&blake3::Hash>,
) -> Result<Option<PathBuf>, RestoreError> {
    if request.object_tier_configs.is_empty() || request.encryption_keys.is_empty() {
        return Ok(None);
    }

    // Find a catalog-recorded location on an object tier (tier name matches a config).
    for location in recorded_locations {
        let config = request
            .object_tier_configs
            .iter()
            .find(|c| c.name == location.tier);
        let Some(config) = config else {
            continue;
        };
        let key = request.encryption_keys.first();
        let Some(key) = key else {
            continue;
        };

        // Download to the hot file's parent directory as a partial sibling.
        let parent = hot.parent().unwrap_or_else(|| Path::new("."));
        let partial = disk_management::partial_sibling(parent);

        // Decode the recorded checksum into a blake3::Hash for verification.
        let expected = match recorded {
            Some(hash) => *hash,
            None => {
                // Without a recorded digest we cannot verify the download; skip.
                continue;
            }
        };

        crate::object_store::download_and_verify(
            config,
            key,
            &location.storage_key,
            &expected,
            &partial,
        )
        .map_err(|err| RestoreError::Copy {
            path: hot.to_path_buf(),
            from: PathBuf::from(config.display_key(&location.storage_key)),
            error: std::io::Error::other(err.to_string()),
        })?;

        let _downloaded_meta = std::fs::metadata(&partial).map_err(|error| RestoreError::Stat {
            path: partial.clone(),
            error,
        })?;

        // The downloaded file is already verified (download_and_verify checks the
        // digest). We return it as the cold copy; the caller will use restore_from()
        // to move it into place with its own verification step.
        return Ok(Some(partial));
    }

    Ok(None)
}

/// Copy `cold` onto `hot`, verifying before the swap, optionally dropping the cold copy.
///
/// `recorded` is the catalog's checksum for the object, when a catalog is configured: the
/// private copy is read back and hashed against it rather than against the cold copy's own
/// bytes, so a cold copy that was already corrupt is refused instead of restored. Without
/// one, the cold copy is the only digest available and the behaviour is unchanged.
fn restore_from(
    hot: &Path,
    cold: &Path,
    remove_copy: bool,
    recorded: Option<&blake3::Hash>,
) -> Result<RestoreOutcome, RestoreError> {
    let cold_metadata = fs::metadata(cold).map_err(|error| RestoreError::Stat {
        path: cold.to_path_buf(),
        error,
    })?;
    let expected_digest = match recorded {
        Some(hash) => *hash,
        None => digest_of(cold)?,
    };

    let parent = hot.parent().unwrap_or_else(|| Path::new("."));
    // A path whose name vanished entirely (an audited "orphaned-copy") may need the
    // directory it lived in back too. The user named this path explicitly, so recreating
    // its parent is part of honoring the request — nothing else in the tree is touched.
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent).map_err(|error| RestoreError::Copy {
            path: hot.to_path_buf(),
            from: cold.to_path_buf(),
            error,
        })?;
    }

    // The partial lives in the *hot* directory so the rename below stays on one
    // filesystem and is atomic — a reader sees the old state or the whole file, never a
    // truncated one. Same marker the mover uses, so the walk skips it (invariant 8).
    let partial = disk_management::partial_sibling(parent);
    copy_to_partial(hot, cold, &partial, cold_metadata.len())?;

    // Read the bytes back and hash them before they go live. A torn copy is caught here
    // rather than being renamed over a path the user trusts.
    let found_digest = digest::file_digest(&partial).map_err(|error| {
        let _ = fs::remove_file(&partial);
        RestoreError::Stat {
            path: partial.clone(),
            error,
        }
    })?;
    if found_digest != expected_digest {
        // The partial is dropped and the path untouched either way; the error only names
        // which comparison failed, because the two say different things about the data:
        // a cold-copy mismatch is a torn copy, a recorded-checksum mismatch is a cold
        // copy that was already corrupt before this command ran.
        let _ = fs::remove_file(&partial);
        return Err(match recorded {
            Some(_) => RestoreError::CatalogChecksumMismatch {
                path: hot.to_path_buf(),
                expected_digest: hex(&expected_digest),
                found_digest: hex(&found_digest),
            },
            None => RestoreError::VerifyMismatch {
                path: hot.to_path_buf(),
                expected_digest: hex(&expected_digest),
                found_digest: hex(&found_digest),
            },
        });
    }

    fs::rename(&partial, hot).map_err(|error| {
        let _ = fs::remove_file(&partial);
        RestoreError::Copy {
            path: hot.to_path_buf(),
            from: partial.clone(),
            error,
        }
    })?;

    let mut removed_copy = false;
    if remove_copy {
        // Verify-before-delete (§6): the cold bytes are dropped only once the restored
        // file has been read back and checksummed clean. The pre-swap read-back already
        // covered the same bytes; this second check is the one the invariant asks for,
        // on the file that is actually at the hot path now.
        let restored_digest =
            digest_of(hot).map_err(|error| RestoreError::RemoveCopyNotVerified {
                path: hot.to_path_buf(),
                copy: cold.to_path_buf(),
                detail: error.to_string(),
            })?;
        if restored_digest != expected_digest {
            return Err(RestoreError::RemoveCopyNotVerified {
                path: hot.to_path_buf(),
                copy: cold.to_path_buf(),
                detail: format!(
                    "restored BLAKE3 {} != expected BLAKE3 {}",
                    hex(&restored_digest),
                    hex(&expected_digest)
                ),
            });
        }
        fs::remove_file(cold).map_err(|error| RestoreError::RemoveCopy {
            path: hot.to_path_buf(),
            copy: cold.to_path_buf(),
            error,
        })?;
        removed_copy = true;
    }

    Ok(RestoreOutcome::Restored {
        cold_copy: cold.to_path_buf(),
        bytes: cold_metadata.len(),
        removed_copy,
    })
}

/// Copy `cold`'s bytes and metadata into a private sibling, cleaning it up on failure.
///
/// Reuses the mover's copy: reflink where possible, `SEEK_DATA`/`SEEK_HOLE` so holes stay
/// holes, plain sequential only as the last resort — the same three strategies that keep a
/// sparse 40 GB image from becoming 40 GB on the way back to the hot tier.
fn copy_to_partial(hot: &Path, cold: &Path, partial: &Path, size: u64) -> Result<(), RestoreError> {
    let outcome = (|| -> io::Result<()> {
        let source = File::open(cold)?;
        let source_metadata = source.metadata()?;
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial)?;
        disk_management::copy_contents(&source, &mut destination, size)?;
        // Metadata while the file is still private: no reader ever sees the process
        // umask's guess at a mode.
        disk_management::preserve_metadata(cold, partial, &source_metadata)?;
        destination.sync_all()?;
        Ok(())
    })();

    match outcome {
        Ok(()) => Ok(()),
        Err(error) => {
            // Nothing of ours was renamed into place; drop the partial.
            let _ = fs::remove_file(partial);
            Err(RestoreError::Copy {
                path: hot.to_path_buf(),
                from: cold.to_path_buf(),
                error,
            })
        }
    }
}

/// Overwrite a corrupt copy at its own location with bytes from a verified sibling.
///
/// This is `restore`'s build-a-private-copy-then-rename machinery, used in the other
/// direction: instead of writing into the watched tree, it repairs a tier in place. The
/// guarantees are the same ones the module documents — the replacement is built beside
/// the corrupt file under the `.just_cache-partial-*` marker the walk skips (invariant
/// 8), read back and hashed against `expected` *before* the atomic rename puts it live,
/// and the good sibling is never touched. So the worst a failure here does is leave a
/// partial to be cleaned up; it can never make a location hold unverified bytes, and it
/// never deletes the only copy of anything.
///
/// `expected` is the object's checksum from the catalog, which is what makes this a fix
/// rather than a guess: the bytes are known before the corrupt copy is overwritten.
pub fn replace_from_verified(
    good: &Path,
    corrupt: &Path,
    expected: &blake3::Hash,
) -> Result<u64, RestoreError> {
    let good_metadata = fs::metadata(good).map_err(|error| RestoreError::Stat {
        path: good.to_path_buf(),
        error,
    })?;
    if !good_metadata.is_file() {
        return Err(RestoreError::Stat {
            path: good.to_path_buf(),
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "the repair source is not a regular file",
            ),
        });
    }
    // Refuse to replace anything that is not the regular file a location is expected to
    // be: a symlink or directory at a location is a different problem, and the rename
    // below would either follow or fail on it in a way that is not this function's job.
    let corrupt_metadata = fs::symlink_metadata(corrupt).map_err(|error| RestoreError::Stat {
        path: corrupt.to_path_buf(),
        error,
    })?;
    if !corrupt_metadata.is_file() {
        return Err(RestoreError::Stat {
            path: corrupt.to_path_buf(),
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "the corrupt location is not a regular file",
            ),
        });
    }

    let parent = corrupt.parent().unwrap_or_else(|| Path::new("."));
    let partial = disk_management::partial_sibling(parent);
    copy_to_partial(corrupt, good, &partial, good_metadata.len())?;

    // Read the private copy back and hash it before it replaces anything. A torn copy is
    // caught here rather than becoming the location's bytes.
    let found_digest = digest::file_digest(&partial).map_err(|error| {
        let _ = fs::remove_file(&partial);
        RestoreError::Stat {
            path: partial.clone(),
            error,
        }
    })?;
    if found_digest != *expected {
        let _ = fs::remove_file(&partial);
        return Err(RestoreError::VerifyMismatch {
            path: corrupt.to_path_buf(),
            expected_digest: expected.to_hex().to_string(),
            found_digest: found_digest.to_hex().to_string(),
        });
    }

    // Same directory, so this rename stays on one filesystem and atomically replaces the
    // corrupt file with verified bytes.
    fs::rename(&partial, corrupt).map_err(|error| {
        let _ = fs::remove_file(&partial);
        RestoreError::Copy {
            path: corrupt.to_path_buf(),
            from: partial.clone(),
            error,
        }
    })?;

    Ok(good_metadata.len())
}

/// Build a verified copy of `good` at a destination that is currently *absent*.
///
/// This is the create half of [`replace_from_verified`]'s machinery, and it is what the
/// reconcile pass (`crate::reconcile`) uses to rebuild a replica a re-added disk is
/// missing. The bytes are written to a private `.just_cache-partial-*` sibling of the
/// destination, hashed against `expected` (the object's recorded checksum) *there*, and
/// only a verified copy is renamed into place — so a failure leaves the location absent
/// rather than holding bytes nobody can vouch for. That is deliberately stronger than
/// "write then check then remove a bad copy": nothing unverified ever gets a published
/// name, so the reconcile pass never has to delete even its own output.
///
/// It refuses a destination that already exists, of any kind. Whether an existing file
/// there is the object (adopt it) or a stranger (refuse it) is a catalog question the
/// caller answers by hashing it; this function must not clobber either way. The caller
/// also owns creating the nested directories under an existing destination root
/// (invariant 1 keeps this away from creating the root itself).
pub fn build_verified_copy(
    good: &Path,
    dest: &Path,
    expected: &blake3::Hash,
) -> Result<u64, RestoreError> {
    if fs::symlink_metadata(dest).is_ok() {
        return Err(RestoreError::DestinationExists {
            path: dest.to_path_buf(),
        });
    }

    let good_metadata = fs::metadata(good).map_err(|error| RestoreError::Stat {
        path: good.to_path_buf(),
        error,
    })?;
    if !good_metadata.is_file() {
        return Err(RestoreError::Stat {
            path: good.to_path_buf(),
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "the rebuild source is not a regular file",
            ),
        });
    }

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let partial = disk_management::partial_sibling(parent);
    copy_to_partial(dest, good, &partial, good_metadata.len())?;

    let found_digest = digest::file_digest(&partial).map_err(|error| {
        let _ = fs::remove_file(&partial);
        RestoreError::Stat {
            path: partial.clone(),
            error,
        }
    })?;
    if found_digest != *expected {
        let _ = fs::remove_file(&partial);
        return Err(RestoreError::VerifyMismatch {
            path: dest.to_path_buf(),
            expected_digest: expected.to_hex().to_string(),
            found_digest: found_digest.to_hex().to_string(),
        });
    }

    // The partial has a private name, so this rename is what first publishes the bytes —
    // and only now, after the read-back, does that happen. Same directory, so it stays on
    // one filesystem and cannot become a half-file.
    fs::rename(&partial, dest).map_err(|error| {
        let _ = fs::remove_file(&partial);
        RestoreError::Copy {
            path: dest.to_path_buf(),
            from: partial.clone(),
            error,
        }
    })?;

    Ok(good_metadata.len())
}

fn digest_of(path: &Path) -> Result<blake3::Hash, RestoreError> {
    digest::file_digest(path).map_err(|error| RestoreError::Stat {
        path: path.to_path_buf(),
        error,
    })
}

fn hex(hash: &blake3::Hash) -> String {
    hash.to_hex().to_string()
}

fn is_regular_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

/// True when `candidate` resolves to a path inside one of `roots`.
///
/// Both sides are canonicalized, so this is real containment rather than the lexical
/// prefix test `strip_prefix` does: a path that reaches a root through `..` or a
/// further symlink still counts, while one that leaves every root on the way — or whose
/// root cannot be canonicalized because it is not there — does not. This is the same test
/// `audit::resolve_target` applies to a watched symlink (src/audit.rs).
fn under_any_root(candidate: &Path, roots: &[PathBuf]) -> bool {
    let Ok(canonical) = fs::canonicalize(candidate) else {
        return false;
    };
    roots.iter().any(|root| {
        fs::canonicalize(root)
            .map(|resolved| canonical.starts_with(&resolved))
            .unwrap_or(false)
    })
}

/// Refuse `--remove-copy` on a path the command cannot prove is a cold copy.
///
/// `locate` already returns only candidates the command can vouch for, so this is
/// belt-and-braces: the delete it guards is the one step in the command that cannot be
/// undone, and the containment was proved some distance away, so it is repeated here rather
/// than assumed.
fn ensure_removable(copy: &Path, roots: &[PathBuf], path: &Path) -> Result<(), RestoreError> {
    if under_any_root(copy, roots) {
        Ok(())
    } else {
        Err(RestoreError::RemoveCopyOutsideDests {
            path: path.to_path_buf(),
            copy: copy.to_path_buf(),
        })
    }
}

/// Resolve a symlink's raw text against the directory holding the link.
fn resolve_link(link: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.parent().unwrap_or_else(|| Path::new(".")).join(target)
    }
}

/// An absolute form of `path`, without resolving symlinks (which would lose the very
/// link state restore needs to see). `std::path::absolute` is purely lexical plus the
/// current directory, which is exactly right here.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(target: &Path, at: &Path) {
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    fn request<'a>(path: &'a Path, watch: &'a Path, dests: &'a [PathBuf]) -> RestoreRequest<'a> {
        RestoreRequest {
            path,
            watch,
            dests,
            remove_copy: false,
            catalog: None,
            object_tier_configs: &[],
            encryption_keys: &[],
        }
    }

    /// A path whose symlink is broken but whose mirrored cold copy is intact is repaired.
    #[test]
    fn a_broken_symlink_is_repaired_from_the_mirrored_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(watch.join("shows")).unwrap();
        fs::create_dir_all(cold.join("shows")).unwrap();
        fs::write(cold.join("shows/ep2.mkv"), b"episode two").unwrap();
        // The link points somewhere the copy is not.
        link(
            Path::new("../../cold/vanished/ep2.mkv"),
            &watch.join("shows/ep2.mkv"),
        );

        let dests = vec![cold.clone()];
        let hot = watch.join("shows/ep2.mkv");
        let outcome = restore(&request(&hot, &watch, &dests)).unwrap();
        assert!(
            matches!(outcome, RestoreOutcome::Restored { .. }),
            "{outcome:?}"
        );
        assert!(!fs::symlink_metadata(&hot).unwrap().is_symlink());
        assert_eq!(fs::read(&hot).unwrap(), b"episode two");
        assert!(cold.join("shows/ep2.mkv").is_file(), "cold copy stays");
    }

    /// A same-size regular file with different bytes must never be clobbered: that is the
    /// exact case a size-only comparison gets wrong.
    #[test]
    fn a_same_size_different_hot_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(watch.join("stale.bin"), b"AAAA").unwrap();
        fs::write(cold.join("stale.bin"), b"BBBB").unwrap();

        let dests = vec![cold.clone()];
        let hot = watch.join("stale.bin");
        let err = restore(&request(&hot, &watch, &dests)).expect_err("must refuse");
        match err {
            RestoreError::HotPathMismatch { .. } => {}
            other => panic!("expected HotPathMismatch, got {other:?}"),
        }
        assert_eq!(fs::read(&hot).unwrap(), b"AAAA", "hot bytes untouched");
        assert_eq!(fs::read(cold.join("stale.bin")).unwrap(), b"BBBB");
    }
}

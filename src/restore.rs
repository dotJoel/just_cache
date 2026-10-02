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
//! There is no catalog yet (issue #16, being built separately), so the search is by
//! filesystem state, in this order:
//!
//! 1. if the path is a symlink that resolves, its target is the copy the mover left;
//! 2. otherwise — a broken link, or no name at all — the mirrored relative path under
//!    each `--dest`, in the order given (fastest tier first, the order the mover fills).
//!
//! If several of those candidate paths hold *different* bytes for one name, that is not
//! a situation to guess through: restoring either would be a coin flip over someone's
//! data, so the command refuses and names them. If they agree, the first is used.
//!
//! # What "verified" means without a catalog
//!
//! The freshly written bytes are read back and hashed (BLAKE3, the one digest the whole
//! tool uses) and compared to the cold copy before they are renamed into place, so a torn
//! copy can never become the file at the path. What this *cannot* do — and does not
//! pretend to — is detect a cold copy that was already corrupt before this command ran:
//! with no recorded digest there is nothing independent to compare against. A catalog
//! digest is the fix; until then this gap is named in `docs/design.md` §9 rather than
//! papered over.
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
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::digest;
use crate::disk_management;

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("{path} is not inside the watched tree {watch}")]
    OutsideWatch { path: PathBuf, watch: PathBuf },

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
         from the cold copy {cold} (BLAKE3 {hot_digest} vs {cold_digest}); nothing was touched"
    )]
    HotPathMismatch {
        path: PathBuf,
        cold: PathBuf,
        hot_digest: String,
        cold_digest: String,
    },

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

    #[error("cannot inspect {path}: {error}")]
    Stat { path: PathBuf, error: io::Error },

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

/// Everything one restore needs.
#[derive(Debug)]
pub struct RestoreRequest<'a> {
    /// The path to bring back. May be a symlink (working or broken) or missing.
    pub path: &'a Path,
    /// The watched tree the path belongs to.
    pub watch: &'a Path,
    /// Cold roots, fastest tier first.
    pub dests: &'a [PathBuf],
    /// Drop the cold copy once the restored file has been verified.
    pub remove_copy: bool,
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

    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => Err(RestoreError::HotPathIsDirectory { path }),
        // A real file at the path is the state restore is trying to reach. Never replace
        // it: compare, and either no-op (identical) or refuse (different).
        Ok(metadata) if metadata.is_file() => match locate(&path, &relative, request.dests)? {
            None => Ok(RestoreOutcome::AlreadyPresent { cold_copy: None }),
            Some(cold) => {
                let hot_digest = digest_of(&path)?;
                let cold_digest = digest_of(&cold)?;
                if hot_digest != cold_digest {
                    return Err(RestoreError::HotPathMismatch {
                        path,
                        cold,
                        hot_digest: hex(&hot_digest),
                        cold_digest: hex(&cold_digest),
                    });
                }
                // The path already holds bytes verified identical to the cold copy, so
                // `--remove-copy` can reclaim the cold bytes now: verify-before-delete is
                // satisfied by the digest comparison just above. Without it there is
                // nothing to do at all — this is the idempotent no-op.
                if request.remove_copy {
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
            let cold = locate(&path, &relative, request.dests)?.ok_or_else(|| {
                RestoreError::NoColdCopy {
                    path: path.clone(),
                    dests: request.dests.to_vec(),
                }
            })?;
            restore_from(&path, &cold, request.remove_copy)
        }
    }
}

/// Find the cold copy for one relative path, refusing if the candidates disagree.
fn locate(hot: &Path, relative: &Path, dests: &[PathBuf]) -> Result<Option<PathBuf>, RestoreError> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // A resolving symlink at the path names the copy the mover left, wherever it is.
    if let Ok(target) = fs::read_link(hot) {
        let resolved = resolve_link(hot, &target);
        if is_regular_file(&resolved) {
            candidates.push(resolved);
        }
    }

    // The mirrored layout: each tier holds the file at its path relative to the watched
    // root, so a broken link — or a name that vanished entirely — is still recoverable.
    for dest in dests {
        let mirrored = dest.join(relative);
        if is_regular_file(&mirrored) {
            candidates.push(mirrored);
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

/// Copy `cold` onto `hot`, verifying before the swap, optionally dropping the cold copy.
fn restore_from(
    hot: &Path,
    cold: &Path,
    remove_copy: bool,
) -> Result<RestoreOutcome, RestoreError> {
    let cold_metadata = fs::metadata(cold).map_err(|error| RestoreError::Stat {
        path: cold.to_path_buf(),
        error,
    })?;
    let expected_digest = digest_of(cold)?;

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
        let _ = fs::remove_file(&partial);
        return Err(RestoreError::VerifyMismatch {
            path: hot.to_path_buf(),
            expected_digest: hex(&expected_digest),
            found_digest: hex(&found_digest),
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
                    "restored BLAKE3 {} != cold BLAKE3 {}",
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

//! The offline-volume tier driver (#143): export a file to a volume a person inserts, and
//! the insert prompt that names the volume a read needs.
//!
//! An offline tier is a disk that is *not* on the machine most of the time. Its edge is not
//! a wire at all — the bytes leave the host because the volume itself does (docs/design.md
//! §2 rule 2), so the same [`crate::envelope`] every boundary-crossing driver uses seals
//! them, and the mover's verify-before-delete rule still holds: the source is removed only
//! after the stored copy is read back, decrypted, and hashed to the recorded digest.
//!
//! # The catalog is the source of truth, not a marker file (D2)
//!
//! There is deliberately **no marker file** written into the volume. A marker the tool
//! writes is a second copy of the truth that can go stale the moment a volume is wiped,
//! swapped between machines, or half-written; docs/design.md §3 puts the answer in the
//! catalog and only there. `storage_key` is a *hint inside a row the catalog already
//! vouches for* — it names where in the mounted volume the bytes were last seen, but the
//! row itself (and the `volume` ledger row binding the volume to the tier) is what says the
//! copy exists and where its disk is.
//!
//! # Storage key (D3)
//!
//! `storage_key` is `<volume-id>/<path relative to the mount root>`, e.g.
//! `drawer-01/photos/2024/cat.jpg`. The volume id leads because the read path must know
//! *which* volume to ask for before it can resolve anything else; the rest is the path the
//! file has inside the mounted volume, which is the file's path relative to the watch root
//! (the same relative path the fs mover uses), joined under the mount point. A forward slash
//! separates the two on every platform, because the key is a catalog string, not a path.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::catalog::{self, VolumeRecord};
use crate::disk_management;
use crate::envelope::{self, Key};

/// The `volume.state` value meaning a volume is in a tier's drive.
pub const MOUNTED: &str = catalog::VOLUME_MOUNTED;

/// The catalog storage key for a copy stored in `volume_id` at `relative` (a path relative
/// to the mount root, joined under it when the volume is mounted — D3).
pub fn storage_key(volume_id: &str, relative: &Path) -> String {
    let relative = relative.to_string_lossy().replace('\\', "/");
    format!("{volume_id}/{relative}")
}

/// Split a [`storage_key`] back into `(volume_id, path relative to the mount root)`.
///
/// `None` when the key has no `/`, which names no volume: a key written by an older tool or
/// corrupted by hand is refused rather than guessed at, because a guess would send the read
/// to the wrong disk. An empty volume id is refused for the same reason.
pub fn parse_storage_key(key: &str) -> Option<(String, PathBuf)> {
    let (volume_id, relative) = key.split_once('/')?;
    if volume_id.is_empty() {
        return None;
    }
    Some((volume_id.to_string(), PathBuf::from(relative)))
}

/// A human-facing rendering of a storage key for a report line: `drawer-01:/photos/cat.jpg`.
pub fn display_key(volume_id: &str, relative: &Path) -> String {
    format!(
        "{}:/{}",
        volume_id,
        relative.to_string_lossy().replace('\\', "/")
    )
}

/// The insert prompt (D5): a refusal that names the tier, the volume needed, its mount
/// point, and where that volume physically lives. A recall of an offline object must be
/// actionable ("insert drawer-07"), never a bare ENOENT (docs/design.md §3).
///
/// `mounted` is the volume currently recorded mounted in the tier, when there is one: if it
/// is a *different* volume, the prompt says so, because the operator has to take that one
/// out to put the right one in.
pub fn insert_prompt(
    tier: &str,
    mount: &Path,
    vaults: &[String],
    needed: &str,
    mounted: Option<&str>,
) -> String {
    let vaults = if vaults.is_empty() {
        "the tier's vault".to_string()
    } else {
        vaults.join(", ")
    };
    match mounted {
        Some(other) => format!(
            "insert volume `{needed}` and mount it at {}; tier `{tier}` currently has volume \
             `{other}` in its drive (volume `{needed}` lives in: {vaults})",
            mount.display()
        ),
        None => format!(
            "insert volume `{needed}` and mount it at {}; tier `{tier}` has no volume recorded \
             mounted (volume `{needed}` lives in: {vaults})",
            mount.display()
        ),
    }
}

/// Write `source` into `dest` as a fresh sealed envelope, then rename it into place.
///
/// The copy lands under a `.just_cache-partial-*` sibling and is renamed once it is on disk
/// and fsynced, so a crash mid-export leaves a partial the walk skips (invariant 8) and never
/// a half-written file under the name a catalog row will point at. The containing directory
/// is fsynced after the rename so the name is durable, not just the bytes.
pub fn export(key: &Key, source: &Path, dest: &Path) -> Result<(), OfflineError> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| OfflineError::Io {
        stage: "create volume directory",
        path: parent.to_path_buf(),
        source: error,
    })?;
    let partial = disk_management::partial_sibling(parent);

    let written = (|| -> Result<(), OfflineError> {
        let mut input = File::open(source).map_err(|error| OfflineError::Io {
            stage: "open source",
            path: source.to_path_buf(),
            source: error,
        })?;
        let mut out = File::create(&partial).map_err(|error| OfflineError::Io {
            stage: "create partial",
            path: partial.clone(),
            source: error,
        })?;
        envelope::encrypt_all(key, &mut input, &mut out).map_err(|error| {
            OfflineError::Envelope {
                stage: "encrypt",
                source: error,
            }
        })?;
        out.sync_all().map_err(|error| OfflineError::Io {
            stage: "fsync partial",
            path: partial.clone(),
            source: error,
        })?;
        Ok(())
    })();
    if let Err(error) = written {
        // Never leave a partial behind on a failed export: the next attempt would have to
        // walk past it, and the walk skips partials silently.
        let _ = fs::remove_file(&partial);
        return Err(error);
    }

    fs::rename(&partial, dest).map_err(|error| OfflineError::Io {
        stage: "rename into place",
        path: dest.to_path_buf(),
        source: error,
    })?;
    sync_dir(parent)?;
    Ok(())
}

/// Read the sealed copy at `stored`, decrypt it into `out`, and — when the catalog recorded a
/// plaintext digest — hash `out` and refuse it if it disagrees. Returns the plaintext length.
///
/// The recorded digest is the plaintext BLAKE3 (docs/design.md §2 rule 2 keeps it so, exactly
/// as for object tiers), so a correct volume verifies like a local copy and "cannot decrypt"
/// never reads as bitrot. A wrong key fails on the first chunk, inside `decrypt_all`.
pub fn read_verify_decrypt(
    key: &Key,
    stored: &Path,
    expected: Option<&blake3::Hash>,
    out: &Path,
) -> Result<u64, OfflineError> {
    let input = File::open(stored).map_err(|error| OfflineError::Io {
        stage: "open stored copy",
        path: stored.to_path_buf(),
        source: error,
    })?;
    let mut output = File::create(out).map_err(|error| OfflineError::Io {
        stage: "create plaintext",
        path: out.to_path_buf(),
        source: error,
    })?;
    let plaintext =
        envelope::decrypt_all(key, input, &mut output).map_err(|error| OfflineError::Envelope {
            stage: "decrypt",
            source: error,
        })?;
    output.sync_all().map_err(|error| OfflineError::Io {
        stage: "fsync plaintext",
        path: out.to_path_buf(),
        source: error,
    })?;
    drop(output);

    if let Some(expected) = expected {
        let got = crate::digest::file_digest(out).map_err(|error| OfflineError::Io {
            stage: "hash plaintext",
            path: out.to_path_buf(),
            source: error,
        })?;
        if got.as_bytes() != expected.as_bytes() {
            return Err(OfflineError::ChecksumMismatch {
                stored: stored.to_path_buf(),
            });
        }
    }
    Ok(plaintext)
}

/// The volume currently recorded mounted in `tier`, if a catalog says so. A thin wrapper so
/// the driver reads the ledger through one function.
pub fn mounted_volume(
    catalog: &catalog::Catalog,
    tier: &str,
) -> Result<Option<VolumeRecord>, OfflineError> {
    catalog
        .mounted_volume_for_tier(tier)
        .map_err(|source| OfflineError::Catalog { source })
}

/// Where an `offline` copy's bytes live *right now*: `<mount>/<relative>`, but only when the
/// volume ledger says the same volume the key names is the one in the tier's drive.
///
/// The key is `<volume-id>/<relative>` (D3), and the `volume` row binding that id to the
/// tier is what says its disk is present (docs/design.md §3) — the filesystem cannot be asked
/// whether a *different* disk is mounted in its place. `Ok(None)` therefore means *not this
/// volume*: the caller reports the copy unavailable by its volume identity rather than
/// reading another disk's bytes. The mount is joined only after the identity matches, and a
/// key that would escape the mount is refused with `Ok(None)` for the same reason — a read
/// path never resolves to a path outside the volume it was told to trust.
///
/// This answers *where the bytes are*, not whether the mount is actually there: a tier
/// recorded mounted whose directory is absent is still the caller's check, because only the
/// caller knows whether it can say "insert the volume" (D5). The one resolver both the scrub
/// (#181) and the reconcile (#182) read an `offline` row through, so the identity rule is
/// written once.
pub fn resolve_mounted_location(
    catalog: &catalog::Catalog,
    tier: &str,
    storage_key: &str,
    mount: &Path,
) -> Result<Option<PathBuf>, OfflineError> {
    let Some((volume_id, relative)) = parse_storage_key(storage_key) else {
        return Ok(None);
    };
    let mounted = mounted_volume(catalog, tier)?;
    match mounted {
        Some(record) if record.id == volume_id => {
            let path = mount.join(&relative);
            if path.starts_with(mount) {
                Ok(Some(path))
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

/// fsync a directory so a rename into it survives a crash. Opening a directory read-only is
/// enough on the platforms this tool runs on.
fn sync_dir(dir: &Path) -> Result<(), OfflineError> {
    let handle = File::open(dir).map_err(|error| OfflineError::Io {
        stage: "open volume directory",
        path: dir.to_path_buf(),
        source: error,
    })?;
    handle.sync_all().map_err(|error| OfflineError::Io {
        stage: "fsync volume directory",
        path: dir.to_path_buf(),
        source: error,
    })
}

/// What can go wrong on the offline-volume edge.
#[derive(Debug, thiserror::Error)]
pub enum OfflineError {
    #[error("cannot {stage} {path}: {source}")]
    Io {
        stage: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the envelope for an offline copy failed to {stage}: {source}")]
    Envelope {
        stage: &'static str,
        #[source]
        source: envelope::EnvelopeError,
    },
    #[error("the copy at {stored} decrypted but does not match the recorded checksum")]
    ChecksumMismatch { stored: PathBuf },
    #[error("cannot read the catalog for an offline-volume operation: {source}")]
    Catalog {
        #[source]
        source: catalog::CatalogError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_storage_key_round_trips_through_its_volume_id() {
        let relative = Path::new("photos/2024/cat.jpg");
        let key = storage_key("drawer-01", relative);
        assert_eq!(key, "drawer-01/photos/2024/cat.jpg");
        let (volume, back) = parse_storage_key(&key).expect("the key names a volume");
        assert_eq!(volume, "drawer-01");
        assert_eq!(back, relative);
    }

    #[test]
    fn a_key_without_a_volume_is_refused_rather_than_guessed() {
        // The first segment is always the volume id; an empty one names no volume.
        assert!(parse_storage_key("/photos/cat.jpg").is_none());
        // No `/` at all: nothing separates a volume id from a path.
        assert!(parse_storage_key("drawer-01").is_none());
        // A key with a slash parses, even when the id is not a name we know: the catalog row
        // is what vouches for it.
        let (volume, relative) = parse_storage_key("drawer-01/a").expect("two segments");
        assert_eq!(volume, "drawer-01");
        assert_eq!(relative, Path::new("a"));
    }

    #[test]
    fn the_insert_prompt_names_the_tier_volume_and_mount() {
        let vaults = vec!["shelf-a".to_string(), "shelf-b".to_string()];
        let prompt = insert_prompt(
            "drawer",
            Path::new("/mnt/offline"),
            &vaults,
            "drawer-07",
            None,
        );
        assert!(prompt.contains("insert volume `drawer-07`"));
        assert!(prompt.contains("/mnt/offline"));
        assert!(prompt.contains("tier `drawer`"));
        assert!(prompt.contains("shelf-a, shelf-b"));

        let busy = insert_prompt(
            "drawer",
            Path::new("/mnt/offline"),
            &vaults,
            "drawer-07",
            Some("drawer-08"),
        );
        assert!(busy.contains("drawer-08"));
    }
}

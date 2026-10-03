//! Replicating one offloaded object across several destinations, and refusing to
//! believe a copy until its bytes have been read back.
//!
//! `docs/design.md` §6 sets the durability rule: a tier declares a copy floor, a missing
//! copy is a repair job rather than a silent loss, and **no source bytes are removed
//! until the destination copy checksums clean**. The single-copy mover in
//! [`crate::disk_management`] already compares an adopted destination by BLAKE3, but it
//! only ever places one copy and it trusts the write it just made. This module is the
//! difference between "offloaded to a disk" and "offloaded to a disk that can fail":
//!
//! * a file is copied to `floor` **distinct destination roots**, not to the first one
//!   that has room;
//! * every copy — the one just written as much as one already present — is HASHED and
//!   compared to the source's digest before it is allowed to count;
//! * a copy that lands but fails its checksum is **not** a replica. It is reported and
//!   does not satisfy the floor, and the source is kept (invariant 2).
//!
//! ## Distinctness is by configured root, not by device
//!
//! "N distinct destinations" here means N distinct `--dest` roots, keyed by
//! canonicalized path. It is deliberately *not* a `st_dev` comparison: two roots that
//! happen to share a device (two directories on one pool, or a test's two subdirectories
//! under `/dev/shm`) are still two places a copy can live, and a device check would make
//! the result depend on a property the operator cannot see in the command they ran. The
//! honest caveat is the design's own: with a floor of two and one other disk, the second
//! copy normally lands on **that other disk on the same host** — this is replication
//! across a disk failure, not off-host backup (§10). A tier that leaves the machine is
//! P3.
//!
//! ## A write that returned is not a copy we can vouch for
//!
//! `copy_into_place` returns once the partial has been renamed into place. That is
//! necessary but not sufficient: the read has not happened. Every freshly written copy
//! is therefore hashed here, and one that a digest cannot vouch for is treated as
//! **unknown**, never as a good copy. Because the source is still present at that point,
//! an unverifiable copy of our own making is dropped rather than left to masquerade as
//! data of record; a read error (as opposed to a digest mismatch) leaves the bytes but
//! still marks them unknown.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::digest;
use crate::disk_management::{self, FileEntry};
use crate::faults::{Fault, FaultMode};

/// What happened at one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaStatus {
    /// The bytes were copied and the read-back digest matched the source.
    Verified,
    /// The destination already held a byte-identical copy; it was adopted, not re-copied.
    Adopted,
    /// The destination is below the free-space floor, so it was not touched.
    NoRoom,
    /// The destination root is not a directory any more — an unmounted disk, in the
    /// failure this tool exists to survive. Nothing was written and nothing crashed.
    Unavailable,
    /// The copy could not be made, or could not be proven. This destination must not
    /// count toward the floor.
    Failed(String),
}

impl ReplicaStatus {
    /// Only a copy whose bytes matched the source counts as a replica.
    pub fn is_valid(&self) -> bool {
        matches!(self, ReplicaStatus::Verified | ReplicaStatus::Adopted)
    }

    /// True when bytes were written to the destination and not immediately removed, so
    /// the catalog may record them — as verified only when a digest vouched for them.
    pub fn bytes_left_in_place(&self) -> bool {
        matches!(self, ReplicaStatus::Verified | ReplicaStatus::Adopted)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ReplicaStatus::Verified => "verified",
            ReplicaStatus::Adopted => "adopted",
            ReplicaStatus::NoRoom => "no-room",
            ReplicaStatus::Unavailable => "unavailable",
            ReplicaStatus::Failed(_) => "failed",
        }
    }
}

/// The outcome at one destination root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaPlacement {
    pub dest_root: PathBuf,
    pub path: PathBuf,
    pub status: ReplicaStatus,
}

impl ReplicaPlacement {
    pub fn is_valid(&self) -> bool {
        self.status.is_valid()
    }
}

/// The whole result of replicating one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationOutcome {
    /// One entry per destination that was tried, in order.
    pub replicas: Vec<ReplicaPlacement>,
    /// Copies that verified. The number that must reach `floor`.
    pub verified: usize,
    pub floor: usize,
    /// The source's digest, hex-encoded, when it could be computed. This is the object
    /// identity a catalog would record for the copies.
    pub digest: Option<String>,
}

impl ReplicationOutcome {
    pub fn meets_floor(&self) -> bool {
        self.verified >= self.floor
    }

    /// The first verified replica — the one the hot path should be linked to. `None`
    /// exactly when the floor was not met, which is why the caller never links without
    /// checking [`Self::meets_floor`] first.
    pub fn primary(&self) -> Option<&Path> {
        self.replicas
            .iter()
            .find(|placement| placement.is_valid())
            .map(|placement| placement.path.as_path())
    }

    /// The destinations a copy verified on, in order.
    pub fn verified_paths(&self) -> Vec<&Path> {
        self.replicas
            .iter()
            .filter(|placement| placement.is_valid())
            .map(|placement| placement.path.as_path())
            .collect()
    }

    /// A one-line description of the placements that did not count, for a failure line.
    pub fn unmet_detail(&self) -> String {
        let mut notes = Vec::new();
        for placement in &self.replicas {
            match &placement.status {
                ReplicaStatus::Verified | ReplicaStatus::Adopted => {}
                ReplicaStatus::NoRoom => {
                    notes.push(format!("{}: no room", placement.dest_root.display()))
                }
                ReplicaStatus::Unavailable => notes.push(format!(
                    "{}: destination is gone",
                    placement.dest_root.display()
                )),
                ReplicaStatus::Failed(err) => {
                    notes.push(format!("{}: {err}", placement.dest_root.display()))
                }
            }
        }
        if notes.is_empty() {
            "no destination was tried".to_string()
        } else {
            notes.join("; ")
        }
    }
}

/// Place a verified copy of `entry` on up to `floor` distinct destination roots.
///
/// The source is never removed here — that is the caller's step, and it may only take it
/// when [`ReplicationOutcome::meets_floor`] is true. Destination roots are tried in the
/// order given (fastest first, as the sweep's contract expects), a root that cannot be
/// used is skipped rather than fatal, and the walk stops as soon as `floor` copies have
/// verified.
pub fn replicate(
    entry: &FileEntry,
    dest_roots: &[PathBuf],
    floor: usize,
    min_free: u64,
) -> ReplicationOutcome {
    let mut outcome = ReplicationOutcome {
        replicas: Vec::new(),
        verified: 0,
        floor,
        digest: None,
    };

    // One hash of the source per object, not one per destination: the identity is the
    // same for every copy and re-reading a 40 GB file once per disk would be the tool's
    // own worst enemy.
    let expected = match digest::file_digest(&entry.path) {
        Ok(hash) => hash,
        Err(err) => {
            // Without a source digest there is nothing to verify against, so no copy can
            // be trusted. Report it against every destination and place nothing.
            for root in dedupe(dest_roots) {
                outcome.replicas.push(ReplicaPlacement {
                    path: root.join(&entry.relative),
                    dest_root: root,
                    status: ReplicaStatus::Failed(format!("cannot hash the source: {err}")),
                });
            }
            return outcome;
        }
    };
    outcome.digest = Some(expected.to_hex().to_string());

    // Read once per replication. When unset this is `None` and every check below is a
    // branch on `None` — the hook is inert, not merely quiet.
    let fault = Fault::from_env();
    // Fresh copies attempted so far, in order — the ordinal `vanish-readback` and
    // `corrupt-readback` count.
    let mut fresh_copies = 0usize;

    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for root in dest_roots {
        if outcome.verified >= floor {
            break;
        }
        // Fault seam, "between copy N and copy N+1": once the ordinal's copies have
        // verified, the next root is treated exactly as one whose mount went away. The
        // check sits *before* the real `is_dir`, so the injected failure is the same
        // branch the real failure takes, just reached at a moment the filesystem cannot
        // be arranged into from outside the process.
        let injected_gone = fault.is_some_and(|fault| {
            fault.mode == FaultMode::DestinationUnavailableAfter && outcome.verified >= fault.at
        });
        if injected_gone {
            outcome.replicas.push(ReplicaPlacement {
                dest_root: root.clone(),
                path: root.join(&entry.relative),
                status: ReplicaStatus::Unavailable,
            });
            continue;
        }
        // Distinct destinations only, keyed on the canonical root so `--dest /a --dest /a/`
        // cannot be mistaken for two disks.
        let key = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if !seen.insert(key) {
            continue;
        }

        let dest = root.join(&entry.relative);

        // A destination root is never created (invariant 1): a disk that has been
        // unmounted between copies must be reported, not silently turned into a plain
        // directory on whatever filesystem its path now falls through to.
        if !root.is_dir() {
            outcome.replicas.push(ReplicaPlacement {
                dest_root: root.clone(),
                path: dest,
                status: ReplicaStatus::Unavailable,
            });
            continue;
        }

        if disk_management::destination_with_room(root, entry.allocated, min_free).is_none() {
            outcome.replicas.push(ReplicaPlacement {
                dest_root: root.clone(),
                path: dest,
                status: ReplicaStatus::NoRoom,
            });
            continue;
        }

        let status = place(entry, &dest, &expected, fault.as_ref(), &mut fresh_copies);
        if status.is_valid() {
            outcome.verified += 1;
        }
        outcome.replicas.push(ReplicaPlacement {
            dest_root: root.clone(),
            path: dest,
            status,
        });
    }

    outcome
}

/// Copy or adopt one destination, then verify what is there.
///
/// `fresh_copies` counts the freshly-written copies this replication has attempted, so
/// the read-back fault ordinals can name "the Nth copy I just wrote". It is bumped only
/// on the write branch: an adopted pre-existing copy is not a copy this sweep made and
/// there is no read-back of our own output to fault.
fn place(
    entry: &FileEntry,
    dest: &Path,
    expected: &blake3::Hash,
    fault: Option<&Fault>,
    fresh_copies: &mut usize,
) -> ReplicaStatus {
    match fs::symlink_metadata(dest) {
        Ok(metadata) => {
            // Only a regular file can be an adopted copy. A symlink is neither a
            // directory nor a copy: hashing through it would count a link whose target
            // can vanish, satisfying the floor with a name the tool cannot vouch for
            // (invariant 6). `!is_dir()` was not enough, because a symlink passes it.
            if !metadata.is_file() {
                return ReplicaStatus::Failed("destination path is not a regular file".to_string());
            }
            let size = metadata.len();
            if size != entry.size {
                return ReplicaStatus::Failed(format!(
                    "destination holds {size} bytes, the source is {}",
                    entry.size
                ));
            }
            // Same length is not the same file. An interrupted replica is byte-identical
            // and adoptable; a same-size stranger must be refused, because adopting it
            // would satisfy the floor with bytes that are not the object.
            match digest::file_digest(dest) {
                Ok(hash) if hash == *expected => ReplicaStatus::Adopted,
                Ok(hash) => ReplicaStatus::Failed(format!(
                    "same size but different content (BLAKE3 {} vs {})",
                    hash.to_hex(),
                    expected.to_hex()
                )),
                Err(err) => {
                    ReplicaStatus::Failed(format!("cannot verify the existing copy: {err}"))
                }
            }
        }
        Err(_) => {
            if let Err(err) = disk_management::copy_into_place(&entry.path, dest, entry.size) {
                return ReplicaStatus::Failed(err.to_string());
            }
            *fresh_copies += 1;
            // Fault seam, "during the read-back of copy N": the copy has been renamed
            // into place (it *is* the destination file the hash would read) and is then
            // made unreadable-as-the-object, exactly the window between "bytes written"
            // and "bytes vouched for" that no test can otherwise enter. The verification
            // below runs unchanged and fails on its own: a vanished file is a read error
            // (unknown, bytes left as-is by the mover's rules), a flipped byte is a
            // digest mismatch (dropped, since the source is still present).
            match fault {
                Some(Fault {
                    mode: FaultMode::VanishReadback,
                    at,
                }) if *at == *fresh_copies => {
                    let _ = fs::remove_file(dest);
                }
                Some(Fault {
                    mode: FaultMode::CorruptReadback,
                    at,
                }) if *at == *fresh_copies => {
                    inject_single_byte_flip(dest);
                }
                _ => {}
            }
            // A copy that merely *returned* is not a copy we can vouch for. Read it back
            // and compare digests before it is allowed to count toward the floor.
            match digest::file_digest(dest) {
                Ok(hash) if hash == *expected => ReplicaStatus::Verified,
                Ok(hash) => {
                    // Our own fresh output and demonstrably not the object. The source is
                    // still present, so dropping it cannot lose the last copy, and it
                    // stops a corrupt same-size file from being adopted as a replica
                    // later.
                    let _ = fs::remove_file(dest);
                    ReplicaStatus::Failed(format!(
                        "copy read back with a different digest ({} vs {})",
                        hash.to_hex(),
                        expected.to_hex()
                    ))
                }
                Err(err) => {
                    // A read *error* is not proof of corruption; leave the bytes in place
                    // (they are unknown, not good) and let a later sync hash them. The
                    // source is untouched either way.
                    ReplicaStatus::Failed(format!("cannot read the copy back to verify it: {err}"))
                }
            }
        }
    }
}

/// Flip one byte of the file at `dest` in place, for the `corrupt-readback` fault.
///
/// One byte, not the whole file: the point is a same-length stranger, which is the case
/// a size check would wave through, and flipping the last byte leaves the length intact
/// even when the copy is interrupted. A zero-byte copy has no byte to flip — but a
/// zero-byte source would have failed the size comparison long before here, so that is
/// unreachable for a file the mover would touch.
fn inject_single_byte_flip(dest: &Path) {
    if let Ok(mut bytes) = fs::read(dest) {
        if let Some(last) = bytes.last_mut() {
            *last ^= 0xff;
        }
        let _ = fs::write(dest, &bytes);
    }
}

/// The destination roots with duplicates removed, preserving first-seen order.
fn dedupe(dest_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out = Vec::new();
    for root in dest_roots {
        let key = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if seen.insert(key) {
            out.push(root.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn entry(path: &Path, relative: &str, size: u64) -> FileEntry {
        FileEntry {
            path: path.to_path_buf(),
            relative: PathBuf::from(relative),
            size,
            allocated: size,
            last_access: SystemTime::now(),
            is_symlink: false,
        }
    }

    #[test]
    fn two_distinct_destinations_receive_verified_copies_and_the_source_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("d1");
        let d2 = tmp.path().join("d2");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        let src = tmp.path().join("a.bin");
        fs::write(&src, b"payload bytes").unwrap();

        let outcome = replicate(&entry(&src, "a.bin", 13), &[d1.clone(), d2.clone()], 2, 0);

        assert!(outcome.meets_floor(), "{outcome:?}");
        assert_eq!(outcome.verified, 2);
        assert_eq!(fs::read(d1.join("a.bin")).unwrap(), b"payload bytes");
        assert_eq!(fs::read(d2.join("a.bin")).unwrap(), b"payload bytes");
        assert!(
            src.is_file(),
            "the source must remain until the caller retires it"
        );
    }

    #[test]
    fn a_same_size_stranger_at_a_destination_does_not_count_toward_the_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("d1");
        let d2 = tmp.path().join("d2");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        let src = tmp.path().join("a.bin");
        fs::write(&src, b"source bytes!").unwrap();
        // Same length, different content: the classic verify-before-delete trap.
        fs::write(d2.join("a.bin"), b"flipped bytes").unwrap();

        let outcome = replicate(&entry(&src, "a.bin", 13), &[d1, d2], 2, 0);

        assert!(
            !outcome.meets_floor(),
            "the stranger must not satisfy the floor"
        );
        assert_eq!(outcome.verified, 1);
        assert!(
            outcome
                .replicas
                .iter()
                .any(|placement| matches!(&placement.status, ReplicaStatus::Failed(msg) if msg.contains("different content"))),
            "{outcome:?}"
        );
        assert!(src.is_file(), "a below-floor object must keep its source");
        // The stranger is left exactly as it was; the tool does not clobber it.
        assert_eq!(
            fs::read(tmp.path().join("d2/a.bin")).unwrap(),
            b"flipped bytes"
        );
    }

    #[test]
    fn a_destination_that_disappears_is_reported_not_crashed() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("d1");
        fs::create_dir_all(&d1).unwrap();
        let gone = tmp.path().join("unmounted");
        let src = tmp.path().join("a.bin");
        fs::write(&src, b"payload").unwrap();

        let outcome = replicate(&entry(&src, "a.bin", 7), &[d1, gone], 2, 0);

        assert!(!outcome.meets_floor());
        assert_eq!(outcome.verified, 1);
        assert!(
            outcome
                .replicas
                .iter()
                .any(|placement| placement.status == ReplicaStatus::Unavailable),
            "{outcome:?}"
        );
        assert!(
            src.is_file(),
            "the surviving source is the last copy and must stay"
        );
    }

    #[test]
    fn an_identical_existing_copy_is_adopted_rather_than_recopied() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("d1");
        fs::create_dir_all(&d1).unwrap();
        let src = tmp.path().join("a.bin");
        fs::write(&src, b"same").unwrap();
        fs::write(d1.join("a.bin"), b"same").unwrap();

        let outcome = replicate(&entry(&src, "a.bin", 4), &[d1], 1, 0);
        assert!(outcome.meets_floor());
        assert_eq!(outcome.replicas[0].status, ReplicaStatus::Adopted);
    }

    /// A symlink at a destination is not a copy, even when it resolves to byte-identical
    /// bytes: `place` must not return `Adopted`, and the link must not satisfy the floor.
    ///
    /// The symlink's *target text* is exactly the source's size and its target hashes to the
    /// object, so the old `!is_dir()` check — which compares the link's length and then hashes
    /// through the link — adopted it. Requiring `is_file()` refuses it.
    #[test]
    fn a_symlink_at_a_destination_is_not_adopted_and_does_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("d1");
        let d2 = tmp.path().join("d2");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();

        let link_text = "identical.bin";
        let bytes: Vec<u8> = (0..link_text.len()).map(|i| (i % 251 + 1) as u8).collect();
        let src = tmp.path().join("a.bin");
        fs::write(&src, &bytes).unwrap();
        // The link resolves, relative to its own directory, to a byte-identical file.
        fs::write(d2.join(link_text), &bytes).unwrap();
        std::os::unix::fs::symlink(link_text, d2.join("a.bin")).unwrap();
        assert_eq!(
            fs::symlink_metadata(d2.join("a.bin")).unwrap().len(),
            bytes.len() as u64,
            "the link's own length must equal the source's for this test to mean anything"
        );

        let outcome = replicate(
            &entry(&src, "a.bin", bytes.len() as u64),
            &[d1, d2.clone()],
            2,
            0,
        );

        assert!(
            !outcome.meets_floor(),
            "a symlink must not satisfy the floor: {outcome:?}"
        );
        assert_eq!(outcome.verified, 1, "only the real copy counts");
        assert_eq!(
            outcome.replicas.iter().filter(|p| p.is_valid()).count(),
            1,
            "the symlink placement must not be valid: {outcome:?}"
        );
        assert!(
            outcome
                .replicas
                .iter()
                .any(|placement| placement.dest_root == d2
                    && matches!(&placement.status, ReplicaStatus::Failed(msg) if msg.contains("not a regular file"))),
            "{outcome:?}"
        );
        assert!(
            fs::symlink_metadata(d2.join("a.bin")).unwrap().is_symlink(),
            "the symlink is left exactly as it was"
        );
    }

    #[test]
    fn the_same_root_twice_is_one_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let d1 = tmp.path().join("d1");
        fs::create_dir_all(&d1).unwrap();
        let src = tmp.path().join("a.bin");
        fs::write(&src, b"payload").unwrap();

        let outcome = replicate(&entry(&src, "a.bin", 7), &[d1.clone(), d1], 2, 0);
        assert!(
            !outcome.meets_floor(),
            "two names for one disk are not two disks"
        );
        assert_eq!(outcome.verified, 1);
    }
}

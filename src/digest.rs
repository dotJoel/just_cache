//! Content digests, in one place.
//!
//! BLAKE3 is the house hash: the design specifies it for the catalog, and the mover
//! already compares destinations with it, so a file has one identity across the whole
//! tool instead of a different digest per code path. (The audit command originally
//! carried its own hand-written SHA-256. That is more cryptography to own than catching
//! a corrupt copy is worth, and it meant two answers to "is this the same file" — the
//! mover comparing BLAKE3 digests while audit compared SHA-256 ones.)
//!
//! Read in bounded chunks rather than slurped: these are the files the tool exists for.
//!
//! # The tool never marks its own use
//!
//! The mover's idle signal is atime, and on a mount that maintains atime under the
//! `relatime` rule — every normal Linux mount — any read refreshes it. A hash is not a
//! use: if `catalog sync`, `scrub`'s verify or `audit`'s probe counted, a sync would make
//! every file it read look freshly used and postpone its move by a whole idle window (#129).
//! So every internal, read-only open goes through [`open_for_hashing`], which asks for
//! `O_NOATIME`. The kernel only grants that flag to the file's owner (or `CAP_FOWNER`);
//! anything else gets `EPERM`, and the read falls back to a normal open, because a hash
//! that cannot be taken is a worse failure than an atime the tool could not avoid touching.
//!
//! Deliberately *not* routed here: the mover's copy of a source (the source is deleted
//! once the copy verifies, and the copy's times are set from the source's own metadata)
//! and `restore`'s copy back from a cold tier (a restore is the user asking for the
//! file — that one *is* use).

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use blake3::Hasher;

/// How much is read at a time. Large enough that the syscall overhead disappears, small
/// enough that hashing a 40 GB video stays flat in memory.
const CHUNK: usize = 128 * 1024;

/// BLAKE3 digest of a file's contents.
pub fn file_digest(path: &Path) -> io::Result<blake3::Hash> {
    file_digest_reading(path, |_| {})
}

/// The same digest, with a callback after every chunk that was read.
///
/// `scrub` needs to stay inside an I/O budget while it hashes a whole tier, and the
/// honest way to do that is to throttle the one read loop rather than to grow a second
/// hasher beside it: two read loops is how the tool ends up with two answers to "is
/// this the same file" (`disk_management::same_contents` already had to be folded back
/// onto this module for exactly that reason). The callback gets the number of bytes
/// just read and is expected to return promptly; a rate limiter turns that into a sleep.
pub fn file_digest_reading<F>(path: &Path, mut on_read: F) -> io::Result<blake3::Hash>
where
    F: FnMut(usize),
{
    let mut file = open_for_hashing(path)?;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match file.read(&mut buffer)? {
            0 => break,
            read => {
                hasher.update(&buffer[..read]);
                on_read(read);
            }
        }
    }
    Ok(hasher.finalize())
}

/// Open `path` read-only for the tool's own reading, without refreshing its atime.
///
/// `O_NOATIME` where the kernel grants it; a plain open where it refuses with `EPERM`
/// (the tool does not own the file). See the module docs for why this is the rule for
/// every internal reader rather than a fix to one call site.
pub fn open_for_hashing(path: &Path) -> io::Result<File> {
    let file = open_noatime(path)?;
    relatime_seam(path, &file);
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_noatime(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOATIME.bits() as i32)
        .open(path)
    {
        Err(error) if error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error()) => {
            File::open(path)
        }
        other => other,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn open_noatime(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// `JUST_CACHE_FAULT=relatime=1`: behave as a `relatime` mount would, on any mount.
///
/// Whether a read refreshes atime is the mount's decision, so a test of "the tool never
/// marks its own use" would only fail on a mount that maintains atime — and pass, blind,
/// on the `noatime` box the #45 pin tests were written on (#124). The seam takes the
/// mount out of it: when the descriptor was opened *without* `O_NOATIME`, it stamps atime
/// to now exactly as the kernel would on the first read under `relatime`. A descriptor
/// that carries the flag is left alone, as the kernel leaves it alone. Inert unless set.
fn relatime_seam(path: &Path, file: &File) {
    match crate::faults::Fault::from_env() {
        Some(crate::faults::Fault {
            mode: crate::faults::FaultMode::Relatime,
            ..
        }) => {}
        _ => return,
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let flags = rustix::fs::fcntl_getfl(file).expect("relatime seam: F_GETFL");
        if flags.contains(rustix::fs::OFlags::NOATIME) {
            return;
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = file;
    let now = std::time::SystemTime::now();
    // Through a fresh write handle: setting an explicit time needs one, and the fault is
    // only ever armed on a test's own files.
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_times(std::fs::FileTimes::new().set_accessed(now)))
        .expect("relatime seam: stamp atime");
}

/// Digest of an in-memory buffer.
pub fn bytes_digest(data: &[u8]) -> blake3::Hash {
    blake3::hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_matches_the_published_vector_for_empty_input() {
        // The BLAKE3 reference digest of the empty input, so a wrong implementation
        // cannot agree with itself.
        assert_eq!(
            bytes_digest(b"").to_hex().as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn a_file_digest_matches_the_same_bytes_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("payload.bin");
        // Larger than one chunk, so the loop is exercised more than once.
        let payload: Vec<u8> = (0..(CHUNK * 2 + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).unwrap();
        assert_eq!(file_digest(&path).unwrap(), bytes_digest(&payload));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a_hashing_open_carries_noatime_when_the_tool_owns_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("payload.bin");
        std::fs::write(&path, b"owned").unwrap();
        let file = open_for_hashing(&path).unwrap();
        let flags = rustix::fs::fcntl_getfl(&file).unwrap();
        assert!(
            flags.contains(rustix::fs::OFlags::NOATIME),
            "an internal read must not be able to refresh atime: {flags:?}"
        );
    }

    #[test]
    fn different_bytes_have_different_digests() {
        assert_ne!(bytes_digest(b"one"), bytes_digest(b"two"));
    }
}

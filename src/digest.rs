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

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use blake3::Hasher;

/// How much is read at a time. Large enough that the syscall overhead disappears, small
/// enough that hashing a 40 GB video stays flat in memory.
const CHUNK: usize = 128 * 1024;

/// BLAKE3 digest of a file's contents.
pub fn file_digest(path: &Path) -> io::Result<blake3::Hash> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match file.read(&mut buffer)? {
            0 => break,
            read => {
                hasher.update(&buffer[..read]);
            }
        }
    }
    Ok(hasher.finalize())
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
    fn different_bytes_have_different_digests() {
        assert_ne!(bytes_digest(b"one"), bytes_digest(b"two"));
    }
}

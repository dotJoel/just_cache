//! The envelope: authenticated encryption for bytes that cross the machine boundary.
//!
//! §2 rule 2: "anything crossing the machine boundary is encrypted first." A cloud object
//! tier, a LAN peer and an exported offline volume all cross — the bytes leave the host in
//! the first two cases and the volume itself leaves it in the third, so the envelope is one
//! requirement shared by every P3 driver, not a wrap added after one of them works (#140).
//!
//! # The choices this module records (the issue asked each be decided, with a reason)
//!
//! **The cipher is XChaCha20-Poly1305** (`chacha20poly1305` + `rand_core`'s OsRng via
//! `aead::OsRng`): pure Rust, no `unsafe`, no third-party assembly. AES-GCM via `ring`
//! would have been equally correct but drags in its build machinery, and the tool's only
//! existing "cryptography" is BLAKE3 — one house hash for identity, one cipher for secrecy.
//!
//! **Chunked AEAD, 64 KiB plaintext per chunk.** One AEAD over a whole 40 GB file would
//! need the whole file in memory to encrypt or decrypt, and one corrupted byte would make
//! the *whole file* unreadable instead of one chunk. A chunk boundary the driver can seek
//! to is what makes a resumable upload a local property rather than a redesign: upload what
//! is uploaded, resume from the first missing chunk. 64 KiB keeps per-chunk overhead (the
//! 16-byte tag) near 0.03 % while a 40 GB object is 640 000 chunks — large enough that
//! per-chunk cost disappears, small enough to stream.
//!
//! **One key per tier, from a file or the environment.** Never argv: a key on the command
//! line is readable by every process on the host through `/proc/<pid>/cmdline` (invariant
//! 3's posture applied to key material). Never KMS here: one configured key is the scope of
//! this issue, and a key-management service is a different trust boundary, not a bigger key.
//!
//! **A fresh random 24-byte nonce per object; each chunk's nonce is that prefix plus the
//! chunk index.** Chunks stay independently decryptable (what resume and scrub want)
//! without 24 random bytes apiece, and no counter state has to persist across an
//! interrupted upload: a resumed envelope simply has whatever chunks landed, each
//! self-describing. The per-chunk AAD is the chunk index, so two chunks swapped in the
//! stream fail their tags rather than decrypting into each other's places.
//!
//! **The format is versioned and self-naming.** Header: magic `JCV`, version byte 1, the
//! promised plaintext length, then `<length-prefix><ciphertext+tag>` chunks. A stored
//! object says which scheme wrote it, so the format can change (a V2, another cipher)
//! without a migration guess: read the header, refuse an unknown version by name.
//!
//! # What the catalog still verifies
//!
//! The recorded digest is BLAKE3 of the **plaintext** — the file's identity, unchanged by
//! where it lives or whether it was encrypted on the way. A decrypt-then-hash read-back
//! verifies a remote copy exactly the way a local copy is verified, and scrub reports the
//! two failures separately: *wrong bytes* (decrypted fine, digest mismatch — the ciphertext
//! survived, the contents did not) versus *cannot decrypt* (a tag failure — wrong key,
//! tampered or truncated). A key problem must never read as bitrot, because the operator
//! actions differ: one is "where is a good copy", the other is "where is the key".
//!
//! # What never leaks
//!
//! Key material never reaches a log line, an error message, `--json`, or a catalog row:
//! errors name the stage (`cannot decrypt`, `wrong key`), never the key or its file's
//! contents, and `Key`'s `Debug` writes `Key(hidden)`. The key is zeroised on drop.
//!
//! # Interrupted work
//!
//! This module places no bytes and adopts nothing, so it cannot violate "resumable or
//! discardable, never adopted" — that invariant belongs to the driver that places bytes
//! (the object-store issue states it). What the envelope guarantees is that a partial
//! stream is *detectable*: a truncated chunk fails its tag rather than decrypting into
//! silent garbage, and a length-prefix catches a stream cut short before the last chunk.

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};

/// How much plaintext one chunk carries.
const CHUNK: usize = 64 * 1024;
/// The Poly1305 tag: what each chunk carries beyond its plaintext.
const TAG: usize = 16;
/// A header's length field is `u64`, but a chunk index is 8 bytes of the nonce and a chunk
/// length is a `u32`; the practical ceiling is far below `u64::MAX`, and refusing a size
/// the format never meant to carry beats overflowing a length somewhere in the middle.
const MAX_PLAINTEXT: u64 = 1 << 50; // 1 PiB

/// Magic + version: the first four bytes of every envelope. A stored object names the
/// scheme it was written with; `JCV` + version 1 is this format.
const MAGIC: [u8; 3] = *b"JCV";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 3 + 1 + 8;

/// A key: exactly 32 bytes, zeroised when dropped.
pub struct Key(chacha20poly1305::Key);

impl Key {
    /// Exactly 32 bytes; anything else is refused rather than hashed or padded, because a
    /// key whose length the tool guessed is a key the tool made up.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        if bytes.len() != 32 {
            return Err(EnvelopeError::KeyLength(bytes.len()));
        }
        Ok(Key(<chacha20poly1305::Key as TryFrom<&[u8]>>::try_from(
            bytes,
        )
        .map_err(|_| EnvelopeError::KeyLength(bytes.len()))?))
    }

    /// 64 hex characters, as a key file or env var holds it. Whitespace around the value
    /// is trimmed; whitespace inside is refused by the length check.
    pub fn from_hex(text: &str) -> Result<Self, EnvelopeError> {
        let text = text.trim();
        if text.len() != 64 {
            return Err(EnvelopeError::KeyLength(text.len()));
        }
        let mut bytes = [0u8; 32];
        for (i, pair) in text.as_bytes().chunks(2).enumerate() {
            let half = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            match (half(pair[0]), half(pair[1])) {
                (Some(hi), Some(lo)) => bytes[i] = hi << 4 | lo,
                _ => return Err(EnvelopeError::KeyLength(text.len())),
            }
        }
        Self::from_bytes(&bytes)
    }
}

impl fmt::Debug for Key {
    /// `Key(hidden)` and nothing else: a key's debug output is a log line waiting to happen.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(hidden)")
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

/// Why an envelope operation failed. Every variant names the stage, never the key: the
/// reader of the error needs to know *which tier to look at*, not *what the key was*.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("a key is exactly 32 bytes (64 hex characters), got {0}")]
    KeyLength(usize),
    #[error("cannot decrypt: the stream does not start with the envelope magic")]
    NotAnEnvelope,
    #[error("cannot decrypt: envelope version {0} is not supported (this tool writes version {VERSION})")]
    UnsupportedVersion(u8),
    #[error("cannot decrypt: the header is truncated")]
    TruncatedHeader,
    #[error("cannot decrypt chunk {0}: the data failed its integrity check (wrong key, or tampered/truncated bytes)")]
    Chunk(u64),
    #[error("cannot decrypt: the stream ended mid-chunk at chunk {0}")]
    TruncatedChunk(u64),
    #[error(
        "cannot decrypt: the header promised {promised} plaintext bytes, the stream produced {got}"
    )]
    LengthMismatch { promised: u64, got: u64 },
    #[error("the plaintext is larger than this format can express")]
    TooLarge,
    #[error("io error in the envelope's {stage}: {source}")]
    Io {
        stage: &'static str,
        #[source]
        source: io::Error,
    },
}

fn write_header(out: &mut impl Write, plaintext_len: u64) -> Result<(), EnvelopeError> {
    out.write_all(&MAGIC)
        .and_then(|_| out.write_all(&[VERSION]))
        .and_then(|_| out.write_all(&plaintext_len.to_le_bytes()))
        .map_err(|source| EnvelopeError::Io {
            stage: "header write",
            source,
        })
}

struct Header {
    plaintext_len: u64,
}

fn read_header(input: &mut impl Read) -> Result<Header, EnvelopeError> {
    let mut bytes = [0u8; HEADER_LEN];
    match input.read_exact(&mut bytes) {
        Ok(()) => {}
        // A zero-byte read is a different refusal from a half-header: an empty file is
        // "not an envelope" (nothing was ever written here), a cut header is "truncated".
        Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(EnvelopeError::NotAnEnvelope)
        }
        Err(source) => {
            return Err(EnvelopeError::Io {
                stage: "header read",
                source,
            })
        }
    }
    if bytes[0..3] != MAGIC {
        return Err(EnvelopeError::NotAnEnvelope);
    }
    if bytes[3] != VERSION {
        return Err(EnvelopeError::UnsupportedVersion(bytes[3]));
    }
    Ok(Header {
        plaintext_len: u64::from_le_bytes(bytes[4..12].try_into().unwrap()),
    })
}

/// The per-chunk nonce: the object's fresh random 24-byte prefix for the first 12 bytes,
/// then 8 bytes of chunk index.
fn chunk_nonce(object: &XNonce, index: u64) -> XNonce {
    let mut bytes = [0u8; 24];
    bytes[..12].copy_from_slice(&object.as_slice()[..12]);
    bytes[12..20].copy_from_slice(&index.to_le_bytes());
    XNonce::from(bytes)
}

/// One chunk, encrypted. The chunk index is the additional data, so two chunks swapped in
/// the stream fail their tags rather than decrypting into each other's places.
fn seal_chunk(
    cipher: &XChaCha20Poly1305,
    object: &XNonce,
    index: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    cipher
        .encrypt(
            &chunk_nonce(object, index),
            Payload {
                msg: plaintext,
                aad: &index.to_le_bytes(),
            },
        )
        .map_err(|_| EnvelopeError::TooLarge)
}

fn open_chunk(
    cipher: &XChaCha20Poly1305,
    object: &XNonce,
    index: u64,
    ciphertext: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    cipher
        .decrypt(
            &chunk_nonce(object, index),
            Payload {
                msg: ciphertext,
                aad: &index.to_le_bytes(),
            },
        )
        .map_err(|_| EnvelopeError::Chunk(index))
}

/// Encrypt the whole reader into the writer as one envelope, returning the number of
/// plaintext bytes sealed. The source must be seekable — the drivers read from files, and
/// the header (which promises the plaintext length, so a truncated stream is detectable at
/// the end, not just per-chunk) needs the length before the first byte is written. A fresh
/// random nonce-prefix is drawn per call, so two objects encrypted from the same plaintext
/// under the same key are different ciphertexts without any counter state to persist.
pub fn encrypt_all(
    key: &Key,
    plaintext: &mut (impl Read + Seek),
    mut out: impl Write,
) -> Result<u64, EnvelopeError> {
    let promised = plaintext
        .seek(SeekFrom::End(0))
        .map_err(|source| EnvelopeError::Io {
            stage: "length probe",
            source,
        })?;
    plaintext.rewind().map_err(|source| EnvelopeError::Io {
        stage: "length probe",
        source,
    })?;
    if promised > MAX_PLAINTEXT {
        return Err(EnvelopeError::TooLarge);
    }
    write_header(&mut out, promised)?;

    let cipher = XChaCha20Poly1305::new(&key.0);
    let mut prefix = [0u8; 24];
    getrandom::fill(&mut prefix).map_err(|source| EnvelopeError::Io {
        stage: "nonce draw",
        source: io::Error::other(source.to_string()),
    })?;
    out.write_all(&prefix).map_err(|source| EnvelopeError::Io {
        stage: "nonce write",
        source,
    })?;
    let object = XNonce::from(prefix);

    let mut chunk = vec![0u8; CHUNK];
    let mut index: u64 = 0;
    let mut total: u64 = 0;
    loop {
        let read = read_chunk(plaintext, &mut chunk)?;
        if read == 0 {
            break;
        }
        let sealed = seal_chunk(&cipher, &object, index, &chunk[..read])?;
        out.write_all(&(sealed.len() as u32).to_le_bytes())
            .and_then(|_| out.write_all(&sealed))
            .map_err(|source| EnvelopeError::Io {
                stage: "chunk write",
                source,
            })?;
        index += 1;
        total += read as u64;
    }
    if total != promised {
        // The source changed size between the probe and the read: exactly the condition the
        // mover refuses a move for (invariant 3), and here it must be refused too, because
        // the header has already promised the probed length.
        return Err(EnvelopeError::LengthMismatch {
            promised,
            got: total,
        });
    }
    Ok(total)
}

/// Read up to a full chunk, returning 0 at end of stream.
fn read_chunk(source: &mut impl Read, buffer: &mut [u8]) -> Result<usize, EnvelopeError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source
            .read(&mut buffer[filled..])
            .map_err(|source| EnvelopeError::Io {
                stage: "plaintext read",
                source,
            })? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Decrypt a whole envelope from the reader into the writer, returning the number of
/// plaintext bytes produced. Fails on the first chunk that does not verify — the answer to
/// "is this the right key" arrives at the first chunk, not the last — and on a stream that
/// stops short of the length the header promised.
pub fn decrypt_all(
    key: &Key,
    mut input: impl Read,
    mut out: impl Write,
) -> Result<u64, EnvelopeError> {
    let cipher = XChaCha20Poly1305::new(&key.0);
    let header = read_header(&mut input)?;
    let mut prefix = [0u8; 24];
    input
        .read_exact(&mut prefix)
        .map_err(|source| EnvelopeError::Io {
            stage: "nonce read",
            source,
        })?;
    let object = XNonce::from(prefix);

    let mut sealed_len = [0u8; 4];
    let mut produced: u64 = 0;
    let mut index: u64 = 0;
    loop {
        match input.read_exact(&mut sealed_len) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => {
                // End of stream exactly at a chunk boundary: fine only if nothing was
                // promised beyond what was produced.
                if produced < header.plaintext_len {
                    return Err(EnvelopeError::LengthMismatch {
                        promised: header.plaintext_len,
                        got: produced,
                    });
                }
                return Ok(produced);
            }
            Err(source) => {
                return Err(EnvelopeError::Io {
                    stage: "chunk length read",
                    source,
                })
            }
        }
        let len = u32::from_le_bytes(sealed_len) as usize;
        if len > CHUNK + TAG {
            return Err(EnvelopeError::Chunk(index));
        }
        let mut sealed = vec![0u8; len];
        match input.read_exact(&mut sealed) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(EnvelopeError::TruncatedChunk(index))
            }
            Err(source) => {
                return Err(EnvelopeError::Io {
                    stage: "chunk read",
                    source,
                })
            }
        }
        let plain = open_chunk(&cipher, &object, index, &sealed)?;
        out.write_all(&plain).map_err(|source| EnvelopeError::Io {
            stage: "plaintext write",
            source,
        })?;
        index += 1;
        produced += plain.len() as u64;
    }
}

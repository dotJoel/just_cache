//! The envelope, exercised end to end: round trip, tamper, wrong key, missing key, and the
//! digest contract (§2: the recorded digest is BLAKE3 of the *plaintext*, so a remote copy
//! verifies exactly the way a local one does).
//!
//! These are the tests the issue names, plus the ones the format's reasons depend on: a
//! swapped chunk fails (the AAD binds a chunk to its position), a truncated stream is
//! detected (the header promises the length), and two encryptions of the same plaintext
//! differ (a fresh random nonce per object — no counter state to persist across an
//! interrupted upload).

use std::io::Cursor;

use just_cache::envelope::{decrypt_all, encrypt_all, EnvelopeError, Key};
use just_cache::file_digest;

const CHUNK: usize = 64 * 1024;

fn key() -> Key {
    // A fixed test key is fine here: the point is that the *wrong* one fails, not that the
    // right one is secret from the test itself.
    Key::from_hex("3d1f4a6b2c8e5f704a9d6c1b8e3f2a5d7c9b0e1f4a6d8c2b3e5f7a9d1c3b5e7f").unwrap()
}

fn other_key() -> Key {
    Key::from_hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f").unwrap()
}

fn plaintext(len: usize) -> Vec<u8> {
    // Not zeros: a constant plaintext would let a broken cipher pass a round trip, because
    // the output would still look like the input after a partial decrypt.
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

#[test]
fn a_round_trip_returns_the_plaintext() {
    for len in [0, 1, 100, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
        let data = plaintext(len);
        let mut sealed = Vec::new();
        let sealed_len = encrypt_all(&key(), &mut Cursor::new(&data), &mut sealed).unwrap();
        assert_eq!(sealed_len, len as u64);

        let mut opened = Vec::new();
        let opened_len = decrypt_all(&key(), &mut Cursor::new(&sealed), &mut opened).unwrap();
        assert_eq!(opened_len, len as u64, "len {len}");
        assert_eq!(opened, data, "len {len}");
    }
}

#[test]
fn the_digest_of_a_decrypted_copy_is_the_plaintext_digest() {
    let data = plaintext(CHUNK + 999);
    let mut sealed = Vec::new();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut sealed).unwrap();

    // Write the envelope to a file and the plaintext to a file: the catalog records the
    // digest of the local plaintext, and the driver's read-back hashes what it decrypted.
    let dir = tempfile::tempdir().unwrap();
    let envelope_path = dir.path().join("envelope");
    let plain_path = dir.path().join("plain");
    std::fs::write(&envelope_path, &sealed).unwrap();
    std::fs::write(&plain_path, &data).unwrap();

    let mut opened = Vec::new();
    decrypt_all(&key(), &mut Cursor::new(&sealed), &mut opened).unwrap();
    let decrypted_path = dir.path().join("decrypted");
    std::fs::write(&decrypted_path, &opened).unwrap();

    let recorded = file_digest(&plain_path).unwrap();
    assert_eq!(file_digest(&decrypted_path).unwrap(), recorded);
}

#[test]
fn a_tampered_chunk_fails_its_check() {
    let data = plaintext(CHUNK + 100);
    let mut sealed = Vec::new();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut sealed).unwrap();

    // Flip one bit of the last chunk's ciphertext: the tag must catch it, not the length.
    let last = sealed.len() - 1;
    sealed[last] ^= 1;
    let err = decrypt_all(&key(), &mut Cursor::new(&sealed), &mut Vec::new()).unwrap_err();
    match err {
        EnvelopeError::Chunk(_) => {}
        other => panic!("expected a chunk failure, got {other}"),
    }
}

#[test]
fn a_swapped_chunk_fails_its_check() {
    let data = plaintext(3 * CHUNK);
    let mut sealed = Vec::new();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut sealed).unwrap();

    // Swap two whole chunks: each carries a valid tag, but each tag binds the chunk to its
    // index, so neither verifies in the other's place. A stream whose chunks were merely
    // re-ordered is not "the same bytes, shuffled" — it is corrupt.
    let header = 3 + 1 + 8 + 24;
    let chunk_len = 4 + CHUNK + 16;
    let start = header;
    let first = sealed[start..start + chunk_len].to_vec();
    let second = sealed[start + chunk_len..start + 2 * chunk_len].to_vec();
    sealed[start..start + chunk_len].copy_from_slice(&second);
    sealed[start + chunk_len..start + 2 * chunk_len].copy_from_slice(&first);
    let err = decrypt_all(&key(), &mut Cursor::new(&sealed), &mut Vec::new()).unwrap_err();
    assert!(
        matches!(err, EnvelopeError::Chunk(_)),
        "a swapped chunk must fail its tag, got {err}"
    );
}

#[test]
fn a_wrong_key_is_a_named_refusal_not_a_panic() {
    let data = plaintext(5000);
    let mut sealed = Vec::new();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut sealed).unwrap();

    let err = decrypt_all(&other_key(), &mut Cursor::new(&sealed), &mut Vec::new()).unwrap_err();
    match err {
        EnvelopeError::Chunk(0) => {} // the first chunk is where the answer arrives
        other => panic!("expected a chunk failure at index 0, got {other}"),
    }
    // The message names the stage and the chunk, never the key material that was tried.
    assert!(!err.to_string().contains("0001"), "{err}");
}

#[test]
fn a_truncated_stream_is_detected_at_the_end() {
    let data = plaintext(2 * CHUNK + 11);
    let mut sealed = Vec::new();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut sealed).unwrap();

    // Cut the tail off mid-final-chunk.
    let err = decrypt_all(
        &key(),
        &mut Cursor::new(&sealed[..sealed.len() - 5]),
        &mut Vec::new(),
    )
    .unwrap_err();
    match err {
        EnvelopeError::TruncatedChunk(2) => {}
        other => panic!("expected a truncation at chunk 2, got {other}"),
    }

    // Cut it off exactly at a chunk boundary: no tag to fail, so the promised length is
    // what catches it.
    let err = decrypt_all(
        &key(),
        &mut Cursor::new(&sealed[..sealed.len() - (11 + 16 + 4)]),
        &mut Vec::new(),
    )
    .unwrap_err();
    match err {
        EnvelopeError::LengthMismatch { .. } => {}
        other => panic!("expected a length mismatch, got {other}"),
    }
}

#[test]
fn not_an_envelope_and_a_future_version_are_named() {
    // An empty stream was never an envelope; random bytes were never written by this tool.
    for garbage in [Vec::new(), b"movie bytes".to_vec()] {
        let err = decrypt_all(&key(), &mut Cursor::new(garbage), &mut Vec::new()).unwrap_err();
        assert!(
            matches!(err, EnvelopeError::NotAnEnvelope),
            "plain bytes are not an envelope, got {err}"
        );
    }

    let data = plaintext(10);
    let mut sealed = Vec::new();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut sealed).unwrap();
    sealed[3] = 2; // a future version's byte
    let err = decrypt_all(&key(), &mut Cursor::new(&sealed), &mut Vec::new()).unwrap_err();
    match err {
        EnvelopeError::UnsupportedVersion(2) => {}
        other => panic!("expected an unsupported version, got {other}"),
    }
}

#[test]
fn the_same_plaintext_twice_is_two_different_envelopes() {
    let data = plaintext(4096);
    let mut first = Vec::new();
    let mut second = Vec::new();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut first).unwrap();
    encrypt_all(&key(), &mut Cursor::new(&data), &mut second).unwrap();
    assert_ne!(
        first, second,
        "a fresh random nonce per object: no counter state to persist, so no reuse"
    );
    // And both still open to the same plaintext.
    for envelope in [&first, &second] {
        let mut opened = Vec::new();
        decrypt_all(&key(), &mut Cursor::new(envelope), &mut opened).unwrap();
        assert_eq!(opened, data);
    }
}

#[test]
fn a_bad_key_is_refused_never_guessed() {
    assert!(matches!(
        Key::from_bytes(&[0u8; 31]),
        Err(EnvelopeError::KeyLength(31))
    ));
    assert!(matches!(
        Key::from_hex("too short"),
        Err(EnvelopeError::KeyLength(_))
    ));
    assert!(Key::from_hex("gg").is_err());
    // A valid hex key works, and its Debug never shows it.
    let k = Key::from_hex("  3D1F4A6B2C8E5F704A9D6C1B8E3F2A5D7C9B0E1F4A6D8C2B3E5F7A9D1C3B5E7F \n")
        .unwrap();
    assert_eq!(format!("{k:?}"), "Key(hidden)");
}

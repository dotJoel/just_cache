//! End-to-end behaviour of the cross-device copy: metadata, sparseness and
//! checksum-based adoption, exercised against a real second filesystem so the
//! `EXDEV` path the mover takes in production actually runs.
//!
//! The sparse and xattr checks skip cleanly when the host offers no second
//! filesystem, or offers one that cannot represent holes/attributes — but on any
//! Linux box with `/tmp` on a different mount than `$TMPDIR` they run for real.
#![cfg(unix)]

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use just_cache::disk_management::{self, DiskError, MoveOutcome};

/// Two temporary directories that are guaranteed to be on *different* filesystems,
/// or `None` when the host only has one. A pair is required to force `fs::rename` to
/// fail with `EXDEV` and so reach the copy path at all.
fn cross_device_pair() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
    let candidates: Vec<PathBuf> = vec![
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/dev/shm"),
    ];

    for from in &candidates {
        for to in &candidates {
            if from == to {
                continue;
            }
            let Ok(first) = tempfile::Builder::new().prefix("jc-a-").tempdir_in(from) else {
                continue;
            };
            let Ok(second) = tempfile::Builder::new().prefix("jc-b-").tempdir_in(to) else {
                continue;
            };
            let first_dev = fs::metadata(first.path()).map(|m| m.dev()).ok();
            let second_dev = fs::metadata(second.path()).map(|m| m.dev()).ok();
            if first_dev.is_some() && first_dev != second_dev {
                return Some((first, second));
            }
        }
    }
    None
}

fn write_sparse(path: &Path, apparent: u64, marker: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(marker)?;
    file.seek(SeekFrom::Start(32 * 1024 * 1024))?;
    file.write_all(marker)?;
    file.set_len(apparent)?;
    file.sync_all()
}

#[test]
fn a_cross_device_move_preserves_mode_ownership_xattrs_and_mtime() {
    let Some((source_tmp, dest_tmp)) = cross_device_pair() else {
        eprintln!("skipping: no second filesystem to cross");
        return;
    };
    let watch = source_tmp.path().join("hot");
    let cold = dest_tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();

    let src = watch.join("episode.mkv");
    fs::write(&src, b"a whole episode, minus the metadata bug").unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();

    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::options()
        .write(true)
        .open(&src)
        .unwrap()
        .set_times(fs::FileTimes::new().set_accessed(mtime).set_modified(mtime))
        .unwrap();

    if rustix::fs::setxattr(
        &src,
        "user.just_cache_test",
        b"kept",
        rustix::fs::XattrFlags::empty(),
    )
    .is_err()
    {
        eprintln!("skipping: source filesystem cannot hold a test xattr");
        return;
    }

    // Precondition: the two roots really are on different devices, so this move cannot
    // have gone through `rename`.
    assert_ne!(
        fs::metadata(&watch).unwrap().dev(),
        fs::metadata(&cold).unwrap().dev(),
        "the pair must straddle a mount point"
    );

    let src_metadata = fs::metadata(&src).unwrap();
    let entries = disk_management::list_files_recursive(&watch).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.relative == Path::new("episode.mkv"))
        .unwrap();
    let outcome = disk_management::move_file_with_symlink(&cold, entry).unwrap();
    assert_eq!(outcome, MoveOutcome::Moved);

    let dest = cold.join("episode.mkv");
    let dest_metadata = fs::metadata(&dest).unwrap();

    assert_eq!(
        dest_metadata.mode() & 0o7777,
        0o640,
        "the copy must carry the source mode, not the process umask"
    );
    assert_eq!(
        (dest_metadata.uid(), dest_metadata.gid()),
        (src_metadata.uid(), src_metadata.gid()),
        "ownership is preserved (when running as root this is a real chown)"
    );
    assert_eq!(
        dest_metadata.modified().unwrap(),
        mtime,
        "mtime must survive the copy"
    );
    assert_eq!(
        fs::read(&dest).unwrap(),
        b"a whole episode, minus the metadata bug"
    );

    let mut value = [0u8; 32];
    let len = rustix::fs::getxattr(&dest, "user.just_cache_test", &mut value).unwrap();
    assert_eq!(&value[..len], b"kept", "xattrs must cross the device");

    assert!(
        fs::symlink_metadata(&src).unwrap().is_symlink(),
        "the source path must be left as a symlink"
    );
    assert_eq!(fs::read(&src).unwrap(), fs::read(&dest).unwrap());
}

#[test]
fn a_cross_device_move_keeps_a_sparse_file_sparse() {
    let Some((source_tmp, dest_tmp)) = cross_device_pair() else {
        eprintln!("skipping: no second filesystem to cross");
        return;
    };
    let watch = source_tmp.path().join("hot");
    let cold = dest_tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();

    let apparent: u64 = 1 << 30; // 1 GiB apparent, ~32 KiB real
    let marker = vec![0x5au8; 16 * 1024];
    let src = watch.join("vm-image.qcow2");
    write_sparse(&src, apparent, &marker).unwrap();

    let src_metadata = fs::metadata(&src).unwrap();
    let src_allocated = src_metadata.blocks() * 512;
    if src_allocated == 0 || src_allocated >= apparent / 2 {
        eprintln!("skipping: source filesystem does not represent holes sparsely");
        return;
    }

    let entries = disk_management::list_files_recursive(&watch).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.relative == Path::new("vm-image.qcow2"))
        .unwrap();
    let outcome = disk_management::move_file_with_symlink(&cold, entry).unwrap();
    assert_eq!(outcome, MoveOutcome::Moved);

    let dest = cold.join("vm-image.qcow2");
    let dest_metadata = fs::metadata(&dest).unwrap();
    let dest_allocated = dest_metadata.blocks() * 512;

    assert_eq!(dest_metadata.len(), apparent, "apparent size is preserved");
    assert!(
        dest_allocated < apparent / 2,
        "a sparse image must not be materialized: {dest_allocated} allocated of {apparent} apparent"
    );
    assert!(
        dest_allocated <= src_allocated + 1024 * 1024,
        "destination allocated {dest_allocated} bytes against the source's {src_allocated}"
    );

    // The bytes that were there are still there, and the holes still read as zeroes.
    assert_eq!(fs::read(&dest).unwrap()[..marker.len()], marker[..]);
    let mut file = fs::File::open(&dest).unwrap();
    file.seek(SeekFrom::Start(4 * 1024 * 1024)).unwrap();
    let mut middle = vec![0u8; 4096];
    std::io::Read::read_exact(&mut file, &mut middle).unwrap();
    assert!(
        middle.iter().all(|byte| *byte == 0),
        "holes must read as zeroes"
    );
}

#[test]
fn adoption_of_a_same_size_destination_requires_matching_checksums() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();

    // Same length, different bytes: the old length-only check adopted this and deleted
    // the source, losing the real data. Both payloads are exactly 16 bytes.
    fs::write(watch.join("same-length.bin"), b"the real payload").unwrap();
    fs::write(cold.join("same-length.bin"), b"a fake payload!!").unwrap();

    let entries = disk_management::list_files_recursive(&watch).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.relative == Path::new("same-length.bin"))
        .unwrap();
    let err = disk_management::move_file_with_symlink(&cold, entry)
        .expect_err("a same-size stranger must be refused, not adopted");
    assert!(matches!(err, DiskError::DestinationContentMismatch { .. }));

    assert_eq!(
        fs::read(watch.join("same-length.bin")).unwrap(),
        b"the real payload",
        "the source must be untouched"
    );
    assert_eq!(
        fs::read(cold.join("same-length.bin")).unwrap(),
        b"a fake payload!!",
        "the destination must be untouched"
    );
    assert!(!fs::symlink_metadata(watch.join("same-length.bin"))
        .unwrap()
        .is_symlink());
}

#[test]
fn adoption_of_a_genuinely_identical_destination_still_works() {
    let tmp = tempfile::tempdir().unwrap();
    let watch = tmp.path().join("hot");
    let cold = tmp.path().join("cold");
    fs::create_dir_all(&watch).unwrap();
    fs::create_dir_all(&cold).unwrap();

    // A byte-identical destination is the resumed state of an interrupted move and
    // must still be adopted, so the checksum check cannot be a blunt refusal.
    fs::write(watch.join("resumed.bin"), b"identical bytes here").unwrap();
    fs::write(cold.join("resumed.bin"), b"identical bytes here").unwrap();

    let entries = disk_management::list_files_recursive(&watch).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.relative == Path::new("resumed.bin"))
        .unwrap();
    let outcome = disk_management::move_file_with_symlink(&cold, entry).unwrap();
    assert_eq!(outcome, MoveOutcome::LinkedExisting);
    assert!(fs::symlink_metadata(watch.join("resumed.bin"))
        .unwrap()
        .is_symlink());
}

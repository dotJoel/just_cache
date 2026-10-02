//! Filesystem layer: walking a watched tree, moving a file onto another disk and
//! leaving a symlink behind.
//!
//! Every path here is a [`PathBuf`] and is joined with [`Path::join`] — never with
//! string concatenation — and the destination mirrors the file's path *relative to
//! the watched root*, so nested trees keep their shape and same-named files in
//! different directories cannot collide.
//!
//! # The cross-device copy is the path that matters
//!
//! A "slower disk" is a different filesystem, so `rename` fails with `EXDEV` and
//! [`copy_then_remove`] does the work. A plain `io::copy` there would silently drop
//! everything that is not file content, so the copy is deliberate about metadata:
//!
//! * **mode, ownership and timestamps** are copied onto the destination before it is
//!   put in place, so no reader ever sees the process umask's guess at a mode;
//! * **extended attributes are copied best-effort, and this is the documented
//!   default**: an attribute that cannot be set — because the destination filesystem
//!   has no xattr support (`ENOTSUP`), or because setting a `security.*`/`trusted.*`
//!   label needs privileges the process does not have — produces a warning and the
//!   move still completes. SELinux labels are the important case: losing one makes a
//!   file unreadable on an enforcing host, so the warning names the attribute rather
//!   than failing silently. The alternative, refusing the move, would make the tool
//!   unusable on any xattr-less destination, so it is not the default;
//! * **holes stay holes**: a `SEEK_DATA`/`SEEK_HOLE` copy writes only real extents and
//!   `ftruncate`s the tail, with a reflink/clone attempt in front of it, so a sparse
//!   40 GB image does not become 40 GB of allocated blocks on the tier that was meant
//!   to save the space.
//!
//! Ownership can only be set by root; when it cannot be, that is a warning rather than
//! an error, because a file that moved with the wrong owner is recoverable and a file
//! that refused to move is not.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiskError {
    #[error("cannot read directory {path}: {source}")]
    ListError {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot stat {path}: {source}")]
    StatError {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot move {from} -> {to}: {source}")]
    MoveError {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Recovery wanted to restore a name, but something already occupies it. Refusing is
    /// the only safe answer: the occupant may be a link a human created deliberately.
    #[error("refusing to link {link}: something is already there")]
    LinkExists { link: PathBuf },
    #[error("cannot link {link} -> {target}: {source}")]
    SymlinkError {
        link: PathBuf,
        target: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "refusing to touch {path}: destination copy is {dest_size} bytes but source is {src_size}"
    )]
    DestinationConflict {
        path: PathBuf,
        dest_size: u64,
        src_size: u64,
    },
    #[error(
        "refusing to adopt {path}: destination has the source's length ({src_size} bytes) but \
         different content (BLAKE3 mismatch); source is {src_size} bytes and destination is \
         {dest_size} bytes"
    )]
    DestinationContentMismatch {
        path: PathBuf,
        dest_size: u64,
        src_size: u64,
    },
    /// The source is not the file the scan decided to move: it changed size between
    /// being scanned and being copied. Refused before any bytes are written, because a
    /// file that changed under us is a file something is using, and copying it would put
    /// a torn view on the cold tier and then delete the original.
    #[error(
        "source {path} changed since it was scanned: expected {expected} bytes, found {actual}"
    )]
    SourceChanged {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("copy of {path} onto {dest} left {actual} bytes, expected {expected}")]
    ShortCopy {
        path: PathBuf,
        dest: PathBuf,
        actual: u64,
        expected: u64,
    },
}

/// A file found under the watched root.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Full path of the file as it exists in the watched tree.
    pub path: PathBuf,
    /// Path of the file relative to the watched root.
    pub relative: PathBuf,
    pub size: u64,
    /// Bytes the file actually occupies on disk (`st_blocks`). Below `size` for a sparse
    /// file, and the number that matters when asking whether a tier has room: a copy that
    /// preserves holes writes these bytes, not `size` of them.
    pub allocated: u64,
    /// Best available "when was this last used" stamp: atime, falling back to mtime.
    pub last_access: SystemTime,
    /// True when this entry is itself a symlink (i.e. already migrated).
    pub is_symlink: bool,
}

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    /// The file was transferred to the destination disk and symlinked back.
    Moved,
    /// The destination already held a byte-identical copy; only the symlink was (re)made.
    LinkedExisting,
    /// The path was already a symlink — nothing to do.
    AlreadyLinked,
}

/// Marker prefix used for the temporary files created during a cross-device copy.
/// Files matching this are leftovers from an interrupted run and are ignored by the walk.
pub const PARTIAL_PREFIX: &str = ".just_cache-partial-";

/// Every file under `root`, recursively, in no particular order.
///
/// Directories are walked but not followed through symlinks, so a symlink pointing
/// back up the tree cannot make this loop forever. Symlinked files are returned with
/// [`FileEntry::is_symlink`] set so callers can skip already-migrated entries.
pub fn list_files_recursive(root: &Path) -> Result<Vec<FileEntry>, DiskError> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let entries = fs::read_dir(&dir).map_err(|source| DiskError::ListError {
            path: dir.clone(),
            source,
        })?;

        for entry in entries {
            // A single unreadable entry (permissions, race with a delete) should not
            // abort the whole sweep.
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();

            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };

            if metadata.is_symlink() {
                found.push(entry_for(root, path, 0, 0, last_access(&metadata), true));
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                // sockets, fifos, devices: not our business
                continue;
            }
            let name = entry.file_name();
            // Our own bookkeeping: the journal and any in-flight copies. Moving the
            // journal onto a cold tier would be absurd, and worse, would destroy the
            // record of what is in flight exactly when it is needed.
            if name
                .to_string_lossy()
                .starts_with(crate::journal::INTERNAL_PREFIX)
            {
                continue;
            }

            found.push(entry_for(
                root,
                path,
                metadata.len(),
                allocated_bytes(&metadata),
                last_access(&metadata),
                false,
            ));
        }
    }

    Ok(found)
}

fn entry_for(
    root: &Path,
    path: PathBuf,
    size: u64,
    allocated: u64,
    last_access: SystemTime,
    is_symlink: bool,
) -> FileEntry {
    let relative = path
        .strip_prefix(root)
        .map(|rel| rel.to_path_buf())
        .unwrap_or_else(|_| path.file_name().map(PathBuf::from).unwrap_or_default());
    FileEntry {
        path,
        relative,
        size,
        allocated,
        last_access,
        is_symlink,
    }
}

/// Bytes a file occupies on disk. A sparse file's holes are not allocated, so this is
/// below `len()`, and it is the amount a hole-preserving copy has to find room for.
#[cfg(unix)]
pub fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
pub fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

/// Where the "when was this last used" stamp came from.
///
/// The mover's usage signal is atime, with mtime as the fallback when a filesystem does
/// not report an access time (`docs/design.md` §9 names the `noatime` weakness). `explain`
/// has to say *which* of the two it is looking at, because "idle for 90 days" reads very
/// differently when the stamp is the last write rather than the last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessSource {
    /// The filesystem reported a last-access time.
    Atime,
    /// No access time was available, so the last-modification time stands in for it.
    MtimeFallback,
}

impl AccessSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AccessSource::Atime => "atime",
            AccessSource::MtimeFallback => "mtime-fallback",
        }
    }
}

/// Best available last-use stamp for `metadata`, and where it came from.
pub fn last_use(metadata: &fs::Metadata) -> (SystemTime, AccessSource) {
    choose_access(metadata.accessed(), metadata.modified())
}

/// The choice behind [`last_use`], split out so the fallback branch can be exercised
/// without a filesystem that refuses to report atime — which no mainstream Linux mount
/// does, so the branch is otherwise unreachable in a test.
fn choose_access(
    accessed: io::Result<SystemTime>,
    modified: io::Result<SystemTime>,
) -> (SystemTime, AccessSource) {
    match accessed {
        Ok(accessed) => (accessed, AccessSource::Atime),
        Err(_) => (
            modified.unwrap_or(SystemTime::UNIX_EPOCH),
            AccessSource::MtimeFallback,
        ),
    }
}

fn last_access(metadata: &fs::Metadata) -> SystemTime {
    last_use(metadata).0
}

/// The destination to use for a file, or `None` when the tier has no room left.
///
/// Shared by the sweep and by `explain`, so "would this move?" and "does this move?" agree
/// about what "room" means. `needed` is the file's *allocated* size, not its apparent
/// length: the copy preserves holes, so a 1 GiB sparse file needs kilobytes of room, and
/// measuring it by `len()` would refuse a move the tier can easily afford. The floor keeps
/// a sweep from filling the disk it is moving onto.
///
/// `None` back from `available_space` (free space unknown on this filesystem) means "try
/// the tier rather than stall".
pub fn destination_with_room(dest: &Path, needed: u64, min_free: u64) -> Option<PathBuf> {
    match available_space(dest) {
        Some(free) if free >= min_free.saturating_add(needed) => Some(dest.to_path_buf()),
        Some(_) => None,
        None => Some(dest.to_path_buf()),
    }
}

/// Move `entry` from the watched tree onto `dest_root`, leaving a symlink in the
/// original location that points at the new home.
///
/// This is idempotent: an entry that is already a symlink is left alone, and when the
/// destination already holds a *byte-identical* copy the source file is dropped in
/// favour of the symlink instead of being moved again. A destination of a different
/// size, or of the same size but different content, is refused rather than clobbered:
/// the resumed state of an interrupted move is only a destination whose checksum
/// matches, which is stronger than the length-only check this used to trust.
pub fn move_file_with_symlink(
    dest_root: &Path,
    entry: &FileEntry,
) -> Result<MoveOutcome, DiskError> {
    let src = &entry.path;

    let src_metadata = fs::symlink_metadata(src).map_err(|source| DiskError::StatError {
        path: src.clone(),
        source,
    })?;
    if src_metadata.is_symlink() {
        return Ok(MoveOutcome::AlreadyLinked);
    }

    // The file must still be the one the policy chose. A size that moved since the scan
    // means something is writing to it, and a file in flux must not be relocated on the
    // strength of bytes we only *thought* we measured. Checked here, before either
    // transfer path, because a same-filesystem `rename` would otherwise move the changed
    // file without a murmur; `copy_then_remove` re-checks, since a cross-device copy takes
    // long enough for the source to change underneath it.
    if src_metadata.len() != entry.size {
        return Err(DiskError::SourceChanged {
            path: src.clone(),
            expected: entry.size,
            actual: src_metadata.len(),
        });
    }

    let dest = dest_root.join(&entry.relative);
    // The symlink target is computed before the move, while both paths still refer to
    // their final locations.
    let link_target = symlink_target(src, &dest);

    let already_there = match fs::symlink_metadata(&dest) {
        Ok(dest_metadata) => {
            if dest_metadata.is_dir() {
                return Err(DiskError::DestinationConflict {
                    path: dest,
                    dest_size: 0,
                    src_size: entry.size,
                });
            }
            let dest_size = dest_metadata.len();
            if dest_size != entry.size {
                return Err(DiskError::DestinationConflict {
                    path: dest,
                    dest_size,
                    src_size: entry.size,
                });
            }
            // Same length is not the same file. An interrupted move leaves the
            // destination identical, so a checksum is the honest way to tell the
            // resumed state from a same-size stranger that must not be adopted —
            // adopting it would delete the source and lose the data.
            let identical = same_contents(src, &dest).map_err(|source| DiskError::StatError {
                path: src.clone(),
                source,
            })?;
            if !identical {
                return Err(DiskError::DestinationContentMismatch {
                    path: dest,
                    dest_size,
                    src_size: entry.size,
                });
            }
            true
        }
        Err(_) => false,
    };

    if already_there {
        fs::remove_file(src).map_err(|source| DiskError::MoveError {
            from: src.clone(),
            to: dest.clone(),
            source,
        })?;
    } else {
        transfer(src, &dest, entry.size)?;
    }

    create_symlink(&link_target, src).map_err(|source| DiskError::SymlinkError {
        link: src.clone(),
        target: link_target.clone(),
        source,
    })?;

    Ok(if already_there {
        MoveOutcome::LinkedExisting
    } else {
        MoveOutcome::Moved
    })
}

/// Whether two files hold the same bytes, compared with BLAKE3 (the content identity
/// the design's catalog is built on, §3), not by length.
fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(file_digest(a)? == file_digest(b)?)
}

fn file_digest(path: &Path) -> io::Result<blake3::Hash> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize())
}

/// Move `src` to `dest`, falling back to copy-then-delete when the two paths live on
/// different filesystems (`rename` cannot cross a mount point, and a "slower disk" is
/// almost always a different mount).
fn transfer(src: &Path, dest: &Path, expected: u64) -> Result<(), DiskError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|source| DiskError::MoveError {
            from: src.to_path_buf(),
            to: dest.to_path_buf(),
            source,
        })?;
    }

    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device(&err) => copy_then_remove(src, dest, expected),
        Err(source) => Err(DiskError::MoveError {
            from: src.to_path_buf(),
            to: dest.to_path_buf(),
            source,
        }),
    }
}

fn is_cross_device(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::CrossesDevices {
        return true;
    }
    #[cfg(unix)]
    {
        // EXDEV
        err.raw_os_error() == Some(18)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// The cross-device fallback: copy the bytes *and* the metadata into a temporary file
/// next to the destination, then make it visible with an atomic rename, then drop the
/// source.
///
/// The temporary lives in the destination directory so the final rename stays on one
/// filesystem, which is what makes it atomic — a reader never sees a half-copied file.
fn copy_then_remove(src: &Path, dest: &Path, expected: u64) -> Result<(), DiskError> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let partial = partial_sibling(dir);

    // Anything that fails here is reported as a failed move against this pair, except
    // `SourceChanged`, which has already said precisely what went wrong.
    let failed_move = |source: io::Error| DiskError::MoveError {
        from: src.to_path_buf(),
        to: dest.to_path_buf(),
        source,
    };

    let copied = (|| -> Result<u64, DiskError> {
        let src_file = File::open(src).map_err(failed_move)?;
        let src_metadata = src_file.metadata().map_err(failed_move)?;
        // Compare against the size the scan promised, not just against the live file:
        // a source truncated between the scan and now would otherwise be copied happily
        // and its short version accepted as a successful move. Checked before the copy
        // starts so a file in flux costs nothing but a stat.
        if src_metadata.len() != expected {
            return Err(DiskError::SourceChanged {
                path: src.to_path_buf(),
                expected,
                actual: src_metadata.len(),
            });
        }
        let mut dest_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(failed_move)?;

        copy_contents(&src_file, &mut dest_file, src_metadata.len()).map_err(failed_move)?;
        // Metadata is applied while the file is still private, so the mode/owner never
        // briefly differ from the source for anything that can see the destination.
        preserve_metadata(src, &partial, &src_metadata).map_err(failed_move)?;
        dest_file.sync_all().map_err(failed_move)?;
        Ok(src_metadata.len())
    })();

    let live_len = match copied {
        Ok(live_len) => live_len,
        Err(err) => {
            // No partial was renamed into place, so nothing of ours survives.
            let _ = fs::remove_file(&partial);
            return Err(err);
        }
    };

    // The source could have been truncated *while* we copied it; a short destination
    // must never replace the original. Two comparisons, because they catch different
    // failures: the copy must match what the scan promised, and it must match what the
    // live file had when the copy started.
    match fs::symlink_metadata(&partial) {
        Ok(metadata) if metadata.len() == expected && metadata.len() == live_len => {}
        Ok(metadata) => {
            let actual = metadata.len();
            let _ = fs::remove_file(&partial);
            return Err(DiskError::ShortCopy {
                path: src.to_path_buf(),
                dest: dest.to_path_buf(),
                actual,
                expected,
            });
        }
        Err(source) => {
            let _ = fs::remove_file(&partial);
            return Err(DiskError::MoveError {
                from: src.to_path_buf(),
                to: dest.to_path_buf(),
                source,
            });
        }
    }

    // Same directory, so this rename stays on one filesystem and is atomic.
    fs::rename(&partial, dest).map_err(|source| DiskError::MoveError {
        from: partial.clone(),
        to: dest.to_path_buf(),
        source,
    })?;
    fs::remove_file(src).map_err(|source| DiskError::MoveError {
        from: src.to_path_buf(),
        to: dest.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Copy the logical contents of `src` into `dest`, with holes kept as holes.
///
/// Three strategies, best first:
///
/// 1. a reflink/clone (`FICLONE`), which shares extents and so is both instant and
///    perfectly faithful — holes, inline data and compression all come along;
/// 2. an explicit `SEEK_DATA`/`SEEK_HOLE` walk that copies only real extents and
///    `ftruncate`s the tail, which is what actually keeps a sparse image sparse;
/// 3. a plain sequential copy, only when the filesystem does not implement
///    `SEEK_DATA` (some network and exotic filesystems return `EINVAL`).
pub(crate) fn copy_contents(src: &File, dest: &mut File, size: u64) -> io::Result<()> {
    #[cfg(all(
        target_os = "linux",
        not(any(target_arch = "sparc", target_arch = "sparc64"))
    ))]
    {
        // The kernel only allows a clone within one filesystem; across devices this
        // fails with EXDEV and we fall through, which is the expected case here.
        if rustix::fs::ioctl_ficlone(&*dest, src).is_ok() {
            return Ok(());
        }
    }

    if copy_extents(src, dest, size)? {
        return Ok(());
    }
    copy_all(src, dest, size)
}

/// Walk the source's real extents with `SEEK_DATA`/`SEEK_HOLE`, writing only those and
/// leaving the gaps untouched, so the destination allocates only what the source did.
///
/// Returns `Ok(false)` when the filesystem does not support the seeks, having left the
/// destination empty so the caller can fall back.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris",
    target_os = "illumos"
))]
fn copy_extents(src: &File, dest: &mut File, size: u64) -> io::Result<bool> {
    use rustix::fs::SeekFrom as RustixSeekFrom;

    let mut position = 0u64;
    while position < size {
        let data_start = match rustix::fs::seek(src, RustixSeekFrom::Data(position)) {
            Ok(offset) => offset,
            // There is no data at or after `position`: the rest of the file is a hole,
            // which `set_len` at the end turns back into an implicitly-zero tail.
            Err(err) if err == rustix::io::Errno::NXIO => break,
            // Not every filesystem implements SEEK_DATA/SEEK_HOLE.
            Err(err) if err == rustix::io::Errno::INVAL || err == rustix::io::Errno::NOTSUP => {
                return Ok(false);
            }
            Err(err) => return Err(err.into()),
        };
        let hole_start = {
            let offset = rustix::fs::seek(src, RustixSeekFrom::Hole(data_start))?;
            offset.min(size)
        };
        if hole_start <= data_start {
            // A malformed answer; treat it as "no extent support" and fall back rather
            // than loop forever.
            return Ok(false);
        }

        // Position both files at the extent and copy it. The destination's offset never
        // advances over a hole, so nothing is written there and the gap stays sparse.
        rustix::fs::seek(src, RustixSeekFrom::Start(data_start))?;
        dest.seek(SeekFrom::Start(data_start))?;
        let length = hole_start - data_start;
        let mut extent = io::Read::take(src, length);
        let copied = io::copy(&mut extent, &mut *dest)?;
        if copied != length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "source shrank while its extents were being copied",
            ));
        }
        position = hole_start;
    }

    // The final extend is what gives the file its apparent size even though the tail
    // was never written; on a sparse-capable filesystem this allocates nothing.
    dest.set_len(size)?;
    Ok(true)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn copy_extents(_src: &File, _dest: &mut File, _size: u64) -> io::Result<bool> {
    Ok(false)
}

/// The honest fallback for filesystems that cannot walk holes: read and write every
/// byte. The result is correct but fully materialized, which is exactly the cost this
/// module exists to avoid where it can.
fn copy_all(src: &File, dest: &mut File, size: u64) -> io::Result<()> {
    let mut source = src;
    source.seek(SeekFrom::Start(0))?;
    dest.seek(SeekFrom::Start(0))?;
    dest.set_len(0)?;
    let copied = io::copy(&mut source, &mut *dest)?;
    if copied != size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "source shrank while it was being copied",
        ));
    }
    Ok(())
}

/// Copy mode, ownership, extended attributes and timestamps from a source file onto a
/// destination that already holds its bytes.
#[cfg(unix)]
pub(crate) fn preserve_metadata(
    src: &Path,
    dest: &Path,
    src_metadata: &fs::Metadata,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // Ownership first: chown clears setuid/setgid bits, so the mode has to be applied
    // after it. Changing owner needs root, so a failure is a warning and not a reason
    // to abandon the move — a file with the wrong owner is recoverable, a move that
    // refuses to happen leaves the tier unwritten.
    let want = (src_metadata.uid(), src_metadata.gid());
    if let Ok(dest_metadata) = fs::metadata(dest) {
        if (dest_metadata.uid(), dest_metadata.gid()) != want {
            match rustix::fs::chown(
                dest,
                Some(rustix::fs::Uid::from_raw(want.0)),
                Some(rustix::fs::Gid::from_raw(want.1)),
            ) {
                Ok(()) => {}
                Err(err) => eprintln!(
                    "warning: {}: cannot preserve ownership {}:{} on {}: {err}",
                    src.display(),
                    want.0,
                    want.1,
                    dest.display()
                ),
            }
        }
    }

    fs::set_permissions(dest, fs::Permissions::from_mode(src_metadata.mode()))?;
    copy_xattrs(src, dest);

    // Timestamps last: writing and chmoding the file both move mtime, so this is the
    // only order in which the recorded times survive.
    filetime::set_file_times(
        dest,
        filetime::FileTime::from_last_access_time(src_metadata),
        filetime::FileTime::from_last_modification_time(src_metadata),
    )?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn preserve_metadata(
    _src: &Path,
    _dest: &Path,
    _src_metadata: &fs::Metadata,
) -> io::Result<()> {
    // The Unix-only metadata model (mode, uid/gid, xattrs) has no equivalent here; on
    // Windows the symlink fallback is already best-effort.
    Ok(())
}

/// Copy every extended attribute from `src` to `dest`, best-effort.
///
/// Attributes that cannot be read or set are reported and skipped rather than failing
/// the move; see the module docs for why that is the default. POSIX ACLs are skipped
/// explicitly: they live in `system.posix_acl_*` xattrs, copying them is out of scope,
/// and the mode bits set just above are the ACL's access mask in the common case.
#[cfg(unix)]
fn copy_xattrs(src: &Path, dest: &Path) {
    let names = match list_xattr_names(src) {
        Ok(names) => names,
        Err(err) => {
            eprintln!(
                "warning: {}: cannot list extended attributes: {err}",
                src.display()
            );
            return;
        }
    };

    for name in names {
        if name == "system.posix_acl_access" || name == "system.posix_acl_default" {
            continue;
        }
        match get_xattr(src, &name).and_then(|value| set_xattr(dest, &name, &value)) {
            Ok(()) => {}
            Err(err) => eprintln!(
                "warning: cannot copy extended attribute {name} from {} to {}: {err}",
                src.display(),
                dest.display()
            ),
        }
    }
}

#[cfg(unix)]
fn list_xattr_names(path: &Path) -> io::Result<Vec<String>> {
    let mut capacity = 4096usize;
    loop {
        let mut buffer = vec![0u8; capacity];
        match rustix::fs::listxattr(path, &mut buffer) {
            Ok(len) => {
                buffer.truncate(len);
                return Ok(buffer
                    .split(|byte| *byte == 0)
                    .filter(|name| !name.is_empty())
                    .map(|name| String::from_utf8_lossy(name).into_owned())
                    .collect());
            }
            // The list outgrew the buffer; ERANGE is the kernel telling us the real
            // size is larger, and the size is not otherwise queryable.
            Err(err) if err == rustix::io::Errno::RANGE && capacity < (1 << 20) => {
                capacity *= 4;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(unix)]
fn get_xattr(path: &Path, name: &str) -> io::Result<Vec<u8>> {
    // A zero-length buffer makes the kernel return the value's length without
    // copying it, which is the only way to size the read precisely.
    let mut empty = [0u8; 0];
    let size = rustix::fs::getxattr(path, name, &mut empty)?;
    let mut value = vec![0u8; size];
    let len = rustix::fs::getxattr(path, name, &mut value)?;
    value.truncate(len);
    Ok(value)
}

#[cfg(unix)]
fn set_xattr(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    rustix::fs::setxattr(path, name, value, rustix::fs::XattrFlags::empty())?;
    Ok(())
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// A fresh, private temporary name in `dir` for an in-flight copy.
///
/// Shared by the mover's cross-device path and by `restore`, so both leave the same
/// `.just_cache-partial-*` marker that the walk skips (invariant 8): a crash mid-restore
/// can never leave a name the next sweep would try to move onto a cold tier.
pub(crate) fn partial_sibling(dir: &Path) -> PathBuf {
    dir.join(format!(
        "{PARTIAL_PREFIX}{}-{}.tmp",
        std::process::id(),
        nanos()
    ))
}

/// Free bytes available on the filesystem holding `path`.
///
/// `None` means "could not tell" — callers should fall back to trying the destination.
pub fn available_space(path: &Path) -> Option<u64> {
    fs4::available_space(path).ok()
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(target: &Path, link: &Path) -> io::Result<()> {
    let _ = (target, link);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

/// The path to store *inside* the symlink: relative when the source and destination
/// share an ancestor, absolute otherwise. Relative links keep the pair working when
/// the whole tree is moved or the destination is mounted elsewhere.
/// Make `link` a symlink to `target`, creating no directories and replacing nothing.
///
/// Shared by the mover and by journal recovery, so a link restored after a crash is
/// byte-for-byte the kind of link a normal move would have left: relative where possible,
/// absolute when the two paths share no ancestor.
pub fn link_into_place(target: &Path, link: &Path) -> Result<(), DiskError> {
    if fs::symlink_metadata(link).is_ok() {
        return Err(DiskError::LinkExists {
            link: link.to_path_buf(),
        });
    }
    let relative = symlink_target(link, target);
    create_symlink(&relative, link).map_err(|source| DiskError::SymlinkError {
        link: link.to_path_buf(),
        target: relative.clone(),
        source,
    })
}

pub fn symlink_target(src: &Path, dest: &Path) -> PathBuf {
    let (Some(src_dir), Some(dest_dir)) = (src.parent(), dest.parent()) else {
        return dest.to_path_buf();
    };
    let src_parts: Vec<Component> = src_dir.components().collect();
    let dest_parts: Vec<Component> = dest_dir.components().collect();
    let common = src_parts
        .iter()
        .zip(dest_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    if common == 0 {
        return dest.to_path_buf();
    }

    let mut relative = PathBuf::new();
    for _ in common..src_parts.len() {
        relative.push("..");
    }
    for part in &dest_parts[common..] {
        relative.push(part.as_os_str());
    }
    match dest.file_name() {
        Some(name) => relative.push(name),
        None => return dest.to_path_buf(),
    }

    if relative.as_os_str().is_empty() {
        dest.to_path_buf()
    } else {
        relative
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn walks_nested_directories_and_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("watched");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("top.txt"), b"top").unwrap();
        fs::write(root.join("a/nested.txt"), b"nested").unwrap();
        fs::write(root.join("a/b/deep.txt"), b"deep").unwrap();
        create_symlink(Path::new("/nowhere"), &root.join("link.txt")).unwrap();

        let mut names: Vec<String> = list_files_recursive(&root)
            .unwrap()
            .into_iter()
            .map(|e| e.relative.to_string_lossy().into_owned())
            .collect();
        names.sort();

        assert_eq!(
            names,
            vec!["a/b/deep.txt", "a/nested.txt", "link.txt", "top.txt"]
        );
        let linked = list_files_recursive(&root)
            .unwrap()
            .into_iter()
            .find(|e| e.relative == Path::new("link.txt"))
            .unwrap();
        assert!(
            linked.is_symlink,
            "symlinks must be flagged as already migrated"
        );
    }

    #[test]
    fn symlink_target_is_relative_between_siblings() {
        let target = symlink_target(
            Path::new("/pool/cache/movies/a.mkv"),
            Path::new("/pool/cold/movies/a.mkv"),
        );
        assert_eq!(target, Path::new("../../cold/movies/a.mkv"));
    }

    #[test]
    fn symlink_target_falls_back_to_absolute_without_common_ancestor() {
        let target = symlink_target(Path::new("relative/a.mkv"), Path::new("/pool/cold/a.mkv"));
        assert_eq!(target, Path::new("/pool/cold/a.mkv"));
    }

    /// The copy path, exercised directly so it does not depend on a second filesystem
    /// existing. This is the same code `EXDEV` reaches in production.
    #[test]
    fn cross_device_copy_preserves_mode_ownership_xattrs_and_times() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.bin");
        let dest = tmp.path().join("dest.bin");
        fs::write(&src, b"payload with metadata").unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o640)).unwrap();

        let src_metadata = fs::metadata(&src).unwrap();
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        let times = fs::FileTimes::new().set_accessed(mtime).set_modified(mtime);
        File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_times(times)
            .unwrap();

        // Extended attributes are not guaranteed to be representable; the assertion
        // below is skipped rather than failed when this filesystem cannot hold one.
        let xattr_supported = rustix::fs::setxattr(
            &src,
            "user.just_cache_test",
            b"kept",
            rustix::fs::XattrFlags::empty(),
        )
        .is_ok();

        let expected = fs::metadata(&src).unwrap().len();
        copy_then_remove(&src, &dest, expected).expect("copy path should succeed");
        assert!(
            !src.exists(),
            "the source is deleted after a successful copy"
        );

        let dest_metadata = fs::metadata(&dest).unwrap();
        assert_eq!(
            dest_metadata.mode() & 0o7777,
            0o640,
            "mode must survive the copy"
        );
        assert_eq!(
            (dest_metadata.uid(), dest_metadata.gid()),
            (src_metadata.uid(), src_metadata.gid()),
            "ownership is preserved when it can be set"
        );
        assert_eq!(
            dest_metadata.modified().unwrap(),
            mtime,
            "mtime must be copied"
        );
        assert_eq!(fs::read(&dest).unwrap(), b"payload with metadata");

        if xattr_supported {
            let mut value = [0u8; 16];
            let len = rustix::fs::getxattr(&dest, "user.just_cache_test", &mut value).unwrap();
            assert_eq!(&value[..len], b"kept", "xattrs must be copied");
        }
    }

    #[test]
    fn extent_copy_keeps_holes_and_reports_allocated_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let src_path = tmp.path().join("sparse.img");
        let dest_path = tmp.path().join("sparse-copy.img");
        let apparent: u64 = 256 * 1024 * 1024;
        let marker = vec![0x5au8; 16 * 1024];

        {
            let mut file = File::create(&src_path).unwrap();
            file.write_all(&marker).unwrap();
            file.seek(SeekFrom::Start(32 * 1024 * 1024)).unwrap();
            file.write_all(&marker).unwrap();
            file.set_len(apparent).unwrap();
        }

        let src_metadata = fs::metadata(&src_path).unwrap();
        if src_metadata.blocks() == 0 || src_metadata.blocks() * 512 >= apparent / 2 {
            eprintln!("skipping: filesystem does not represent holes sparsely");
            return;
        }

        let src_file = File::open(&src_path).unwrap();
        let mut dest_file = File::create(&dest_path).unwrap();
        // Call the hole walker directly rather than the whole copy, so a reflink of an
        // unrelated test machine cannot mask whether SEEK_DATA did the work.
        let used_extents = copy_extents(&src_file, &mut dest_file, apparent).unwrap();
        dest_file.sync_all().unwrap();
        assert!(used_extents, "this filesystem should support SEEK_DATA");

        let dest_metadata = fs::metadata(&dest_path).unwrap();
        assert_eq!(dest_metadata.len(), apparent, "apparent size is preserved");
        let src_allocated = src_metadata.blocks() * 512;
        let dest_allocated = dest_metadata.blocks() * 512;
        assert!(
            dest_allocated < apparent / 2,
            "holes must not be materialized: {dest_allocated} allocated of {apparent} apparent"
        );
        assert!(
            dest_allocated <= src_allocated + 1024 * 1024,
            "the copy allocated {dest_allocated} bytes against the source's {src_allocated}"
        );
        assert_eq!(
            fs::read(&dest_path).unwrap()[..marker.len()],
            marker[..],
            "the first extent must survive"
        );
    }

    #[test]
    fn same_length_different_content_is_not_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let watch = tmp.path().join("hot");
        let cold = tmp.path().join("cold");
        fs::create_dir_all(&watch).unwrap();
        fs::create_dir_all(&cold).unwrap();
        fs::write(watch.join("clash.bin"), b"source bytes").unwrap();
        fs::write(cold.join("clash.bin"), b"other! bytes").unwrap();

        let entry = list_files_recursive(&watch).unwrap().pop().unwrap();
        let err = move_file_with_symlink(&cold, &entry)
            .expect_err("a same-size stranger must not be adopted");
        assert!(matches!(err, DiskError::DestinationContentMismatch { .. }));
        assert_eq!(fs::read(watch.join("clash.bin")).unwrap(), b"source bytes");
        assert_eq!(fs::read(cold.join("clash.bin")).unwrap(), b"other! bytes");
    }

    /// The atime→mtime fallback is the only branch a normal mount cannot reach: Linux
    /// reports an access time even under `noatime` (it is simply not updated). So the
    /// decision is exercised directly rather than pretended at with a mount option.
    #[test]
    fn a_missing_access_time_falls_back_to_mtime() {
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let (stamp, source) = choose_access(
            Err(io::Error::new(io::ErrorKind::Unsupported, "no atime")),
            Ok(mtime),
        );
        assert_eq!(stamp, mtime);
        assert_eq!(source, AccessSource::MtimeFallback);

        let atime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        let (stamp, source) = choose_access(Ok(atime), Ok(mtime));
        assert_eq!(stamp, atime, "a reported atime always wins");
        assert_eq!(source, AccessSource::Atime);
    }
}

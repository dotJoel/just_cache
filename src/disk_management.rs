//! Filesystem layer: walking a watched tree, moving a file onto another disk and
//! leaving a symlink behind.
//!
//! Every path here is a [`PathBuf`] and is joined with [`Path::join`] — never with
//! string concatenation — and the destination mirrors the file's path *relative to
//! the watched root*, so nested trees keep their shape and same-named files in
//! different directories cannot collide.

use std::fs::{self, File};
use std::io::{self, Write};
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
    /// The destination already held an identical copy; only the symlink was (re)made.
    LinkedExisting,
    /// The path was already a symlink — nothing to do.
    AlreadyLinked,
}

/// Marker prefix used for the temporary files created during a cross-device copy.
/// Files matching this are leftovers from an interrupted run and are ignored by the walk.
const PARTIAL_PREFIX: &str = ".just_cache-partial-";

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
                found.push(entry_for(root, path, 0, last_access(&metadata), true));
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
            if name.to_string_lossy().starts_with(PARTIAL_PREFIX) {
                continue;
            }

            found.push(entry_for(
                root,
                path,
                metadata.len(),
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
        last_access,
        is_symlink,
    }
}

fn last_access(metadata: &fs::Metadata) -> SystemTime {
    metadata
        .accessed()
        .or_else(|_| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Move `entry` from the watched tree onto `dest_root`, leaving a symlink in the
/// original location that points at the new home.
///
/// This is idempotent: an entry that is already a symlink is left alone, and when the
/// destination already holds a byte-identical copy the source file is dropped in
/// favour of the symlink instead of being moved again. It refuses to overwrite a
/// destination of a different size.
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

/// Move `src` to `dest`, falling back to copy-then-delete when the two paths live on
/// different filesystems (`rename` cannot cross a mount point, and a "slower disk" is
/// almost always a different mount).
fn transfer(src: &Path, dest: &Path, expected_size: u64) -> Result<(), DiskError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|source| DiskError::MoveError {
            from: src.to_path_buf(),
            to: dest.to_path_buf(),
            source,
        })?;
    }

    match fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device(&err) => copy_then_remove(src, dest, expected_size),
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

fn copy_then_remove(src: &Path, dest: &Path, expected_size: u64) -> Result<(), DiskError> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let partial = dir.join(format!(
        "{PARTIAL_PREFIX}{}-{}.tmp",
        std::process::id(),
        nanos()
    ));

    let written = (|| -> io::Result<u64> {
        let mut reader = File::open(src)?;
        let mut writer = File::create(&partial)?;
        let copied = io::copy(&mut reader, &mut writer)?;
        writer.flush()?;
        writer.sync_all()?;
        Ok(copied)
    })();

    match written {
        Ok(copied) if copied == expected_size => {}
        Ok(actual) => {
            let _ = fs::remove_file(&partial);
            return Err(DiskError::ShortCopy {
                path: src.to_path_buf(),
                dest: dest.to_path_buf(),
                actual,
                expected: expected_size,
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

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
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
}

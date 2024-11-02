use std::{fs, io};
use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
fn symlink<P: AsRef<std::path::Path>, Q: AsRef<std::path::Path>>(
    _original: P,
    _link: Q,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Other,
        "Symlink creation is not supported on this platform",
    ))
}

#[derive(Error, Debug)]
pub enum DiskError {
    #[error("Failed to list files: {0}")]
    ListError(io::Error),
    #[error("Failed to move file: {0}")]
    MoveError(io::Error),
    #[error("Failed to create symlink: {0}")]
    SymlinkError(io::Error),
}

pub fn list_files(disk: &str) -> Result<Vec<String>, DiskError> {
    let paths = fs::read_dir(disk).map_err(DiskError::ListError)?;
    let files = paths
        .filter_map(|entry| entry.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    Ok(files)
}

pub fn move_file_with_symlink(src_disk: &str, dest_disk: &str, file: &str) -> Result<(), DiskError> {
    let src_path = format!("{}/{}", src_disk, file);
    let dest_path = format!("{}/{}", dest_disk, file);

    // Move the file
    fs::rename(&src_path, &dest_path).map_err(DiskError::MoveError)?;

    // Create a symlink in the original location
    symlink(&dest_path, &src_path).map_err(DiskError::SymlinkError)?;

    Ok(())
}

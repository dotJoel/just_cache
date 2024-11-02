use super::disk_management::{self, DiskError};
use std::collections::HashMap;

pub struct UsageTracker {
    usage: HashMap<String, usize>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self {
            usage: HashMap::new(),
        }
    }

    pub fn record_access(&mut self, file: &str) {
        let count = self.usage.entry(file.to_string()).or_insert(0);
        *count += 1;
    }

    pub fn access_count(&self, file: &str) -> usize {
        *self.usage.get(file).unwrap_or(&0)
    }
}

pub fn move_least_used_files(
    tracker: &mut UsageTracker,
    src_folder: &str,
    dest_disk: &str,
    threshold: usize,
) -> Result<(), DiskError> {
    let files = disk_management::list_files(src_folder)?;
    for file in files {
        if tracker.access_count(&file) < threshold {
            match disk_management::move_file_with_symlink(src_folder, dest_disk, &file) {
                Ok(_) => log_file_movement(&file, src_folder, dest_disk),
                Err(e) => eprintln!("Error moving file {}: {}", file, e),
            }
        }
    }
    Ok(())
}

pub fn log_file_movement(file: &str, src: &str, dest: &str) {
    println!("File {} moved from {} to {}", file, src, dest);
}

mod disk_management;
mod file_movement;

fn main() {
    let watched_folder = "/path/to/watched/folder"; // Folder to monitor
    let slower_disks = vec!["/path/to/slow/disk1", "/path/to/slow/disk2"]; // Slower disks
    let mut usage_tracker = file_movement::UsageTracker::new();
    loop {
        // Record file access in the watched folder
        let files = match disk_management::list_files(watched_folder) {
            Ok(files) => files,
            Err(e) => {
                eprintln!("Error listing files in {}: {}", watched_folder, e);
                continue;
            }
        };
        for file in &files {
            usage_tracker.record_access(file);
        }
        // Move files to slower disks
        for dest_disk in &slower_disks {
            if let Err(e) = file_movement::move_least_used_files(&mut usage_tracker, watched_folder, dest_disk, 5) {
                eprintln!("Failed to move files from {} to {}: {}", watched_folder, dest_disk, e);
            }
        }
        // Sleep for a specified period before the next iteration
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

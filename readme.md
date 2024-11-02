# File Management System

This Rust program monitors a specified folder, tracks file usage, and moves the least-used files to slower disks. It also creates symbolic links in the original location to maintain file accessibility.

## Modules

### `disk_management`

This module handles file operations such as reading, writing, deleting, listing files, and moving files with symbolic links.

- [`disk_management::write_file`](src/disk_management.rs)
- [`disk_management::read_file`](src/disk_management.rs)
- [`disk_management::delete_file`](src/disk_management.rs)
- [`disk_management::list_files`](src/disk_management.rs)
- [`disk_management::move_file_with_symlink`](src/disk_management.rs)

### `file_movement`

This module tracks file usage and moves the least-used files to slower disks.

- [`file_movement::UsageTracker`](src/file_movement.rs)
- [`file_movement::move_least_used_files`](src/file_movement.rs)
- [`file_movement::log_file_movement`](src/file_movement.rs)

## Usage

1. **Clone the repository:**

    ```sh
    git clone <repository-url>
    cd <repository-directory>
    ```

2. **Build the project:**

    ```sh
    cargo build
    ```

3. **Run the project:**

    ```sh
    cargo run
    ```

## Configuration

- **Watched Folder:** The folder to monitor for file access.
- **Slower Disks:** A list of slower disks where the least-used files will be moved.

These configurations can be modified in the [`main.rs`](src/main.rs) file.

## Example

In the `main.rs` file, you can specify the folder to monitor and the slower disks:

```rust
fn main() {
    let watched_folder = "/path/to/watched/folder"; // Folder to monitor
    let slower_disks = vec!["/path/to/slow/disk1", "/path/to/slow/disk2"]; // Slower disks
    // ...
}
```

## License
This project is licensed under the MIT License. See the LICENSE file for details.

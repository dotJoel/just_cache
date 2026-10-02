//! Library surface of `just_cache`, so the walk / policy / move layers can be
//! exercised from integration tests and reused as a crate.

pub mod disk_management;
pub mod file_movement;
pub mod scope;

pub use disk_management::{
    available_space, list_files_recursive, move_file_with_symlink, DiskError, FileEntry,
    MoveOutcome,
};
pub use file_movement::{
    log_file_movement, migrate_least_used, select_candidates, FileOutcome, MigrationRecord,
    MigrationReport, Policy, SkipReason, UsageTracker,
};
pub use scope::{human_bytes, parse_size, Rejected, Scope, ScopeError};

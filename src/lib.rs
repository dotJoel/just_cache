//! Library surface of `just_cache`, so the walk / policy / move / audit layers can be
//! exercised from integration tests and reused as a crate.

pub mod audit;
pub mod digest;

pub mod disk_management;
pub mod file_movement;
pub mod opened;
pub mod scope;

pub use audit::{
    audit, classify, repair, AuditError, AuditReport, Finding, RepairAction, RepairOutcome,
    SourceState, Verdict, VerdictKind, DEFAULT_EXAMPLES,
};
pub use digest::{bytes_digest, file_digest};
pub use disk_management::{
    available_space, list_files_recursive, move_file_with_symlink, DiskError, FileEntry,
    MoveOutcome,
};
pub use file_movement::{
    log_file_movement, migrate_least_used, select_candidates, FileOutcome, MigrationRecord,
    MigrationReport, Policy, SkipReason, UsageTracker,
};
pub use opened::{Coverage, FileId, Guards, InUse, OpenFiles};
pub use scope::{human_bytes, parse_size, Rejected, Scope, ScopeError};

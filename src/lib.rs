//! Library surface of `just_cache`, so the walk / policy / move / audit layers can be
//! exercised from integration tests and reused as a crate.

pub mod audit;
pub mod catalog;
pub mod digest;

pub mod disk_management;
pub mod explain;
pub mod file_movement;
pub mod journal;
pub mod opened;
pub mod replication;
pub mod restore;
pub mod scope;

pub use audit::{
    audit, audit_with_copies, classify, repair, AuditError, AuditReport, Finding, RepairAction,
    RepairOutcome, SourceState, Verdict, VerdictKind, DEFAULT_EXAMPLES,
};
pub use catalog::{
    Catalog, CatalogError, Difference, DifferenceKind, LocationRecord, SyncReport, CATALOG_NAME,
};
pub use digest::{bytes_digest, file_digest};
pub use disk_management::{
    allocated_bytes, available_space, destination_with_room, last_use, list_files_recursive,
    move_file_with_symlink, AccessSource, DiskError, FileEntry, MoveOutcome,
};
pub use explain::{
    explain, explain_with, CatalogAnswer, ExplainContext, Explanation, GuardsReport, PolicyReport,
    ScopeReport, Verdict as ExplainVerdict,
};
pub use file_movement::{
    log_file_movement, migrate_least_used, migrate_replicated, select_candidates, FileOutcome,
    MigrationRecord, MigrationReport, Policy, ReplicationDetail, SkipReason, UsageTracker,
};
pub use opened::{link_count, Coverage, FileId, Guards, InUse, OpenFiles};
pub use replication::{replicate, ReplicaPlacement, ReplicaStatus, ReplicationOutcome};
pub use restore::{RestoreError, RestoreOutcome, RestoreRequest};
pub use scope::{human_bytes, parse_size, Rejected, Scope, ScopeError, ScopeRefusal};

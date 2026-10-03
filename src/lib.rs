//! Library surface of `just_cache`, so the walk / policy / move / audit layers can be
//! exercised from integration tests and reused as a crate.

pub mod audit;
pub mod catalog;
pub mod digest;

pub mod disk_management;
pub mod explain;
// Test-only, but not `#[cfg(test)]`: integration tests drive the real binary, which is
// built without test cfg, so the fault seam has to be compiled into production and stay
// inert there. It is private because nothing outside the crate may set a fault.
mod faults;
pub mod file_movement;
pub mod journal;
pub mod locate;
pub mod opened;
pub mod reconcile;
pub mod replication;
pub mod restore;
pub mod scope;
pub mod scrub;

pub use audit::{
    audit, audit_with_copies, catalog_audit, catalog_repair, classify, repair, AuditError,
    AuditReport, AuditSource, Finding, RepairAction, RepairOutcome, SourceState, Verdict,
    VerdictKind, COPY_FLOOR, DEFAULT_EXAMPLES,
};
pub use catalog::{
    resolve_location_path, Catalog, CatalogError, Difference, DifferenceKind, LocationRecord,
    MalformedRow, ObjectRecord, RowPathError, ScrubSummary, ScrubTarget, SyncReport, CATALOG_NAME,
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
pub use locate::{locate, LocateError, LocateReport, LocateRequest, QueryKind};
pub use opened::{link_count, Coverage, FileId, Guards, InUse, OpenFiles};
pub use reconcile::{
    reconcile, ReconcileError, ReconcileOutcome, ReconcileRecord, ReconcileReport, ReconcileRequest,
};
pub use replication::{replicate, ReplicaPlacement, ReplicaStatus, ReplicationOutcome};
pub use restore::{
    build_verified_copy, replace_from_verified, RestoreError, RestoreOutcome, RestoreRequest,
};
pub use scope::{human_bytes, parse_size, Rejected, Scope, ScopeError, ScopeRefusal};
pub use scrub::{DamageRecord, RateLimiter, RepairRecord, ScrubError, ScrubReport, ScrubRequest};

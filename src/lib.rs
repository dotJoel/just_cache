//! Library surface of `just_cache`, so the walk / policy / move / audit layers can be
//! exercised from integration tests and reused as a crate.

pub mod audit;
#[cfg(unix)]
pub mod cache;
pub mod catalog;
pub mod digest;
pub mod disk_management;
pub mod envelope;
pub mod explain;
// The FUSE namespace provider (issue #42). Unix-only: it is the only module that
// links `fuser`, and the only path into it is the `mount` subcommand, so a build on a
// host with no FUSE kernel module still compiles every other subcommand.
#[cfg(unix)]
pub mod fuse;
// The gateway namespace provider (issue #47): WebDAV over the catalog. Pure std; only
// the `gateway` subcommand reaches it.
pub mod gateway;
pub mod namespace;
// Test-only, but not `#[cfg(test)]`: integration tests drive the real binary, which is
// built without test cfg, so the fault seam has to be compiled into production and stay
// inert there. It is private because nothing outside the crate may set a fault.
mod faults;
pub mod file_movement;
pub mod journal;
pub mod locate;
pub mod observe;
pub mod opened;
pub mod policy;
pub mod recall;
pub mod reconcile;
pub mod replication;
pub mod restore;
pub mod schedule;
pub mod scope;
pub mod scrub;
pub mod tiers;

pub use audit::{
    audit, audit_with_copies, catalog_audit, catalog_audit_no_filesystem, catalog_repair, classify,
    repair, AuditError, AuditReport, AuditSource, CatalogOnlySection, Finding, RepairAction,
    RepairOutcome, SourceState, Verdict, VerdictKind, COPY_FLOOR, DEFAULT_EXAMPLES,
};
pub use catalog::{
    format_rfc3339, resolve_location_path, Catalog, CatalogError, Difference, DifferenceKind,
    LocationRecord, MalformedRow, ObjectRecord, PinRecord, Pins, Resolution, ResolveReport,
    RowPathError, ScrubSummary, ScrubTarget, SyncReport, CATALOG_NAME,
};
pub use digest::{bytes_digest, file_digest};
pub use disk_management::{
    allocated_bytes, available_space, destination_with_room, last_use, list_files_recursive,
    move_file_with_symlink, AccessSource, DiskError, FileEntry, MoveOutcome,
};
pub use explain::{
    explain, explain_with, CatalogAnswer, ExplainContext, Explanation, GuardsReport, PolicyReport,
    RuleReport, ScopeReport, Verdict as ExplainVerdict,
};
pub use file_movement::{
    log_file_movement, migrate_least_used, migrate_replicated, select_candidates, FileOutcome,
    MigrationRecord, MigrationReport, Policy, ReplicationDetail, SkipReason, UsageTracker,
};
#[cfg(unix)]
pub use fuse::{serve as mount_serve, MountError, MountRequest};
pub use gateway::{Gateway, GatewayConfig, GatewayError};
pub use locate::{locate, LocateError, LocateReport, LocateRequest, QueryKind};
pub use namespace::{Entry as NamespaceEntry, Namespace, NamespaceError};
pub use observe::{AccessLog, FLUSH_INTERVAL};
pub use opened::{link_count, Coverage, FileId, Guards, InUse, OpenFiles};
pub use policy::{
    DownDecision, DownRule, Lifecycle, PolicyError, Rule, RuleSet, UpRule, POLICY_FILE_NAME,
};
pub use reconcile::{
    reconcile, ReconcileError, ReconcileOutcome, ReconcileRecord, ReconcileReport, ReconcileRequest,
};
pub use replication::{replicate, ReplicaPlacement, ReplicaStatus, ReplicationOutcome};
pub use restore::{
    build_verified_copy, replace_from_verified, RestoreError, RestoreOutcome, RestoreRequest,
};
pub use schedule::{
    human_duration, is_due, next_run, roots_below_floor, unix_seconds, Pass, PassSchedule,
    ScheduleError, ScheduleSet, ScheduleState, SCHEDULE_FILE_NAME, SCHEDULE_STATE_NAME,
};
pub use scope::{human_bytes, parse_size, Rejected, Scope, ScopeError, ScopeRefusal};
pub use scrub::{DamageRecord, RateLimiter, RepairRecord, ScrubError, ScrubReport, ScrubRequest};
pub use tiers::{
    CacheConfig, PromoteOn, Recall, Tier, TierSet, TiersError, Volatility, TIERS_FILE_NAME,
};

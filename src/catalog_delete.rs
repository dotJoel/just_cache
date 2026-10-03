//! Deleting a name through the catalog (issue #128).
//!
//! A delete is a catalog transition first and a filesystem operation second. One
//! transaction drops the name row and — when it was the object's last name — every
//! location row, the object and its lifecycle, and in the same commit records each file
//! it released in `pending_removal`. Only after that commit are bytes unlinked. The order
//! is the whole point:
//!
//! * a crash **before** the commit leaves the catalog and every byte as they were;
//! * a crash **after** it leaves no row vouching for the released bytes (so nothing
//!   counts them as a good copy and no name resolves to them), plus a durable record of
//!   exactly which files to finish removing. The next delete or `catalog sync` does that
//!   before it observes anything, so the leftover is never re-ingested.
//!
//! Reference counting is by object, not by file: an object reached by two names loses
//! only the name (and the name's own entry in the watch tree) when one goes, and its
//! copies are released only with the last name.
//!
//! Every refusal is decided before the transaction writes anything, so a delete either
//! commits whole or changes nothing. The refusals are the cases where finishing would
//! need a judgement the tool must not make on its own: a pin, a damaged copy (the only
//! evidence of what the bytes should be, §9), a tier that is not mounted (its bytes
//! cannot be removed, and dropping their rows would orphan them unseen), a row that does
//! not resolve under a recorded root.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use thiserror::Error;

use super::{canonical, hex, key_of, now_seconds, resolve_location_path, Catalog, CatalogError};
use crate::digest;

const KIND_BYTES: &str = "bytes";
const KIND_LINK: &str = "link";

/// Why a delete was refused. Nothing was written when one of these is returned.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeleteRefusal {
    #[error("{path} is not named in the catalog")]
    NotNamed { path: String },
    #[error("{path} is pinned until {until} (unix seconds); unpin it first")]
    Pinned { path: String, until: i64 },
    #[error(
        "{path}: a copy is on {tier}, which is not mounted; deleting now would drop the \
         row for bytes nothing could remove"
    )]
    TierUnmounted { path: String, tier: String },
    #[error(
        "{path}: the copy at {tier}/{storage_key} is recorded as damaged ({detail}); a \
         human decides what happens to the only evidence of those bytes"
    )]
    Damaged {
        path: String,
        tier: String,
        storage_key: String,
        detail: String,
    },
    #[error(
        "{path}: the row {tier}/{storage_key} does not resolve under a recorded root: {detail}"
    )]
    Unresolvable {
        path: String,
        tier: String,
        storage_key: String,
        detail: String,
    },
    #[error("--watch {watch} is not a root this catalog was synced from")]
    UnknownWatch { watch: PathBuf },
    #[error(
        "{path}: removing this name's own copy would leave the object's other names with \
         no copy at all"
    )]
    WouldStrandNames { path: String },
}

#[derive(Debug, Error)]
pub enum DeleteError {
    #[error("refused: {0}")]
    Refused(#[from] DeleteRefusal),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
}

/// What happened to one released file once the transition had committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalOutcome {
    Removed(PathBuf),
    /// Already gone: an earlier, interrupted pass removed it, or something else did.
    AlreadyGone(PathBuf),
    /// Not removed, because the file is no longer what was released (rewritten, replaced
    /// by a directory, a different size or digest). The file is left for the next sync
    /// to observe as whatever it now is; deleting it would destroy bytes nobody released.
    Kept {
        path: PathBuf,
        reason: String,
    },
    /// The unlink itself failed. The pending row is kept so the next pass retries.
    Failed {
        path: PathBuf,
        reason: String,
    },
}

/// The committed result of one delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub path: String,
    pub object: String,
    /// Names still pointing at the object. Zero means the object itself was released.
    pub names_left: usize,
    /// Location rows released by the transition.
    pub locations_released: usize,
    pub removals: Vec<RemovalOutcome>,
}

impl Catalog {
    /// Delete the name `path` (a namespace path, relative to `watch`). See the module
    /// docs for the ordering; `watch` is needed because the catalog records the watch
    /// root only as one of its roots, and the name's own entry in the tree is under it.
    pub fn delete_name(&mut self, watch: &Path, path: &str) -> Result<DeleteOutcome, DeleteError> {
        // A previous delete that died between its commit and its unlinks finishes first,
        // so its files cannot be mistaken for anything this one decides about.
        self.finish_pending_removals(true)?;

        let path = path.trim_matches('/').to_string();
        let roots = self.roots()?;
        let watch_root = canonical(watch);
        if !roots.contains(&watch_root) {
            return Err(DeleteRefusal::UnknownWatch {
                watch: watch.to_path_buf(),
            }
            .into());
        }
        let watch_tier = key_of(&watch_root);

        // IMMEDIATE: the checks below and the writes after them see one catalog. A sync
        // landing in between could otherwise add a name this delete counted as absent.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(CatalogError::from)?;

        let object: Vec<u8> = match tx
            .query_row(
                "SELECT object_id FROM name WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()
            .map_err(CatalogError::from)?
        {
            Some(object) => object,
            None => return Err(DeleteRefusal::NotNamed { path }.into()),
        };
        let size: i64 = tx
            .query_row(
                "SELECT size FROM object WHERE id = ?1",
                params![object],
                |row| row.get(0),
            )
            .map_err(CatalogError::from)?;

        let pinned: Option<i64> = tx
            .query_row(
                "SELECT pinned_until FROM lifecycle WHERE object_id = ?1",
                params![object],
                |row| row.get(0),
            )
            .optional()
            .map_err(CatalogError::from)?
            .flatten();
        if let Some(until) = pinned {
            if until > now_seconds() {
                return Err(DeleteRefusal::Pinned { path, until }.into());
            }
        }

        let other_names: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM name WHERE object_id = ?1 AND path <> ?2",
                params![object, path],
                |row| row.get(0),
            )
            .map_err(CatalogError::from)?;
        let last = other_names == 0;

        let locations: Vec<(String, String)> = {
            let mut stmt = tx
                .prepare("SELECT tier, storage_key FROM location WHERE object_id = ?1")
                .map_err(CatalogError::from)?;
            let rows = stmt
                .query_map(params![object], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(CatalogError::from)?;
            rows.collect::<Result<_, _>>().map_err(CatalogError::from)?
        };
        // With other names left, only this name's own hot copy goes; the object's other
        // copies are still vouching for the names that remain.
        let released: Vec<(String, String)> = locations
            .iter()
            .filter(|(tier, key)| last || (*tier == watch_tier && *key == path))
            .cloned()
            .collect();
        if !last && released.len() == locations.len() {
            return Err(DeleteRefusal::WouldStrandNames { path }.into());
        }

        let mut pending: Vec<(PathBuf, &str)> = Vec::new();
        for (tier, key) in &released {
            let damage: Option<String> = tx
                .query_row(
                    "SELECT detail FROM damage WHERE tier = ?1 AND storage_key = ?2",
                    params![tier, key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(CatalogError::from)?;
            if let Some(detail) = damage {
                return Err(DeleteRefusal::Damaged {
                    path,
                    tier: tier.clone(),
                    storage_key: key.clone(),
                    detail,
                }
                .into());
            }
            let file = resolve_location_path(tier, key, &roots).map_err(|err| {
                DeleteRefusal::Unresolvable {
                    path: path.clone(),
                    tier: tier.clone(),
                    storage_key: key.clone(),
                    detail: err.detail().to_string(),
                }
            })?;
            // An unmounted tier reads as a missing root directory. Its bytes cannot be
            // removed, and releasing their row anyway would leave them on that disk with
            // nothing recording they exist (out of scope here: the vault model, P3).
            if !Path::new(tier).is_dir() {
                return Err(DeleteRefusal::TierUnmounted {
                    path,
                    tier: tier.clone(),
                }
                .into());
            }
            pending.push((file, KIND_BYTES));
        }
        // The name's own entry in the tree: the hot file (already listed above when it
        // is a location) or the symlink a migrated name is.
        let name_file = watch_root.join(&path);
        if let Ok(metadata) = fs::symlink_metadata(&name_file) {
            if metadata.file_type().is_symlink() {
                pending.push((name_file, KIND_LINK));
            }
        }

        tx.execute("DELETE FROM name WHERE path = ?1", params![path])
            .map_err(CatalogError::from)?;
        tx.execute("DELETE FROM cache_residency WHERE key = ?1", params![path])
            .map_err(CatalogError::from)?;
        for (tier, key) in &released {
            for table in ["location", "scrub_state", "damage"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE tier = ?1 AND storage_key = ?2"),
                    params![tier, key],
                )
                .map_err(CatalogError::from)?;
            }
        }
        if last {
            for statement in [
                "DELETE FROM scrub_state WHERE object_id = ?1",
                "DELETE FROM damage WHERE object_id = ?1",
                "DELETE FROM lifecycle WHERE object_id = ?1",
                "DELETE FROM object WHERE id = ?1",
            ] {
                tx.execute(statement, params![object])
                    .map_err(CatalogError::from)?;
            }
        }
        for (file, kind) in &pending {
            tx.execute(
                "INSERT OR IGNORE INTO pending_removal (path, object_id, size, kind)
                 VALUES (?1, ?2, ?3, ?4)",
                params![key_of(file), object, size, kind],
            )
            .map_err(CatalogError::from)?;
        }
        tx.commit().map_err(CatalogError::from)?;

        // The fault seam for the window this module exists to make safe: the transition
        // is durable, no byte has been removed yet. Abort, not return — a crash runs no
        // destructor and no cleanup, and the test must see exactly what one leaves.
        if let Some(fault) = crate::faults::Fault::from_env() {
            if fault.mode == crate::faults::FaultMode::DeleteAfterCommit {
                std::process::abort();
            }
        }

        let removals = self.finish_pending_removals(false)?;
        Ok(DeleteOutcome {
            path,
            object: hex(&object),
            names_left: other_names as usize,
            locations_released: released.len(),
            removals,
        })
    }

    /// Unlink every file a committed delete released and has not yet removed.
    ///
    /// `verify` re-hashes a regular file before removing it. Straight after a commit the
    /// file is the one just checked, so size and type suffice; on recovery the window may
    /// have been arbitrarily long, and a new file written at the same path must never be
    /// removed on the strength of an old transaction.
    pub fn finish_pending_removals(
        &mut self,
        verify: bool,
    ) -> Result<Vec<RemovalOutcome>, CatalogError> {
        let rows: Vec<(String, Vec<u8>, i64, String)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path, object_id, size, kind FROM pending_removal ORDER BY path")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let mut outcomes = Vec::with_capacity(rows.len());
        for (path_text, object, size, kind) in rows {
            let path = PathBuf::from(&path_text);
            let outcome = match fs::symlink_metadata(&path) {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    RemovalOutcome::AlreadyGone(path)
                }
                Err(err) => RemovalOutcome::Failed {
                    path,
                    reason: err.to_string(),
                },
                Ok(metadata) => {
                    let still_released = if kind == KIND_LINK {
                        if metadata.file_type().is_symlink() {
                            Ok(())
                        } else {
                            Err("no longer a symlink".to_string())
                        }
                    } else if !metadata.is_file() {
                        Err("no longer a regular file".to_string())
                    } else if metadata.len() != size as u64 {
                        Err(format!("size is now {}, released {size}", metadata.len()))
                    } else if verify {
                        match digest::file_digest(&path) {
                            Ok(hash) if hash.as_bytes()[..] == object[..] => Ok(()),
                            Ok(_) => Err("bytes no longer match the released object".to_string()),
                            Err(err) => Err(format!("cannot re-read before removing: {err}")),
                        }
                    } else {
                        Ok(())
                    };
                    match still_released {
                        Err(reason) => RemovalOutcome::Kept { path, reason },
                        Ok(()) => match fs::remove_file(&path) {
                            Ok(()) => RemovalOutcome::Removed(path),
                            Err(err) => RemovalOutcome::Failed {
                                path,
                                reason: err.to_string(),
                            },
                        },
                    }
                }
            };
            // A failed unlink keeps its row so the next pass retries; every other outcome
            // has settled what happens to that file.
            if !matches!(outcome, RemovalOutcome::Failed { .. }) {
                self.conn.execute(
                    "DELETE FROM pending_removal WHERE path = ?1",
                    params![path_text],
                )?;
            }
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// Files a committed delete released that are still waiting to be unlinked.
    pub fn pending_removals(&self) -> Result<Vec<PathBuf>, CatalogError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM pending_removal ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(PathBuf::from(row?));
        }
        Ok(out)
    }
}

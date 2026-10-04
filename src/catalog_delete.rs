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
//! evidence of what the bytes should be, §9), a row that does not resolve under a
//! recorded root and names no volume.
//!
//! # A tier that is not mounted is not a refusal (issue #144)
//!
//! When a released copy sits on a tier that is not reachable *now* — an `fs` root that is
//! not mounted, or an `offline` volume that is not the one in the drive — the delete is
//! still committed: the release is recorded in `pending_removal` as `(tier, storage_key)`,
//! not only as a path, and the same code that finishes a crashed delete completes it on
//! the next pass, once the tier returns. The old behaviour refused whole and left the file
//! undeletable; §9 named that refusal as the gap this closes. Completion is deliberately
//! conservative: an `offline` location resolves to `<mount>/<relative>` **only** when the
//! volume ledger says the exact volume named by the storage key is the one mounted in that
//! tier, so bytes are never unlinked from the wrong disk.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use thiserror::Error;

use super::{canonical, hex, key_of, now_seconds, resolve_location_path, Catalog, CatalogError};
use crate::digest;
use crate::offline;
use crate::tiers::TierSet;

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
    /// The released bytes are on a tier that is not reachable now. The intent stays in
    /// `pending_removal` and is completed when the tier returns (issue #144); the location
    /// is named here so silence is never the answer.
    Deferred {
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

/// One file the transition released, as recorded in `pending_removal`.
struct PendingRow {
    /// The path to unlink when the location resolves to one; a `<tier>/<key>` surrogate
    /// for an offline copy whose mount is not knowable yet. Never unlinked unless it came
    /// from [`resolve_released`], which is what makes the surrogate safe.
    path: PathBuf,
    kind: &'static str,
    /// The released location, when it is one (`None` for a name symlink).
    tier: Option<String>,
    storage_key: Option<String>,
}

/// A `pending_removal` row as read back: `(path, object, size, kind, tier, storage_key)`.
type PendingRecord = (String, Vec<u8>, i64, String, Option<String>, Option<String>);

/// Where a recorded `(tier, storage_key)` resolves to, or why it cannot be finished yet.
enum Released {
    /// A path that exists now and is safe to unlink. `sealed` marks an `offline` copy:
    /// its bytes on the volume are the encrypted envelope, so the plaintext digest and
    /// size the row carries cannot (and need not) be re-checked before the unlink.
    Ready { path: PathBuf, sealed: bool },
    /// Correct and recorded, but the tier is not reachable now: keep the intent.
    Deferred { path: PathBuf, reason: String },
    /// The row itself cannot be trusted (a key that escapes, a volume the ledger does not
    /// know). Kept and reported, never guessed at.
    Refused { reason: String },
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

        let mut pending: Vec<PendingRow> = Vec::new();
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
            // An `offline` copy: its key is `<volume-id>/<relative>` and the volume ledger
            // vouches for the id. The tier is a configured name, not a root, and the mount
            // is unknown until the volume is inserted — record the location and let the
            // resolver finish it when the volume is the mounted one (never otherwise).
            let offline_volume = match offline::parse_storage_key(key) {
                Some((volume_id, _)) if volume_exists(&tx, &volume_id)? => Some(volume_id),
                _ => None,
            };
            let target = match offline_volume {
                Some(_) => PathBuf::from(format!("{tier}/{key}")),
                None => {
                    // A filesystem location: it must resolve under a recorded root. An
                    // absent root directory is *not* a refusal any more (#144) — the
                    // intent is recorded and completed when the tier returns; only a tier
                    // that is not a root, or a key that escapes one, is refused by name.
                    resolve_location_path(tier, key, &roots).map_err(|err| {
                        DeleteRefusal::Unresolvable {
                            path: path.clone(),
                            tier: tier.clone(),
                            storage_key: key.clone(),
                            detail: err.detail().to_string(),
                        }
                    })?
                }
            };
            pending.push(PendingRow {
                path: target,
                kind: KIND_BYTES,
                tier: Some(tier.clone()),
                storage_key: Some(key.clone()),
            });
        }
        // The name's own entry in the tree: the hot file (already listed above when it
        // is a location) or the symlink a migrated name is.
        let name_file = watch_root.join(&path);
        if let Ok(metadata) = fs::symlink_metadata(&name_file) {
            if metadata.file_type().is_symlink() {
                pending.push(PendingRow {
                    path: name_file,
                    kind: KIND_LINK,
                    tier: None,
                    storage_key: None,
                });
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
        for row in &pending {
            tx.execute(
                "INSERT OR IGNORE INTO pending_removal
                     (path, object_id, size, kind, tier, storage_key)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    key_of(&row.path),
                    object,
                    size,
                    row.kind,
                    row.tier,
                    row.storage_key
                ],
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
    ///
    /// A row that records a `(tier, storage_key)` is resolved before its `path` is used:
    /// an absent tier, or an offline volume that is not the one in the drive, leaves the
    /// row in place and reports it as [`RemovalOutcome::Deferred`] (issue #144).
    pub fn finish_pending_removals(
        &mut self,
        verify: bool,
    ) -> Result<Vec<RemovalOutcome>, CatalogError> {
        let tiers = self.pending_tiers.clone().or_else(|| self.tiers_beside());
        let roots = self.roots()?;
        let rows: Vec<PendingRecord> = {
            let mut stmt = self.conn.prepare(
                "SELECT path, object_id, size, kind, tier, storage_key
                   FROM pending_removal ORDER BY path",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let mut outcomes = Vec::with_capacity(rows.len());
        for (path_text, object, size, kind, tier, storage_key) in rows {
            let recorded = PathBuf::from(&path_text);
            // Resolve the recorded location when the row names one; otherwise the `path`
            // column is authoritative (a name symlink, or a row predating #144).
            let (path, sealed) = match (kind.as_str(), &tier, &storage_key) {
                (KIND_BYTES, Some(tier), Some(key)) => {
                    match resolve_released(self, tier, key, &roots, tiers.as_ref()) {
                        Released::Ready { path, sealed } => (path, sealed),
                        Released::Deferred { path, reason } => {
                            outcomes.push(RemovalOutcome::Deferred { path, reason });
                            continue;
                        }
                        Released::Refused { reason } => {
                            // Never guessed at, and never dropped: forgetting the row would
                            // forget a file entirely.
                            outcomes.push(RemovalOutcome::Deferred {
                                path: recorded,
                                reason,
                            });
                            continue;
                        }
                    }
                }
                _ => (recorded, false),
            };
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
                    } else if sealed {
                        // An offline copy is the encrypted envelope: its plaintext size and
                        // digest cannot be checked against the sealed bytes, and the ledger
                        // already vouched for the volume the row names. Type is enough.
                        Ok(())
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
            // A failed unlink keeps its row so the next pass retries; a deferred location
            // keeps it until its tier returns. Every other outcome has settled what
            // happens to that file.
            if !matches!(
                outcome,
                RemovalOutcome::Failed { .. } | RemovalOutcome::Deferred { .. }
            ) {
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

    /// The tier configuration beside this catalog, when there is one — open-if-present,
    /// never created, the rule every other config lookup follows. Used to locate an
    /// `offline` tier's mount when finishing a deferred removal (#144).
    fn tiers_beside(&self) -> Option<TierSet> {
        let dir = self.path.parent().filter(|p| !p.as_os_str().is_empty())?;
        TierSet::load_beside(dir).ok().flatten()
    }
}

/// Whether the ledger records a volume with this id. That is what makes a `<id>/<path>`
/// storage key an `offline` location rather than a filesystem key.
fn volume_exists(conn: &Connection, id: &str) -> Result<bool, CatalogError> {
    let found: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM volume WHERE id = ?1)",
        params![id],
        |row| row.get(0),
    )?;
    Ok(found != 0)
}

/// Resolve a released `(tier, storage_key)` to the file to unlink now, or say why not.
///
/// A filesystem tier is one of the catalog's recorded roots: its location resolves under
/// that root, and an absent root directory defers rather than refuses. An `offline` tier
/// is a configured name whose key is `<volume-id>/<relative>`; it resolves to
/// `<mount>/<relative>` only when the ledger says the *same* volume is the one mounted in
/// the tier, so a removal is never carried out against the wrong disk.
fn resolve_released(
    catalog: &Catalog,
    tier: &str,
    storage_key: &str,
    roots: &[PathBuf],
    tiers: Option<&TierSet>,
) -> Released {
    if roots.iter().any(|root| root.as_path() == Path::new(tier)) {
        return match resolve_location_path(tier, storage_key, roots) {
            Err(err) => Released::Refused {
                reason: err.detail().to_string(),
            },
            Ok(path) => {
                if Path::new(tier).is_dir() {
                    Released::Ready {
                        path,
                        sealed: false,
                    }
                } else {
                    Released::Deferred {
                        path,
                        reason: format!(
                            "tier `{tier}` is not mounted; the copy is held until it returns"
                        ),
                    }
                }
            }
        };
    }
    let Some((volume_id, relative)) = offline::parse_storage_key(storage_key) else {
        return Released::Refused {
            reason: format!(
                "`{tier}/{storage_key}` is neither a recorded root nor a volume storage key"
            ),
        };
    };
    match catalog.volume(&volume_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Released::Refused {
                reason: format!(
                    "no volume `{volume_id}` is recorded in the ledger; `{tier}/{storage_key}` \
                     cannot be finished"
                ),
            }
        }
        Err(err) => {
            return Released::Refused {
                reason: err.to_string(),
            }
        }
    }
    let Some(tier_config) = tiers.and_then(|set| set.get(tier)) else {
        return Released::Deferred {
            path: PathBuf::from(format!("{tier}/{storage_key}")),
            reason: format!(
                "tier `{tier}` is offline and no tiers.toml beside the catalog describes its \
                 mount; insert the volume and place a tiers.toml so the removal can finish"
            ),
        };
    };
    let mount = tier_config.path.clone();
    match catalog.mounted_volume_for_tier(tier) {
        Err(err) => Released::Refused {
            reason: err.to_string(),
        },
        Ok(Some(mounted)) if mounted.id == volume_id => {
            if !mount.is_dir() {
                Released::Deferred {
                    path: PathBuf::from(format!("{tier}/{storage_key}")),
                    reason: format!(
                        "volume `{volume_id}` is recorded mounted in tier `{tier}` but {} is \
                         not there; insert it and run the next pass",
                        mount.display()
                    ),
                }
            } else {
                let path = mount.join(&relative);
                if path.starts_with(&mount) {
                    Released::Ready { path, sealed: true }
                } else {
                    Released::Refused {
                        reason: format!("`{storage_key}` escapes the mount {}", mount.display()),
                    }
                }
            }
        }
        Ok(Some(mounted)) => Released::Deferred {
            path: PathBuf::from(format!("{tier}/{storage_key}")),
            reason: format!(
                "tier `{tier}` holds volume `{}`, not `{volume_id}`; nothing is removed from \
                 the wrong volume",
                mounted.id
            ),
        },
        Ok(None) => Released::Deferred {
            path: PathBuf::from(format!("{tier}/{storage_key}")),
            reason: format!(
                "volume `{volume_id}` is not mounted in tier `{tier}`; insert it and run the \
                 next pass"
            ),
        },
    }
}

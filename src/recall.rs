//! Inline recall: a read of an offloaded object pulls it back to the hot tier and is
//! served from there (issue #44, `docs/design.md` §4).
//!
//! This is the provider-agnostic half of recall-on-read. The FUSE binding calls
//! [`Recaller::recall`] from `open`; everything that decides *whether* a recall may run,
//! *how* the bytes are placed, and *what the catalog says afterwards* lives here, so it
//! is tested against a real catalog and a real second filesystem with no `/dev/fuse`.
//!
//! # The placement is `restore`, not a second copier
//!
//! A recall under [`RecallPolicy::Promote`] is [`restore::restore`] with `--remove-copy`
//! off: copy into a private partial, read back, hash against the catalog's recorded
//! checksum, atomic rename over the symlink the mover left. Writing a second copier for
//! the mount would mean a second place where verify-before-trust could drift. Only after
//! that does the catalog learn about the copy, and what it records is the digest of the
//! placed file read back *again* — the real checksum of what is on disk — so a copy that
//! does not read back to the recorded checksum is written unverified (unknown, never
//! good) and the object is left `offloaded`.
//!
//! # Slow tiers refuse
//!
//! A tier whose recall class is `min` or `hours` is not recalled inline: a read that
//! blocks for minutes is an application timeout, and a timeout looks like a broken
//! file. Such a read is refused with [`RecallError::Refused`], naming the tier and the
//! `just_cache restore <path>` that does the job explicitly (the S3 restore-request
//! model the gateway already uses). A tier the mount has no `tiers.toml` entry for has
//! no recorded class, and is recalled: the CLI only accepts a `--dest` that exists, and
//! refusing every read on an unconfigured box would turn the default into a broken one.
//!
//! # One recall per object
//!
//! Two readers of one offloaded object must not place two copies. Recalls are
//! single-flighted per object id: the first reader recalls, every concurrent reader of
//! the same object waits on that flight and is handed its result. A reader arriving
//! after the flight finished finds the hot copy in the catalog and is served from it.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use crate::catalog::{self, Catalog};
use crate::digest;
use crate::restore::{self, RestoreRequest};
use crate::tiers::{Recall, TierSet};

/// What happens to the bytes an inline recall pulls up.
///
/// The default is [`RecallPolicy::Promote`] (`docs/design.md` §4): a file someone just
/// read is the best available evidence that it is warm again, and leaving it on the
/// slow tier means paying the slow read every time. The idle rule demotes it again
/// when it cools, exactly as it would any other hot file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecallPolicy {
    /// Place a verified copy on the hot tier and make it the tier of record.
    #[default]
    Promote,
    /// Serve the bytes from the tier of record and place nothing.
    ReadThrough,
}

impl RecallPolicy {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "promote" => Some(RecallPolicy::Promote),
            "read-through" => Some(RecallPolicy::ReadThrough),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RecallPolicy::Promote => "promote",
            RecallPolicy::ReadThrough => "read-through",
        }
    }
}

/// Where a read of the object should take its bytes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallOutcome {
    /// The object already had a hot copy; nothing was recalled.
    Hot { bytes: PathBuf },
    /// This call recalled the object onto the hot tier.
    Recalled { bytes: PathBuf, tier: String },
    /// Another reader's concurrent recall placed the copy; this call waited for it.
    Joined { bytes: PathBuf, tier: String },
    /// Policy is read-through: serve the tier of record directly, nothing placed.
    ReadThrough { bytes: PathBuf, tier: String },
}

impl RecallOutcome {
    /// The file the read should be served from.
    pub fn bytes(&self) -> &Path {
        match self {
            RecallOutcome::Hot { bytes }
            | RecallOutcome::Recalled { bytes, .. }
            | RecallOutcome::Joined { bytes, .. }
            | RecallOutcome::ReadThrough { bytes, .. } => bytes,
        }
    }
}

/// Why a read could not be served by an inline recall. Every variant that concerns a
/// tier names it, so the operator reading the mount's stderr knows which disk to look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallError {
    /// The catalog does not name this path.
    NotCatalogued { path: String },
    /// The tier of record is too slow to recall inline; run `restore` instead.
    Refused {
        path: String,
        tier: String,
        recall: Recall,
    },
    /// The recall ran and failed. The catalog was not told the object is `present`.
    Failed {
        path: String,
        tier: String,
        detail: String,
    },
    /// The catalog could not be read or written.
    Catalog { detail: String },
}

impl fmt::Display for RecallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecallError::NotCatalogued { path } => {
                write!(f, "nothing in the catalog names {path:?}")
            }
            RecallError::Refused { path, tier, recall } => write!(
                f,
                "{path} is offloaded to tier `{tier}` (recall class `{}`), which is too slow \
                 to recall inline; run `just_cache restore {path}` and read it again",
                recall.as_str()
            ),
            RecallError::Failed { path, tier, detail } => write!(
                f,
                "recalling {path} from tier `{tier}` failed: {detail}; the catalog still \
                 records it as not present"
            ),
            RecallError::Catalog { detail } => write!(f, "the catalog could not be used: {detail}"),
        }
    }
}

impl std::error::Error for RecallError {}

type FlightResult = Result<RecallOutcome, RecallError>;

/// One in-progress recall that concurrent readers of the same object wait on.
#[derive(Default)]
struct Flight {
    result: Mutex<Option<FlightResult>>,
    done: Condvar,
}

/// Test seam: called by the recalling reader immediately before bytes are copied.
pub type CopyHook = Arc<dyn Fn() + Send + Sync>;

/// Recalls objects for one mount (or one test).
///
/// It opens its own catalog connection per recall rather than sharing the mount's: a
/// recall can take as long as copying the file, and holding the mount's connection
/// across it would stall every other lookup.
pub struct Recaller {
    catalog_path: PathBuf,
    watch: PathBuf,
    watch_tier: String,
    tiers: Option<TierSet>,
    policy: RecallPolicy,
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    copy_hook: Option<CopyHook>,
}

impl Recaller {
    pub fn new(
        catalog_path: PathBuf,
        watch: PathBuf,
        tiers: Option<TierSet>,
        policy: RecallPolicy,
    ) -> Self {
        let canonical = std::fs::canonicalize(&watch).unwrap_or_else(|_| watch.clone());
        Self {
            catalog_path,
            watch_tier: canonical.to_string_lossy().into_owned(),
            watch: canonical,
            tiers,
            policy,
            flights: Mutex::new(HashMap::new()),
            copy_hook: None,
        }
    }

    /// Install a hook run right before the copy. Exists so a test can hold a recall open
    /// long enough for a second reader to race it; production never sets one.
    #[doc(hidden)]
    pub fn with_copy_hook(mut self, hook: CopyHook) -> Self {
        self.copy_hook = Some(hook);
        self
    }

    pub fn policy(&self) -> RecallPolicy {
        self.policy
    }

    /// Make the object named `path` readable, recalling it if it is offloaded, and say
    /// which file the read should be served from.
    pub fn recall(&self, path: &str) -> FlightResult {
        let path = path.trim_matches('/').to_string();
        let plan = self.plan(&path)?;
        let (object, tier) = match plan {
            Plan::Serve(outcome) => return Ok(outcome),
            Plan::Recall { object, tier } => (object, tier),
        };

        let (flight, leader) = {
            let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            match flights.get(&object) {
                Some(flight) => (Arc::clone(flight), false),
                None => {
                    let flight = Arc::new(Flight::default());
                    flights.insert(object.clone(), Arc::clone(&flight));
                    (flight, true)
                }
            }
        };

        if !leader {
            let mut result = flight.result.lock().unwrap_or_else(|e| e.into_inner());
            while result.is_none() {
                result = flight.done.wait(result).unwrap_or_else(|e| e.into_inner());
            }
            return match result.clone().expect("checked above") {
                Ok(RecallOutcome::Recalled { bytes, tier }) => {
                    Ok(RecallOutcome::Joined { bytes, tier })
                }
                other => other,
            };
        }

        // The leader re-plans under the flight: a recall that finished between this
        // reader's plan and its claiming the flight has already placed the copy, and
        // copying again would be the second placement this exists to prevent.
        let result = match self.plan(&path) {
            Ok(Plan::Serve(outcome)) => Ok(outcome),
            Ok(Plan::Recall { .. }) => self.place(&path, &object, &tier),
            Err(err) => Err(err),
        };

        *flight.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result.clone());
        flight.done.notify_all();
        self.flights
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&object);
        result
    }

    fn open_catalog(&self) -> Result<Catalog, RecallError> {
        Catalog::open(&self.catalog_path).map_err(|err| RecallError::Catalog {
            detail: err.to_string(),
        })
    }

    fn tier_name(&self, tier: &str) -> String {
        match &self.tiers {
            Some(tiers) => tiers.name_for_tier_key(tier),
            None => tier.to_string(),
        }
    }

    /// Decide, from the catalog, whether this path needs a recall at all.
    fn plan(&self, path: &str) -> Result<Plan, RecallError> {
        let catalog = self.open_catalog()?;
        let catalog_err = |err: catalog::CatalogError| RecallError::Catalog {
            detail: err.to_string(),
        };
        let record = catalog
            .record_for_path(path)
            .map_err(catalog_err)?
            .ok_or_else(|| RecallError::NotCatalogued {
                path: path.to_string(),
            })?;
        let roots = catalog.roots().map_err(catalog_err)?;

        // A verified hot copy that is really there is served as is: no recall.
        for location in &record.locations {
            if location.tier == self.watch_tier && location.verified {
                if let Ok(bytes) =
                    catalog::resolve_location_path(&location.tier, &location.storage_key, &roots)
                {
                    if std::fs::symlink_metadata(&bytes).is_ok_and(|m| m.is_file()) {
                        return Ok(Plan::Serve(RecallOutcome::Hot { bytes }));
                    }
                }
            }
        }

        let primary = record.primary().ok_or_else(|| RecallError::Failed {
            path: path.to_string(),
            tier: "<none>".to_string(),
            detail: "the object has no location row, so there is nothing to recall".to_string(),
        })?;
        let tier = self.tier_name(&primary.tier);

        if let Some(recall) = self
            .tiers
            .as_ref()
            .and_then(|tiers| tiers.recall_for_tier_key(&primary.tier))
        {
            if recall >= Recall::Min {
                return Err(RecallError::Refused {
                    path: path.to_string(),
                    tier,
                    recall,
                });
            }
        }

        if self.policy == RecallPolicy::ReadThrough {
            let bytes = catalog::resolve_location_path(&primary.tier, &primary.storage_key, &roots)
                .map_err(|err| RecallError::Failed {
                    path: path.to_string(),
                    tier: tier.clone(),
                    detail: err.detail().to_string(),
                })?;
            return Ok(Plan::Serve(RecallOutcome::ReadThrough { bytes, tier }));
        }

        Ok(Plan::Recall {
            object: record.id,
            tier,
        })
    }

    /// Place the verified hot copy and tell the catalog what was read back.
    fn place(&self, path: &str, object: &str, tier: &str) -> FlightResult {
        let failed = |detail: String| RecallError::Failed {
            path: path.to_string(),
            tier: tier.to_string(),
            detail,
        };
        if let Some(hook) = &self.copy_hook {
            hook();
        }

        let mut catalog = self.open_catalog()?;
        let roots = catalog.roots().map_err(|err| RecallError::Catalog {
            detail: err.to_string(),
        })?;
        let dests: Vec<PathBuf> = roots
            .into_iter()
            .filter(|root| root.as_path() != self.watch.as_path())
            .collect();
        let hot = self.watch.join(path);
        restore::restore(&RestoreRequest {
            path: &hot,
            watch: &self.watch,
            dests: &dests,
            remove_copy: false,
            catalog: Some(&catalog),
            object_tier_configs: &[],
            encryption_keys: &[],
        })
        .map_err(|err| failed(err.to_string()))?;

        // Read the placed file back once more and record *that* digest. `restore`
        // already refused a copy that did not match, so a mismatch here means the bytes
        // changed under us after the rename; recording it unverified keeps it unknown.
        let found = digest::file_digest(&hot).ok().map(|hash| hash.to_hex());
        let good = catalog
            .record_recall(object, &self.watch_tier, path, found.as_deref())
            .map_err(|err| RecallError::Catalog {
                detail: err.to_string(),
            })?;
        if !good {
            return Err(failed(
                "the placed copy did not read back to the recorded checksum; it is recorded \
                 as unknown, not good"
                    .to_string(),
            ));
        }
        Ok(RecallOutcome::Recalled {
            bytes: hot,
            tier: tier.to_string(),
        })
    }
}

enum Plan {
    Serve(RecallOutcome),
    Recall { object: String, tier: String },
}

//! The namespace layer: the catalog's names, resolved into a directory tree with a
//! filesystem path for every file's bytes.
//!
//! This is the provider-agnostic half of `just_cache mount` (issue #42). The FUSE
//! binding in [`crate::fuse`] is a thin adapter over it; everything that decides
//! *which names exist*, *what a file's size is*, and *where its bytes live* lives
//! here, so it can be tested against a real catalog with no `/dev/fuse` and no
//! privileges — which is exactly what CI can do.
//!
//! # Where a file's bytes come from
//!
//! The catalog records a **tier of record** per object: the primary location row
//! (`ObjectRecord::primary`). Resolving that row into a filesystem path goes through
//! [`catalog::resolve_location_path`] against the roots the catalog was synced from,
//! so a hand-edited catalog can never point the mount at a file outside every
//! configured root (#72). A name whose primary row is missing, or whose row is
//! malformed, is reported as [`NamespaceError::Unresolvable`] — never silently turned
//! into an empty file or a missing one. That is the lookup half of "the mount fails
//! closed": a directory that the catalog says has files never reads as an empty tree
//! just because a row could not be resolved.
//!
//! # Directories
//!
//! Only files are named in the catalog; directories are the prefixes of those names.
//! A path is a directory when some catalogued name sits beneath it, and its children
//! are the next path component of each such name. A name that is also a prefix of
//! another name (a file and a directory claiming one path, which a POSIX tree cannot
//! hold) is resolved as the file — the catalog is the authority on what is named, and
//! the FUSE layer never has to guess.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::RwLock;

use crate::catalog::{self, Catalog};

/// What a namespace path is.
///
/// The bytes path for a [`Entry::File`] is resolved and proven under a trusted root
/// before this is returned, so a caller can open it without re-checking containment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A directory: a prefix of one or more catalogued names. `children` are the
    /// immediate child names, files and directories together.
    Directory { path: String, children: Vec<String> },

    /// A catalogued file, with the recorded size and the resolved bytes path of its
    /// tier of record.
    File {
        path: String,
        size: u64,
        object: String,
        bytes: PathBuf,
    },
}

impl Entry {
    /// The namespace-relative path this entry answers to.
    pub fn path(&self) -> &str {
        match self {
            Entry::Directory { path, .. } | Entry::File { path, .. } => path,
        }
    }

    /// True when this entry names a file.
    pub fn is_file(&self) -> bool {
        matches!(self, Entry::File { .. })
    }
}

/// Why a namespace path could not be resolved.
#[derive(Debug)]
pub enum NamespaceError {
    /// Nothing in the catalog names this path or sits beneath it.
    NotFound { path: String },

    /// The path is named, but its tier of record could not be turned into a path the
    /// mount may touch: no location row, or a row that is not under a trusted root.
    /// This is deliberately *not* `NotFound` — an unresolvable row is a failure to
    /// serve, not evidence that the file is absent, and a read of it must report an
    /// error rather than an empty file (#42: access reports failure, never success
    /// with no files).
    Unresolvable { path: String, detail: String },

    /// The catalog could not be read.
    Catalog(catalog::CatalogError),
}

impl std::fmt::Display for NamespaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamespaceError::NotFound { path } => {
                write!(f, "nothing in the catalog names {path:?}")
            }
            NamespaceError::Unresolvable { path, detail } => write!(
                f,
                "cannot resolve the tier of record for {path:?}: {detail}"
            ),
            NamespaceError::Catalog(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for NamespaceError {}

impl From<catalog::CatalogError> for NamespaceError {
    fn from(err: catalog::CatalogError) -> Self {
        NamespaceError::Catalog(err)
    }
}

/// The names a catalog holds, indexed for lookup, with the roots its rows are proven
/// against. Built once at mount time; a mount does not observe catalog changes, so a
/// `catalog sync` while the daemon runs is picked up on the next mount.
pub struct Namespace {
    /// Behind a lock so a delete through the mount (issue #128) can retire a name the
    /// moment its catalog transition commits: an index frozen at mount time would keep
    /// answering a name the catalog no longer holds, and serve bytes nobody vouches for.
    names: RwLock<BTreeMap<String, String>>,
    roots: Vec<PathBuf>,
}

impl Namespace {
    /// Build the index from a catalog. Reads every name and the recorded roots.
    pub fn new(catalog: &Catalog) -> Result<Self, NamespaceError> {
        let names = catalog
            .all_names()?
            .into_iter()
            .collect::<BTreeMap<String, String>>();
        let roots = catalog.roots()?;
        Ok(Self {
            names: RwLock::new(names),
            roots,
        })
    }

    /// The trusted roots every location row is resolved against. Empty for a catalog
    /// written before roots were recorded — in which case every row is unresolvable
    /// and the mount must refuse to start rather than serve nothing.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Whether the catalog names this path as a file.
    pub fn names_file(&self, path: &str) -> bool {
        self.names.read().unwrap().contains_key(path)
    }

    /// Forget a name after the catalog transition that deleted it has committed.
    pub fn forget(&self, path: &str) {
        self.names.write().unwrap().remove(&normalize(path));
    }

    /// True when some catalogued name sits below `dir`, i.e. the directory is not empty
    /// as far as the catalog is concerned.
    pub fn has_children(&self, dir: &str) -> bool {
        !self.children(&normalize(dir)).is_empty()
    }

    /// The recorded object id for a catalogued path, hex-encoded.
    pub fn object_id(&self, path: &str) -> Option<String> {
        self.names.read().unwrap().get(path).cloned()
    }

    /// Resolve a namespace-relative path. `""` is the mount root.
    ///
    /// A catalogued name always wins over an implied directory of the same path.
    pub fn lookup(&self, catalog: &Catalog, path: &str) -> Result<Entry, NamespaceError> {
        let path = normalize(path);

        if path.is_empty() {
            let children = self.children(&path);
            return Ok(Entry::Directory { path, children });
        }

        if self.names_file(&path) {
            return self.file_entry(catalog, &path);
        }

        let children = self.children(&path);
        if !children.is_empty() {
            return Ok(Entry::Directory { path, children });
        }

        Err(NamespaceError::NotFound { path })
    }

    /// The immediate child names of a directory path, sorted. Empty when nothing is
    /// catalogued beneath it — which for a real directory prefix cannot happen, but a
    /// caller still has to distinguish "empty directory" from "not a directory".
    fn children(&self, dir: &str) -> Vec<String> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut children = BTreeSet::new();
        for name in self.names.read().unwrap().keys() {
            if let Some(rest) = name.strip_prefix(&prefix) {
                if rest.is_empty() {
                    continue;
                }
                let child = rest.split('/').next().unwrap_or(rest);
                children.insert(child.to_string());
            }
        }
        children.into_iter().collect()
    }

    /// Resolve a catalogued name into a [`Entry::File`], proving its tier of record
    /// stays under a trusted root.
    fn file_entry(&self, catalog: &Catalog, path: &str) -> Result<Entry, NamespaceError> {
        let record =
            catalog
                .record_for_path(path)?
                .ok_or_else(|| NamespaceError::Unresolvable {
                    path: path.to_string(),
                    detail: "the name row exists but its object does not".to_string(),
                })?;

        let primary = record
            .primary()
            .ok_or_else(|| NamespaceError::Unresolvable {
                path: path.to_string(),
                detail: "the object has no location row, so it has no bytes to serve".to_string(),
            })?;

        let bytes =
            catalog::resolve_location_path(&primary.tier, &primary.storage_key, &self.roots)
                .map_err(|err| NamespaceError::Unresolvable {
                    path: path.to_string(),
                    detail: format!(
                        "{}/{}/{}: {}",
                        primary.tier,
                        primary.storage_key,
                        primary.object,
                        err.detail()
                    ),
                })?;

        Ok(Entry::File {
            path: path.to_string(),
            size: record.size,
            object: record.id,
            bytes,
        })
    }
}

/// Trim leading/trailing slashes so "" and "/" and "a/" and "/a" name the same path.
/// The mount's own paths never contain `.`/`..` components (the kernel resolves those
/// before issuing a request) and a `..` that did arrive would simply fail lookup.
fn normalize(path: &str) -> String {
    path.trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_slashes() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("/"), "");
        assert_eq!(normalize("a/b"), "a/b");
        assert_eq!(normalize("/a/b/"), "a/b");
    }
}

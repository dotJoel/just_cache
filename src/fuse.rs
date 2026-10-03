//! The FUSE namespace provider (`just_cache mount`, issue #42).
//!
//! This is the adapter between the kernel's FUSE requests and the provider-agnostic
//! [`crate::namespace`] layer: it turns an inode into a namespace path, asks the
//! namespace where the bytes are, and answers with the real file's bytes. The
//! interesting decisions are in [`crate::namespace`]; what is here is the mapping,
//! the open-file table, and the write/rename policy.
//!
//! # What is served
//!
//! The catalog's names. A name whose tier of record is the watch root (a *present*
//! file) is read straight from the tree; a name whose tier of record is a cold tier
//! (an *offloaded* object, whose name would otherwise be a symlink) is read from the
//! tier — the whole point of §4: a consumer that does not follow symlinks sees bytes.
//! Directory structure is the prefix tree of the names, overlaid with the real
//! directories already under the watch root so `mkdir`/`create` behave normally.
//!
//! # What a write does
//!
//! A write goes **through to the bytes at the tier of record**, in place. The mount
//! never invents a second placement mechanism: it does not copy into a partial, it
//! does not choose a tier, it does not write the journal. A new name is created under
//! the watch root (the hot tier), which is where a new file belongs and where the
//! mover would later pick it up. Writes and renames do **not** rewrite the catalog —
//! a rewrite that changed an object's bytes leaves its recorded checksum stale, and
//! the existing `catalog sync` observes and re-ingests that; the mount is not a
//! second writer of catalog rows. That is also why a read can never touch the
//! catalog: it is never a write path at all.
//!
//! # What is refused, and why that is the safe direction
//!
//! * A name whose bytes resolve but whose file is gone — an unmounted tier — answers
//!   `EIO`, never a zero-length file. A directory the catalog says holds files never
//!   silently reads as empty while the daemon is alive.
//! * `unlink`/`rmdir` answer `EROFS`: deletion is a catalog transition with
//!   reference counting (§3), and this issue deliberately does not implement it.
//! * A rename of a name whose bytes are on a cold tier answers `EROFS`. The mount
//!   cannot rewrite the catalog, so renaming the cold copy would leave the mount
//!   unable to see the name it just moved; refusing is the honest boundary (§9).
//! * `create` over a catalogued name answers `EIO` rather than shadowing it with a
//!   hot file the catalog does not know about.
//!
//! # Failing closed
//!
//! If the daemon dies the kernel marks the connection dead and every access returns
//! `ENOTCONN` — the mountpoint cannot read as an empty tree. Before mounting, the
//! mountpoint must exist, be a directory, and be empty; a catalog with no recorded
//! roots is refused rather than mounted as a namespace in which every row is
//! unresolvable. On SIGINT/SIGTERM the session is unmounted before the process exits.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, Session,
};
use rustix::fs::OFlags;

use crate::cache::Overlay;
use crate::catalog::{Catalog, CatalogError};
use crate::namespace::{Entry, Namespace, NamespaceError};
use crate::observe::AccessLog;

/// One second: names come from a catalog the mount does not watch, so a fresh lookup
/// is cheap and a cached one must not outlive a `catalog sync` by long.
const TTL: Duration = Duration::from_secs(1);

/// The mount options a namespace provider needs. `FSName`/`Subtype` name the mount
/// in `mount`/`/proc/mounts`; `NoDev`/`NoSuid` refuse device nodes and suid bits on a
/// filesystem whose content is other people's data.
fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("just_cache".to_string()),
        MountOption::Subtype("just_cache".to_string()),
        MountOption::RW,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
    ]
}

/// What `mount` needs to start serving.
#[derive(Debug, Clone)]
pub struct MountRequest {
    /// The catalog to serve. It must already exist: a mount never creates one
    /// (invariant 9), and one conjured empty would serve an empty tree.
    pub catalog_path: PathBuf,
    /// The watched tree, whose namespace the mount presents and under which new
    /// names are created (the hot tier).
    pub watch: PathBuf,
    /// The cold-tier roots the operator named, fastest first. The catalog, not this
    /// list, says where a given object lives; these are checked against the roots the
    /// catalog recorded so a mount cannot be pointed at a catalog that was synced from
    /// different disks.
    pub dests: Vec<PathBuf>,
    /// The directory to mount on. Must exist and be empty.
    pub mountpoint: PathBuf,
    /// `[[cache]]` overlays to serve reads through, each with the root of the tier it
    /// sits in front of (§2.1, issue #46). Empty means no overlay.
    pub caches: Vec<(crate::tiers::CacheConfig, PathBuf)>,
}

/// Why a mount could not be served.
#[derive(Debug)]
pub enum MountError {
    Catalog(CatalogError),
    Namespace(NamespaceError),
    /// The mountpoint is not an existing directory. It is never created — the same
    /// rule a destination root follows (invariant 1).
    MountpointNotADirectory(PathBuf),
    /// The mountpoint holds entries. Overmounting would hide them, so it is refused.
    MountpointNotEmpty(PathBuf),
    /// The catalog records no roots, so every location row would be unresolvable.
    NoRoots(PathBuf),
    /// A `--dest` the invocation names is not a root the catalog recorded. Serving
    /// from this catalog would resolve that disk's rows to nothing, so the mismatch is
    /// refused rather than mounted.
    UnknownTier(PathBuf),
    /// The FUSE session could not be established or run.
    Mount(io::Error),
    /// A cache overlay could not be opened (its path is missing, or its owned directory
    /// could not be reset). Refused rather than mounted without it, so a ramdisk that
    /// failed to mount is noticed instead of silently costing every read.
    Cache(crate::cache::CacheError),
    /// The signal handler that unmounts on Ctrl-C could not be installed; without it
    /// a SIGINT would leave a dead mount behind, so the mount is refused.
    Signal(ctrlc::Error),
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountError::Catalog(err) => write!(f, "the catalog could not be read: {err}"),
            MountError::Namespace(err) => write!(f, "the catalog namespace is unusable: {err}"),
            MountError::MountpointNotADirectory(path) => write!(
                f,
                "mountpoint {} is not an existing directory (create it first; just_cache \
                 will not create it)",
                path.display()
            ),
            MountError::MountpointNotEmpty(path) => write!(
                f,
                "mountpoint {} is not empty; refusing to hide the files already there",
                path.display()
            ),
            MountError::NoRoots(path) => write!(
                f,
                "the catalog {} records no roots, so no location row can be resolved; run \
                 `just_cache catalog sync` first",
                path.display()
            ),
            MountError::UnknownTier(path) => write!(
                f,
                "--dest {} is not a tier the catalog recorded; mount the catalog that was \
                 synced with this disk, or name it (run `just_cache catalog sync` first)",
                path.display()
            ),
            MountError::Mount(err) => write!(f, "the FUSE mount failed: {err}"),
            MountError::Cache(err) => write!(f, "{err}"),
            MountError::Signal(err) => write!(
                f,
                "could not install the unmount-on-signal handler: {err}; refusing to mount \
                 without a clean unmount path"
            ),
        }
    }
}

impl std::error::Error for MountError {}

/// Mount the catalog namespace at the request's mountpoint and serve it until the
/// filesystem is unmounted. Blocks.
pub fn serve(request: &MountRequest) -> Result<(), MountError> {
    let catalog = Catalog::open(&request.catalog_path).map_err(MountError::Catalog)?;
    let namespace = Namespace::new(&catalog).map_err(MountError::Namespace)?;
    // No roots means no row can be proven under a trusted root, so the mount would
    // serve nothing. Refuse up front rather than present an empty namespace.
    if namespace.roots().is_empty() {
        return Err(MountError::NoRoots(request.catalog_path.clone()));
    }
    // The roots come from the catalog, not from the invocation. A `--dest` the catalog
    // does not record means this catalog was synced from different disks, so serving
    // it would resolve that disk's rows to nothing — refuse rather than present a
    // namespace that quietly lost a tier.
    for dest in &request.dests {
        let canonical = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
        if !namespace.roots().iter().any(|root| root == &canonical) {
            return Err(MountError::UnknownTier(dest.clone()));
        }
    }

    let metadata = fs::symlink_metadata(&request.mountpoint)
        .map_err(|_| MountError::MountpointNotADirectory(request.mountpoint.clone()))?;
    if !metadata.is_dir() {
        return Err(MountError::MountpointNotADirectory(
            request.mountpoint.clone(),
        ));
    }
    if directory_non_empty(&request.mountpoint) {
        return Err(MountError::MountpointNotEmpty(request.mountpoint.clone()));
    }

    let mut overlays = Vec::with_capacity(request.caches.len());
    for (config, home_root) in &request.caches {
        let overlay = Overlay::open(config.clone(), home_root).map_err(MountError::Cache)?;
        // The overlay opens empty, so any residency rows a previous process left are
        // describing bytes that no longer exist. Losing them is the point: a cache is
        // never something the catalog has to account for.
        if let Err(err) = catalog.clear_cache_residency(overlay.name()) {
            return Err(MountError::Catalog(err));
        }
        overlays.push(overlay);
    }

    let mut filesystem = MountFs::new(catalog, namespace, request.watch.clone());
    filesystem.overlays = Mutex::new(overlays);
    let mut config = Config::default();
    config.mount_options = mount_options();

    let mut session =
        Session::new(filesystem, &request.mountpoint, &config).map_err(MountError::Mount)?;
    // A clean unmount has to survive Ctrl-C: fuser's pure-Rust path installs no
    // signal handler of its own, so without this the process would die with the
    // mount still live and a dead mountpoint behind.
    let mut unmounter = session.unmount_callable();
    ctrlc::set_handler(move || {
        if let Err(err) = unmounter.unmount() {
            eprintln!("just_cache: unmount failed: {err}");
        }
    })
    .map_err(MountError::Signal)?;

    session.run().map_err(MountError::Mount)
}

/// True when a directory has at least one entry. A read error is treated as "not
/// empty" so a permission oddity cannot be mistaken for a safe-to-overmount directory.
fn directory_non_empty(path: &Path) -> bool {
    match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => true,
    }
}

/// The FUSE filesystem. Every method takes `&self`, so mutable state is behind locks.
struct MountFs {
    /// The catalog, behind a mutex because `rusqlite::Connection` is `Send` but not
    /// `Sync`, and FUSE calls arrive from fuser's own threads.
    catalog: Mutex<Catalog>,
    /// The names and roots, indexed once at mount time.
    namespace: Namespace,
    /// The hot tier: the tree the namespace is rooted at, where new names are created.
    watch: PathBuf,
    /// Owner of the watch root, reported for directories the catalog implies but that
    /// have no directory on disk to stat.
    hot_owner: (u32, u32),
    /// Inode <-> path, allocated on demand.
    inodes: Mutex<Inodes>,
    /// Open file handles, keyed by the handle returned from `open`/`create`, with the
    /// namespace path each was opened as so a read or close can be attributed to it.
    handles: Mutex<HashMap<u64, (File, String)>>,
    /// Accesses observed but not yet written to `lifecycle` (issue #43). Lock order is
    /// `access_log` then `catalog`, never the reverse.
    access_log: Mutex<AccessLog>,
    next_handle: AtomicU64,
    /// Cache overlays (§2.1). Reads may be served from a copy here; writes always land on
    /// the home and drop the copy first (write-invalidate).
    overlays: Mutex<Vec<Overlay>>,
}

impl MountFs {
    fn new(catalog: Catalog, namespace: Namespace, watch: PathBuf) -> Self {
        let hot_owner = fs::metadata(&watch)
            .map(|md| (md.uid(), md.gid()))
            .unwrap_or((0, 0));
        Self {
            catalog: Mutex::new(catalog),
            namespace,
            watch,
            hot_owner,
            inodes: Mutex::new(Inodes::new()),
            handles: Mutex::new(HashMap::new()),
            access_log: Mutex::new(AccessLog::new()),
            next_handle: AtomicU64::new(1),
            overlays: Mutex::new(Vec::new()),
        }
    }

    fn path_of(&self, ino: INodeNo) -> Option<String> {
        self.inodes.lock().unwrap().path(u64::from(ino))
    }

    fn ino_for(&self, path: &str) -> u64 {
        self.inodes.lock().unwrap().inode(path)
    }

    fn add_handle(&self, file: File, path: &str) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.handles
            .lock()
            .unwrap()
            .insert(handle, (file, path.to_string()));
        handle
    }

    /// Record one provider-observed event and flush when the bound says to (see
    /// [`crate::observe`]). `opened` adds an access; a read or close only refreshes the
    /// stamp. A failed flush is reported and the batch kept: observation must never turn
    /// into a failed read for the consumer.
    fn observe(&self, path: &str, opened: bool, force_flush: bool) {
        let mut log = self.access_log.lock().unwrap();
        let now = SystemTime::now();
        if opened {
            log.opened(path, now);
        } else {
            log.touched(path, now);
        }
        if force_flush || log.due(Instant::now()) {
            self.flush_observations(&mut log);
        }
    }

    fn flush_observations(&self, log: &mut AccessLog) {
        let mut catalog = self.catalog.lock().unwrap();
        if let Err(err) = log.flush(&mut catalog) {
            eprintln!(
                "just_cache: could not record {} observed access(es) in the catalog: {err}; \
                 kept for the next flush",
                log.pending()
            );
        }
    }

    /// Join a parent namespace path and a child name.
    fn join(parent: &str, name: &str) -> String {
        if parent.is_empty() {
            name.to_string()
        } else {
            format!("{parent}/{name}")
        }
    }

    /// Resolve a namespace path into something servable, falling back to the real
    /// watch tree for entries the catalog does not name (freshly created files and
    /// directories).
    fn resolve(&self, path: &str) -> Result<Resolved, Errno> {
        let found = {
            let catalog = self.catalog.lock().unwrap();
            self.namespace.lookup(&catalog, path)
        };
        match found {
            Ok(Entry::Directory { children, .. }) => {
                Ok(Resolved::Dir(self.merged_children(path, &children)))
            }
            Ok(Entry::File { bytes, .. }) => {
                // The row resolved, so the name exists; the bytes must be there too.
                // A missing file is a failure to serve (an unmounted tier), reported
                // as EIO rather than as an absence or an empty file.
                let metadata = fs::metadata(&bytes).map_err(|_| Errno::EIO)?;
                if metadata.is_dir() {
                    return Err(Errno::EIO);
                }
                Ok(Resolved::File { bytes })
            }
            Err(NamespaceError::NotFound { .. }) => self.resolve_disk(path),
            // An unresolvable row is a failure to serve, not evidence of absence.
            Err(_) => Err(Errno::EIO),
        }
    }

    /// Resolve a path from the watch tree alone, for names the catalog does not hold.
    fn resolve_disk(&self, path: &str) -> Result<Resolved, Errno> {
        let disk = if path.is_empty() {
            self.watch.clone()
        } else {
            self.watch.join(path)
        };
        let metadata = fs::symlink_metadata(&disk).map_err(|_| Errno::ENOENT)?;
        if metadata.is_dir() {
            Ok(Resolved::Dir(self.merged_children(path, &[])))
        } else {
            Ok(Resolved::File { bytes: disk })
        }
    }

    /// The immediate children of a directory: catalogued names merged with the real
    /// entries under the watch root. A directory implied only by catalogued names still
    /// lists those names; a directory created with `mkdir` still lists its files.
    fn merged_children(
        &self,
        path: &str,
        catalog_children: &[String],
    ) -> Vec<(String, u64, FileType)> {
        let mut children: BTreeMap<String, FileType> = BTreeMap::new();
        for name in catalog_children {
            let child_path = Self::join(path, name);
            let kind = if self.namespace.names_file(&child_path) {
                FileType::RegularFile
            } else {
                FileType::Directory
            };
            children.insert(name.clone(), kind);
        }

        let disk = if path.is_empty() {
            self.watch.clone()
        } else {
            self.watch.join(path)
        };
        if let Ok(entries) = fs::read_dir(&disk) {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                children.entry(name).or_insert_with(|| {
                    entry
                        .file_type()
                        .map(|kind| FileType::from_std(kind).unwrap_or(FileType::RegularFile))
                        .unwrap_or(FileType::RegularFile)
                });
            }
        }

        children
            .into_iter()
            .map(|(name, kind)| {
                let child_path = Self::join(path, &name);
                (name, self.ino_for(&child_path), kind)
            })
            .collect()
    }

    /// Inode and attributes for a path, in one step. Used by `lookup`, `getattr` and
    /// the `create`/`mkdir` replies.
    fn lookup_path(&self, path: &str) -> Result<(INodeNo, FileAttr), Errno> {
        let ino = self.ino_for(path);
        match self.resolve(path)? {
            Resolved::Dir(_) => Ok((INodeNo(ino), self.dir_attr(ino, path))),
            Resolved::File { bytes, .. } => {
                let metadata = fs::metadata(&bytes).map_err(|_| Errno::EIO)?;
                Ok((INodeNo(ino), attr_from_metadata(ino, &metadata)))
            }
        }
    }

    /// Attributes for a directory, from disk when it exists there and synthesized from
    /// the watch root's owner when it is only implied by catalogued names.
    fn dir_attr(&self, ino: u64, path: &str) -> FileAttr {
        let disk = if path.is_empty() {
            self.watch.clone()
        } else {
            self.watch.join(path)
        };
        match fs::symlink_metadata(&disk) {
            Ok(metadata) if metadata.is_dir() => attr_from_metadata(ino, &metadata),
            _ => FileAttr {
                ino: INodeNo(ino),
                size: 0,
                blocks: 0,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: self.hot_owner.0,
                gid: self.hot_owner.1,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            },
        }
    }

    /// Where a read-only open of `path` (home bytes at `bytes`) should read from: an
    /// overlay copy when one is resident or this read earns a promotion, else the home.
    /// Residency changes are mirrored into the catalog's `cache_residency` table for
    /// observability; a failure to mirror is ignored, because that table is never read as
    /// data of record and the read itself is unaffected.
    fn read_source(&self, path: &str, bytes: PathBuf) -> PathBuf {
        let mut overlays = self.overlays.lock().unwrap();
        if overlays.is_empty() {
            return bytes;
        }
        let object = {
            let catalog = self.catalog.lock().unwrap();
            match self.namespace.lookup(&catalog, path) {
                Ok(Entry::File { object, .. }) => object,
                // Only catalogued names are cached: a fresh hot file has no object id to
                // report residency against, and is already on the fastest tier anyway.
                _ => return bytes,
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for overlay in overlays.iter_mut() {
            if !overlay.covers(&bytes) {
                continue;
            }
            let outcome = overlay.read(path, &object, &bytes, now);
            let catalog = self.catalog.lock().unwrap();
            for dropped in &outcome.dropped {
                let _ = catalog.drop_cache_residency(overlay.name(), dropped);
            }
            if outcome.promoted {
                let _ = catalog.record_cache_residency(overlay.name(), path, &object, now as i64);
            }
            if let Some(serve) = outcome.serve {
                return serve;
            }
        }
        bytes
    }

    /// Drop every overlay copy of `path` before its home is written. Called before the
    /// write so a reader racing the write can never be handed the old copy afterwards.
    fn invalidate(&self, path: &str) {
        let mut overlays = self.overlays.lock().unwrap();
        for overlay in overlays.iter_mut() {
            if overlay.invalidate(path) {
                let catalog = self.catalog.lock().unwrap();
                let _ = catalog.drop_cache_residency(overlay.name(), path);
            }
        }
    }

    /// The bytes path for a file, or the reason it cannot be served.
    fn file_bytes(&self, path: &str) -> Result<PathBuf, Errno> {
        match self.resolve(path)? {
            Resolved::File { bytes, .. } => Ok(bytes),
            Resolved::Dir(_) => Err(Errno::EISDIR),
        }
    }
}

enum Resolved {
    Dir(Vec<(String, u64, FileType)>),
    File { bytes: PathBuf },
}

/// Inode allocation. The root is inode 1, as the FUSE protocol requires.
struct Inodes {
    next: u64,
    by_path: HashMap<String, u64>,
    by_ino: HashMap<u64, String>,
}

impl Inodes {
    fn new() -> Self {
        let mut by_path = HashMap::new();
        let mut by_ino = HashMap::new();
        by_path.insert(String::new(), 1);
        by_ino.insert(1, String::new());
        Self {
            next: 2,
            by_path,
            by_ino,
        }
    }

    fn path(&self, ino: u64) -> Option<String> {
        self.by_ino.get(&ino).cloned()
    }

    fn inode(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert(path.to_string(), ino);
        self.by_ino.insert(ino, path.to_string());
        ino
    }
}

/// Build a `FileAttr` from a real file's metadata, so mode, owner and times shown by
/// the mount are the bytes' own — not an invented set.
fn attr_from_metadata(ino: u64, metadata: &fs::Metadata) -> FileAttr {
    let kind = if metadata.is_dir() {
        FileType::Directory
    } else if metadata.file_type().is_symlink() {
        FileType::Symlink
    } else {
        FileType::RegularFile
    };
    FileAttr {
        ino: INodeNo(ino),
        size: metadata.len(),
        blocks: metadata.blocks(),
        atime: metadata.accessed().unwrap_or(UNIX_EPOCH),
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
        ctime: seconds_to_time(metadata.ctime()),
        crtime: UNIX_EPOCH,
        kind,
        perm: (metadata.mode() & 0o7777) as u16,
        nlink: metadata.nlink().min(u32::MAX as u64) as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn seconds_to_time(seconds: i64) -> SystemTime {
    if seconds <= 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds as u64))
            .unwrap_or(UNIX_EPOCH)
    }
}

impl Filesystem for MountFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.path_of(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        let path = Self::join(&parent_path, name);
        match self.lookup_path(&path) {
            Ok((_, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.lookup_path(&path) {
            Ok((_, attr)) => reply.attr(&TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let children = match self.resolve(&path) {
            Ok(Resolved::Dir(children)) => children,
            Ok(Resolved::File { .. }) => {
                reply.error(Errno::ENOTDIR);
                return;
            }
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };

        // "." and ".." as the kernel expects; ".." of the root is the root.
        let parent_path = path
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default();
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (u64::from(ino), FileType::Directory, ".".to_string()),
            (
                self.ino_for(&parent_path),
                FileType::Directory,
                "..".to_string(),
            ),
        ];
        entries.extend(
            children
                .into_iter()
                .map(|(name, child_ino, kind)| (child_ino, kind, name)),
        );

        for (index, (child_ino, kind, name)) in
            entries.into_iter().enumerate().skip(offset as usize)
        {
            // The offset of an entry is the index of the *next* one, which is how the
            // kernel resumes a readdir that did not fit in one buffer.
            if reply.add(INodeNo(child_ino), (index + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let bytes = match self.file_bytes(&path) {
            Ok(bytes) => bytes,
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };
        let mut options = OpenOptions::new();
        let bytes = match flags.acc_mode() {
            OpenAccMode::O_RDONLY => {
                options.read(true);
                self.read_source(&path, bytes)
            }
            OpenAccMode::O_WRONLY => {
                options.write(true);
                self.invalidate(&path);
                bytes
            }
            OpenAccMode::O_RDWR => {
                options.read(true).write(true);
                self.invalidate(&path);
                bytes
            }
        };
        match options.open(&bytes) {
            Ok(file) => {
                let handle = self.add_handle(file, &path);
                self.observe(&path, true, false);
                reply.opened(FileHandle(handle), FopenFlags::empty());
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let (result, path) = {
            let handles = self.handles.lock().unwrap();
            let Some((file, path)) = handles.get(&u64::from(fh)) else {
                reply.error(Errno::EBADF);
                return;
            };
            let mut buffer = vec![0u8; size as usize];
            let result = file.read_at(&mut buffer, offset).map(|read| {
                buffer.truncate(read);
                buffer
            });
            (result, path.clone())
        };
        match result {
            Ok(buffer) => {
                self.observe(&path, false, false);
                reply.data(&buffer);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let handles = self.handles.lock().unwrap();
        let Some((file, _)) = handles.get(&u64::from(fh)) else {
            reply.error(Errno::EBADF);
            return;
        };
        // The write itself adds nothing to `lifecycle` (its open was already counted as a
        // use) and is never a transition. Any cache copy was already dropped when the
        // handle was opened for writing (write-invalidate, §2.1), so this call has nothing
        // further to invalidate.
        match file.write_at(data, offset) {
            Ok(written) => reply.written(written as u32),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        // Writes go straight to the tier of record's file, so there is nothing to
        // flush; answering ENOSYS (the default) would make every `close()` look like
        // a failure.
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let handles = self.handles.lock().unwrap();
        let Some((file, _)) = handles.get(&u64::from(fh)) else {
            reply.error(Errno::EBADF);
            return;
        };
        match file.sync_all() {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let released = self.handles.lock().unwrap().remove(&u64::from(fh));
        // A close completes the access, so it is flushed now: a daemon killed after this
        // point loses nothing about this open (the bound in `crate::observe`).
        if let Some((_, path)) = released {
            self.observe(&path, false, true);
        }
        reply.ok();
    }

    fn destroy(&mut self) {
        // Unmount: whatever is still buffered (files open at unmount) is written now.
        let log = self.access_log.get_mut().unwrap();
        let mut catalog = self.catalog.lock().unwrap();
        if let Err(err) = log.flush(&mut catalog) {
            eprintln!("just_cache: observed accesses lost at unmount: {err}");
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_path) = self.path_of(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = Self::join(&parent_path, name);
        // A catalogued name is served from its tier of record; creating a hot file at
        // that path would shadow the object the catalog vouches for with bytes the
        // catalog has never seen. Refuse rather than diverge.
        if self.namespace.names_file(&path) {
            reply.error(Errno::EIO);
            return;
        }
        let target = self.watch.join(&path);
        if flags & OFlags::EXCL.bits() as i32 != 0 && target.exists() {
            reply.error(Errno::EEXIST);
            return;
        }
        let opened = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(mode & !umask)
            .open(&target);
        match opened {
            Ok(file) => {
                let ino = self.ino_for(&path);
                let attr = match file.metadata() {
                    Ok(metadata) => attr_from_metadata(ino, &metadata),
                    Err(_) => {
                        reply.error(Errno::EIO);
                        return;
                    }
                };
                let handle = self.add_handle(file, &path);
                reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(handle),
                    FopenFlags::empty(),
                );
            }
            Err(err) => reply.error(errno_from_io(&err)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_path) = self.path_of(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = Self::join(&parent_path, name);
        if self.namespace.names_file(&path) {
            reply.error(Errno::EEXIST);
            return;
        }
        let target = self.watch.join(&path);
        match fs::create_dir(&target) {
            Ok(()) => {
                let _ = fs::set_permissions(&target, fs::Permissions::from_mode(mode & !umask));
                let ino = self.ino_for(&path);
                reply.entry(&TTL, &self.dir_attr(ino, &path), Generation(0));
            }
            Err(err) => reply.error(errno_from_io(&err)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        // Changing ownership needs a privilege the mount does not have, and silently
        // ignoring it would report success for a change that did not happen.
        if uid.is_some() || gid.is_some() {
            reply.error(Errno::EPERM);
            return;
        }

        if let Some(size) = size {
            self.invalidate(&path);
            let bytes = match self.file_bytes(&path) {
                Ok(bytes) => bytes,
                Err(errno) => {
                    reply.error(errno);
                    return;
                }
            };
            let truncated = OpenOptions::new()
                .write(true)
                .open(&bytes)
                .and_then(|file| file.set_len(size));
            if let Err(err) = truncated {
                reply.error(errno_from_io(&err));
                return;
            }
        }
        if let Some(mode) = mode {
            let disk = if self.namespace.names_file(&path) {
                self.file_bytes(&path).ok()
            } else {
                Some(self.watch.join(&path))
            };
            if let Some(disk) = disk {
                if let Err(err) = fs::set_permissions(&disk, fs::Permissions::from_mode(mode)) {
                    reply.error(errno_from_io(&err));
                    return;
                }
            }
        }

        match self.lookup_path(&path) {
            Ok((_, attr)) => reply.attr(&TTL, &attr),
            Err(errno) => reply.error(errno),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let Some(parent_path) = self.path_of(parent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(newparent_path) = self.path_of(newparent) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let source = Self::join(&parent_path, name);
        let destination = Self::join(&newparent_path, newname);
        // A rename changes which bytes both names answer to; neither copy is coherent.
        self.invalidate(&source);
        self.invalidate(&destination);

        let source_disk = self.watch.join(&source);
        if let Ok(metadata) = fs::symlink_metadata(&source_disk) {
            if metadata.is_dir() {
                let destination_disk = self.watch.join(&destination);
                if flags.contains(RenameFlags::RENAME_NOREPLACE) && destination_disk.exists() {
                    reply.error(Errno::EEXIST);
                    return;
                }
                return match fs::rename(&source_disk, &destination_disk) {
                    Ok(()) => reply.ok(),
                    Err(err) => reply.error(errno_from_io(&err)),
                };
            }
        }

        let source_bytes = match self.file_bytes(&source) {
            Ok(bytes) => bytes,
            // A directory the catalog implies but that has no directory on disk cannot
            // be renamed: there is no bytes tree to move and no catalog row to update.
            Err(Errno::EISDIR) => {
                reply.error(Errno::EROFS);
                return;
            }
            Err(errno) => {
                reply.error(errno);
                return;
            }
        };
        // The mount does not rewrite the catalog, so it only renames names whose bytes
        // it can see reflected afterwards: those under the watch root. Renaming a cold
        // copy would leave a name the mount cannot serve. Refuse, do not half-move.
        if !source_bytes.starts_with(&self.watch) {
            reply.error(Errno::EROFS);
            return;
        }
        let destination_disk = self.watch.join(&destination);
        if flags.contains(RenameFlags::RENAME_NOREPLACE) && destination_disk.exists() {
            reply.error(Errno::EEXIST);
            return;
        }
        match fs::rename(&source_bytes, &destination_disk) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_from_io(&err)),
        }
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        // Deletion is a catalog transition with reference counting across names and
        // copies (§3); this provider does not implement it, and EROFS says so without
        // pretending the mount is read-only.
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        // The default ACL (mount owner only) already bounds who can reach the mount;
        // re-checking the recorded file's mode here would deny the owner of the mount
        // access to a cold copy owned by another user.
        reply.ok();
    }
}

/// Map an `io::Error` to the closest FUSE errno, so `EEXIST`/`ENOENT` from the
/// filesystem survive instead of every failure becoming `EIO`.
fn errno_from_io(err: &io::Error) -> Errno {
    match err.kind() {
        io::ErrorKind::NotFound => Errno::ENOENT,
        io::ErrorKind::AlreadyExists => Errno::EEXIST,
        io::ErrorKind::PermissionDenied => Errno::EACCES,
        io::ErrorKind::NotADirectory => Errno::ENOTDIR,
        io::ErrorKind::IsADirectory => Errno::EISDIR,
        io::ErrorKind::DirectoryNotEmpty => Errno::ENOTEMPTY,
        io::ErrorKind::InvalidInput => Errno::EINVAL,
        _ => Errno::EIO,
    }
}

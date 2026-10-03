//! Guards against moving a file that something is still using.
//!
//! Two failures are cheap to prevent and expensive to discover later: moving a file out
//! from under a process that has it open, and splitting a hardlinked pair across two
//! filesystems — where the link is not merely broken but impossible.
//!
//! The open-file check is deliberately conservative and honest about its own limits: it
//! can only see the descriptors of processes this user is allowed to inspect, so a
//! non-root run sees its own processes and not much else. Callers are told that via
//! [`OpenFiles::coverage`] rather than being quietly reassured.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::disk_management::FileEntry;

/// Identity of a file on disk, independent of its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub device: u64,
    pub inode: u64,
}

impl FileId {
    /// Identity of whatever `path` resolves to (following symlinks).
    #[cfg(unix)]
    pub fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(path).ok()?;
        Some(FileId {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(not(unix))]
    pub fn of(_path: &Path) -> Option<Self> {
        None
    }

    /// Identity of a walked entry, skipping the syscall when the platform cannot tell.
    pub fn of_entry(entry: &FileEntry) -> Option<Self> {
        Self::of(&entry.path)
    }
}

/// How much of the process table a snapshot could actually see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Coverage {
    /// Every descriptor on the machine was inspected (running as root).
    Complete,
    /// Some processes could not be inspected: the check sees this user's own files only.
    OwnProcessesOnly,
    /// Open files cannot be enumerated on this platform at all.
    ///
    /// Also the default: a hand-built [`OpenFiles`] has inspected nothing, so it must not
    /// claim the guard is working.
    #[default]
    Unsupported,
}

/// The set of files open right now, as of one sweep.
#[derive(Debug, Default)]
pub struct OpenFiles {
    ids: HashSet<FileId>,
    /// Which processes hold each open file, by pid. Only populated from the `/proc` scan;
    /// kept beside `ids` because the mover only asks "is it open?", while `explain` has to
    /// answer "by whom", and a second process-table scan just to name the pid would cost
    /// more than the whole command.
    holders: HashMap<FileId, Vec<u32>>,
    inspected: usize,
    /// Processes whose descriptor table could not be read, so their open files are
    /// invisible to us and the guard is weaker than it looks.
    unreadable: usize,
    coverage: Coverage,
}

impl OpenFiles {
    /// Take a snapshot of every open file this user can see.
    ///
    /// One scan, taken when a sweep starts, and used as a cheap prefilter when candidates
    /// are chosen: it keeps the walk from selecting a file that was already open without
    /// paying a process-table scan for every file in the tree. It is not the final word —
    /// a descriptor opened after it is taken is invisible to it, which is why the mover
    /// re-scans through [`Guards::recheck`] immediately before the bytes move.
    #[cfg(target_os = "linux")]
    pub fn snapshot() -> Self {
        let own_pid = std::process::id();
        let mut ids = HashSet::new();
        let mut holders: HashMap<FileId, Vec<u32>> = HashMap::new();
        let mut inspected = 0usize;
        let mut unreadable = 0usize;

        let processes = match fs::read_dir("/proc") {
            Ok(entries) => entries,
            Err(_) => return Self::unsupported(),
        };

        for process in processes.flatten() {
            let name = process.file_name();
            let name = name.to_string_lossy();
            let pid = match name.parse::<u32>() {
                Ok(pid) => pid,
                Err(_) => continue,
            };
            if pid == own_pid {
                // Our own descriptors are not a reason to leave a file alone: the sweep
                // opens files for its own checks and would otherwise pin everything.
                continue;
            }

            let fds = match fs::read_dir(process.path().join("fd")) {
                Ok(fds) => fds,
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
            };
            inspected += 1;
            for fd in fds.flatten() {
                // /proc/<pid>/fd/<n> is a symlink to the open file; metadata follows it.
                if let Some(id) = FileId::of(&fd.path()) {
                    ids.insert(id);
                    let holders = holders.entry(id).or_default();
                    if !holders.contains(&pid) {
                        holders.push(pid);
                    }
                }
            }
        }

        let coverage = if unreadable == 0 {
            Coverage::Complete
        } else {
            Coverage::OwnProcessesOnly
        };
        Self {
            ids,
            holders,
            inspected,
            unreadable,
            coverage,
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn snapshot() -> Self {
        Self::unsupported()
    }

    fn unsupported() -> Self {
        Self {
            ids: HashSet::new(),
            holders: HashMap::new(),
            inspected: 0,
            unreadable: 0,
            coverage: Coverage::Unsupported,
        }
    }

    pub fn contains(&self, id: FileId) -> bool {
        self.ids.contains(&id)
    }

    /// Pids this snapshot saw holding `id` open, oldest scan order first. Empty when the
    /// file is not open, or when it is open in a process the scan could not inspect — the
    /// two are not distinguished here, which is why [`OpenFiles::coverage`] is reported
    /// alongside.
    pub fn holder_pids(&self, id: FileId) -> &[u32] {
        self.holders.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Test-only: record that `id` is held open, without a `/proc` scan. The pids the
    /// scan finds are what the real code names; this lets a test pin the naming path
    /// deterministically instead of racing a child process.
    #[cfg(test)]
    pub(crate) fn hold_for_test(&mut self, id: FileId, pid: Option<u32>) {
        self.ids.insert(id);
        if let Some(pid) = pid {
            let holders = self.holders.entry(id).or_default();
            if !holders.contains(&pid) {
                holders.push(pid);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Processes whose descriptor table was readable.
    pub fn processes_seen(&self) -> usize {
        self.inspected
    }

    /// Processes we could not look into at all (other users' processes, without root).
    pub fn processes_unreadable(&self) -> usize {
        self.unreadable
    }

    pub fn coverage(&self) -> Coverage {
        self.coverage
    }

    /// A line worth printing when the check is weaker than it looks.
    pub fn coverage_note(&self) -> Option<String> {
        match self.coverage {
            Coverage::Complete => None,
            Coverage::OwnProcessesOnly => Some(format!(
                "open-file check is partial: {} of {} process(es) could not be inspected without \
                 privileges, so only this user's open files were considered",
                self.unreadable,
                self.inspected + self.unreadable
            )),
            Coverage::Unsupported => Some(
                "open-file check is unavailable on this platform; files in use may be moved"
                    .to_string(),
            ),
        }
    }
}

/// Why a file must not be moved, even though it looks cold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InUse {
    /// A process has the file open.
    OpenElsewhere,
    /// The file has more than one link, so moving it would break the pair.
    Hardlinked { links: u64 },
}

impl std::fmt::Display for InUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InUse::OpenElsewhere => write!(f, "open by another process"),
            InUse::Hardlinked { links } => write!(f, "hardlinked elsewhere ({links} links)"),
        }
    }
}

/// The in-use checks for one sweep.
#[derive(Debug)]
pub struct Guards {
    open_files: OpenFiles,
    check_hardlinks: bool,
}

impl Guards {
    pub fn new(open_files: OpenFiles, check_hardlinks: bool) -> Self {
        Self {
            open_files,
            check_hardlinks,
        }
    }

    /// No checks at all. For tests that are not about usage.
    pub fn permissive() -> Self {
        Self {
            open_files: OpenFiles::unsupported(),
            check_hardlinks: false,
        }
    }

    pub fn open_files(&self) -> &OpenFiles {
        &self.open_files
    }

    /// Whether more-than-one-link files are refused. `explain` reports the rule that is in
    /// force, not just whether it fired.
    pub fn hardlink_check(&self) -> bool {
        self.check_hardlinks
    }

    /// Is this file safe to move, given the sweep's snapshot of what is using it?
    ///
    /// The open half consults the snapshot taken when the sweep began, so this is a cheap
    /// prefilter for candidate selection — **not** the check that guards the move. A
    /// descriptor opened after the snapshot is invisible here; the mover calls
    /// [`Guards::recheck`] immediately before the bytes move to catch that window.
    pub fn check(&self, entry: &FileEntry) -> Result<(), InUse> {
        if let Some(id) = FileId::of_entry(entry) {
            if self.open_files.contains(id) {
                return Err(InUse::OpenElsewhere);
            }
        }
        if self.check_hardlinks {
            if let Some(links) = link_count(&entry.path) {
                if links > 1 {
                    return Err(InUse::Hardlinked { links });
                }
            }
        }
        Ok(())
    }

    /// Re-check a file immediately before its bytes would move, against the process table
    /// *as it is now* rather than the snapshot taken when the sweep started.
    ///
    /// The gap this closes is real: the sweep's snapshot can only see descriptors that
    /// already existed when it was taken, so a file opened during the walk would be moved
    /// out from under its writer (invariant 3). A fresh scan per candidate costs a
    /// process-table walk, but only for the files a sweep is actually about to move — not
    /// for every file in the tree — and the hardlink half is re-stat'ed live in the same
    /// step. The scan and the rename/remove are still two steps, so a descriptor opened in
    /// between is missed; that residual window is inherent to any userspace check and is
    /// named in `docs/design.md` §9 rather than implied away.
    pub fn recheck(&self, entry: &FileEntry) -> Result<(), InUse> {
        if let Some(id) = FileId::of_entry(entry) {
            let live = OpenFiles::snapshot();
            // An unsupported platform has no process table to consult; the sweep-wide
            // snapshot said so already, and a second scan cannot add what it could not see.
            if live.coverage() != Coverage::Unsupported && live.contains(id) {
                return Err(InUse::OpenElsewhere);
            }
        }
        if self.check_hardlinks {
            if let Some(links) = link_count(&entry.path) {
                if links > 1 {
                    return Err(InUse::Hardlinked { links });
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
pub fn link_count(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|metadata| metadata.nlink())
}

#[cfg(not(unix))]
pub fn link_count(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn entry(path: &Path) -> FileEntry {
        FileEntry {
            path: path.to_path_buf(),
            relative: PathBuf::from(path.file_name().unwrap_or_default()),
            size: 1,
            allocated: 1,
            last_access: SystemTime::now(),
            is_symlink: false,
        }
    }

    #[test]
    fn a_plain_file_is_not_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.bin");
        fs::write(&path, b"data").unwrap();
        let guards = Guards::new(OpenFiles::default(), true);
        assert_eq!(guards.check(&entry(&path)), Ok(()));
    }

    #[test]
    fn a_file_open_in_this_process_is_detected_by_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("held.bin");
        fs::write(&path, b"data").unwrap();

        let id = FileId::of(&path).expect("unix gives dev/inode");
        let mut open = OpenFiles::default();
        open.ids.insert(id);
        let guards = Guards::new(open, false);

        assert_eq!(guards.check(&entry(&path)), Err(InUse::OpenElsewhere));
    }

    /// The sweep snapshot cannot see a descriptor opened after it was taken; the mover's
    /// pre-move re-check must, because that mid-sweep window is exactly what it exists for.
    /// Reverting `recheck` to consult the stale snapshot makes this fail.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_descriptor_opened_after_the_snapshot_is_caught_by_the_recheck() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("opens-late.bin");
        fs::write(&path, b"data").unwrap();

        // The snapshot a sweep would take, while nothing holds the file.
        let guards = Guards::new(OpenFiles::snapshot(), true);
        assert_eq!(
            guards.check(&entry(&path)),
            Ok(()),
            "a file nobody holds yet is not in use at selection time"
        );

        // A real descriptor opens, after that snapshot, held by another process.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exec 3< '{}'; sleep 20", path.display()))
            .spawn()
            .expect("spawn a holder process");
        let target = FileId::of(&path).unwrap();
        let mut seen = false;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if OpenFiles::snapshot().contains(target) {
                seen = true;
                break;
            }
        }

        // Re-check while the holder is still alive: killing it first would close the
        // descriptor and make the test pass for the wrong reason.
        let rechecked = guards.recheck(&entry(&path));
        let _ = child.kill();
        let _ = child.wait();

        assert!(seen, "the holder must be visible to a fresh scan");
        assert_eq!(
            rechecked,
            Err(InUse::OpenElsewhere),
            "the pre-move re-check must see a descriptor opened after the sweep snapshot"
        );
    }

    #[test]
    fn a_hardlinked_file_is_refused_unless_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("original.bin");
        let link = tmp.path().join("link.bin");
        fs::write(&original, b"data").unwrap();
        fs::hard_link(&original, &link).unwrap();

        let refusing = Guards::new(OpenFiles::default(), true);
        assert_eq!(
            refusing.check(&entry(&original)),
            Err(InUse::Hardlinked { links: 2 })
        );

        let allowing = Guards::new(OpenFiles::default(), false);
        assert_eq!(allowing.check(&entry(&original)), Ok(()));
    }

    /// The /proc scan is the part that can silently do nothing, so it is exercised for
    /// real: a child process holds the file open while we take the snapshot.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_held_open_by_another_process_is_found_by_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("held-by-child.bin");
        fs::write(&path, b"data").unwrap();

        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exec 3< '{}'; sleep 20", path.display()))
            .spawn()
            .expect("spawn a holder process");

        // Wait for the child to actually reach the open() before snapshotting.
        let target = FileId::of(&path).unwrap();
        let mut seen = false;
        let mut named = Vec::new();
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let snapshot = OpenFiles::snapshot();
            if snapshot.contains(target) {
                seen = true;
                named = snapshot.holder_pids(target).to_vec();
                break;
            }
        }
        let child_pid = child.id();
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            seen,
            "a file held open by a child process must appear in the snapshot"
        );
        assert!(
            named.contains(&child_pid),
            "the snapshot must name the pid holding the file ({child_pid} not in {named:?})"
        );
    }

    #[test]
    fn coverage_is_reported_honestly() {
        let snapshot = OpenFiles::snapshot();
        match snapshot.coverage() {
            Coverage::Complete => assert!(snapshot.coverage_note().is_none()),
            Coverage::OwnProcessesOnly => {
                let note = snapshot
                    .coverage_note()
                    .expect("partial coverage is disclosed");
                // The count must be unreadable *processes*, not open files — a note that
                // conflates them misleads exactly when the guard is weakest.
                assert!(
                    note.contains(&snapshot.processes_unreadable().to_string()),
                    "note should name how many processes were unreadable: {note}"
                );
                assert!(snapshot.processes_unreadable() > 0);
            }
            Coverage::Unsupported => assert!(snapshot.coverage_note().is_some()),
        }
    }
}

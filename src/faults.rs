//! # Test-only fault injection: `JUST_CACHE_FAULT`
//!
//! The windows the durability rule exists for — a destination that becomes unavailable
//! *between* the copies of one sweep, during the read-back of a copy that was just
//! written, or **while bytes are still being written into a partial file** — cannot be
//! timed from a test process that drives the binary: no arrangement of the filesystem
//! before `sweep` starts removes a disk in the middle of the loop, and a polling test
//! would be a race, not a proof. This hook is the deterministic seam for exactly those
//! windows, and nothing else.
//!
//! It is **off by default and inert in production**: it fires only when `JUST_CACHE_FAULT`
//! is set in the environment of the process, which no production invocation sets, and an
//! unset variable is read once per operation and changes no code path. It is not
//! `#[cfg(test)]` because integration tests drive the *binary*, which is compiled without
//! test cfg — that is the whole point: the failure must reach the real binary.
//!
//! The value is `mechanism=N`, where `N` means what the mechanism needs it to mean:
//!
//! * `unavailable-after=N` — once `N` copies have verified, every further destination
//!   root is reported as `Unavailable`, exactly as a root whose mount went away looks to
//!   `replication::replicate`'s own `is_dir` check. This lands the fault *between* copy
//!   `N` and copy `N+1`, which is where the source-kept rule earns its keep.
//! * `vanish-readback=N` — the `N`th freshly written copy is removed just before its
//!   read-back hash, so verification fails on bytes that were there a moment ago: the
//!   state "the copy was written but I cannot prove it" (#20), not a verified copy.
//! * `corrupt-readback=N` — one byte of the `N`th fresh copy is flipped before the
//!   read-back, so verification fails on a same-length stranger of our own making.
//! * `unlink-mid-copy=N` — during the **first** copy the process makes, once `N` bytes
//!   have been written into the private `.just_cache-partial-*` file, the partial is
//!   unlinked and the nested directory the copy created is removed. This is the destination
//!   going away *while the copy is in flight*, and it fires at most once per process: one
//!   destination vanished, so exactly one copy is torn out and every later destination is
//!   an ordinary one. `N` is a byte count here, not a copy ordinal, which is why it cannot
//!   also name the copy — the first is the only one it can touch.
//! * `partial-mode-mid-copy=N` — during the **first** copy, once at least `N` bytes have
//!   been written into the private `.just_cache-partial-*` file, its mode is read back and
//!   reported. The file is only briefly in this state — it is renamed into place at the end
//!   of the copy — and the process doing the copy is the only one that can observe it while
//!   the bytes are still moving, so the seam exists for a test to prove the in-flight
//!   partial is `0600` rather than the process umask's guess. Like `unlink-mid-copy`, `N` is
//!   a byte count and the seam fires at most once per process.
//! * `replace-verified-dest=N` — on the **first** destination whose existing bytes verify
//!   as an identical copy of the source, and *after* that verification but *before* the
//!   source is removed, the destination is unlinked and replaced by a different file.
//!   This is the window in which removing the source would delete the last verified copy
//!   because the name no longer resolves to what was read. `N` is unused (the seam fires
//!   once); it exists because the value grammar is uniform.
//!
//! A *set but unparseable* value panics rather than being ignored: a fault switch that
//! silently no-ops would let a mistyped test pass green with no fault injected, which is
//! precisely the fake coverage AGENTS.md forbids. Panic-on-garbage is safe in production
//! because the variable is never set there (the same trade `JUST_CACHE_REQUIRE_SECOND_FS`
//! makes for skipped tests).

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultMode {
    DestinationUnavailableAfter,
    VanishReadback,
    CorruptReadback,
    UnlinkMidCopy,
    PartialModeMidCopy,
    ReplaceVerifiedDest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fault {
    pub(crate) mode: FaultMode,
    /// The mechanism's `N`. A 1-based copy ordinal for the three replication mechanisms,
    /// and a byte position for `unlink-mid-copy`.
    pub(crate) at: usize,
}

impl Fault {
    /// Parse `JUST_CACHE_FAULT`, or `None` when it is unset — the production case.
    pub(crate) fn from_env() -> Option<Fault> {
        let spec = env::var("JUST_CACHE_FAULT").ok()?;
        let (mechanism, ordinal) = spec
            .split_once('=')
            .unwrap_or_else(|| panic!("JUST_CACHE_FAULT `{spec}`: expected `mechanism=N`"));
        let mode = match mechanism {
            "unavailable-after" => FaultMode::DestinationUnavailableAfter,
            "vanish-readback" => FaultMode::VanishReadback,
            "corrupt-readback" => FaultMode::CorruptReadback,
            "unlink-mid-copy" => FaultMode::UnlinkMidCopy,
            "partial-mode-mid-copy" => FaultMode::PartialModeMidCopy,
            "replace-verified-dest" => FaultMode::ReplaceVerifiedDest,
            other => panic!("JUST_CACHE_FAULT `{other}`: unknown mechanism"),
        };
        let at = ordinal
            .parse()
            .unwrap_or_else(|_| panic!("JUST_CACHE_FAULT `{spec}`: N must be an integer >= 1"));
        assert!(at >= 1, "JUST_CACHE_FAULT `{spec}`: N is 1-based, so >= 1");
        Some(Fault { mode, at })
    }
}

/// Disarms after the one copy `unlink-mid-copy` is allowed to tear out.
///
/// Per process, not per call: the fault models *a* destination going away, and the
/// invariant under test depends on the sweep still trying the next destination and
/// succeeding there. Without this, every copy would fault and there would be no verified
/// sibling to compare the wreckage against. It is only ever consulted when the fault is
/// set, so a production process pays nothing for it.
static MID_COPY_DISARMED: AtomicBool = AtomicBool::new(false);

/// The first caller wins the single `unlink-mid-copy` fault; every later copy is spared.
pub(crate) fn claim_unlink_mid_copy() -> bool {
    !MID_COPY_DISARMED.swap(true, Ordering::SeqCst)
}

/// Disarms after the one `partial-mode-mid-copy` observation. Per process for the same
/// reason as `unlink-mid-copy`: it witnesses *a* copy, and the sweep must still be able to
/// process every later file normally.
static PARTIAL_MODE_DISARMED: AtomicBool = AtomicBool::new(false);

/// The first caller wins the single `partial-mode-mid-copy` observation.
pub(crate) fn claim_partial_mode_mid_copy() -> bool {
    !PARTIAL_MODE_DISARMED.swap(true, Ordering::SeqCst)
}

/// Disarms after the one `replace-verified-dest` seam has fired. Like `unlink-mid-copy`,
/// per process: it models *a* destination being swapped, and the sweep must still be able
/// to process the next file normally.
static REPLACE_DEST_DISARMED: AtomicBool = AtomicBool::new(false);

/// The first caller wins the single `replace-verified-dest` fault; later moves are spared.
pub(crate) fn claim_replace_verified_dest() -> bool {
    !REPLACE_DEST_DISARMED.swap(true, Ordering::SeqCst)
}

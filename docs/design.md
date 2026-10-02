# Design: tiered storage with a lifecycle engine

Status: draft. This doc defines where the project is going; the current symlink mover
(`src/`) is the lowest-fidelity provider of what is described here, not the end state.

## 1. The idea

One namespace over every storage tier you own — SSD, spinning disks, a LAN peer, cloud
object storage, an offline disk in a drawer — plus cache overlays in front of them (RAM
being the fastest of those), with files placed by observed use, and transparently
promoted back when they go hot again.

The model is S3 storage classes without S3: classes there are defined by **recall
latency and price**, not by hardware. The same holds locally. A tier is:

> a place where bytes live, described by how long it takes to serve one, how likely it
> is to still be there next year, what it costs per month, and which driver moves
> bytes to and from it.

Everything else — lifecycle rules, pins, restore requests, scrubbing — operates on
tiers as described above, never on specific devices.

### What the current v0.2.0 tool is in this picture

The symlink mover is the **symlink namespace provider + the hot→warm transport
driver**. It stays: it is useful on its own (no daemon, no FUSE, plain filesystems) and
it is the fallback when the real namespace provider is not running. What it will gain
is a catalog (below) so that its state is auditable and recoverable.

## 2. Tier model

A tier is configured, not discovered. `tiers.toml`:

```toml
[tiers.ssd]
kind = "fs"
path = "/mnt/nvme-pool"
volatility = "persistent"
recall = "ms"
copies = 1

[tiers.hdd]
kind = "fs"
path = "/mnt/hdd-pool"
volatility = "persistent"
recall = "ms"              # always spinning
copies = 2                 # two disks inside this tier

[tiers.hdd_parked]
kind = "fs"
path = "/mnt/hdd-pool-parked"
volatility = "persistent"
recall = "s"               # spindles spin down; first read pays spin-up
copies = 2

[tiers.offsite]
kind = "object"
provider = "b2"            # or s3, or "peer" for a LAN box
volatility = "persistent"
recall = "min"             # B2 free tier: minutes; Glacier-class: hours
copies = 1                 # but geographically separate = real durability

[tiers.drawer]
kind = "offline"
recall = "hours"           # a human walks to a shelf
copies = 1
vaults = ["drawer-07", "drawer-08"]   # where this tier's volumes physically live
```

| Field | Meaning |
|---|---|
| `kind` | Which transport driver serves this tier. |
| `recall` | Latency class: `us` / `ms` / `s` / `min` / `hours`. Drives policy and recall UX. |
| `volatility` | `persistent` for every tier of record; `volatile` only for cache overlays (§2.1). |
| `copies` | Durability floor *within* the tier. The engine schedules replication; scrubbing verifies it. |
| `cost` | Optional $/GB-month or W-idle. Enables honest placement decisions later. |

Hard rules:

1. **Every tier of record is durable.** A file's home tier is always one that survives
   a reboot and a disk swap; anything volatile is a mirror, never a home (§2.1).
2. **Every tier edge is a different transport.** fs→fs is copy + fsync; fs→object is
   chunked upload with resumable state; fs→offline is export-to-volume plus a
   catalog handshake; anything crossing the machine boundary is encrypted first.
3. **Recall latency is honest.** A parked pool is not "fast when idle"; it is `recall =
   s`, and recall-aware consumers get that answer before the read is attempted.

### 2.1 Cache overlays: RAM is a promotion target, not a tier

Decided in review: **RAM acts as a fast read cache in front of a durable tier, never as
a home.** A hot file gets a copy promoted into RAM while continuing to live on SSD,
exactly as reads are served from a cache in front of the tier of record.

```toml
[[cache]]
name = "ram"
over = "ssd"                # the tier it accelerates; never a home
kind = "fs"
path = "/mnt/ramdisk"
max_size = "32GiB"
promote_on = "2 accesses / 24h"
evict = "lru"               # eviction is cache policy, not a lifecycle rule
write_policy = "write-invalidate"   # writes go to the home tier; the RAM copy is dropped
```

Why the distinction earns its keep:

- **A tier transition moves the file; a cache population copies bytes.** The catalog's
  authoritative locations change in the first case and not in the second, so cache
  traffic never churns lifecycle state or durability accounting.
- **Losing a cache entry is not data loss**, so it needs no journal, no copy floor, no
  scrub — the resilience machinery simply does not apply. On boot the catalog's homes
  are all durable and the RAM contents are re-derivable (or just gone, as a cache).
- **Eviction is a different algorithm.** Tiers are governed by lifecycle rules (§5);
  caches are governed by capacity and recency. Keeping them separate stops the policy
  engine from growing "rules" that are really cache tuning.

The same construct generalizes beyond RAM: an SSD mirror in front of the HDD pool is
the same object with a larger `max_size` — a promotion target, not a lifecycle stage.

Two consequences worth stating early:

- **Coherency.** With `write-invalidate`, a write or rename through the namespace
  provider lands on the home tier and drops the RAM copy; a writeback cache would be
  faster but has no battery, which makes it a data-loss design on a storage server.
- **The best use of a RAM promotion target is not media.** Its real value is hot small
  files — indexes, thumbnail caches, application metadata — where residency survives a
  reboot deliberately rather than being re-warmed by re-reading. Media is already
  served acceptably from SSD, and the page cache covers incidental reuse for free.

## 3. The catalog is the source of truth

SQLite (single host) or Postgres (multi-host). One row per file version:

```sql
CREATE TABLE object (
    id          BLOB PRIMARY KEY,      -- content hash (BLAKE3), not path
    size        INTEGER NOT NULL,
    checksum    BLOB NOT NULL,         -- verified at every ingest and scrub
    created_at  INTEGER NOT NULL,
    state       TEXT NOT NULL          -- 'present' | 'offloaded' | 'restoring'
);

CREATE TABLE location (
    object_id   BLOB REFERENCES object,
    tier        TEXT NOT NULL,
    storage_key TEXT NOT NULL,         -- path on fs, key on object, volume+offset offline
    is_primary  INTEGER NOT NULL,      -- tier of record
    updated_at  INTEGER NOT NULL
);

CREATE TABLE name (
    object_id   BLOB REFERENCES object,
    path        TEXT NOT NULL,         -- namespace path, provider-agnostic
    PRIMARY KEY (path)
);

CREATE TABLE lifecycle (
    object_id   BLOB PRIMARY KEY REFERENCES object,
    last_access INTEGER NOT NULL,      -- observed by the namespace provider, or atime
    accesses    INTEGER NOT NULL,      -- observed counter since ingest
    pinned_until INTEGER,              -- pin wins over policy
    rule        TEXT                   -- which rule decided the last transition
);

CREATE TABLE volume (                  -- offline tier only
    id          TEXT PRIMARY KEY,      -- 'drawer-07'
    state       TEXT NOT NULL,         -- 'in_vault' | 'mounted' | 'loaned' | 'lost'
    note        TEXT
);
```

Why content-addressed ids: dedup comes free, a scrub can re-verify any copy from any
other copy, and restore-after-loss has a stable identity that survives path moves.
Path stays a namespace concept, not an identity.

Consequences:

- **"Does this file exist?" is a catalog question, not a filesystem question.** For an
  offline tier this is the only possible answer, and it must be actionable: a read of
  an offloaded object names the volume needed ("insert drawer-07") — never a bare
  ENOENT.
- **Deletion is a catalog transition** with reference counting across names and
  copies; the physical delete happens only when no name references the object and the
  durability floor allows it.
- **Audit is a query**: names whose primary location is missing, copies failing
  checksum, objects below their tier's copy floor, symlinks pointing at nothing.
- **Cache residency is not a location.** A copy in a promotion target (§2.1) is never
  written to `location` and never counts toward a tier's copy floor; if it is recorded
  at all it is in a separate ephemeral table for observability, so a restart can never
  make the catalog believe a volatile copy is data of record.

## 4. Namespace providers

The namespace is the product; bytes are an implementation detail (this is the S3
invariant, and the reason the symlink-only design breaks real workloads).

| Provider | Mechanism | Recall | Breakage mode |
|---|---|---|---|
| **fuse** (primary) | FUSE mount; offloaded file reads trigger recall inline | streaming | daemon crash: mount fails closed; keep the symlink provider as fallback |
| **symlink** (today's tool) | real file replaced by relative symlink | n/a — no recall, consumer must cope | consumers that do not follow links (rsync/backup defaults, some SMB clients, qBittorrent verify) |
| **gateway** | S3/WebDAV endpoint over the catalog, for backup apps and non-POSIX consumers | explicit restore semantics | none unusual |

The FUSE provider's one non-negotiable: **access observation**. Every open/read/close
updates `lifecycle` directly — no atime, no relatime caveats, no fanotify. This fixes
the v0.2.0 weakness where "usage" is really atime and degrades to mtime on
`noatime`/ZFS.

Recall behavior: a read of an offloaded object blocks while the driver pulls it one
tier up, streaming into the caller's file descriptor. Policy decides whether the
recalled copy is promoted permanently, cached with TTL, or read-through only. A slow
tier (`min`, `hours`) is also allowed to refuse inline recall and instead require an
explicit `just_cache restore <path>` — the S3 "restore request" model — so that
no application timeout ever turns into a broken read.

## 5. Policy engine

Rules in `policy.toml`, evaluated per file against the catalog:

```toml
# S3 Intelligent-Tiering: move down when idle, up when read again
[[rule]]
name = "intelligent-tiering"
match = "**"
down = { after_idle = "30d", from = "ssd", to = "hdd_parked" }
down = { after_idle = "180d", from = "hdd_parked", to = "offsite" }
up = { on_access = true }          # recall + promote on read

[[rule]]
name = "camcorder-raw"             # per-path overrides
match = "video/raw/**"
down = { after_idle = "3d", from = "ssd", to = "hdd" }
pins = ["*.drp", "*.fcpxml"]       # never move active project files

[[rule]]
name = "drawer-annual"
match = "photos/2019/**"
down = { after_idle = "365d", from = "hdd_parked", to = "drawer" }
requires_verify = true             # scrub before export
```

Rules must be **explainable**: every transition records which rule fired and why
(`rule` column), and `just_cache explain <path>` answers "where is this file and why".
Dry-run remains a first-class mode for the whole engine.

Policy governs **tiers only**. Populating and evicting a cache overlay (§2.1) is
capacity- and recency-driven and never appears here — a file "promoted to RAM" is not a
lifecycle transition, and no rule should be able to make a volatile copy the place a
file lives.

Cost-aware placement (later phase): given a tier's cost model, report — and
optionally act on — the delta of keeping each subtree where it is. This is the part
S3 does inside one provider and self-hosted stacks don't do at all.

### 5.1 Scope and exclusions: an allowlist, with size and path filters

Nobody wants everything moved. A lifecycle engine that acts on a whole tree by default
is a foot-gun, so scope is explicit and comes in three layers:

```toml
[scope]
# Layer 1: only these subtrees are managed at all. Nothing outside them is observed,
# scored, reported or moved — a tree is opted in, never swept by default.
include = ["media/**", "scratch/**", "backups/nightly/**"]

[scope.never]
# Layer 2: patterns that are never candidates, at any tier, by any rule.
paths = ["**/node_modules/**", "**/.git/**", "**/*.part", "**/*.db", "**/*.sqlite*"]
# Live application state: moving it under a running writer is a corruption, not a
# stale copy.
min_size = "1MiB"          # symlink churn over 4 KiB files buys nothing
max_size = "500GiB"        # a huge image takes hours to move; handle deliberately

[scope.never.offsite]
# Layer 3: tier-specific vetoes — never send this anywhere off the machine, no matter
# how cold it gets.
paths = ["documents/personal/**", "**/*.kdbx", "**/id_rsa*"]
```

Why the layers are separated:

- **Allowlist over denylist.** A denylist fails open: a new directory nobody thought
  about gets swept. An allowlist fails closed, which is the only acceptable direction
  for something that moves other people's files.
- **Size bounds are policy, not trivia.** Tiny files make symlinks that cost more than
  the bytes they save; enormous files turn a sweep into a multi-hour transfer and a
  free-space gamble. Both deserve to be named in config rather than hit by accident.
- **Tier-specific vetoes matter once remote tiers exist.** Cost and latency are not the
  only reasons to keep data home — privacy and legal scope are, and the moment a tier
  leaves the building the answer can differ per subtree.
- **Exclusions are explainable too.** `just_cache explain <path>` must be able to say
  "not managed: outside `scope.include`" or "excluded by `scope.never.paths`", in the
  same way it names the rule that moved something. An exclusion that cannot be
  explained is indistinguishable from a bug.
- **Belt and braces in the mover.** Exclusion is checked where candidates are chosen
  *and* again immediately before the bytes move, so a miscomputed rule or a stale
  catalog entry cannot move something an exclusion protects.

This is a P0 requirement for the existing tool, not a future nicety: `--include` and
`--exclude` globs, plus `--min-size`/`--max-size`, belong in the CLI before anyone
points it at a real tree.

## 6. Durability

- **Checksums everywhere**: computed at ingest, verified on every copy and at scrub.
- **Copy floor per tier**: the scheduler maintains `copies`; a missing copy is a
  repair job, reported, not silent.
- **Scrub**: background read-through of every location, checksum against catalog;
  corrupt copy → restore from a good copy (or mark object damaged if none).
- **Verify-before-delete**: no source bytes are removed until the destination copy
  checksums clean. (v0.2.0 compares sizes only — known gap, listed below.)
- **Encryption at tier edges that leave the machine**: client-side, keys never leave
  the host. Object and offline tiers get this by default.

## 7. The mover's safety rules (all providers)

These are lessons already learned in v0.2.0 and are binding for every driver:

1. **Journal every operation** (intent → done), and on startup, replay/repair: a crash
   between "source removed" and "symlink created" must be recoverable.
2. **Idempotency**: re-running any driver on any state converges — skip already-done
   work, adopt byte-identical destinations, refuse size-mismatched ones (upgrade to
   checksum comparison when the catalog exists).
3. **Preserve everything**: mode, owner, xattrs, mtime, and sparseness
   (`copy_file_range`/`SEEK_HOLE`). A sparse 40 GB image must not become 40 GB real.
4. **Never create a destination root**; an unmounted disk must error, not become a
   directory on the wrong filesystem.
5. **No silent data loss**: one file failing never stops a sweep; failures are
   reported per file; `--once` exits nonzero if anything failed.

## 8. Phases

- **P0 — harden what exists** (symlink provider): journal + startup repair;
  preserve mode/owner/xattrs/sparse; do-not-move if the file is open or hardlinked
  elsewhere; `audit` command (cold copies without a symlink, symlinks without a target);
  CI test that exercises the EXDEV path. *(Done: scope controls per §5.1; metadata,
  sparseness and checksum adoption; the open-file/hardlink guard; `audit` + `--repair`;
  the journal and startup recovery; CI running the EXDEV path. P0 is closed.)*
- **P1 — catalog**: SQLite catalog ingesting the current mover's state (it already
  leaves an auditable pattern); `explain`, `locate`, `restore`; two-disk replication
  within a tier; scrubbing. *(The filesystem half of `explain` has shipped in the symlink
  provider: it reports the mover's own decision and carries the seam the catalog slots
  into; the catalog-backed `explain`/`locate`/`restore` land with the catalog itself.)*
- **P2 — FUSE namespace provider**: observe accesses properly; streaming recall;
  pins/restore semantics; gateway (S3/WebDAV) provider; cache overlays (§2.1) — a RAM
  promotion target first, since the same construct later fronts the HDD pool with SSD.
- **P3 — remote tiers**: object-store driver (chunked, resumable, encrypted);
  LAN-peer driver; offline-volume driver with vault tracking and insert-prompt recall.
- **P4 — cost-aware policy**: per-tier cost models, placement reports, rule
  suggestions from observed access.

Each phase ships something usable alone: P0 is a better standalone mover; P1 makes it
trustworthy; P2 removes the symlink breakage; P3 completes the S3 analogy; P4 is the
part nothing else does.

## 9. Known gaps (tracked, not hidden)

Closed in P0: metadata/sparseness loss on cross-device copies; size-only adoption;
the open-file/hardlink gap; the crash window between source removal and symlink
creation; and CI's failure to exercise the EXDEV path.

Closed in P1 by `just_cache catalog sync` (#16): the catalog is now *written*, so "where
does this file live" is finally a question the filesystem is not the only answer to. It is
content-addressed (id = BLAKE3 of the bytes, computed at ingest, so a rename keeps its
identity and a scrub can compare any copy to any other); a location is a `(tier,
storage_key)` pair where a tier is a real root — the watch root or a `--dest` root — and
never a cache overlay (§2.1); and idle files move through the lifecycle states
`present` -> `offloaded` -> `restoring`. Ingest is one SQLite transaction, committed at the
end, so an interrupted sync leaves the catalog exactly as it was rather than half-applied
(the same guarantee `journal.rs` gives a move). A tree the mover has already been running
against is ingested by the first sync — migrations that predate the catalog are found by
the same walk, which is why the migration story is "ingest, not migrate". A location is
only removed when another location still holds the object (a normal offload, where the
stale hot row is replaced by the cold one) or when no name references the object: a name
is never stranded pointing at an object with nowhere to live.

Still open, and honestly so:

- **A tree edited by hand is reported, not reconciled.** A name the catalog recorded that
  is gone or now hashes differently, and a location whose file vanished or was replaced,
  are reported — `sync` exits non-zero — and their rows are left exactly as they were; new
  names and locations that nothing contradicts are still ingested. What is missing is a
  resolution step: deciding a vanished name was a rename might be a human command, but it
  does not exist yet, so a difference repeats on every sync. That is deliberately louder
  than auto-healing in the wrong direction, and it is the honest state of #16.
- **Only the symlink provider's flat mirrored layout is understood.** Two-disk replication
  within a tier, remote/object tiers, and offline volumes are later work; the `volume`
  table ships empty.
- **`lifecycle` is ingested, not maintained.** `last_access` is filled at first ingest
  from atime; nothing updates it yet because proper access observation is the namespace
  provider's job (§4, P2). A `restoring` state is recorded when both a hot and a cold copy
  exist, but no command drives a restore to completion; `audit` remains the tool that
  tells a restore-in-progress from a true duplicate.
- **Every file is hashed on every sync.** Correct, because identity is the hash, but not
  cheap; there is no digest cache keyed on size and mtime yet.
- **One unreadable file aborts the sync** rather than being skipped: a catalog built by
  silently ignoring a file would be a catalog that disagrees with the tree. The mover's
  rule that one failure never stops a sweep has no catalog equivalent yet.

- **Journal records carry a size, not a digest.** Recovery refuses to link a name to a
  copy whose size does not match what the move promised, which is the strongest check
  available without hashing every file before every move. A same-size-but-corrupt copy
  would still be linked; the catalog's digests (P1) are what close that, and the mover
  already checksums destinations it *adopts*, so the hole is narrow and named.
- **No read-back verification of freshly copied bytes.** A torn copy is caught by the
  short-copy and source-changed guards, not by re-reading what was written: hashing a
  sparse file means materializing its holes, and re-reading a 40 GB copy doubles I/O.
- **Restore has no catalog digest to check a cold copy against.** `restore` reads its own
  write back and hashes it before the atomic swap, so a torn copy cannot reach the path,
  but with no recorded digest there is nothing independent to compare a *pre-existing*
  cold copy to: a copy that was already corrupt would be restored faithfully. The catalog
  (issue #16) is what closes this; until then `restore` also refuses to overwrite a hot
  regular file whose content differs from the cold copy (checked by digest, not size), so
  the one thing it can compare is never ignored.
- **`locate` is not built.** Finding an object by path or digest across tiers is a catalog
  question (§3), and it is issue #16's to answer; `restore` finds a copy by filesystem
  state alone (the symlink, or the mirrored relative path under each `--dest`) and refuses
  when the candidates disagree rather than guessing a schema the catalog has not defined.
- **A default restore leaves a `duplicate` for `audit`.** Because the cold copy stays
  unless `--remove-copy` is given, the tree ends with a regular file *and* its cold copy —
  the exact state `audit` calls `duplicate`. That is the requested behavior (the copy stays
  by default), but a user restoring many files without `--remove-copy` should expect the
  duplicates, not be surprised by them.
- **The journal is one file per watched root**, not part of the catalog. Two sweeps of
  different trees cannot see each other's in-flight moves, which is correct today and
  a thing to revisit when the catalog exists.
- **A damaged journal stops the sweep** rather than being ignored. Deliberate — it may
  describe bytes already on a cold tier — but it is a hard stop a human has to clear.
- **Root-owned files cannot be relocated** by a non-root sweep: the copy fails on
  chown. Failing is right (a silently wrong owner is worse), but a privileged mode is
  not built.
- Access tracking depends on atime semantics of the host mounts (`noatime`, ZFS).
- **`explain` never moved files and reads the filesystem, not the catalog.** The command
  is the first half of the P1 item: it evaluates scope, guards and policy in the mover's
  order and reports the outermost reason, but its "where does this live / when was it last
  accessed" answers come from the filesystem. `src/explain.rs` carries a single commented
  seam (`catalog_answer`) that returns `None` until the catalog exists; it names the one
  query the catalog must provide (existence, `state`, primary `location`, `last_access`
  and its provenance, `accesses`, `pinned_until`, `rule`). A disagreement between the two
  sources is reported, not resolved silently — the branch exists and is tested through an
  injected answer, but no catalog is read yet, because its schema belongs to the catalog
  issue and guessing it would collide with the build happening in parallel.
- **`explain` answers for one path, so it cannot account for the per-sweep `--limit`.**
  A file can be in scope, unguarded and cold and still miss its slot to older files in a
  real sweep over the tree. The command says what the policy decides for the path, not
  what one particular sweep's ordering decided; naming the difference is preferable to a
  "would move now" that a tree-level tie-break could overrule.
- **The atime→mtime fallback cannot be reached on a normal Linux mount.** `stat` reports
  an access time even under `noatime` (it is simply not updated), so the fallback branch
  is exercised at its decision point in a unit test rather than end to end. What `explain`
  *can* surface on such a mount is the symptom: when atime equals mtime it says so, since
  the stamp may then be the last write rather than the last read.

## 10. Non-goals

- Block-level tiering (dm-cache/bcache/L2ARC): different layer, no per-file policy;
  out of scope but composes (a block cache in front of this is fine).
- Multi-user quotas/permissions: single-trust-domain system.
- Backup *tooling* (dedupe, snapshots of the whole tree): this is a lifecycle engine;
  backup apps are consumers via the gateway.

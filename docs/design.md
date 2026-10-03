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
  within a tier; scrubbing. *(Two-disk replication has shipped as `sweep --copies N`
  with verify-before-delete, a recorded per-tier floor, and `under-replicated` /
  `replica-lost` reporting; the copy is same-host — see §9 and §10. The filesystem half
  of `explain` has shipped in the symlink provider: it reports the mover's own decision
  and carries the seam the catalog slots into; the catalog-backed
  `explain`/`locate`/`restore` land with the catalog itself.)*
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
the open-file/hardlink gap (the open-descriptor half is now backed by a per-candidate
`/proc` re-scan immediately before each move — #77 — with the residual window named
below); the crash window between source removal and symlink creation; and CI's failure
to exercise the EXDEV path.

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

Closed in P1 by `just_cache locate` (#18): finding an object is now a catalog query, not a
walk. `locate <QUERY> --catalog <FILE>` reads the `name` table for a namespace path and
searches `object` by hex-id prefix for a content digest (a query of at least eight hex
characters is read as a digest; anything else is a path, and the reading chosen is reported
in `--json` as `kind`). The answer names every copy and its tier, which copy is the tier of
record, and the object's state (`present` / `offloaded` / `restoring`), and it stays
answerable when a tier is not mounted because no byte is read. Exit codes are the contract:
`0` found, `1` nothing found, `2` a missing or unreadable catalog. Multiple prefix matches
are all listed, never guessed down to one. Alongside it, `explain` now consults a catalog
when one exists (the default beside the watch root, or `--catalog`) through the single
`catalog_answer` seam and reports a catalog/filesystem disagreement instead of resolving it
silently; with no catalog its answers are exactly what they were before.

Closed in P1 by `just_cache scrub` (#21): every location in the catalog is read back and
hashed against the object id, which *is* the checksum, so "are the bytes still what I put
there" is a question the tool answers between operations. A location that no longer matches
is repaired from a sibling that verifies clean — through `restore`'s
build-a-private-copy-then-rename path, so the replacement is read back and hashed before the
atomic swap — and one with no good sibling is *marked damaged* (the `damage` table) and
reported, never deleted. Last-verified state is written per location (`scrub_state`) as the
run goes, so a kill resumes instead of re-reading a tier, and `audit --catalog` can say
"never scrubbed" for a copy instead of implying the catalog vouches for it. `--dry-run`
reads and reports what it would repair, writing neither the repair nor any state. On the §9
sparse-file question below: a scrub only *reads*, and `read(2)` on a hole returns zeroes
without allocating disk blocks or unsharing a reflink extent on common Linux filesystems
(ext4/xfs/btrfs/tmpfs) — the copies a repair writes reuse `copy_contents`, which already
preserves holes — and `tests/scrub.rs` measures `st_blocks` across a scrub of a 64 MiB
sparse file and asserts it is unchanged. The §9 warning was about the copy path, which was
already fixed; it did not apply to verification reads.

Closed in P1 by issue #72: `audit`, `scrub`, and `reconcile` no longer turn a location row's
`tier` and `storage_key` into a filesystem path blindly. A sync records the canonical watch
and destination roots in the catalog; readers prove the tier is one of those roots and reject
an absolute key or any key with a `..` component before stat, hash, create, or replace. A
refused row is reported as `malformed-catalog`, and the outside path is not touched. The
integration test edits the catalog directly to reproduce absolute-key, parent-component,
and unknown-tier rows; it asserts the outside sentinel and the would-be reconcile target
remain unchanged/nonexistent. A catalog from before root tracking fails closed until a
successful `catalog sync` records independently observed roots. This prevents a path-escape
through a location row, not an operator who edits both the location and trusted-root rows
consistently; the catalog is not a tamper-proof trust store.

Closed in P1 by #75: the resume path no longer discards a repair candidate's verdict. When a
group has rot but no sibling verified clean *this run*, `scrub` re-reads its already-verified
locations as possible repair sources — and that re-read is now recorded as the location's real
verdict rather than thrown away. A candidate that has rotted since it was last verified
replaces its `AlreadyVerified` entry with `Corrupt`, so it joins the damage pass and
`mark_damaged` drops its `scrub_state` row instead of leaving a row that keeps claiming the
bytes were verified (which every later scrub would skip without reading). The search also
carries on past a failed candidate, so a clean sibling *later* in the group is still found and
used as the source. Before the fix only the first already-verified location was tried, its
non-clean result was ignored, and `tests/scrub.rs` pins both behaviours: two locations rotted
in place are both marked damaged, and a rotted first candidate plus a clean later one repairs
from the later one.

Closed in P1 by `just_cache restore` through the catalog (#18): restoring an object now
verifies the bytes it writes against the object's **recorded checksum**, not against the
cold copy's own bytes. The open-if-present rule the rest of the tool uses applies: a
`--catalog <FILE>` must exist (a bad invocation otherwise), the default
`.just_cache-catalog.sqlite` beside the watch root is consulted only when it is already
there, and `restore` never creates a catalog (invariant 9). With a catalog, the digest the
read-back is compared to is independent of the bytes being copied, so a cold copy that was
already corrupt before the command ran is refused and nothing is touched — the hot path
keeps its link, the cold bytes stay, and no partial is left behind. A catalog that is
present but does not name the path is also a hard error: silently falling back to
filesystem-only verification would skip exactly the check the catalog exists to provide.
A hot regular file is likewise compared against the recorded checksum, so a path that
already holds the right bytes is still the idempotent no-op and `--remove-copy` still
drops the cold copy only after the restored file verifies. Without a catalog the restore
is byte-for-byte what it was before (verify against the cold copy; a pre-corrupt copy
cannot be caught — there is nothing independent to compare against, see below).

Hardened as a security fix (#70): `restore` now proves a cold copy lives under a `--dest`
root before it acts on one. A symlink in the watched tree is writable by anyone who can
write the tree, so its target is not accepted as a copy on trust: with an unchecked target,
`restore --remove-copy` deletes an arbitrary file the operator can read, and a plain
`restore` copies an out-of-tree file into the watched tree. The link target is now accepted
only when `canonicalize` puts it under a canonicalized `--dest` root — the same containment
`audit::resolve_target` applies to a watched symlink — and a target that escapes every root
is a *foreign link*, dropped rather than guessed at. The mirrored candidate is held to the
same test, so a name inside a tier that is itself a symlink out of it is no more a copy; and
`--remove-copy` re-tests containment immediately before its one unrecoverable delete rather
than trusting a proof made in `locate`. Finally, a path argument whose relative part carries
a `..` component is refused before any lookup, copy or delete: `absolute()` is lexical and
does not resolve `..`, so `T/../escaped.bin` would otherwise strip to `../escaped.bin` and
name a file outside the tree. `tests/restore.rs` drives all three refusals through the real
binary, and each fails when the fix is reverted.

Closed in P1 by `just_cache audit` reading the catalog (#19): "audit is a query" (§3) is
now true rather than an aspiration. When a catalog exists — `--catalog <FILE>`, or the
default `.just_cache-catalog.sqlite` beside the watch root — the audit answers from the
recorded `name`, `object` and `location` rows plus a *single* pass over the watched
namespace, instead of walking the tree and every tier and reconstructing the truth from
both. It reports a name the tree no longer has (`name-vanished`), a recorded location with
no file (`missing-copy`, primary or replica), a primary copy whose bytes no longer match
the recorded checksum (`checksum-mismatch`), an object with no surviving copy
(`copy-floor`), a symlink that resolves to a version the catalog does not hold
(`unknown-version`), and a path the catalog has never seen (`unknown-path`) — the last is
reported and never adopted. The walk-based audit stays as the bootstrap and the fallback
(auto-selected when no catalog file exists), manual `--repair` there is unchanged, and a
catalog/file disagreement is a finding, never a rewrite: catalog-mode `--repair` marks each
finding for resync and touches neither the tree nor the rows. The exit-code contract is
unchanged (`0` clean / `1` findings / `2` usage).

Closed in P1 by `just_cache reconcile` (#24): a disk that comes back after a sweep ran
while it was out no longer has to wait for a human to re-run that sweep. The command reads
the catalog — the `tier` table's recorded floors, every location row, the `damage` marks —
and for every offloaded object below its floor it rebuilds the missing copy from a sibling
*whose bytes are hashed and compared to the object's recorded checksum before anything is
copied*: a sibling that only matches on size is not a source, and one that hashes to
something else is marked damaged and skipped. The copy itself is written through
`restore`'s build-private-copy-then-rename path — the bytes are read back and hashed while
still private under the `.just_cache-partial-*` marker, so nothing unverified is ever
published and the pass never has to delete even its own output — and only then is the
location recorded via `record_replica` with the real checksum. A destination root is never
created (a still-unmounted disk is reported, not turned into a directory), a file already
at the destination is hashed and either adopted or left exactly as it was, and nothing is
deleted: not the source of a rebuild, not a stranger, not the last copy. It is a new
subcommand rather than a sweep phase or a scrub mode on purpose: a sweep's job is choosing
what to offload next from the watched tree, a scrub's job is reading back every copy that
is *present*, and a rebuild is a writer with its own contract — folding it into either
would make a policy run or a rate-limited read pass quietly place copies. Its exit code
follows the scrub precedent: anything rebuilt is still a finding (a disk was out, and cron
has to see that once), so `0` means every recorded floor was already met, `1` means
something was rebuilt/adopted or could not be, `2` a missing or unreadable catalog — which
is a usage error here, because the recorded checksum a rebuild is proved against comes
from it. What is deliberately out of scope is deciding *when* to reconcile (§10, like
scrub scheduling) and verifying copies that are present — a stat per location answers
"is this copy absent", and anything more is the scrubber's job.

Closed in P1 by the journal-record containment check (#68): `journal::repair` no longer acts
on the paths a record names without first proving they are paths it is allowed to touch. A
`rel` that is absolute, or that contains a `..` component, is refused — `watch_root.join`
discards the root for an absolute path and does not resolve `..`, so either shape let a
hand-edited or truncated journal line make the sweep create a symlink (with a
size-checked-but-never-hashed destination) or delete a `.just_cache-partial-*` file
anywhere on the filesystem, before the first move ran. The joined source is also required
to sit under the watched root, the same `strip_prefix` containment `restore` applies
(§4), and a record's `dest` must sit under one of the `--dest` roots the invocation was
given. A refused record is reported as `REFUSED a journal record` and kept in the journal
so the operator sees the line on the next run; nothing it names is touched. This is the
`SECURITY.md` in-scope case (a journal line that escapes the watched tree or a destination
root), and `tests/journal.rs` writes such lines and asserts the outside paths are
untouched — the test fails if the check is removed.

Closed in P1 by `catalog sync` (#73): the catalog is now opened as the untrusted input it
can be. Its default path is inside the watched tree, and SQLite opens a database with a
plain `open(2)` and writes predictable `-journal`/`-wal`/`-shm` siblings beside it, so a
symlink planted at any of those names used to make the tool write, truncate or unlink the
link's target as its own user. Now the name is created with `O_CREAT|O_EXCL` and mode 0600
(exact under any umask, because umask can only clear bits 0600 does not have), an existing
name is refused unless it is a regular file (`symlink_metadata`, so a dangling link is
refused rather than reported absent by `exists`), the sibling names are refused the same
way *before* the database is created, and SQLite is opened with `SQLITE_OPEN_NOFOLLOW`.
`open_existing` — the mover's read-only path — refuses a symlink too instead of following
it. Nothing is read or written through the link. Left open and named rather than hidden: the
foreign-owner refusal (a catalog owned by another uid inside a group- or world-writable
directory) has no deterministic test, because producing it needs a second user or root, so
it is only as good as the code that reads it; and the sibling check is a stat taken before
SQLite's own open, so a writer that swaps a name in that window is not covered — a race no
unprivileged test can win, and one that `SQLITE_OPEN_NOFOLLOW` narrows for the database file
itself. What is *not* fixed here is a catalog an earlier version already created 0644: this
change fixes creation, not the mode of a file already on disk (§10).

Closed by the symlink-location hardening (security review, #76): a symlink at a stored
location is no longer accepted as a copy. `scrub`'s `check_location` used `fs::metadata`,
which follows the final component, so a link to a byte-identical target statted as a
regular file, hashed clean, and was recorded verified in `scrub_state`;
`replication::place` rejected only a directory and then hashed through the link, so a
symlink whose own target text happened to be the source's size was `Adopted` and counted
toward the floor — letting the source be retired over a name whose target can be removed
without the catalog noticing (the invariant-6 shape). Both now stat with
`symlink_metadata` and require `metadata.is_file()`, matching `reconcile` and `restore`,
which already refused a non-regular location. A link is reported `Unreadable` by scrub and
`Failed` by place, is never recorded verified, and does not satisfy the floor; the source
is kept. `tests/scrub.rs` and the `replication` module's tests both drive a link to a
byte-identical target — with the link's own length equal to the source's, so the old
length check waved it through — and fail when either guard is reverted.

Closed by #71: a `--dest` path that resolves through a symlink no longer escapes the root.
The mover now opens the destination directory once, walking each component *beneath* the
root with `O_NOFOLLOW|O_DIRECTORY` (creating missing ones with `mkdirat`), and refuses a
component that is a symlink (or is not a directory) with a named `DestinationSymlink` /
`DestinationNotDirectory` report. The partial is created and the finished copy published
with `openat`/`renameat` relative to that held descriptor, so a component swapped after
resolution cannot redirect the write — resolving with `openat2(RESOLVE_BENEATH)` was the
alternative, but per-component `O_NOFOLLOW` needs no kernel-version floor and no `unsafe`.
The same descriptor is re-stat'ed (`fstat` of a fresh `O_NOFOLLOW` open, compared by
`(st_dev, st_ino)`) immediately before the source is removed, so a destination replaced
after its content verified leaves the source in place instead of deleting the last verified
copy. Both behaviours have deterministic integration tests: a symlinked `--dest` component
on a second filesystem (`tests/cross_device.rs`), and, through a new
`JUST_CACHE_FAULT=replace-verified-dest` seam, an adoption whose destination is swapped in
the verification-to-removal window (`tests/fault_injection.rs`). Both fail if the guard is
reverted.

Closed by #80: a `--dest` root nested inside the watched tree — or a `--watch` nested inside
a `--dest` root — is now refused up front, in both directions. The shared `validate_paths`
check used to reject only `dest == watch` (canonical equality), so `--watch /data --dest
/data/cold` passed: the sweep wrote its copy into a directory it also scans, discovered that
copy on the next pass, and tiered it deeper, moving its own output once per pass. The check
now compares the canonical roots component-wise (`Path::starts_with`), which is what makes
`/data` a prefix of `/data/cold` but not of a sibling named `/database`, and it names the
direction in the refusal ("inside the watched tree" / "inside the `--dest` root"). Because
`validate_paths` is shared, every subcommand that takes a `--watch`/`--dest` pair inherits
it; the acceptance criteria cover the sweep and `catalog sync`, and `tests/nested_dest.rs`
drives the real binary for both, both directions, plus a symlinked `--dest` that resolves
inside the watch (the check is on the canonical path, not the spelling) and a name-prefix
sibling that must still be accepted.

Still open, and honestly so:

- **The open-descriptor re-check is a fresh scan, not a lock (#77).** The sweep's
  snapshot only sees descriptors that existed when the sweep began, so the mover re-scans
  `/proc/*/fd` immediately before each candidate's bytes move (`Guards::recheck`) — once
  per file it is about to move, not per file in the tree — and the hardlink half re-stats
  live in the same step. That catches a file opened during the walk, which the snapshot
  could not. What remains is the gap between the scan and the rename/remove: a descriptor
  opened in that instant is still missed, because no kernel primitive says "refuse the next
  open". The window is now microseconds at the point of the operation rather than the whole
  sweep, and it is named here rather than implied away. Its timing through the binary is not
  tested (the window cannot be hit deterministically from a test that only drives the
  binary); the re-check is pinned instead by a library test that takes the snapshot, opens a
  real descriptor in another process, and asserts the move is refused — reverting the mover
  to the snapshot-only check fails it.

Closed by the journal hardening (#69): the journal and its compaction temp no longer follow
a symlink at their fixed names. `Journal` opens the journal `O_NOFOLLOW` and creates the
compaction temp with `create_new(true)` — the same refusal the copy path already made — so a
link planted at `.just_cache-journal` or `.just_cache-journal.compacting` is reported and
refused rather than redirecting every appended intent record into, or truncating, the file it
points at. The end-of-sweep compaction now reports a failure instead of discarding it (the
old `let _ = journal.compact()`), so a refused temp is visible even when the sweep itself had
nothing to do. Both the journal and the temp are created 0600 *explicitly* rather than under
the caller's umask, and an existing journal is brought down to 0600 the moment it is opened:
the journal names every in-flight move, destination included, so 0644 on a shared tree is a
disclosure and a permissive umask makes it the primitive that lets another writer forge a
record. Proven by `tests/journal.rs` — both links planted with the target asserted intact and
the refusal named on stderr, and the created file's mode asserted under `umask 0` — and by
unit tests in `src/journal.rs` that fail when either guard is reverted.

Closed by #78: `migrate_replicated` now journals the destination a replicated move
*actually retires to*, not the first destination it tried. The mover retires the source to
the first copy whose read-back digest verified, which need not be the first `--dest`: an
unavailable or full first destination is skipped and a later one verifies. The intent
written before the copy named that first destination, so a crash between removing the
source and creating the symlink made `journal::repair` read a path holding no copy and
report DATA LOST while verified copies existed. The verified destination is now recorded
again after replication and before the source is removed, and if that record cannot be
written the source is kept rather than retired under a record recovery cannot trust. The
report names the same verified copy. `tests/migration.rs` drives the mover with a first
destination that is not a directory and a second that verifies, leaves the record standing
(the watched tree is read-only so the retirement cannot finish), removes the source to
stand in for the crash, and asserts recovery restores the name from the verified copy
rather than reporting the data lost — the test fails when the re-record is reverted.

Still open, and honestly so:

- **A hard link at the journal name is not distinguishable from the real journal.** The
  `O_NOFOLLOW` guard closes the symlink redirect, but a file carrying the journal's name that
  is a hard link to another file passes the regular-file check, and an append lands in the
  shared inode just as a following open would have. Nothing can tell the two apart without
  ownership metadata, and the journal lives inside the watched tree where any writer can plant
  one; a `nlink > 1` refusal (the rule the mover already applies to move candidates) would
  cover the planted case but not a backup tool that legitimately shares the inode. Left
  unpatched deliberately and recorded here rather than in a commit message.
- **Closed by the `JUST_CACHE_FAULT` hook (#25): a destination that disappears
  mid-sweep is now deterministically testable.** A destination that becomes unavailable
  *between* two copies of one sweep, or while a freshly written copy is being read back,
  cannot be timed from a test process that only drives the binary — the old library tests
  had to remove the root before the call, an arrangement production never produces. The
  hook is the narrow seam for exactly that window, in `src/replication.rs`: with
  `JUST_CACHE_FAULT=unavailable-after=N` in the child process's environment, once N copies
  have verified every further destination root is reported exactly as an unmounted mount
  is (`Unavailable`); `vanish-readback=N` removes the Nth freshly written copy just before
  its read-back hash, so verification fails on bytes that were there a moment ago; and
  `corrupt-readback=N` flips one byte of the Nth fresh copy, so verification fails on a
  same-length stranger of the sweep's own making. It is test-only by construction: unset —
  as production always leaves it — the variable is read once per `replicate` and every
  check is a branch on `None`, and `tests/fault_injection.rs` asserts the inert case
  against the hook-carrying binary itself. A value that is set but unparseable panics,
  deliberately: a fault switch that silently no-ops would let a mistyped test pass green
  with nothing injected. What the tests prove through the real binary: the source is kept
  below the floor, the report names the disk, no `.just_cache-partial-*` file survives,
  the catalog is never told the unverifiable copy is good (`locate` lists only the copy
  that verified), and a later sweep heals what a sweep can heal. One failure mode remains
  untestable end-to-end and is said so rather than faked: a destination *root* that is
  gone makes `audit`/`catalog sync` a usage error by design (invariant 1 — a root is never
  recreated), so a truly unmounted tier is verified by restoring the mount, not by a
  command that invents a directory.
- **The same hook now covers the copy itself: a destination that disappears *while bytes
  are in flight* is deterministically testable (#36).** The #25 mechanisms above land
  *between* copies and during a read-back; the narrowest window — after the private
  `.just_cache-partial-*` file exists and while the write loop is still filling it — could
  not be reached at all, because the loop sees only `File` handles and the unlink needs a
  path. `disk_management::copy_contents` therefore takes a test-only progress seam: an
  `Option` callback invoked after each copied chunk, `None` in production, so an unobserved
  copy is the same `io::copy` it always was. `copy_into_place`, which owns the paths, arms it
  only under `JUST_CACHE_FAULT=unlink-mid-copy=N`, and only for the **first** copy in the
  process: once `N` bytes have been written it unlinks the partial and removes the nested
  directory the copy created, modelling a mount going away mid-write. The open descriptor
  keeps the unlinked inode alive, so the write loop itself can keep succeeding while every
  path-based step after it — `preserve_metadata`, the length re-check, the rename, and the
  caller's read-back — gets `ENOENT`, which is exactly what the test asserts. Through the
  real binary: the source is kept below the floor (invariant 2), the report names the vanished
  disk, no `.just_cache-partial-*` survives on either side (invariant 8), the catalog is never
  told the faulted tier holds the object (`locate` lists only the copy that verified), and a
  later clean sweep heals from the intact source and the surviving copy.
  `tests/fault_injection.rs` drives it; a `disk_management` unit test proves the seam reports
  mid-copy (several chunks, the first strictly before the end), and disabling the seam fails
  both.
- **What the mid-copy seam does not cover, named rather than implied.** It reproduces the
  *effect* of an unmount — bytes and name gone, path no longer resolving — at a byte position
  a test can name; it is not the `umount` syscall, which a test has no privilege to make, and
  not a thread racing a multi-megabyte copy, which would pass only when it won the race, and
  so is not written. It cannot cover a *remount* of a different filesystem at the same path,
  nor a failure the kernel reports as `ESTALE` rather than `ENOENT`. It also reaches only the
  chunked copy paths: on one filesystem a copy is a `FICLONE` reflink with no in-flight bytes,
  which is why the through-the-binary test puts the destination on the second filesystem and
  asserts the cross-device pair before it starts.
- **Reconcile is on demand and trusts the catalog's state.** Like `scrub`, nothing
  schedules it; P2. It also decides "does this object have a hot copy" from the `state`
  column, so a catalog left stale by a sweep (a sweep records replicas but does not
  rewrite state) makes reconcile see a `present` object and skip it — the same `catalog
  sync` that reports the under-replication brings the state current, and reconcile acts on
  the catalog as recorded rather than re-deriving it.
- **Reconcile hashes every sibling candidate it considers.** The `verified` flag is not
  trusted as a source vouch (that is the whole point), so a rebuild reads its source once
  to prove it and once to copy it. No read budget yet (`scrub` has `--rate`); a rebuild of
  a large object on a busy tier is unthrottled.
- **Reconcile has no free-space gate.** A replicated sweep refuses a destination below
  `--min-free-gb`; a rebuild only fails when the copy itself fails, which is reported per
  object. A pre-flight floor is a small, honest addition when someone needs it.

- **Replication is opt-in and same-host.** `sweep --copies N` places a verified copy on
  N distinct `--dest` roots before it retires the source, and `catalog sync --copies N`
  records the floor per tier and reports objects below it as `under-replicated` (a lost
  copy is a `location-missing` finding that names the disk it was on). What distinctness
  means is a configured *root*, not a device: two directories on one pool count as two,
  and the operator can see that in the command they ran. With a floor of 2 and one other
  disk the second copy lands on that disk on the **same host** — this is replication
  across a disk failure, not off-host backup (§10); a machine-level loss still takes both
  copies. Off-host tiers are P3.
- **A copy the mover could not verify is unknown, never good.** The mover counts only a
  copy whose read-back digest matched, and it does not retire the source until the floor
  is met; the catalog's `location.verified` is 0 until a sync hashes the bytes, and rows
  that stay unverified are reported as `replica-unknown`. The state a crash between
  "written" and "checked" leaves is that unknown row, and no floor counts it.
- **`--limit` under replication is per sweep, not per disk.** With `--copies N` the sweep
  replicates the same `--limit` candidates across all destinations; it does not fill each
  disk to its own limit first, because a floor is about one object living in N places, not
  about how many objects a disk takes.
- **A tree edited by hand is reported, not reconciled.** A name the catalog recorded that
  is gone or now hashes differently, and a location whose file vanished or was replaced,
  are reported — `sync` exits non-zero — and their rows are left exactly as they were; new
  names and locations that nothing contradicts are still ingested. What is missing is a
  resolution step: deciding a vanished name was a rename might be a human command, but it
  does not exist yet, so a difference repeats on every sync. That is deliberately louder
  than auto-healing in the wrong direction, and it is the honest state of #16.
- **The copy floor is not in the schema, so `audit` can only enforce one.** §6 wants a
  *per-tier* `copies` floor the scheduler maintains; the catalog shipped without a floor
  column, and adding one needs a versioned migration (an `ALTER TABLE` a `CREATE TABLE IF
  NOT EXISTS` schema never reaches), which is more than the small, honest change #19
  allowed. The audit therefore enforces the only floor the schema expresses — every object
  must keep at least one existing location — and reports an object whose every copy is gone
  as `copy-floor`. A per-tier floor is later work.
- **`audit` verifies the copy of record, not every replica.** Hashing both a hot and a cold
  copy of a `restoring` object (or of a replicated tier, once that exists) on every audit is
  the scrub §6 describes, and it would double the I/O an audit costs. Catalog mode hashes
  the primary location and only *stats* the others, so a corrupt non-primary replica is not
  caught by an audit; catching it is the scrubber's job.
- **There is no catalog-only (`--no-filesystem`) audit.** #19 allows one "where even that is
  unavailable"; not built. Every catalog-mode audit makes one pass over the watched tree,
  which is also what lets it see a path the catalog does not know. An answer with no
  filesystem at all (a catalog for a tier that is not mounted) is a thing the schema could
  support and no command does yet.
- **Catalog-mode `repair` marks for resync in its report, not in a row.** A durable per-row
  `needs_resync` flag would need the same migration the copy floor needs; until then the mark
  is the printed outcome plus the non-zero exit, and the operator runs `catalog sync` to
  re-record the tree.
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
  short-copy and source-changed guards, not by re-reading what was written while the copy
  runs: re-reading a 40 GB copy doubles I/O. Deliberate verification is now `scrub`'s job
  (#21), which reads stored copies back on a schedule the operator controls and inside an
  I/O budget; the mover still does not hash what it just wrote. The old half of this note —
  "hashing a sparse file means materializing its holes" — was wrong about *reading*: it is
  a non-hole-preserving *copy* that materializes holes, and `copy_contents` already
  preserves them. `tests/scrub.rs` measures it.
- **A restore without a catalog still has no independent digest.** With a catalog, restore
  verifies against the recorded checksum and a pre-corrupt cold copy is refused (above).
  Without one, the cold copy is the only digest available: the read-back catches a torn
  copy, a copy that was already corrupt would be restored faithfully, and that limitation
  is inherent — there is nothing independent to compare against. It is named rather than
  papered over, and `restore` refuses outright (rather than falling back) when a catalog
  is configured but does not name the path, so verification is never silently skipped.
- **A >8-character prefix collision is unit-tested, not end-to-end.** `locate` lists every
  match (a library test writes two objects sharing a prefix), but forcing two real BLAKE3
  hashes to share eight hex characters needs an impossible amount of data, so the
  through-the-binary test exercises a unique prefix only.
- **A path that is also eight hex characters is read as a digest.** Namespace paths and
  digest prefixes are disjoint in practice, but not provably so; `locate` reports which
  reading it chose (`kind` in `--json`) rather than hiding the ambiguity. A future flag
  could force the reading.
- **`locate` reports an offline copy's state; it cannot mount a volume.** State is
  `offloaded`/`restoring` and the summary says the tier must be mounted to read, but
  mounting and streaming recall are the FUSE provider's job (P2/P3), so the `volume` table
  stays the only place a drawer ID could live and `locate` never prints one.
- **`explain`'s verdict still comes from the filesystem.** The catalog is consulted for
  existence, state, primary location, and lifecycle (through the one `catalog_answer` seam),
  and a disagreement is reported, but the would-move answer is the one the mover would
  compute from the tree. That is deliberate — the explanation cannot drift from the mover —
  so a catalog-only fact (an offline tier the filesystem cannot see) informs the report but
  not the verdict.
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
- **`explain` never moved files, and now reads the catalog when one exists.** The command
  evaluates scope, guards and policy in the mover's order and reports the outermost reason,
  and its "where does this live / when was it last accessed" answers are cross-checked
  against the catalog (`catalog_answer`). The catalog is consulted, not obeyed: the verdict
  is still the filesystem's, because that is the source the mover acts on. A disagreement
  between the two sources is reported, not resolved silently — the disagreement path is
  tested both through an injected answer and end to end (a tree hand-edited after `catalog
  sync`).
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
- **Scrub has no schedule of its own.** `just_cache scrub` is an on-demand command; the
  background scheduler that decides how often to re-scrub is P2, and until it exists a
  location verified once is skipped until its content changes under the catalog (or the
  damage record for it is cleared) — unless it is the only candidate repair source for a
  group that has rot, in which case it is re-read and a failed read is marked damaged
  (#75, above). `--rate` is what makes a cron-driven scrub safe to run
  against a tier that is serving reads.
- **`audit --json` does not carry the scrub section.** `audit --catalog` adds
  "never scrubbed"/"damaged" counts to its readable output, but the hand-written JSON
  document has no `scrub` object yet; only the readable path reports it. The readable
  output is what ships, and the omission is named rather than silently implied.
- **The symlinked-component guard covers the mover, not every writer.** #71 resolves and
  holds the destination directory only in `move_file_with_symlink`. `copy_into_place`, which
  `--copies` replication and `reconcile` call, still opens `dest.parent()` by path, so a
  symlinked component under a `--dest` root reached by those paths is not yet refused. The
  mover is where the sweep's bytes land first, which is what the issue asked to close; the
  replication/restore writers are a follow-up, named here rather than left implied.
- **Destination metadata is applied through a path, not the held descriptor.** `chmod`,
  `chown`, xattr and timestamp helpers in this crate take a path; the partial's name is
  private and unguessable and is created inside the verified directory, and the in-scope
  escape (a symlink already in the destination tree) is refused before it. A component
  swapped by a process with write access to the root's *ancestors* remains out of scope
  (SECURITY.md), and is the reason the metadata helpers were not converted to `f*` variants.

## 10. Non-goals

- **Deciding *when* to reconcile is not this feature's job.** `just_cache reconcile` is an
  on-demand command, exactly like `scrub`: the operator (or a cron entry) decides when a
  disk has come back and it is time to fill it. Scheduling re-scan and repair passes is
  P2, the same phase that owns scrub scheduling.
- **Ongoing verification of copies that are present is not reconcile's job either.** It
  stats each recorded location — one stat answers "is this copy absent", which is all it
  exists to ask — and leaves the read-back of present copies to `scrub`, the only place
  with an I/O budget for it. A copy that is present but unverified is not touched here.
- **Off-host replication is not what a floor of 2 buys.** The `--copies` floor replicates
  within (or across) the destinations on one host; it survives a disk failure, not the
  loss of the machine or a site. Remote/object/offline tiers are P3, and until they exist
  a floor of 2 is not a backup. More than one copy *per disk* is backup tooling too, and
  is out of scope (§6.1 of the issue).
- Block-level tiering (dm-cache/bcache/L2ARC): different layer, no per-file policy;
  out of scope but composes (a block cache in front of this is fine).
- Mounting an offline volume or streaming recall from an object store: `locate` reports the
  state that makes a copy unreadable and `restore` reads a mounted tier, but bringing a
  volume online is out of band (P2/P3) and a read command never mutates catalog or disk.
- **`restore` refuses a lexically-unresolved path rather than resolving it.** A path
  argument carrying a `..` component, or a link target that escapes every `--dest`, is a
  refusal, not something to `canonicalize` and then act on: canonicalizing to decide what a
  path "really" is would lose the symlink state `restore` exists to see, and would make the
  command act on a name the operator did not give it. The operator re-runs with the path as
  the filesystem spells it (§9, #70).
- Multi-user quotas/permissions: single-trust-domain system.
- **The catalog is not a tamper-proof trust store.** Row-path validation rejects an edited
  `location` row against the roots recorded by sync (or supplied to `audit`); protecting
  against an actor who edits both the location and the root table is outside the threat
  model. Protect the catalog file with the same access controls as the data it describes.
- **Tightening a catalog that already exists is not this guard's job.** #73 fixes how a
  catalog is *created* and *opened*: a fresh one is 0600, and a symlink at its name or a
  SQLite sibling is refused. A catalog an earlier version already created 0644 is left
  exactly as it is — silently chmodding a file the operator already has would be a
  surprising side effect of running `catalog sync` — and a caller who wants it private
  changes the mode once, by hand.

- **`openat2`/`RESOLVE_BENEATH` and sandboxing are not the containment mechanism.**
  #71 refuses a symlinked destination component with per-component `O_NOFOLLOW` and holds
  the resolved directory open for the writes that follow. That needs no minimum kernel
  beyond ordinary `openat`, keeps `unsafe` out of the move layer (the crate already routes
  Unix metadata through `rustix`, never raw `libc`), and draws the line at the
  already-present symlink the issue is about. Landlock/seccomp confinement of the whole
  process is a different, larger feature and is not implied by the fix.
- Backup *tooling* (dedupe, snapshots of the whole tree): this is a lifecycle engine;
  backup apps are consumers via the gateway.
- **Locking a watched file against future opens is not attempted.** The pre-move
  re-check (§9) narrows the open-descriptor window to the scan itself, but closing it
  completely would need a kernel "no one may open this next" primitive that does not
  exist. The residual gap is declared in §9 rather than hidden behind a check that
  cannot promise it.

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
   **As implemented (#140)**: the crossing form is `src/envelope.rs` — versioned header,
   fresh random per-object nonce, chunked XChaCha20-Poly1305 with the chunk index as
   additional data. Keys come from a file or the environment (never only argv), are
   zeroized on drop and never printed; the recorded digest stays the plaintext BLAKE3, so
   a remote copy verifies like a local one and "cannot decrypt" never reads as bitrot.
3. **Recall latency is honest.** A parked pool is not "fast when idle"; it is `recall =
   s`, and recall-aware consumers get that answer before the read is attempted.

**As implemented (#146).** `kind` is a parsed value, not free text: the four kinds named
above are recognised, and a kind no transport driver serves is refused with its line rather
than accepted and then treated as a filesystem. **As implemented (#141, #142, #143):** all
four kinds have drivers — `fs`, `object`, `peer` and `offline`. **As implemented
(#155 + #142 — the LAN-peer tier):** a `peer` tier speaks the same S3-compatible subset the
object-store client speaks — a peer runs `just_cache object-server` (the server half, #155),
and the `peer` driver (the client half, #142) connects to it. **As implemented (#143 — the
offline-volume tier):** an `offline` tier has a `path` (the volume's mount point, present
only while a disk is in the drive) and a required non-empty `vaults` list (the shelves its
volumes live on), and requires `encryption_key` like the wire tiers because the volume
leaves the host. A sweep exports each file to `<mount>/<path>` through the envelope, reads
it back and verifies it before retiring the source, and records an
`<volume-id>/<path>` location whose `volume-id` is the volume the catalog says is mounted in
the tier. There is no marker file in the volume: the catalog's `volume` ledger is the only
record of where a disk is (§3). A read that cannot reach the volume is refused with the
insert prompt — "insert `drawer-07`" — never a bare ENOENT. The boundary rule (rule 2) is the type's own answer,
`TierKind::crosses_machine_boundary()`: `object` and `peer` cross because the bytes leave the
host, `offline` because the volume does.

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

**As implemented (#46).** `[[cache]]` blocks are parsed beside the tiers in `tiers.toml`
but kept in a separate list, so no walk over tiers — `location`, copy floors, scrub,
policy — can yield one. Only `kind = "fs"`, `evict = "lru"` and `write_policy =
"write-invalidate"` are accepted; `writeback` is refused at parse with the reason above.
`over` must name a configured persistent tier, a cache's `path` must exist (never
created) and must not overlap any tier root, and a cache name may not reuse a tier's.
`mount` serves read-only opens through the overlay (`src/cache.rs`): a read that meets
`promote_on` copies the home's bytes into `<path>/.just_cache-overlay-<name>/` and is
served from the copy; LRU evicts until the new copy fits `max_size`, and a file larger
than `max_size` is never promoted. A write-open, truncate or rename drops the copy before
touching the home, and every hit re-checks the home's size/mtime/inode so a home changed
outside the mount is never served stale. A `--dest` overlapping an overlay and a policy
rule naming one are refused by name. Residency is mirrored into `cache_residency` for
observability only (`just_cache cache status`, every line marked ephemeral). On restart
the overlay is **emptied, not re-derived**: re-deriving would mean trusting bytes nothing
watched while the process was down, so the "residency survives a reboot" note above is
the deliberate cost of correctness for now (§9).

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
    tier        TEXT,                  -- the tier whose drive holds it while mounted; NULL otherwise
    note        TEXT
);
```

The `volume` ledger is the *only* record of a disk's whereabouts (#143): the tool never
writes a marker file into a volume, because the volume is exactly the thing that can leave
and whose filesystem cannot be asked. At most one volume may be recorded `mounted` per
tier, and a location's `storage_key` is `<volume-id>/<path>` so the recall path can prove
which disk a copy needs before it asks a person to insert it. `just_cache volume
list`/`set` is the operator's interface to the ledger; a `mounted` volume's bytes are
reachable at the tier's configured root, and a state other than `mounted` clears the
tier binding.

Why content-addressed ids: dedup comes free, a scrub can re-verify any copy from any
other copy, and restore-after-loss has a stable identity that survives path moves.
Path stays a namespace concept, not an identity.

Consequences:

- **"Does this file exist?" is a catalog question, not a filesystem question.** For an
  offline tier this is the only possible answer, and it must be actionable: a read of
  an offloaded object names the volume needed ("insert drawer-07") — never a bare
  ENOENT.
- **Deletion is a catalog transition** with reference counting across names and
  copies (#128, `catalog delete`, and `unlink` through the mount). Deleting a name drops
  its `name` row; the object's copies are released only when that was its **last** name,
  and then every `location` row, the `object` and its `lifecycle` go with it. An object
  with another name keeps its copies, losing only the deleted name's own entry in the
  watch tree (its hot file or its symlink). The rows and a `pending_removal` record of
  every released file commit in one transaction **before** any byte is unlinked, so a
  crash leaves either nothing changed or a catalog that no longer vouches for the bytes
  plus a durable list of what to finish — the next delete or `catalog sync` finishes it
  (re-hashing first, so a file rewritten since is kept) before it observes anything. A
  delete that cannot finish — the object is pinned, a released copy is marked damaged,
  its tier is not mounted, a row does not resolve under a recorded root — is refused
  before anything is written.
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

The gateway (`just_cache gateway`, #47) is WebDAV, not S3: `PROPFIND` lists the namespace
with size and lifecycle state (`jc:state` = `present` | `offloaded`), `GET`/`HEAD` with a
single `Range` reads a present copy, and `POST <path>?restore` is the restore request. A
`GET` of an offloaded object is answered `409` naming the restore request rather than
streamed from a slow tier — that is the "explicit restore semantics" above. A read mutates
nothing: the catalog is only read, bytes are opened read-only with `O_NOATIME` where the
kernel allows it (so a backup app reading everything does not make everything look hot),
and every write method is `405`. The restore request is the only thing that places bytes,
and it is `restore::restore` — copy, read back, hash against the recorded checksum — with
`--remove-copy` never set. Every request needs the token (`Bearer`, or the password of
HTTP Basic), read from `--token-file` or `JUST_CACHE_GATEWAY_TOKEN` and never logged; an
object any of whose copies sits on a root not given as `--watch`/`--dest` is `403` even
when the catalog knows that root. The HTTP layer is plain `std::net`, so the gateway adds
no dependency, and nothing but the `gateway` subcommand reaches it: the mover and the FUSE
mount neither link a server nor need one running.

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

As built (#44): `just_cache mount --recall <promote|read-through>`, **default `promote`** —
a read-only open of an offloaded name places a verified copy on the hot tier (the watch
root) and makes it the tier of record, so the next read is fast and the idle rule demotes
it again when it cools. `read-through` serves the tier of record and places nothing. A
TTL-cached copy is not offered as a recall policy: a `[[cache]]` overlay (§2.1) already is
the TTL-shaped copy, and it sits in front of whichever file the recall settles on. A tier
whose `tiers.toml` recall class is `min` or `hours` refuses: the open answers `EAGAIN`
(never `ENOENT`) and the mount's stderr names the tier and the `just_cache restore <path>`
to run. A tier with no `tiers.toml` entry has no class and is recalled.

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
6. **Plan the fault seam with the feature, not after it.** A feature whose failure
   window no test can time deterministically ships its injection seam *in the same
   change*, never as a retrofit once the window is discovered. The shape to copy is fixed
   by the seams in `src/faults.rs` and the tests in `tests/fault_injection.rs`: **one**
   env var, read once per operation and inert when unset, so a production process that
   never sets it takes no different code path; a value that is set but malformed
   **panics** rather than silently no-oping, because a switch that does nothing on a typo
   lets a bogus test pass green with no fault injected; and a control test that runs the
   same operation with the variable unset and asserts the inert path. It is test-only by
   construction rather than `#[cfg(test)]`, because integration tests drive the compiled
   binary and the seam must therefore live in that binary.

## 8. Phases

- **P0 — harden what exists** (symlink provider): journal + startup repair;
  preserve mode/owner/xattrs/sparse; do-not-move if the file is open or hardlinked
  elsewhere; `audit` command (cold copies without a symlink, symlinks without a target);
  CI test that exercises the EXDEV path. *(Done: scope controls per §5.1; metadata,
  sparseness and checksum adoption; the open-file/hardlink guard; `audit` + `--repair`;
  the journal and startup recovery; CI running the EXDEV path. P0 is closed.)*
- **P1 — catalog**: SQLite catalog ingesting the current mover's state (it already
  leaves an auditable pattern); `explain`, `locate`, `restore`; two-disk replication
  within a tier; scrubbing. *(Done: the catalog is the source of truth for where files
  live, ingested by `catalog sync` (#16); `explain`/`locate`/`restore` work through it
  (#17, #18); `audit` reads the catalog instead of walking both sides (#19); two-disk
  replication shipped as `sweep --copies N` with verify-before-delete, a recorded
  per-tier floor, and `under-replicated`/`replica-lost` reporting (#20); scrub repairs
  from a sibling (#21); reconcile rebuilds a missing replica (#24). The copy is
  same-host — see §9 and §10. P1 is closed.)*
- **P2 — FUSE namespace provider**: observe accesses properly; streaming recall;
  pins/restore semantics; gateway (S3/WebDAV) provider; cache overlays (§2.1) — a RAM
  promotion target first, since the same construct later fronts the HDD pool with SSD.
  *(Done: the catalog mounts as a FUSE namespace and accesses drive a lifecycle table
  (#42, #43); offloaded files recall inline on read, streaming into the caller (#44);
  pins and explicit restore are enforced (#45); a RAM cache overlay fronts a tier as a
  promotion target (#46); an S3/WebDAV gateway provider serves the catalog (#47);
  tiers and lifecycle rules configure via `tiers.toml` and `policy.toml` (#40, #41).
  P2 is closed.)*
- **P3 — remote tiers**: object-store driver (chunked, resumable, encrypted);
  LAN-peer driver; offline-volume driver with vault tracking and insert-prompt recall.
  *(Done: the tier edge became a driver seam that refuses an unsupported `kind` (#146); the
  encrypted envelope every boundary-crossing driver passes through (#140); the object-store
  driver (#141); the LAN-peer driver (#142); the offline-volume driver with vault tracking
  and the insert prompt (#143); and delete plus garbage collection on tiers that are not
  mounted (#144). The envelope follows the seam rather than leading it because the seam is
  what decides what "crosses the boundary" means, and rule 2 then makes encryption the seam's
  requirement rather than a wrap applied later. Each driver lands as a variant of the same
  `kind` dispatch; what each leaves open is in §9. P3 is closed.)*
- **P4 — cost-aware policy**: per-tier cost models, placement reports, rule
  suggestions from observed access.

Each phase ships something usable alone: P0 is a better standalone mover; P1 makes it
trustworthy; P2 removes the symlink breakage; P3 completes the S3 analogy; P4 is the
part nothing else does.

**Where the MCP server lands.** Exposing the catalog-backed commands to agents (#137) is
deliberately not a phase: nothing in P3 or P4 waits on it, so a phase slot would imply a
gate that does not exist. It was scheduled for **after P3**, because P3 is both what makes
it worth having and what settles its interface. Once a remote tier exists, "where does
this file live" is a question the filesystem cannot answer at all, and a recall is
something that can cost egress, a wait, or a human inserting a volume — exactly the
decisions an agent should be able to consult before it acts. Those are also the semantics
the tool surface has to express, and they do not exist yet: tool names and schemas are as
much a public API as the CLI is, and agents get pinned by the prompts they are written
into, so designing them ahead of the drivers buys a rewrite rather than a head start.
Waiting costs nothing in rework — the CLI's `--json` output is already the contract an
adapter would wrap. **Landed as `just_cache mcp` (#137), the first work after P3 closed**,
with the read-only tools as the default surface and `restore` behind `--allow-restore`;
§9 records what it decides and what it leaves open.

**Where the web dashboard lands.** A web front end over the same surfaces — a `just_cache
ui` server (#169–#172) — is, like the MCP server, deliberately not a phase: nothing in P4
waits on it. Its decisions are recorded here because they are policy, not presentation.
The trust model is `gateway`'s: a token from a file or the environment, never the command
line, loopback by default, plain HTTP behind a TLS terminator before it is exposed. The
exposed surface is the policy — the rule the MCP server set, applied to a browser:
read-only views by default, and a mutating action (a restore, a pin, a triggered pass)
exists only when the invocation named its `--allow-*` flag (#172). Live activity is not
invented: every maintenance pass appends the JSON report it printed to an events file
under the `.just_cache` prefix (#169), and the server streams that file over SSE — the
event a viewer sees is the report the CLI printed, never a second record of what
happened. The views are thin over the read-only commands' `--json` documents (audit,
explain, locate, the pin list, the volume ledger, the schedule), so a number shown in the
browser is one a cron alert would also have seen. The frontend ships as static files
embedded in the binary — no node build step — so the cargo-only build and the release
packaging stay true. It lands as four PR-sized issues in dependency order: the events
file (#169), the server (#170), the views (#171), the action surface (#172).

## 9. Known gaps (tracked, not hidden)

Each fix that lands adds a "Closed by #NN" paragraph to this section. A new entry is
**appended after the last existing entry, at the end of the list** — the
`Still open, and honestly so:` headings are fixed anchors that are never an insert
point. Using the heading as the anchor made every branch in a parallel wave insert at
the same line, so the second branch to land always conflicted on this file; appending
at the moving tail of the list means entries land in the order they merge, and a
branch that rebases onto landed entries re-appends after them. Two branches still
fanned off the same base do meet at the tail — the conflict, when it happens, is the
trivial "keep both, in order" one, resolved by appending in merge order rather than
re-inserting at the heading.

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
from it. What is deliberately out of scope is verifying copies that are present — a stat
per location answers "is this copy absent", and anything more is the scrubber's job.

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

Closed by #81: the in-flight partial is created `0600` explicitly rather than under the
caller's umask. The destination-resolution work above already opened it that way
(`DestDir::create_partial` uses `openat(..., O_CREAT|O_EXCL, 0600)`, and umask can only
clear bits 0600 does not have), so the module's "no reader ever sees the process umask's
guess at a mode" guarantee held; what was missing was the proof and an aligned doc. The
source's own mode is still applied only once every byte is in, just before the rename
publishes the name — the file bears the private partial name until then. The proof is a new
`JUST_CACHE_FAULT=partial-mode-mid-copy=N` seam: after N bytes it reads the partial's mode
back from the process holding it open, the only vantage point that can see the window before
the rename, and `tests/fault_injection.rs` asserts a cross-device copy of a 0600 source is
witnessed at a mode with no group/other bits and publishes 0600. It fails (mode 100644) if
`create_partial` is regressed to the process umask, and needs a second filesystem like the
other copy-path seams, because a same-filesystem copy is a reflink and never enters the
chunked reader the seam reports through.

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
- **Reconcile hashes every sibling candidate it considers; a read budget now bounds the
  pass.** The `verified` flag is not trusted as a source vouch (that is the whole point),
  so a rebuild reads its source once to prove it and once to copy it — the cost the pass
  states. `--read-budget` (a *total*, not `scrub`'s per-second `--rate`) caps the bytes one
  pass may read: an object the budget cannot admit is deferred and reported, and because a
  deferred object is left byte-for-byte unchanged, the next pass resumes at it. What is
  still open is the *number* of reads, not their total: every candidate that misses the
  checksum is read before it is rejected, so a directory of near-miss siblings costs more
  than the single read a match would.
- **Reconcile refuses a destination below the free-space floor a replicated sweep uses.**
  `--min-free-gb` (default 1.0, the sweep's value) is checked through the same
  `disk_management::destination_with_room` a sweep calls, before any read: a destination
  below the floor is left alone and reported, naming the root that would have been filled.
  The check is by the object's recorded length, which is all the catalog holds, so a sparse
  object whose allocated size is smaller may be refused conservatively (§10).
- **Reconcile trusts the catalog's state; its schedule is shared with scrub.** The command
  stays usable on demand, and `schedule.toml` can run it on its own cadence through the
  same mechanism as scrub (the Closed by #48 entry at the end of this list). It decides
  "does this object have a hot copy" from the `state` column, so a catalog left stale by a
  sweep (a sweep records replicas but does not rewrite state) makes reconcile see a
  `present` object and skip it — the same `catalog sync` that reports the
  under-replication brings the state current, and reconcile acts on the catalog as recorded
  rather than re-deriving it.
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
  copies. Off-host copies are what the remote drivers provide (§8).
- **A copy the mover could not verify is unknown, never good.** The mover counts only a
  copy whose read-back digest matched, and it does not retire the source until the floor
  is met; the catalog's `location.verified` is 0 until a sync hashes the bytes, and rows
  that stay unverified are reported as `replica-unknown`. The state a crash between
  "written" and "checked" leaves is that unknown row, and no floor counts it.
- **`--limit` under replication is per sweep, not per disk.** With `--copies N` the sweep
  replicates the same `--limit` candidates across all destinations; it does not fill each
  disk to its own limit first, because a floor is about one object living in N places, not
  about how many objects a disk takes.
- **A tree edited by hand is reported, not reconciled by default.** A name the catalog recorded
  that is gone or now hashes differently, and a location whose file vanished or was replaced,
  are reported — `sync` exits non-zero — and their rows are left exactly as they were; new
  names and locations that nothing contradicts are still ingested. That is deliberately louder
  than auto-healing in the wrong direction, and it stays the default. What used to be missing
  was a resolution step: deciding a vanished name was a rename might be a human command, but it
  did not exist, so a difference repeated on every sync. That command now exists (`catalog
  resolve`, #52) and concludes a rename, a delete or a replacement only where the evidence
  supports it; it is report-only unless `--apply` is given, so nothing is inferred silently.
- **The copy floor is recorded per tier; a catalog-mode `audit` still checks only the one-copy
  floor.** §6 wants a *per-tier* `copies` floor the scheduler maintains. The schema now holds
  one — the `tier` table's `copies` column, added with the table by #20 — and `catalog sync`
  records it: each configured tier's `copies` from `tiers.toml`, or an explicit `--copies N`,
  wins for every `--dest` root. A sync then reports an offloaded object with fewer verified
  copies than the recorded floor as `under-replicated`, naming the disks it is missing from,
  and the walk-based `audit --copies N` reports the filesystem-only view of the same shortfall
  as `replica-lost`. What is still single-copy is the *catalog-mode* audit itself: it hashes
  each object's copies against `COPY_FLOOR` (one surviving location) and does not read the
  `tier` floors, so an object whose every copy is gone is still `copy-floor`. Nothing schedules
  a replication pass to repair a shortfall — a sync reports the gap, it does not fill it, and a
  hot object is exempt because replication is a property of offloaded bytes.
- **`audit` verifies the copy of record, not every replica.** Hashing both a hot and a cold
  copy of a `restoring` object, or both copies of a replicated one, on every audit is
  the scrub §6 describes, and it would double the I/O an audit costs. Catalog mode hashes
  the primary location and only *stats* the others, so a corrupt non-primary replica is not
  caught by an audit; catching it is the scrubber's job.
- **Catalog-mode `repair` marks for resync in its report, not in a row.** There is still no
  durable per-row `needs_resync` flag. The copy floor's growth to a per-tier value did not
  need a versioned migration, because it landed as a *new table* (`tier`, which `CREATE TABLE
  IF NOT EXISTS` creates on the next open); a flag on an existing row is the case that needs an
  `ALTER TABLE` the current schema never performs. Until then the mark is the printed outcome
  plus the non-zero exit, and the operator runs `catalog sync` to re-record the tree.
- **Only the symlink provider's flat mirrored layout is understood.** Two-disk replication
  has shipped as `sweep --copies N` (across distinct `--dest` roots, with the per-tier floor
  above); replicating across multiple disks *inside* one tier (§2's `copies = 2`), remote or
  object tiers, and offline volumes are later work, and the `volume` table ships empty.
- **`lifecycle` keeps the rule, but its usage columns are ingest-only.** A sweep that moves a
  path the catalog already names writes the deciding rule to `lifecycle.rule`
  (`Catalog::record_lifecycle_rule`, #41), so the "which rule fired" half is maintained.
  For an object accessed through the mount, `last_access` and `accesses` are maintained
  by the provider since #43 (see the `Closed by #43` entry below); for one only ever reached
  through the symlink provider, `last_access` is still the atime ingested at first sync and
  `accesses` stays 0, because that provider sees no opens to count. (`pinned_until` is the exception since #45: `pin`/`unpin` write it, so it is a
  maintained column rather than an ingest-only one — see the `Closed by #45` entry below.)
  The `state` column is likewise derived at sync from whether a hot and a cold copy
  both exist, not written by `restore`: a completed restore only becomes `present` after the
  next `catalog sync`, and until then `audit` tells a restore-in-progress from a true duplicate.
- **A same-size, same-mtime rewrite is invisible to the digest cache.** A sync keys a
  file's digest on `(size, mtime, inode)` (#50, below), so a rewrite that preserves all
  three — an in-place edit of exactly the same length with the mtime restored — returns
  the stale digest and the sync reports no difference. `catalog sync --force-rehash`
  reads every file again for exactly that case. Named rather than hidden: the cache is an
  optimization, and this is the one change it cannot see.
- **Journal records carry a size, not a digest.** Recovery refuses to link a name to a
  copy whose size does not match what the move promised, which is the strongest check
  available without hashing every file before every move. A same-size-but-corrupt copy
  would still be linked; the catalog's digests (P1) are what close that, and the mover
  already checksums destinations it *adopts*, so the hole is narrow and named.
- **A single-copy move does not hash what it wrote; a replicated copy is read back.** The
  plain sweep's torn copy is caught by the short-copy and source-changed guards, not by
  re-reading the freshly written file — re-reading a 40 GB copy doubles I/O — so a
  single-copy offload still leaves byte verification to `scrub` (#21), which reads stored
  copies back on a schedule the operator controls and inside an I/O budget. With
  `--copies N`, though, each *placed* copy is read back and hashed against the source before
  it counts toward the floor (`replication::place`): a copy that only returned from write is
  not a copy the mover can vouch for, and an unverified copy never retires the source. The
  old half of this note — "hashing a sparse file means materializing its holes" — was wrong
  about *reading*: it is a non-hole-preserving *copy* that materializes holes, and
  `copy_contents` already preserves them. `tests/scrub.rs` measures it.
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
- Access tracking depends on atime semantics of the host mounts (`noatime`, ZFS). What
  counts as use is a read by someone other than the tool: the tool never marks its own
  use. Every internal read-only open — `catalog sync`'s hash, `scrub`'s verify, `audit`'s
  probe, the mover's and replication's comparison read-backs — goes through
  `digest::open_for_hashing`, which opens with `O_NOATIME` and falls back to a plain open
  on `EPERM` (the tool does not own the file; that read can still refresh atime under
  `relatime`, and is accepted over failing the hash). Deliberately excluded: the mover's
  copy of a source it is about to delete, and `restore`'s copy back from a cold tier, which
  is the user asking for the file. The reproduction lives in `tests/noatime_hashing.rs`
  (#129).
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
- **Scrub now has a schedule; the resume rule is what it relies on.** `just_cache scrub`
  remains an on-demand command, and `schedule.toml` plus `just_cache schedule --run` (from
  cron) is how it runs on its own cadence — the mechanism is the Closed by #48 entry at the
  end of this list. A location verified once is still skipped until its content changes
  under the catalog (or the damage record for it is cleared) — unless it is the only
  candidate repair source for a group that has rot, in which case it is re-read and a
  failed read is marked damaged (#75, above) — and a scheduled scrub carries the configured
  `rate`, so a cron-driven pass keeps its budget against a tier that is serving reads.
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

Closed by #40: a tier is now the configured object §2 describes, not a root path wearing
its name. `tiers.toml` — read from `--tiers <FILE>`, or from `tiers.toml` beside the watch
root **only when it is already there** (invariant 9: nothing is created in the watched
tree that was not asked for) — names each tier and carries its `kind`, `path`,
`volatility`, `recall`, copy floor `copies`, and optional `cost`. `locate` and `audit`
print those names instead of raw paths, and `locate` states a copy's recall class even
though nothing acts on it yet; `audit` prints the configured tiers once and renders a
finding's tier by name. A `volatile` tier is refused as a destination root and as a policy
target — §2.1, a volatile tier is a mirror, never a home — by `sweep`, `audit`,
`catalog sync`, `explain`, and `restore` alike, naming the tier in the refusal. `catalog
sync` records each configured tier's `copies` as the floor the `tier` table already held
(#20), with an explicit `--copies` still winning. An explicitly named `--tiers` file that
is missing or malformed is a usage error that names the line, never a silent fallback;
with no file the tool behaves exactly as before — a destination root's path is its own
tier name, and `locate`'s output is byte-for-byte what it was. What the config deliberately
does not yet do is named rather than implied: nothing acts on `recall` or `cost` beyond
reporting them, cache overlays (§2.1)
are a later issue, and only the `fs` driver exists — `object`, `offline`, and `peer` tiers
parse and are named but no driver moves bytes for them. `tests/tiers.rs` drives the real
binary for the accepted behaviours: two tiers named in `locate`/`audit` output with their
recall classes, a volatile tier refused as `--dest` by both `sweep` and `catalog sync`, a
malformed config naming its line (a syntax error and a bad enum alike), and no config
meaning today's behaviour and creating nothing.

Closed by #41: lifecycle rules are evaluated. `policy.toml` — read from `--policy <FILE>`,
or from `policy.toml` beside the watch root **only when it is already there**, the same
open-if-present rule `tiers.toml` uses — holds `[[rule]]` tables, each with a `match` glob,
a `down = { after_idle, from, to }` transition, optional `pins`, and an optional
`up = { on_access }`. Rules are resolved per file against the tier the file sits on: a pin
wins over policy, the *last* matching rule that names the file's tier wins over an earlier
one (so a per-path rule overrides a catch-all), and only then does the idle gate apply. A
rule's `to` tier must be a configured tier that is also a `--dest` root this sweep was
given, and `down` must go *slower*; a rule naming an unconfigured tier, a `volatile` tier
(§2.1), an unparseable duration or an `up` with no `down`, or a `policy.toml` with rules
but no `tiers.toml`, is a usage error naming the rule — never a silent skip. A transition's
rule is written to `lifecycle.rule` when the existing catalog already has that namespace
name, and `show`/`explain` name it, so `just_cache explain <path>` answers "which rule fired
or which exclusion stopped it", including "no `policy.toml`" when there is none — with no
policy the flag-driven `--min-idle-days` decision stands unchanged. A sweep never creates a
catalog (invariant 9): if there is no catalog row to update, the rule is still reported for
the move and a warning tells the operator to run `catalog sync`. A rule's `to` is what
routes the move: with a policy the sweep no longer pours candidates into the fastest disk
with room, it sends each file to the tier its rule names (`--copies` still replicates across
the destinations).
Two deliberate bounds are named rather than implied: a multi-step chain (`ssd →
hdd_parked → offsite`) is written as one rule per step, because the §5 snippet above
repeats the `down` key and TOML forbids that — a later issue may accept a `down` array;
and `up` is *reported*, not performed, because promotion is the recall path's job (§8, P2).
`tests/policy.rs` drives the real binary for the accepted behaviours: a rule moving on its
idle gate with the rule recorded and named by `explain`, `up.on_access` named for an
offloaded path, dry-run making the same decision without moving bytes, a pin protecting a
file, a per-path override beating a catch-all, a config file in the watched tree left
alone, an explicit `--policy` naming a file elsewhere, no policy meaning the flags still
decide, and refusals (unconfigured tier, volatile target, bad duration) naming their rule.

Closed by #39: the fault-injection seam is now discoverable from `AGENTS.md`. Its
build/test/verify section already warned that a skipped test looks like a pass; beside that
warning it now names the convention — the `JUST_CACHE_FAULT` env var, its `mechanism=N`
grammar and the one-shot claims parsed in `src/faults.rs`, the seams it arms in
`src/replication.rs` and `src/disk_management.rs`, the deliberate panic on a set-but-malformed
spec (a no-op switch would let a mistyped test pass green with nothing injected), and
`tests/fault_injection.rs` as the worked example. No command already documented in `AGENTS.md`
is restated; the pointer works from the repo alone.
Closed by #38: the mandate to plan a fault seam with its feature is now rule 6 of §7, stated
as a rule rather than left implicit in the two retrofits that taught it. Both deterministic
seams in this repository — #25's replication hook and #36's mid-copy hook — arrived *after*
the feature they test, and each is explained in this section by what it simulates rather
than by the shape the next seam must follow. Rule 6 makes that shape binding at design time:
one env var read once per operation and inert when unset, a malformed value that panics
instead of silently no-oping, a control test that asserts the inert path, and test-only by
construction rather than `#[cfg(test)]` because the integration tests drive the compiled
binary. It points at `src/faults.rs` and `tests/fault_injection.rs` (whose
`the_hook_is_inert_when_unset` is the control test) as the concrete pattern, so the rule is
actionable from the repository alone. No code changed; retrofitting the two existing seams
is a §10 non-goal.
Closed by #54: the open §9 entries were checked against the code and the six the code had
outgrown were corrected in place. The copy-floor entry no longer says the floor is absent
from the schema: `tier.copies` (`catalog.rs`) holds it, `catalog sync` records each
configured tier's `copies` or an explicit `--copies N` (`main.rs`) and reports a shortfall
as `under-replicated` (`Catalog::under_replicated`), and the walk audit's `--copies N`
reports `replica-lost` (`audit::audit_with_copies`) — only the catalog-mode audit's
`COPY_FLOOR` of one is unchanged. The resync entry's premise that a durable flag "would
need the same migration the copy floor needs" is gone; the floor was added as a new table,
which `CREATE TABLE IF NOT EXISTS` reaches, whereas a column on an existing row would need
an `ALTER TABLE` the schema never performs. The layout entry no longer calls two-disk
replication later work (`sweep --copies N` ships it); replicating across multiple disks
*inside* one tier, remote/object tiers and offline volumes still are. The `lifecycle` entry
now records that `lifecycle.rule` is written by a transition while `last_access`/`accesses`
stay ingest-only and `state` is derived at sync. The read-back entry is scoped: a
single-copy move does not hash what it wrote, a `--copies N` copy is read back before it
counts (`replication::place`). One aside in the copy-of-record entry (`once that exists`)
went with it. The remaining entries were checked and left as they stand — still true in the
code, or deliberate limits that stay recorded (the fault seam's reach, the atime→mtime
fallback, the hard-link-at-the-journal-name gap, root-owned files, the absent catalog-only
audit and digest cache, the per-root journal, single-copy catalog audit, and the
reconcile/scrub scheduling bounds).
Closed by #53: a `catalog sync` no longer aborts on the first unreadable file. Each file
whose bytes could not be read is reported per file — its path and the error, under an
`unreadable:` line kept apart from `differences:` — and the rest of the tree is ingested in
the one transaction, so the cost of one locked file is a named hole, not an un-built
catalog. A sync in which any file failed exits non-zero (rule 7 applied to the catalog),
exactly as one with a difference does; a clean sync still exits `0`. The pass infers
nothing from the failure: the file is recorded neither as vanished nor as a missing
location nor as empty — "could not read" is the only thing said about it — so the catalog
can never claim a deletion it did not see. A later sync, once the file can be read, ingests
it and the finding clears. `tests/catalog.rs` drives the real binary for both halves: one
locked file beside a readable one (the readable file ingested, the report naming the locked
one, a non-zero exit, then zero after `chmod` restores readability and the file is
ingested), and a file already in the catalog going unreadable (reported unreadable, *not*
re-reported as name-vanished or location-missing, its rows kept). Deliberately not done, as
§9 already names: a privileged re-read for a root-owned file a non-root sync cannot open.
Closed by #55: `audit --json` carries the scrub section. When a catalog answered, the
document now holds `"scrub":{"locations":N,"verified":N,"never-scrubbed":N,"damaged":N}` —
the same four counts the readable `scrub:` line prints, read once in `run_audit` and carried
by the report (`audit::ScrubSection`, `AuditReport::with_scrub`) so the human line and the
JSON object cannot drift: there is one query and one struct behind both, not two renderings
that a test would have to keep in step. The addition is additive — every existing JSON key
is unchanged and the document still parses. Without a catalog the key is `"scrub":null`, not
four zeroes: a walk-based audit read no stored bytes, and a zeroed object would read as
"everything verified", the exact claim the section exists to avoid. One behaviour grew on
the readable side for the same reason: a catalog-mode audit now prints its `scrub:` line
whether the catalog was named with `--catalog` or found at the default path, because both
renderings read the report's section rather than reopening the file on their own.
`tests/audit_catalog.rs`'s `json_carries_the_scrub_counts_the_readable_output_prints` parses
both renderings for the same catalog — before a scrub (nothing verified) and after one
(every copy verified) — and asserts the counts agree and move, so it cannot pass on a
hard-coded zero in both; `json_scrub_is_null_for_a_walk_based_audit` pins the no-catalog
`null`. A unit test in `audit.rs` pins the object's exact shape.

Closed by #49: `reconcile` now carries the two pre-flight gates §9 named. Before a byte is
read for a rebuild, `destination_with_room` — the exact function a replicated sweep calls —
answers whether the destination is above `--min-free-gb` (default 1.0, the sweep's default);
a destination below it is refused and reported as `no room to rebuild`, naming the root that
would have been filled, rather than being filled and failing the copy later. A
`--read-budget` (human sizes: `512`, `64KiB`, `2GiB`; omitted means unlimited) is a *total*
cap on a pass, deliberately not `scrub`'s per-second `--rate`: every read the pass plans —
proving a sibling, hashing a copy already present to decide adopt-or-conflict, and the copy
itself — is admitted against it *before* it happens, so the pass cannot start a read it
cannot afford. An object the budget cannot admit is reported as `deferred` and left
byte-for-byte unchanged; with no scrub-state row to mark it skipped, the next run simply
finds it again, and the summary states the cost ("a rebuild reads its source once to prove
it matches the recorded checksum and once to copy it") and how much of the budget was spent.
Both gates only ever add records, so the exit-code contract is untouched: a gated pass is
still `1` ("something was found"), never a clean `0`. `tests/reconcile.rs` drives the real
binary for both — a destination under a floor far above any available space is refused with
its root named and no copy written, and a pass stopped by a 1 KiB budget reports the object
as deferred and leaves it absent, so a following pass with room rebuilds it — and both tests
fail if the gates are removed. What is deliberately left to `scrub`, and named in §10, is
*pacing*: a budget and a rate answer different questions.

Closed by #48: scrub and reconcile now have a schedule of their own. `schedule.toml` — read
from `--config <FILE>`, or from `schedule.toml` beside the catalog **only when it is already
there**, the same open-if-present rule `tiers.toml`/`policy.toml` use — holds one table per
pass (`[scrub]`, `[reconcile]`), each with a required `every` and optional `rate` (scrub
only) and `min_free_gb`; an unknown key is an error, not a pass nobody scheduled. The
mechanism is a config file plus a cron contract, not a daemon: cron runs `just_cache schedule
--run` and the tool decides whether anything is due, so `every` is how often a pass wants to
run and cron's own period is how often it checks. `just_cache schedule` (no `--run`) is the
read-only answer to "what runs next": it names each pass's cadence and prints either `due now
(last run <time>)` or `next run <time>`, so the operator sees the plan without running
anything — the next-run decision is a pure function of an injected clock, so it is testable
without waiting for a schedule window. Last-run times are kept in a small
`.just_cache-schedule.state` file beside the catalog — named under the `.just_cache` prefix
the walk skips (invariant 8) and written only by an explicit `--run` (invariant 9) — rather
than in the catalog, because "when did this pass last run" is not a fact the catalog records:
scrub's `verified_at` is per location, so a run that skips every verified location would move
no timestamp. A scheduled pass carries its own budget: the scrub in `schedule.toml` is the
same `--rate` a manual scrub would use (pinned by pacing a 256 KiB read at 512 KiB/s through
the binary), and each pass is **held back** — reported and left due, never started — while
any root the catalog records is below its `min_free_gb` floor, because a scrub can write a
repair and a reconcile a rebuild. The floor is a schedule-level pre-flight rather than a new
gate inside `scrub`/`reconcile`: "is this a safe moment to start a writer" is a scheduling
decision, and the reconcile command's own free-space gate is still open (above). Find-or-quiet
is the exit contract: a due pass that finds something prints its report once and exits `1`
(the house code a manual pass uses), a pass that finds nothing prints nothing, and with no
`schedule.toml` nothing runs at all — `just_cache schedule` says so and exits `0`.
`tests/schedule.rs` drives the real binary for these: nothing-configured runs nothing, the
visible next-run, a repair reported once and then quiet, the configured rate pacing the read,
and a floor holding a pass back without verifying a byte; the `schedule.rs` unit tests cover
the next-run calculation, the config refusals naming their line, and a corrupt state file
reading as never-run.

Closed by #52: a tree edited by hand is no longer a difference that repeats forever.
`just_cache catalog resolve --watch … --dest … [--apply]` re-observes the tree and the tiers
exactly as `sync` does and concludes a difference only where the evidence is a *surviving
reference the catalog already records*: a vanished name is dropped when the object is still
named elsewhere in the tree (the rename/duplicate case, the file at the new name hashing to the
recorded digest), a vanished location is dropped when the object survives as another name or
another copy (a deleted replica with a sibling left), and a name whose path now holds different
bytes is repointed only when the object it used to name survives named elsewhere. It is
report-only unless `--apply` is given, so the default stays report-not-heal and a cron job can
run it just to see the plan. Everything the evidence does not settle is reported as
irreconcilable and left alone: an object with no surviving copy keeps its rows (invariant 6),
and a replacement whose old object survives nowhere is refused because repointing would drop
the last record of it. No file is ever deleted; only catalog rows move, inside one transaction,
and a row is never dropped if that would leave an object without its last reference. Two bounds
are named rather than hidden. First, `resolve` consumes what a `sync` already ingested and does
not ingest itself — run `catalog sync` first, its non-zero exit is the report — so a surviving
name the tree shows but the catalog never recorded is not a conclusion this command will make.
Second, a vanished name whose object survives only as an *unnamed* cold copy is refused, because
dropping the name would orphan that copy and turn one reported difference into another; the
operator restores the link or adopts the copy by hand. `tests/resolve.rs` drives the real binary
through a rename, a deleted replica, a replacement that is repointed and one that is refused, an
irreconcilable delete, a report-only pass and a clean tree; the three resolution cases were
checked to fail with the drop/repoint step removed, which is what makes them coverage rather
than decoration.
Closed by #50: `catalog sync` no longer hashes every file on every run. It keeps a digest
cache in the catalog's own `digest_cache` table keyed on `(size, mtime, inode)`: a file whose
identity still matches its recorded digest is not read, and only a changed or unseen file is
hashed and re-recorded. The shortcut is never silent — a sync's summary now prints
`digests: N hashed, M trusted from the (size, mtime, inode) cache`, and `SyncReport` carries
both counts for callers. The cache lives in the catalog (under the `.just_cache` prefix, or
beside `--catalog`), has no foreign key to `object`, and is written only after the ingest
commits, so it is never a second source of truth: deleting the table — or the whole catalog —
costs the next sync its time and changes nothing else, which
`a_second_sync_trusts_the_digest_cache_instead_of_hashing`,
`touching_a_file_makes_the_next_sync_hash_it_again`, and
`losing_the_digest_cache_costs_time_not_correctness` pin (each fails if the trust branch is
removed). The risk the key hides is named above: a rewrite preserving size *and* mtime *and*
inode is not detected, so `catalog sync --force-rehash` re-reads every file on demand and
rebuilds the cache as it goes. Deliberately out of scope: a scrub still reads every byte by
definition, and an audit's hash is a verification rather than the same "is this the file I
last saw" question — neither consults the cache.
Closed by #51: `audit --no-filesystem` answers from the catalog rows alone — no walk, no
`stat`, no hash, no tier access — for a catalog whose tier is not mounted (or a host where
the tree is gone). It reports what the rows can honestly say: the recorded object/name/
location counts, the recorded per-tier floors, an object whose *recorded* locations fall
below the floor for its copy-of-record's tier (`under-replicated`), a location a scrub
marked damaged (`damaged-copy`), a row that could never be joined into a path
(`malformed-catalog` — the check is lexical), and the scrub-state summary, all in a
`no-filesystem` object in `--json` and a readable block that prints only the classes it
could check. It states out loud the findings it *cannot* make, from
`VerdictKind::filesystem_only`: `missing-copy`, `checksum-mismatch`, `copy-floor`,
`name-vanished`, `unknown-path`, `unknown-version`, `dangling-symlink`, `unexpected-target`,
`duplicate`, `orphaned-copy`, and `replica-lost` — every one needs the tree — as an `unchecked`
list in the JSON and a `cannot be checked without the tree:` line in the summary, precisely so
a `0` is not read as "checked and clean". `--watch`/`--dest` are labels only in this mode and
are never touched (the path checks every other mode runs are skipped), the source is named
`catalog-only`, exit codes are unchanged (0/1/2), and it is read-only: nothing is written to
the catalog or the tree, and `--repair` marks for resync exactly as catalog mode does.
`tests/audit_catalog_only.rs` builds a catalog, deletes the watch root and every tier root,
and asserts the answer still comes back — which is what proves the abstention, since a
command that validated or walked those paths would exit 2. The mode deliberately does not
verify whether recorded copies still exist; that is the whole limit it names.
Closed by #42: `just_cache mount <MOUNTPOINT>` mounts the catalog's namespace as a FUSE
filesystem, so a consumer that does not follow symlinks (rsync/backup defaults, some SMB
clients, qBittorrent verify) sees real bytes for an offloaded file instead of a link — the
§4 provider that the symlink mover is the fallback for. The provider is split in two so the
part CI can check is testable at all. `src/namespace.rs` is provider-agnostic: it indexes the
catalog's `name` rows into a prefix tree (directories are implied prefixes; only files are
named), resolves a path to its **tier of record** — the object's primary `location` — and
turns that row into a filesystem path through `resolve_location_path` against the roots
`catalog sync` recorded, so a hand-edited catalog can never point the mount outside a trusted
root (#72). A named path whose row is missing or malformed is `NamespaceError::Unresolvable`,
never `NotFound` and never a zero-length file: a directory the catalog says holds files cannot
read as empty just because a row could not be joined into a path. `src/fuse.rs` is the thin
adapter: it maps inodes to namespace paths (`&self` methods behind mutexes, as fuser 0.18
requires), serves a directory as the catalog's children merged with the real entries under the
watch root so `mkdir`/`create` behave normally, and answers a file from the bytes at its tier
of record. A **read** is a pure read of those bytes and can never touch a row. A **write**
goes through to the tier of record's file in place, and a **create** lands under the watch root
(the hot tier); the mount invents no second placement mechanism — no partial, no journal, no
tier choice — and it does not rewrite the catalog, so a rewrite leaves the recorded checksum
stale until the next `catalog sync` observes it exactly as it would any other rewrite. Fail
closed is enforced before and during serving: `mount` requires an existing catalog (a mount
over none would present an empty namespace), refuses a catalog with no recorded roots, refuses
a mountpoint that is missing or non-empty, answers `EIO` for a name whose bytes are gone (an
unmounted tier) rather than reporting an empty file, and answers `EIO` if `create` would shadow
a catalogued name. A dead daemon is the kernel's own `ENOTCONN` — the mountpoint cannot read as
an empty tree — and `Ctrl-C`/`SIGTERM` unmount through fuser's `SessionUnmounter` before the
process exits, so no live mount is left behind. `fuser` is built with `default-features = false`
(the pure-Rust `/dev/fuse` path), so a build host needs no libfuse headers — CI's image has
none — and no other subcommand links FUSE, so a sweep/audit on a host without the kernel module
still builds and runs. Tests: `tests/fuse_mount.rs` exercises the namespace layer against a
real catalog with **no mount** (name→tier-of-record resolution for a present and an offloaded
file, an unknown path as `NotFound`, an absolute key and a deleted location row as
`Unresolvable`, and that resolving names rewrites neither the catalog nor the tiers), and the
`serve` refusals that happen before any kernel call (no roots, non-empty mountpoint, a missing
catalog as a usage error). The real mount — bytes served through the handler, a write through
to a tier file, a rename, a clean unmount — is gated behind `JUST_CACHE_TEST_FUSE=1` and is
**not run by CI**, which has no usable `/dev/fuse`; everything only a real mount proves is
therefore verified by a human running it with the gate set, and is named rather than implied.

Closed by #45: `lifecycle.pinned_until` is written by commands now. `pin <path> --until
<when>` sets it, `unpin <path>` clears it, and `pin --list` prints every pin, live or
lapsed, with the instant it lapses; `--until` takes a duration from now (`30d`) or a Unix
timestamp in seconds. A sweep reads the pins from the catalog beside the watch root
(opened only if it already exists, so a sweep never creates one), and a live pin is a skip
in its own right — `SkipReason::PinnedUntil`, reported as `pinned in the catalog until
<instant> (a pin wins over policy)` — checked before the observed-access pin and before the
rule, and re-checked immediately before a move so a pin set mid-sweep still holds. An
*expired* pin blocks nothing: it is shown as `pin expired <instant> (blocks nothing)`, both
in `pin --list` and in `explain`'s catalog note, and `explain` names a *live* pin in its
verdict the way it names a rule, with a `pin_live` field in `--json`. `restore` now searches
the catalog's recorded locations in addition to the link target and the mirrored `--dest`
path, resolving each through the catalog's path guard (#72), so an object on a tier this
invocation was not given as `--dest` is reached; `restore` stays idempotent and verifies
against the recorded checksum on the catalog-backed path, and `--remove-copy` may drop a
copy under a catalog-recorded tier root once the restored file verifies. Exit codes follow
the house contract. `tests/pins.rs` proves a pin blocks a sweep, an expired pin releases
it, `explain` names a live pin and shows an expiry, and `unpin` is idempotent — the blocking
test was shown to fail with the mover's pin check removed. `tests/restore.rs` proves a
restore reaches a copy on a catalog-recorded tier that neither the `--dest` roots nor the
symlink target can serve. A tier named in `tiers.toml` that no `catalog sync` has recorded
yet is still not consulted by `restore`: the catalog is where a location is known, and a
root it has never seen is not one it can vouch for.

Closed by #129: the tool's own hashing reads no longer count as use on a `relatime`
mount. Internal read-only opens use `O_NOATIME` with an `EPERM` fallback, so a sync is never
the "last access" the next sweep reads. The test does not depend on the mount — on the
`noatime` box the pollution is invisible — so `JUST_CACHE_FAULT=relatime=1` stamps atime
after any internal open that lacks the flag, the deterministic form of #124; removing the
flag fails both tests in `tests/noatime_hashing.rs`. Gap: a file the tool does not own
still takes the plain-open fallback and can be refreshed by the read; that path is not
exercised end to end, because the suite does not run as a second user.

Closed by #46: a `[[cache]]` overlay is a promotion target, never a tier of record. The
config is parsed and validated beside the tiers (`writeback`, a non-`lru` eviction, an
`over` that is missing or volatile, a path overlapping a tier root, a reused name are each
refused with the line); `TierSet::tiers` never yields a cache. Promotion copies bytes into
an overlay-owned directory and moves nothing; LRU eviction keeps the overlay at or under
`max_size`; a write, truncate or rename through the mount drops the copy first; a hit
whose home changed behind the mount is dropped and re-read from home. A cache is refused
as a `--dest` (on every subcommand that checks volatile tiers) and as a policy `from`/`to`.
Losing the overlay is not data loss: no journal entry, no scrub state, no `location` row,
and the overlay reopens empty, clearing its `cache_residency` rows. `tests/cache_overlay.rs`
covers promotion, LRU eviction at `max_size`, write-invalidate, stale-home detection,
reopen-empty, the `--dest` and policy refusals, and `cache status` marking residency
ephemeral. Still open, and named: the FUSE call sites (`open`/`setattr`/`rename` calling
`invalidate`) are exercised only through the library API, because the real-mount test is
gated on `/dev/fuse`; a writer that opened its handle *before* a concurrent promotion is
caught only by the size/mtime/inode re-check on the next hit, so a same-size rewrite within
one mtime tick could be served stale until the next invalidating call; and residency does
not yet survive a restart (the overlay is emptied instead).

Closed by #47: the gateway provider. `just_cache gateway --watch W --dest D --token-file F`
serves the catalog over WebDAV (`src/gateway.rs`, see §4): listing and metadata by
`PROPFIND`, single-range reads of present copies, and `POST <path>?restore` through the
verified restore path. `tests/gateway.rs` drives it over real HTTP against a real catalog:
a listing and a range read of a present object that leave every catalog row and every byte
in both roots unchanged; a restore request that turns the offloaded symlink into a verified
regular file and leaves the cold copy; a restore from a rotted cold copy that is refused
and places nothing; missing, wrong-Bearer and wrong-Basic credentials answered `401`; an
object on a tier the gateway was not configured with refused `403` for both listing and
restore; and the CLI's startup errors not containing the token. Still open, and named: a
gateway restore writes no catalog rows, so the catalog keeps calling the object offloaded
until the next `catalog sync` (the gateway serves the restored hot file meanwhile); the
server handles one connection at a time and speaks plain HTTP only, so it belongs on
loopback or behind a TLS terminator; there is no S3 API, only WebDAV; and `O_NOATIME` is
best-effort — a file the gateway user does not own is read with a plain open, which can
update atime and so the mover's usage signal.

Closed by #43: the FUSE provider observes access and maintains `lifecycle`. Every open
through the mount adds one to `lifecycle.accesses` and stamps `lifecycle.last_access`; a read
or a close refreshes the stamp without counting (`read` calls track a consumer's buffer size,
not its use, so an access is an open). The bookkeeping lives in `src/observe.rs`
(`AccessLog`), buffered in memory and written in one transaction per flush by
`Catalog::record_observed_accesses`, which moves `last_access` only forward (`MAX`) and writes
nothing but `last_access`, `accesses` and a new `lifecycle.observed` flag — an access is an
observation, never a transition, so `rule` and `pinned_until` are untouched. The flag
(`ALTER TABLE` on open for an older catalog, default 0) is what lets every reader tell a
provider-observed stamp from an ingested atime. **No path consults atime for an observed
object**: a sweep judges it by `lifecycle.last_access` (read once per sweep beside the pins),
and the atime→mtime fallback survives only for objects never accessed through the mount —
the symlink provider's view. `explain` shows the source as `via provider` and the catalog
note says the count was observed by the mount provider (counted opens, not a timestamp);
the noatime caveat is not printed for such a stamp. **The loss bound, named**: a flush
happens on every close, on any open or read at least `FLUSH_INTERVAL` (5 s) after the last
flush, and at unmount, so a `SIGKILL`ed daemon loses at most the observations since the last
flush — opens and reads of files still open at the kill, plus at most 5 s of activity on
files never closed; a completed access (its close reached the daemon) is never lost, and a
failed flush keeps its batch for the next one. **Write-invalidate**: a write or rename
through the mount writes no `lifecycle` column and is not a transition; the overlay copy is
dropped at the `open`-for-write/`setattr`/`rename` call sites #46 added (§2.1), and a read
served from an overlay copy is observed exactly like one served from home. Tests: `tests/access_observation.rs`
drives `AccessLog` against a real catalog with no mount — an access updates the row, a second
increments it, a late flush never makes an object colder, an unflushed observation is not in
the catalog (the bound), an unsynced name writes nothing — and proves atime is not read by
contradiction: a file stamped 400 days idle stays put after one observed access while a
control with the same atime moves, and `explain` names `provider`; the test was shown to fail
with the sweep's provider-stamp lookup removed. That the FUSE handlers call the log is
asserted only in the `JUST_CACHE_TEST_FUSE=1` mount test, which CI does not run. What to *do*
with an observed access is the policy engine's (§5), not this change's.

Closed by #128: an object can be deleted through the catalog. `just_cache catalog delete
<PATH> --watch W` and `unlink` through the mount both call `Catalog::delete_name`
(`src/catalog_delete.rs`), which in one `IMMEDIATE` transaction drops the name row and —
only when it was the object's last name — every location, scrub and damage row, the
lifecycle row and the object; it records every file it released (the cold copies, the hot
file, the name's symlink) in a new `pending_removal` table in the same commit, and unlinks
only after the commit. Reference counting is by object: with another name left, only the
deleted name's own hot copy is released, and a delete that would leave the remaining names
with no copy is refused. Refusals are decided before any write and named: not catalogued,
pinned, a released copy marked damaged, its tier not mounted (root directory absent), a row
that does not resolve under a recorded root, a `--watch` that is not a recorded root.
**Crash recovery**: a process that dies between the commit and the unlinks leaves rows in
`pending_removal`; `catalog sync`, `catalog resolve --apply` and the next delete finish them
before observing the tree, re-hashing each regular file first, so a file rewritten at a
released path since is kept rather than removed on the strength of an old transaction.
Without that step the next sync would re-ingest the leftovers — the deleted name back, its
cold copy as an orphaned object. Through the mount, `unlink` of a catalogued name performs
that transition, forgets the name from the mount's index (now behind a lock — the one
exception to "indexed once"), and drops any overlay copy *after* the commit, so a refused
delete keeps its cache; refusals map to `EPERM` (pinned) or `EIO`, the reason on stderr. An
uncatalogued regular file is unlinked plainly; an uncatalogued symlink is refused (`EROFS`),
because removing it would orphan a cold copy no row records. `rmdir` answers `ENOTEMPTY`
while a catalogued name lies beneath the directory. Tests: `tests/catalog_delete.rs` deletes
one of two names, the last name, both names in turn, each refusal (rows and bytes
unchanged), and opens the crash window with `JUST_CACHE_FAULT=delete-after-commit=1`
(aborts after the commit): the catalog no longer locates the name, the bytes are still
there, and the next sync removes them and ingests nothing — shown to fail with the sync's
recovery step removed; a second fault test proves a rewritten file survives recovery.
`src/fuse.rs` unit tests drive the handler's `unlink`/`rmdir` logic against a real catalog
and overlay. **Still open, and named**: those do not cross the kernel — this host has no
`/dev/fuse` — so the real round-trip is only in the `JUST_CACHE_TEST_FUSE=1` mount test,
which CI does not run either; empty directories a delete leaves on a cold tier are not
pruned by the delete itself; deleting from a tier that is not mounted, and a garbage collector
for bytes no delete reaches, are closed by #144 (below).

Closed by #44: a read of an offloaded object recalls it inline. `src/recall.rs`
(`Recaller`) is the provider-agnostic half; the FUSE `open` calls it for a read-only open of
a catalogued name, then hands the result to the cache overlay (#46) and observes the open
(#43) exactly as before, so all three compose. **The placement is `restore`**: copy into a
private partial, read back and hash against the recorded checksum, atomic rename over the
mover's symlink, cold copy kept. Only then does the catalog hear about it, through
`Catalog::record_recall`, which is handed the digest of the placed file read back *again*:
equal to the recorded checksum, the hot row is written verified and primary and the object
`present`; anything else (or no digest) writes the row unverified, leaves the primary and
the state alone, and the read fails — unknown, never good. A failed recall (no copy, a copy
that fails the checksum) names the tier and records nothing. Policy and refusal are in §4
(default `promote`; `min`/`hours` refuse with `EAGAIN` and the `restore` instruction).
**One copy per object**: recalls are single-flighted per object id — the first reader
recalls, concurrent readers of the same object wait on that flight and are handed its
result, and the leader re-plans under the flight so a recall that finished a moment earlier
is served, not repeated. The recaller opens its own catalog connection so a long copy never
holds the mount's lookup lock, and `Catalog::open` now sets a 10 s SQLite busy timeout so
the two writers wait for each other instead of failing. Tests (`tests/inline_recall.rs`,
cold root on the second filesystem): end-to-end recall (bytes served, symlink replaced,
cold copy kept, hot row verified/primary with the real checksum, `present`, a second read
served hot); `min` and `hours` refusals naming the tier and `restore`, with nothing placed;
read-through placing nothing; a same-length rotten cold copy failing with the tier named and
the object still `offloaded`; two concurrent readers placing exactly one copy (a copy hook
holds the leader for 400 ms, so the window is a sleep, not a proof — but both tests were
shown to fail with the single-flight and the refusal removed). Not covered: the FUSE
handler's errno mapping runs only in the `JUST_CACHE_TEST_FUSE=1` mount test, which CI does
not run; and the "read back again disagrees" branch of `Recaller` has no deterministic trigger
(`restore` already refused a mismatch), so `record_recall` is tested directly with a wrong
and a missing digest (row unverified, not primary, object `offloaded`) rather than through
the recaller. Writes to an offloaded
name still goes through to the cold copy in place; recall is read-only opens.

Closed by #146: a tier's `kind` is no longer free text that nothing reads. It is a
`TierKind` — the four kinds §2 names — and the parse refuses a kind no transport driver
serves, naming the tier, the kind and the line, instead of accepting it and then serving the
tier as an ordinary local directory. That was the real gap: `kind = "object"` with a `path`
parsed, and every path that read it treated the tier as `fs`, so a mover pointed at such a
root would have written a file to a destination it had not understood (invariant 3 — refuse
what you do not fully understand). The type also answers, once, the two questions the P3
drivers need answered in the same place: `is_served()` (only `Fs` today, and the single line
a driver flips) and `crosses_machine_boundary()` (§2 rule 2 — `object` and `peer` because the
bytes leave the host, `offline` because the volume itself does, so the envelope is the
export path's requirement too). No trait, no new flag, no new dependency, and no behaviour
change for `fs`: the existing suite plus the binary against two real filesystems is the
evidence. The transport trait itself is deliberately absent — it arrives with the first
driver that needs it (the object store), where there are two implementations to reconcile
rather than one abstraction and no second caller. **Still open, and named**: the kind check
now runs before the volatility check, so a tier that is both `volatile` and of a kind nothing
serves is refused for the kind (`fs` is the only kind a volatile tier can be written with),
and a `[[cache]]` overlay's own `kind` is still validated by the cache parser with its own
message (`only fs exists`) rather than through `TierKind` — the two paths to "a kind nothing
serves" do not share one message yet.

Closed by #140: the envelope for bytes that cross the machine boundary — §2 rule 2 — is now
a real format in `src/envelope.rs`, decided here rather than with the first driver so the
drivers start from a seam instead of growing one. **Format**: `JCV` + version byte + the
plaintext length + a fresh random 24-byte nonce prefix, then chunks — 64 KiB of plaintext
each, sealed with XChaCha20-Poly1305 under a per-chunk nonce (prefix[0..12] ∥ chunk index
LE) with the chunk index as additional data. The version byte is the "stored object names
the scheme it was written with": a future format changes it and refuses older ones by name,
not by a migration guess. Fresh random nonces per object mean two encryptions of one
plaintext differ, and no counter state survives an interrupted upload to reuse.
**Key source** (`Key::from_hex`): 64 hex characters, accepted from a file path or an
environment variable — never only argv, where every process on the host reads it. The key
is `Debug`-hidden (`Key(hidden)`), zeroized on drop, and no error path prints it: every
message names the stage ("chunk 3 of the copy", "the header"), never the material. The
three decisions rule 2 left open are thus: key from file/env with a named refusal when a
boundary-crossing tier has none (that refusal is wired by the driver issues, which own the
point of use); the format above; and the digest contract below. **The digest contract**:
what a tier of record stores is ciphertext, but the catalog's recorded digest is the
BLAKE3 of the *plaintext*, so a remote copy verifies exactly the way a local one does —
decrypt, read back, hash. A chunk that fails its tag is "cannot decrypt" and a decrypted
read-back that hashes wrong is "wrong bytes": `EnvelopeError` and the scrub path report
them separately, so a wrong or missing key never reads as bitrot. Chunks bind their index
as AAD, so a re-ordered stream is corrupt rather than shuffled-plaintext; the header's
length catches a cut at a chunk boundary, where no tag exists to fail. The seam is
deliberately not wired into any command yet — a driver that consumes it does not exist
(#141/#142/#143), and the issue scoped the wiring to those PRs; `encrypt_all` takes a
seekable source because the header must lead the stream and the length is not known
before it, and drivers read from files, which are seekable. **Still open, and named**: the
key-refusal-at-point-of-use is enforced by the driver issues (nothing here can refuse a
tier no command touches yet); one key per config, no rotation or KMS (the issue's out of
scope); and the format has no streaming-encrypt-from-a-pipe form, which would need a
trailer and a different header — a want, not a need, until a driver pipes instead of
reading a file.

**Closed by #141 (revised):** the object-store tier driver dispatches end-to-end.
When `just_cache sweep` resolves a `--dest` to an `object` tier, the move uploads
encrypted chunks through the envelope, verifies the remote copy by download + decrypt
+ hash against the plaintext BLAKE3 digest, records the object-tier location in the
catalog (tier = configured name, storage_key = S3 object key), and removes the source
— no symlink is left. An object tier holds no local bytes; a moved name has no local
representation (the **no-local-representation decision**, §4). The catalog is the
source of truth: `locate`/`explain`/`audit`/`scrub` find the object through the catalog
with state `offloaded`. `restore` downloads from an object tier through
`download_and_verify` (decrypt + hash against the recorded plaintext digest; a download
whose bytes fail the digest adopts nothing and names the tier). The FUSE mount serves
the name via recall: download + decrypt through `envelope.rs` + verify. Resumable
upload state lives in a catalog row (`resumable_upload` table). A `min`/`hours` tier
refuses inline recall with `EAGAIN` and names `just_cache restore <path>`. Credentials
and the envelope key come from a file or environment variable, never argv, never in a
log line or catalog row. `insecure = true` enables plain HTTP for loopback testing.
The `path` field remains the local scratch directory. E2E tests in
`tests/object_store_e2e.rs` drive the real binary against a fake S3 endpoint: sweep
→upload→verify→catalog-record→remove-source, restore downloads and verifies, an
interrupted upload keeps the source, a checksum mismatch adopts nothing. A manual run
against a real S3 bucket is documented below since CI cannot cover a cloud endpoint
deterministically.

Closed by #155: `just_cache object-server` — the server half of the LAN-peer tier (#142). A
peer runs this on their machine, speaking the same S3-compatible subset the object-store
client (#141) uses: PUT/GET/HEAD/DELETE with SigV4 auth, backed by local directories under
`--root`, validated against `--bucket`. Key containment is lexical + canonical (the #68/#71
pattern); writes land via `.partial` temp + atomic rename; Content-Length/Content-MD5 are
verified; the server fails closed on unknown requests. `--insecure` exists for plain HTTP on
loopback; a non-loopback bind without TLS or `--insecure` is refused. TLS itself is the
follow-up wiring work. Tests (7 unit tests) cover
the PUT+GET round trip with real SigV4 signing, bad signature refusal, key escape refusal,
atomicity, DELETE, Content-MD5 mismatch, and bind safety.

Closed by #142: the `peer` tier driver — the client half of the LAN-peer tier. **The
transport decision the issue asked to record:** a peer runs `just_cache object-server`
(#155), and the `peer` driver is the object-store client pointed at it — the same
S3-compatible subset, one credential pair, and the envelope (§2 rule 2) sealing every byte
that crosses. This host runs no new daemon; the peer is reached only for roots configured
for it, and an unreachable, refusing or full peer is a *named* per-file refusal that leaves
the source, the journal and the catalog exactly as they were (invariants 2, 5 and 7). A
`peer` tier config is the `object` shape minus a meaningful region: `endpoint` (the peer
host), `bucket`, `credential_source`, `encryption_key`, optional `chunk_size`/`prefix`, and
`insecure` for a trusted LAN or loopback — the driver defaults the signing region to `peer`
(the object-server derives the signing region from the request's credential scope, so the
value only has to be self-consistent). Recall and restore flow through the same verified
download path as object tiers, and a `min`/`hours` peer tier refuses inline recall exactly
as a slow object tier does.

Two things #142 records honestly rather than hides:

- **Single-PUT uploads.** The object-server accepts single-object PUT/GET/HEAD/DELETE only —
  no multipart endpoints — so a peer tier never uses the multipart/resume path; the whole
  encrypted file goes in one PUT (the envelope already buffers it, so nothing extra is
  spent). The trade is recorded: a large upload to a peer interrupted mid-flight restarts
  from scratch on the next sweep rather than resuming — the source is kept until the
  read-back verifies, so nothing is lost, only re-sent. Resumable multipart remains
  cloud-S3-only.
- **Three SigV4 bugs the real round trip caught.** #141's fake S3 never verified
  signatures, so client and server had never met. The first real client→server exchange
  (#142's tests run `just_cache sweep`/`restore` against a real `object-server` over
  loopback) exposed: the server's canonical request omitted the newline before the signed
  headers list; the server's recency check rejected every real `YYYYMMDDTHH:MM:SSZ`
  timestamp (its own test helper used a colon-less form that hid it); and the client signed
  an empty `host` while doubling `host` and `x-amz-content-sha256`. All three are fixed, and
  the server's test helper now emits the standard date format.

Tests: 7 end-to-end tests in `tests/peer_tier_e2e.rs` drive the real binary against a real
`object-server` subprocess — a move + verified recall, a restore back to the original
content, an unreachable peer, a refused credential, a transfer interrupted and resumed, a
>8 MiB file over the single-PUT path, and a `hours`-class peer tier parsing as a valid
destination — plus the tier-parser tests in `tests/tier_kind.rs`. Server-side TLS remains
the follow-up wiring work.

Closed by #143: the `offline` tier driver — export a file to a volume a person inserts, and
refuse a read that cannot reach it with the insert prompt. **The decisions the issue asked to
record:** (D1) the vault list is where the tier's volumes physically live; `vaults` is a
required non-empty list on an `offline` tier and the wire fields (`endpoint`, `bucket`,
`region`, `credential_source`) are refused *by name* rather than ignored, because a config
that says `endpoint` on an offline tier means something the tool did not understand. (D2) the
catalog's `volume` table is the only record of a volume's whereabouts — no marker file is
ever written into a volume — and it gains a `tier` column naming the tier whose drive holds a
volume while it is `mounted` (`just_cache volume list`/`set`); at most one volume may be
recorded mounted per tier, and a non-mounted state clears the binding. (D3) a location's
`storage_key` is `<volume-id>/<path>`, so recall can name the disk a copy needs before asking
for it. (D4) `sweep` exports through the envelope (§2 rule 2), reads the copy back, decrypts
and hashes it against the source digest, records the location (marked primary — the volume
copy is the tier of record) and only then retires the source; an absent mount or an
unrecorded/unmounted volume is a *per-file* refusal that never stops the sweep. (D5) a read
that cannot reach the volume is refused with the insert prompt — the tier, the volume id and
the mount point, plus the vaults — never a bare ENOENT, through the same `restore` the
slow-tier recall path already uses. (D6) `TierKind::offline` is now served and still
`crosses_machine_boundary()`; the parser tests pin the new fact.

Tests: 5 end-to-end tests in `tests/offline_tier_e2e.rs` drive the real binary — a sweep
exports to a mounted volume (ciphertext on disk, plaintext restored), a restore with the
volume out of the drive names the tier, volume and mount, a sweep with no mounted volume
refuses per file, an absent offline mount is not a usage error, and the `volume` ledger
round-trips through the CLI (`--json` included) and refuses a bad state and a second mounted
volume — plus the catalog ledger unit tests and the `tests/tier_kind.rs` additions.

What #143 records honestly rather than hides:

- **One copy per volume, no cross-volume dedup, no delete/GC.** The driver exports one
  encrypted copy to the mounted volume; it does not spread copies across volumes, and it
  never deletes from a volume. Deleting and garbage-collecting on tiers whose disk may not be
  mounted is exactly #144, and the vault model is what makes it a separate problem.
- **`restore --remove-copy` on an offline tier is refused, not obeyed.** The restored
  plaintext is a temporary sibling of the hot path, which is not under a trusted cold root,
  so the remove is refused — reclaiming the volume's bytes is #144's job, and leaving them
  costs nothing but space.
- **Scrub, reconcile and audit do not read offline volumes.** Their bytes live at
  `<mount>/<path>` while the recorded `storage_key` is `<volume-id>/<path>`, so the
  filesystem-based verification passes do not resolve the copy (and the volume is usually not
  mounted anyway). A catalog-only view is still correct — the location row is the record —
  but a scrub cannot verify an offline copy until a driver-aware scrub exists.
- **The ledger is operator-asserted.** `volume set <id> mounted` records that the named
  volume is in the drive; nothing verifies it against a marker in the volume (there is none),
  so a mislabelled disk is an operator error the catalog cannot catch. The one-mounted rule
  is what keeps the tier binding unambiguous.
- **`locate`/`catalog sync` treat an offline root specially only for existence.** A
  `--dest` that is a configured offline tier is skipped by the destination-existence check
  (its mount may legitimately be absent); every other command's handling of an offline root
  is unchanged.

Closed by #144: deleting and garbage-collecting bytes on tiers that are not mounted. This is
the P3 gap §9 named in the paragraph closed by #128. **D1 — the choice, and why:** a delete
whose copy sits on an absent tier is **deferred, not refused**. The transition commits and
records the released location as `(tier, storage_key)` in `pending_removal` (two new nullable
columns, added by the same `pragma_table_info` migration the other columns use); the next
`catalog sync`, `resolve --apply`, `delete` or `gc --apply` finishes it through one resolver.
An `fs` row resolves to its path under the recorded root — exact even while the root's
directory is absent, which is what lets the completion wait — and is unlinked when the root
returns; an `offline` row resolves to `<mount>/<rel>` **only** when the `volume` ledger says
the volume the key names is the one mounted in that tier, and only when a `tiers.toml` beside
the catalog describes the mount. A *different* mounted volume, or none, keeps the intent and
names it. Refusal stays only for a row that cannot be resolution-checked at all (a tier that
is not a recorded root and is not a volume key, a key that escapes its root). An offline copy
is unlinked without a plaintext re-hash: the bytes on the volume are the sealed envelope, so
the recorded plaintext size and digest cannot be checked against them, and the ledger already
vouched for the volume. **D2 — `just_cache gc`:** `gc --catalog FILE [--tier NAME] [--apply]
[--json] [--quiet] [--tiers FILE]` reports bytes no catalog row references, per configured
tier, and removes them **only** under `--apply`; never on a read, never as part of a sweep. The
report-only default exits `1` (a finding to act on) and prints every unreferenced regular file,
`.just_cache-partial-*` file and empty directory with its byte count. **D3 — resolution is
shared:** the GC's reference set is built from `location` rows resolved the same way D1
completes them, so a copy unreferenced *by path* but claimed by a row is kept — including a row
whose copy is *unverified* (unknown is not none) — and a row the GC cannot resolve is a
finding, never silently "no row". **D4 — crash safety:** before each unlink the GC journals its
intent in `.just_cache-gc-journal` beside the catalog; a *distinct* file from the mover's
journal (whose recovery reads every record as a move, so a GC record in it would be
misrecovered), but the same `Journal` type and grammar. `JUST_CACHE_FAULT=gc-after-delete=N`
aborts right after the Nth successful unlink; a stale record for a file that is already gone is
dropped on the next `--apply` pass, and the GC never writes a catalog row. The pass touches
nothing outside a configured root: it walks the recorded roots (plus a mounted `offline` tier's
mount when a `tiers.toml` describes it), never follows a symlink, never crosses a device
boundary, never removes the catalog, a journal or a config file, and removes empty directories
deepest-first. Test #4's "a path outside every configured root" is the GC's finding for a row
whose tier is not a root: reported by name, never touched.

Tests: `tests/gc.rs` (6) — a report-only pass names the bytes and removes nothing (bytes and
rows unchanged); a copy claimed only by a row, and one claimed by an *unverified* row, are
kept; a row outside every root is a finding, not a licence; `--apply` removes exactly what was
reported and a second pass finds nothing; an interrupted pass (`gc-after-delete=1`) adopts no
row and its stale journal entry is dropped on the next pass; `gc --json`'s shape and the exit
codes, plus a bad invocation is exit `2`. `tests/catalog_delete.rs` gains
`deleting_a_copy_on_an_absent_tier_defers_and_completes_when_it_returns` (the release commits,
the bytes survive, the deferred location is named, and the next sync finishes it).
`tests/offline_gc_e2e.rs` drives the real binary for the offline case: a released location
whose volume is not the mounted one stays pending and is reported; with the right volume
mounted the next sync unlinks the envelope.

What #144 records honestly rather than hides:

- **An offline tier is unreachable without a `tiers.toml`.** The mount is configuration, not a
  catalog fact; with no config the offline copy is neither scanned nor completed — the pending
  intent is kept and reported, never guessed at. The config an invocation loaded (`--tiers`, or
  `tiers.toml` beside the catalog by default) is used for both the scan and for completion, so
  naming it once is enough.
- **The GC does not lock against a running sweep.** A `.just_cache-partial-*` file is garbage
  by definition (no row references it) and a move in flight at the same moment could lose its
  partial; the move retries, so no data is lost, but the collision is not prevented.
- **A directory the GC empties is pruned by the GC, not by the delete that emptied it.**
- **Retention beyond "no row references" — age- or cost-based expiry — remains P4.**
- **Scrub and reconcile still do not read offline volumes** (unchanged from #143): a scrub
  cannot verify an offline copy until a driver-aware scrub exists.

**Closed by #137 (the MCP adapter).** `just_cache mcp` serves the catalog-backed commands to
an agent as JSON-RPC on stdio. What it decides:

- **The adapter relays the CLI rather than reimplementing it.** Each tool re-executes this
  same binary as a subcommand and returns what it printed, so an answer given to an agent is
  byte-identical to the answer given to a shell and there is no second code path to drift.
  The alternative — calling the library functions directly — would mean re-deriving each
  command's output and exit-code rules inside a module with no business knowing them.
- **`serde_json` is added, for parsing only.** Every `--json` writer in this repo stays
  hand-rolled as it was; the request bodies are written by the client, so a malformed or
  hostile document reaches the parser, and a hand-rolled *reader* is where a wrong answer
  becomes a security answer. The dependency is confined to `src/mcp.rs`.
- **The tool list is the policy.** The surface is `locate`, `explain`, `audit` and a
  `scrub` pinned to `--dry-run`; there is no `sweep`, `catalog delete`, `gc --apply`,
  `reconcile` or `pin`. A tool is offered only when the invocation was given the roots it
  needs, so the list states what the server can actually answer rather than offering a call
  that will fail for a reason a model cannot fix. Each call names a *query*, never a path:
  the roots are chosen once, at startup.
- **`restore` is opt-in, once.** It moves bytes and writes, so it is exposed only under
  `--allow-restore` — the operator's decision at startup, not an agent's per call. §10's
  non-goal (no autonomous mutation) is unchanged by this: the tool still waits to be asked.
- **A nonzero exit is an answer, not a failure.** `audit` exits 1 to say "findings exist"
  and `explain` exits nonzero for "not this sweep". `isError` is therefore set only when a
  command produced no output at all and failed; the exit code is stated in the text either
  way. Reporting these as tool failures would tell an agent to ignore the answer it asked
  for.

What #137 records honestly rather than hides:

- **`scrub` and `restore` relay text, not JSON, because neither has a `--json` writer.**
  #137 sets the rule "reuse the existing `--json` output rather than inventing a second
  response format"; where none exists, the command's own text is the answer and no second
  format was invented for it. A future `--json` on those commands would need no adapter
  change beyond passing the flag.
- **`call_tool` re-checks availability after the name is matched.** The list is filtered by
  configuration and the call path is filtered again, so a call cannot succeed by naming a
  tool whose roots the server was never given; the two filters are separate on purpose
  rather than one guarding the other.
- **The surface is read-only by construction, not by convention.** Nothing in the adapter
  can move, repair or delete a byte: it can only run the four read-only commands (plus
  opt-in `restore`), and it has no code path that writes to the catalog. A future tool that
  mutates would be a change to *that* list — which the tests name explicitly, so adding one
  means deleting a line that says it must not exist.
- **There is no progress or cancellation support.** A tool call runs the command to
  completion; the notifications a long `scrub` could emit are not implemented, so a client
  sees one reply per call. `scrub` is the only slow one, and it is bounded by the same
  `--read-budget`/rate limits the CLI has.
- **Protocol revision is pinned, not negotiated.** The server states one revision
  (`2025-06-18`); it does not read the client's requested version and pick a compatible one,
  because the tool surface has nothing version-dependent in it yet.

Closed by #169 (the maintenance events file): every maintenance pass now appends the report
it produced to an append-only JSONL file the walk already skips, so the dashboard server
(#170) has a stream to tail. The decisions this issue left open, and the honest gaps:

- **The event is the pass's own report, wrapped only for routing.** Each line is
  `{"v":1,"pass":<name>,"time":<unix>,"report":<document>}`, where `<document>` is exactly
  the command's own JSON — the existing `--json` output for `gc` and `audit`, and new
  `to_json` methods on `MigrationReport` (sweep), `ScrubReport`, `ReconcileReport` and
  `SyncReport`. No UI-only schema is invented; verbosity (`-v`/`-q`/`--json`) moves only
  what stdout shows, never what lands, and tests pin that equality.
- **The name is fixed and it lives beside the catalog.** `.just_cache-events.jsonl`, that
  path's sibling: with `--catalog` naming a file the events file sits in the same directory,
  and with no catalog it falls back to the catalog's default location beside the watch root.
  Either way it is one well-known file under the `.just_cache` prefix (invariant 8), never
  scattered per directory the pass touches — and a catalog outside the tree keeps the events
  file outside it too.
- **Growth is bounded by rotation, not by rewriting.** The live file is never rewritten,
  only appended to; when an incoming line would push it past the byte budget (default 4 MiB,
  overridable with `JUST_CACHE_EVENTS_MAX_BYTES`) it is renamed aside to a single segment
  and a fresh file is started, dropping the oldest events first. A single event larger than
  the budget is written whole — splitting a line would leave two non-events. Named gap: a
  tailing reader that attaches after a rotation misses events now in the dropped segment;
  the segment is not part of the live stream.
- **`--dry-run` records nothing.** A dry run changed nothing, so there is no event to record
  for `sweep`, `scrub` or `reconcile`; a report-only `gc` (no `--apply`) does record, because
  it is still a pass the operator watches, and `catalog sync` always records.
- **Sub-sweep progress is deferred.** Events land at report granularity in this issue: an
  interval-mode sweep appends one event per completed pass, and no per-copy `progress` ping
  is written from `disk_management` mid-copy. The enveloped report is the contract a later
  issue can extend.
- **`schedule --run` records through the pass it runs.** A scheduled scrub or reconcile is
  the same code path as the on-demand command, so it appends that pass's own report; the
  scheduler emits no separate event.
- **The reader skips a torn tail without a JSON dependency.** A final line that is not a
  complete, brace-balanced object — the truncated write a crash mid-pass leaves — is skipped
  rather than reported as corrupt. The writer escapes quotes and backslashes and emits no raw
  newline inside a string, so the brace scan is exact for what this tool writes.
- **`audit --no-filesystem` writes no event.** That mode promises no filesystem access at
  all, and appending an event is a write; the pass is deliberately silent in the stream
  rather than breaking its own contract. This is the one maintenance mode that does not
  append, and it is named here rather than hidden.

Closed by #171 (the backend documents of the dashboard's views): every view in the issue now
has a document a CLI command prints, so no view computes policy. The decisions this issue
left open, and the honest gaps:

- **The commands are new, not routes.** #170's rule — a `ui` route only replays what a
  command prints, and an endpoint that computes something no command prints is a design
  decision, not default behaviour (docs/design.md §9) — means the tier map, the pending
  view and the schedule view needed `just_cache tiers`, `catalog pending` and
  `schedule --json` first. Each route runs the same binary and replays its `--json`
  document byte for byte; nothing in the server reads a config or a table itself.
- **Per-tier counts are the catalog's, not policy.** `tiers --json` carries, per tier, the
  rows the catalog records: `locations` (location rows), `damaged` (locations a scrub
  marked), `pending_removals` (releases a delete deferred). Each is one `SELECT COUNT(*)`
  over the table that owns the fact, so a browser cannot show a number a query would not
  return. Without a catalog the counts are omitted entirely — a number with no document
  behind it is worse than an absent one. Because the catalog already keys local tiers on
  the canonical root and offline exports on the tier name, a count reads both keys; the
  dual keying is named at the query, not worked around.
- **The insert prompt is served verbatim.** `tiers` builds the prompt from the volume ids
  the catalog's own location rows name, minus the one the ledger records mounted, through
  the same `insert_prompt` the recall path refuses with — so the dashboard shows the
  prompt, never a paraphrase of it.
- **`schedule --json` retires the text-wrapping.** `/api/schedule` now replays a document;
  `pin --list` is the only route still wrapped as `{"text": ...}`, because it has no
  printer yet. The document carries no `--run` behaviour: JSON or text, a report never
  runs a pass.
- **The views named in the issue that need no new backend are not invented here.** The
  audit and explain views render documents #170 already replays (`audit --json`,
  `explain --json`); the live activity view reads the events stream #169 lands. What they
  look like is the frontend's work, in a later issue against the same documents.

Closed by #186 (optional webhook notifications): a scheduled run and an `audit` can post a
short message to a Discord-compatible webhook. The decisions this issue left open, and the
honest gaps:

- **`notify.toml`, its own file.** The endpoint is `url = "https://..."` in `notify.toml`
  beside the catalog, opened only when it is already there and never created — the same
  posture as `tiers.toml`/`policy.toml`/`schedule.toml` (invariant 9). Absent means
  notifications are off and the run opens no socket at all. It is its own file, not a
  `[notify]` section of `schedule.toml` or `tiers.toml`, because it is the one config that
  holds a *credential*: the operator can make `notify.toml` readable by the cron user alone,
  rotate the webhook without touching a schedule or a placement, and switch notifications
  off by deleting one file. A malformed `notify.toml` is a refusal that names the line,
  exactly like a broken `schedule.toml`; a *delivery* failure is a warning and nothing more.
- **Only the scheduled run and `audit` notify, and no CLI surface is added.** A summary is
  sent once, at the end of a `schedule --run` that ran a pass or held one back — so it
  follows the configured schedule rather than any ad-hoc invocation — and an `audit` that
  has findings posts them promptly, while a clean audit stays quiet. Nothing else notifies:
  a manual `sweep` or `scrub` is not a maintenance run the operator asked to hear about, and
  a flag that made any command notify would be CLI surface the acceptance criteria did not
  need. `--run --dry-run` sends nothing (it changed nothing), and `audit --no-filesystem`
  reads no config at all — that mode makes no filesystem access, and reading `notify.toml`
  is one.
- **The message is counts, not paths.** A run summary names the catalog by its last path
  component and gives each pass's counts (scrub's locations/verified/repaired/damaged/
  missing/malformed; reconcile's rebuilt/adopted/conflicts/refused/failed figures). An audit
  message gives the total findings, the non-zero counts by verdict kind, and the repair
  outcome — never a finding's path. A failed pass names the pass, not its error text, which
  can carry paths and hosts. The webhook URL is a credential: it is never printed, never
  logged, and never echoed by a config error (which names the line instead); a delivery
  warning names at most the endpoint's host.
- **Delivery is hand-rolled over `std::net` + `rustls`, mirroring `object_store`.** It does
  not reuse `object_store`'s transport: that stream is private and bound to
  `ObjectTierConfig` and S3 signing, so exposing it would couple a notification to a tier's
  config type. The JSON body is hand-built (`{"content": ...}`) with the same escaping every
  `--json` writer uses. Plain `http` is accepted only for a loopback host — so the tests can
  drive a real socket with no network — and refused elsewhere, so the credential is never
  sent in the clear.
- **Bounded and best-effort.** Connect, write and read each carry a 5-second timeout
  (`NOTIFY_TIMEOUT`): one tiny POST, well under a cron tick; and, unlike `object_store`'s TLS
  stream, a read timeout is surfaced as an error rather than retried, so a hung endpoint ends
  the wait instead of spinning. An unreachable endpoint, a TLS failure, a non-2xx answer and
  a malformed response are each a single warning on stderr — never a nonzero exit, never a
  failed or retried pass, and never any effect on what the command prints.
- **Not verified locally: a real Discord delivery.** The tests cover an unconfigured run
  (nothing sent, nothing dialled), the exact request a configured run sends (method, path,
  `Content-Type`, `Content-Length`, and a `{"content": ...}` body whose counts and reduced
  paths are asserted), a `500`, and an unreachable port; no test talks to Discord, so the
  endpoint's own acceptance of the payload is untested here. DNS resolution is also
  unbounded: `std` gives no timeout around the system resolver, so a stalled name server
  could delay a run before the connect timeout applies — named rather than hidden.

## 10. Non-goals

- **Automating a pin's removal or a restore's placement.** `pin`/`unpin` are operator
  commands and `restore` is an explicit request: nothing schedules a pin's lapse (an expired
  pin simply stops mattering, and says so) and nothing decides *where* a restored object
  should live. §4's "a slow tier may require an explicit restore request" is the operator
  asking for the bytes back; the policy-driven placement that would put them somewhere
  again is later work.
- **The tool does not run a maintenance daemon.** `just_cache reconcile` and `just_cache
  scrub` are on-demand commands, and #48 schedules them without one: `schedule.toml` names a
  cadence and a budget per pass, and `just_cache schedule --run` — run from cron or a
  systemd timer — runs whatever is due and records it. A long-lived supervisor is
  deliberately not the mechanism: it would need a restart policy, log rotation and a pid
  file for a job that is one command, and cron already exists on every host that would run
  this.
- **An MCP server is a client-driven adapter, not that daemon by another name.** It starts
  with its client, answers requests and exits with it: no timer, no scheduled pass, no
  autonomous mutation. The read-only tools (`locate`, `explain`, `audit`, `scrub --dry-run`)
  are the default surface; recall and `restore` are separate tools the operator opts into,
  because both move bytes and `restore` writes. §8 records when it lands and why.
- **Ongoing verification of copies that are present is not reconcile's job either.** It
  stats each recorded location — one stat answers "is this copy absent", which is all it
  exists to ask — and leaves the read-back of present copies to `scrub`, the only place
  with an I/O budget for it. A copy that is present but unverified is not touched here.
- **Off-host replication is not what a floor of 2 buys.** The `--copies` floor replicates
  within (or across) the destinations on one host; it survives a disk failure, not the
  loss of the machine or a site. Remote/object/offline tiers are a separate mechanism (§8),
  so the `--copies` floor itself is not a backup. More than one copy *per disk* is backup
  tooling too, and is out of scope (§6.1 of the issue).
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

- **The tier config drives the drivers; it does not yet act on `recall` or `cost`.** #40 makes
  a tier a configured object — a name, a driver, a recall class, a volatility, a copy floor, a
  cost — and lets `locate`/`audit` speak in those names, and refuses a volatile tier as a
  destination. Lifecycle rules are evaluated by #41 (above); the `fs`, `object`, `peer` and
  `offline` drivers move bytes to and from their tiers (P3, #140–#144), cache overlays (§2.1)
  are served only through `mount` (#46), and nothing acts on `recall` or `cost` beyond
  reporting them — that is P4.

- **Promoting a recalled file is not the mover's job.** #41 evaluates `up = { on_access }`
  and `explain` names the rule that would promote, but no sweep performs a promotion:
  recall is the namespace provider's job (§8, P2), and a `down` engine that also moved
  bytes back up on every read would be a cache with no policy boundary. The `up` half is
  reported, not acted on, and named here rather than left implied.
- **Cost-aware placement is a later phase.** §5's cost model — reporting (and optionally
  acting on) the delta of keeping each subtree where it is — is not part of #41, which
  evaluates rules that name tiers by name and does not read a tier's `cost`.

- **Retrofitting the existing fault seams is not this rule's job.** Rule 6 of §7 binds the
  *next* feature whose failure window cannot be timed from a test; the two seams that
  already ship (#25, #36) are the pattern it points at, not a queue of rework. A seam that
  a later change proves malformed — or inert when it should fire — is a bug in the feature
  that owns it, fixed on its own terms rather than by reopening this rule.
- **Reconcile's read budget is a total, not a pace.** `--read-budget` caps the bytes one
  pass may read and defers what it cannot admit; it does not smooth those reads over time
  the way `scrub --rate` does, so a pass that stays inside its budget still reads a busy
  tier as fast as the disk allows. A rate limiter is a separate question (#49 stops at the
  bound the acceptance criteria asked for).
- **Reconcile's free-space check uses the recorded length, not the allocated size.** The
  catalog holds an object's length and not its block count, so a sparse object whose holes
  make it cost far less than its length is measured by the length and refused against the
  floor conservatively. The mover measures allocation because it has the file in hand;
  reconcile has only the row.
- **A catalog-only audit cannot say whether a recorded copy still exists.** #51 reports
  what the rows say and never touches a tier, so it cannot turn a `location` row into
  "the file is there" — that stat is exactly what catalog mode above adds, and what this
  mode exists to avoid. It reports the recorded floor and damage state instead, marks the
  rest `unchecked`, and leaves the filesystem answer to a catalog-mode run once a tier is
  mounted. Making the mode reconstruct paths to probe them would defeat its purpose.
- **The mount does not rewrite the catalog, and that bounds what it can do.** `just_cache
  mount` (#42) serves reads from the tier of record and writes through to those bytes in
  place, but it never writes a `name`/`location` row: a rewrite leaves the object's recorded
  checksum stale until the next `catalog sync` observes it, exactly as any other rewrite is
  observed. Three consequences are deliberate and named rather than hidden. **Rename is
  confined to the watch root**: the mount can move a hot name (the disk overlay shows it
  afterwards) but a name whose bytes are on a cold tier answers `EROFS`, because renaming the
  cold copy would leave a name the mount cannot serve and no catalog row to update. (Deletion,
  once listed here, landed with #128: `unlink` is a catalog delete — see §9. Recall, once
  listed here, landed with #44: a read of an offloaded object recalls it, and the mount then
  writes the recalled copy's `location` row, verified, plus the object's `present` state.)
  (Access observation,
  once listed here, landed with #43: the mount writes `lifecycle.last_access`/`accesses`,
  and nothing else.)
- **The mount's FUSE surface is deliberately incomplete, and the catalog is indexed once.**
  `just_cache mount` (#42) implements the operations a read/write workload needs and refuses
  the rest rather than answering them wrongly: `statfs` is the FUSE default, so `df`/`statvfs`
  on the mount reports zeros and a consumer that checks free space before copying sees none;
  `readlink` answers `ENOSYS`, so a symlink under the watch root that the catalog does not
  name — one the mount therefore cannot serve as bytes — cannot be followed through it (a
  symlink the mover left for a *catalogued* name never reaches `readlink`, because lookup
  answers the tier of record instead); `chown` answers `EPERM`; and `symlink`/`link`/`mknod`
  and the extended attributes answer `ENOSYS`. The namespace is also built **once, at mount
  time**: a `catalog sync` that runs while the daemon serves is not observed until the next
  mount, and a name created through the mount is served by the disk overlay before the
  catalog knows it. Neither is a correctness hole — every read is still answered from a root
  the catalog proved, and every row that cannot be resolved still refuses — but a long-lived
  mount is a snapshot of the catalog it opened, not a live view of it.
- **The object-server speaks to exactly one credential pair, and in the clear unless the
 operator wires TLS.** `just_cache object-server` (#155) verifies one SigV4 credential
 pair — no tenants, no per-root credentials, no IAM — because a peer tier is a machine the
 same person owns on both ends; anything richer is cloud-provider work. Transport
 encryption is likewise out of scope for it: the *content* is always sealed by the
 envelope (§2 rule 2), the loopback fake and a trusted-LAN peer may run plain HTTP, and a
 non-loopback bind without TLS refuses unless `--insecure` is given explicitly, with a
 warning naming the exposure. Server-side TLS (the client already speaks it via `rustls`)
 is the peer-tier wiring work that follows.
- **The dashboard is a view of the catalog and the passes, not a second policy engine.**
  `just_cache ui` (#170–#172) serves what the read-only commands already print, and it
  computes no policy of its own: the numbers a view shows are `--json` documents a cron
  alert would have seen, and a mutating action exists only when the invocation named its
  `--allow-*` flag. The UI can never do something the same binary invoked with fewer flags
  could not, and it never writes the catalog directly.
- **Animations on the dashboard are real events, never a simulation.** The live views
  render the reports the maintenance passes appended to the events file (#169); nothing is
  interpolated between events, and a quiet system is shown as still. A UI that invented
  movement would be a second, untrusted record of what happened to someone's data.
- **The dashboard hand-rolls HTTP over `std::net`, exactly as `gateway` and
  `object-server` do, and takes one thread per connection.** `just_cache ui` (#170) adds
  no web framework and no runtime: the request line and headers are parsed by hand, the
  routes are a fixed match, and the only concurrency is a thread spawned per accepted
  connection. That last point is the one place `ui` departs from `gateway`'s strictly
  sequential loop: the SSE stream (below) holds a connection open for the life of a
  viewer, and a single-threaded accept loop would answer nobody else while one person
  watched the events file. A framework would buy routing and a worker pool at the price of
  a large dependency tree — already the reasoning that kept `gateway` dependency-free — so
  the choice is the repo's own approach plus `std::thread`, at zero added dependency
  weight.
- **The frontend is one self-contained `index.html` embedded with `include_str!`, and the
  token is entered by the operator.** The shell is plain HTML, inline CSS and inline
  vanilla JS — no framework, no node build step, no second release artifact — so the
  cargo-only build stays true. The page is served publicly, because it is inert (it holds
  no catalog data) and it has to load before a person can present a token to it; **every
  route that answers from the catalog, and the event stream itself, requires the bearer
  token and answers `401` without it.** The page keeps the token in `sessionStorage` and
  sends it in the `Authorization` header on every `fetch`. The SSE stream is consumed with
  `fetch`'s streaming reader rather than the browser `EventSource`, precisely because
  `EventSource` cannot set headers and the token would have to ride in a query string,
  where an access log would capture it. This is why the token is in a header and never in
  a URL anywhere in this command.
- **The event stream tails the rotating file by byte offset, replays the retained ring on
  attach, and drains a rotated segment before restarting.** `src/events.rs` (§9) names the
  gap: the file rotates at a byte budget, so a reader that attaches after a rotation has
  missed whatever the old segment held. The tail (#170) answers it the only honest way. A
  client connecting gets the retained ring replayed first — the rotated segment, then the
  live file, in order — so a late viewer sees the recent window rather than nothing. A
  client already connected holds a byte offset into the live file; when the file shrinks
  under that offset (the signal that it was renamed to a segment and a fresh live file
  begun), the tail reads the *unread remainder* of the rotated segment before it resets and
  continues on the new live file, so a viewer that had not caught up across a rotation still
  loses nothing. What is genuinely lost is the segment dropped at rotation beyond the
  two-file ring: the previous segment is overwritten, and no reader — this one included —
  can recover events older than the retained window. That bound is the cost of a file nobody
  has to prune, and it is stated here rather than papered over. A torn final line is held
  back until it is whole (the same `is_complete_object` test `read_events` uses) so a crash
  mid-append cannot put half an object on the wire.
- **Every JSON route answers `200` with either the command's own document or a wrapped
  refusal.** A route runs this same binary (`program`), exactly as `mcp` does, and returns
  its stdout verbatim when the command produced a `--json` document — so `GET /api/audit`
  is byte-for-byte what `just_cache audit --json` prints to a shell. When the command
  refused and produced no document (a missing catalog, a route whose roots were not given
  at invocation), the body is `{"error": "...", "command": "...", "exit": N}` carrying the
  command's own stderr; a missing catalog is therefore the refusal a person would have read,
  not a stack trace and not a `5xx`. `401` (no or wrong token), `404` (unknown path) and
  `405` (a method other than `GET`/`HEAD`) are the only non-`200` answers. Routes that map
  to a command with no `--json` printer — `pin --list`, `schedule` — wrap the command's text
  as `{"text": "..."}`, again with no recomputation.
- **Two views named in the issue are not invented here, and the routes are fixed.** There
  is no read-only command that prints the tier *configuration* (`tiers.toml`) as a document:
  the per-tier copy floor the catalog records is served by `GET /api/catalog`
  (`audit --no-filesystem`), and the configured set of tiers, kinds and recall classes is
  not synthesized, because §9's rule is that an endpoint which computes something no command
  prints is a design decision, not default behaviour. Likewise the `pending_removal` rows a
  deferred `catalog delete` leaves have no `--json` printer, so no route reports them yet.
  Both are view work for #171, recorded here so their absence is a decision rather than an
  oversight. This PR also ships no mutating route at all — mutating actions are #172 — so
  the exposed surface is the whole policy: the fixed set of read-only queries above.
- **The shell is the one unauthenticated route, and that is the deliberate exception to
  `gateway`'s all-or-nothing auth.** `gateway` authenticates every request because every
  request can move bytes; the `ui` shell cannot — it is a static document that names no
  object. Requiring a token for it would make the dashboard unusable from a browser, since
  a page cannot present a credential before it loads. So `/` is public and holds no data;
  every `/api/` route and `/api/events` requires the token. The 401 carries
  `WWW-Authenticate: Bearer` and never echoes the presented value, and the access log line
  names only method, target and status — never a header — so the token cannot reach a log.
- **The views are renderings of documents, never a second policy.** Each view fetches one
  route's document and shows its fields; a page that computed a verdict, a health label or
  a number would be a decision the commands did not make, so the tier map prints the
  recorded locations and the recorded copy floor as the two numbers they are and a gauge
  only as their proportion. Empty states come from the document's own `configured`/`error`
  fields, not a guessed default. The SSE-driven animation lights a tier card only when a
  real event's paths fall under it — no event, no motion; there is no ambient animation,
  because a feed of real events that shimmered when nothing happened would be lying. What
  could not be tested: the page's JavaScript. The tests drive the binary over HTTP only,
  so they pin the served shell (its scaffolding, its empty-state wording, that every
  `/api/…` string it names is a route the surface serves) and the documents behind the
  views; the browser behaviour itself — animation, drill-downs, the explain panel — is
  verified by hand, and this is the gap, named here rather than hidden.

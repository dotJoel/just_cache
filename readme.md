# just_cache

Move cold, rarely used files off a fast disk onto slower ones — and leave a symlink
behind, so every existing path keeps working.

Point it at a directory and one or more cold-storage roots. On each sweep it walks the
tree, asks the filesystem when each file was last *used*, and moves the ones that have
gone untouched onto the cold tier, replacing them with a symlink to the new location.

```sh
just_cache \
  --watch /mnt/cache/media \
  --dest /mnt/disk-slow/media \
  --dest /mnt/disk-archive/media \
  --min-idle-days 60 \
  --limit 25
```

That is the `sweep` subcommand with the subcommand omitted (existing scripts and cron
entries keep working); `just_cache sweep ...` is the explicit form. The other
subcommands: [`audit ...`](#auditing-consistency) checks that the tree and the cold tiers
still agree, [`explain ...`](#explaining-one-path) answers why one path is where it is,
[`catalog sync ...`](#the-catalog) records the state of both sides in a SQLite catalog,
[`locate ...`](#finding-an-object) says where an object lives,
[`restore ...`](#restoring-a-file) brings an offloaded file back,
[`scrub ...`](#scrubbing-for-bitrot) reads every stored copy back and verifies it against
the catalog, and [`reconcile ...`](#reconciling-a-re-added-disk) rebuilds copies a
re-added disk is missing.

## Why

Trees like caches, media libraries and build directories accumulate files that have not
been read in months. Deleting them loses data; leaving them on a fast disk wastes it.
`just_cache` moves those files to slower storage and leaves a symlink at the original
path, so the fast disk is freed and every existing path still opens the same bytes.

## Configuration

Every option is a flag; there is no config file and nothing is hardcoded.

| Flag | Default | Meaning |
|---|---|---|
| `--watch <DIR>` | *required* | Tree to watch. Walked recursively. |
| `--dest <DIR>` | *required* | Cold root, fastest tier first. Repeat for each slower disk. Must already exist. |
| `--include <GLOB>` | *(whole tree)* | Only manage paths matching this glob. Repeatable. |
| `--exclude <GLOB>` | *(nothing)* | Never manage paths matching this glob. Repeatable; wins over `--include`. |
| `--min-size <SIZE>` | `0` | Ignore files smaller than this (e.g. `1MiB`). |
| `--max-size <SIZE>` | *(no limit)* | Ignore files larger than this (e.g. `500GiB`). |
| `--allow-hardlinked` | off | Move files with more than one hard link (see Guards below). |
| `--min-idle-days <DAYS>` | `30` | Only touch files last used at least this long ago. |
| `--min-observed-accesses <N>` | `1` | Pin a file once it has been read `N` times *during this run*; `0` disables the pin. |
| `--limit <N>` | `10` | Most files moved per destination per sweep. |
| `--copies <N>` | `1` | Durability floor: place this many verified copies on N distinct `--dest` roots before removing the source. `1` is the original single-copy mover. |
| `--min-free-gb <GB>` | `1.0` | Floor of free space a destination must keep, on top of room for the file. |
| `--interval <SECS>` | `3600` | Delay between sweeps. |
| `--once` | | One sweep, then exit. |
| `--dry-run` | | Report the plan, touch nothing. |
| `-v` / `--verbose` | | Show every file considered, including why it was left alone. |
| `-q` / `--quiet` | | Only problems. |

### Scope: what this tool is allowed to touch

Nothing is moved unless the scope allows it. Scope is the outermost gate — checked
before usage, size or any policy consideration — so a file that is out of scope can
never be moved by a rule that later grows more eager. It is also checked twice: once
when candidates are chosen, and again immediately before any bytes move.

```sh
just_cache \
  --watch /mnt/cache \
  --dest /mnt/cold \
  --include 'media/**' \
  --include 'scratch/**' \
  --exclude 'node_modules' \
  --exclude '*.part' \
  --min-size 1MiB \
  --max-size 500GiB
```

Patterns match a file's path **relative to `--watch`**, and also match its ancestor
directories — so including a directory includes everything under it, and excluding one
drops its contents:

- `--include 'media/**'` — the subtree, nothing else.
- `--exclude 'node_modules'` — a bare name with no separator or wildcard is expanded to
  match at any depth, because that is what it reads as. Same for `--exclude '*.part'`.
- `--exclude 'media/.git/**'` — fully explicit when you want to be.

Size units are binary, with or without the trailing `B`: `4K`, `1MiB`, `2G`, `1TiB`.
Bounds are inclusive — a file exactly at `--min-size` or `--max-size` qualifies. A file
that is both out of scope and too small reports the outermost reason (out of scope).
Out-of-scope files are not even tracked, so a long-running sweep does not carry them in
memory.

### Guards: things that are in use

A cold file that something is *currently using* is not cold, so two checks run before any
move — once when candidates are chosen, and again immediately before the bytes move,
because a descriptor can be opened in between:

- **Open by another process.** On Linux the sweep takes one snapshot of open descriptors
  per sweep (from `/proc/*/fd`, matched by device + inode) and skips anything in it.
  Taking the snapshot once per sweep rather than once per file matters: a process-table
  scan per candidate would cost more than the copy it protects. The check is honest about
  its own reach — without root it can only see this user's processes, and it says so
  (`open-file check is partial: N of M process(es) could not be inspected without
  privileges`) rather than implying a guarantee it cannot make. On platforms where
  descriptors cannot be enumerated at all, it warns that files in use may be moved.
- **Hardlinked elsewhere.** A file with more than one link cannot be moved across
  filesystems without breaking the pair — the link is not merely broken, it is impossible,
  and the other name keeps pointing at the old bytes. Such files are skipped by default;
  `--allow-hardlinked` does it anyway, deliberately.

The sweep summary counts these separately (`N in use`), so a busy box reads differently
from a genuinely cold one — if every sweep reports files in use, the guard is working, not
failing.

### Destinations

**A destination is never created.** A missing `--dest` is a startup error, not a
`mkdir`: if a slow disk is unmounted, silently creating its mount point would have the
tool write terabytes into a directory on the wrong filesystem.

With several destinations, sweeps fill the fastest tier first and only spill onto the
next when a tier is out of room (or has hit `--limit`). A file too large for what a tier
can spare is reported as *waiting for room* rather than failed — it will be picked up on
a later sweep, and the source is left untouched until then.

### Replication: `--copies N`

With `--copies N` (N > 1) a file is copied to N **distinct** `--dest` roots and every
copy is hashed and compared to the source before the original is removed. A copy that
fails its checksum — even a same-size one — does not count toward the floor, and if the
floor is not met the source is kept and the file is reported as *under-replicated*. The
tool never deletes the only copy to satisfy a number.

```sh
just_cache --watch /mnt/cache/media \
  --dest /mnt/disk-a/media --dest /mnt/disk-b/media \
  --copies 2 --min-idle-days 30 --once
```

An invocation that cannot satisfy the floor (fewer distinct `--dest` roots than
`--copies`) is refused before anything moves. `catalog sync --copies N` records the floor
once per destination tier: afterwards a sync reports objects holding fewer verified copies
as `under-replicated` (with the disks they *are* and *are not* on), and a copy deleted by
hand shows up as `location-missing` naming the disk it was on. `audit --copies N` reports
the same filesystem-side as `replica-lost`.

Distinctness is by configured root, not by device: two directories on one pool are two
destinations. With a floor of 2 and one other disk, the second copy lands on that other
disk **on the same host** — this survives a disk failure, not the loss of the machine.
Off-host copies are a later phase.

## How a file is chosen

A file is moved when **all** of these hold:

0. it is in scope (see Scope below): inside `--include`, not `--exclude`, within the size window;
1. it is not already a symlink (already migrated);
2. it is not empty;
3. nothing has read it during this run (the `--min-observed-accesses` pin), and nothing is
   *holding it open* or hardlinked to it right now (see Guards above);
4. its last-use stamp is at least `--min-idle-days` old;
5. it is within the per-destination `--limit`, oldest use first.

"Last use" is the filesystem's **access time (atime)**, falling back to mtime where
atime cannot be read. That is the honest signal: it reflects every reader since the
file was written, not just the fact that the file exists. Two mount options matter,
though:

- `relatime` (the Linux default) only updates atime when the old atime is more than 24
  hours old, so a file read twice today still looks "today"; fine for a tool that
  measures idleness in weeks.
- `noatime` — and **ZFS, where atime is off by default** — never updates atime, so the
  fallback to mtime means a file that is read often but never rewritten looks cold.
  Before pointing this at a ZFS pool, either turn atime on
  (`zfs set atime=on <pool/dataset>`, with `relatime` a reasonable middle ground) or
  accept mtime semantics.

## What happens to a file

1. The destination keeps the file's path *relative to the watched root*, so nested
   trees keep their shape and same-named files in different directories cannot collide.
2. The file is `rename`d when the move stays on one filesystem. Since a "slower disk"
   is normally a different mount — where `rename` fails with `EXDEV` — it falls back to
   copy, `fsync`, rename-into-place, then delete the source. A partial copy is never
   left at the destination.
3. The original path becomes a **relative** symlink (absolute only when the two trees
   share no ancestor), so the pair survives the tree being moved or the cold disk being
   remounted elsewhere.

Sweeps are safe to interrupt and safe to repeat:

- an entry that is already a symlink is skipped;
- a destination that already holds a byte-identical copy is adopted — the source is
  replaced by the symlink instead of being transferred twice;
- a destination of a *different* size is refused rather than overwritten, and reported
  as a failure;
- one file failing never stops the sweep; `--once` exits `1` if anything failed.

A destination below its free-space floor is not a failure: those files are reported as
*waiting for room* and picked up when the slow disk has space again.

## If the machine dies mid-move

A move is the three operations described above — the bytes land on the cold
tier, the source is removed, the symlink appears — and they cannot be one. Crash
between the last two and the file is still on disk but nothing points at it.
`<watch>/.just_cache-journal` — and fsyncs it — before anything moves, and clears the
record once the symlink is in place.

The next run reads that journal and asks the *filesystem* what happened, because the
journal only knows what was attempted:

- **bytes cold and complete, name gone** → the symlink is recreated, and the run says
  so (`restored the name shows/ep2.mkv -> /dev/shm/cold/shows/ep2.mkv`). This is the
  case the journal exists for.
- **both copies present** → left alone; the next sweep's adoption path hashes the
  destination and either adopts or refuses it. Recovery does not guess.
- **nothing moved yet** → nothing to do.
- **an interrupted copy, with the source intact** → removed (it is ours and worthless).
- **an interrupted copy with nothing else left** → *kept*, deliberately: deleting it
  could be deleting the only surviving bytes.
- **no source and no complete copy** → reported as `DATA LOST` and kept in the journal,
  because nothing on disk remains for `audit` to find later.

Two rules hold throughout: recovery never deletes anything that might be the only copy,
and it never creates a name pointing at a copy it cannot vouch for — a cold file whose
size does not match what the move promised is refused, not linked.

A journal that cannot be *read* stops the sweep, naming the damaged line: it may
describe bytes already on a cold tier, and there is no safe way to guess. The journal
itself is never a move candidate, and after a clean pass it is empty — a finished move
is cleared rather than recorded, since the symlink is better evidence that it finished.

## Example run

```
$ just_cache --watch /mnt/cache/media --dest /mnt/disk-slow/media --min-idle-days 30 --once -v
  skipped: idle for only 0h /mnt/cache/media/recent.txt
  moved /mnt/cache/media/shows/s1/ep1.mkv -> /mnt/disk-slow/media/shows/s1/ep1.mkv
pass 1: 3 files scanned, 3 tracked, 1 moved, 0 linked, 0 waiting for room, 0 in use (71 open files seen), 1 skipped (0 outside scope or size), 0 failed, 21 B onto the cold tiers
```

The counters are separate on purpose: a file someone is holding open is not the same
thing as a file that is merely too warm, and neither is the same as a file the scope
excludes.

`/mnt/cache/media/shows/s1/ep1.mkv` is now a symlink; opening it still reads the
episode.

## Running it continuously

```sh
# one sweep per hour, forever
just_cache --watch /mnt/cache/media --dest /mnt/disk-slow/media --interval 3600
```

Or schedule single sweeps from cron/systemd and use `--once` instead.

## Explaining one path

A sweep reports in aggregate, and `-v` buries each reason among every other file. When
the question is "why is *this* file still here?", `explain` answers it for one path, in
the order the mover evaluates — and it is read-only:

```sh
just_cache explain /mnt/cache/media/shows/s1/ep1.mkv \
  --watch /mnt/cache/media \
  --dest /mnt/disk-slow/media \
  --min-idle-days 30
```

```
explain: /mnt/cache/media/shows/s1/ep1.mkv
  scope:   managed: no --include set, so every path under the watched tree is fair game; 1.2 GiB is within the [0 B, unlimited) size window
  guards:  clear: not open by another process; 1 link(s), hardlinks refused; size unchanged since this scan
  policy:  last use 1735689600 (2025-01-01T00:00:00Z) via atime; idle 154.0 d against --min-idle-days 30.0; not a symlink; 0 witnessed access(es) in this run (pin 1)
  verdict: would move now -> /mnt/disk-slow/media/shows/s1/ep1.mkv (tier 0)
```

The four sections are the mover's own gates, in its own order, and the **outermost**
decisive reason wins: a file that is both out of scope and too warm answers with the
`--exclude` that excluded it and marks the later stages `not evaluated`, because that is
the gate a sweep never reaches.

- **scope** names the rule — `excluded by --exclude 'node_modules'`, `below the 1.0 MiB
  size floor (5 B)`, `not under any --include pattern (...)`.
- **guards** reports the pid holding a file open (when the `/proc` scan could see it), the
  link count, and whether the size moved since the scan.
- **policy** prints the last-use stamp and its *source* (atime, or the mtime fallback),
  the idle duration against `--min-idle-days`, and the access pin.
- **verdict** is `would move now`, `would move in N days`, or `would never move: <reason>`.

Already-migrated paths answer with the cold location and the tier the symlink resolves
into; a path that does not exist says so. `--json` prints the same content as a stable
document, for a UI or a test.

The exit code is the script contract: **`0` when the engine manages the path** (a sweep
would move it now, or it is already a symlink into a configured tier), **`1` when it
would not be moved** — out of scope, guarded, too warm, outside every tier, or absent —
and **`2` on a bad invocation. `1` is an answer, not an error: a warm in-scope file is
exit `1` on purpose.

```sh
# act only on files that are actually about to move
just_cache explain "$path" --watch /mnt/cache/media --dest /mnt/disk-slow/media && reclaim "$path"
```

## The catalog

`audit` inspects the tree; the catalog *records* it. `catalog sync` walks the watched
tree and every cold tier and ingests their current state into a SQLite file whose id for
each object is the BLAKE3 hash of its bytes — so identity survives a rename, dedup falls
out, and "does this file exist?" is a question the catalog can answer even for a tier that
is not mounted.

```sh
just_cache catalog sync \
  --watch /mnt/cache/media \
  --dest /mnt/disk-slow/media \
  --dest /mnt/disk-archive/media
```

The catalog defaults to `.just_cache-catalog.sqlite` beside the watch root (the walk
skips the `.just_cache` prefix, so it can never be moved onto a cold tier); `--catalog
<FILE>` puts it anywhere else. A file the mover already offloaded is picked up by the
first sync exactly like one offloaded afterwards — the migration story is ingest, not
migrate.

Each row records where the bytes are (a tier plus a storage key), their size, and the
BLAKE3 checksum computed at ingest. An object moves through `present` -> `offloaded` ->
`restoring`, and a location is only removed when another location still holds the object
or when no name references it. Cache residency is never written to `location` (§2.1 of the
design): a copy in a RAM or SSD promotion target is re-derivable, so a restart can never
turn a volatile copy into data of record.

`sync` **ingests new facts and reports contradictions; it never rewrites the catalog to
match a hand-edited tree**. If a name the catalog recorded is gone or now hashes to
different bytes, or a location's file vanished or changed, the difference is printed and
the rows are left alone, and the command **exits `1`** so cron can alert (`0` clean, `2`
bad invocation). An interrupted sync is rolled back whole — a catalog that disagrees with
the tree is worse than none.

## Auditing consistency

The mover leaves one of a few states behind, and they can drift apart: a crash between
"copy landed" and "source removed" leaves a duplicate, a deleted cold disk leaves a
dangling symlink, and so on. `audit` walks both the watched tree and every cold tier
(read-only) and classifies each relative path:

```sh
just_cache audit \
  --watch /mnt/cache/media \
  --dest /mnt/disk-slow/media \
  --dest /mnt/disk-archive/media
```

| Classification | The two sides of the path |
|---|---|
| `healthy` | a plain file with no cold copy, or a symlink resolving under a `--dest` |
| `duplicate` | a real file at the source path **and** a copy on a cold tier |
| `orphaned-copy` | cold bytes exist but the name at the source path is gone |
| `dangling-symlink` | the symlink at the source path points at nothing |
| `unexpected-target` | the symlink resolves, but outside every `--dest` |

The readable run prints counts plus the first `--examples` findings; `--json` prints a
stable machine-readable document with every finding. **The exit code is `1` whenever
findings exist**, so cron can alert without parsing text (`0` clean, `2` bad invocation
or unreadable tree).

```sh
# alert on any drift, without parsing anything
just_cache audit --watch /mnt/cache/media --dest /mnt/disk-slow/media || notify
```

`--repair` fixes what can be fixed without guessing:

- **duplicate** — the two copies are hashed (BLAKE3, the same digest the mover compares
  destinations with); only on a match is the source file removed and replaced by the
  symlink the mover intended. A mismatch is refused and both copies are left untouched.
- **dangling-symlink** — the link is re-pointed at the cold copy at the mirrored path,
  if one exists. No bytes are deleted.
- **unexpected-target** and **orphaned-copy** are reported, never silently changed:
  the target of the first still resolves, and the second needs a human to decide
  between restoring the name and reclaiming the cold bytes.

With `--repair` the exit code is `0` only when every finding was resolved, so a cron
job that keeps the tree healthy stays quiet.

### When a catalog exists

If a catalog is present — `--catalog <FILE>`, or the default
`.just_cache-catalog.sqlite` beside the watch root that `catalog sync` writes — `audit`
answers **from it** instead of walking both sides. The catalog supplies every cold-side
fact; one pass over the watched tree is still made, because a path the catalog does not
know is exactly what the walk must find:

| Classification | What the catalog says |
|---|---|
| `missing-copy` | a recorded location has no file (primary or replica) |
| `checksum-mismatch` | a primary copy's bytes no longer match the recorded checksum |
| `copy-floor` | an object has no surviving copy (`docs/design.md` §6) |
| `name-vanished` | the catalog records a name the tree no longer has |
| `unknown-version` | a symlink resolves to a version the catalog does not hold |
| `unknown-path` | a path the catalog has never seen — reported, never adopted |

`duplicate`, `orphaned-copy`, `dangling-symlink` and `unexpected-target` keep their meaning.
The summary says which source answered (`from catalog …` or `walked; no catalog`), so a
reader never has to guess. Without a catalog file the walk-based audit above is the
fallback, unchanged, and its `--repair` still checksum-verifies duplicates and re-points
dangling links. In catalog mode `--repair` **never rewrites**: it marks each finding for
resync and touches neither the tree nor the rows — the catalog may record a move the walk
sees as unfinished, and guessing is how a repair deletes the wrong thing. Exit codes are
unchanged (`0` clean, `1` findings, `2` bad invocation).

## Finding an object

`locate` answers "where does this live" straight from the catalog, without walking the
tree — so it works for an object whose tier is not mounted.

```sh
# by namespace path
just_cache locate shows/s1/ep1.mkv \
  --catalog /mnt/cache/media/.just_cache-catalog.sqlite

# by content digest, full or a prefix of at least 8 hex characters
just_cache locate 8a623e5d \
  --catalog /mnt/cache/media/.just_cache-catalog.sqlite
```

A query of at least eight hex characters is read as a digest prefix and matched against
object ids; anything else is read as a name. `--json` reports which reading (`kind`) it
chose. The answer names every copy and its tier, marks the tier of record (`[PRIMARY]`),
and reports the object's state (`present` / `offloaded` / `restoring`); an offloaded copy
is called out as needing its tier mounted to read. A prefix that matches several objects
lists them all — never a guess at which one was meant.

Exit codes: `0` found, `1` nothing found (an answer, not an error), `2` a missing or
unreadable catalog.

`explain` consults the same catalog when one exists — the default beside the watch root,
or `--catalog` — and **reports** a disagreement between the catalog and the filesystem
instead of silently resolving it. With no catalog it answers from the filesystem exactly
as before.

## Restoring a file

`restore` is the other half of the loop: it brings the bytes of an offloaded object back
to its hot path, verified, and collapses the symlink into a real file.

```sh
just_cache restore /mnt/cache/media/shows/s1/ep1.mkv \
  --watch /mnt/cache/media \
  --dest /mnt/disk-slow/media
```

The cold copy itself is found by *state*, not by the catalog: if the path is a symlink
that resolves, its target is the copy the mover left; otherwise — a broken link, or a
name that vanished — the mirrored relative path under each `--dest`, in order. If several
candidates hold **different** bytes for one name, restore refuses and names them rather
than guessing which tier is right.

The **verification**, though, prefers the catalog when one exists — `--catalog <FILE>`,
or the default `.just_cache-catalog.sqlite` beside the watch root that `catalog sync`
writes (consulted only if it is already there; `restore` never creates one). With a
catalog, the restored bytes are checked against the object's **recorded checksum**, so a
cold copy that was already corrupt is refused instead of restored faithfully, and a
catalog that does not name the path is a hard error rather than a silent skip of the
check. Without a catalog the bytes are verified against the cold copy itself — a torn
copy is still caught, a pre-corrupt one cannot be.

What it guarantees:

- **Verify before it goes live.** The bytes are copied into a `.just_cache-partial-*`
  sibling *in the hot directory*, fsynced, read back and hashed (BLAKE3), and only then
  renamed into place. The rename is same-directory and atomic, so a reader sees the old
  state or the whole file — never a truncated one, and a crash cannot leave a torn file at
  the path.
- **The cold copy stays by default.** `--remove-copy` drops it — but only *after* the
  restored copy has been verified (`docs/design.md` §6, verify-before-delete). A default
  restore therefore leaves a regular file beside its cold copy, which `audit` reports as a
  `duplicate`; that is the honest state of the tree, not a bug.
- **It is idempotent.** Restoring a path that already holds the right bytes is a no-op
  that exits `0`.
- **It never clobbers data.** A regular file at the path is compared to the cold copy: if
  the bytes are identical, nothing changes; if they differ — even at the same size, which a
  length-only check would miss — restore **refuses** and leaves both copies untouched. The
  hot file may be newer than the copy, and silently overwriting it is the one outcome this
  command must never produce.

Exit codes: `0` restored or already present, `1` refused or failed (no copy found,
mismatch, unverifiable), `2` bad invocation (missing directory, path outside `--watch`).

```sh
# free the cold tier once the bytes are safely back home
just_cache restore /mnt/cache/media/shows/s1/ep1.mkv \
  --watch /mnt/cache/media --dest /mnt/disk-slow/media --remove-copy
```

A named gap: with no catalog there is no recorded digest to check a cold copy against
*before* copying it, so a copy that was already corrupt would be restored faithfully —
the read-back catches a torn copy, and only a catalog can catch a corrupt one
(`docs/design.md` §9). With a catalog, restore verifies against the recorded checksum and
refuses a corrupt cold copy; without one, that limitation is inherent and named.

## Scrubbing for bitrot

`catalog sync` records what a copy *should* hash to; `scrub` is what proves it still does.
Every location in the catalog is read back and hashed against the object id (which is the
BLAKE3 checksum, so there is no second copy of the truth to drift), and a copy that no
longer matches is repaired from a sibling that verifies clean — or, if none exists, marked
damaged and reported, **never deleted**.

```sh
# verify every stored copy; --rate caps read throughput (KiB/s) so a busy tier keeps up
just_cache scrub --catalog /mnt/cache/media/.just_cache-catalog.sqlite --rate 2048

# see the damage before the tool touches anything
just_cache scrub --catalog /mnt/cache/media/.just_cache-catalog.sqlite --dry-run
```

What it guarantees:

- **A corrupt copy is repaired from a verified sibling.** The replacement is built beside
  the corrupt file under the `.just_cache-partial-*` marker, read back and hashed against
  the recorded checksum, and only then renamed into place — the same verify-before-delete
  path restore uses, run in the other direction. The good copy is never touched.
- **The last copy is never deleted.** If every copy of an object is corrupt, the object is
  recorded damaged (the catalog's `damage` table) and named in the output; the bytes stay
  exactly where they are for a human to recover or retire.
- **A missing copy is reported, not called rot.** An unmounted tier may hold perfectly good
  bytes, so a missing file is a finding, not damage; a tier root that does not exist is
  named once instead of once per location under it.
- **It resumes.** The last verification of every location is written to the catalog as the
  scrub goes (`scrub_state`), so a run that is killed part-way re-reads only what it had
  not reached, and `audit --catalog` can say "never scrubbed" for a copy instead of
  implying the catalog vouches for bytes nobody has read back.
- **`--dry-run` changes nothing.** It reads and reports what it would repair, writing
  neither the repair nor any last-verified state, so it is safe to repeat.

Exit codes: `0` clean, `1` corruption (repaired *or* damaged — rot is evidence about the
tier and cron should see it), `2` bad invocation (missing catalog file, `--rate 0`).

A named limit: `scrub` is on demand; it does not schedule itself (that is P2), and a
location verified once is skipped until its content changes under the catalog. `--rate`
is what makes a cron-driven scrub safe to run against a tier that is serving reads.

## Reconciling a re-added disk

A disk that is unmounted while a sweep runs misses its copies: the sweep places the file
only on the disks that are present, and the object stays below its floor (`catalog sync`
reports it as `under-replicated`, `audit --copies N` as `replica-lost`). When the disk
comes back, `reconcile` is what fills it in:

```sh
# see what would be rebuilt before anything is written
just_cache reconcile --catalog /mnt/cache/media/.just_cache-catalog.sqlite --dry-run

# rebuild every missing copy a surviving sibling can supply
just_cache reconcile --catalog /mnt/cache/media/.just_cache-catalog.sqlite
```

What it guarantees:

- **The source is proved, not assumed.** A sibling is used only after its bytes are hashed
  and compared to the object's recorded checksum — a copy that merely matches on size is
  not a source, and one that hashes to something else is marked damaged and skipped.
- **The rebuilt copy is verified before it is recorded.** The bytes go to a private
  `.just_cache-partial-*` sibling, are read back and hashed there, and only a verified
  copy is renamed into place; only then is the location recorded, with the real checksum.
  A failed rebuild leaves the location absent, never holding unknown bytes.
- **Nothing is deleted.** Not the source of a rebuild, not a copy restored by hand that
  already sits where the copy belongs (that one is hashed and *adopted* instead), not a
  file reconcile cannot vouch for.
- **A destination root is never created.** A disk that is still unmounted is reported
  (`tier not mounted`), not silently turned into a directory on the wrong filesystem.

Exit codes: `0` every recorded floor already met, `1` something was rebuilt/adopted or
could not be (a disk being out is evidence cron should see once), `2` bad invocation — a
missing catalog file, since the recorded checksum a rebuild is proved against lives there.
Like `scrub`, it runs on demand; deciding *when* to reconcile is P2.

## Building and testing

```sh
cargo build --release
cargo test        # unit + integration tests against real temporary trees
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

Tests cover the walk (nested directories, symlink loops), the move (nested layout,
existing symlinks, resumed moves, size conflicts, same-named files in sibling
directories, a source that changed since the scan), the policy (idle thresholds,
oldest-first ordering, access pins, dry runs, full tiers), scope (include/exclude globs
and the size window), the guards (a real second process holding a real descriptor, and
hardlinked pairs), metadata and sparseness across a real mount point, journal recovery
(each crash state, plus the CLI against a damaged journal), audit (every
classification, the checksum-guarded repair, and the exit-code contract cron sees),
`explain` (the four-stage ordering, a real holding process named by pid, the migrated and
missing cases, and the exit-code contract through the binary — `--json` included),
`restore` (the round trip, idempotence, repairing a broken symlink, refusing a same-size
stranger, `--remove-copy`, and a genuine cross-device restore), and `scrub` (a hand-corrupted
copy repaired from its sibling, a last surviving copy marked rather than deleted, `--dry-run`,
resume via last-verified state, a measured sparse-file check, and `--rate` pacing).

A few of these drive the actual binary rather than the library, because the promises that
matter — exit codes, recovery messages, refusing to sweep with an unreadable journal —
are promises the command line makes.

### Testing the cross-device move

A cold tier is a different mount by definition, so out in the world `rename` always
fails with `EXDEV` and the move falls back to copy-then-delete. That fallback is the
only path that can leave a partial file behind, so it is worth testing — but the rest
of the suite keeps source and destination on one filesystem and never reaches it.

The cross-device tests (`tests/cross_device.rs`) need a directory on a *second*
filesystem. Point `JUST_CACHE_TEST_SECOND_FS` at one — a tmpfs is easiest:

```sh
sudo mkdir -p /mnt/just_cache_test
sudo mount -t tmpfs -o size=256m tmpfs /mnt/just_cache_test

JUST_CACHE_TEST_SECOND_FS=/mnt/just_cache_test cargo test
sudo umount /mnt/just_cache_test   # when you are done
```

The tests compare the `st_dev` of the source and destination and refuse to run unless
they really differ, so pointing the variable at an ordinary directory cannot turn them
into a silent same-filesystem no-op. With no second filesystem available they **skip**
(and say so on stderr) rather than fail, because a developer machine is not expected to
have a spare mount. CI does have one — the workflow mounts a tmpfs — so it also sets
`JUST_CACHE_REQUIRE_SECOND_FS=1`, which turns that skip into a hard failure. That way a
broken mount in CI can never leave the job green while the coverage quietly disappears.

They check byte-for-byte equality of the copied file, that reading through the symlink
still works and resolves to the cold copy, and that a source changing under the mover is
refused before any bytes are written — leaving the source untouched with nothing at the
destination and no `.just_cache-partial-*` temporary to clean up.

One gap is admitted rather than papered over: a source that changes *during* a long copy
has no automated test, because arranging that means racing a thread against the copy, and
a test that passes only when the race is lost is worse than no test. That path's cleanup
is a single `remove_file` beside the copy loop, and its comment says so.

A sibling set of tests for metadata preservation and interruption/repair uses the same
`tests/support` helpers and the same two environment variables.

## How this codebase is built

`just_cache` is written by AI coding agents (Hermes Agent subagents working in parallel),
directed and reviewed by a human. Saying so plainly changes what to assume when reading a
commit: nobody typed this from memory of the failure it prevents, so the reasoning lives in
the comments and in [`docs/design.md`](docs/design.md), and an explanation that is not
there is worth asking about.

The workflow that is meant to keep that trustworthy:

- **One issue, one worktree, one branch, one writer.** Each unit of work starts as an
  issue and gets its own worktree on its own branch. Two agents are never pointed at one
  checkout: concurrent writers produce a tree that looks coherent and that no single agent
  ever verified.
- **A PR per issue, squash-merged.** Conventional commit subjects. The PR body states what
  was verified, and an issue whose acceptance criteria were not all met stays open with the
  unmet criterion named, rather than closing on the strength of a passing test run.
- **CI is the gate, not a green local run.** Format, clippy (`-D warnings`), build and the
  full suite run in [`.github/workflows/rust.yml`](.github/workflows/rust.yml). The
  workflow mounts a second filesystem so the cross-device tests reach the copy path, and
  sets `JUST_CACHE_REQUIRE_SECOND_FS` so a missing mount fails instead of skipping — a skip
  and a pass look identical in a test summary. It also rejects conflict markers committed
  as text, which is a mistake no compiler or test can catch in markdown.
- **Claims about the work are read from the remote, not from the summary.** "Done" means the
  branch is on `origin`, the PR is open and CI is green — checked, not asserted. A branch
  that is only committed locally counts as unfinished, because a scratch directory is one
  cleanup away from taking the work with it.
- **Gaps are named where the next reader will find them.** An untested path, an unverified
  assumption, a question only a human can answer: those go into `docs/design.md` §9 and the
  readme, not into a commit message.

[`AGENTS.md`](AGENTS.md) carries the conventions in full, including the invariants a change
must not break — the rules that make this tool safe to point at someone's data.

## Layout

| Path | Contents |
|---|---|
| [`src/disk_management.rs`](src/disk_management.rs) | Walking the tree, moving a file, making the symlink, reading free space. |
| [`src/scope.rs`](src/scope.rs) | Which paths the tool may touch at all: include/exclude globs and the size window, plus size parsing. |
| [`src/journal.rs`](src/journal.rs) | What the mover was in the middle of: the intent record, and recovery from an interrupted move. |
| [`src/catalog.rs`](src/catalog.rs) | The SQLite catalog — content-addressed locations, names, and the transactional `sync` that ingests the tree. |
| [`src/opened.rs`](src/opened.rs) | Whether something is using a file right now: open descriptors and hard links. |
| [`src/file_movement.rs`](src/file_movement.rs) | `UsageTracker` and the policy that picks cold files, plus the report types. |
| [`src/audit.rs`](src/audit.rs) | Classifying the watched tree against the cold tiers, and the guarded `--repair`. |
| [`src/scrub.rs`](src/scrub.rs) | Reading every stored copy back, repairing rot from a verified sibling, marking what cannot be repaired. |
| [`src/reconcile.rs`](src/reconcile.rs) | Rebuilding a copy that is missing from a re-added destination root, from a sibling proved against the recorded checksum. |
| [`src/explain.rs`](src/explain.rs) | Answering, in the mover's evaluation order, why one path is where it is — with the catalog seam for issue #16. |
| [`src/digest.rs`](src/digest.rs) | BLAKE3 content digests, streamed — one answer to "are these the same file" for the whole tool. |
| [`src/main.rs`](src/main.rs) | The CLI (`sweep`, `audit`, `catalog`, `explain`, `locate`, `restore`, `scrub` and `reconcile` subcommands) and the sweep loop. |
| [`tests/migration.rs`](tests/migration.rs) | End-to-end behaviour against temporary trees. |
| [`tests/cross_device.rs`](tests/cross_device.rs) | The EXDEV copy fallback, against a real second filesystem. |
| [`tests/support/mod.rs`](tests/support/mod.rs) | Shared helpers for tests that need a second filesystem. |
| [`tests/audit.rs`](tests/audit.rs) | Audit classifications, repair, and the CLI exit-code contract. |
| [`tests/explain.rs`](tests/explain.rs) | `explain`'s ordering guarantee, exit codes and `--json`, through the binary. |
| [`tests/restore.rs`](tests/restore.rs) | Restore through the binary: round trip, idempotence, broken-link repair, mismatch refusal, `--remove-copy`, and a cross-device restore. |
| [`tests/scrub.rs`](tests/scrub.rs) | Scrub through the binary: a hand-corrupted copy repaired, a last copy marked not deleted, `--dry-run`, resume, sparse-file measurement, and `--rate` pacing. |
| [`tests/reconcile.rs`](tests/reconcile.rs) | Reconcile through the binary: a re-added disk rebuilt from a sibling across a mount point, refusals for same-size/corrupt siblings, a still-out root never created, adoption of a hand-restored copy, and `--dry-run`. |
| [`tests/journal.rs`](tests/journal.rs) | Recovery from each crash state, and the CLI around a damaged journal. |
| [`tests/catalog.rs`](tests/catalog.rs) | Catalog ingest, the lifecycle states, and the report-don't-rewrite contract, plus CLI exit codes. |
| [`tests/preserve_metadata.rs`](tests/preserve_metadata.rs) | Mode, ownership, xattrs, mtime and sparseness across a real mount point. |

## Design

This tool is one piece of a larger idea. [`docs/design.md`](docs/design.md) sketches it:
one namespace over every storage tier you own — SSD, spinning disks, a LAN peer, cloud
object storage, an offline disk in a drawer — with a fast cache overlay (RAM) in front
of them, placement driven by observed use, and promotion on re-access, in the spirit of
S3 storage classes. The mover in `src/` is the symlink namespace provider and the
hot→warm driver of that design, not the whole of it.

## License

MIT — see [LICENSE](LICENSE).

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

## Why

Caches, media libraries and build directories fill up with files nobody has opened in
months. Deleting them loses data; leaving them wastes expensive flash. `just_cache`
buys back the fast tier while every script, playlist and application path keeps
resolving, because the file is still there — it is just a symlink now.

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
| `--min-idle-days <DAYS>` | `30` | Only touch files last used at least this long ago. |
| `--min-observed-accesses <N>` | `1` | Pin a file once it has been read `N` times *during this run*; `0` disables the pin. |
| `--limit <N>` | `10` | Most files moved per destination per sweep. |
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

**Destinations are never created.** A missing `--dest` is a startup error, not a
`mkdir`: if a slow disk is unmounted, silently creating its mount point would have the
tool write terabytes into a directory on the wrong filesystem.

With several destinations, sweeps fill the fastest tier first and only spill onto the
next when a tier is out of room (or has hit `--limit`).

## How a file is chosen

A file is moved when **all** of these hold:

0. it is in scope (§above): inside `--include`, not `--exclude`, within the size window;
1. it is not already a symlink (already migrated);
2. it is not empty;
3. nothing has read it during this run (the `--min-observed-accesses` pin);
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

## Example run

```
$ just_cache --watch /mnt/cache/media --dest /mnt/disk-slow/media --min-idle-days 30 --once -v
  skipped: idle for only 0h /mnt/cache/media/recent.txt
  moved /mnt/cache/media/shows/s1/ep1.mkv -> /mnt/disk-slow/media/shows/s1/ep1.mkv
pass 1: 3 files scanned, 3 tracked, 1 moved, 0 linked, 0 waiting for room, 1 skipped, 0 failed, 21 B onto the cold tiers
```

`/mnt/cache/media/shows/s1/ep1.mkv` is now a symlink; opening it still reads the
episode.

## Running it continuously

```sh
# one sweep per hour, forever
just_cache --watch /mnt/cache/media --dest /mnt/disk-slow/media --interval 3600
```

Or schedule single sweeps from cron/systemd and use `--once` instead.

## Building and testing

```sh
cargo build --release
cargo test        # unit + integration tests against real temporary trees
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

Tests cover the walk (nested directories, symlink loops), the move (nested layout,
existing symlinks, resumed moves, size conflicts, same-named files in sibling
directories), and the policy (idle thresholds, oldest-first ordering, access pins, dry
runs, full tiers).

## Layout

| Path | Contents |
|---|---|
| [`src/disk_management.rs`](src/disk_management.rs) | Walking the tree, moving a file, making the symlink, reading free space. |
| [`src/scope.rs`](src/scope.rs) | Which paths the tool may touch at all: include/exclude globs and the size window, plus size parsing. |
| [`src/file_movement.rs`](src/file_movement.rs) | `UsageTracker` and the policy that picks cold files, plus the report types. |
| [`src/main.rs`](src/main.rs) | The CLI and the sweep loop. |
| [`tests/migration.rs`](tests/migration.rs) | End-to-end behaviour against temporary trees. |

## Design

This tool is one piece of a larger idea. [`docs/design.md`](docs/design.md) sketches it:
one namespace over every storage tier you own — SSD, spinning disks, a LAN peer, cloud
object storage, an offline disk in a drawer — with a fast cache overlay (RAM) in front
of them, placement driven by observed use, and promotion on re-access, in the spirit of
S3 storage classes. The mover in `src/` is the symlink namespace provider and the
hot→warm driver of that design, not the whole of it.

## License

MIT — see [LICENSE](LICENSE).

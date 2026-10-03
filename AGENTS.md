# Working in this repository

Orientation for coding agents (and anyone else) making changes to `just_cache`. It covers
what the code expects of you, not what the tool does — that is [`readme.md`](readme.md),
and the reasoning behind the design is [`docs/design.md`](docs/design.md).

## Build, test, verify

```sh
cargo build
cargo fmt --all
cargo clippy --all-targets -- -D warnings   # CI fails on any warning
cargo test
```

If `cargo` is not on `PATH`, this machine keeps the toolchain at `~/.cargo/bin`.

Two local accelerators sit in front of those commands, both wired in so the plain
commands still work but the fast ones are what the machine actually runs:

- **`cargo nextest run` in place of `cargo test`.** nextest compiles and runs the test
  binaries in parallel — measured at ~21s against `cargo test`'s ~49s on a cold cache for
  the same tests. It does not run doctests, so `cargo test --doc` is a separate command;
  CI runs both, and a doctest added later is covered without anyone remembering to look.
- **sccache**, set as `RUSTC_WRAPPER` in `~/.cargo/config.toml`, caches compiled objects
  across every worktree. A build into a fresh `target/` — which is what each new branch
  pays — drops from ~15s to ~6s. sccache requires `CARGO_INCREMENTAL=0`, which that config
  sets: an edit-build of this crate becomes ~3s instead of ~1s, the trade the config
  explains. If sccache is not installed the wrapper fails loudly rather than building
  uncached, so an unconfigured machine says so instead of quietly slowing down.

CI (`.github/workflows/rust.yml`) runs all four, and mounts a tmpfs so the cross-device
tests have a real second filesystem. Reproduce that locally when touching the mover:

```sh
JUST_CACHE_TEST_SECOND_FS=/dev/shm JUST_CACHE_REQUIRE_SECOND_FS=1 cargo test
```

`JUST_CACHE_REQUIRE_SECOND_FS` turns "no second filesystem, skipping" into a failure, which
is what CI sets. Without it, tests that need one skip — and a skip looks exactly like a pass
in the summary line, so do not read a green run as coverage unless that variable was set.

**A failure window inside the binary is opened with `JUST_CACHE_FAULT`.** A window a test
process cannot time from outside — a destination vanishing between the copies of one sweep, a
read-back failing on bytes that were just written, a partial file observed while bytes are
still moving — is injected by setting `JUST_CACHE_FAULT` to a `mechanism=N` spec. The grammar
and the one-shot claims are in `src/faults.rs`; the seams it arms fire in `src/replication.rs`
and `src/disk_management.rs`. The variable is inert unless set, and a set-but-malformed spec
panics deliberately: a fault switch that silently no-opped would let a mistyped test pass
green with nothing injected. `tests/fault_injection.rs` is the worked example.

**Unit tests are not evidence that a change works.** The mover deletes people's files, and
several of the bugs found while building P0 were invisible to a passing suite: a regression
in short-copy detection, a 57-byte-per-move journal leak, an unreadable-journal path. Run the
binary against two real filesystems before claiming a move is safe:

```sh
mkdir -p /dev/shm/tier && printf 'payload\n' > tree/file.bin && touch -a -d '2026-01-01' tree/file.bin
./target/debug/just_cache --watch "$PWD/tree" --dest /dev/shm/tier --min-idle-days 30 --once -v
```

`/dev/shm` is a different filesystem from the workspace, so the move genuinely takes the
`EXDEV` copy path rather than `rename`.

## Invariants

These are the rules that make the tool safe to point at someone's data. A change that breaks
one is a bug regardless of what the tests say, and the comments that explain each of them are
worth reading before editing the code around them.

1. **A destination root is never created.** A missing `--dest` is an error. An unmounted disk
   must not silently become a directory on the wrong filesystem.
2. **Nothing is deleted without a verified copy.** Adoption requires a matching digest; a
   same-size-but-different destination is refused. Verify-before-delete is the whole reason
   `src/digest.rs` exists.
3. **The mover refuses anything it does not fully understand**: a file that changed size since
   the scan, one that something holds open, one with more than one hard link (unless
   `--allow-hardlinked`). In doubt, leave the file alone and report it.
4. **Scope is the outermost gate.** It is checked before usage, size or policy — and again
   immediately before bytes move. New filters belong inside scope, not beside it, so a file
   the user excluded can never be caught by a rule that grows more eager.
5. **Journalling happens before the move, not after.** The intent record is fsynced before
   anything moves; a record still in the page cache when the machine dies was never written,
   and the window it covers is exactly that one.
6. **Recovery never deletes anything that might be the only copy**, and never creates a name
   pointing at a copy it cannot vouch for. An interrupted copy with nothing else left is kept.
7. **One file failing never stops a sweep.** Failures are per-file and reported; `--once`
   exits nonzero if any occurred.
8. **The journal and `.just_cache-partial-*` files are never move candidates.** The walk skips
   that prefix; anything else written into the watched tree needs the same treatment.
9. **Nothing is created in the watched tree without being asked for.** The journal and the
   catalog are the only files the tool leaves there, both under the `.just_cache` prefix
   the walk skips, and both empty-or-idle after a clean pass. The catalog is only created
   when `catalog sync` is run (or `--catalog` names it elsewhere), never by a sweep.

## Conventions

- **Comments explain why, not what.** The existing style is heavy on rationale — the reasoning
  that stops the next person from "simplifying" a guard away. Match it; a comment that restates
  the code is noise, one that explains the failure it prevents is the point.
- **Tests that pass when the feature is broken are worse than no test.** Where a real failure
  cannot be produced deterministically, say so in a comment and in `docs/design.md` §9 rather
  than writing something that only looks like coverage.
- **Name gaps instead of hiding them.** Anything not done — untested paths, unverified
  assumptions, a hard stop that needs a human — is recorded where the next reader will find it.
- **Append §9 entries at the end of the list.** A new "Closed by #NN" paragraph in
  `docs/design.md` §9 goes after the last existing entry, not before a
  `Still open, and honestly so:` heading — the headings are fixed anchors, never an
  insert point. With the heading as anchor, every branch in a parallel wave inserted at
  the same line and the second to land always conflicted; the moving tail of the list
  keeps entries in merge order, and a rebase re-appends after the entries that landed.
  Two branches off the same base still meet at the tail; resolve by keeping both
  entries, in the order they merge.
- **Conventional commits** (`feat:`, `fix:`, `docs:`, `ci:`, `test:`, `chore:`) with a body that
  explains the change and its verification.
- **Branch, then PR — never commit on `main`.** Even in a repo where you own every commit:
  undoing an accidental push to `main` costs a force push, which is worse than the PR it
  skipped. Check `git branch --show-current` before committing if you have several worktrees
  checked out. A ruleset now enforces this: `main` requires a PR and a green `build` check,
  and rejects force pushes and branch deletion.
- **Cleanup after a squash merge is ordered, not careful.** Merge from outside the PR's
  worktree, then `git worktree remove <path>`, then `git branch -D <branch>`. The repo
  auto-deletes the remote head branch on merge. Lowercase `-d` will always refuse here:
  a squash commit shares no history with the branch, so every branch looks unmerged to
  it — `-D` after a squash merge is expected and safe, because the content is already on
  `main` under the squash commit. Deleting the branch *before* removing its worktree
  triggers an avoidable approval prompt on destructive-looking commands.
- Commits are authored as the repository owner, not as an agent.
- Pushes touching `.github/workflows/` need the token's **Workflows** permission (fine-grained
  PATs have no `workflow` scope); without it the push is rejected and the change is stranded.

## Layout

| Path | Responsibility |
|---|---|
| `src/main.rs` | CLI (clap subcommands), the sweep loop, exit codes. |
| `src/disk_management.rs` | The walk, the move, symlink creation, metadata/sparseness, free space. |
| `src/faults.rs` | The test-only `JUST_CACHE_FAULT` seam: parsing, and the one-shot mid-copy claim. Inert unless the variable is set. |
| `src/file_movement.rs` | `UsageTracker`, the policy that selects candidates, the report types. |
| `src/scope.rs` | Eligibility: include/exclude globs, size window, size parsing. |
| `src/opened.rs` | Live state: open descriptors (one `/proc` snapshot per sweep) and hard links. |
| `src/journal.rs` | The intent record and recovery from an interrupted move. |
| `src/catalog.rs` | The SQLite catalog: content-addressed locations, names, and the transactional `sync`. |
| `src/scrub.rs` | Reading every stored copy back, repairing rot from a verified sibling, and marking what cannot be repaired. |
| `src/reconcile.rs` | Rebuilding a copy that is missing from a re-added destination root, from a sibling proved against the recorded checksum. |
| `src/audit.rs` | Audit from the catalog (one namespace pass) or, without one, the walk-based fallback; the guarded `--repair`. |
| `src/digest.rs` | BLAKE3, streamed. One digest for the whole tool. |
| `tests/` | Integration tests against real temporary trees; `tests/support/` for the shared second-filesystem helpers. |

P0–P2 (the phase list in `docs/design.md` §8) are complete. P3 — remote tiers — is next; §9 lists
what is still open, and anything you add there should be added to §9 and §10 rather than left
in a commit message.

# Security Policy

## Supported versions

The current release is [`v0.5.0`][current] — the standalone symlink mover with the
catalog, the namespace provider, the lifecycle engine, the remote tiers, the MCP server
and the read-only web dashboard, published as a normal release. Only the current `main` is
supported: a fix lands there, and the tag is superseded rather than patched in place —
there is no maintenance branch. A fix on `main` is not in a published tag until the next
one, so running the release means running fixed-on-main, tagged-sometimes.

[current]: https://github.com/dotJoel/just_cache/releases/tag/v0.5.0

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for anything that can be exploited:
<https://github.com/dotJoel/just_cache/security/advisories/new>, reached from the
*Report a vulnerability* button under the [Security tab][security-tab]. A report filed
there is private until a fix is published.

A public issue is the right place for a bug that has no attacker in it — including data
loss, which is the failure class this tool is most exposed to.

What to expect: this is a single-maintainer project with no support commitment. Reports
are read and answered as time allows, and there is no response-time or fix-time promise.

[security-tab]: https://github.com/dotJoel/just_cache/security

## Scope

`just_cache` moves and deletes files, so the failures that matter here destroy data or
touch files the operator did not point it at. In scope:

- bytes deleted without a verified copy, or a name created for a copy the tool cannot
  vouch for (invariants 2 and 6 in [`AGENTS.md`](AGENTS.md));
- a file moved, renamed or removed outside `--watch` or outside the
  `--include`/`--exclude`/size scope, including by way of a `--dest` root that is not
  what it appears to be (invariant 4);
- a path built from `..`, a symlink target, a catalog row or a journal line that escapes
  the watched tree or a destination root;
- a malformed journal, catalog or command-line value that makes the tool act on paths it
  did not read, or that lets `restore`, `scrub` or `reconcile` overwrite good bytes with
  unverified ones (invariants 5 and 8);
- the access-time read, or the `/proc` scan, being trusted far enough to move a file that
  is in use.

Out of scope: the filesystem's own behaviour; the tool run as root against a tree someone
else can write to (the operator chooses both the privileges and the tree); and bugs in a
dependency that `just_cache` does not expose. Report those upstream — Dependabot watches
the lockfile here and opens a weekly update PR, but that is the only thing that files
them; it is not a report of what affects this tool.

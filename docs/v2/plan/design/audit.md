---
status: plan
summary: Dependencies are untrusted by default. A git source builds only from a tree whose BLAKE3 digest `blaze.lock` pins, since a SHA-1 commit id is no pin. `blaze audit` checks every locked package against a pinned RustSec database and against yanked versions, and every source for a strong pin, with one finding per package and reason, each with its chain, fix and manifest line.
date: 2026-11-13
---
# `blaze audit` and strong source pins

Objective SEC1. The code is in `crates/blaze-audit`, which is sans-IO: it parses, matches
and decides. The drivers live in `crates/blaze-cargo/src/cli/audit.rs` (the command),
`crates/blaze-units/src/load/source.rs` (the load's refusal) and
`crates/blaze-cargo/src/serve/fetch/git.rs` (the fetcher's verification).

## Threat model

Everything a build reads from outside the workspace is untrusted until a digest blaze checked
covers it.

- **A git source moved under its commit.** SHA-1 is broken for collisions (SHAttered in 2017,
  chosen-prefix in 2020), and a host can serve any objects it likes. A commit id therefore pins
  nothing: the tree's BLAKE3 digest does.
- **A registry package changed after publication.** Its `.crate` sha256 is in `Cargo.lock`.
  A registry package with no checksum is refused.
- **A path dependency outside the workspace.** It reads a directory nothing pins. It is refused
  unless `allow-unpinned-paths` names it. `blaze new --bstd` writes one, so the refusal is
  `blaze audit`'s, not the build's.
- **A known-bad version.** This covers a RustSec vulnerability or an `unsound` advisory
  (errors), `unmaintained` or `notice` advisories (warnings), and a yanked version (an error).
- **A tampered advisory database.** The database is a pinned input like any other. Its tree is
  hashed and checked before any advisory in it is parsed.
- **Hostile bytes.** Every reader has a named size bound (the lock, an advisory, the database,
  an index file, a tree) and refuses past it. A symlink that leaves its tree is refused. A name
  becomes a path only after its characters are checked.

Not covered yet: build scripts and proc macros are sandboxed separately, and reachability of
an advisory's affected functions is not checked, only listed.

## `blaze.lock`

The pins go in the one file that holds every pin that is not a crate (docs/cargo.md, Lock
files). Two tables are added:

```toml
[git."https://github.com/o/r"]
0123456789abcdef0123456789abcdef01234567 = "blake3:<64 hex>"

[advisories]
commit = "<40 or 64 hex>"            # with url, when fetched; both or neither
tree = "blake3:<64 hex>"
url = "https://github.com/rustsec/advisory-db"
```

- Rows are sorted by url and then by commit. A commit must be 40 or 64 lower-case hex digits.
- A digest is `blake3:`, `sha256:` or `sha512:`. `md5` and `sha1` are refused by name.
- Pins are recorded only by `blaze audit pin`. `blaze lock` carries them through unchanged.

**The tree digest.** It is the `blaze tree 2` listing of blaze-fetch: one line per file, sorted
by path bytes, `<file|exec|link> <BLAKE3> <len> <path>`. A symlink's bytes are its target.
Nothing in `.git` is listed, and neither are `.cargo-ok` or `.blaze-ok` at the root. The
digest is the pin's algorithm over the listing.

## Where the pin is checked

- **Load** (`Sources::git_package`). A git package whose `(url, commit)` has no pin is refused
  before its checkout is read or fetched. The error is rule `weak_source_pin` and names the
  package, url, commit and `blaze audit pin`. The pin travels in `FetchKind::Git { commit,
  tree }` and `PackagePlan::tree`.
- **Fetch** (`git::checkout`). The fetched tree is hashed and compared with the pin before
  anything is mounted. A mismatch names both digests.
- **Sealing** (`sealed::pin`). A git package is sealed only with a tree digest. Its pin hashes
  the commit and the tree together, so `.git/HEAD`, which is outside the digest, is still
  covered.
- **`blaze audit`** hashes each checkout it finds on disk and compares it with its pin.

## The audit

- **Parsing.** `Cargo.lock` formats 3 and 4 are read with dependency strings resolved exactly.
  Advisories use the RustSec format: a TOML front block in Markdown, with unknown keys refused.
  Only `crates/<package>/<id>.md` is read, and duplicate ids are refused.
- **Matching.** A version is affected when no `patched` range and no `unaffected` range admits
  it, by cargo's semver rules. A pre-release is admitted only by a comparator of the same
  `major.minor.patch`. A blaze-sim refinement test checks this against a brute-force model, with a
  planted bug the test must catch.
- **Policy** (`[workspace.metadata.blaze.audit]`):
  - `database = "<dir>"` names the database; `BLAZE_AUDIT_DB` overrides it.
  - `allow-unpinned-paths = ["pkg"]`.
  - `accept = [{ id, reason, until }]`. An acceptance needs a reason and an end date. After
    `until`, the finding is an error again and says it expired. An acceptance that matches
    nothing is reported as stale (a warning).
- **Findings.** There is one finding per (package, version, rule, advisory). Each gives:
  - its severity and rule;
  - the shortest chain from a member, by breadth-first search in name order;
  - the least patched version above the current one, and whether it is semver-compatible;
  - the line to change: the member's manifest line for a direct dependency, otherwise "only
    `Cargo.lock` moves";
  - the affected functions, aliases, CVSS and url.
- **Not checked.** The report lists what it did not check, never as a pass: no database set, a
  registry with no index on disk, and reachability.
- **Order.** The output is sorted by (severity, package, version, rule, advisory) and
  deduplicated. A blaze-sim test shuffles the lock's packages and checks that the report does not
  change.

## Commands and output

- **`blaze audit [--json] [--today YYYY-MM-DD]`** runs the check. It exits nonzero on any
  error. `--today` fixes the date acceptances expire against, so a report can be reproduced
  byte for byte. The JSON has `version: 1`, with these fields: `today`, `ok`, `errors`,
  `warnings`, `accepted`, `packages`, `database {path, tree, advisories, withdrawn, newest,
  age_days}`, `not_checked`, `pinned`, `findings`.
- **`blaze audit pin`** pins trust at first use:
  - Each git source is fetched fresh into the fetcher's scratch and hashed, and the database
    is hashed.
  - Each digest is printed, along with the moves (`+`, `~`, `-`), and the person is told to
    review the diff.
  - `[git]` ends up exactly the lock's git sources. A commit whose recorded tree differs from
    the fresh one is refused, not overwritten.
- **`blaze audit fetch`** is the database fetch, as a machine (`Effect`/`Event`). Its driver
  answers `Refused::NotWired` until the git and HTTP drivers land. Until then, clone the
  database by hand and pin it.

## What is still open

- **Checkouts already on disk.** A checkout already on disk, or recovered from a mount, is not
  hashed again at load. The fix is for the fetcher to record the digest in `.blaze-ok` and for
  the load to compare against it.
- **Advisory scope.** Advisories are matched against crates.io packages only (RustSec's
  scope).
- **Transitive path dependencies.** Outside path dependencies are found among the members'
  direct dependencies only.

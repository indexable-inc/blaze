# Releases: canary and stable

blaze has two release channels, made of the same releases:

- **canary** (opt-in): every release, as soon as it is built. A release is cut from a main whose
  merge gate (`ci`) is green: the unit tests, the build, and the fast part of the smoke corpus
  below. It is published as a GitHub *prerelease*.
- **stable** (the default): a canary that then passed the heavy tests, unchanged. Promotion
  builds nothing: the archives users get on stable are the bytes that were tested as canary.

## Versions

A release is `v0.YYYYMMDD.N` (`0.YYYYMMDD.N` in Cargo.toml and `blaze --version`), N counting
the releases of that day (UTC) from 0. `.github/scripts/train_release.py` picks the number.

A version is one release on one tag, for good: **promotion flips that same release** from
prerelease to stable and makes it GitHub's *Latest*. No second tag is made and no asset is
uploaded again, so a pin to a version (`--version 0.20261028.0`, or a workspace's
`[workspace.metadata.blaze] version`) names the same bytes before and after promotion. What
moves is two pointers:

| channel | which release | how it is found |
|---|---|---|
| stable | the newest promoted release | GitHub's `releases/latest`, which never points at a prerelease |
| canary | the newest `v0.YYYYMMDD.N` release, prerelease or not | the repository's release feed (`releases.atom`) |

Canary is the newest release of either kind: every stable release was a canary first, so when
the newest canary is promoted the two channels name the same release.

Each channel also has a pointer, for what reads a channel without GitHub's API (a workspace
pinned to `"stable"` or `"canary"`): the assets `stable` and `canary` of the release `channels`,
each the channel's version and a newline,
`https://github.com/indexable-inc/blaze/releases/download/channels/stable`. release.yaml moves
`canary` when it has published a release, and promote.yaml moves `stable` when it makes a
release stable and *Latest* (`.github/scripts/promote.py pointer`).

A release older than this scheme (`v0.20261027.0`, the `nightly` build of 2026-10-20) was
published as a full release and counts as stable.

## Cutting a canary

Every green main is a canary, cheaply. `canary.yaml` runs when the merge gate's run on a push to
main ends green (and hourly, and by hand): `.github/scripts/canary.py cut` tags the newest
commit of main whose `ci` check is green (main moves faster than its gate, so that is seldom
HEAD itself; it looks through main's first parents since the newest release) `v0.YYYYMMDD.N`
through the API and dispatches `release.yaml` on the tag, when

- something besides the docs (`docs/`, `site/`, `tools/agents/`, `*.md`) and the bots' own commits
  changed from the newest release to that commit,
- no `release-cut.yaml` is running and no `release.yaml` is building a release (a run on a
  branch is not one), and the newest release is at least 30
  minutes old (`-f min-minutes=N` by hand). A green main inside that window is cut by the next
  green push or the hourly run.

No commit goes to main, so a cut never races a merge, and a canary is the bytes of a commit
that passed the gate as it is. release.yaml stamps the tag's version into its own checkout
(Cargo.toml's workspace version and the workspace's entries of Cargo.lock, `canary.py stamp`),
so `blaze --version` and the archives' names are the tag's, and writes the notes: CHANGELOG.md's
[Unreleased] and the title of each change merged since the previous release. Main's Cargo.toml
keeps the version of the last release commit.

`release-cut.yaml` (by hand, on main) or the release train's `release` job still cuts a release
the old way: it commits `release: v0.YYYYMMDD.N` (the version, a CHANGELOG.md section, Cargo.lock,
tools/nix/units) and tags it, and that release's notes are its CHANGELOG.md section. Both count every
tag, so they never take the same number.

Either way `release.yaml` builds the archives on the tag for macOS (Apple silicon) and Linux
(x86-64, ARM64) and publishes them as a prerelease with `SHA256SUMS`. PGO is off for now (#870):
the release is built with `--no-pgo`.

## Promotion to stable

`release.yaml` starts `promote.yaml` once it has published the prerelease (by dispatch: a release
the workflow's own token publishes starts no `release:` workflow). One gate runs at a time; a
newer canary's run replaces one still waiting, so the gate always takes the newest canary when it
is free, and a canary it never took stays a canary. It runs, against the
published archives, installed with `install.sh --version <v>` as a user installs them, on macOS
(Apple silicon) and Linux (x86-64 and ARM64):

- the install checks: `blaze --version` is the release's, `blaze new` + `blaze run` prints
  `Hello, world!`;
- the whole smoke corpus (below), dev and `--release`, `blaze run` and `blaze test`;
- each application of the corpus (below), in two jobs of its own per platform: **works** (its
  dev and release builds run, its tests pass under `blaze test`) and **speed** (`harness/bench.py`
  with the release's `blaze-engine`, beside plain cargo on the same runner: the cold build, the
  no-op build, every edit and the tests are faster with blaze than with cargo, the program and
  its tests do the same as cargo's, and nothing is more than 25% worse than the stable release's
  numbers, which it keeps as assets `bench-<runner>-<app>.json`; a stable release without them,
  as the first, is not compared). Hosted runners are shared, so 25% is their noise and not a
  benchmark: docs/benchmarks.md's numbers come from the bench machines;
- the train (`train.yaml`, every tier on every platform), dispatched on the tag: built from the
  commit the archives were built from. A train on a tag releases nothing and opens no issue.

When every job passes, `.github/scripts/promote.py` marks the release stable and *Latest*,
appends to its notes the line `Promoted to stable on <date> after <run URL>`, and adds the
applications' numbers to it. When one fails the
release stays a canary, and an issue `Canary v... is not promoted to stable` is opened (or
commented on, for a run again) with each failed job, each failed smoke case with its platform
and the commands that repeat it after installing that release, and the run's link. A later
canary that passes is promoted over it. Stable never moves backwards: a canary that passes after
a newer one was promoted becomes stable but not *Latest*.

By hand: `gh workflow run promote.yaml -f tag=v0.YYYYMMDD.N` tests that release again (after a
flake, or a fix to the gate itself); `-f train-run=<id>` counts a green train run on the tag
instead of starting another; `-f promote=false` tests without promoting or opening an issue;
`-f train=false` leaves the train out (and promotes nothing); `-f apps=zed,cargo` tests only those
applications. A pull request that changes the gate (`promote.yaml`, `promote.py`, the smoke
corpus) runs it against the newest release, without the applications, the train, the promotion
or an issue.

## The smoke corpus

`harness/smoke/` holds small programs that must build and run correctly with `blaze run` and
`blaze test`, in dev and `--release`, each with the output it must print, and the projects
(`smoke.json`). `harness/smoke.py` runs them against a blaze (its input's `blaze`, or `blaze` on
PATH for an installed release).

- **std** (no dependencies; the merge gate runs these on every pull request, under two minutes):
  `HashMap` insert and iterate, `thread::spawn` + `join`, `thread_local!`, `Mutex`/`OnceLock`/
  atomic statics, `std::env` and `std::fs` basics.
- **crates** (from crates.io, at the versions their Cargo.lock pins): tokio (multi-threaded
  runtime, a TCP echo on localhost), a serde_json round trip, proc-macro derives (thiserror,
  clap's derive).
- **projects** (real repositories at pinned commits): ripgrep, indexable-inc/anim, and blaze
  itself.
- **apps** (large applications at pinned commits, with the system libraries each builds with
  in its entry): cargo 0.92.0 (which says `cargo 1.91.0`), zed v1.24.4 (a window it does not open: `--help`; its tests are
  those of the crates below gpui, since the whole suite with both builds is longer than a
  hosted runner's six hours), servo v0.7.0 (`--version`; the tests of crates without a window
  or a GPU), bun's Rust port (bun-v1.4.3, after `harness/bunprep.py` built its C and C++ in
  `nix develop .#bench-bun`), and NVIDIA Dynamo v1.6.1 (the workspace, on macOS `dynamo-llm`
  without its Linux-only default feature; its Python bindings built and imported; the CPU-only
  vLLM mocker server's `--help`; its tests that need no GPU), Dmitriy Kovalenko's fff v0.11.0
  (the file finder formerly fff.nvim: its MCP server's `--version` and health check; the tests
  of fff-search and the crates below it) and fframes v1.3.0 (a video rendering framework, with
  the prebuilt static FFmpeg and Skia its build scripts download: its hello-world example's
  timeline and one frame rendered to PNG on the CPU; the tests of the crates that need no GPU).
  What a runner without a GPU cannot build or test is listed with why in the entry's
  `excluded`, which smoke.py prints. Each is also measured against cargo (above); cargo builds
  dynamo with its pinned Rust 1.96.1, which its users build with.

The std cases are the merge gate's; all four groups are the promotion gate. blaze itself is in
the corpus but not measured against cargo here: how fast blaze builds blaze is the self-build's
measurement.

## Installing a channel

```sh
curl -fsSL https://ix.dev/blaze | sh                         # stable
curl -fsSL https://ix.dev/blaze | sh -s -- --channel canary  # the newest release
curl -fsSL https://ix.dev/blaze | sh -s -- --version 0.20261028.0
```

`BLAZE_CHANNEL=canary` is the same as `--channel canary`; `--version` wins over a channel. A
workspace's pin, `[workspace.metadata.blaze] version`, takes a version or `"stable"` /
`"canary"` too.

---
status: current
summary: Releases: the canary and stable channels, how a release is cut and what gates promotion to stable.
---
# Releases: canary and stable

blaze has two release channels, made of the same releases:

- **canary** (opt-in): every release, as soon as it is built. A release is cut from a commit of
  main as soon as main's merge gate has built it on Linux (the job
  `linux arm64 / build`, which runs on an ix runner with room for the build; #1654), without waiting for the unit tests or the other platforms. It is published as a GitHub
  *prerelease*.
- **stable** (the default): a canary that then passed the heavy tests, unchanged, and whose
  commit passed main's whole merge gate: its whole run green, with the unit tests on Linux
  x86-64, Linux ARM64 and macOS. Promotion builds nothing: the archives users get on stable are the bytes that
  were tested as canary.

## Versions

A release is `v0.YYYYMMDD.N` (`0.YYYYMMDD.N` in `blaze --version`), N counting the releases of
that day (UTC) from 0. `.github/scripts/train_release.py` picks the number.

The version is never committed. Cargo.toml's workspace version stays `0.0.0` on main: it is
part of every crate's identity (`CARGO_PKG_VERSION`), so a version committed with each release
made every build of main after it miss blaze's store and rebuild all of blaze. A release's
build is told which release it is by `BLAZE_RELEASE` (the tag), which release.yaml sets and
only `crates/blaze-engine/build.rs` reads, the one crate that changes with every commit anyway;
the program's `main` (`program.rs`) hands it to the rest of blaze (`blaze_cargo::release::set`)
before any command runs. It says

    blaze 0.20261028.9 (fb82b09 2026-10-28)     a release: the version, the commit and its date (UTC)
    blaze 0.0.0-dev (fb82b09)                   a build of the tree: no release, and no pin names it
                                                (it runs itself in any checkout of blaze's tree, or with --ignore-pin)

The version is the second field (`.github/actions/blaze` reads it). The binary does not know its
channel: a canary is made stable unchanged. Where a manifest's version must be the release's
(publishing to crates.io), `canary.py stamp` writes it into that job's checkout only.

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
pinned to `"stable"` or `"canary"`, install.sh): `https://blaze-releases.ix.dev/channels/stable`
and `.../canary` in R2 ([below](#where-the-bytes-are)), each the channel's version and a newline,
also kept as the assets `stable` and `canary` of the GitHub release `channels`. Each channel's
pointer names its newest release that is not yanked. The job `metadata` (`.github/scripts/promote.py
metadata`) sets both pointers and writes latest.json (below) from the releases as they are: in
release.yaml after it published a release, in promote.yaml after it made one stable, and in
yank.yaml. The three jobs share the concurrency group `release-metadata`, so one runs at a time and
the last one to run writes what the releases say then; a job the group cancels while it waits
behind a newer one loses nothing.

## latest.json

What every blaze daemon reads every five minutes to tell its user about newer and yanked
releases ([install.md](install.md#updates)): `https://ix.dev/blaze/latest.json`, an object in
the R2 bucket `blaze-releases` that the site's worker serves (`site/worker/index.ts`) with its
ETag (a request with a matching `If-None-Match` gets a `304`) and `Cache-Control: public,
max-age=60`. It is static, so no daemon spends GitHub's API limit (60 requests an hour per
address, shared by every machine behind it):

    {
     "canary": {"notes": "<the first 12 lines of its notes>", "url": "https://github.com/indexable-inc/blaze/releases/tag/v0.20261104.3", "version": "0.20261104.3"},
     "schema": 1,
     "stable": {"notes": "...", "url": "...", "version": "0.20261104.0"},
     "yanked": {"0.20261101.0": "miscompiles drops (#1234)"}
    }

Each channel's entry is its newest release that is not yanked; a yanked release counts only on
its own first line of notes, `blaze-yanked: <reason>`. The job `metadata` (above) writes it from
the releases (the same bytes for the same releases), and `wrangler r2 object put`
(cloudflare/wrangler-action) puts it. The token it puts it with is the secret
`CLOUDFLARE_R2_API_TOKEN`, an R2 API token with Object Read & Write on the bucket
`blaze-releases` alone, apart from the site's deploy token; it reaches wrangler-action only,
never promote.py. A run without it fails that job, after the release and the pointers, so
neither is held back.

## Where the bytes are

Everything blaze downloads from blaze's own releases comes from one place, the R2 bucket
`blaze-releases` at its custom domain `https://blaze-releases.ix.dev`, served from Cloudflare's
edge: a pinned release's archive and sums (`crates/blaze-engine/src/versions.rs`), the channels'
pointers, the standard library's source (`crates/blaze-cargo/src/toolchain/library.rs`), install.sh's
download (CI's stage 0 too, `.github/actions/blaze`) and the pinned LLVM
(`crates/hir-ty-codegen/llvm.json`'s `release`, which blaze's native inputs, `llvm.py fetch` and
build_release.py read). GitHub's release CDN ran at 0.3 to 0.55 MB/s per download from a
developer's Mac on 2026-11-05, minutes for a 155 MB archive. GitHub is no fallback: a key R2
lacks is an error naming it (`BLAZE_RELEASES` names another place laid out the same). Every
byte is still checked against its pinned hash before use (the release's `SHA256SUMS` and
`B3SUMS`, the library's digest, the pin's sha256), so the bucket and the edge are trusted for
availability, not for content.

| key | what | `Cache-Control` |
|---|---|---|
| `v<version>/<asset>` | each asset of a release: `blaze-v<version>-<triple>.tar.gz` and its `.sha256`, `SHA256SUMS`, `B3SUMS`, `library-<digest>.tar.xz` | a year, `immutable` |
| `channels/<channel>` | the version `stable` or `canary` names, and a newline | 60 s |
| `latest.json` | [above](#latestjson) (also at `https://ix.dev/blaze/latest.json`, through the site's worker) | 60 s |
| `native/<name>` | a pinned native input's archive (`llvm.json`'s targets), its name ending in its sha256's first 16 hex digits | a year, `immutable` |

A version is one release for good, and a native archive's name carries its hash (a rebuild gets
a new name), so the immutable keys never change bytes and are cached at the edge and by clients.

`.github/scripts/r2.py` owns the layout: given a release's assets, a native build or the
pointers, it prints the `wrangler r2 object put` commands, which the workflows hand to
cloudflare/wrangler-action with `CLOUDFLARE_R2_API_TOKEN` (the token reaches wrangler alone).
release.yaml puts a release's assets before it makes the GitHub release, so a release GitHub
lists is one blaze can fetch, and fails without the token, making no release. The metadata jobs
put the pointers beside latest.json; llvm.yaml puts each LLVM build under `native/` (GitHub has
no copy of builds after 2026-11-05). By hand, logged in with `wrangler login`, `"run": true`
puts them itself, which is how a release made outside release.yaml gets there:

    gh release download v0.YYYYMMDD.N -R indexable-inc/blaze -D out
    python3 -I .github/scripts/r2.py '{"kind": "release", "tag": "v0.YYYYMMDD.N", "dir": "out", "run": true}'

R2 holds every release from v0.20261105.1 on, v0.20261027.0 (stable on 2026-11-05) and LLVM
23.1.4 at e152d9fa2bc2. A pin to an
older release fails until it is put there the same way.

## Yanking a release

A release found broken after it was published is yanked, not deleted (a pin to it still
fetches it):

    gh workflow run yank.yaml -f tag=v0.20261101.0 -f reason="miscompiles drops (#1234)"

yank.yaml (`promote.py yank`) makes `blaze-yanked: <reason>` the first line of the release's
notes, the one place a yank is kept (the same words further down yank nothing), and then its
job `metadata` moves each channel's pointer to the channel's newest release that is not yanked
and writes latest.json again. From each daemon's next check
(five minutes at most) every blaze that runs it, pinned or not, says so on every command, CI
too, with the reason and what moves off it (`blaze update`, or `blaze pin <version>`).
`blaze update` and `"auto"` never install a yanked release. `-f reason=` (empty) takes the yank
back.

A release older than this scheme (`v0.20261027.0`, the `nightly` build of 2026-10-20) was
published as a full release and counts as stable.

## Cutting a canary

Every built main is a canary, cheaply. `canary.yaml` runs on each push to main (and hourly, and
by hand): `.github/scripts/canary.py cut` waits for main's build of the push (the job `linux arm64
/ build` of the merge gate's run on it; when the push changed nothing the build reads, the
newest commit of main that has a build), at most 90 minutes, and then tags the newest commit of
main where one of them passed (main moves faster than its build, so that is seldom HEAD
itself; it looks through main's first parents since the newest release) `v0.YYYYMMDD.N` through
the API and dispatches `release.yaml` on the tag. It does not wait for the unit tests, the other
platforms (macOS often queues half an hour) or the rest of the run: stable does (below). The
cut waits in canary.yaml, polling the job through the API, rather than being dispatched from
the build job: that would need `actions: write` in `blaze-platform.yaml`, which pull requests
and the train run too. It cuts when

- something besides the docs (`docs/`, `site/`, `agents/`, `*.md`) and the bots' own commits
  changed from the newest release to that commit,
- no `release-cut.yaml` is running and no `release.yaml` is building a release (a run on a
  branch is not one), and the newest release is at least 30
  minutes old (`-f min-minutes=N` by hand). A build of main inside that window is cut by the
  next push or the hourly run.

No commit goes to main, so a cut never races a merge, and a canary is the bytes of a commit
main's gate built, as it is. release.yaml builds it with `BLAZE_RELEASE` set to the tag, so
`blaze --version` and the archives' names are the tag's, and writes the notes (`canary.py
notes`): the lines docs/CHANGELOG.md's [Unreleased] gained since the previous release and the title
of each change merged since then. [Unreleased] is never emptied: a release's notes are in its
GitHub release, not in a section of docs/CHANGELOG.md.

`release-cut.yaml` (by hand, on main) or the release train's `release` job cuts a release of
main's HEAD the same way (`train_release.py`): a tag through the API and release.yaml on it,
no commit. Both count every tag, so they never take the same number.

Before it publishes, its `explore-page` job checks that the deployed explorer reads the profile
schema this tag writes (`.github/scripts/explore_page.py` against `https://ix.dev/explore/schemas.json`;
the page is indexable-inc/ix's `packages/web/src/lib/explore/schema.ts`). A daemon whose schema the
page lacks is refused by ix.dev/explore, so a release ahead of the page is not published, and a pull
request that changes `schema::VERSION` fails blaze.yaml's `scripts` job the same way: the page
moves first, then blaze.

Either way `release.yaml` builds the archives on the tag for macOS (Apple silicon) and Linux
(x86-64, ARM64) and publishes them as a prerelease with `SHA256SUMS` (what install.sh checks) and `B3SUMS`; a
pinned release's fetch checks both, and a release from before `B3SUMS` (dated before 2026-10-31,
when no release lacks it) by its `SHA256SUMS` alone; one dated later that lacks `B3SUMS` is
refused ([cargo.md](cargo.md#pinning-a-release-of-blaze)). PGO is off for now (#870):
the release is built with `"pgo": "off"`, by the stage-0 blaze `.github/blaze-version` names
(`.github/scripts/build_release.py`; blaze builds blaze, never cargo or rustc).

The sums come from the same release as the archives, so they show an archive arrived whole and
unchanged, not that blaze's release workflow made it: whoever can replace a release's assets can
replace its sums too. The plan for that, in order:

1. A pin records the digest. `blaze pin` writes each host's archive digest beside the version
   (`[workspace.metadata.blaze] b3 = { "<host>" = "<hex>", ... }`), read from the release's
   lists at pin time and then reviewed and committed with the manifest, as `go.sum` is: trust on
   first use, then the launcher refuses an archive whose digest is not the pinned one. This
   covers every pin to a version.
2. Releases are signed. release.yaml signs `B3SUMS` (`B3SUMS.sig`, a key blaze carries, or
   GitHub's artifact attestations) and the launcher checks the signature before it reads the
   list. This covers pins to a channel, which can't record a digest, and the first pin of a
   version.

## Promotion to stable

`release.yaml` starts `promote.yaml` once it has published the prerelease (by dispatch: a release
the workflow's own token publishes starts no `release:` workflow). One gate runs at a time; a
newer canary's run replaces one still waiting, so the gate always takes the newest canary when it
is free, and a canary it never took stays a canary. It runs, against the
published archives, installed with `install.sh --version <v>` as a user installs them, on macOS
(Apple silicon) and Linux (x86-64 and ARM64):

- the install checks: `blaze --version` is the release's, `blaze new` + `blaze run` prints
  `Hello, world!` (v2's blaze has no `new` command, so a v2 release fails this check until it
  has one or the check makes the package itself);
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
  commit the archives were built from. A train on a tag releases nothing and opens no issue;
- main's tests (`promote.py tested`): the merge gate's run on the push of the tag's commit to
  main is green (the whole run: every job of it, the smoke corpus's std cases and the script
  checks too), and its unit tests (`linux arm64 / tests`, `macos arm64 / tests`; no Linux x86-64 until ix
  has x86-64 capacity) ran and passed on that exact commit. The canary was cut before they finished, so the
  job waits for the run while it is going, polling every two minutes for up to five hours, beside
  the other jobs, which take longer. No run, a red run, a run that skipped the tests (a commit
  that changed nothing they read, such as one `release-cut.yaml` cut at a docs-only HEAD) or one
  still going after five hours fails the job with the commit and the run named, and the release
  stays a canary. After re-running a flaked job of that run, `gh workflow run promote.yaml -f
  tag=v...` tries again.

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
  atomic statics, `std::env` and `std::fs` basics, `const_heap` (`Vec`, `String`, `Box` and
  `BTreeMap` in constants), and `arbitrary_ints` (`u3` and `i7` of `#![blaze::arbitrary_ints]`:
  overflow panics in dev and wraps in release, compiled code agrees with const eval).
- **crates** (from crates.io, at the versions their Cargo.lock pins): tokio (multi-threaded
  runtime, a TCP echo on localhost), a serde_json round trip, proc-macro derives (thiserror,
  clap's derive), and three constants of real crates: SHA-256 of 1 MiB (`ce_sha256`, sha2), a
  Cargo manifest parsed with basic-toml (`ce_toml`) and 4 MiB inflated with miniz_oxide
  (`ce_inflate`).
  `const_heap` and the three `ce_` cases call ordinary functions in a `const`, which the pinned
  nightly refuses (E0015) and blaze does not yet (issue #1440). Each asserts that the constant
  equals what the program computes at run time, and prints the values. `expected.txt` has the
  same values computed independently (Python's `hashlib` and `tomllib`, and the same
  arithmetic).
- **projects** (real repositories at pinned commits): ripgrep and blaze itself.
- **apps** (large applications at pinned commits, with the system libraries each builds with
  in its entry): cargo 0.92.0 (which says `cargo 1.91.0`), zed v1.24.4 (a window it does not open: `--help`; its tests are
  those of the crates below gpui, since the whole suite with both builds is longer than a
  hosted runner's six hours), servo v0.7.0 (`--version`; the tests of crates without a window
  or a GPU), bun's Rust port (bun-v1.4.3, after `harness/bunprep.py` built its C and C++ in
  `nix develop .#bench-bun`), NVIDIA Dynamo v1.6.1 (the workspace, on macOS `dynamo-llm`
  without its Linux-only default feature; its Python bindings built and imported; the CPU-only
  vLLM mocker server's `--help`; its tests that need no GPU), Dmitriy Kovalenko's fff v0.11.0
  (the file finder formerly fff.nvim: its MCP server's `--version` and health check; the tests
  of fff-search and the crates below it), fframes v1.3.0 (a video rendering framework, with
  the prebuilt static FFmpeg and Skia its build scripts download: its hello-world example's
  timeline and one frame rendered to PNG on the CPU; the tests of the crates that need no GPU),
  flox v1.19.2 (in the environment of flox's own package build, which
  `harness/floxprep.py` takes from its flake: the CLI's `--version`; the tests of flox-core and
  flox-config) and OpenAI's Codex CLI rust-v0.165.0 (its workspace `codex-rs`, the entry's
  `dir`; `codex --version`; the tests of codex-apply-patch, codex-protocol, codex-execpolicy
  and codex-features, which need no model, network or credentials). What a runner without a
  GPU cannot build or test is listed with why in the entry's `excluded`, which smoke.py prints.
  Each is also measured against cargo (above); cargo builds dynamo and codex with their pinned
  Rust (1.96.1 and 1.99.1), which their users build with.

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

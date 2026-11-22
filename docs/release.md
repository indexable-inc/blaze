---
status: current
summary: Releases: the release pipeline was deleted on 2026-11-15; what is left is the R2 layout, the smoke corpus and installing a release.
---
# Releases

The release pipeline (cutting canaries, promotion to stable, yanking and the version a release
build carries) was deleted on 2026-11-15 with blaze-engine, and returns with the new command
line. It stays in git history. What is left is below: where the bytes are, the smoke corpus,
and installing a release that was already published.

## Where the bytes are

Everything blaze downloads from blaze's own releases comes from one place, the R2 bucket
`blaze-releases` at its custom domain `https://blaze-releases.ix.dev`, served from Cloudflare's
edge: a pinned release's archive and sums, the channels'
pointers, the standard library's source (`crates/blaze-toolchain`), install.sh's
download (CI's stage 0 too, `.github/actions/blaze`) and the pinned LLVM
(`pins/llvm.json`'s `release`, which blaze's native inputs and `llvm.py fetch`
read). GitHub's release CDN ran at 0.3 to 0.55 MB/s per download from a
developer's Mac on 2026-11-05, minutes for a 155 MB archive. GitHub is no fallback: a key R2
lacks is an error naming it (`BLAZE_RELEASES` names another place laid out the same). Every
byte is still checked against its pinned hash before use (the release's `SHA256SUMS` and
`B3SUMS`, the library's digest, the pin's sha256), so the bucket and the edge are trusted for
availability, not for content.

| key | what | `Cache-Control` |
|---|---|---|
| `v<version>/<asset>` | each asset of a release: `blaze-v<version>-<triple>.tar.gz` and its `.sha256`, `SHA256SUMS`, `B3SUMS`, `library-<digest>.tar.xz` | a year, `immutable` |
| `channels/<channel>` | the version `stable` or `canary` names, and a newline | 60 s |
| `native/<name>` | a pinned native input's archive (`llvm.json`'s targets), its name ending in its sha256's first 16 hex digits | a year, `immutable` |

A version is one release for good, and a native archive's name carries its hash (a rebuild gets
a new name), so the immutable keys never change bytes and are cached at the edge and by clients.

`.github/scripts/r2.py` owns the layout: given a release's assets, a native build or the
pointers, it prints the `wrangler r2 object put` commands, which the workflows hand to
cloudflare/wrangler-action with `CLOUDFLARE_R2_API_TOKEN` (the token reaches wrangler alone).
llvm.yaml puts each LLVM build under `native/` (GitHub has no copy of builds after
2026-11-05). By hand, logged in with `wrangler login`, `"run": true` puts them itself, which is
how a published release gets there:

    gh release download v0.YYYYMMDD.N -R indexable-inc/blaze -D out
    python3 -I .github/scripts/r2.py '{"kind": "release", "tag": "v0.YYYYMMDD.N", "dir": "out", "run": true}'

R2 holds every release from v0.20261105.1 on (.1 and .4, the pin on 2026-11-05), v0.20261027.0
(stable on 2026-11-05) and LLVM 23.1.4 at e152d9fa2bc2. A pin to an older release fails until it
is put there the same way.

## The smoke corpus

`harness/smoke/` holds small programs that must build and run correctly with `blaze run` and
`blaze test`, in dev and `--release`, each with the output it must print, and the projects
(`smoke.json`). `harness/smoke.py` runs them against a blaze (its input's `blaze`, or `blaze` on
PATH for an installed release).

- **std** (no dependencies):
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

## Installing a channel

```sh
curl -fsSL https://ix.dev/blaze | sh                         # stable
curl -fsSL https://ix.dev/blaze | sh -s -- --channel canary  # the newest release
curl -fsSL https://ix.dev/blaze | sh -s -- --version 0.20261028.0
```

`BLAZE_CHANNEL=canary` is the same as `--channel canary`; `--version` wins over a channel. A
workspace's pin, `[workspace.metadata.blaze] version`, takes a version or `"stable"` /
`"canary"` too.

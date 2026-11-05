# docs/upstream/roadmap: what was fetched, from where, when

Fetched on **2026-10-24** for [docs/roadmap.md](../../roadmap.md). The `.md` extracts are the
page text as Exa returned it, trimmed to the parts the roadmap cites; nothing in them is
reworded.

| file | source | licence | what it backs |
|---|---|---|---|
| `edenfs-overview.md` | `https://github.com/facebook/sapling/blob/main/eden/fs/docs/Overview.md` (`main`, 2026-10-24) | GPL-2.0 (Sapling), quoted | EdenFS serves a checkout lazily from source control; FUSE on Linux, NFSv3 on macOS, ProjFS on Windows; content hashes without reading files; notifications through Watchman |
| `bazel-skyframe.md` | `https://bazel.build/reference/skyframe` | CC-BY-4.0 (Bazel docs), quoted | Skyframe's nodes, bottom-up invalidation that must `stat()` every input unless told by inotify, change pruning (early cutoff), no incremental mutation of a node |
| `watchman-cookies.md` | `https://facebook.github.io/watchman/docs/cookies` | MIT (Watchman docs), quoted | cookie files to make a query see every write before it; FSEvents gives no ordering guarantee on macOS, so a settle period is the workaround |
| `cacm-google-monorepo.md` | `https://cacm.acm.org/research/why-google-stores-billions-of-lines-of-code-in-a-single-repository/` (Potvin and Levenberg, CACM 59(7), 2016) | ACM, short excerpt | Piper and CitC; the scale (one billion files, 800k queries per second at peak, most from build and test systems) |
| `clippy-README.md` | `https://github.com/rust-lang/rust-clippy/blob/master/README.md` (`master`, 2026-10-24) | MIT OR Apache-2.0 | clippy's lint groups and their default levels, `#[allow(clippy::...)]`, `cargo clippy --fix` |

Measured by us, in `measured/` (**read** in roadmap.md's terms):

| file | what |
|---|---|
| `merkle-proto.rs`, `merkle-proto.Cargo.toml` | a throwaway prototype of a content-addressed Merkle tree over a git checkout: the whole tree hashed with BLAKE3, git's blob SHA-1 for comparison, a three-file write by path copying, and a diff of two roots that skips equal subtrees. Not part of the build |
| `merkle-proto-nushell.txt` | its output on nushell at `bf251dc54` (2,240 files, 15.1 MB), Apple M5 Max, macOS, release build, one thread |
| `clippy-lints.txt` | clippy's lints counted by group from `declare_clippy_lint!` in a sparse checkout of `rust-lang/rust-clippy` at `43c0efca14` (2026-10-24), and how many lint files name `MachineApplicable`, implement `LateLintPass` or `EarlyLintPass`, or use MIR |

Read in place, not copied:

- rust-analyzer's `vfs` crate, `vendor/rust-analyzer/crates/vfs/src/lib.rs` in this tree.
- ix (private, `indexable-inc/ix` at `4a647c63ef`, 2026-10-23): `crates/storage/stream/stream-node/src/shard/coreio.rs`
  (the `CoreIo` trait: submit, poll completions into a sink, a clock, park, offload) and
  `docs/dst/README.md` (the shared deterministic simulation loop, `crates/tools/dst-kit`).
- `/Volumes/Projects/nix/kb/build-determinism/fuse-input-tracing.md` (indexable's knowledge base, 2026-10-15):
  ix's own FUSE server in its buck2 fork, measured on a kernel build.
- den (`andrewgazelka/den`): `docs/vfs.md` (den-vfs, a sans-IO Loro CRDT space with per-file
  versions and hub tools) and `docs/code-lsp.md` (hub tools `lsp_*`, `code_run`).

Cited from memory or common knowledge and marked **reported** where used: git's tree and blob
object format, the Nix store, REAPI's `Directory` digests (the proto is in
[../ci-runner/remote_execution.proto](../ci-runner/remote_execution.proto)), Unison's
content-addressed codebase, io_uring's registered files and buffers, FSKit on macOS 15.4 and later,
and makepad's Relax (read for [#232](https://github.com/indexable-inc/blaze/issues/232)).

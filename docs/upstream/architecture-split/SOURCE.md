# docs/upstream/architecture-split: what was fetched, from where, when

Fetched or produced on **2026-10-21** for [docs/architecture-split.md](../../architecture-split.md).
Files are verbatim unless the notes say otherwise. In file names, `/` in the upstream path
became `_`.

## rustc-1.99.0/ (how rustc splits the compiler into crates)

Source: `https://raw.githubusercontent.com/rust-lang/rust/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/<path>`,
the 1.99.0 release, the toolchain blaze pins. Licence: MIT OR Apache-2.0. Verbatim.

| file | `<path>` |
|---|---|
| `rustc-dev-guide_compiler-src.md` | `src/doc/rustc-dev-guide/src/compiler-src.md` (the crate layout and why it is split) |
| `rustc-dev-guide_overview.md` | `src/doc/rustc-dev-guide/src/overview.md` |
| `rustc_<crate>_Cargo.toml` | `compiler/rustc_<crate>/Cargo.toml` for `borrowck`, `mir_build`, `mir_transform`, `mir_dataflow`, `monomorphize`, `const_eval`, `codegen_ssa`, `codegen_cranelift`, `middle`, `interface` (who depends on whom) |

## rust-analyzer/ (upstream's own split of hir-ty, and its AI policy)

Source: `gh pr view <n> -R rust-lang/rust-analyzer --json ...`, title, author, merge time and
body as GitHub has them. Licence: MIT OR Apache-2.0. Verbatim body.

| file | pull request |
|---|---|
| `pr-23181-break-hir-ty-up-step-1.md` | [#23181](https://github.com/rust-lang/rust-analyzer/pull/23181) "Break hir-ty up, step 1" (extracts `hir-ide`), merged 2026-10-12 |
| `pr-23233-ai-rustc-citation.md` | [#23233](https://github.com/rust-lang/rust-analyzer/pull/23233) "Require rustc citation for analysis changes when using AI", merged 2026-08-26 |

The plan #23181 links is a Zulip thread
(`t-compiler/rust-analyzer`, topic "Breaking up hir-ty", message 617085080). Zulip's API needs
a login for it and the public page renders nothing without JavaScript, so it was **not read**.

rust-analyzer's architecture document is not copied here: it is in the tree, at
`vendor/rust-analyzer/docs/book/src/contributing/architecture.md` (the sections on `base-db`,
`hir-*` and `hir`).

## measured/ (produced here, not fetched)

From a scratch bare repository with blobless fetches of `indexable-inc/rust-analyzer` `blaze`
(at `c0dabc5588`), `rust-lang/rust-analyzer` `master` (at `ea09c7b724`, 2026-10-21) and blaze
`main` (at `22b1267233`). The merge base of the fork and upstream is `0b21bd6596` (2026-10-20),
the upstream commit the 2026-10-20 merge took.

| file | command |
|---|---|
| `vendor-numstat.txt` | `git diff --numstat 0b21bd6596 <main>:vendor/rust-analyzer` |
| `vendor-new-files.txt` | the same with `--diff-filter=A --name-only` |
| `upstream-churn.txt` | for each file of `vendor-numstat.txt` that upstream has: `git rev-list --count --no-merges --since=2026-04-21 <upstream master> -- <file>`, then blaze's added and removed lines |
| `rebuild-times.txt` | on the M5 Max (18 cores) under the load of other lanes (the load averages are in each line): for each file, twice, append a comment line, `/usr/bin/time -p cargo build --release -p blaze-engine` with Rust 1.99.0, restore the file, rebuild. `cpu` is user plus system seconds, `crates` the ones cargo compiled. `before` is `main` at `5f26408566`; `after` is the `codegen-crate` branch before it was rebased over step 4, so the borrow checker was still a module of `hir-ty` in both; `after-step3` is the `store-crate` branch on `main` after steps 2 and 4 |

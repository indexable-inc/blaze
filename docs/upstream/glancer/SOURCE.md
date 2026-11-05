# docs/upstream/glancer: what was fetched, from where, when

Everything here was fetched on **2026-10-21** for
[docs/research/glancer.md](../../research/glancer.md) and [docs/glancer.md](../../glancer.md).
Files are verbatim unless the notes say "excerpt". In file names, `/` in the upstream path became
`_`. Pages were found with Exa search and fetched with `curl`.

## matklad/

| file | source | licence |
|---|---|---|
| `2026-08-21-rust-glancer.html` | `https://matklad.github.io/2026/08/21/rust-glancer.html` | the author's; quoted for review |
| `2020-05-18-next-few-years.html` | `https://rust-analyzer.github.io/blog/2020/05/18/next-few-years.html` | rust-analyzer blog, MIT OR Apache-2.0 |

Not saved, cited from Exa's excerpt only: matklad's 2018 internals post
`https://internals.rust-lang.org/t/next-steps-for-rls-rustc-and-ide-support/8428/6` and
"What Is a dependency?" `https://matklad.github.io/2024/12/30/what-is-dependency.html`. The HackMD
design matklad links (`https://hackmd.io/ytd82QNiT_Ku2XFr1EAtiQ`) was not fetched.

## rust-glancer/

Rendered documentation from `https://rust-glancer.github.io/<path>`, and three source files from
`https://raw.githubusercontent.com/rust-glancer/rust-glancer/3c36614b2b6f67e62ed167d90670ca03518b8f5d/<path>`
(`HEAD` of `main` on 2026-10-21, from `git ls-remote`). Licence: Apache-2.0 (GitHub's licence
field for the repository).

| file | `<path>` |
|---|---|
| `docs.html` | `docs/` (intro) |
| `docs_intro_SCOPE.html` | `docs/intro/SCOPE.html` |
| `docs_development_MEMORY.html` | `docs/development/MEMORY.html` |
| `docs_architecture_ARCHITECTURE.html` | `docs/architecture/ARCHITECTURE.html` |
| `blog_why-lsp-is-hard.html` | `blog/why-lsp-is-hard/` (2026-09-16) |
| `changelog_0-2-0.html` | `changelog/0-2-0/` |
| `storage_cache_fingerprint.rs` | `crates/engine/project/src/storage/cache/fingerprint.rs` |
| `storage_cache_header.rs` | `crates/engine/project/src/storage/cache/header.rs` |
| `storage_cache_mod.rs` | `crates/engine/project/src/storage/cache/mod.rs` |

## rustc/

`1.99.0_encoder_should_encode_mir.rs`: excerpt, lines 1095 to 1135 of
`https://raw.githubusercontent.com/rust-lang/rust/1.99.0/compiler/rustc_metadata/src/rmeta/encoder.rs`
(tag `1.99.0` = `daa8d75bc715f77869fa06d80b6ea1a3fe52d38e`, the toolchain blaze pins). MIT OR
Apache-2.0.

## salsa/

From the crates.io release `salsa-0.28.5` in cargo's registry cache, the version in blaze's
`Cargo.lock`. MIT OR Apache-2.0.

| file | what |
|---|---|
| `0.28.5_tests_persistence.rs` | `tests/persistence.rs`, verbatim |
| `0.28.5_Cargo.toml_features.txt` | excerpt: lines 36 to 60 of `Cargo.toml`, the feature list |

# Requests from selfbuild-srv

`vendor/proc-macro-srv/` is deleted: blaze's proc-macro server is `crates/proc-macro-srv` (the
loader of `BLAZE_PROC_MACROS`) and `crates/proc-macro-srv-cli` (binary `blaze-proc-macro-srv`),
members of the root workspace. These files outside selfbuild-srv's paths still name the old tree.

## 1. `tools/nix/units/daemon.json`: drop `vendor/proc-macro-srv` from `exclude`
- What: remove the `"vendor/proc-macro-srv",` entry of the workspace's `exclude` (line 7088), as
  the root `Cargo.toml` no longer has it.
- Why: the units mirror the root manifest; an exclude of a path that is gone is stale, and the
  server is now a member (`crates/proc-macro-srv-cli` is under `crates/*`), so the units need
  its package too when they are next generated.

> forwarded to the coordinator: `tools/` is its (the units are regenerated from the root manifest); on requests' coordinator list. Still at line 7088 (pass 50).

## 2. `harness/smoke/ce_toml/manifest.toml`: the same `exclude` entry
- What: remove `"vendor/proc-macro-srv",` (line 9).

> forwarded to the coordinator: `harness/` outside `v2/` is its; on requests' coordinator list. Still at line 9 (pass 50).

## 3. `crates/blaze-cargo/src/cargo/own_macro.rs`: `missing_server`'s message
- What: the command it tells the user (line 236) is now
  `blaze . --crate blaze_proc_macro_srv --crate proc_macro --out <scratch>/server.o --exe {beside}`
  run from a blaze checkout's root (what `build_release.py --server` runs), or
  `blaze build -p proc-macro-srv-cli --release` and copying `target/release/blaze-proc-macro-srv`
  beside blaze.
- Why: `vendor/proc-macro-srv` and the crate `rust_analyzer_proc_macro_srv` no longer exist.
  The doc comment at the top of the file is already changed (selfbuild-srv owns that line).

> forwarded to cargo-next2 (`blaze-cargo/src/cargo/` is cargo's, not DONE); told by tmux (requests, pass 50). The first command is in the message already (lines 236-237); the `blaze build -p proc-macro-srv-cli --release` alternative is not.

## 4. Comments naming the old tree
- `crates/blaze-macro-wasm/guest/proc_macro.rs:16`: `vendor/proc-macro-srv/crates/proc-macro-srv/src/token_stream.rs`
  is now `crates/proc-macro-srv/src/token_stream.rs`.
- `crates/blaze-expand/src/compare.rs:2`: the server is built from `crates/proc-macro-srv-cli`.
- `crates/blaze-macro-eval/tests/corpus/config.rs:21`: it is the toolchain's own server
  (`<sysroot>/libexec/rust-analyzer-proc-macro-srv`), never one of blaze's tree; drop
  "of `vendor/proc-macro-srv`".
- `tools/rename.py:112-115`: `FORKS_ELSEWHERE` names `vendor/proc-macro-srv/`, which is gone;
  drop it and the sentence about it.
- `.gitattributes:9-10`: the two `vendor/proc-macro-srv/...` `-text` lines name deleted files;
  `crates/proc-macro-srv/src/bridge.rs -text` (line 8) stays.

> applied, checked by requests (pass 50): the three crate comments already read as asked (`blaze-macro-wasm/guest/proc_macro.rs:16` names `crates/proc-macro-srv/src/token_stream.rs`, `blaze-expand/src/compare.rs:2` `crates/proc-macro-srv-cli`, `blaze-macro-eval/tests/corpus/config.rs:21` the toolchain's own server), and `tools/rename.py` and `.gitattributes` name no `vendor/proc-macro-srv` any more. Nothing left.

## 5. The lexer of `crates/proc-macro-srv` (selfbuild)
- What: choose how the server lexes without a `rustc_private` crate. `ra-ap-rustc_lexer` 0.166
  (the workspace's) is the toolchain's `rustc_lexer` but for `TokenKind::ForcedKeywordIdent`
  (`k#ident`), which `src/token_stream.rs` matches for `bridge::IdentKind::ForcedKeyword`.
  (a) use `ra-ap-rustc_lexer` and drop that arm: `k#fn` is then an unknown prefix, as `tt` and
  the parser already lex it; or (b) carry the toolchain's `rustc_lexer` as a module of
  `proc-macro-srv` (needs `memchr`, `unicode-properties`, not a workspace dependency yet, and
  `unicode-ident`).
- Until then the crate keeps `extern crate rustc_lexer;` and the `rustc_private` feature.

> applied (selfbuild): 3 and 4 as written. 5 is option (b), as a crate rather than a module: `crates/rustc_lexer` is the toolchain's `compiler/rustc_lexer/src` (lib.rs and tests.rs) at 16d1ffac, unchanged, re-vendored by `toolchain_bump.py` beside rustc_abi; workspace deps `rustc_lexer` and `unicode-properties`; proc-macro-srv depends on it and no longer has `rustc_private` or `extern crate rustc_lexer`. 1 left for the next regeneration of `tools/nix/units/daemon.json` (generated; it also still lists the old `proc-macro-test` package). 2 left: `harness/smoke/ce_toml/manifest.toml` is a parse fixture, not the workspace.
> forwarded to selfbuild (it names itself; a choice for the lane that holds `crates/proc-macro-srv`); told by tmux (requests, pass 50), with requests' note: (a) changes what a macro is given for `k#ident` from what rustc's server gives it, which the superset rule (every program the pinned nightly builds behaves the same) and decisions' best-long-term rule weigh against; (b) keeps rustc's lexer, and adding `memchr`, `unicode-properties` and `unicode-ident` to `[workspace.dependencies]` is requests' to apply on request.

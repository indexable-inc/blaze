---
status: record
summary: blaze on the latest nightly: the 2026-10-25 handoff.
date: 2026-10-26
---
# NIGHTLY: blaze on the latest nightly (2026-10-25 handoff)

Andrew, 2026-10-25: "blaze should only support latest nightly"; "you can add features to ra
jit, it is not dependent on rustc anymore, to allow nightly things". The goal: the space game
(`#![feature(allocator_api)]`, `-Zthreads=8`, `cargo-features = ["profile-rustflags"]`,
edition 2024) builds with `blaze build`.

## What landed (PR #372)

Seven lanes (`rjn-abi`, `rjn-deref`, `rjn-layout`, `rjn-macro`, `rjn-pms`, `rjn-typeid`,
`rjn-zflags`) combined onto one batch branch `nightly-1025` off main c8e5f6a88a, plus
`nightly-cargo-features` and `nightly-typeinfo-1025`:

- The pin: `harness/toolchain` is `nightly-2026-10-25` (rust-lang/rust 12433c307,
  `rustc 1.101.0-nightly (12433c307 2026-10-24)`); install.sh, CI's toolchain action, the flake,
  README, docs/install.md, docs/support.md and contributing.md name it. `harness/toolchain_bump.py`
  moves it. A rustc counts as the pin only when its whole `rustc -V` line matches.
- `vendor/rustc_abi` and the proc-macro server (J1) from that commit; the abort intrinsic's
  new name (`abort_immediate`); `TypeId`'s DefPathData numbering (Ctor 11, AnonConst 12); the
  `llvm.x86.*` intrinsics the nightly's `core::arch` calls, lowered lane by lane.
- Deref patterns lowered as `rustc_mir_build` lowers them (`box` patterns are gone upstream).
- `crates/blaze-engine/src/unstable.rs`: every `-Z` flag of the nightly's `rustc -Z help` classed
  as ignored, modelled (`ub-checks`/`contract-checks` off) or refused with a reason.
  `-Zthreads=N` is ignored: no warning, no refusal, no fallback (tests `treatments`,
  `a_refusal_names_the_flag`).
- cargo's second build-dir layout (`build/<pkg>/<hash>/{out,fingerprint,run}`), the
  nightly cargo's default, followed beside the first (`cargo/layout.rs`).
- blaze-manifest reads `cargo-features` as the nightly's cargo (cad07cf8e) does: a listed
  unstable feature is on (`profile-rustflags` with a profile's `rustflags`), `trim-paths` is
  stable, path bases are joined. The game's root manifest loads (metadata example: 174
  packages).
- `harness/live-alloc` (allocator_api, live edits) runs in the gate via `live_e2e.py`.
- Const eval answers all 19 reflection intrinsics of the nightly's `core::intrinsics`
  (`hir-ty/src/mir/eval/shim/reflection.rs`), the renamed `type_id_size_of` and
  `type_id_variant_name` and the new `type_id_type_of`, generics, `function_ptr`,
  `non_exhaustive`; lang items `type_info`, `type_info_generic`, `FnPtr`.

Fixes CI asked for on the batch:

- AArch64: a one-lane 64-bit SIMD vector (NEON's `*x1_t`, LLVM's legal `v1i64`/`v1f64`) by
  value in the Rust ABI goes in a D register (`hir-ty-codegen` `rust_simd_pass_mode`); the
  nightly's memchr and hashbrown pass them, and every aarch64 harness main differed without it.
- The fork's own workspace (`fork alone`) takes `rustc_abi` from `vendor/rustc_abi`
  beside it (a path patch), not the fork's git main, which only main's sync moves.
- `build_release.py` builds without PGO, with a warning, while `tools/pgo/profiles.json` is of
  another toolchain (not `--strict`); `tools/nix/units` regenerated for rust-version 1.101;
  install.yaml reads `harness/toolchain` before leaving the checkout.

## What I ran (dev-aws-8, x86-64 Linux, Cranelift)

Release build of the batch before the cargo-features merge; `ci.py quick-1/2/3`:
quick-3 passed; quick-2 failed only `storefull.py` (sudo not setuid under systemd-run: the
host, not the code); quick-1 failed `live_types.py` under load (5 jobs at once) and passed when
run alone. `wholeprogram.py` takes ~650 s, as on main's CI (696 s), not a hang.
`macroserver.py` PASS; `cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p
blaze -p blaze-build-script` pass; `cargo fmt` needed the nightly's rustfmt on
`toolchain/own.rs` (committed). The cargo-features change: `cargo test -p blaze-manifest` on
the Mac only. Not run by me: the LLVM backend, macOS, aarch64; CI's fast tier (workflow_dispatch on the
branch) ran them.
An earlier lane ran the game's `rgb-arena` crate (allocator_api, `-Zthreads=8`) through
`blaze build/test/run` with an edit: all builds by the daemon, 0 by cargo.

## Still refused or missing for the game

- `-Zshare-generics=no` in the game's `[profile.max]`, `dist`, `pgo-*` rustflags: refused
  (`EXPORTS`). The dev profile has only `-Zthreads=8`, so `blaze build` (dev) is not
  affected; a `--profile max` build is. `=no` is the default at opt-level 3 and could be
  `OffOnly`, if blaze never shares generics with rustc's code: decide and test.
- `-Cllvm-args=-fp-contract=fast` and `-Ctarget-cpu=apple-m5` in `max`: not checked here.
- `allocator_api` is "partially stabilized since 1.100.0, succeeded by `allocator_ext`" on this
  nightly: a warning from rustc, the game builds.
- blaze-manifest does not read `[unstable]` in `.cargo/config.toml` or cargo `-Z` flags
  (`-Zprofile-rustflags`, `-Zhost-config`, `-Zbindeps`, `-Zpublic-dependency`); `trim_paths`
  shows as absent where nightly cargo's unit graph prints `"none"` (cargo-util-schemas is
  still 1.99's 0.14.3); `feature-metadata` and builtin dependencies are accepted but probably
  do not parse; blaze-resolve's docs and fixtures still name cargo 1.99.0.
- `tools/pgo/profiles.json` still names 1.99.0: the `pgo` workflow retrains on the toolchain change;
  until it has, releases build without PGO (and `--strict` refuses).
- The whole game has not been built with `blaze build` yet. That is the next check
  (on the Mac, after it moves to nightly-2026-10-25).

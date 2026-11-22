---
status: record
summary: "The twenty crates that were parked (kept on disk, out of the build) from 2026-11-15 and deleted on 2026-11-20: where each is in git history."
date: 2026-11-20
---
# Parked crates: deleted 2026-11-20

On 2026-11-15 the old item layer was deleted to be rebuilt from scratch (design/bottom.md), and
twenty crates that depended on it were parked: kept on disk, out of the build (the root
Cargo.toml's `exclude`), each "to be rewritten on the new layer, not reconnected". On 2026-11-20
the owner asked for dead weight to go, and they were deleted (445,814 lines of Rust): blaze-cargo,
blaze-certify, blaze-codegen, blaze-diagnostics, blaze-doc, blaze-global, blaze-item-hash,
blaze-items, blaze-launch-host, blaze-lazy, blaze-link, blaze-macro-eval, blaze-metadata,
blaze-registry, blaze-shader, blaze-stencil, blaze-testsel, blaze-tree, blaze-units,
blaze-usercode. No crate of the build depended on any of them.

Each is in git history: the tree before this deletion is the round ref
`refs/lanes/ALL/b13-1240` (tree da1094abb9) of the working repository, and every commit of
`rebuild-1115` before it. What a rewrite of one needs from the old code is read from there.

A second pass the same day deleted the workspace members no live crate reached (followed from
blaze-cli, blaze-launcher, blaze-fmt, blaze-bench, blaze-lint-defs, scripts and astlog over every
kind of dependency): blaze-vfs, blaze-host, blaze-solo, blaze-image, blaze-kernel,
blaze-front-items, span, macros, blaze-gc, blaze-storemodel, blaze-llvm, blaze-native-load and
blaze-program (79,298 lines of Rust), with blaze-build-script's unused `outputs4` record and its
test, blaze-cli's dead `run_in_process`, and the stale nix unit graph `tools/nix/units/daemon.json`
(read only by the flake's unused `daemon-units`, removed with it). A third pass removed the plain
store's `Opened` and `Form` (a root opens only the plain store; a `store.json` naming `"form"` is
refused) and then blaze-cas's old pack store, its tests, sims, fixtures and kept tapes (49,651
lines of Rust): blaze-cas is now `Key`, `Corrupt` and `plain`, and blaze-bench's store bench
reads the plain store. The method, the table and what was kept and why are in the notes'
rounds/wave-1122-arch/DEAD-CRATES.md.

Still excluded from the workspace and NOT deleted: `crates/harness` (the native app, its own
build) and `crates/blaze-hvbox` (never in git).

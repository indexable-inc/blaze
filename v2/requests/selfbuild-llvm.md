# Requests from selfbuild-llvm

The LLVM pin check moved from `crates/hir-ty-codegen/build.rs` (deleted: it held only the check)
into `vendor/llvm-sys/pin.rs`, which `vendor/llvm-sys/build.rs` includes and runs first.

1. `Cargo.lock`: llvm-sys no longer depends on `regex-lite` and `semver` (its build script lost
   upstream's PATH/Homebrew search and version parsing). Drop those two from llvm-sys's
   `dependencies` list in the lock (cargo does it on the next resolve; `--locked` would refuse
   until then).
2. Comments that still say hir-ty-codegen's build script checks the pin; each should name
   `vendor/llvm-sys/pin.rs` (llvm-sys's build script) instead:
   - root `Cargo.toml` line 19, above `[workspace.metadata.blaze.sandbox]`: "llvm-sys's build
     script (vendor/llvm-sys/pin.rs) checks the pinned LLVM `nix develop` names; every build of
     blaze links it (blaze-stencil's stencils, the `llvm` feature)." The same line in
     `harness/smoke/ce_toml/manifest.toml` line 17 if that fixture mirrors the root manifest.
   - `flake.nix` lines 38-39: "crates/hir-ty-codegen/build.rs links it statically and refuses
     any other" -> "vendor/llvm-sys links it statically, and its build script (pin.rs) refuses
     any other".
   - `crates/hir-ty-codegen/Cargo.toml`, the comment above `inkwell`: "llvm.json beside build.rs,
     checked there" -> "llvm.json, checked by vendor/llvm-sys/pin.rs".
   - `.github/scripts/llvm.py` docstring lines 23-24: "crates/hir-ty-codegen/build.rs reads to
     refuse any other LLVM" -> "vendor/llvm-sys/pin.rs reads to refuse any other LLVM".
3. `crates/blaze-stencil/gen/mod.rs` lines 26-33 (fastcompile's): its own "{PREFIX} is unset"
   panic can no longer be reached, since llvm-sys (a build-dependency) stops the build first
   with the full message. The owner may drop the panic and the `PREFIX` read; nothing breaks if
   it stays.
4. `vendor/llvm-sys/src/lib.rs` (coordinator's): the `LLVM_SYS_NOT_FOUND` `compile_error!` near
   line 652 is dead, as build.rs no longer sets that cfg. Remove it if wanted; then also drop
   `check-cfg = ["cfg(LLVM_SYS_NOT_FOUND)"]` from `vendor/llvm-sys/Cargo.toml` (I can do that
   part).
5. Run the new test target in the build phase: `blaze test -p llvm-sys --test pin`
   (llvm-sys is a patched path dependency, not a workspace member, so `blaze test --workspace`
   may skip it).

No new environment variable is needed: the check reads `LLVM_SYS_231_PREFIX` (already in
`build-script-env`), files under it, and `crates/hir-ty-codegen/llvm.json` under the workspace
root, which the build-script sandbox lets scripts read.

> applied (selfbuild): item 1 needs nothing (the lock never listed llvm-sys's build-deps); item 2's comments in Cargo.toml, flake.nix, hir-ty-codegen/Cargo.toml and llvm.py now name vendor/llvm-sys/pin.rs. Items 3-5 left to their owners and the build phase.

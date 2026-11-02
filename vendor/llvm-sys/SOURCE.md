# llvm-sys

- Upstream: https://gitlab.com/taricorp/llvm-sys.rs, crates.io `llvm-sys` 231.0.1
  (sha256 21cdb31b2047b57121a31a911af86e4bacb93d165843d27cea1e14e071ac2b4c, git d17ede3188).
- Licence: MIT (LICENSE).
- Left out: examples, scripts, CI files, Cargo.lock.

Changes, each marked `blaze:` in build.rs:

1. Static LLVM archives are linked `static:-bundle`: named to the final link, not copied into the
   rlib.
2. A static LLVM on Linux links libstdc++ statically, from the C++ compiler's own `libstdc++.a`
   (`$CXX -print-file-name`), so a release binary needs no C++ runtime where it runs.
3. `wrappers/passes.cpp` (built with `llvm-config --cxxflags`, declared in
   `src/transforms/pass_builder.rs` as `LLVMBlaze*Passes`): the pass builder, analysis managers,
   standard instrumentation and parsed pipelines `LLVMRunPasses` makes on every call, kept per
   target machine instead. blaze runs passes over one small module per function.
   `cargo:rerun-if-changed` is printed for both wrappers.
4. `wrappers/passes.cpp` also has `LLVMBlazeRunSanitizers` (declared with its options struct
   `LLVMBlazeSanitizers` and the `LLVMBlazeCoverage*` bits in `src/transforms/pass_builder.rs`):
   SanitizerCoverage, MemorySanitizer, ThreadSanitizer and AddressSanitizer over a module, in
   clang's order and with rustc's options, through the analysis managers kept for the machine.
5. `src/lib.rs` states the contract of the C API as blaze calls it (who owns each `LLVM*Ref`,
   which calls take ownership, which strings need a NUL and which are freed with
   `LLVMDisposeMessage`, the thread rules) and says `#![forbid(unsafe_code)]`: the crate stays
   declarations only, and every call is made behind a safe wrapper in hir-ty-codegen's `llvm`.
6. `pin.rs`, included by build.rs and run before anything else there, links only the LLVM
   `crates/hir-ty-codegen/llvm.json` pins: `LLVM_SYS_231_PREFIX` must name a prefix whose
   `blaze-llvm.json` (written by `.github/scripts/llvm.py build`) has the pinned commit and
   version and whose `llvm-config --version` says that version. Anything else (the variable
   unset, a stock LLVM, another build) stops the build with one `cargo::error` naming what was
   found, the pin and the fix. Upstream's search of `PATH` and Homebrew, its version range and
   its blocklist (`LLVM_SYS_231_IGNORE_BLOCKLIST`, `LLVM_SYS_231_STRICT_VERSIONING`) are gone,
   with the `regex-lite` and `semver` build-dependencies; the `strict-versioning` feature is
   kept for inkwell and does nothing. `rust-version` is 1.77 for the `cargo::` directives. Every
   LLVM consumer in blaze links through llvm-sys, so this is the one check; `pin.rs` is also the
   `pin` test target.

Why here and not in hir-ty-codegen's build.rs: rustc names a crate's native libraries right after
that crate's rlib, and GNU ld takes from an archive only what earlier inputs asked for. Libraries
named by hir-ty-codegen come before inkwell's rlib, which calls LLVM, so ld.bfd left inkwell's
calls undefined. llvm-sys sits below inkwell, so its libraries come after every caller.

# flecs v4.1.6 and the hyperion flecs crates (vendored)

- Copied from hyperion (github.com/indexable-inc/hyperion) at commit `cb6a56bdba52cc22e576591a82863a5a10814f67`:
  `vendor/flecs` (amalgamation plus HYPERION-PATCHES.md), `crates/core/hyperion-flecs-c`,
  `crates/core/hyperion-ecs-sys` (bindings.rs unchanged except the nightly-only `#[assume_nonblocking]` lines) and
  `crates/core/hyperion-ecs` (world.rs, traits.rs, hooks.rs; trimmed).
- Copied from hyperion at commit `d428da7` (`crates/core`), trimmed to stable Rust (no `#[assume_nonblocking]` or
  `#[image_sink]`; layout recipes from `offset_of!` instead of the reflection intrinsic): the rest of `hyperion-ecs`
  (component.rs, query.rs, system.rs, untyped.rs, check.rs, module.rs), `hyperion-ecs-core` (identity by name and layout
  hash), `hyperion-layout` (layout.rs only; package `hyperion-abi-layout`), `hyperion-abi`, `hyperion-api-layouts`,
  `hyperion-panic`, `hyperion-wyhash` and `hyperion-manifest-types` (the data-only module manifest den-native's modules
  export).
- Upstream: https://github.com/SanderMertens/flecs/archive/refs/tags/v4.1.6.tar.gz, tarball sha256
  29ccf56961b7ffbd38cce2227a06c0722c7df464422e86619a65ee37bb31bae7. Licence MIT (LICENSE in this dir).
- Files: `distr/flecs.c` and `distr/flecs.h`, unmodified apart from the patches in HYPERION-PATCHES.md (hyperion's one and den's backport of an upstream script fix).
- Dropped from hyperion: its typed SDK crates and module macros, the dlopen loader and linker, phases, threads, hyperion-ecs's
  script loader, REST, stats, doc, units, nightly features, and the `m`/`pthread` link flags.
- flecs addons are features of `hyperion-flecs-c`: none is the entity-component core (`FLECS_CUSTOM_BUILD` plus `FLECS_LOG`),
  which den-core and den-web use, also on wasm32-unknown-unknown; `system`, `pipeline`, `meta` and `script` are what
  den-native's scene needs (the pipeline brings flecs's own OS API, so it is refused on wasm32).
- Added for den: `hyperion-flecs-c/wasm-libc` (headers and libc.c, the C library calls flecs makes, for wasm32 only),
  `hyperion-flecs-c/csrc/layout.c` (C struct sizes for the binding check) and the Rust allocator in
  `hyperion-flecs-c/src/lib.rs`.

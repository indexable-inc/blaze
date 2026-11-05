# The hyperion flecs crates (vendored) and the flecs they build

- flecs is not vendored: `hyperion-flecs-c` compiles the indexable-inc/flecs fork's amalgamation (`distr/flecs.c`,
  `distr/flecs.h`) through the fork's `flecs-distr` crate, a cargo git build-dependency pinned by rev in
  `hyperion-flecs-c/Cargo.toml`. That comment lists what the rev is (upstream v4.2.0 plus the fork's fixes) and how to
  move it. Licence MIT (the fork's LICENSE).
- Copied from hyperion (github.com/indexable-inc/hyperion) at commit `cb6a56bdba52cc22e576591a82863a5a10814f67`:
  `crates/core/hyperion-flecs-c`, `crates/core/hyperion-ecs-sys` (bindings.rs unchanged except the nightly-only
  `#[assume_nonblocking]` lines; now regenerated from the pinned flecs.h by gen-bindings.py) and `crates/core/hyperion-ecs`
  (world.rs, traits.rs, hooks.rs; trimmed).
- Copied from hyperion at commit `d428da7` (`crates/core`), trimmed to stable Rust (no `#[assume_nonblocking]` or
  `#[image_sink]`; layout recipes from `offset_of!` instead of the reflection intrinsic): the rest of `hyperion-ecs`
  (component.rs, query.rs, system.rs, untyped.rs, check.rs, module.rs), `hyperion-ecs-core` (identity by name and layout
  hash), `hyperion-layout` (layout.rs only; package `hyperion-abi-layout`), `hyperion-abi`, `hyperion-api-layouts`,
  `hyperion-panic`, `hyperion-wyhash` and `hyperion-manifest-types` (the data-only module manifest den-native's modules
  export).
- Dropped from hyperion: its typed SDK crates and module macros, the dlopen loader and linker, phases, threads, hyperion-ecs's
  script loader, REST, stats, doc, units, nightly features, and the `m`/`pthread` link flags.
- flecs addons are features of `hyperion-flecs-c`: none is the entity-component core (`FLECS_CUSTOM_BUILD` plus `FLECS_LOG`),
  which den-core and den-web use, also on wasm32-unknown-unknown; since 4.2 prefabs, constraint traits, cached queries and query plans are addons too (`prefab`, `constraint_traits`,
  `cached_queries`, `query_plans`); `system`, `pipeline`, `meta` and `script` are what den-native's scene needs (`system`
  brings flecs's frame addon and own OS API, so it and `pipeline` are refused on wasm32).
- Added for den: `hyperion-flecs-c/wasm-libc` (headers and libc.c, the C library calls flecs makes, for wasm32 only),
  `hyperion-flecs-c/csrc/layout.c` (C struct sizes for the binding check) and the Rust allocator in
  `hyperion-flecs-c/src/lib.rs`.

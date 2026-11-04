# flecs v4.2.0 and the hyperion flecs crates (vendored)

- Copied from hyperion (github.com/indexable-inc/hyperion) at commit `cb6a56bdba52cc22e576591a82863a5a10814f67`:
  `vendor/flecs` (the v4.1.6 amalgamation then; replaced by the fork build below), `crates/core/hyperion-flecs-c`,
  `crates/core/hyperion-ecs-sys` (bindings.rs unchanged except the nightly-only `#[assume_nonblocking]` lines) and
  `crates/core/hyperion-ecs` (world.rs, traits.rs, hooks.rs; trimmed).
- Copied from hyperion at commit `d428da7` (`crates/core`), trimmed to stable Rust (no `#[assume_nonblocking]` or
  `#[image_sink]`; layout recipes from `offset_of!` instead of the reflection intrinsic): the rest of `hyperion-ecs`
  (component.rs, query.rs, system.rs, untyped.rs, check.rs, module.rs), `hyperion-ecs-core` (identity by name and layout
  hash), `hyperion-layout` (layout.rs only; package `hyperion-abi-layout`), `hyperion-abi`, `hyperion-api-layouts`,
  `hyperion-panic`, `hyperion-wyhash` and `hyperion-manifest-types` (the data-only module manifest den-native's modules
  export).
- flecs: the fork github.com/indexable-inc/flecs, branch `indexable/v4.2`, commit
  `4086c73fd7da789822dc8d359263fefd6149383b`: upstream tag v4.2.0 (`6566d4b7bdc597d0b74d5e0a4104d4ae6ac49402`, also
  upstream master on 2026-11-04; tarball https://github.com/SanderMertens/flecs/archive/refs/tags/v4.2.0.tar.gz, sha256
  4107b0a36aa25f80ddcd6f939451ccfe1bacafcccdd2695840a4a8c413d05b40) plus nine commits: the fork's per-component column
  allocator, shortest round-trip JSON floats and query-cache notification while quitting; refusing pairs that break
  relationship constraints under FLECS_SOFT_ASSERT (was HYPERION-PATCHES #3); copying OrderedChildren to prefab instances
  (was #4); and two script fixes, world aliases winning over the script's own declarations and scoped declarations
  resolving as children rather than aliases; and two commits for deleting a parent before its Parent-component children,
  which aborted at shutdown with prefab scopes (upstream issue SanderMertens/flecs#2384, PR #2385). Licence MIT (LICENSE in
  this dir).
- Files: the fork's `distr/flecs.c` and `distr/flecs.h`, unmodified, built with upstream's own tool (`bake build .` in
  the fork; bake reproduces upstream's v4.2.0 distr byte for byte). Patch here by committing to the fork and re-vendoring.
- Dropped from hyperion: its typed SDK crates and module macros, the dlopen loader and linker, phases, threads, hyperion-ecs's
  script loader, REST, stats, doc, units, nightly features, and the `m`/`pthread` link flags.
- flecs addons are features of `hyperion-flecs-c`: none is the entity-component core (`FLECS_CUSTOM_BUILD` plus `FLECS_LOG`),
  which den-core and den-web use, also on wasm32-unknown-unknown; since 4.2 prefabs, constraint traits, cached queries and query plans are addons too (`prefab`, `constraint_traits`,
  `cached_queries`, `query_plans`); `system`, `pipeline`, `meta` and `script` are what den-native's scene needs (`system`
  brings flecs's frame addon and own OS API, so it and `pipeline` are refused on wasm32).
- Added for den: `hyperion-flecs-c/wasm-libc` (headers and libc.c, the C library calls flecs makes, for wasm32 only),
  `hyperion-flecs-c/csrc/layout.c` (C struct sizes for the binding check) and the Rust allocator in
  `hyperion-flecs-c/src/lib.rs`.

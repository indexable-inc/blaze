---
name: must_read_blaze
description: "Mandatory orientation guide for blaze. Load BEFORE doing anything wrt Rust or Cargo in this repo: what blaze is, bstd (crates/blaze-std) and its batteries, persistence/evolving schemas & VT snapshots, blaze's Rust language extensions & pattern types, LIR/Sota non-LLVM optimizer and slot-based execution, the Rust GPU/UI/PBR/CAD stack, simulation/linting/C-import/blaze-ctx tooling, and logging missing features or strictness wins in agents/feature-requests.md."
---

# Must-Read Orientation: `blaze`

Read this before touching Rust, Cargo, or tooling in this repo. Also check [`agents/howto/`](../../../agents/howto/) and [`agents/kb/`](../../../agents/kb/) for your area (rule 63).

> **Feature requests & strictness rule:** If anything in blaze (language, strictness/lints, `bstd`, compiler diagnostics, tooling) would have prevented a bug, confusion, or workaround — especially where stricter checking would have caught your mistake — append a concise entry to [`agents/feature-requests.md`](../../../agents/feature-requests.md).

## 1. What `blaze` is

- Vertically integrated Rust stack owning every layer: language, `bstd`, MIR/LIR, ABI, JIT/PGO/AOT, exporter, linter, runtime, and bare-metal/unikernel VMs (kernel too). Design in full: [`docs/v2/plan/design/bottom.md`](../../../docs/v2/plan/design/bottom.md).
- **Commands**: `blaze run`, `blaze test`, `blaze check`, `blaze export` (native, PGO by default). **No `build`, never `rustc`** (rules 1, 2). Dogfood `blaze` for inner loops (`blaze test crates/<crate>`), never cold `cargo`.
- **Storage & incrementality**: Item-grain records keyed from text before work (rule 65), written once, read in place via kernel page cache only; no in-memory dependency graph; restarts re-index nothing.

## 2. `bstd` (`crates/blaze-std`) & Batteries

Blaze's `#![no_std]` sans-IO, explicit-allocator, capability-based standard library replacing `std`, `alloc`, and third-party crates ([`docs/v2/plan/design/std.md`](../../../docs/v2/plan/design/std.md), [`crates/blaze-std/src/lib.rs`](../../../crates/blaze-std/src/lib.rs)):

- **Core primitives**:
  - Explicit allocators: `bstd::mem::{Alloc, Frame, Heap, String, format_in}` — never implicit `Vec::new()`/`format!()` (rule 11).
  - Task context: `bstd::cx::Cx` (monotonic deadline, cancellation, span, frame); capabilities narrowed from `bstd::io::Io` (`Fs`/`Dir`, `Net`, `Clock`, `Rng`, `Env`, `Args`, `Out`, ...).
  - Linear handles (`#[blaze::linear]` / `#[blaze::consumes]`: `Scope`, `Task`, `Pending`, writable `File`, `Conn`) and `#[blaze::args]` options structs (`T::new(TOpts { .. })` with `default_field_values`, rule 83).
- **Re-exported batteries (`bstd::*`)**:
  - Collections & concurrency: `vec`, `boxed`, `rows`, `soa` (`blaze-own`/`blaze-kit`), `map`/`set`/`hash` (`blaze-hash`/`blaze-hashcons`), `id`, `bits`, `paths`, `sync` (named `RankedLock`, `Swap`, `Flight`, `Chan`), `par`, `sort` (`blaze-radix`/`blaze-sort`), `phf`.
  - Data & formats: `bstd::data` ([`crates/blaze-data`](../../../crates/blaze-data)), `bstd::cbor` ([`crates/blaze-cbor`](../../../crates/blaze-cbor)), `bstd::json` ([`crates/blaze-json`](../../../crates/blaze-json)), `bstd::toml` ([`crates/blaze-toml`](../../../crates/blaze-toml)), `bstd::tab` ([`crates/blaze-tab`](../../../crates/blaze-tab)), `bstd::record` ([`crates/blaze-format`](../../../crates/blaze-format)), `bstd::columns` / `bstd::arrow`.
  - Domain & type-level batteries: `bstd::cli` ([`crates/blaze-std/src/cli.rs`](../../../crates/blaze-std/src/cli.rs) — in-house `#![no_std]` `clap` replacement: const `CommandSpec` with `default_field_values`, nested subcommands, aliases, hidden flags/cmds, `default_missing_value`, typo hints, ANSI `--help`/`--version`; **always use for CLI binaries — never hand-roll `env::args` loops or add `clap`**; pair with `blaze-cli-spec` for `blaze-shell` completion specs), `bstd::ecs` ([`crates/blaze-ecs`](../../../crates/blaze-ecs)), `bstd::git` ([`crates/blaze-git`](../../../crates/blaze-git), feature `git`), `bstd::algebra` ([`crates/blaze-algebra`](../../../crates/blaze-algebra)), `bstd::pat_ty` ([`crates/blaze-std/src/pat_ty.rs`](../../../crates/blaze-std/src/pat_ty.rs)), `bstd::record_ty`, `bstd::union_ty`, `bstd::view`, `bstd::type_set`, `bstd::comptime`, `bstd::reflect`, `bstd::test` (`blaze-core::sim::Sim`, `blaze-check`, `blaze-alloc-count`, plus [`crates/blaze-sim`](../../../crates/blaze-sim) under `sim-draws`).
- **Companion crates beside `bstd`**: [`crates/blaze-vt`](../../../crates/blaze-vt) (sans-IO Ghostty-grade terminal core; [`docs/v2/plan/design/vt.md`](../../../docs/v2/plan/design/vt.md)), [`crates/blaze-ui`](../../../crates/blaze-ui) (ECS UI + SDF/glyph renderer), [`crates/blaze-ty/src/pat_ty.rs`](../../../crates/blaze-ty/src/pat_ty.rs) (pattern-type algebra), and planned `blaze-sh` / `blaze-shell` ([`docs/v2/plan/design/sh.md`](../../../docs/v2/plan/design/sh.md)).

### Persistence, Evolving Schemas & Portable VT Snapshots

- **Choose the right persistence tier**:
  - **`blaze-format`** ([`crates/blaze-format`](../../../crates/blaze-format)): `Persist` + `SchemaHash` for **immutable, zero-copy, rebuildable compiler caches** — schema bump = clean rebuild, no migrations.
  - **`blaze-data` + `blaze-cbor`** ([`crates/blaze-data`](../../../crates/blaze-data), [`crates/blaze-cbor`](../../../crates/blaze-cbor)): self-describing `Value` / RFC 8949 CBOR for **evolvable user/app state** (configs, sessions, indexes).
- **Explicit strict vs. lossy decoding (`bstd::data`, [`crates/blaze-data/src/lossy.rs`](../../../crates/blaze-data/src/lossy.rs))**:
  - `decode_strict` (default): refuses unknown or missing keys with exact dotted path + typo hints (`ShapeError`).
  - `decode_lossy` → `Decoded<T, A> { value, unknown, defaulted }`: never silent; preserves unconsumed keys and defaulted paths explicitly so `merge_unknown_into` retains newer-schema fields across downgrades without overwriting current fields.
  - `decode_versioned_str` validates a finite allowed version-tag list (with typo hints); `decode_list_isolated` decodes collections per item into `ItemOutcome::Ok(T)` | `ItemOutcome::Degraded { index, error, raw }` so one corrupt entry never drops healthy siblings.
- **Canonical portable VT snapshots ([`crates/blaze-vt/src/snapshot.rs`](../../../crates/blaze-vt/src/snapshot.rs))**: `Terminal::snapshot_vt` / `Terminal::restore_vt_snapshot` (`VtSnapshot`) emit minimal canonical VT restore byte streams (`primary_vt`, optional `alt_vt`) plus compact soft-wrap & alt-screen metadata instead of internal `PageList`/`StyleSet`/`ExtTable` structs — keeping terminal persistence decoupled from internal layout changes.

## 3. Language Additions Relative to Rust

Check live item-layer status in [`docs/v2/plan/design/extensions.md`](../../../docs/v2/plan/design/extensions.md) and value refinements in [`docs/v2/plan/design/refined-values.md`](../../../docs/v2/plan/design/refined-values.md).

- **Comptime reflection**: `#![feature(blaze_comptime)]`, `for const f in fields_of::<T>()`, `x.[f]`, `build`, `#[blaze::reflect]`, `#[blaze::mark(M)]`, `const static {}`, `type_of!`, `fn_info` ([`library/core/src/reflect.rs`](../../../library/core/src/reflect.rs), `bstd::comptime`, `bstd::reflect`). Prefer reflection over proc macros or auto traits (rule 56).
- **Arbitrary-width ints & contracts**: `u1..u128` / `i1..i128` (`#![blaze::arbitrary_ints]`), `where const { COND }`, relational `#[blaze::invariant(..)]`, `requires`/`ensures`.
- **Structural types & views**: structural records `{ a: T, b: U = 0 }` (`bstd::record_ty`), `#[blaze::args]` + `default_field_values`, anonymous error unions `A | B` (`bstd::union_ty`), `&uniq`, field views `&self.{a}` (`bstd::view`).
- **Effects, linearity & proofs**: `#![blaze::sans_io]`, `noalloc`/`nonblocking`/`nopanic`/`pure` (under the `bstd` edition, `noalloc`/`nonblocking`/`sans_io`/`pure` derive from signatures while `nopanic`/`diverge` propagate), linear types (`#[blaze::linear]`, `#[blaze::consumes]`), Lean proofs (`#[blaze::theorem]`).

### Pattern & Refinement Types (`T is PAT`)

Unified single-value refinement surface in [`crates/blaze-ty/src/pat_ty.rs`](../../../crates/blaze-ty/src/pat_ty.rs) and [`crates/blaze-typeck`](../../../crates/blaze-typeck), with a portable zero-cost `#![no_std]` library counterpart in `bstd::pat_ty` ([`crates/blaze-std/src/pat_ty.rs`](../../../crates/blaze-std/src/pat_ty.rs)):

- **`IntPatTy`** (`u32 is 0..=100`, `usize is 1..`): zero-cost same-layout canonical sorted disjoint intervals, free widening (`T is 1..=10 <: T is 1.. <: T`), niche optimization (`Option<u8 is 0..=100>` = 1 byte), exact bridge to packed `u1..u128`.
- **`VariantPatTy`** (`Conn is Conn::Open { .. } | Conn::Draining { .. }`): bitset enum narrowing (`<= 64` variants), direct single-variant field access (`sole_variant`), flow-sensitive `match` exclusion, and `&uniq` whole-write guard preventing escapes from `PAT`.
- **`ConstSetPatTy`** (`SemVer is in VERSIONS`, `&str is in ["1.1.1", "1.2.2"]`): canonical sorted/deduped finite sets of `StructuralPartialEq` compile-time constants; free subset subtyping (`OLD <: ALL <: T`), flow-sensitive `match` exhaustiveness (`narrow_exclude` + `missing_members` witnesses), and strict `&uniq` field + whole write guards.
- **Portable `bstd::pat_ty` today (`#![no_std]`, zero `unsafe`)**: `ValueSet<T>`, `FinitePat`, `#[repr(transparent)] InSet<T, S>` (exposes `Deref<Target = T>`, never `DerefMut`), and `bstd::define_value_set!`:
  ```rust
  const VERSIONS: [SemVer; 3] = [SemVer(1, 1, 1), SemVer(1, 2, 2), SemVer(3, 4, 5)];
  bstd::define_value_set! { pub struct KnownVersions: SemVer = &VERSIONS; }
  let v = bstd::pat_ty::InSet::<SemVer, KnownVersions>::refine(VERSIONS[1]).unwrap();
  // `v` derefs to `&SemVer` with zero wrapper overhead; `v.widen()` returns `SemVer`.
  ```

## 4. LLVM Replacement & Execution

- **LIR ([`crates/blaze-lir`](../../../crates/blaze-lir))**: Columnar SSA LIR (`Func`/`View`), bottom-up Tarjan SCC ThinLTO (`lto.rs`), coroutine splitting (`coroutine.rs`), decayed cross-run PGO `BPG1` (`pgo.rs`, rule 86), and `aegraph` e-graph + loop/regalloc planners (`simplify/egraph.rs`).
- **Codegen ([`crates/blaze-emit`](../../../crates/blaze-emit), [`crates/blaze-tier`](../../../crates/blaze-tier))**: Single-pass AArch64 baseline (`emit_record`) + experimental non-LLVM optimizing tier `TierName::Sota` ([`crates/blaze-emit/src/sota/`](../../../crates/blaze-emit/src/sota/), [`docs/v2/plan/design/sota-tier.md`](../../../docs/v2/plan/design/sota-tier.md)): Acyclic E-Graphs (`aegraph`) + ISLE-style DAG ISEL (`isel.rs`) + Chordal-SSA regalloc (`regalloc.rs`) for AArch64 & x86-64. Cranelift is ONLY used inside Wasmtime for `wasm32-wasip1` proc macros (rule 10).
- **Slot-based execution ([`crates/blaze-run`](../../../crates/blaze-run), [`crates/blaze-launch`](../../../crates/blaze-launch))**: Position-independent code records placed into slot words in the program's own process, compiled on first call, epoch-retired on edit or background tier-up — no link step ([`docs/v2/plan/design/run.md`](../../../docs/v2/plan/design/run.md), [`docs/v2/plan/design/launcher.md`](../../../docs/v2/plan/design/launcher.md)).

## 5. GPU, UI, PBR & CAD Stack

- **Rust GPU shaders ([`crates/gpu`](../../../crates/gpu), [`crates/gpu-macros`](../../../crates/gpu-macros), [`docs/future/gpu/shaders.md`](../../../docs/future/gpu/shaders.md))**: `#![no_std]` Rust shaders (`#[gpu::vertex]`, `#[gpu::fragment]`, `#[gpu::kernel]`) emitting MSL 2.4 & WGSL via `gpu::emit`; `Pod`/`ShaderStruct`/`Varyings`; phased workgroup `SharedMem` (`SharedWrite` → `.barrier()` → `SharedRead`); inline `RayQuery`/`Tlas`; deterministic CPU compute & software triangle rasterizer oracle (`gpu::cpu`).
- **UI ([`crates/blaze-ui`](../../../crates/blaze-ui))**: `#![no_std]` sans-IO relational ECS UI + Rust-authored SDF & Slug glyph GPU renderer (Metal & WebGPU).
- **PBR & CAD**: [`crates/blaze-pbr`](../../../crates/blaze-pbr) (GGX BRDF, AgX/ACES tonemap, procedural IBL/smart materials) and `crates/blaze-cad*` ([`blaze-cad`](../../../crates/blaze-cad), `blaze-cad-field`, `blaze-cad-mesh`, `blaze-cad-out`, `blaze-cad-picture`, `blaze-cad-cloth`) for SDF/CSG CAD, meshing, glTF/STL/SVG export, and ECS/Flecs scenes.

## 6. Other Essential Systems

- **Deterministic simulation ([`crates/blaze-sim`](../../../crates/blaze-sim), [`docs/v2/plan/design/sim.md`](../../../docs/v2/plan/design/sim.md))**: Shrinking choice-tape engine + `blaze_core::sim::Sim` + planted-bug controls. Mandatory proof (`dst-not-review`, rule 43) for concurrent, durable, crash-recovering, billing, or deleting code.
- **Linting & structural rewrites**: [`crates/blaze-lint`](../../../crates/blaze-lint) (single-pass item-grain token lints, `Finding` records, `Fix` edits) + Profile-Guided Linting ([`crates/blaze-lint/src/pgo.rs`](../../../crates/blaze-lint/src/pgo.rs)) + [`tools/astlog`](../../../tools/astlog) Datalog AST queries/rewrites (rule 13).
- **LLM-first data & fast repo context**: [`crates/blaze-tab`](../../../crates/blaze-tab) (compact tabular format over `blaze-data::Value`, 40–60% fewer tokens than JSON, sparse `_` sentinels) and **`blaze-ctx`** ([`crates/blaze-ctx`](../../../crates/blaze-ctx), [`docs/v2/plan/design/ctx.md`](../../../docs/v2/plan/design/ctx.md)): incremental BLAKE3/CBOR hierarchical context & hybrid search (`index`, `map`, `query "..." --budget 2000`, `area <crate>`, `outline <doc>`) emitting compact `blaze-tab` tables with exact `#Lstart-Lend` slice pointers.
- **Hermetic C import ([`crates/blaze-c`](../../../crates/blaze-c), [`docs/v2/plan/design/c.md`](../../../docs/v2/plan/design/c.md))**: Sans-IO `blaze::c::import!` header import without build scripts.
- **macOS hypervisor sandbox (`hvbox`)**: [`docs/v2/plan/design/hvbox.md`](../../../docs/v2/plan/design/hvbox.md) & [`crates/blaze-proc-macro/src/spawn_hvbox.rs`](../../../crates/blaze-proc-macro/src/spawn_hvbox.rs) — experimental kernel-less Hypervisor.framework sandbox for build-script spawns (**DO NOT run**; gated after the 2026-11-16 macOS SPTM kernel panic).

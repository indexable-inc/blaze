---
status: current
summary: Blaze's automatic performance stack: explicit-allocator stdlib primitives, OS-in-the-types Dir<P, S> and comptime IO capabilities, Profile-Guided Linting (PGL) with newtype/tuple-return discipline, and closed-loop cross-run decayed PGO driving LIR bottom-up SCC LTO, live JIT tier-up, and `blaze export` by default.
date: 2026-11-22
---
# Automatic Performance, Capability Types & Closed-Loop PGO

## Priority Order

When performance tradeoffs compete in Blaze, decisions follow one fixed order across both
128-core EPYC / Graviton 5 servers and typical Macs (one code path):

1. **Incremental compile time** (including huge crates and huge files even after a cold reboot,
   with zero re-indexing and automatic cross-worktree sharing) **=** **Lint speed**
2. **Remote cache fetch** (R2 / P2P content-addressed records, coalesced HTTP Range fetches)
3. **Memory footprint** (explicit allocators, SoA columns, frame-reset scratch)
4. **Cold compile time**
5. **Non-locking disk GC**
6. **Disk usage**

Selective incremental testing (`blaze-test-reach` `BTI1` zero-reindex SoA impact store) rides
the same content-addressed discipline: only tests whose transitive read set intersects the
edited item digests rerun.

## Prior Art vs. Blaze's Innovations

| Layer | Prior Art | What Blaze Does Differently |
|---|---|---|
| **Stdlib perf primitives** | `smallvec`, `crossbeam-utils::CachePadded`, custom generation arrays | Built into `bstd::mem` and `bstd::sync` (`#![no_std]`, zero `unsafe`, explicit `Allocator`): `SmallVec<T, N, A>`, `GenMap<K, V, A>` (O(1) epoch-bump `clear()`), `const fn` `KindSet<W>`, unconditional 128-byte `CachePadded<T>`, safeguarded O(log log N) `interp_search_by_prefix`. |
| **Capability OS & Comptime IO** | Capsicum, CloudABI, WASI Preview 2 `Dir`, Zig `@embedFile` | **OS in the types** via monotone zero-cost marker axes: `Dir<P: Rights, S: Scope = Subtree>` (`ReadWrite -> ReadOnly`, `Subtree -> Flat` with `Leaf` single-segment names, plus monotone `Budget` quotas). Comptime runs (`blaze-comptime` + `blaze-roots`) bind every read/stat/list—and every `Seen::Absent` lookup—into the incremental `ReadList` automatically. |
| **Linting** | Clippy, rust-analyzer diagnostics | **Profile-Guided Linting (PGL)** (`crates/blaze-lint/src/pgo.rs`): weights findings by historical runtime `share_ppm`, auto-promotes hot-path perf findings (`Clones`, `Allocs`, `AmbientHeap`, `AmbientHasher`, `SeqCstUnexplained`, `RawSync`, `BlockingOnPool`) to `Error`, optionally suppresses cold perf noise, and suggests `GenMap`/`SmallVec`/`CachePadded`/`interp_search` directly on hot functions. |
| **Type discipline** | Manual code review, scattered lints | Zero-extra-pass token rules (`126+ MB/s/core` across all 31 rules): `blaze::bool_param`, `blaze::swappable_parameters`, `blaze::primitive_meaning`, and `blaze::anon_tuple_return` (denying anonymous stateful tuple returns like `(Option<T>, bool)` in favor of named structs). |
| **PGO + LTO + JIT** | V8 Maglev/TurboFan, Graal/Truffle partial eval, AutoFDO/BOLT/Propeller | **Closed-loop content-addressed PGO** (`crates/blaze-lir/src/{pgo.rs, lto.rs}` + `blaze-inproc`): cold-start static estimator (`estimate_static`) seeds run #1; cross-run exponentially decayed `BPG1` records survive reboots and share across worktrees; bottom-up Tarjan SCC LTO runs `scalar_replace -> propagate -> dead` + PGO-guided `inline_guided` + post-inline SROA/const-fold; `PgoTier` joins JIT code keys for zero-warmup reuse; and **`blaze export` consumes persisted PGO by default**. |

## 1. `bstd` Explicit-Allocator Performance Primitives

Located in `crates/blaze-std/src/mem/fast.rs` and `crates/blaze-std/src/sync.rs`:

- **`bstd::mem::SmallVec<T, const N: usize, A: Allocator>`**: inline stack storage up to `N`
  `Copy` items, spilling to explicit allocator `A` only past `N`, with fallible `try_push`
  returning `Stopped::OutOfMemory { arena, asked }`.
- **`bstd::mem::GenMap<K, V, A: Allocator>`**: open-addressed generation-stamped table with
  O(1) `clear()` by bumping a `u32` epoch counter (zero memory zeroing in hot per-item loops).
- **`bstd::mem::KindSet<const W: usize>`**: `const fn` fixed bitset for O(1) enum/token-kind
  membership checks, compiled to a single shift + bit-test.
- **`bstd::mem::interp_search_by_prefix`**: O(log log N) interpolation search over sorted
  64-bit hash/digest prefixes with automatic binary-search fallback after 3 stale steps on
  adversarially skewed keys.
- **`bstd::sync::CachePadded<T>`**: unconditionally `#[repr(align(128))]` on both x86-64
  (adjacent-line hardware prefetcher pairs = 128B) and ARM64 (Apple Silicon / Graviton 128B
  L1/L2 lines), eliminating false sharing on 128-core EPYC / Graviton 5 systems with one code path.

## 2. OS in the Types: `Dir<P, S>` & Comptime Directory IO

### Why a Sealed Capability Lattice Beats `const DEPTH: usize`

An arbitrary numeric depth parameter (`Dir<P, const D: usize>` with `where { D > 0 }`) creates
infectious const-generic bounds across helper signatures without matching how OS and sandbox
policies actually work. Instead, `bstd::fs::Dir<P: Rights, S: Scope = Subtree>` separates three
orthogonal, monotone narrowing axes with zero runtime cost (`PhantomData`):

1. **Rights (`P: Rights`)**: `ReadWrite -> ReadOnly` (`dir.read_only()`). Never widens.
2. **Scope (`S: Scope = Subtree`)**: `Subtree -> Flat` (`dir.flat()`).
   - `Dir<P, Subtree>` (default, source-compatible with `Dir<P>`): exposes `subdir()`, `walk()`
     (with runtime `WalkLimits { max_depth, max_entries }`), and relative `Path` operations.
   - `Dir<P, Flat>`: direct children only. `subdir()` and `walk()` do not exist at compile time,
     and methods accept only `Leaf` (a validated single-segment name with no `/`, `.`, or `..`).
3. **Budget (`Budget`)**: `dir.quota(bytes)` nests monotonically inside any parent quota.

### Comptime Directory IO (`blaze-comptime` + `blaze-roots`)

Compile-time code (proc macros and build scripts) should receive capability handles rather than
ambient `CARGO_MANIFEST_DIR` strings:

- **Proc macros**: `Dir<ReadOnly, Subtree>` (or narrowed `Dir<ReadOnly, Flat>`) rooted at the
  package directory, backed by `blaze-roots::Roots` and `ReadList`. Every read, stat, directory
  listing, and **missing-file lookup (`Seen::Absent`)** is recorded automatically so incremental
  invalidation is sound by construction.
- **Build scripts**: package `Dir<ReadOnly, Subtree>` plus a quota-bounded `OUT_DIR`
  `Dir<ReadWrite, Subtree>`, with zero ambient environment or filesystem reach.

## 3. Profile-Guided Linting (PGL) & Type-Discipline Rules

Located in `crates/blaze-lint/src/{pgo.rs, engine.rs}`:

- **PGL ranking & auto-promotion (`rank_findings`)**: maps every finding to its innermost
  enclosing function's historical runtime share (`Heat::{Cold, Warm, Hot, Critical}`), promotes
  performance rules (`Clones`, `Allocs`, `AmbientHeap`, `AmbientHasher`, `SeqCstUnexplained`,
  `RawSync`, `BlockingOnPool`) in hot functions to `Error`, optionally suppresses cold perf
  noise (`PgoPolicy::HOT_ONLY`), and orders output deterministically by impact.
- **PGO-guided perf advisor (`suggest_from_pgo`)**: single-pass token scan over hot functions
  recommending `GenMap` (for `.clear()` inside loops), `SmallVec` (for hot local `Vec::new`),
  `CachePadded` (for bare `Atomic*` fields in structs), and `interp_search_by_prefix` (for hot
  `.binary_search` calls).
- **Static newtype & return-struct rules** (zero extra lex pass):
  - `blaze::bool_param` (Rule 28): flags bare `bool` params on `pub fn` (excluding trivial `set_*` setters).
  - `blaze::swappable_parameters` (Rule 29): flags adjacent identical primitive/string params on `pub fn` (excluding symmetric math/range pairs like `(x, y)`, `(min, max)`).
  - `blaze::primitive_meaning` (Rule 30): flags bare numeric params whose names carry units or IDs (`_ns`, `_ms`, `_bytes`, `_id`, `timeout`, `port`, `pid`, `fd`).
  - `blaze::anon_tuple_return` (Rule 31): flags anonymous tuple returns carrying unnamed state (`(Option<T>, bool)`, `(T, bool)`, `Option<(A, B, C)>`, `Result<(T, bool), E>`) where a named struct should name each field.

## 4. Closed-Loop Cross-Run PGO -> LIR SCC LTO -> JIT & `blaze export`

Located in `crates/blaze-lir/src/{pgo.rs, lto.rs, summary.rs, inline.rs}`:

1. **Cold-start static estimate (`PgoSiteTable::estimate_static`)**: on run #1 before any runtime
   samples exist, seeds hot weights for calls inside loops and `0` for `#[cold]` / landing pads.
2. **Cross-run persistence & decay (`BPG1`, `merge_decayed`)**: persisted as a compact,
   BLAKE3-verified `BPG1` binary record shared across reboots and worktrees; each new run merges
   with exponential decay (`old * decay_num / decay_den + fresh`) so hot paths stay warm without
   stale drift.
3. **Bottom-up Tarjan SCC LTO (`lto_optimize_module`)**: condenses intra-module direct calls into
   reverse-topological SCCs, simplifies callees first (`scalar_replace -> propagate -> dead`),
   caches `FnSummary` (`const_param_mask`, `sroa_ptr_param_mask`, `summary_hash` for early cutoff),
   runs PGO-guided priority inlining (`inline_guided_with_cache`), and re-runs post-inline SROA +
   constant folding + DCE across inlined boundaries.
4. **Live JIT tier-up & code reuse (`advise_jit_tier_up`, `PgoTier`)**: prioritizes hot looping
   bodies (`4x` loop boost) for background Tier-1 compilation and atomic trampoline patching
   (`blaze-inproc::JitPool::retarget`), while keying compiled code by `(body_digest, PgoTier)` so
   hot functions are reused directly from disk on the next run.
5. **`blaze export` default**: native release exports load the persisted `BPG1` PGO record by
   default (falling back to `estimate_static` when absent), automatically applying real workload
   profiles to ahead-of-time builds with zero manual PGO workflow.

## 5. Language Extensions Enabled Next

- **`#[blaze::soa]`**: compiler-lowered Structure-of-Arrays container transformation (matching
  `blaze-own::Soa` without manual column boilerplate).
- **`#[blaze::specialize]` / PGO-inferred cloning**: leverages `FnSummary::const_param_mask` to
  clone hot callees for dominant constant arguments observed in `BPG1` profiles.
- **`#[blaze::scratch]`**: lowers loop-local temporary collections to frame-backed `SmallVec` /
  `GenMap` automatically when escape analysis proves no values escape the iteration.

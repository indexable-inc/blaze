---
status: plan
summary: speed of light for hir-ty's type relations in next_solver/infer/relate (generalization, LUB/GLB, entering binders), with the arithmetic, what the code does to reach 90% of it, and what outside these files still stands in the way.
date: 2026-10-29
---
# Speed of light: `next_solver/infer/relate/`

Files: `crates/hir-ty/src/next_solver/infer/relate/{generalize,lattice,higher_ranked,mod}.rs`.
Nothing here was run: costs are counted from the code and the machine, not measured. `proof`'s
budgets measure them once the tree builds.

## Machine and unit costs

Apple M5 Max (`sysctl`: 6 performance cores, 16 MB L2 per performance cluster). Assumed per
performance core: about 4.5 GHz, L1 hit about 3 cycles, L2 hit about 16 cycles, DRAM about
100 ns, sustained IPC about 3.5 on branchy code.

One inference context is sequential, so every component below is bound by the critical path of
one core. The parallelism comes from running many bodies at once (the db lane's scheduler), not
from inside a relation. The working set is 4-byte interned handles and their interned kinds,
which stay in L1 and L2. Memory bandwidth (hundreds of GB/s) is never the bound: one
generalization touches well under a kilobyte.

The unit costs the bounds use:

| step | cost |
|---|---|
| read a type's cached flags (one dependent load, L1 or L2) | 4 to 16 cycles, about 2 ns |
| intern one rebuilt node: hash the kind (~10 instructions) plus one probe (L2) and a compare | about 45 cycles, about 10 ns |
| make a fresh region variable: push a universe, log the undo, intern `ReVar` | about 4 ns |
| make a fresh type or const variable: push to the union-find, `sub_unify`, log the undo | about 4 ns |

## Generalization (`instantiate_ty_var`, `instantiate_const_var`)

Take one instantiation `?T rel S`. The work that can't be skipped:

- every *flagged* node of `S` (one whose flags meet `NEEDS_GENERALIZATION`: free regions,
  inference variables, placeholders or non-rigid aliases) is read and rebuilt: `F` of them;
- every unflagged child of a flagged node has its flags read once and is returned as is: `C`;
- every region in a variant position, or one the target's universe can't name, gets a fresh
  variable: `R`;
- every type or const variable that needs a fresh one gets it: `V`.

So the bound is

\[ T_{\text{gen}} \approx 10F + 2C + 4R + 4V \ \text{ns}. \]

For example, `?0 <: &'a Vec<(u32, &'b str)>` has \(F = 4\) (the two `Ref`s, the `Adt` and the
`Tuple`), \(C = 2\) (`u32`, `str`), \(R = 2\) and \(V = 0\), so
\(4 \cdot 10 + 2 \cdot 2 + 2 \cdot 4 = 52\) ns.

How the code reaches 90% of this:

- **Fast path:** `tys` returns an unflagged type after one flags check, before the cache and
  before any recursion. That makes `C` cost one load each, and a whole unflagged subtree costs
  one load.
- **Cache:** the cache is an inline 8-entry array that spills to an `FxHashMap` (rustc's
  `SsoHashMap` layout), so a generalization allocates nothing until it has seen more than 8
  distinct flagged types.
  - A lookup scans at most 8 keys of 8 bytes each. For small `S` the array holds fewer than
    `F` entries, so about 1 to 2 compares, around 5 cycles or 1 ns per flagged node, which is
    10% of that node's 10 ns.
  - The cache can't be dropped. Generalizing two occurrences of `?1` must give the same fresh
    variable, as rustc's does, and without it a type with shared subtrees takes exponential
    time.
- **No per-node tracing:** `#[instrument]` is off `tys`, `regions`, `consts`, `binders`,
  `relate_with_variance` and the inner `instantiate_var`. The workspace `tracing` has no
  compile-time level cap, so a disabled span still costs a callsite check, about 1 to 2 ns per
  call, which is 10 to 20% of a node. The two per-instantiation entry points keep their spans:
  about 1 ns against about 50 ns.
- **Setup:** `RefCell` borrows are a flag check each. The target's `try_resolve_*_var` and
  root lookup are two union-find finds, which hit L1 after path compression. Setting up the
  generalizer costs a few stores.

- **Memory:** the spill past 8 entries is the one general-purpose allocation left in these
  files. [memory.md](../../design/memory.md) puts it in the worker's frame allocator, which
  `blaze-mem` will provide; this lane asked the `mem` lane for it.

That leaves the code's overhead at about 10% of \(T_{\text{gen}}\), from the cache scan.

What stands in the way, outside these files:

1. **Unchanged nodes are re-interned** (`vendor/rustc_type_ir/src/relate.rs`,
   `structurally_relate_tys`).
   - Every kind except `Adt` is rebuilt through the interner even when every part came back
     unchanged: `Ref`, `RawPtr`, `Tuple`, `Dynamic`, `Coroutine` and even `Foreign`.
   - In an invariant generalization whose variables are all nameable, most flagged nodes come
     back unchanged. Each still pays a hash probe of about 5 to 10 ns that an "every part is
     identical, return `a`" check would skip. On those nodes this brings the generalization to
     about 2 ns per node, under the bound above.
   - Owner: solver-sync, as a blaze patch on `vendor/rustc_type_ir`.
2. **Tracing in release.** A workspace `tracing` feature `release_max_level_info` (or `_off`)
   would make every remaining span free in release builds across all crates. That is a
   coordinator decision about release logging, not something this lane can set.

Benchmark: rustc's own generalizer at 16d1ffac. This code has the same structure (the flags
fast path and the inline cache) without rustc's per-node tracing spans.

## LUB and GLB (`lattice.rs`)

These are used by coercion to find a common supertype or subtype. Per node pair, the work is:

- one `shallow_resolve` on each side (a union-find find, about 2 ns);
- a kind match;
- one rebuilt node (10 ns);
- for a side that is a type variable: one fresh variable (4 ns) plus two subtype relations of
  that variable against the two sides, each a generalization bounded as above.

\[ T_{\text{lub}} \approx 14P + 4X + 2\,T_{\text{gen}} \cdot X \ \text{ns} \]

for `P` node pairs and `X` variable sides.

The code does no work beyond rustc's: two `sub_raw` calls per variable side, the region LUB or
GLB pushed straight into the collector, and no allocation unless obligations are produced.

What stands in the way: each invariant argument goes through `At::eq`, which builds a fresh
`TypeRelating` and an obligations list per argument. That is a few stores when no obligations
come out, under 10% of a 14 ns pair. rustc does the same. Making it cheaper needs a lattice
relation that relates invariant arguments in place, which would change `infer/at.rs`, not these
files.

## Entering a binder (`enter_forall`, `enter_forall_and_leak_universe`)

The work is:

- one check of the binder's escaping bound variables, returning at once when there are none
  (no clone of the binder);
- one universe increment;
- one fold over the nodes that contain escaping bound variables (`N`), skipping every other
  subtree after one flags load (`K`);
- one placeholder interned per bound-variable occurrence (`B`), a probe hit after the first.

\[ T_{\text{forall}} \approx 10N + 2K + 8B \ \text{ns}. \]

To reach it, the bound variables are replaced through a static `ToPlaceholders` delegate rather
than `FnMutDelegate`'s three `dyn FnMut` closures. That removes an indirect call per occurrence
and lets the placeholder intern inline into the fold. rustc still uses `FnMutDelegate` here.

What stands in the way: none in these files. The fold is vendor `BoundVarReplacer`, which
already rebuilds only nodes that hold escaping bound variables.

## Leak check (`leak_check`)

It forwards to `RegionConstraintCollector::leak_check` in `infer/region_constraints/` (not this
lane), and its bound belongs to that file's notes.

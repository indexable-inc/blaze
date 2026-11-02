---
status: plan
summary: Speed of the IR builder (`crates/hir-ty-codegen/mod.rs`) on v2's path, per component - the state of the art it is held to, what one function's compile cost before and after the 2026-10-29 pass (allocations, copies, refcount writes, environment reads, hashes), and what is still missing.
date: 2026-10-29
---
# The IR builder's hot paths (speed-backend-lower)

The IR builder lowers one MIR body to Cranelift IR (`build`, `settle`, `FunctionCx` and every
`fx.*` method), builds callees in (`inline`) and, through `emit`, runs Cranelift on the result.
v2 calls it once per function of a unit: `v2/backend.rs::built` calls `instance_code(..,
Detach::Unit)`, which builds and settles and hands the IR over (`Produced::Pending`); later
`v2/backend.rs::compiled` runs Cranelift on it. Its cost is a per-function cost, paid for every
function of every unit the memo misses on. Nothing here was measured: the pass ran with no
builds (the lane's rule), so the figures are counts read off the code, and the timings are the
benchmark lane's to take.

## Cranelift contexts

**State of the art.** Cranelift's `Context` keeps the capacity of its control flow graph,
dominator tree, loop analysis and, the largest, the register allocator's `regalloc2::Ctx`
through `Context::clear`; its docs advise reusing one per thread for throughput. Wasmtime's
Cranelift compiler keeps a pool of contexts (`CompilerContext`: a `Context`, a
`FunctionBuilderContext` and more) and takes one per function, so a compile allocates only what
grows past the largest function that thread compiled before.

**Before.** Each `build` made a fresh `Context::new()` and `FunctionBuilderContext::new()`; each
built-in body (`built_in_body`) did too; v2's `compiled` made a third, `Context::for_function`.
So three contexts per function, each allocating its CFG, dominator tree, SSA tables and register
allocator from empty: tens of allocations per function before any IR is written.

**After.** `crates/hir-ty-codegen/spare.rs` keeps, per thread, a small stack (4) of cleared
`Context`s and of `FunctionBuilderContext`s that a `FunctionBuilder` has finalized (which leaves
them empty). `build`, `compile_as` and `built_in_body` take from it and give back; the function
itself moves out (`mem::replace` with an empty `ir::Function`) so the context can be kept while
the IR goes on to v2. `with_context(func, |context| ..)` is the same for v2's compile, which
`v2/backend.rs::compiled` moves to (request in `v2/requests/codegen-adapter.md`). It is a stack
and not one slot because builds nest: a callee built in is built while its caller is. A
context whose function had more than `SINGLE_PASS` instructions is let go, so one huge function
doesn't pin its capacity on the thread for good. Steady state: no allocation for a context of a
function no larger than one the thread built before. Tests: `spare.rs` (a kept context and
builder build the IR a fresh one does; at most `KEPT` are held) and `mod.rs`
`a_kept_context_compiles_what_a_fresh_one_does` (same bytes, relocations and alignment out of
Cranelift after a larger function grew the kept context).

**Missing.** The shims built on `Recorder::builder_ctx` (`mod.rs`, v1's glue and thunks) keep
one builder context per `Recorder` rather than drawing from the thread's. Cranelift's `CompiledCode` buffer is still copied out (`bytes.to_vec()`)
because `Code` owns its bytes; that is the `Code` layout's change (speed-backend-code), as are
relocations as 16-byte columns (offset, kind, target).

## Cranelift's incremental cache

**State of the art.** A compile cache keys once, at the coarsest unit that changes: v2's memo
keys a function by its shape (`v2/shape.rs`, the printed IR with names by place).

**Before.** `emit` compiled through Cranelift's incremental cache in v1's store when there was
one (`compile_with_cache`): a serialization and hash of the whole function, plus a store lookup,
for a function the v2 memo had just missed on, over the same IR.

**After.** `emit` (v2's) compiles with no cache: one path, not "try the cache then compile".
v1's `Backend::emit` gets `emit_cached` (request to speed-backend-debug); both go through
`emit_with(ctx, store)`. Same bytes either way: the cache changes where code comes from, not
what it is.

## Layouts

**State of the art.** rustc interns layouts (`Layout<'tcx>` is an interned reference); a
lookup is a hash and a pointer copy. Zig's self-hosted backend reads type facts out of the
intern pool by index, with no refcount and no heap per lookup.

**Before.** `FunctionCx::layout` returned `Arc<Layout>` from a per-function
`RefCell<FxHashMap<Ty, Arc<Layout>>>`: on a hit, one hash and one `Arc` clone, a write to a
refcount that other threads' compiles of the same type share (an atomic on a shared cache line,
concurrency.md). There are 169 `self.layout(` sites, each per place, operand or field, so
several per MIR statement. `variant_shape` (each aggregate of an enum variant, each
`SetDiscriminant`, each downcast) rebuilt `Layout::for_variant` (two `Vec`s cloned) and
recomputed the tag every time.

**After.** Each `build` owns a `KeptLayouts`: two `typed_arena::Arena`s, one of the `Arc`s the
query gave and one of variant layouts. `FunctionCx::layouts` maps `Ty` to `&'cx Layout` lent
from them, so a hit is one hash and no refcount write. This is safe code: the arena drops what
it holds when the build ends, and `mod.rs` has no `unsafe` for it (`docs/v2/plan/design/unsafe.md`).
A second map, `(VariantId, GenericArgs) -> Option<(&Layout, Tag)>`, keeps each variant's layout
and tag once per function. The `for_variant` copy and the tag computation happen once per
variant, not once per use. A single-variant type lends the whole type's layout, not a clone.
Test: `a_kept_layout_is_the_one_the_query_gave` (4096 seeded layouts; each lent reference is
the query's `Layout`, not a copy).

**Missing.** The salsa query `layout_of_ty` is `returns(clone)`, so the miss path still clones
an `Arc` out of salsa: a `returns(ref)` (hir-ty, not this lane) would make it a borrow. The map
is per function; a per-unit or per-thread layout table keyed by interned type would hash once
per unit.

## Environment switches

**Before.** 17 `std::env::var`/`var_os` reads in the builder, several per function or per call
(`BLAZE_DUMP_MIR`, `BLAZE_DUMP_CLIF` in drop glue, `BLAZE_DUMP_TIER` per callee shown,
`BLAZE_CACHE_CHECK` per function and per entry). Each read takes the process environment lock
and allocates the `OsString`.

**After.** `dump_mir_of`, `dumps_clif`, `dumps_tier` and `checks_cache` read once per process
(`LazyLock`), like `mir_opt_level` already did. What remains are v1's per-unit reads
(`BLAZE_STATS`, `BLAZE_LAYOUTS`, `BLAZE_SHAPES`), once per unit, not per function. `tier.rs`'s
three `BLAZE_DUMP_TIER` reads are asked to use `dumps_tier()` (speed-backend-llvm).
`Recorder::new`'s `BLAZE_LAYOUTS` is already read once (`fragment.rs` `LAYOUTS`).

## Strings on per-statement paths

**Before.** Each overflow or division check (`panic_if`, per checked arithmetic statement)
built the panic's name with `format!("panic_const_{name}")` and looked it up with
`core_function`, which walks the def maps and interns each path segment as a `Name` every time.

**After.** The names are `&'static str` written whole, and `core_panic` keeps what each resolved
to in `FunctionCx::core_panics` (a few entries, scanned): one def-map walk per panic kind per
function, no `String`.

**Missing.** Per function rather than per unit; a per-unit table of lang functions would make it
one walk per unit. `BLAZE_LAYOUTS` logging (`format!` of types) is off unless asked.

## Building callees in (`inline`)

**State of the art.** Zig's AIR to MIR lowering and TPDE (Schwarz et al., 2025) emit in one
pass over the IR with no per-node heap allocation; TPDE compiles about an order of magnitude
faster than LLVM's -O0 at similar code quality to it.

**Before.** For each call site built in, `inline` made a new `Dfs` and collected the callee's
instructions in a new `Vec`.

**After.** One `Dfs` and one order `Vec` per `inline`, cleared and reused for every call site
it builds in (`Dfs::pre_order_iter` clears its own state first).

Separately, `Recorder::declare_inlined` (`fragment.rs`, speed-backend-code's) used to clone the
callee's whole IR per call site. It now moves a callee's body once per instance, and every call
site shares that body and its data (one `Location`/`Text` per body, as rustc has).

**Missing.** Building callees in for v2 units (see "Code speed" below).

## `FunctionCx` maps

**Before.** `locals` and `blocks` (`ArenaMap`) grew from empty, reallocating as they filled.

**After.** Both are presized from the body (`locals.len()`, `basic_blocks.len()`): one
allocation each.

**Missing.** The other maps (`registers`, `glue`, `constant_locals`, the pad lists) are still
fresh per function; a per-thread pool of cleared maps, as `spare.rs` does for contexts, needs
them free of `'db` borrows across functions (they hold `Ty<'db>`), so it waits for the
builder's state to be split from its lifetimes.

## Code speed

**State of the art.** Copy-and-patch (Xu and Kjolstad, OOPSLA 2021) compiles by copying
precompiled stencils and patching holes, faster to compile than LLVM -O0 by about two orders of
magnitude while its code beats a plain baseline compiler's; the lesson for blaze's dev
builds is that the code of each MIR construct should be fixed and good, not re-derived.

**Still missing, outside this pass:**

- Building callees in for v2 units (devspeed request 6): v2 refuses `INLINED` names
  (`v2/shape.rs`, `v2/convert.rs`), since a unit's code must be a function of its own MIR; it
  needs the callee's code identity in the unit's key (`v2/refs`, the adapter's).
- std at `opt_level=none` (devspeed request 8): std's functions are compiled at the unit's
  level, so a dev build's std is unoptimized.
- rustc's MIR `Inline` pass (`crates/hir-ty-mir/`, not this lane).
- `integer_slot` gives an integer constant operand a stack slot that `promote` then removes;
  lowering it straight to `iconst` would save the slot and the pass's work on it.

---
status: current
summary: The MIR passes from built MIR to runtime MIR, ported from rustc, that codegen and the interpreter run on.
---
# MIR passes: from built MIR to runtime MIR

blaze's front end gives its MIR the pre-codegen passes rustc runs, so that the code generator
(`crates/hir-ty-codegen`) and the interpreter (`crates/hir-ty-mir/src/mir/eval.rs`) execute a
body as rustc's post-pass MIR says, instead of each re-deriving drop flags, moves and unwinding
on its own.

The code is `crates/hir-ty-mir/src/mir/transform.rs` and `transform/`.

## Phases and queries

`MirBody::phase` is `MirPhase::Built` or `MirPhase::Runtime`.

- **Built** is what lowering produces, and what the checks read (`mir_body`,
  `monomorphized_mir_body`: borrow checking, the const checker, diagnostics). A `Drop` there drops what the place holds *if* it
  holds anything. Calls and drops unwind into the scopes' unwind drop tree, as rustc's MIR
  building makes it. A statement can panic (`CheckedBinaryOp`, an `Index`), and
  `MirBody::panic_unwinds` says where to.
- **Runtime** is rustc's `MirPhase::Runtime(PostCleanup)` minus optimizations:
  - every `Drop` drops;
  - whether a drop runs is decided by the blocks around it, which read drop flags that are
    ordinary `bool` locals;
  - what panics is an `Assert` terminator before it, with its own unwind edge, and the only
    binary operation is the non-panicking `BinaryOp` (with `AddWithOverflow` and the like);
  - cleanup blocks end in `UnwindResume`.

The runtime bodies are separate salsa queries, so the checks never compute them:
`runtime_mir_body`, `runtime_mir_body_for_closure`, `runtime_mir_body_for_coroutine`,
`runtime_mir_body_for_coroutine_by_move`, `runtime_promoted_mir_body` (the constant behind a
promoted `&expr`), and the `monomorphized_runtime_mir_body*` variants
(the passes run on the generic body; monomorphization substitutes after, as it does for built
bodies). Only codegen and the interpreter ask for them.

## What changed in lowering

**Move vs Copy** (`MirBody::mark_moves`, run at the end of every lowering). RA's lowering
wrote `Copy` for every operand that reads a place. Now an operand whose place type is not
`Copy` (modulo regions) is `Move`, as rustc's MIR building does, with no exceptions.

**`[CONSTANT; N]`** repeats the constant operand, as rustc's MIR has it: no temporary is made
and none is dropped. An inline `const { .. }` is lowered in place into a temporary, which the
`Repeat` moves: each element is a copy of its bytes. `Repeat` itself never drops; a value
`[value; 0]` does not take is dropped before it, as rustc's `build_zero_repeat` does.

**Constant indices from the end** count as rustc's do: `ConstantIndex { offset, from_end: true }`
with offset 1 is the last element. RA's lowering used 0.

**Unwind drop trees**, as `rustc_mir_build/src/builder/scope.rs` builds them. Every call, drop
and panicking statement unwinds to the node of the tree for what is in scope where it is:
what every scope holds, last declared first, its value dropped if it needs dropping and, in a
coroutine, its storage ended. Nodes are cleanup blocks keyed by the node unwinding goes on to,
so ways out of sibling scopes share what they have in common and never what they do not
(rustc's cached exits). A drop at a scope's end unwinds through what was declared before the
local it drops. The drop before an assignment (`drop_assigned`) unwinds to a block that still
assigns: rustc's drop-and-replace. Before, one chain of drops for the whole body was built
after lowering.

## The passes, in order

1. **`asserts::add_asserts`**: rustc's checks, as its MIR building makes them, each an
   `Assert` with the unwind edge lowering recorded for the statement:
   - `a + b`, `a - b`, `a * b`: `t = AddWithOverflow(a, b); assert(!t.1); x = t.0`;
   - `a / b`, `a % b`: `assert(b != 0)`, and for signed integers `assert(!(b == -1 && a == MIN))`;
   - `a << b`, `a >> b`: `assert(b as unsigned < BITS)`;
   - `-a`: `assert(a != MIN)`;
   - `base[i]`: `len = Len(base); assert(i < len)`, at the index expression.

   Overflow checks are made where rustc makes them: in a crate that checks overflow, in a
   function with `#[rustc_inherit_overflow_checks]` and in every constant and static. They are
   optional checks, which the code generator and the interpreter skip where the instance does
   not check overflow, as rustc's codegen does. Every `CheckedBinaryOp` becomes a `BinaryOp`.
2. **`upvars::upvars_in_place`**: see Coroutines.
3. **`cleanup::remove_noop_landing_pads`**: rustc's `RemoveNoopLandingPads`. An unwind edge to
   a cleanup block that only ends storage on the way to `UnwindResume` goes.
4. **`elaborate_drops::elaborate_drops`**: a port of rustc's `elaborate_drops` and
   `DropCtxt`.
   - *Move paths* (`move_paths.rs`) are created for:
     - every local that needs drop (promoted constants are never dropped);
     - every assigned or moved place inside it: fields of structs without a destructor, of
       tuples and closures; enum variants (`Downcast`); `Box` contents; constant indices and
       subslices of arrays, by element.
   - *Init analysis*: a forward maybe-init / maybe-uninit dataflow over the move paths.
     - Unwind edges are taken after each terminator's effects.
     - A call initializes its destination only on its normal edge.
     - Blocks in `unreached` are not executed.
   - *Each drop* is one of:
     - **dead** (`Goto`);
     - **static** (left as is);
     - **conditional** (a `SwitchInt` on the path's flag);
     - **open**: the value is taken apart:
       - struct, tuple and closure fields are dropped in order, each unwinding to the drops
         of the rest (rustc's drop ladder);
       - an enum is switched on its `Discriminant` and the variants are dropped field by
         field;
       - a `Box` drops its contents, then `<Box as Drop>::drop`, then its fields;
       - an array drops its elements.
   - *Flags*:
     - one `bool` per path that needs one, all made before any drop is elaborated, in
       rustc's order (`collect_drop_flags`), written right after each statement that
       initializes or moves out of it, so that a statement that panics unwinds with the state
       before it;
     - a call's destination flag is written on its normal edge;
     - flags are cleared after each elaborated drop, on the normal and the unwind edge;
     - flags are set at entry from the parameters.
5. **`packed_drops::add_moves_for_packed_drops`**: a drop of a field that a `#[repr(packed)]`
   struct leaves misaligned is turned into a move to an aligned temporary and a drop of that.

There are no optimization passes: no inlining, GVN, SROA, const propagation or simplification.

## Coroutines

`async` bodies go through the same passes. Lowering gives each `Yield` a `drop` edge to a
path that drops what is in scope there, innermost first, and ends in `CoroutineDrop`, as
rustc's coroutine drop tree does; drop elaboration makes those drops conditional like any
other, and the flags it adds are ordinary `bool` locals.

What the future keeps (`MirBody::locals_held_across_yields`, `mir/held.rs`) is computed on the
runtime body as rustc's `StateTransform` computes it (`rustc_mir_transform/src/coroutine/
layout.rs` at 1.99.0), with ports of the analyses it runs:
- `MaybeLiveLocals`, backwards, a call's destination and a yield's resume argument written
  on their edges, a `Drop` a use;
- `MaybeBorrowedLocals`, where a `Drop` counts as a borrow;
- `MaybeRequiresStorage`, with a local moved out whole and never borrowed before needing no
  storage after;
- `always_storage_live_locals`.

At each reachable yield, in block order, the locals live at the end of the block, plus the
borrowed ones (an `async` block is immovable), intersected with the ones that need storage,
less the reference to the future, are kept there: one variant each. Two kept locals conflict
when both need storage at a statement or terminator of a reachable block that does not end in
`Unreachable`; one that is always storage live (a drop flag, a parameter) conflicts with all.
The layout (`layout.rs`) hands those to `rustc_abi`'s coroutine layout, with only the upvars in
the prefix and each local a `MaybeUninit` of its type, as `rustc_ty_utils` does. The vendored
`rustc_abi` coroutine layout is the same code as rustc 1.99's.

So for the same MIR the layout is rustc's. What lowering and the passes now do as rustc's MIR
building does, because it decides which locals are kept, in which order, in which variant:
- **Declaring bindings.** A binding gets its local where rustc declares it: a `let`'s before
  its initializer is lowered, a match arm's or an `if let`'s where it is bound (rustc declares
  those after the tests, arm by arm). Locals were all allocated before the body before.
- **`await`.** The future `into_future` returns is moved into a local of its own, rustc's
  `__awaitee`, whose storage starts there; the loop borrows that one.
- **Captures in place.** The pass `upvars::upvars_in_place` makes the locals lowering copies
  an `async` body's captures into the captures themselves (`MirBody::upvars_in_place`): they
  hold a value at entry, live in the future's prefix and are never kept for a yield, as rustc
  uses `(_1.i)`. An `async fn`'s parameters are still moved into locals by the `let x = x;`
  statements of its body, as in rustc's HIR.
- **Drop flags.** They are made before any drop is elaborated, in rustc's order: for each drop
  in block order, the paths under the dropped one that may and may not hold a value, depth
  first, the child made last first (`collect_drop_flags`).
- **Yields.** Numbered in block order, which is the order lowering makes them in. rustc numbers
  them in block order after `SimplifyCfg`, which never moves a yield: its block is an arm of
  the switch on the poll result, not the target of a `goto` it could be merged into.

- **Unwind paths.** Lowering builds rustc's unwind drop trees, with `StorageDead` on them in
  coroutines, and checks are `Assert`s with their own unwind edges, so what is in scope in one
  scope is never seen with what is in scope in another.
- **Temporaries.** A temporary is made before what is lowered into it, as rustc's `as_temp`
  does, and `into_future`'s destination before the awaited expression's temporaries. A
  `let` that binds one name by value has its initializer lowered into the binding, as
  rustc's `expr_into_pattern` does, not into a temporary copied after: `let x = f.await`
  writes `x` before the awaitee is dropped, so `x` conflicts with it as in rustc.
- **Comparisons of scalars** compare the values (`Eq(copy i, const 0)`): rustc's typeck drops
  the autoref of `PartialEq::eq(&a, &b)` for them (writeback's `fix_scalar_builtin_expr`).
  RA kept it and compared references, and a borrowed local is live to the end of its storage
  in an `async` block, so a loop variable compared once was kept across every await.
- **Promotion.** The promoted locals rustc promotes (nothing to drop, no cell) are never kept
  for a yield (`MirBody::promoted_constants`); rustc's `PromoteTemps` makes them constants.

rustc's other passes before the coroutine transform change nothing that is kept: its
post-borrowck cleanup removes fake reads (ignored here) and fake borrows (RA makes none),
`SimplifyCfg` and `Derefer` do not change which locals live across a yield, and
`RemoveStorageMarkers` runs only in the optimizations, after it.

What may still differ: where a temporary's scope ends in an expression RA lowers otherwise
than rustc (edition-dependent tail and condition temporaries, `for`, `?`), an array's
bounds check reading its length into a temporary where rustc uses the constant, and
`AbortUnwindingCalls`, which drops the unwind edges of calls that cannot unwind. These change
what a future keeps only where such a temporary lives across a yield, or a no-unwind call
changes which unwind paths there are.

## What the consumers do with it

**blaze codegen** (`crates/hir-ty-codegen/mod.rs`) generates a Runtime body directly, and
generates nothing else:
- no drop flags of its own, in `async` bodies too;
- `Drop` is `drop_in_place` and a jump;
- each cleanup block a call unwinds to gets its own landing pad, which keeps the exception
  and jumps to the block;
- `UnwindResume` resumes with the kept exception;
- `Assert` branches to rustc's `panic_const_*` or `panic_bounds_check`, through the landing pad
  of its unwind edge, and skips an optional overflow check where overflow is not checked; no
  place it reads checks an index, and no operation panics on its own (the bounds checks in
  `element`, the overflow and division checks in `binary_op`, the index spans and the
  `FakeRead` of an indexed place are deleted);
- an `async` body that unwinds stores the panicked state before resuming;
- dropping a suspended future enters the drop path of the yield it is suspended at; one that
  never started drops its upvars.

The flag-following machinery that tried to decide drops statically was deleted
(`FlagEvent`, `Held`, `FlagFlow`, `follow_flags`, `forget_unread_flags`; 529 lines), and then
the rest of the drop handling for built bodies: drop flags and partial-move bits, deferred
drops, the landing pad that dropped live locals, flags in the future's prefix.

**The interpreter** runs Runtime bodies only, promoted constants included: a `Drop` drops. Its
place-set drop tracking (`DropFlags`) is deleted, and a body that is not elaborated (an `async`
body, which it can't run anyway) is reported as not supported. A failed `Assert` is a panic
with the words its own checks used (`Overflow in Add`, `index out of bounds: ..`).

## Optimizations

The code is `crates/hir-ty-mir/src/mir/optimize.rs` and `optimize/`: rustc's
`run_optimization_passes`, ported pass by pass in its order, on top of the runtime
body: `optimized_mir_body(def, level)`, `optimized_mir_body_for_closure` and their
`monomorphized_optimized_*` variants, again separate salsa queries so nothing else computes
them. `MirOptLevel` is rustc's `-Zmir-opt-level`: `Off` is the runtime body unchanged,
`Debug` is level 1 (what `-Copt-level=0` gets), `Release` level 2 (above `-Copt-level=0`).
The code generator takes rustc's level for each crate from the unit it compiles
(`Build::options` in `crates/hir-ty-codegen/v2/build.rs`: `Debug` at `-Copt-level=0`,
`Release` above it and for the library's crates, inlined where the unit inlines), with UB
checks (`CheckAlignment`, `CheckNull`, `CheckEnums`) where rustc's `sess.ub_checks()` has them.
`BLAZE_MIR_OPT` is not an override in v2: set, it stops a unit's build, since a unit's key has
no part of it (`CHANGES_CODE` in `crates/hir-ty-codegen/v2/backend.rs`). The interpreter takes
the level from `set_interpreter_mir_opt_level`.

- Level 1: `CleanupPostBorrowck` (fake reads, false edges, `CopyForDeref`; also the storage
  markers lowering gives parameters), `LowerSliceLenCalls`, `InstSimplify` (bool comparisons,
  `&*` of a reference, casts to the same type, `[x; 1]`, `Clone::clone` of a primitive,
  duplicate switch targets), `RemoveStorageMarkers`, `RemoveUnneededDrops`, `SimplifyCfg`,
  `SimplifyConstCondition`, `SimplifyLocals`, `SingleUseConsts`, `RemoveNoopLandingPads`,
  `CopyProp`, `CriticalCallEdges`.
- Level 2 adds `Inline` (`optimize/inline.rs`, where the unit inlines), `UnreachablePropagation`
  and both `DeadStoreElimination`s.
- Storage markers are kept at every level: the code generator gives its backend lifetimes, and
  rustc runs `RemoveStorageMarkers` only where it gives none.
- Not ported: `passes_not_ported(level)` lists them. At level 1 `ForceInline`, `RemoveZsts`
  and `UnreachableEnumBranching`; at level 2 also `GVN`, `ReferencePropagation`,
  `ScalarReplacementOfAggregates`, `JumpThreading`, `DestinationPropagation` and a few smaller
  ones.

Where a port differs from rustc:

- Coroutine bodies are not optimized, only given the UB checks: `held.rs` computes a future's
  layout from the runtime body, and the layout has to stay what rustc's is.
- Promoted locals, upvars, parameters, the return slot and every local a binding's debug info
  names are kept, as nothing here rewrites debug info the way rustc's `SimplifyLocals` does.
- Dead-store elimination ends a local's life at a write only, not at its `StorageDead`. For MIR
  rustc builds that's the same; for MIR where lowering put a `StorageDead` before a read it
  keeps the store.
- In a debug build of the fork every pass is followed by a check of the body (locals, blocks
  and terminators in range), which names the pass that broke it.

The interpreter's tests run every program at all three levels and require the same output
(`hir-ty-mir/src/tests/mir/eval.rs`, `tests/mir/optimize.rs`).

## Not done yet

- **Abort on unwind.** A drop in a cleanup block that unwinds should abort (rustc's
  `UnwindAction::Terminate`); it continues unwinding today, which is what codegen did before.
  `nounwind` callees do not yet lose their unwind edges (`AbortUnwindingCalls`).
- **Subslices of arrays.** `Subslice { from, to }` always counts `to` from the end; rustc's
  has `from_end` and counts from the start for arrays.

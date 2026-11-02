---
status: plan
summary: frontend-fast-mir — MIR building's cost per HIR node against its bound, counted from hir-ty-mir's lower.rs and lower/; the quadratic unwind and scope-exit paths replaced by rustc's cached unwind block per scope (lower/unwind.rs), the match tree's quadratic candidate handling, and the differential and allocation tests that prove the MIR is the same.
date: 2026-10-29
---
# frontend-fast-mir: MIR building at its bound

Area: section 7 of [fast.md](fast.md). Machine figures: the top of
[../db/speed-of-light.md](../db/speed-of-light.md) (3.5 GHz, L1 4 to 5 cycles, L2 14, branchy
code at 2 instructions per cycle). Costs are counted from the code as of 2026-10-29 21:00, not
measured: nothing builds yet.

What this builds on, not redone here:
- lane codegen's count of the per-expression work
  ([../codegen/speed-of-light.md](../codegen/speed-of-light.md), "3. MIR building"):
  the bound of about 30 cycles an expression, and the two gaps it requested of hir-ty (a
  path's value-namespace resolution and adjustments as columns from inference);
- lane mem's request [v2/requests/mem-codegen-mir.md](../../../../../v2/requests/mem-codegen-mir.md)
  §5, which moves every builder container into the task's frame and copies the body out once,
  at its exact size. `codegen-mir-store-lower` is applying it to `lower.rs` and `lower/`. With
  it, lowering's own allocations are a constant per body (§10.3's test), so the "per-node `Vec`
  and `Box`" candidate is that change, not this one.

Types are now hash-consed `u32` rows, so `Ty::store()` (codegen's "types interned again", 50
to 100 cycles) is an index copy (`next_solver/interner.rs` `impl_stored_interned!`): that gap
is closed.

## 1. The work and its bound

Per HIR expression, about 1.5 statements and 0.7 locals come out (codegen's count). The minimum
is one dispatch on the expression, a type and adjustment fetch, building and pushing the
statements and the local: about 50 instructions, **30 cycles an expression**.

The parts of that work this note owns, each with its own bound:

| Part | Work it must do | Bound |
|---|---|---|
| Places and projections | push a projection element onto a place being built | 2 to 4 cycles an element |
| Temporaries | allocate a local, `StorageLive`, schedule it in a scope | about 10 cycles |
| Unwind path of a statement that may panic, or a call | name the cleanup block for what is in scope | O(1): the innermost scope's cached block, about 5 cycles, plus one drop-tree node (one hash probe, 25 cycles) for each drop scheduled since the last ask |
| Leaving a scope (its end, `break`, `return`, `?`) | one `Drop` and one `StorageDead` per local it leaves, and each drop's unwind | O(locals left): about 40 cycles a dropped local (the block, the node probe) |
| Operands and constants | build the operand, intern a constant's bytes | 10 to 20 cycles; the constant interning is D1 of mem's §11 |
| The match decision tree (`match_tree.rs`) | rustc's candidate sorting over the arms | O(pattern nodes + arms × tests on the arm's way) |
| Allocations | the kept body, once, at its size | a constant number of buffers per body |

## 2. What the code does today, counted

### 2.1 The unwind path of every statement that may panic (the dominant gap)

`push_statement` (`lower.rs`) calls `unwind_here` for every statement whose
`StatementKind::may_panic()` holds (checked arithmetic, an index projection), and
`lower_call` and `drop_assigned` call it for every call and every drop-and-replace. Each call:

- `leaving_order` for **every scope on the stack**: `in_order` (a `Cow`, owned with three
  `Vec`s when the scope has a pre-2024 tail), then a `partition` into two new `Vec`s with an
  `operand_locals` hash probe per local, then a third `collect` of all of them;
- `unwind_through` over **every local in scope**: an `is_copy` probe of the `copy` map (keyed
  by `Ty`), an `unscheduled` probe, and an `unwind_nodes` probe keyed by
  `(BasicBlockId, LocalId, bool)`, in a coroutine twice.

Per local in scope: about 20 (operands) + 20 (copy) + 20 (unscheduled) + 25 (node) = **about
85 cycles**, plus 3 or more `Vec` allocations per call (§5.3 of mem's request removes the
allocations, not the walk). With L locals in scope that is about 85 L cycles a site. A typical
function has L = 10 to 30 at its calls and one such site per 3 to 5 expressions: **about 200
to 800 cycles an expression, 7 to 25 times the 30-cycle bound**, and quadratic in body length:
a function of 200 `let`s has L near 200 at its last call. This alone is larger than everything
codegen counted for the rest of lowering.

rustc does the same work once per drop: each scope caches its unwind block
(`rustc_mir_build/src/builder/scope.rs`, `Scope::cached_unwind_block`, invalidated by
`schedule_drop` for the scope it schedules into and the ones inside it), and
`diverge_cleanup_target` walks out only to the innermost scope with a cached block.

### 2.2 Leaving a scope

- `emit_drop_and_storage_dead_for_scope`, for each droppable local of the scope it leaves,
  calls `unwind_from_drop`, which builds `in_scope()` (every local of every scope, `in_order`
  per scope), a linear `position` in it, and `unwind_through` over the whole prefix: **about
  90 L cycles a dropped local**, so k × 90 L for a scope of k droppable locals.
- `exit_to` (`break`, `continue`, `return`, `?`, a future dropped at an `await`), for each
  droppable local whose exit node is new, builds `before` (every outer scope's
  `leaving_order`) and runs `unwind_through` over it: again about 90 L a dropped local.
- `drop_until_scope` clones each `DropScope` it leaves (two `Vec`s); mem §5.4 removes the
  clone.

### 2.3 Scheduling a local

- `push_storage_live_for_local`: `scope.locals.contains(&l)`, a linear scan of the scope, for
  every temporary and binding: a block of n `let`s scans n² / 2 entries (vectorized, about
  0.25 cycle each: 1250 cycles a `let` at n = 10 000). A temporary just allocated by `temp` is
  never in any scope, so the scan is always a miss for it.
- `as_place.rs` `declare_last`: `find` over scopes with `contains`, then a `retain`: O(size of
  the scope holding it), usually the innermost, small.

### 2.4 Places, projections and place types

Places are built with `Place::project` onto an interned projection list; lowering reads a
place's type in one site (`pattern_matching.rs` `place_ty`, which mem §5.1 turns into a fold
over the projection from the local's type). There is no recomputation of place types in
lowering: that gap is borrowck's ([../db/speed-of-light-borrowck.md](../db/speed-of-light-borrowck.md),
owned by `frontend-fast-borrowck`).

### 2.5 The match tree (`lower/match_tree.rs`)

`reached_arms` runs rustc's candidate sorting only to say which arms rustc reaches (the MIR
itself tests arms in order, in `pattern_matching.rs`). Per `match`:
- `Tree::run` takes the first candidate with `candidates.remove(0)`: O(arms) a candidate,
  O(arms²) over the match;
- `Tree::test` finds a candidate's branch by a linear `find` over the branches: O(arms ×
  branches), O(arms²) for a `match` on an integer or a `char` with an arm per value;
- `sort`'s `Ask::Int` case scans every candidate already sorted into `Failure` for a range
  covering the value: O(arms²) when ranges and values mix;
- every `Pair` owns its place as a `Vec<Step>`, cloned per field (`place.clone()`,
  `to_vec()`), and `matched` copies `then` (`to_vec`). mem §5.5 puts these in the frame.

At 5 to 20 arms these are a few hundred cycles per `match`, under 10% of the arms' own
lowering; at 256 arms (a lexer's `match` on a byte) they are about 65 000 compares, several
times the lowering of the arms. rustc's `sort_candidates` keys its switch targets in an
`FxIndexMap` and splits slices of candidates instead of removing from a `Vec`.

### 2.6 Summary against the bound

| Part | Today | Bound | Off |
|---|---|---|---|
| Unwind path, per site | 85 L + allocations | 5 + 25 per new drop | 7 to 25× an expression at typical L, unbounded with L |
| Scope end, per dropped local | 90 L | 40 | 2 to 50× |
| `exit_to`, per new exit node | 90 L | 40 | 2 to 50× |
| Scheduling a fresh temporary | O(scope) scan | O(1) | quadratic in a long block |
| Match tree | O(arms²) | O(arms × tests) | at most 10% under 20 arms, many times above 100 |
| Per-expression rest | codegen's 200 to 400 (path resolution again, adjustment probes) | 30 | codegen's request 12 to hir-ty |
| Allocations | per node | constant per body | mem §5 (being applied) |

## 3. The design

### 3.1 The unwind chain per scope (`lower/unwind.rs`, new)

The unwind path from a point is a fold over the locals in scope, outermost first, each step
`node(at, local, kind)` memoized in `unwind_nodes`. Because the memo is append-only and keyed
by the step's inputs, the fold over a prefix that has not changed returns the block it returned
before, and every probe in it is a hit. So each scope keeps the block its fold ended at (its
*link*), and only the scopes that changed since are folded again, from the outermost changed
one inward: rustc's `cached_unwind_block`.

What a scope's link depends on, and how each change marks it stale:
- its locals and its pre-2024 tail mark: these move into `ScopeLocals`, a type of
  `lower/unwind.rs` with private fields. Reads go through `Deref<Target = [LocalId]>`; every
  write goes through `ScopeLocals::edit()`, which marks the scope stale. The compiler holds every
  write site to it: `lower.rs` cannot reach the `Vec` without `edit()`.
- the `operand_locals` and `unscheduled` membership of a local in it: both become
  `LocalSet`s (a bit set over local indices in the frame) whose writes go through
  `MirLowerCtx::mark_operand` / `unschedule` / `reschedule`. Each marks stale the outermost scope
  that held the local when the links were last built (`ScopeOf`, a per-local column of
  (scope depth, scope stamp)); see the invariant below.
- pushing or popping a scope: the links are truncated to the stack.
- whether storage ends on unwinding (a coroutine): kept with the links; a change refolds all.

The invariant that makes `ScopeOf` exact: for every clean scope S holding local l, the
recorded entry for l is valid (the scope at its depth still has its stamp) and its depth is at
most S's. A refold of S writes l's entry when the entry is invalid or deeper than S, and scopes
are folded outermost first; an entry goes invalid only when its scope is popped, and a stack pops
every scope inside it first. A stale entry (the scope since lost l) only makes one more scope
refold, which costs time, never changes a block.

Two chains are kept, because the code folds two orders: `leaving_order` (operands last, for
`unwind_here` and `exit_to`) and `in_order` (for the drops at a scope's end). Each has its own
links and its own stale marks, and each is folded lazily, only when an unwind path is asked
of it, so blocks are allocated at the moment and in the order the old code allocated them.

Uses:
- `unwind_here`: refold the leaving chain from its first stale scope; the answer is the last
  link. O(1) when nothing changed, else O(locals of the changed scopes).
- the drops at a scope's end (`emit_drop_and_storage_dead_for_scope`): the base is the in-order
  chain's last link; the prefix folds over the dropped scope's own locals are made once, in
  ascending order, at the first drop that needs them, which is when and in what order the old
  per-drop folds first made those nodes. A local found in a scope still on the stack (the
  scopes `drop_until_scope` and `guard_failed` drop) is found through its `ScopeOf` entry and
  its position in that scope's folded order.
- `exit_to`: the fold over `before` is the leaving chain's link of the scope before, then a
  cursor over this scope's order that only moves forward, as the old code's `position`s do.

Cost: about 5 cycles per site when nothing changed; one probe per local of a changed scope,
as in rustc. Usually the changed scope is the statement's temporaries' scope, one to three
locals.

### 3.2 Scheduling without the scan

Each `ScopeLocals` keeps one more than the largest local index it holds (`bound`, recounted
lazily after an `edit`). Locals are numbered in the order they are made, so a fresh temporary
or a first declaration is at or above it and `holds` answers without the scan: O(1) for those,
the old scan only for a re-declared binding (a loop's body, or-pattern expansions).
`declare_last`, `guard_held` and `lower_guard` ask `holds` first and leave a scope untouched
(and its unwind fold clean) when nothing moves.

### 3.3 The match tree

- Candidates as a stack, the first one last (`Stack`): taking the first is a pop, and what a
  test leaves stays where it is instead of being copied into a new list each time (an arm per
  string constant made the old run quadratic). The lowest index of a candidate that starts
  with an or-pattern is found once per run: candidates below the top are not changed while
  they wait, so "any left" is a compare with the length.
- `Branches`: a branch's index by a scan for at most 8 branches and through a frame `FrameMap`
  above that, as rustc's `FxIndexMap`, keeping the first-seen order `run` visits them in; the
  `Int` branch values kept sorted, and the ranges of the `Failure` candidates of an `Int` test
  kept merged, so both of `sort`'s range checks are a binary search instead of a scan of every
  branch's candidates.

`lower.rs` and `lower/` were handed to this lane by codegen-mir-store-lower once mem §5 was in.

## 4. What rustc behavior is kept, and how

The MIR must be the same body, statement for statement; no renumbering is proposed, so borrowck
diagnostics that name locals are unaffected. What could differ, and why it doesn't:
- **Block numbering.** Blocks are allocated in the order the old code allocates them: a link
  is folded only when the old code would fold that prefix, and a fold over a clean prefix
  allocates nothing in either.
- **The `copy` cache and the inference context.** `type_is_copy_modulo_regions` is asked for a
  type the first time a fold reaches a local of that type, as before (a clean prefix was asked
  when it was folded), so the inference context sees the same queries in the same order, and
  the `copy` map `mark_moves` receives holds the same entries.
- **Drop order, `StorageDead` placement, spans**: unchanged code paths; only which probes run
  changes.

## 5. Proof

- `lower/unwind.rs` tests: a model of the scope stack (push, pop, schedule, move to the end,
  tail, outlive, operand, unschedule) driven by generated operation sequences (nested scopes,
  loops with breaks, drops in every scope shape), with the old fold copied into the test
  module, asserting the same answer and the same sequence of node allocations at every ask.
  Probe-count tests: asking twice with nothing changed probes nothing; 2000 `let`s with a call
  after each probe at most once a local (the old fold: n² / 2).
- Wiring: every write to a scope's list goes through `ScopeLocals::edit`, `push_new`,
  `set_tail` or `take_list` (its fields are private to `lower/unwind.rs`, so the compiler holds
  every write site), and `operand_locals` and `unscheduled` are written only through
  `mark_operand`, `unschedule` and `reschedule`. `Unwinds::before` checks in debug builds that
  the order `exit_to` drops in is the order it folded.
- `lower/match_tree/tests.rs`: the old run copied into the test module against the new on 3000
  generated matches (integers, overlapping ranges, `bool`s, constants, slice lengths with
  elements, nested or-patterns, guards; `Variant` needs a database for its ids and is left out),
  plus a 256-value byte match with ranges past the 8-branch index and 2000 constant arms: the
  same arms reached and the same answer on falling through.
- Why no whole-builder copy: the only algorithms replaced are the unwind fold and the match
  tree's run, each copied whole into its test module and compared on generated inputs; every
  other part of the builder is the same code before and after, so copying all of `lower.rs`
  (5000 lines) into a test would compare a function with itself.
- `harness/v2/frontend/cases/mir/` (53 programs, 48 accepted, 5 rejected; `053` is `become`, lowered in `lower/tail_call.rs` as
  rustc's `break_for_tail_call`): run output shows drop
  order on unwinding out of calls, struct literals, method chains, indexing, division and drops
  that panic during assignment; on `break`/`continue`/labeled breaks/`return`/`?`; in guards,
  `let else`, `if let`, `while let` and `for`; temporaries in tails and `if let` scrutinees in
  2021 and 2024; async bodies dropped at an await and panicking after one; a 26-`let` block; and
  match trees on bytes, strings, overlapping ranges, slices and nested or-patterns; calls of an
  empty enum (§8). Rejections: E0716, E0597 (a scope, a 2021 tail temporary, drop check) and
  E0505.
- Allocation budget: building a body allocates the kept body's buffers and a constant number
  more (mem §10.3's lowering test, equal at 8 and 64 statements); the chain's buffers are in the
  frame.

## 6. State of the art

- **rustc** (`rustc_mir_build`): the drop tree with cached unwind blocks per scope, built once
  per scheduled drop; candidate sorting over slices with indexed switch targets. This design is
  that, over dense IDs and frame buffers.
- **Cranelift's frontend** (`cranelift_frontend::FunctionBuilder`): SSA construction on the fly
  with per-block variable maps, no separate drop schedule (it has no destructors); blocks and
  instructions in flat `PrimaryMap`s. Lowering's flat block and local arrays in the frame (mem
  §5.1) are this shape.
- **Zig's AIR** (`Sema` writing `Air` instructions): one pass, instructions as a tag array and
  a payload array, `defer`/`errdefer` lowered by copying the deferred body at each exit, so
  its exits are O(deferred bodies). rustc's shared drop tree, which this keeps, shares exit
  drops between exits instead, which MIR's identity with rustc requires.

## 7. Where each part stands (2026-10-29, uncompiled)

| Part | State against its bound |
|---|---|
| Unwind path per site | `Unwinds`: about 5 cycles when nothing changed, one probe a local of a changed scope (rustc's cached unwind block, and a `let` is one step where rustc refolds the scope) |
| Scope end and `exit_to`, per dropped local | the cached fold's position, O(1) when the prefix is clean |
| Scheduling a temporary | O(1) through `ScopeLocals::holds` |
| Match tree | O(pattern nodes + arms × tests): pops, indexed branches, binary-searched ranges |
| Allocations | the kept body and a constant (codegen-mir-store's `lowering_allocates_the_kept_body_and_its_setup_only`, equal at 8 and 64 statements); every buffer here is in the frame |
| Per-expression rest (path resolution again, adjustment probes) | codegen's request 12 to hir-ty, not this lane's |
| `unreachable_after_calls` | zero extra work without a call of an uninhabited non-`!` type; one probe lowering with one (§8) |

## 8. `MirBody::unreachable_after_calls` (`lower/uninhabited.rs`)

db-ty-d-mir's fill (`v2/requests/db-ty.md`, "db-ty-d-mir", 1), from rustc's
`compiler/rustc_mir_transform/src/lint_and_remove_uninhabited.rs` as read on rust-lang/rust
master (2026-10-29). rustc builds what follows a call into the call's target block, then the
pass cuts the edge of every call of an uninhabited type and, for one whose type is not `!` in a
`fn` or associated `fn` whose return type is inhabited, walks from the target to the first user
code (`find_unreachable_code_from`: past `()` constants, return-place assignments and storage
markers; a `FakeRead` is "definition", anything else "expression"; through a `Goto` or `Drop`
into a block only it leads to; nothing at a `Return`). Lowering here builds nothing after such
a call, so:

- the kept body is built as before; `lower_call` only notes that it gave a call of an
  uninhabited type other than `!` no target (`uninhabited_call`);
- when it did, in a function whose return type is inhabited, the body is lowered a second time
  in the same frame with `probe` set: each call of an uninhabited type keeps a target block
  nothing leads to and lowering goes on in it, and `push_assign_unit`'s statements are noted
  (rustc writes those as a `()` constant it skips; blaze writes an empty aggregate, which a `()`
  the user wrote also is). The walk runs over the probe's blocks, the calls in block order; only
  the entries are kept. The kept body, its block and local numbering, and so borrowck and
  codegen, are unchanged.

Cost: nothing in a body without such a call (a flag, and a `None` check per `()` assignment);
one more lowering of a body with one, which is rare (calls of an empty enum or `Infallible`
type in a function that returns).

The request's examples had `descr` the wrong way round. rustc's own
`tests/ui/lint/dead-code/issue-85071.rs` gives "unreachable definition" at `x` for
`let x = f();`, because the `FakeRead` of the `let` follows the call. The tests follow rustc.

What it does not build: like the kept body, the probe builds nothing after a `return`, a
`break`, a `continue` or a `!` expression other than a call, where rustc builds the rest into a
block nothing reaches. So a call of an uninhabited type that only follows one of those gives no
entry, where rustc lints it. Type checking already warns "unreachable statement" there.

Proof: `lower/uninhabited/tests.rs` runs the walk against rustc's recursive one (copied there,
with a predecessor list per edge) on hand-made blocks and 2000 generated block graphs (storage
markers, `Goto` chains, blocks reached by more than one edge, imaginary and unwind edges,
`Return`, cycles, unterminated blocks). `tests/mir/uninhabited.rs` covers rustc's two UI tests,
a call after a statement, the order across two branches, none after a `!` call, in a function
returning `Void`, where only the return value follows, or in a closure, and checks that the kept
body still ends at the call. Harness cases `mir/051` and `mir/052`: run output across such a
call, and no borrow check of the code after one.

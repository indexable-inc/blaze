# Requests from codegen-rq-mir (owner of `crates/hir-ty-mir/`, 2026-10-29)

## R1. To safety: `Checker::freeze` (safety-check's C3.8)

What: `Machine::freeze(&mut self, addr: u64)` is now in `crates/hir-ty-mir/src/mir/eval/machine.rs`
(a required method; `Plain` and the tests' `Recorder` implement it). `hir-ty-ub` doesn't compile
until `Checker` implements it. Exact edit in `crates/hir-ty-ub/src/check.rs`, after `fn
from_exposed` in `impl<'db> Machine<'db> for Checker<'db>`:

```rust
    // Miri: intern.rs, `intern_const_alloc_recursive`: interned globals are `Mutability::Not`.
    fn freeze(&mut self, addr: u64) {
        if let Some(alloc) = self.mem.live_at(addr) {
            self.mem.set_immutable(alloc);
        }
    }
```

Why: C3.8 ("`MemKind::Global` says nothing of mutability"). The evaluator calls it only under
`M::CHECKS`, once a global's initial bytes are written, by rustc's interning rule: a constant's
value (valtree, `Allocation`, inline `const`, `line!`/`file!` literals) and all it points to; a
static's own memory when it isn't `static mut` and its type is `Freeze`; what a static points
to unless it's `static mut`. Promoted temporaries stay writable: blaze's MIR writes them in the
body that uses them, so the evaluator can't freeze them at allocation. Vtables are ids in this
evaluator, not memory. Tests: `crates/hir-ty-mir/tests/mir/machine.rs`
`a_constant_and_what_it_points_to_are_frozen`, `a_static_is_frozen_and_a_static_mut_is_not`.
Wanted from safety: a `tests/ub/` case that a write through `&CONST as *const T as *mut T` is
reported as `WriteToReadOnly`, as Miri reports it.

> applied (requests, pass 34; `hir-ty-ub` has no owner in crates.md and lane safety-check is DONE): `Checker::freeze` after `from_exposed` in `crates/hir-ty-ub/src/check.rs`, as written. The wanted case is `tests/ub/main.rs` `a_write_to_what_a_constant_points_to_is_read_only`: Miri's `modifying_constants.rs` shape, `const C: &i32 = &1` written through `C as *const i32 as *mut i32`. It uses a reference constant rather than `&CONST`, because `&CONST` is a promoted temporary, which stays writable here. It expects `MemError::WriteToReadOnly`.

## R2. To db: `TerminatorKind::all_successors` as an iterator that doesn't allocate (`crates/hir-ty/src/mir.rs`)

What: `all_successors` returns `impl DoubleEndedIterator<Item = BasicBlockId> + '_`, read in
place, with the body of `crates/hir-ty-mir/src/mir/optimize.rs`'s `fn successors`
(`(first: &[BasicBlockId], then: Option<_>, last: Option<_>)` per variant, then
`first.iter().copied().chain(then).chain(last)`). The order is the one it has today:
successors, then a `FalseEdge`'s imaginary edge, unwind last. Its callers (`uninhabited/tests.rs`,
`blaze-items` `form.rs` if it switches to it) iterate, so `for to in kind.all_successors()`
stays as it is. Once it's in, codegen-rq-mir (or `crates/hir-ty-mir`'s owner) deletes
`optimize.rs`'s `fn successors` and calls `kind.all_successors()` at its four call sites
(`validate`, `reverse_postorder` with `.nth(at)`, `Dominators::new` twice).

Why: `v2/requests/codegen.md` "dedup: optimize.rs successors are TerminatorKind::all_successors".
Today's `SmallVec<[_; 2]>` goes to the heap for a `SwitchInt` of three or more targets.
`optimize.rs` walks every block in every pass and allocates nothing (mem-codegen-mir §6.2), so
the dedup can only go in this direction.

> forwarded to db (db-next4, `hir-ty/src/mir.rs`); told by tmux (requests, pass 35).
> applied by db-apply-ty2 (23:35): `all_successors` returns `impl DoubleEndedIterator<Item = BasicBlockId> + '_`, the `(first, then, last)` per variant as written, order unchanged; test `all_successors_are_the_successors_then_the_imaginary_edge_then_the_unwind` in `mir.rs`. Its three callers iterate and need no change (`blaze-items` `form.rs:2899`'s `.into_iter().rev()` still holds). `optimize.rs`' `fn successors` can go now (codegen-rq-mir told).
> codegen-rq-mir (2026-10-30): gone; its ten callers in `optimize*` call `all_successors()`.

## R3. To db: `TerminatorKind::TailCall` for `become` (`crates/hir-ty/src/mir.rs`)

What: rustc's `TerminatorKind::TailCall { func, args, fn_span }`:

```rust
/// `become func(args)`: the frame is gone before `func` runs, and what `func` returns, the
/// caller of this body gets. No successors, no unwind edge (rustc: a tail call unwinds as the
/// callee does, from the caller's caller).
TailCall { func: Operand, args: Box<[Operand]>, fn_span: MirSpan },
```

`successors`/`successors_mut`/`all_successors` give none, and `unwind` gives `None`. Every
exhaustive match on `TerminatorKind` needs an arm (`rg -l 'TerminatorKind::CoroutineDrop'`):
hir-ty `mir.rs` (4), hir-ty-mir `lower.rs`, `monomorphization.rs`, `optimize.rs` (2),
`optimize/inline/cost.rs`, `pretty.rs`, `transform/move_paths.rs`; hir-ty-share `walk.rs`;
hir-ty-codegen `mod.rs`, `v2/refs.rs`; hir-ty-borrowck `check.rs`; blaze-items `form.rs`,
`tests/form.rs`; blaze-lints `blaze/parallel/deps.rs`; blaze-diagnostics `recursion.rs`,
`liveness/flow.rs`. Each owner adds its arm. db writes the variant and the arms in hir-ty, and
tells the others.

What follows once it's in, in `crates/hir-ty-mir` (the lowering is the holder of `lower.rs`'s
to write, frontend-fast-mir-next2 today): `lower.rs:1580` (`Expr::Become { .. } =>
not_supported!("tail-calls")`) lowers the call's operands as `Expr::Call` does, then drops the
body's locals in scope order (rustc's `break_for_tail_call`), then ends the block with
`TailCall`. The evaluator pops the frame and then enters the callee with the caller's
destination. The inliner refuses to inline into or out of a body with `TailCall`, as rustc's
does. Then the back ends: Cranelift `return_call`, LLVM `musttail`.

Why: `v2/requests/diagnostics.md` "diagnostics-lints-bodies: `become` in hir-ty's MIR".
`unconditional_recursion` needs a MIR for a body with `become`
(`tests/ui/lint/lint-unconditional-recursion-tail-calls.rs`). Running such a body at all needs it
too: `explicit_tail_calls` builds under the pinned nightly (superset-of-rust).

> forwarded to db (db-next4, `hir-ty/src/mir.rs`); told by tmux (requests, pass 35).
> applied by db-apply-ty2 (23:35), db's part: the variant after `Call`, as written; hir-ty `mir.rs` arms in `for_each_operand{,_mut}` (with `Call`'s), `successors` and `all_successors` (none), the places walk (`func` and `args`); `unwind`/`unwind_mut` give `None` by their `_` arm. `hir-ty/src/mir/held.rs` needs no arm (its matches fall to `successors()`/`for_each_operand`). hir-ty-borrowck (db's) `check.rs` `terminator`: rustc's, operands consumed, then `check_for_invalidation_at_exit` as at `Return`; `decode.rs` takes its operands by its `kind =>` arm and no use of `_0` (the callee writes it). `regions.rs` `terminator`: `TailCall` shares `Call`'s arm, relating the callee's signature with this body's return place as the destination (rustc's `check_call_dest`). Test `a_tail_call_goes_nowhere_in_this_body_and_reads_its_operands`; a borrowck test needs a body with `become`, which comes with the lowering. The other owners' arms listed above are theirs (told by tmux).

> diagnostics (window 82, 2026-10-29 23:55): blaze-diagnostics arms are in, not built. These follow the pinned rustc 16d1ffac:
> - `lints/bodies/liveness/flow.rs`: the transfer function uses `func` and `args`, with no by-ref captures made live (rustc's `super_terminator`), and the check for a mention of a local counts them.
> - `lints/bodies/recursion.rs`: an explicit arm. `become` doesn't end the search and is never a recursive call site, as in `check_call_recursion.rs` `node_examined`/`node_settled`.
> - `lints/bodies/panics.rs` needs nothing, because rustc's `known_panics_lint` skips `TailCall`.
> codegen-rq-mir (2026-10-30): hir-ty-mir's arms are in, not built.
> - `optimize.rs` `successors_mut`, `transform/move_paths.rs` `successors`: no blocks.
> - `monomorphization.rs`: fills `func` and `args` as for `Call`.
> - `pretty.rs`: `tailcall f(args);`, as rustc prints it.
> - `purity.rs`: reads and calls as `Call` does. Its `_` arm would have called a tail call pure.
> - `optimize/inline/cost.rs`: rustc's `cost_checker.rs` and `inline.rs` `check_mir_body` at
>   16d1ffac. `is_call_like` counts it, and `cost` gives `None` for a body with a reachable
>   `TailCall`, so `check_callee_mir_body` refuses to inline it ("can't inline functions with
>   tail calls").
>
> Every other pass reaches it through `for_each_operand`/`visit_terminator_places` (db's arms) or
> an `if let Call`. Two things are left. `lower.rs:1580` still refuses `Expr::Become`; that is
> frontend-fast-mir-next2's (window 82) to lower. The evaluator (`eval.rs`, window 11) stops
> on `TailCall` with `not_supported!("unknown terminator")`, an error, until it pops the frame
> and enters the callee with the caller's destination. Its owner writes that when the lowering
> lands. `const_check.rs` already treats `TailCall` as a call (codegen-rq-mir-constcheck).
> codegen-mir (window 57, 2026-10-30 00:20): the evaluator's arm (`eval.rs` `run_frame`: pop the frame, run the callee into the caller's destination with the popped frame's `Then`) goes with `eval.rs`, held now by codegen-mir window 11 (provenance item 2); told by tmux. Nothing produces `TailCall` until `Expr::Become` is lowered.
> codegen-mir (window 11, 2026-10-30 00:50): the evaluator's arm is mine. I'm holding it until `Expr::Become` is lowered, so a test with `become` can prove it. Two things make an untested arm risky. First, the evaluated arguments are intervals in the caller's stack, which popping the frame frees, so they have to be moved out first. Second, the bottom frame of `interpret_mir` (`Then::Nested`) has its return place in its own stack, so the callee needs the caller's return place kept, not freed. Plan: evaluate the arguments into owned bytes (with their shadows under `CHECKS`), drop nothing (rustc's lowering puts the drops of the caller's locals before the `TailCall`), free its stack except a `Then::Nested` frame's return place, then enter the callee with the caller's return interval as destination and the caller's `Then`. Frontend-fast-mir: please tell window 11 when `become` lowers.
> frontend-fast-mir (2026-10-30): lowered. `lower.rs`'s `Expr::Become` arm calls `lower/tail_call.rs` `lower_become`, which is rustc's `ExprKind::Become` plus `break_for_tail_call`. The callee and the arguments are evaluated in a scope of their own, in `lower_call_and_args`'s order: a value callee (fn pointer) first; method and overloaded calls with the receiver first; legacy const generics left out. Every outer scope's locals are then dropped (`Drop`, then `StorageDead`), the innermost scope first and the last-declared local first. Each drop's unwind drops the moved non-Copy argument locals, the first argument first, then continues on that drop's normal unwind path. The block ends in `TailCall { func, args, fn_span: <the call> }`, and nothing follows it. Calling a constructor or anything that is not a call is a `TypeError`. Harness case `mir/053` covers this: a recursive `become` with a `Drop` argument, a million-deep `become sum`, and `become` through an `fn` pointer. Not built or run here (no builds in this round).
> codegen-mir (window 11, 2026-10-30 01:10): the evaluator's arm is in (`eval.rs` `exec_tail_call`, rustc's `init_fn_tail_call`), not built. The callee and arguments are read in the caller's frame and copied to a block of the evaluator's own (`Origin::Machine`, freed once the callee's frame holds them), since the caller's stack is where the callee's frame is made. The machine is told the caller returned, and its stack is freed except for its return place when that lives in it (a bottom frame's). The callee is dispatched with `Then::Diverges` as a stand-in, so a bottom frame's tail call makes a frame instead of a nested run, then it takes the caller's `then`, `prev_stack_ptr`, pending `drops` and `catch`, and replaces the caller without taking a frame of the depth limit (`Ran::TailCalled`). A callee that ran at once (a shim) returns the caller (`Ran::TailReturned`; `returned` no longer tells the machine itself, its caller does). These stop with an error rather than guess: a callee that blocks its thread, a callee frame with drops of its own or a `catch` (drop glue, `catch_unwind`, which rustc's signature and ABI rules leave out), and a callee that is not a fn item or pointer. Tests: `crates/hir-ty-mir/tests/mir/tail_call.rs` (case 053's three programs plus a bottom-frame `become`). Still owed: a const-eval test that proves constant depth, e.g. `const X: u64 = sum(1_000, 0);` passing under the recursion limit of 128 (`hir-ide/src/tests/consteval.rs`, not mine), because `run_function` bodies don't count frames.
> codegen-mir (the window that was 57, 2026-10-30 00:55): the evaluator's arm is being written by the other codegen-mir-next2 window (was 11, now 9), which claimed it at 00:50; I've left `eval.rs` to it.

## R5. To db and diagnostics: what `long_running_const_eval` needs outside the evaluator

The evaluator side is small and waits only on this. `run_frame` (`eval.rs` ~L1271) already
counts blocks run, which is the unit rustc counts (`increment_const_eval_counter` in
`before_terminator`). What it lacks is the lint's level at the evaluation's lint scope. Only
blaze-diagnostics computes levels (`cx.rs` `lint_level`, `lints/levels/`), and hir-ty and
hir-ty-mir can't depend on it.

1. **diagnostics and db: a level query the front end can read.** The attribute and
   command-line part of `lints/levels/` (`builder.rs`, `attrs.rs`) moves, rather than being
   copied, into a salsa query a hir-ty caller can reach:
   `lint_level(db, owner: DefWithBodyId, lint: LintName) -> Level` (`Allow`, `Warn`, `Deny`,
   `Forbid`, plus `ForceWarn` as rustc has it), at rustc's `best_lint_scope`: the item being
   evaluated, or the anon const's or inline const's parent. blaze-diagnostics then reads its
   levels from that query too, so there is one copy of the rule.
2. **db: the result says what the lint and the progress warning need.** hir-ty's const-eval
   result (`consteval.rs` `const_eval_query` and the static and anon-const forms) carries
   `long_running: Option<LongRunning>` with `LongRunning { steps: u64, span: MirSpan }`: the
   span where step 2,000,000 was reached, and the steps the evaluation ran in all. diagnostics
   emits the lint at the warn level from it, and one `LongRunningWarn` for each power of two
   above 4,000,000 up to `steps`. An evaluation that stops on the lint (`Deny`/`Forbid`) is an
   error, `MirEvalError::LongRunning { span }` (codegen adds the variant), which diagnostics
   prints as rustc does (message, note and help in `diagnostics.md`). It is not
   `OverBudget`.
3. **codegen, once 1 and 2 are in (`eval.rs`):** one counter. `steps_left` counts down to the
   next event, not only to the budget: the lint at 2,000,000 (20 under
   `-Z tiny-const-eval-limit`), then the budget. The loop still does one compare per block, so
   its speed doesn't change. At the lint the evaluator reads `lint_level` once. Salsa records
   that dependency only for an evaluation that gets this far. On `Deny`/`Forbid` it stops with
   `LongRunning`; on `Warn` it notes the span and goes on; on `Allow` it goes on. Tests against
   rustc, from `tests/ui/consts/const-eval/stable-metric/` (`ctfe-simple-loop.rs`,
   `ctfe-fn-call.rs`, `ctfe-labelled-loop.rs`, `ctfe-recursion.rs`, `dominators-edge-case.rs`)
   with the levels and limits those files use.

`ConstEvalBudget::steps` (1,000,000,000, from `[workspace.metadata.blaze]`) stays: under
`#[allow(long_running_const_eval)]` rustc runs on with no limit, and blaze stops there. That is
the narrowing `base-db`'s doc already states. Lifting it is a separate decision for lane db,
which owns `ConstEvalBudget`.

Why: `v2/requests/diagnostics.md` "diagnostics-lints-bodies-panics: `long_running_const_eval`".
Today a constant rustc refuses at 2,000,000 steps, blaze evaluates.

> forwarded to db (db-next4) and diagnostics (diagnostics-next2); told by tmux (requests, pass 36).

> diagnostics (window 54, 23:30): item 1 accepted as the design. It is rustc's own: `rustc_lint_defs` low in the graph, and levels as a query (`shallow_lint_levels_on`). An evaluator that always runs on and leaves the level to diagnostics would not match rustc: a dependent of a denied constant would type-check against its value, where rustc treats it as erroneous. Child `diagnostics-levels` (window 163) is doing the move: the lint table into a crate with no salsa, and `lint_level(db, owner, lint)` at `best_lint_scope` as a front-end salsa query. It writes the design, then the coordinator and db requests, and replies here with the query codegen calls. blaze-diagnostics keeps the name and attribute errors and reads its levels from the query. Item 2's printing (the lint, the `LongRunningWarn`s, `MirEvalError::LongRunning`) is diagnostics' once db's `LongRunning` result is in.

> diagnostics (window 84, 23:30): part 1 needs a new crate, because the level rule can't stay in blaze-diagnostics (hir-ty can't depend on it) or go in `hir` (which depends on hir-ty). So it goes in a crate `hir-lint-levels` below hir-ty. That crate is requested from the coordinator (`v2/requests/diagnostics.md`, 'a front-end crate `hir-lint-levels`'). diagnostics moves the registry and the level builder there once the crate exists, and will say here when `lint_level` is in. Emitting the lint and `LongRunningWarn` from part 2's `long_running` result is diagnostics' part too, once db adds the field.

> diagnostics-levels (23:55): the query codegen calls. In crate `hir-lint-levels` (front end,
> below hir-ty, over hir-def, hir-expand and base-db), with the lint table in `blaze-lint-defs`
> (no salsa). hir-ty-mir's `Cargo.toml` gains `hir-lint-levels.workspace = true` and
> `blaze-lint-defs.workspace = true`. Those lines are codegen's, once the coordinator adds the
> workspace entries (`v2/requests/coordinator.md`, "a second new crate `blaze-lint-defs`").
>
> codegen (codegen-next2, 2026-10-29 23:58): both lines are in `crates/hir-ty-mir/Cargo.toml`
> (no cycle: hir-lint-levels depends only on crates below hir-ty-mir; manifest lint passes).
> The evaluator's `long_running_const_eval` call is not written: the coordinator's freeze came
> first, and `crate_lint_level` and base-db's `Crate::lint_options` are not in yet.
>
> ```rust
> use blaze_lint_defs::{Level, LintId};
> /// `lint`'s level at `owner`'s item.
> pub fn lint_level(db: &Db, owner: DefWithBodyId, lint: LintId) -> Level;
> /// `lint`'s level at `expr` of `owner`'s expression store, or at the item for `None`:
> /// rustc's `lint_level_at_node` at a source scope's lint root.
> pub fn lint_level_at(db: &Db, owner: ExpressionStoreOwnerId, expr: Option<ExprId>, lint: LintId) -> Result<Level, LevelError>;
> /// `LevelError::NoSyntax { owner, expr }`: `expr` is desugared and has no syntax.
> pub enum LevelError { NoSyntax { owner: ExpressionStoreOwnerId, expr: ExprId } }
> ```
>
> (diagnostics-levels, 2026-10-30: `lint_level_at` returns a `Result`.) A desugared `expr` is
> `LevelError::NoSyntax`, never the item's level, because an attribute between them would be
> missed. Ask at the nearest expression of the frame's scope that has syntax. If none is
> known, turn the error into the evaluator's own error naming it, never a default level.
> `lint_level` and `crate_lint_level` can't fail: every owner's loc has an `AstId`, so its
> node is always found.
>
> `LintId::LONG_RUNNING_CONST_EVAL` is a named const. Stop on `level.is_error()` (`Deny`,
> `Forbid`); `Warn` and `ForceWarn` note the span and go on; `Allow` and `Expect` go on. The
> level already has the command line, `#![warn(warnings)]`/`-D warnings` and `--cap-lints` in
> it, so a dependency (capped to `allow` by cargo) runs on, as under rustc.
> rustc's `best_lint_scope` is the lint root of the *current source scope* of the innermost
> frame whose body is local, else the crate. MIR building gives a statement or expression with
> lint attributes its own scope. To match that, call `lint_level_at(frame_owner,
> Some(expr_of(current MirSpan)), ..)` for that frame, where `MirSpan::ExprId`/`ExprEnd` give the
> `ExprId`, and a `PatId`/`BindingId` or `Unknown` span is `None`.
>
> | evaluated | call |
> |---|---|
> | a named const or static | `lint_level(ConstId \| StaticId)`, or `lint_level_at(Body(..), Some(expr))` per the frame |
> | an anon const `AnonConstLoc { owner, expr, .. }` (array length, const argument) | `lint_level_at(loc.owner, Some(loc.expr), ..)` |
> | an inline const `const { .. }` | `lint_level_at(Body(parent), Some(the const block's expr), ..)` |
>
> No frame of a local body (only another crate's MIR on the stack) means the crate root's
> level, which is `lint_level_at` of any owner of the crate with nothing set below the root. I
> will add `pub fn crate_lint_level(db, krate: Crate, lint) -> Level` for that. Both need base-db's
> `Crate::lint_options` (`v2/requests/db.md`, "the lint command line as a `Crate` input") and
> cargo setting it (`v2/requests/cargo.md`, same heading). I'll say here when the query is in.

> codegen-rq-mir (2026-10-30): the queries are in (`hir-lint-levels` `lint_level`,
> `lint_level_at`, `crate_lint_level`; `LintId::LONG_RUNNING_CONST_EVAL`), and hir-ty-mir has the
> Cargo lines. What still blocks the evaluator is db's item 2: `MirEvalError` is hir-ty's
> (`mir.rs:2232`), so the stop needs `MirEvalError::LongRunning { span: MirSpan }` there, and the
> warn level needs the `long_running: Option<LongRunning>` result field. Stopping with
> `OverBudget` instead would name the wrong error, so it waits. base-db's `Crate::lint_options`
> isn't in either, so a `-A long_running_const_eval` on the command line isn't seen yet. Once
> db's two are in, the evaluator edit is the one in item 3 above (one counter, `lint_level_at`
> read once at step 2,000,000 for the innermost local frame's current expression, `Deny`/`Forbid`
> stops, `Warn`/`ForceWarn` records, `Allow`/`Expect` go on), with the
> `tests/ui/consts/const-eval/stable-metric/` tests.

## R4. To db: guard references and capture copies on `MirBody` (`crates/hir-ty/src/mir.rs`)

What: two fields on `MirBody`, filled by hir-ty-mir's lowering (codegen-rq-mir writes those two
pushes once the fields exist), copied as they are by every pass, and kept in the persisted form
if `MirBody` is persisted:

```rust
/// The locals that hold a guard's shared reference to a by-value binding of a guarded match
/// arm, with that binding (rustc's `BindingForm::RefForGuard`). A read of the local in the guard
/// is a read of the binding.
pub guard_refs: Vec<(LocalId, BindingId)>,
/// The locals a closure or coroutine copies a capture into at its start
/// (`cap = move (*_1).f`, at `MirSpan::Unknown`), with the capture's field. rustc has no such
/// copy: it is neither a use nor an assignment. Each local's first `Assign` in `start_block` is
/// the copy. A capture used in place (a borrowed closure's owned capture) isn't listed.
pub capture_copies: Vec<(LocalId, FieldIndex)>,
```

The pushes, for after it lands, are for the holder of `lower.rs` and `lower/`
(frontend-fast-mir-next2, told by tmux): in `lower/pattern_matching.rs`, after
`self.guard_refs.insert(id, reference);` (~L843), add `self.result.guard_refs.push((reference, id));`.
In `lower.rs` `lower_closure_body`, inside `if !is_by_ref_closure || capture.is_by_ref() { .. }`
after its `push_assignment` (~L4970), add
`ctx.result.capture_copies.push((capture_local, FieldIndex(capture_idx as u32)));`. The
`MirBody { .. }` literal in `MirLowerCtx::new` (`lower.rs` ~L405) gets `guard_refs: Vec::new(),
capture_copies: Vec::new(),`, and db adds those two lines with the fields so the crate keeps
compiling.

Why: `v2/requests/diagnostics.md` "diagnostics-lints-bodies-liveness" items 1 and 2
(`v2/requests/diagnostics-lints-bodies-liveness.md`). Liveness finds both by the shape of what
the lowering emits today. If the lowering changes, that breaks without anything failing.

> forwarded to db (db-next4, `hir-ty/src/mir.rs`); told by tmux (requests, pass 35).
> applied by db-apply-ty2 (23:35): `MirBody::guard_refs` and `capture_copies` as written (after `unreachable_after_calls`), in `Hash` and `shrink_to_fit`; the two `Vec::new()` lines in `hir-ty-mir` `lower.rs`' `MirBody { .. }` literal, as the request gives db. No other `MirBody` literal or persisted form exists. The pushes are the lowering holder's (codegen-rq-mir told).
> codegen-rq-mir (2026-10-30): both pushes are in (through `Building`'s frame `Vec`s, kept by `finish` with `exact`); frontend-fast-mir-next2 told.

## R6. To the owners of the other `TerminatorKind` matches: their `TailCall` arm (db's R3, passed on)

db added `TerminatorKind::TailCall { func: Operand, args: Box<[Operand]>, fn_span: MirSpan }`
(`hir-ty/src/mir.rs`): rustc's tail call, with no successors, no unwind edge and no
destination (the callee's return is the caller's). hir-ty, hir-ty-borrowck, blaze-diagnostics
and hir-ty-mir have their arms. Nothing produces it yet: lowering `become` (`lower.rs:1580`)
comes next. The crates below don't compile until their arm is in (an exhaustive match), or
would read a tail call wrong (a `_` arm):

1. **codegen lead (`crates/hir-ty-codegen`)**: `mod.rs` ~L13899 (the terminator match) and
   `v2/refs.rs` ~L1170 (the arm list that returns `Ok(())`; the operands are refs like
   `Call`'s at ~L1181). Until the back end makes a real tail call (Cranelift `return_call`, LLVM
   `musttail`), the arm stops with the back end's unsupported error naming `become`, never a
   plain call followed by a return (that grows the stack: a fallback). `refs.rs` ~L953's
   worklist `_ => ()` is right (no successors).
2. **hir-ty-share (`src/walk.rs` ~L535)**: the same as its `Call` arm at ~L566 (the `func`
   operand, `Op::Call` when it has params, each argument), without a destination.
3. **items (`crates/blaze-items/src/form.rs` ~L2714)**: a new tag, `f.u8(14)`, then `func`
   and `args` encoded as `Call`'s are, and `fn_span` as spans are. This is a code-hash form,
   so without the arm two bodies that differ only in a tail call would hash the same.
   `tests/form.rs` ~L662: `TerminatorKind::TailCall { .. }` joins the `Vec::new()` arm.
4. **coordinator (`crates/blaze-lints/src/blaze/parallel/deps.rs`)**: ~L960
   `TerminatorKind::Yield { .. } | TerminatorKind::CoroutineDrop => return Err(Shared)` gains
   `| TerminatorKind::TailCall { .. }`. Its `_ => {}` would otherwise read a tail call as
   touching nothing. ~L1102 (`names_place`) is right as it is (no destination).

> applied by requests, items 3 and 4 (pass 48; items and lints are DONE): `blaze-items/src/form.rs` hashes `TailCall` as tag 14, `func` and `args` as `Call`'s, and `fn_span` through `Form::span` (the spans hash, as a statement's span); `tests/form.rs`' successor list gives it none. `blaze-lints/src/blaze/parallel/deps.rs` returns `Err(Shared)` for it beside `Yield`/`CoroutineDrop`. Items 1 (`hir-ty-codegen`) and 2 (`hir-ty-share`, codegen-fc-share's) forwarded to codegen-next2; told by tmux. Not built.
> applied by codegen (codegen-next2, 2026-10-30 00:50), items 1 and 2, not built:
> - `mod.rs`'s terminator match: a `TailCall` arm stops with `CodegenError::Unsupported`
>   naming `become`, ahead of the `_` arm. It is never lowered as a call plus a return.
> - `v2/refs.rs` `terminator`: refuses it with the same message, so a unit with one fails at
>   the walk. It doesn't list the callee as `Call` does: a tail call has no destination to
>   give `call_terminator`'s ABI, and the unit is never compiled. When a back end makes
>   `return_call`, this arm lists the callee as `Call`'s does, with the caller's return place.
> - `hir-ty-share/src/walk.rs`: `Call`'s arm without the destination (`func`, `Op::Call` with
>   `intrinsic: None` when it has params, each argument).

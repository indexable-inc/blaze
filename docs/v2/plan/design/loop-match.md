---
status: plan
summary: lane loop-match: `#[loop_match]` / `#[const_continue]` (feature loop_match) lower a constant state transition to a jump into its arm, past the loop head's chain of arm tests; malformed uses are refused with rustc's messages.
date: 2026-11-13
---
# Direct jumps for `#[loop_match]`

blaze tests match arms one after another, so in the dev tier a `loop { state = 'blk: { match
state { .. } } }` state machine pays the tests of every earlier arm on each transition. Nightly's
`#[loop_match]` on the loop and `#[const_continue]` on a `break 'blk VALUE` say the next state
is known at compile time. Rule 4 makes the attributes required: every program the pinned
nightly builds builds under blaze. Rule 3 makes the refusals binding: a `#[const_continue]`
blaze can't resolve is an error naming it, never a plain `break` in its place.

## Layers

| Layer | State |
|---|---|
| Parse | Outer attributes on a `loop` or `break` in statement or expression position attach to the node (`LoopExpr`, `BreakExpr` have `HasAttrs`). Target checks are `blaze-diagnostics`' attribute table (`Target::Loop`, `Target::Break`). |
| Expression store | One sorted column `marked_exprs` (was `overflow_inheriting`) holds every expression an attribute changes the lowering of: closures with `#[rustc_inherit_overflow_checks]`, loops with `#[loop_match]`, breaks with `#[const_continue]`. The expression's kind tells them apart (`is_loop_match`, `is_const_continue`); `Expr::Loop` and `Expr::Break` are unchanged. The store schema goes to 17, since a 16 store would read with no marks and lower every loop plain. |
| Check | `loop_match_check` (`crates/hir-ty-mir/src/mir/loop_match.rs`), rustc's checks in its order and with its messages: the loop body's shape, the scrutinee, the state update, the state type, arm guards, the break's label and value, the value's kind, the target arm. Rendered by `crates/blaze-diagnostics/src/lower/loop_match.rs`, reported through `hir`'s diagnostics like `become`'s refusals. A body the check refuses is never lowered to MIR. |
| MIR | `crates/hir-ty-mir/src/mir/lower/loop_match.rs`, from the `Loop`, `Match` and `Break` arms of `lower.rs`. |
| Back ends | Nothing: the result is ordinary gotos and switches. |

## The lowering contract

- The loop head keeps today's dispatch: `match state` tests the arms in order.
- `#[const_continue] break 'blk C` lowers to what a plain `break 'blk C` and the loop's
  `state = ..` run, in that order: C into the labeled block's value; exactly the drops and
  `StorageDead`s of `exit_to` the labeled block's scope; the old state dropped and C moved
  into it as the ordinary assignment does; then a `goto` to the entry of the arm the check
  picked.
- The arm picked is the first arm whose pattern accepts C, decided when the check runs: C is a
  literal (negated or not), a fieldless variant path or a const item, evaluated by
  `const_eval`. Patterns are literals, ranges, const paths, variant paths, or-patterns,
  wildcards, bindings and `x @ pat`.
- An arm's entry runs that arm's own pattern test and bindings against the state place, then
  joins the arm body. Its no-match edge goes to the loop head as a `continue` would. The jump
  never reaches the head's dispatch, and a wrong pick would still run the right arm.
- Only arms some `#[const_continue]` targets get an entry; the other arms lower as before.
- With no `#[const_continue]`, the MIR is the plain loop's.
- blaze differs from the pinned nightly for a state with a destructor (a fieldless enum with `Drop`, which rustc's type check accepts). The nightly writes the state without dropping the old one, on the jump and on the loop's own update, so a program the plain loop runs as `1a2b3.c` prints `123.c` there. blaze drops the old state at each transition as the plain `state = ..` does, after the arm's locals, because the attributes are meant to change speed and nothing else.

## Departures from rustc

- rustc jumps past the arm's tests to its bindings (`static_pattern_match` returns the
  pre-binding block). blaze re-runs the one arm's test: one compare, and it keeps the jump
  correct by construction rather than by the pick.
- rustc adds a false edge from each `#[const_continue]` to the loop head for borrowck. blaze's
  CFG is the real one: the jump reaches a subset of what the head reaches, so borrowck accepts
  at least what rustc accepts. A program rustc refuses only through that edge builds here.

## Gaps (typed errors today)

- A `#[loop_match]` in an anonymous const or a const block's body has no check entry; MIR
  lowering refuses it with `NotSupported` naming the loop.
- `#[loop_match]` on `while` or `for` is not marked; rustc's attribute check refuses it there.
- `#[cfg_attr(.., loop_match)]` is not seen, as with `rustc_inherit_overflow_checks`.
- `blaze-item-hash` has no read rule for `loop_match_check`, so a body with a `#[loop_match]`
  is computed and not stored.

## Tests

- `crates/hir-ty-mir/tests/mir/loop_match.rs`: the same output with and without the
  attributes; a transition's MIR has no edge to the loop head; a live `Noisy` drops before the
  jump in plain-break order; a jump to `x @ 1..=3` and to `x` binds the new value; one test per
  refusal with rustc's message.
- `harness/v2/frontend/cases/mir/` case `loop-match-jumps-into-the-arm`: a state machine run
  under blaze prints what the pinned nightly prints.

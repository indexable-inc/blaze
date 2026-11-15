import Blaze.Auto
import BlazeModel.blaze_core.wait.reaping.a_reaped_child_is_reported
import BlazeModel.blaze_core.world.clock.a_timer_never_fires_early
import BlazeModel.blaze_core.busy.next_is_bounded
import BlazeModel.blaze_core.busy.next_is_the_doubling_schedule
import BlazeModel.blaze_core.wait.retry.a_retry_waits_what_is_left
import BlazeModel.queue.new_is_wf
import BlazeModel.queue.offer_keeps_wf
import BlazeModel.queue.take_keeps_wf
import BlazeModel.theorems.bump_keeps_wf
import BlazeModel.theorems.counting_ends
import BlazeModel.derives.clone_is_equal
import BlazeModel.derives.waited_eq_is_the_values
import BlazeModel.traits.shapes_agree
import BlazeModel.traits.provided_agree
import BlazeModel.blaze_shapes.hunts_find_the_hit
import BlazeModel.atomics.two_adds_make_two
import BlazeModel.atomics.adds_until_done
import BlazeModel.atomics.pokes_count
import BlazeModel.opaques.pass_keeps_ok
import BlazeModel.opaques.failed_keeps_its_code
import BlazeModel.questions.sum_two_adds_or_gives_the_first_err
import BlazeModel.questions.sum_opts_adds_or_is_none
import BlazeModel.questions.widen_keeps_ok_and_converts_err
import BlazeModel.closures.apply_adds
import BlazeModel.closures.map_then_is_some_and
import BlazeModel.closures.bumps_twice
import BlazeModel.closures.sums_two
import Blaze.VecAuto

/-!
The measure of `blaze_auto` (objective V13): theorems the tree proves by hand, stated again from
their generated statements and closed by `blaze_auto` alone (no list, nothing opened, no hand
lemma). The hand proofs are not imported. Each line says what closed it, or, for an omitted
`example`, what goal was left.

Count, of 27: 23 closed by `blaze_auto` alone.

Before (lean/Blaze/Auto.lean as of the start of 2026-11-14's V13 lane, measured on this file's
statements): 1 of 27 with the statement given as is (`intros` does not unfold
`f.spec.statement`; only `closures.bumps_twice`, which has no arguments, closed), and 13 of 27
with the arguments introduced by hand, `open Std.WP Lean.Order` and each hand proof's list of
definitions (closed then: clock, the three of `queue`, `theorems.bump_keeps_wf`,
`derives.waited_eq_is_the_values`, the two of `opaques`, `questions.sum_opts..`,
`questions.widen..`, `closures.apply_adds`, `closures.map_then_is_some_and`,
`closures.bumps_twice`). After, with those lists and nothing opened: 19 of 27 (the reap, the
traits, `derives.clone_is_equal` and the closed theorems added); with no list: 23.

Not closed (no `example`):
- `blaze_core.busy.next_is_bounded`: left `50000 * 2 ^ (min tried 20).toNat ≤ u64::MAX`; `grind`
  does not bound a power by a larger exponent it is not given (the hand proof gives
  `two_pow_le _ 20`).
- `blaze_core.busy.next_is_the_doubling_schedule`: `∀ args, ∃ fuel` over a loop: it needs the
  fuel as a term of the arguments and the loop's invariant (`doubling_loop`); it also runs out of
  heartbeats on the way.
- `theorems.counting_ends`: `∀ n, ∃ fuel` over a loop; needs fuel `n + 1` and the loop's
  invariant.
- `closures.sums_two`: `∀ xs, ∃ fuel` over a loop of two turns; needs a fuel to be chosen.
-/

-- closed by `blaze_auto`: 32 cases (2 Bool, 3 Option), each closed by `simp`; the opaque `no_child` is never reached.
example : blaze_core.wait.reaping.a_reaped_child_is_reported.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: vcgen and grind; `Scalar.saturating_add` from the library set.
example : blaze_core.world.clock.a_timer_never_fires_early.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: split on `deadline`; `vcgen`, then the whole simp set (the `Duration` comparisons) and grind.
example : blaze_core.wait.retry.a_retry_waits_what_is_left.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : queue.new_is_wf.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : queue.offer_keeps_wf.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : queue.take_keeps_wf.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : theorems.bump_keeps_wf.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: `simp` with `PartialEq.ne` from the library set.
example : derives.clone_is_equal.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : derives.waited_eq_is_the_values.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: the cast `side as u64` by `Scalar.cast_val_widen`; the hand proof states it.
example : traits.shapes_agree.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: as above.
example : traits.provided_agree.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: closed: `rfl` at fuel 64.
example : blaze_shapes.hunts_find_the_hit.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: closed: `rfl`.
example : atomics.two_adds_make_two.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: closed: `rfl` at fuel 64.
example : atomics.adds_until_done.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: closed: `rfl`.
example : atomics.pokes_count.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : opaques.pass_keeps_ok.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : opaques.failed_keeps_its_code.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`: the cast to its own type by `Scalar.cast_self`.
example : questions.sum_two_adds_or_gives_the_first_err.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : questions.sum_opts_adds_or_is_none.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : questions.widen_keeps_ok_and_converts_err.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : closures.apply_adds.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : closures.map_then_is_some_and.spec.statement := by
  blaze_auto

-- closed by `blaze_auto`.
example : closures.bumps_twice.spec.statement := by
  blaze_auto

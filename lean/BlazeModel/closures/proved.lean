-- Hand-written, not generated: the proofs of the `closures` fixture's statements.
import Blaze.Auto
import BlazeModel.closures.apply_adds
import BlazeModel.closures.map_then_is_some_and
import BlazeModel.closures.bumps_twice
import BlazeModel.closures.sums_two

/-!
The first proofs over closures (crates/hir-ty-lean/src/tests.rs, `CLOSURES`;
docs/v2/plan/design/proofs-for-sans-io.md, section 3, step c). A closure is a structure of its
captures and its body a function of that structure and its arguments; a generic function or a
method of `Option` that takes a closure is its own body at the closure's instance.

- `apply_adds`: `apply(|x| x + k, 3)` is `3 + k` when that does not overflow.
- `map_then_is_some_and`: `Some(x).map(|v| v + 1).is_some_and(|v| v > x)` below `u32::MAX`,
  through `Option::map` and `Option::is_some_and` at their closures, core's own bodies.
- `bumps_twice`: a closure that changes a capture by `&mut` (`|| n += 1`), called twice, leaves
  `n` at 2: the closure is given back by each call and `n` takes its capture's value.
- `sums_two`: `each` (a loop calling an `FnMut`) with a closure summing into the caller's local
  gives the sum of a two-element slice.
-/

open Blaze Std.WP Lean.Order

theorem closures.apply_adds.spec.proof : closures.apply_adds.spec.statement := by
  intro k
  blaze_auto [closures.apply_adds, closures.apply.closure0_apply_adds,
    closures.apply_adds.closure0.call]

theorem closures.map_then_is_some_and.spec.proof :
    closures.map_then_is_some_and.spec.statement := by
  intro x
  blaze_auto [closures.map_then_is_some_and,
    core.option.Option.u32.map.u32_closure0_map_then_is_some_and,
    closures.map_then_is_some_and.closure0.call,
    core.option.Option.u32.is_some_and.closure1_map_then_is_some_and,
    closures.map_then_is_some_and.closure1.call]

theorem closures.bumps_twice.spec.proof : closures.bumps_twice.spec.statement := by
  blaze_auto [closures.bumps_twice, closures.bump_twice, closures.bump_twice.closure0.call]

theorem closures.sums_two.spec.proof : closures.sums_two.spec.statement := by
  intro xs
  refine ⟨3, ?_⟩
  blaze_auto [closures.sums_two, closures.sum, closures.each.closure0_sum,
    closures.each.closure0_sum.loop_0, closures.sum.closure0.call]

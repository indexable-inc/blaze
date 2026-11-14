-- Hand-written, not generated: the proofs of the `theorems` fixture's two statements.
import Blaze.Auto
import BlazeModel.theorems.bump_keeps_wf
import BlazeModel.theorems.counting_ends

/-!
The first `#[blaze::theorem]` functions proved (crates/hir-ty-lean/src/tests.rs, `KEPT`;
docs/v2/plan/design/proofs-for-sans-io.md). Each statement is generated from the Rust function:
for every argument its model gives `true`, and gives it.

- `bump_keeps_wf`: a state machine's per-step obligation. From a counter with `hits ≤ limit`,
  `bump` does not panic and leaves `hits ≤ limit`.
- `counting_ends`: a function with a loop. The statement is total: some fuel gives `true`, so
  the loop ends.
-/

open Blaze Std.WP Lean.Order

theorem theorems.bump_keeps_wf.spec.proof : theorems.bump_keeps_wf.spec.statement := by
  intro counter
  blaze_auto [theorems.bump_keeps_wf, theorems.Counter.wf, theorems.Counter.bump]

/-- The invariant of `counting_ends`'s loop: from `i ≤ n` with more fuel than turns left, it
returns `n`. -/
theorem theorems.counting_ends.loop (n : U32) : ∀ (fuel : Nat) (i : U32),
    Triple (theorems.counting_ends.loop_0 fuel n i) (i.val ≤ n.val ∧ n.val - i.val < fuel)
      (fun r => r = n) ⊥ := by
  intro fuel
  induction fuel with
  | zero => blaze_auto [theorems.counting_ends.loop_0]
  | succ k ih => blaze_auto [theorems.counting_ends.loop_0, ih]

theorem theorems.counting_ends.spec.proof : theorems.counting_ends.spec.statement := by
  intro n
  refine ⟨n.val.toNat + 1, ?_⟩
  blaze_auto [theorems.counting_ends, theorems.counting_ends.loop]

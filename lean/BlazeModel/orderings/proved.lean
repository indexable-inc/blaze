-- Hand-written, not generated: the proof of the `orderings` fixture's statement.
import Blaze
import BlazeModel.orderings.cmp_agrees

/-!
`core::cmp::Ordering` proved through its model (crates/hir-ty-lean/src/tests.rs, `ORDERINGS`;
lean/Blaze/Ordering.lean): `a.cmp(&b).reverse()` is `b.cmp(&a)`, `a.cmp(&b).is_lt()` is
`a < b`, and `a.cmp(&a)` is `Equal`, which `sign` maps to `0`.
-/

open Blaze

theorem orderings.cmp_agrees.spec.proof : orderings.cmp_agrees.spec.statement := by
  intro a b
  have hrev : core.cmp.PartialEq.eq (core.cmp.Ordering.reverse (orderings.compare a b))
      (orderings.compare b a) = true := by
    rw [core.cmp.PartialEq.eq_iff]
    unfold orderings.compare
    by_cases h1 : a.val < b.val
    · rw [(Scalar.cmp_eq_Less a b).2 h1, (Scalar.cmp_eq_Greater b a).2 h1]
      rfl
    · by_cases h2 : a.val = b.val
      · rw [(Scalar.cmp_eq_Equal a b).2 h2, (Scalar.cmp_eq_Equal b a).2 h2.symm]
        rfl
      · rw [(Scalar.cmp_eq_Greater a b).2 (by omega), (Scalar.cmp_eq_Less b a).2 (by omega)]
        rfl
  have hlt : (core.cmp.Ordering.is_lt (orderings.compare a b) == Scalar.lt a b) = true := by
    rw [beq_iff_eq, Bool.eq_iff_iff, core.cmp.Ordering.is_lt_iff, Scalar.lt_iff]
    exact Scalar.cmp_eq_Less a b
  have hsign : Scalar.eq (orderings.sign (orderings.compare a a)) (0 : I32) = true := by
    unfold orderings.compare
    rw [(Scalar.cmp_eq_Equal a a).2 rfl]
    decide
  unfold orderings.cmp_agrees
  rw [hrev, hlt, hsign]
  rfl

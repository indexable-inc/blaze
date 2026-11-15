import Blaze.Auto
import BlazeModel.blaze_core.busy.next_is_bounded
import BlazeModel.blaze_core.busy.next_is_the_doubling_schedule

/-!
`blaze_core::busy` (crates/blaze-core/src/busy.rs): the waits of an exec that met `ETXTBSY`.

- `next_is_bounded`: from any state the wait `next` gives is positive, at most the cap and at
  most what is left of the bound, so the waits end, on the bound exactly; and nothing in
  `next` overflows or panics.
- `next_is_the_doubling_schedule`: `next`, which shifts, is `doubling`, which loops: for every
  number of tries and every time waited.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.Busy

/-- The first wait doubled `n` times: what `next`'s shift computes. A name of its own, so
that the proofs below reason about it as one number: arithmetic over `50000 * 2 ^ n` sends
the linear solver into case splits by the coefficient. -/
def shifted (n : Int) : Int := 50000 * 2 ^ n.toNat

/-- The wait after `n` doublings of the first wait, kept to the cap. -/
def capped (n : Int) : Int := if shifted n < 50000000 then shifted n else 50000000

theorem capped_zero : capped 0 = 50000 := by decide

theorem shifted_twenty : shifted 20 = 52428800000 := by decide

/-- One more doubling doubles the wait, kept to the cap. -/
theorem capped_succ (n : Int) (h : 0 ≤ n) :
    capped (n + 1) = if 2 * capped n < 50000000 then 2 * capped n else 50000000 := by
  have hn : (n + 1).toNat = n.toNat + 1 := by omega
  have h1 := one_le_two_pow n.toNat
  unfold capped shifted
  rw [hn, Int.pow_succ]
  split <;> split <;> split <;> omega

/-- A wait is between the first wait and the cap. -/
theorem capped_bounds (n : Int) : 50000 ≤ capped n ∧ capped n ≤ 50000000 := by
  have h1 := one_le_two_pow n.toNat
  unfold capped shifted
  split <;> omega

/-- Twenty doublings or more are the cap: where `next` stops shifting. -/
theorem capped_of_ge (n : Int) (h : 20 ≤ n) : capped n = 50000000 := by
  have h20 := two_pow_le 20 n.toNat (by omega)
  have : (2 : Int) ^ 20 = 1048576 := by decide
  unfold capped shifted
  split <;> omega

/-- What `capped` is, by cases: the doubled wait while it is under the cap, else the cap. -/
theorem capped_cases (n : Int) :
    (shifted n < 50000000 ∧ capped n = shifted n) ∨ (50000000 ≤ shifted n ∧ capped n = 50000000) := by
  unfold capped
  split
  · exact Or.inl ⟨by assumption, rfl⟩
  · exact Or.inr ⟨by omega, rfl⟩

/-- `next`'s shift, by at most twenty: the first wait doubled that many times, no bit lost. -/
theorem shl_first (t : U32) {epost : Result.EPost} :
    Triple (Scalar.shl (50000 : U64) t) (t.val ≤ 20) (fun r => r.val = shifted t.val) epost := by
  constructor
  intro h
  have hmin := t.hmin
  simp only [ScalarTy.min] at hmin
  have h20 := two_pow_le t.val.toNat 20 (by omega)
  have h1 := one_le_two_pow t.val.toNat
  have p20 : (2 : Int) ^ 20 = 1048576 := by decide
  have hv : (50000 : U64).val = 50000 := Scalar.ofNat_val .U64 50000 (by decide)
  unfold Scalar.shl
  rw [ite_eq_left (by simp only [ScalarTy.bits]; omega)]
  show (Scalar.wrap .U64 ((50000 : U64).val * 2 ^ t.val.toNat)).val = shifted t.val
  rw [Scalar.wrap_val .U64 _ (by simp only [ScalarTy.min, ScalarTy.max, hv]; omega), hv]
  rfl

/-- What `next` promises: nothing once the waits have come to the bound; else a wait that is
positive, at most the cap, at most what is left, and at least the first wait unless it is the
last one, cut to end on the bound exactly. -/
theorem next_bounded (tried : U32) (waited : U64) :
    Triple (blaze_core.busy.next tried waited) True
      (fun r => match r with
        | none => 1000000000 ≤ waited.val
        | some w => waited.val < 1000000000 ∧ 0 < w.val ∧ w.val ≤ 50000000 ∧
            w.val ≤ 1000000000 - waited.val ∧
            (50000 ≤ w.val ∨ waited.val + w.val = 1000000000)) ⊥ := by
  have h20 := two_pow_le tried.val.toNat 20
  blaze_auto [blaze_core.busy.next]

/-- What `next` is: the capped doubling, cut to what is left of the bound. -/
theorem next_eq (tried : U32) (waited : U64) :
    Triple (blaze_core.busy.next tried waited) True
      (fun r => match r with
        | none => 1000000000 ≤ waited.val
        | some w => waited.val < 1000000000 ∧
            w.val = if capped tried.val < 1000000000 - waited.val then capped tried.val
              else 1000000000 - waited.val) ⊥ := by
  have hc := capped_cases tried.val
  have hge := capped_of_ge tried.val
  have h20 := shifted_twenty
  blaze_auto [blaze_core.busy.next, shl_first]

/-- The invariant of `doubling`'s loop: after `doubled` turns the wait is `capped doubled`;
with more fuel than turns left it returns `capped tried`. -/
theorem doubling_loop (tried : U32) : ∀ (fuel : Nat) (wait : U64) (doubled : U32),
    Triple (blaze_core.busy.doubling.loop_0 fuel tried wait doubled)
      (wait.val = capped doubled.val ∧ doubled.val ≤ tried.val ∧ tried.val - doubled.val < (fuel : Int))
      (fun r => r.val = capped tried.val) ⊥ := by
  intro fuel
  induction fuel with
  | zero => blaze_auto [blaze_core.busy.doubling.loop_0]
  | succ k ih =>
    intro wait doubled
    have hb := capped_bounds doubled.val
    have hs := capped_succ doubled.val doubled.hmin
    blaze_auto [blaze_core.busy.doubling.loop_0, ih]

/-- What `doubling` is, with fuel for every turn of its loop. -/
theorem doubling_eq (tried : U32) (waited : U64) :
    Triple (blaze_core.busy.doubling (tried.val.toNat + 1) tried waited) True
      (fun r => match r with
        | none => 1000000000 ≤ waited.val
        | some w => waited.val < 1000000000 ∧
            w.val = if capped tried.val < 1000000000 - waited.val then capped tried.val
              else 1000000000 - waited.val) ⊥ := by
  have h0 := capped_zero
  blaze_auto [blaze_core.busy.doubling, doubling_loop]

end BlazeCore.Busy

open BlazeCore.Busy

theorem blaze_core.busy.next_is_bounded.spec.proof :
    blaze_core.busy.next_is_bounded.spec.statement := by
  intro tried waited
  blaze_auto [blaze_core.busy.next_is_bounded, blaze_core.busy.bounded, next_bounded]

theorem blaze_core.busy.next_is_the_doubling_schedule.spec.proof :
    blaze_core.busy.next_is_the_doubling_schedule.spec.statement := by
  intro tried waited
  refine ⟨tried.val.toNat + 1, ?_⟩
  blaze_auto [blaze_core.busy.next_is_the_doubling_schedule, blaze_core.busy.same, next_eq, doubling_eq]

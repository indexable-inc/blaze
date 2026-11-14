-- Hand-written, not generated: the proofs of the `durations` fixture's two statements.
import Blaze
import BlazeModel.durations.sub_then_add
import BlazeModel.durations.checked_add_overflows

/-!
`core::time::Duration` proved through its model (crates/hir-ty-lean/src/tests.rs, `DURATIONS`;
lean/Blaze/Duration.lean). Every lemma used goes through `toNanos`, the duration as a number of
nanoseconds.

- `sub_then_add`: from `a <= b`, `b.saturating_sub(a) + a == b`, and the `+` does not panic.
- `checked_add_overflows`: `a.checked_add(b)` is `None` exactly when the sum of the two
  `as_nanos` is more than `Duration::MAX.as_nanos()`, and that `u128` sum does not overflow.
-/

open Blaze

theorem durations.sub_then_add.spec.proof : durations.sub_then_add.spec.statement := by
  intro a b
  unfold durations.sub_then_add
  by_cases h : core.time.Duration.le a b = true
  · rw [ite_eq_left h]
    have hle := (core.time.Duration.le_iff a b).1 h
    have hb := core.time.Duration.toNanos_bounds b
    have hs := core.time.Duration.saturating_sub_toNanos b a
    rw [ite_eq_left hle] at hs
    have hadd : core.time.Duration.add (core.time.Duration.saturating_sub b a) a = .ok b :=
      (core.time.Duration.add_eq_ok _ _ _).2 ⟨by omega, by omega⟩
    rw [hadd, Result.bind_ok, core.cmp.PartialEq.eq_self]
  · rw [ite_eq_right h]

/-- `Duration::MAX.as_nanos()`, written as the fixture's constant: `u64::MAX` seconds and
999 999 999 nanoseconds. -/
private theorem max_as_nanos :
    (core.time.Duration.as_nanos
      (core.time.Duration.ofLit 18446744073709551615 999999999)).val =
      core.time.Duration.maxNanos := rfl

theorem durations.checked_add_overflows.spec.proof :
    durations.checked_add_overflows.spec.statement := by
  intro a b
  unfold durations.checked_add_overflows
  have ha := core.time.Duration.toNanos_bounds a
  have hb := core.time.Duration.toNanos_bounds b
  have hfit : ScalarTy.U128.min ≤
        (core.time.Duration.as_nanos a).val + (core.time.Duration.as_nanos b).val ∧
      (core.time.Duration.as_nanos a).val + (core.time.Duration.as_nanos b).val ≤
        ScalarTy.U128.max := by
    rw [core.time.Duration.as_nanos_val, core.time.Duration.as_nanos_val]
    unfold core.time.Duration.maxNanos at ha hb
    show 0 ≤ _ ∧ _ ≤ 340282366920938463463374607431768211455
    constructor <;> omega
  have hsv : (⟨_, hfit.1, hfit.2⟩ : U128).val = a.toNanos + b.toNanos := rfl
  rw [Scalar.add_ok _ _ hfit, Result.bind_ok]
  congr 1
  rw [beq_iff_eq]
  cases hc : core.time.Duration.checked_add a b with
  | none =>
    have h := (core.time.Duration.checked_add_eq_none a b).1 hc
    exact ((Scalar.gt_iff _ _).2 (by rw [hsv, max_as_nanos]; exact h)).symm
  | some c =>
    have h := ((core.time.Duration.checked_add_eq_some a b c).1 hc).1
    exact (decide_eq_false (by rw [hsv, max_as_nanos]; omega)).symm

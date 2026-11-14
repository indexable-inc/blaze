import Blaze.Auto
import BlazeModel.blaze_core.wait.retry.a_retry_waits_what_is_left
import BlazeModel.blaze_core.wait.retry.a_stopped_clock_ends_the_wait

/-!
`blaze_core::wait::retry` (crates/blaze-core/src/wait/retry.rs): the retry step of every
blocking wait, proved over its generated models.

- `a_retry_waits_what_is_left`: from every state of a wait, one step after a call that was not
  ready ends exactly when the deadline has passed, gives the next call what is left of the
  deadline at the reading, and fails only on the `FUEL`th call in a row with the clock stopped.
- `a_stopped_clock_ends_the_wait`: from every state, `FUEL` calls in a row at a clock that does
  not move past the last reading do not all say "again". Fuel: `65536`, one turn of `ends`'s
  loop per call, and the loop ends by the `65536 - still`th turn.

Neither panics: both statements say the function returns `Result.ok true`.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.Wait.RetryProofs

open blaze_core.wait.retry

/-- The literal `65536 : u32` (`FUEL`) is its value. -/
theorem u32_fuel : (65536 : U32).val = 65536 :=
  Scalar.ofNat_val .U32 65536 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- The literal `1 : u32` is one. -/
theorem u32_one : (1 : U32).val = 1 :=
  Scalar.ofNat_val .U32 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- The literal `0 : u32` is zero. -/
theorem u32_zero : (0 : U32).val = 0 :=
  Scalar.ofNat_val .U32 0 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- Whether the deadline `d` (none: no limit) has passed at `now`: what `steps` computes as
`passed`, and what `Retry::after`'s `is_some_and` computes. -/
def passedAt (d : Option Deadline) (now : core.time.Duration) : Bool :=
  match d with
  | some d => Deadline.passed d now
  | none => false

/-- The instance of `Option::is_some_and` in `Retry::after` is `passedAt`. -/
theorem is_some_and_eq (d : Option Deadline) (now : core.time.Duration) :
    core.option.Option.Deadline.is_some_and.closure0_wait_retry_Retry_after d { c0 := now } =
      passedAt d now := by
  cases d <;> rfl

/-- The instance of `Option::map` in `Retry::left` is what is left of the deadline at `at`. -/
theorem map_eq (d : Option Deadline) (at' : core.time.Duration) :
    core.option.Option.Deadline.map.Duration_closure0_wait_retry_Retry_left d { c0 := at' } =
      d.map (fun x => Deadline.left x at') := by
  cases d <;> rfl

/-- `still + 1`, in range. -/
def bump (s : U32) : U32 := Scalar.clamp .U32 (s.val + 1)

/-- `bump` adds one below `FUEL`. -/
theorem bump_val (s : U32) (h : s.val < 65536) : (bump s).val = s.val + 1 := by
  unfold bump
  rw [Scalar.clamp_val]
  have h0 := s.hmin
  simp only [ScalarTy.min] at h0
  have h1 : ¬ s.val + 1 < ScalarTy.min .U32 := by simp only [ScalarTy.min]; omega
  have h2 : ¬ ScalarTy.max .U32 < s.val + 1 := by simp only [ScalarTy.max]; omega
  simp [h1, h2]

/-- `still + 1` does not overflow below `FUEL`, and is `bump still`. -/
theorem add_one_eq (s : U32) (h : s.val < 65536) :
    Scalar.add s (1 : U32) = Result.ok (bump s) := by
  have h0 := s.hmin
  simp only [ScalarTy.min] at h0
  unfold Scalar.add
  rw [u32_one, Scalar.checked_ok .U32 _
    ⟨by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩]
  exact congrArg Result.ok (Scalar.ext (bump_val s h).symm)

/-- `now > at` is false exactly when `now <= at`. -/
theorem le_of_not_gt (a b : core.time.Duration) (h : core.time.Duration.gt a b = false) :
    core.time.Duration.le a b = true :=
  (core.time.Duration.le_iff a b).2 ((core.time.Duration.gt_eq_false a b).1 h)

/-- A reading is not after itself. -/
theorem gt_self (a : core.time.Duration) : core.time.Duration.gt a a = false :=
  (core.time.Duration.gt_eq_false a a).2 (Int.le_refl _)

/-- `Retry::after` from a count below `FUEL`: the deadline when it has passed; else the count
restarts when the clock moved and grows by one when it did not; an error when it reaches `FUEL`,
else "again" with the reading kept. It never panics (the third arm of the `match` on `now > at`
is not taken). -/
theorem after_eq (r : Retry) (now : core.time.Duration) (h : r.still.val < 65536) :
    Retry.after r now =
      if passedAt r.deadline now then
        Result.ok (core.result.Result.Ok Next.Deadline, r)
      else if core.time.Duration.gt now r.«at» then
        Result.ok (core.result.Result.Ok Next.Again, { r with still := 0, «at» := now })
      else if 65536 ≤ r.still.val + 1 then
        Result.ok (core.result.Result.Err
          ({ what := r.what, why := Why.Stuck (bump r.still) now } : WaitError),
          { r with still := bump r.still })
      else
        Result.ok (core.result.Result.Ok Next.Again,
          { r with still := bump r.still, «at» := now }) := by
  unfold Retry.after
  rw [is_some_and_eq]
  by_cases hp : passedAt r.deadline now = true
  · simp [hp]
  · cases hg : core.time.Duration.gt now r.«at»
    · by_cases hb : 65536 ≤ r.still.val + 1
      · simp [hp, add_one_eq _ h, Scalar.ge, u32_fuel, bump_val _ h, hb]
      · simp [hp, add_one_eq _ h, Scalar.ge, u32_fuel, bump_val _ h, hb]
    · simp [hp, Scalar.ge, u32_fuel, u32_zero]

/-- The loop of `ends` at a clock that has not moved past the retry's last reading: with a
count below `FUEL`, no more calls counted than the retry's count, and more fuel than turns
left before the count reaches `FUEL`, it returns `true` (the deadline passed or the wait
failed), never `false`, and neither panics nor runs out of fuel. -/
theorem loop_spec (now : core.time.Duration) : ∀ (fuel : Nat) (r : Retry) (calls : U32),
    calls.val ≤ r.still.val → r.still.val < 65536 → core.time.Duration.gt now r.«at» = false →
    65535 - r.still.val < (fuel : Int) →
    ends.loop_0 fuel r now calls = Result.ok true := by
  intro fuel
  induction fuel with
  | zero => intro r calls _ _ _ hf; omega
  | succ n ih =>
    intro r calls hc hs hg hf
    have hc0 := calls.hmin
    simp only [ScalarTy.min] at hc0
    have hlt : Scalar.lt calls (65536 : U32) = true := by
      simp only [Scalar.lt, u32_fuel, decide_eq_true_eq]; omega
    by_cases hp : passedAt r.deadline now = true
    · simp [ends.loop_0, hlt, after_eq r now hs, hp]
    · by_cases hb : 65536 ≤ r.still.val + 1
      · simp [ends.loop_0, hlt, after_eq r now hs, hp, hg, hb]
      · have hc' := add_one_eq calls (by omega)
        have hn := ih { r with still := bump r.still, «at» := now } (bump calls)
          (by simp only [bump_val _ hs, bump_val calls (by omega)]; omega)
          (by simp only [bump_val _ hs]; omega) (gt_self now)
          (by simp only [bump_val _ hs]; omega)
        simp [ends.loop_0, hlt, after_eq r now hs, hp, hg, hb, hc', hn]

end BlazeCore.Wait.RetryProofs

open BlazeCore.Wait.RetryProofs

/-- One step of a wait from every state with a count below `FUEL` is what `steps` says. -/
theorem blaze_core.wait.retry.a_retry_waits_what_is_left.spec.proof :
    blaze_core.wait.retry.a_retry_waits_what_is_left.spec.statement := by
  intro deadline at' still now
  unfold blaze_core.wait.retry.a_retry_waits_what_is_left
  by_cases hs : 65536 ≤ still.val
  · simp [Scalar.ge, u32_fuel, hs]
  · have hs' : still.val < 65536 := by omega
    have hr := after_eq ⟨Str.ofBytes [97, 32, 119, 97, 105, 116], deadline, at', still⟩ now hs'
    have hn : Scalar.ge still (65536 : U32) = false := by
      simp only [Scalar.ge, u32_fuel, decide_eq_false_iff_not]; omega
    simp only [hn, Bool.false_eq_true, ↓reduceIte, blaze_core.wait.retry.Retry.new]
    unfold blaze_core.wait.retry.steps
    rw [hr]
    cases deadline with
    | none =>
      cases hg : core.time.Duration.gt now at'
      · have hle := le_of_not_gt _ _ hg
        by_cases hb : 65536 ≤ still.val + 1
        · simp [passedAt, hb, hle, add_one_eq _ hs', Scalar.ge, u32_fuel, bump_val _ hs']
        · simp [passedAt, hb, Scalar.lt, u32_fuel, bump_val _ hs',
            blaze_core.wait.retry.Retry.left, map_eq, blaze_core.wait.retry.left_at]
      · simp [passedAt, Scalar.lt, u32_fuel, u32_zero,
          blaze_core.wait.retry.Retry.left, map_eq, blaze_core.wait.retry.left_at]
    | some d =>
      cases hp : blaze_core.wait.retry.Deadline.passed d now
      · cases hg : core.time.Duration.gt now at'
        · have hle := le_of_not_gt _ _ hg
          by_cases hb : 65536 ≤ still.val + 1
          · simp [passedAt, hp, hb, hle, add_one_eq _ hs', Scalar.ge, u32_fuel, bump_val _ hs']
          · simp [passedAt, hp, hb, Scalar.lt, u32_fuel, bump_val _ hs',
              blaze_core.wait.retry.Retry.left, map_eq, blaze_core.wait.retry.left_at]
        · simp [passedAt, hp, Scalar.lt, u32_fuel, u32_zero,
            blaze_core.wait.retry.Retry.left, map_eq, blaze_core.wait.retry.left_at]
      · simp [passedAt, hp]

/-- Fuel `65536`: `ends`'s loop returns `true` by its `65536 - still`th turn. -/
theorem blaze_core.wait.retry.a_stopped_clock_ends_the_wait.spec.proof :
    blaze_core.wait.retry.a_stopped_clock_ends_the_wait.spec.statement := by
  intro deadline at' still now
  refine ⟨65536, ?_⟩
  unfold blaze_core.wait.retry.a_stopped_clock_ends_the_wait
  by_cases hs : 65536 ≤ still.val
  · simp [Scalar.ge, u32_fuel, hs]
  · have hs' : still.val < 65536 := by omega
    have h0 := still.hmin
    simp only [ScalarTy.min] at h0
    have hn : Scalar.ge still (65536 : U32) = false := by
      simp only [Scalar.ge, u32_fuel, decide_eq_false_iff_not]; omega
    cases hg : core.time.Duration.gt now at'
    · have hl := loop_spec now 65536 ⟨Str.ofBytes [97, 32, 119, 97, 105, 116], deadline, at', still⟩
        0 (by show (0 : U32).val ≤ still.val; rw [u32_zero]; omega) hs' hg
        (by show 65535 - still.val < ((65536 : Nat) : Int); omega)
      simp [hn, blaze_core.wait.retry.ends, blaze_core.wait.retry.Retry.new, hl]
    · simp [hn]

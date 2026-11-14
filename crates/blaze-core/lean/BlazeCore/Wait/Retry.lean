import Blaze.Auto
import BlazeModel.blaze_core.wait.retry.a_retry_waits_what_is_left
import BlazeModel.blaze_core.wait.retry.a_stopped_clock_ends_the_wait
import BlazeModel.blaze_core.wait.retry.a_wait_ends_by_its_deadline

/-!
`blaze_core::wait::retry` (crates/blaze-core/src/wait/retry.rs): the retry step of every
blocking wait, proved over its generated models.

- `a_retry_waits_what_is_left`: from every state of a wait, one step after a call that was not
  ready ends exactly when the deadline has passed, gives the next call what is left of the
  deadline at the reading, and fails only on the `FUEL`th call in a row with the clock stopped.
- `a_stopped_clock_ends_the_wait`: from every state, `FUEL` calls in a row at a clock that does
  not move past the last reading do not all say "again". Fuel: `65536`, one turn of `ends`'s
  loop per call, and the loop ends by the `65536 - still`th turn.
- `a_wait_ends_by_its_deadline`: a whole wait driven by any list of answers the blocking world
  allows ends no later than its deadline plus `late`, and ends at the deadline only when the
  event could not have come by it. Fuel: the number of answers plus two, one turn of `drive`'s
  loop per answer.

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

namespace BlazeCore.Wait.RetryProofs

open blaze_core.wait.retry

/-- What `Blocking::allows` says of an answer it allows: the answer ends at `read`, without
overflow, after a pause that is not zero and not past `late`, starting no earlier than the
world's clock; with a limit, no later than the limit plus `late`; and an answer other than
`Ready` comes before the event. -/
theorem allows_spec (w : blaze_core.world.blocking.Blocking) (left : Option core.time.Duration)
    (a : blaze_core.world.blocking.Answer)
    (h : blaze_core.world.blocking.Blocking.allows w left a = true) :
    ∃ read, core.time.Duration.checked_add a.«at» a.after = some read ∧
      0 < a.after.toNanos ∧ a.after.toNanos ≤ w.late.toNanos ∧
      w.now.toNanos ≤ a.«at».toNanos ∧
      (∀ l, left = some l → read.toNanos ≤
        (core.time.Duration.saturating_add (core.time.Duration.saturating_add w.now l)
          w.late).toNanos) ∧
      (a.ended ≠ blaze_core.world.blocking.Ended.Ready →
        ∀ e, w.event = some e → a.«at».toNanos < e.toNanos) := by
  unfold blaze_core.world.blocking.Blocking.allows at h
  split at h
  next read hc =>
    refine ⟨read, hc, ?_⟩
    cases hz : core.time.Duration.is_zero a.after <;>
      cases hg : core.time.Duration.gt a.after w.late <;>
      cases hl : core.time.Duration.lt a.«at» w.now <;> simp [hz, hg, hl] at h
    have h1 : 0 < a.after.toNanos := by
      have := (core.time.Duration.toNanos_bounds a.after).1
      have : a.after.toNanos ≠ 0 := fun h0 => by
        rw [(core.time.Duration.is_zero_iff _).2 h0] at hz
        cases hz
      omega
    have h2 := (core.time.Duration.gt_eq_false _ _).1 hg
    have h3 := (core.time.Duration.lt_eq_false _ _).1 hl
    refine ⟨h1, h2, h3, ?_⟩
    cases left with
    | none =>
      refine ⟨fun l hl' => (by cases hl'), ?_⟩
      intro hne e he
      cases hend : a.ended <;> simp [he, hend] at h hne <;> omega
    | some l =>
      simp at h
      obtain ⟨hr, hm⟩ := h
      refine ⟨fun l' hl' => ?_, ?_⟩
      · cases hl'
        simp only [core.time.Duration.saturating_add_toNanos]
        exact hr
      · intro hne e he
        cases hend : a.ended <;> simp [he, hend] at hm hne <;> omega
  next => simp at h

/-- `saturating_add` is `checked_add` where that does not overflow. -/
theorem saturating_add_of_checked (a b r : core.time.Duration)
    (h : core.time.Duration.checked_add a b = some r) :
    core.time.Duration.saturating_add a b = r := by
  unfold core.time.Duration.saturating_add
  rw [h]

/-- `i + 1` does not overflow below `usize::MAX`, and is `i`'s value plus one. -/
theorem usize_add_one (i : Usize) (h : i.val < 18446744073709551615) :
    ∃ j : Usize, Scalar.add i (1 : Usize) = Result.ok j ∧ j.val = i.val + 1 := by
  have h1 : (1 : Usize).val = 1 :=
    Scalar.ofNat_val .Usize 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  have h0 := i.hmin
  simp only [ScalarTy.min] at h0
  refine ⟨⟨i.val + 1, by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩,
    ?_, rfl⟩
  unfold Scalar.add
  rw [h1]
  exact Scalar.checked_ok .Usize _
    ⟨by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩

/-- From a clock no later than the deadline `d`, the clock plus what is left of `d` is `d`, so
the limit a call is given, plus `late`, is `d` plus `late`. -/
theorem limit_eq (now d late : core.time.Duration) (h : now.toNanos ≤ d.toNanos) :
    (core.time.Duration.saturating_add
      (core.time.Duration.saturating_add now (core.time.Duration.saturating_sub d now)) late) =
      core.time.Duration.saturating_add d late := by
  have hb := core.time.Duration.toNanos_bounds d
  have hd : core.time.Duration.saturating_add now (core.time.Duration.saturating_sub d now) = d := by
    apply core.time.Duration.ext_toNanos
    simp only [core.time.Duration.saturating_add_toNanos,
      core.time.Duration.saturating_sub_toNanos, ite_eq_left h]
    rw [ite_eq_left (by omega)]
    omega
  rw [hd]

/-- How a wait the loop of `drive` ran ended, when it was not by the answers running out or
being refused: `Ready` at a clock no later than the deadline plus `late`, or `Deadline` at a
clock between the deadline and the deadline plus `late`, with the event (if any) not able to
come by the deadline. -/
def Good (deadline : Option Deadline) (event : Option core.time.Duration)
    (late : core.time.Duration) (waited : core.result.Result Waited.unit WaitError)
    («end» : core.time.Duration) : Prop :=
  (waited = core.result.Result.Ok (Waited.unit.Ready ()) ∧
    ∀ d : Deadline, deadline = some d →
      «end».toNanos ≤ (core.time.Duration.saturating_add d.f0 late).toNanos) ∨
  (waited = core.result.Result.Ok Waited.unit.Deadline ∧
    ∃ d : Deadline, deadline = some d ∧ d.f0.toNanos ≤ «end».toNanos ∧
      «end».toNanos ≤ (core.time.Duration.saturating_add d.f0 late).toNanos ∧
      ∀ e, event = some e → d.f0.toNanos < e.toNanos + late.toNanos)

/-- The loop of `drive` over a kernel of answers, from a retry whose last reading is the
world's clock, no later than the deadline, and a count below `FUEL`, with more fuel than
answers left: it returns, never fails, and either the answers ran out or were refused
(`stopped`), or it ended as `Good` says. -/
theorem drive_spec (deadline : Option Deadline) : ∀ (fuel : Nat) (retry : Retry) (kernel : Answers),
    retry.deadline = deadline → retry.«at» = kernel.world.now → retry.still.val < 65536 →
    (∀ d : Deadline, deadline = some d → kernel.world.now.toNanos ≤ d.f0.toNanos) →
    kernel.next.val ≤ kernel.answers.val.length →
    (kernel.answers.val.length : Int) - kernel.next.val < fuel →
    ∃ waited kernel', drive.unit_Answers.loop_0 fuel retry kernel = Result.ok (waited, kernel') ∧
      (kernel'.stopped = true ∨
        Good deadline kernel.world.event kernel.world.late waited kernel'.world.now) := by
  intro fuel
  induction fuel with
  | zero => intro retry kernel _ _ _ _ hn hf; simp at hf; omega
  | succ n ih =>
    intro retry kernel hd ha hs hD hn hf
    obtain ⟨lft, hleft, hlv⟩ : ∃ l, Retry.left retry = (l, retry) ∧
        ∀ d : Deadline, deadline = some d →
          l = some (core.time.Duration.saturating_sub d.f0 kernel.world.now) :=
      ⟨retry.deadline.map (fun x => Deadline.left x retry.«at»),
        by simp [Retry.left, map_eq], fun d hdd => by rw [hd, hdd, ha]; rfl⟩
    by_cases hge : Scalar.ge kernel.next (Slice.len kernel.answers) = true
    · refine ⟨core.result.Result.Ok (Waited.unit.Ready ()), { kernel with stopped := true },
        ?_, Or.inl rfl⟩
      simp [drive.unit_Answers.loop_0, hleft, Answers.Kernel.unit.once, hge]
    · have hlt : kernel.next.val < kernel.answers.val.length := by
        have h' : ¬ ((Slice.len kernel.answers).val ≤ kernel.next.val) := fun h =>
          hge (by unfold Scalar.ge; exact decide_eq_true h)
        rw [Slice.len_val] at h'
        omega
      have hn0 := kernel.next.hmin
      simp only [ScalarTy.min] at hn0
      have hl64 := kernel.answers.hlen
      obtain ⟨a, hidx⟩ : ∃ a, Slice.index kernel.answers kernel.next = Result.ok a :=
        ⟨_, Slice.index_ok _ _ (by omega)⟩
      obtain ⟨j, hj, hjv⟩ := usize_add_one kernel.next (by omega)
      have hge' : Scalar.ge kernel.next (Slice.len kernel.answers) = false := by
        unfold Scalar.ge
        exact decide_eq_false (by rw [Slice.len_val]; omega)
      cases hal : blaze_core.world.blocking.Blocking.allows kernel.world lft a
      · refine ⟨core.result.Result.Ok (Waited.unit.Ready ()),
          { kernel with next := j, stopped := true }, ?_, Or.inl rfl⟩
        simp [drive.unit_Answers.loop_0, hleft, Answers.Kernel.unit.once, hge', hidx, hj, hal]
      · obtain ⟨read, hc, hpos, hlate, hnow, hlim, hcame⟩ := allows_spec _ _ _ hal
        have hans : blaze_core.world.blocking.Blocking.answer kernel.world a =
            { kernel.world with now := read } := by
          simp [blaze_core.world.blocking.Blocking.answer, saturating_add_of_checked _ _ _ hc]
        have hread := (core.time.Duration.checked_add_eq_some _ _ _).1 hc
        have hbound : ∀ d : Deadline, deadline = some d →
            read.toNanos ≤ (core.time.Duration.saturating_add d.f0 kernel.world.late).toNanos := by
          intro d hdd
          have := hlim _ (hlv d hdd)
          rwa [limit_eq _ _ _ (hD d hdd)] at this
        have hpre := hcame
        cases hend : a.ended
        case Ready =>
          refine ⟨core.result.Result.Ok (Waited.unit.Ready ()),
            { kernel with next := j, world := { kernel.world with now := read } }, ?_,
            Or.inr (Or.inl ⟨rfl, hbound⟩)⟩
          simp [drive.unit_Answers.loop_0, hleft, Answers.Kernel.unit.once, hge', hidx, hj, hal,
            hans, hend]
        all_goals
          have hcame' : ∀ e, kernel.world.event = some e → a.«at».toNanos < e.toNanos :=
            hpre (by rw [hend]; intro h; cases h)
          by_cases hp : passedAt retry.deadline read = true
          · refine ⟨core.result.Result.Ok Waited.unit.Deadline,
              { kernel with next := j, world := { kernel.world with now := read } }, ?_,
              Or.inr (Or.inr ⟨rfl, ?_⟩)⟩
            · simp [drive.unit_Answers.loop_0, hleft, Answers.Kernel.unit.once, hge', hidx, hj,
                hal, hans, hend, Answers.Kernel.unit.now, after_eq retry read hs, hp,
                core.result.Result.Next_WaitError.Try.branch]
            · cases deadline with
              | none => rw [hd] at hp; simp [passedAt] at hp
              | some d =>
                rw [hd] at hp
                simp [passedAt, Deadline.passed] at hp
                refine ⟨d, rfl, hp, hbound d rfl, fun e he => ?_⟩
                have := hcame' e he
                omega
          · have hp' : passedAt retry.deadline read = false := by simpa using hp
            have hgt : core.time.Duration.gt read retry.«at» = true := by
              rw [core.time.Duration.gt_iff, ha]
              omega
            obtain ⟨w, k', heq, hres⟩ := ih { retry with still := 0, «at» := read }
              { kernel with next := j, world := { kernel.world with now := read } }
              hd rfl (by simp only [u32_zero]; omega)
              (fun d hdd => by
                rw [hd, hdd] at hp'
                simp [passedAt, Deadline.passed] at hp'
                show read.toNanos ≤ _
                omega)
              (by show j.val ≤ (kernel.answers.val.length : Int); omega)
              (by show (kernel.answers.val.length : Int) - j.val < (n : Int); omega)
            refine ⟨w, k', ?_, hres⟩
            simp [drive.unit_Answers.loop_0, hleft, Answers.Kernel.unit.once, hge', hidx, hj,
              hal, hans, hend, Answers.Kernel.unit.now, after_eq retry read hs, hp', hgt,
              core.result.Result.Next_WaitError.Try.branch, heq]

/-- The literal `0 : usize` is zero. -/
theorem usize_zero : (0 : Usize).val = 0 :=
  Scalar.ofNat_val .Usize 0 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- `waits` from a retry started at `start`, no later than its deadline, with more fuel than
answers: it returns `true`. -/
theorem waits_eq (fuel : Nat) (retry : Retry) (start : core.time.Duration)
    (deadline : Option Deadline) (event : Option core.time.Duration) (late : core.time.Duration)
    (answers : Vec blaze_core.world.blocking.Answer)
    (hd : retry.deadline = deadline) (ha : retry.«at» = start) (hs : retry.still.val < 65536)
    (hD : ∀ d : Deadline, deadline = some d → start.toNanos ≤ d.f0.toNanos)
    (hf : (answers.val.length : Int) < fuel) :
    waits fuel retry start deadline event late answers = Result.ok true := by
  obtain ⟨w, k', heq, hres⟩ := drive_spec deadline fuel retry
    { world := { now := start, event := event, late := late }, answers := answers,
      next := 0, stopped := false }
    hd ha hs hD (by show (0 : Usize).val ≤ (answers.val.length : Int); rw [usize_zero]; omega)
    (by show (answers.val.length : Int) - (0 : Usize).val < (fuel : Int); rw [usize_zero]; omega)
  dsimp only at hres
  unfold waits
  simp only [drive.unit_Answers, heq, Result.bind_ok]
  cases hst : k'.stopped
  · rcases hres with hres | hgood
    · rw [hst] at hres; cases hres
    · rcases hgood with ⟨rfl, hb⟩ | ⟨rfl, d, rfl, h1, h2, h3⟩
      · cases deadline with
        | none => simp
        | some d =>
          have := hb d rfl
          simp only [core.time.Duration.saturating_add_toNanos] at this
          simp [Deadline.get, this]
      · simp only [core.time.Duration.saturating_add_toNanos] at h2
        cases event with
        | none => simp [Deadline.get, Deadline.passed, h1, h2]
        | some e =>
          have he := h3 e rfl
          cases hc : core.time.Duration.checked_add e late with
          | none => simp [Deadline.get, Deadline.passed, h1, h2, hc]
          | some b =>
            have hb := ((core.time.Duration.checked_add_eq_some _ _ _).1 hc).2
            have hlt : d.f0.toNanos < b.toNanos := by omega
            simp [Deadline.get, Deadline.passed, h1, hc, Int.not_lt.2 h2,
              (core.time.Duration.le_eq_false _ _).2 hlt]
  · simp

end BlazeCore.Wait.RetryProofs

/-- Fuel: one more than the answers: each turn of `drive`'s loop takes an answer or ends. -/
theorem blaze_core.wait.retry.a_wait_ends_by_its_deadline.spec.proof :
    blaze_core.wait.retry.a_wait_ends_by_its_deadline.spec.statement := by
  intro start wait event late answers
  refine ⟨answers.val.length + 2, ?_⟩
  unfold blaze_core.wait.retry.a_wait_ends_by_its_deadline
  refine waits_eq _ _ _ _ _ _ _ rfl rfl
    (by simp only [blaze_core.wait.retry.Retry.new, u32_zero]; omega) ?_ (by omega)
  intro d hdd
  cases wait with
  | none => cases hdd
  | some w =>
    cases hdd
    have := (core.time.Duration.toNanos_bounds start).2
    have := (core.time.Duration.toNanos_bounds w).1
    simp only [blaze_core.wait.retry.Deadline.after, core.time.Duration.saturating_add_toNanos]
    split <;> omega

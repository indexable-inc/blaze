import BlazeCore.Wait.ParkingDeadline

/-!
A park says `Ready` exactly for a token (`blaze_core::wait::parking::a_park_is_ready_exactly_for_a_token`,
over `answers`), on every schedule, every script of clock readings, every choice of the
kernel's and every deadline or none, over the models blaze emits from the real `park` (on the
timed kernel and the world's clock) and `unpark`, called as `keeps`'s closures call them. The
program points and their shapes are `ParkingTimeLib`'s.

- With a token in the word as the park begins (`token_run`): the park's first compare takes it
  and it returns `Ready`; an unpark before that finds the word not `PARKED` and wakes no one.
  `TGood` lists the five reachable combinations; at the end the park is running or returned
  `Ready`, never asleep.
- With the word empty and no step of the unpark's (`alone_run`): the unpark stays at its start,
  so the word never holds a token, and every path to `Ready` needs one. `AInv` is `Good` of
  `ParkingTime` with the unpark at its start and the park not at `Ready`, kept by the park's
  steps with any pick (`two_exec_first`).
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic
set_option linter.unusedVariables false

namespace BlazeCore.Wait.ParkingTokenProof

open blaze_core.wait.retry blaze_core.wait.parking
open BlazeCore.Wait.ParkingTimeProof BlazeCore.Wait.ParkingDeadlineProof
open BlazeCore.Wait.RetryProofs (passedAt bump bump_val)
open BlazeCore.Wait.ParkingProof (W whatP Res readyR u32_0 u32_1 u32_P wrap_E wrap_N wrap_P)

/-! A token waiting. -/

/-- Every combination reachable from the word holding a token. -/
inductive TGood : Pt → UPt → List Int → Prop
  | start_t (k : Nat) : TGood (.start k) .start [1]
  | start_d (k : Nat) : TGood (.start k) .ret [1]
  | ret_0u0 : TGood (.ret readyR) .start [0]
  | ret_0uD : TGood (.ret readyR) .ret [0]
  | ret_1uD : TGood (.ret readyR) .ret [1]

def TInv (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World) : Prop :=
  ∃ pt up, a = parkAt d pt ∧ b = unparkAt up ∧ w.OneCell 0 ∧ TGood pt up w.cells

set_option hygiene false in
/-- After a step: the point the thread is at and the combination after it. -/
local macro "tclose" : tactic => `(tactic| (
  simp [wrap_E, wrap_N, wrap_P, u32_0, u32_1, u32_P, Op.sleeps, uat1_E, uat1_N, uat1_P]
  unfold TInv
  refine ⟨_, _, rfl, rfl, ?_, ?_⟩
  · first | exact World.OneCell.touched hu | exact hu
  · simp [hc, u32_0, u32_1]
    constructor))

set_option hygiene false in
/-- A thread that has returned stays. -/
local macro "tkeep" : tactic => `(tactic| (
  unfold TInv
  refine ⟨_, _, rfl, rfl, hu, ?_⟩
  rw [hc]
  constructor))

theorem token_first (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World)
    (pick : Nat) (h : TInv d (n + 1) a b w) :
    Advance 1 pick a w (fun a' w' => TInv d n a' b w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  generalize hc : w.cells = cs at hg
  cases hg with
  | start_t k | start_d k =>
    exact .of_step (start_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by tclose)
  | ret_0u0 | ret_0uD | ret_1uD => exact .of_done (ret_shape ..) (by tkeep)

theorem token_second (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World)
    (pick : Nat) (h : TInv d (n + 1) a b w) :
    Advance 2 pick b w (fun b' w' => TInv d n a b' w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  generalize hc : w.cells = cs at hg
  cases hg with
  | start_t k | ret_0u0 =>
    exact .of_step ustart_shape (World.step_swap (World.get_single hc) hu.fits) (by tclose)
  | start_d k | ret_0uD | ret_1uD => exact .of_done uret_shape (by tkeep)

/-- At the end: the park running or returned `Ready`. -/
theorem token_last (d : Option Deadline) (a : Prog Res) (b : Prog Unit) (w : World)
    (h : TInv d 0 a b w) :
    ∃ ea eb w', (two [] a b).exec w = .ok ((ea, eb), w') ∧
      ea ≠ .Asleep ∧ (∀ r, ea = .Done r → r = .Ok (.Ready ())) := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  obtain ⟨eb, hb⟩ := unpark_ends up w
  generalize hc : w.cells = cs at hg
  cases hg with
  | start_t k | start_d k =>
    exact ⟨_, _, _, two_nil_ok (park_ends d w _ (fun _ _ _ h => nomatch h) (fun _ h => nomatch h))
      hb, (fun h => nomatch h), (fun _ h => nomatch h)⟩
  | ret_0u0 | ret_0uD | ret_1uD =>
    exact ⟨_, _, _, two_nil_ok rfl hb, (fun h => nomatch h), (fun r h => by cases h; rfl)⟩

/-- With a token in the word as the park begins: the park, with fuel one more than the
schedule's steps, and the unpark, as `keeps`'s closures call them, end; the park is not asleep,
and where it returned it said `Ready`. -/
theorem token_run (d : Option Deadline) (schedule : List Step) (readings : List Int) :
    ∃ ea eb w',
      (two schedule
          (keeps.Timed.closure0.call (schedule.length + 1) { c0 := W, c1 := KT, c2 := d })
          (keeps.Timed.closure1.call { c0 := W, c1 := KT })).exec
        ⟨[1], [], [], [], false, 0, readings⟩ = .ok ((ea, eb), w') ∧
      ea ≠ .Asleep ∧ (∀ r, ea = .Done r → r = .Ok (.Ready ())) :=
  two_ends (fun n => TInv d n) (token_first d) (token_second d)
    (fun ea _ _ => ea ≠ .Asleep ∧ (∀ r, ea = .Done r → r = .Ok (.Ready ()))) (token_last d)
    schedule _ _ _ ⟨.start _, .start, rfl, rfl, Or.inl rfl, .start_t _⟩

/-! No token and no unpark. -/

/-- `Good` with the unpark at its start, the word not holding a token, and the park not at
`Ready`. -/
def AInv (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World) : Prop :=
  ∃ pt, a = parkAt d pt ∧ b = unparkAt .start ∧ w.OneCell 0 ∧
    Good d n pt .start w.cells w.asleep w.woken w.clock ∧ w.cells ≠ [1] ∧ pt ≠ .ret readyR

set_option hygiene false in
/-- The combination after a step, once the park's point after it is named. -/
local macro "afin" : tactic => `(tactic| (
  unfold AInv
  refine ⟨_, rfl, rfl, ?_, ?_, ?_, ?_⟩
  · first | exact World.OneCell.touched hu | exact hu
  · simp [hc, hs, hk, hcl, u32_0, u32_1, u32_P]
    constructor <;> first | omega | assumption | (simp only [u32_0]; omega)
  · simp [hc, u32_0, u32_P, Pk]
  · intro hr'; cases hr'))

set_option hygiene false in
/-- After a step of the park: the point it is at and the combination after it. -/
local macro "aclose" : tactic => `(tactic| (
  simp [wrap_E, wrap_N, wrap_P, u32_0, u32_1, u32_P, Op.sleeps, afterNext, at6_N d, at6_P d,
    at10_N d, at10_P d]
  afin))

theorem alone_first (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World)
    (pick : Nat) (h : AInv d (n + 1) a b w) :
    Advance 1 pick a w (fun a' w' => AInv d n a' b w') := by
  obtain ⟨pt, rfl, rfl, hu, hg, hw1, hr⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | start_t k _ hn => exact absurd hc hw1
  | start_e k _ hn =>
    exact .of_step (start_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by aclose)
  | second_e k _ hn =>
    exact .of_step (second_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by aclose)
  | now0_u0 k _ hn =>
    obtain ⟨c', hst⟩ := World.step_now_move (w := w) (who := 1) (pick := pick)
    exact .of_step (now0_shape ..) hst (by aclose)
  | wait_u0 k t s _ hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    by_cases hpd : Threads.passed c (untilNs d) = true
    · exact .of_step (wait_shape ..) (World.step_wait_until_timed_out_at_once (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) u32_P.symm (by rw [hcl]; exact hpd)) (by aclose)
    · have hpd' : Threads.passed c (untilNs d) = false := by simpa using hpd
      exact .of_step (wait_shape ..) (World.step_wait_until_sleeps (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) u32_P.symm (by rw [hcl]; exact hpd')) (by aclose)
  | wait_u0_asleep k t s _ hn hs' hpd =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    have hpd' : Threads.passed w.clock (untilNs d) = false := by rw [hcl]; exact hpd
    by_cases h1 : pick = 1
    · exact .of_step (wait_shape ..) (World.step_wait_until_spurious (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) hpd' h1) (by aclose)
    · cases hd : d with
      | none =>
        have hpc : Threads.passed c (untilNs none) = false := by rw [← hd]; exact hpd
        have hpw : Threads.passed w.clock (untilNs none) = false := by rw [hcl]; exact hpc
        exact .of_step (wait_shape ..) (World.step_wait_until_stays (World.get_single hc)
          hu.fits (by simp [hk]) (by simp [hs]) hpw h1 (fun _ => rfl)) (by aclose)
      | some dd =>
        have hpc : Threads.passed c (untilNs (some dd)) = false := by rw [← hd]; exact hpd
        have hpw : Threads.passed w.clock (untilNs (some dd)) = false := by rw [hcl]; exact hpc
        by_cases h2 : pick = 2
        · have hlt : w.clock < (Deadline.get dd).toNanos := by
            have := hpw
            simp only [untilNs, Threads.passed, decide_eq_false_iff_not] at this
            omega
          exact .of_step (wait_shape ..) (World.step_wait_until_told (World.get_single hc)
            hu.fits (by simp [hk]) (by simp [hs]) hlt h2) (by aclose)
        · exact .of_step (wait_shape ..) (World.step_wait_until_stays (World.get_single hc)
            hu.fits (by simp [hk]) (by simp [hs]) hpw h1 (fun h => absurd h h2)) (by aclose)
  | after_u0 k t s wkk _ hn hs' =>
    cases wkk <;>
      exact .of_step (after_shape ..) (World.step_cas_err (World.get_single hc) hu.fits
        (by decide)) (by aclose)
  | now1_u0 k t s _ hn hs' =>
    obtain ⟨c', hst⟩ := World.step_now_move (w := w) (who := 1) (pick := pick)
    refine .of_step (now1_shape ..) hst ?_
    simp only [Op.sleeps, Bool.false_eq_true, ite_false]
    rw [now1_next d k t s _ hs']
    generalize core.time.Duration.ofNanosSat c' = t6
    by_cases h1 : passedAt d t6 = true
    · simp only [nowNext, h1, ↓reduceIte]; afin
    · by_cases h2 : core.time.Duration.gt t6 t = true
      · simp only [nowNext, h1, h2, Bool.false_eq_true, ↓reduceIte]; afin
      · by_cases h3 : 65536 ≤ s.val + 1
        · simp only [nowNext, h1, h2, h3, Bool.false_eq_true, ↓reduceIte]; afin
        · have hb : (bump s).val < 65536 := by rw [bump_val _ hs']; omega
          simp only [nowNext, h1, h2, h3, Bool.false_eq_true, ↓reduceIte]; afin
  | swapD_u0 _ =>
    exact .of_step (swapD_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by aclose)
  | swapE_u0 e _ =>
    exact .of_step (swapE_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by aclose)
  | ret_0u0 r _ =>
    refine .of_done (ret_shape ..) ⟨_, rfl, rfl, hu, ?_, hw1, hr⟩
    rw [hc, hs, hk, hcl]
    constructor

/-- At the end: the park has not returned `Ready`. -/
theorem alone_last (d : Option Deadline) (a : Prog Res) (b : Prog Unit) (w : World)
    (h : AInv d 0 a b w) :
    ∃ ea eb w', (two [] a b).exec w = .ok ((ea, eb), w') ∧ ea ≠ .Done (.Ok (.Ready ())) := by
  obtain ⟨pt, rfl, rfl, hu, hg, hw1, hr⟩ := h
  obtain ⟨eb, hb⟩ := unpark_ends .start w
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | wait_u0 k t s _ hn | wait_u0_asleep k t s _ hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    have hend := fun (v : Int) (hv : w.cells[0]? = some v) =>
      ended_exec_wait_until (who := 1) (t := 0) (k := 0) (e := (4294967295 : U32).val)
        (dl := untilNs d) (next := fun got => parkAt d (.after k t s (Woke.ofGot got))) hv
    refine ⟨_, _, _, two_nil_ok ((congrArg (fun p => (ended 1 p).exec w) (wait_shape d k t s)).trans
      (hend _ (World.get_single hc))) hb, fun he => ?_⟩
    split at he <;> cases he
  | ret_0u0 r _ =>
    exact ⟨_, _, _, two_nil_ok rfl hb, fun he => hr (by cases he; rfl)⟩
  | _ =>
    exact ⟨_, _, _, two_nil_ok (park_ends d w _ (fun _ _ _ h => nomatch h) (fun _ h => nomatch h))
      hb, (fun he => nomatch he)⟩

/-- With the word empty and no step of the unpark's in the schedule: the park, with fuel one
more than the schedule's steps, and the unpark, as `keeps`'s closures call them, end, and the
park has not returned `Ready`. -/
theorem alone_run (d : Option Deadline) (schedule : List Step) (readings : List Int)
    (h : ∀ s ∈ schedule, s.second = false) :
    ∃ ea eb w',
      (two schedule
          (keeps.Timed.closure0.call (schedule.length + 1) { c0 := W, c1 := KT, c2 := d })
          (keeps.Timed.closure1.call { c0 := W, c1 := KT })).exec
        ⟨[0], [], [], [], false, 0, readings⟩ = .ok ((ea, eb), w') ∧
      ea ≠ .Done (.Ok (.Ready ())) := by
  have hstart : AInv d schedule.length
      (keeps.Timed.closure0.call (schedule.length + 1) { c0 := W, c1 := KT, c2 := d })
      (keeps.Timed.closure1.call { c0 := W, c1 := KT })
      ⟨[0], [], [], [], false, 0, readings⟩ :=
    ⟨.start _, rfl, rfl, Or.inl rfl, .start_e _ _ (by omega), by simp, fun h => nomatch h⟩
  obtain ⟨a', b', w', hI, he⟩ := two_exec_first (AInv d) (alone_first d) schedule _ _ _ h hstart
  obtain ⟨ea, eb, w'', he', hne⟩ := alone_last d a' b' w' hI
  exact ⟨ea, eb, w'', he.trans he', hne⟩

end BlazeCore.Wait.ParkingTokenProof

import BlazeCore.Wait.ParkingTime
import BlazeModel.blaze_core.wait.parking.keeps.Timed.closure0.call
import BlazeModel.blaze_core.wait.parking.keeps.Timed.closure1.call

/-!
A park keeps its deadline (`blaze_core::wait::parking::a_park_keeps_its_deadline`, over
`keeps`), on every schedule, every script of clock readings, every choice of the kernel's and
every deadline or none, over the models blaze emits from the real `park` (on the timed kernel
and the world's clock) and `unpark`. The program points and their shapes are
`ParkingTimeLib`'s and the reachable combinations `ParkingTime`'s `Good`; `KInv` adds what the
two conclusions need, each kept by every step:

- the clock is never negative (it starts at 0, and only a reading at or after it, or the
  deadline it has not reached, moves it: `World.step_now_up`, `World.step_wait_until_told`);
- at each point from which the park returns `Ok(Deadline)` without reading the clock again
  (`marked`: after a wait that timed out, the swap it leaves with, the return itself) the clock
  has reached the deadline. It becomes true where a wait times out at once (the clock had
  reached it), where a sleeper is told its time ran out (the clock moves to the deadline) and
  where the retry finds the reading at or past it (`passedAt_ofNanosSat`).

A park asleep at the end has not reached its deadline: `ended` says `Asleep` only where the
wait would not return now, which includes that (`ended_exec_wait_until`).
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic
set_option linter.unusedVariables false

namespace BlazeCore.Wait.ParkingDeadlineProof

open blaze_core.wait.retry blaze_core.wait.parking
open BlazeCore.Wait.ParkingTimeProof
open BlazeCore.Wait.RetryProofs (passedAt bump bump_val)
open BlazeCore.Wait.ParkingProof (W whatP Res readyR u32_0 u32_1 u32_P wrap_E wrap_N wrap_P)

/-! The clock as the retry reads it and as the world compares it. -/

/-- A deadline has passed at the reading `c` (a clock, not negative) exactly when the world's
clock `c` has reached the deadline in nanoseconds. -/
theorem passed_ofNanosSat (dl : Deadline) (c : Int) (hc : 0 ≤ c) :
    Deadline.passed dl (core.time.Duration.ofNanosSat c) =
      Threads.passed c (some (Deadline.get dl).toNanos) := by
  have hn : (core.time.Duration.ofNanosSat c).toNanos =
      if c ≤ core.time.Duration.maxNanos then c else core.time.Duration.maxNanos := by
    split
    · exact core.time.Duration.ofNanosSat_toNanos c ⟨hc, by assumption⟩
    · rename_i hm
      have h0 : ¬ c < 0 := by omega
      have h1 : core.time.Duration.maxNanos < c := by omega
      simp only [core.time.Duration.ofNanosSat, h0, h1, ↓reduceDIte,
        core.time.Duration.MAX_toNanos]
  have hb := core.time.Duration.toNanos_bounds dl.f0
  have hg : Deadline.get dl = dl.f0 := rfl
  show core.time.Duration.ge (core.time.Duration.ofNanosSat c) dl.f0 =
    decide ((Deadline.get dl).toNanos ≤ c)
  rw [hg, Bool.eq_iff_iff, core.time.Duration.ge_iff, hn, decide_eq_true_eq]
  split <;> omega

/-- The retry's test of its deadline at a reading of the world's clock `c` is the world's. -/
theorem passedAt_ofNanosSat (d : Option Deadline) (c : Int) (hc : 0 ≤ c) :
    passedAt d (core.time.Duration.ofNanosSat c) = Threads.passed c (untilNs d) := by
  cases d with
  | none => rfl
  | some dl => exact passed_ofNanosSat dl c hc

/-! The invariant. -/

/-- The points from which the park returns `Ok(Deadline)` without reading the clock again. -/
def marked : Pt → Bool
  | .after _ _ _ .TimedOut => true
  | .swapD => true
  | .ret (.Ok .Deadline) => true
  | _ => false

/-- `Good`, a clock that is not negative, and at a marked point the deadline reached. -/
def KInv (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World) : Prop :=
  ∃ pt up, a = parkAt d pt ∧ b = unparkAt up ∧ w.OneCell 0 ∧
    Good d n pt up w.cells w.asleep w.woken w.clock ∧ 0 ≤ w.clock ∧
    (marked pt = true → Threads.passed w.clock (untilNs d) = true)

/-- What the end of a run says: `keeps`'s two conclusions, and the clock not negative. -/
def KPost (d : Option Deadline) (ea : Ended Res) (eb : Ended Unit) (w : World) : Prop :=
  (ea = .Done (.Ok .Deadline) → Threads.passed w.clock (untilNs d) = true) ∧
  (ea = .Asleep → Threads.passed w.clock (untilNs d) = false) ∧ 0 ≤ w.clock

set_option hygiene false in
/-- The combination after a step, once the thread's point after it is named. -/
local macro "kfin" : tactic => `(tactic| (
  unfold KInv
  refine ⟨_, _, rfl, rfl, ?_, ?_, ?_, ?_⟩
  · first | exact World.OneCell.touched hu | exact hu
  · simp [hc, hs, hk, hcl, u32_0, u32_1, u32_P]
    constructor <;> first | omega | assumption | (simp only [u32_0]; omega)
  · first | omega | (simp [h0]; done) | (simp; omega)
  · first
      | (intro hm'; cases hm'; done)
      | (simp [marked]; done)
      | (intro hm'; simpa using hD hm')
      | (intro hm'; simpa using hD rfl)
      | (intro hm'; simpa using hm)
      | (intro hm'; simp [untilNs, Threads.passed])))

set_option hygiene false in
/-- After a step: the point the thread is at and the combination after it. -/
local macro "kclose" : tactic => `(tactic| (
  simp [wrap_E, wrap_N, wrap_P, u32_0, u32_1, u32_P, Op.sleeps, afterNext, at6_N d, at6_P d,
    at10_N d, at10_P d, uat1_E, uat1_N, uat1_P]
  kfin))

set_option hygiene false in
/-- A thread that has returned stays. -/
local macro "kkeep" : tactic => `(tactic| (
  unfold KInv
  refine ⟨_, _, rfl, rfl, hu, ?_, h0, hD⟩
  rw [hc, hs, hk, hcl]
  constructor <;> first | omega | assumption))

theorem keeps_first (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World)
    (pick : Nat) (h : KInv d (n + 1) a b w) :
    Advance 1 pick a w (fun a' w' => KInv d n a' b w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg, h0, hD⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | start_e k _ hn =>
    exact .of_step (start_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by kclose)
  | start_t k _ hn =>
    exact .of_step (start_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by kclose)
  | start_d k _ hn =>
    exact .of_step (start_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by kclose)
  | second_e k _ hn =>
    exact .of_step (second_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by kclose)
  | second_d k _ hn =>
    exact .of_step (second_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by kclose)
  | store_d _ =>
    exact .of_step (store_shape ..) (World.step_store (World.get_single_length hc) hu.fits)
      (by kclose)
  | now0_u0 k _ hn | now0_u1 k _ hn | now0_uD k _ hn =>
    obtain ⟨c', hle, hst⟩ := World.step_now_up (w := w) (who := 1) (pick := pick)
    exact .of_step (now0_shape ..) hst (by kclose)
  | wait_u0 k t s _ hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    by_cases hpd : Threads.passed c (untilNs d) = true
    · have hm : Threads.passed w.clock (untilNs d) = true := by rw [hcl]; exact hpd
      exact .of_step (wait_shape ..) (World.step_wait_until_timed_out_at_once (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) u32_P.symm hm) (by kclose)
    · have hpd' : Threads.passed c (untilNs d) = false := by simpa using hpd
      exact .of_step (wait_shape ..) (World.step_wait_until_sleeps (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) u32_P.symm (by rw [hcl]; exact hpd')) (by kclose)
  | wait_u0_asleep k t s _ hn hs' hpd | wait_u1_asleep k t s _ hn hs' hpd =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    have hpd' : Threads.passed w.clock (untilNs d) = false := by rw [hcl]; exact hpd
    by_cases h1 : pick = 1
    · exact .of_step (wait_shape ..) (World.step_wait_until_spurious (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) hpd' h1) (by kclose)
    · cases hd : d with
      | none =>
        have hpc : Threads.passed c (untilNs none) = false := by rw [← hd]; exact hpd
        have hpw : Threads.passed w.clock (untilNs none) = false := by rw [hcl]; exact hpc
        exact .of_step (wait_shape ..) (World.step_wait_until_stays (World.get_single hc)
          hu.fits (by simp [hk]) (by simp [hs]) hpw h1 (fun _ => rfl)) (by kclose)
      | some dd =>
        have hpc : Threads.passed c (untilNs (some dd)) = false := by rw [← hd]; exact hpd
        have hpw : Threads.passed w.clock (untilNs (some dd)) = false := by rw [hcl]; exact hpc
        by_cases h2 : pick = 2
        · have hlt : w.clock < (Deadline.get dd).toNanos := by
            have := hpw
            simp only [untilNs, Threads.passed, decide_eq_false_iff_not] at this
            omega
          exact .of_step (wait_shape ..) (World.step_wait_until_told (World.get_single hc)
            hu.fits (by simp [hk]) (by simp [hs]) hlt h2) (by kclose)
        · exact .of_step (wait_shape ..) (World.step_wait_until_stays (World.get_single hc)
            hu.fits (by simp [hk]) (by simp [hs]) hpw h1 (fun h => absurd h h2)) (by kclose)
  | wait_u1 k t s _ hn hs' | wait_uD k t s _ hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_until_changed (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by rw [u32_P]; decide)) (by kclose)
  | wait_uD_woken k t s _ hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_until_woken (World.get_single hc) hu.fits
      (by simp [hk])) (by kclose)
  | after_u0 k t s wkk _ hn hs' =>
    cases wkk <;>
      exact .of_step (after_shape ..) (World.step_cas_err (World.get_single hc) hu.fits
        (by decide)) (by kclose)
  | after_u1 k t s wkk _ hn hs' | after_uD k t s wkk _ hn hs' =>
    exact .of_step (after_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by kclose)
  | now1_u0 k t s _ hn hs' | now1_u1 k t s _ hn hs' | now1_uD k t s _ hn hs' =>
    obtain ⟨c', hle, hst⟩ := World.step_now_up (w := w) (who := 1) (pick := pick)
    have hc' : 0 ≤ c' := by omega
    refine .of_step (now1_shape ..) hst ?_
    simp only [Op.sleeps, Bool.false_eq_true, ite_false]
    rw [now1_next d k t s _ hs']
    by_cases h1 : passedAt d (core.time.Duration.ofNanosSat c') = true
    · have hm : Threads.passed c' (untilNs d) = true := by
        rw [← passedAt_ofNanosSat d c' hc']; exact h1
      simp only [nowNext, h1, ↓reduceIte]; kfin
    · by_cases h2 : core.time.Duration.gt (core.time.Duration.ofNanosSat c') t = true
      · simp only [nowNext, h1, h2, Bool.false_eq_true, ↓reduceIte]; kfin
      · by_cases h3 : 65536 ≤ s.val + 1
        · simp only [nowNext, h1, h2, h3, Bool.false_eq_true, ↓reduceIte]; kfin
        · have hb : (bump s).val < 65536 := by rw [bump_val _ hs']; omega
          simp only [nowNext, h1, h2, h3, Bool.false_eq_true, ↓reduceIte]; kfin
  | swapD_u0 _ | swapD_u1 _ | swapD_uD _ =>
    exact .of_step (swapD_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by kclose)
  | swapE_u0 e _ | swapE_u1 e _ | swapE_uD e _ =>
    exact .of_step (swapE_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by kclose)
  | ret_0u0 r _ | ret_0u1 r _ | ret_0uD r _ | ret_1uD r _ =>
    exact .of_done (ret_shape ..) (by kkeep)

theorem keeps_second (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World)
    (pick : Nat) (h : KInv d (n + 1) a b w) :
    Advance 2 pick b w (fun b' w' => KInv d n a b' w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg, h0, hD⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | start_e | start_t | second_e | now0_u0 | wait_u0 | wait_u0_asleep | after_u0 | now1_u0
    | swapD_u0 | swapE_u0 | ret_0u0 =>
    exact .of_step ustart_shape (World.step_swap (World.get_single hc) hu.fits) (by kclose)
  | wait_u1_asleep =>
    exact .of_step uwake_shape (World.step_wake_single (one := (1, 0))
      (World.get_single_length hc) hu.fits (by simp [hs])) (by kclose)
  | now0_u1 | wait_u1 | after_u1 | now1_u1 | swapD_u1 | swapE_u1 | ret_0u1 =>
    exact .of_step uwake_shape (World.step_wake_none
      (World.get_single_length hc) hu.fits (by simp [hs])) (by kclose)
  | start_d | second_d | store_d | now0_uD | wait_uD | wait_uD_woken | after_uD | now1_uD
    | swapD_uD | swapE_uD | ret_0uD | ret_1uD =>
    exact .of_done uret_shape (by kkeep)

/-! The end of a run. -/

/-- The unpark at its end: the world unchanged. -/
theorem unpark_ends (up : UPt) (w : World) :
    ∃ eb, (ended 2 (unparkAt up)).exec w = .ok (eb, w) := by
  cases up with
  | start => exact ⟨.Running, by rw [ustart_shape]; rfl⟩
  | wake => exact ⟨.Running, by rw [uwake_shape]; rfl⟩
  | ret => exact ⟨.Done (), rfl⟩

theorem ended_running_of {α : Type} {p : Prog α} {k : Nat} {o : Op} {next : Got → Prog α}
    (w : World) (hp : p = .op 0 k o next) (ho : ∀ c e tm, o ≠ .wait c e tm)
    (hu : ∀ c e dl, o ≠ .wait_until c e dl) : (ended 1 p).exec w = .ok (.Running, w) := by
  subst hp; exact ended_exec_running w ho hu

/-- The park at a point that is neither its wait nor its return: running. -/
theorem park_ends (d : Option Deadline) (w : World) (pt : Pt)
    (hw : ∀ k t s, pt ≠ .wait k t s) (hr : ∀ r, pt ≠ .ret r) :
    (ended 1 (parkAt d pt)).exec w = .ok (.Running, w) := by
  cases pt with
  | start k =>
    exact ended_running_of w (start_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | second k =>
    exact ended_running_of w (second_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | store =>
    exact ended_running_of w (store_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | now0 k =>
    exact ended_running_of w (now0_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | wait k t s => exact absurd rfl (hw k t s)
  | after k t s wk =>
    exact ended_running_of w (after_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | now1 k t s =>
    exact ended_running_of w (now1_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | swapD =>
    exact ended_running_of w (swapD_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | swapE e =>
    exact ended_running_of w (swapE_shape ..) (fun _ _ _ h => nomatch h) (fun _ _ _ h => nomatch h)
  | ret r => exact absurd rfl (hr r)

/-- At the end of the schedule: the two conclusions. -/
theorem keeps_last (d : Option Deadline) (a : Prog Res) (b : Prog Unit) (w : World)
    (h : KInv d 0 a b w) :
    ∃ ea eb w', (two [] a b).exec w = .ok ((ea, eb), w') ∧
      (ea = .Done (.Ok .Deadline) → Threads.passed w'.clock (untilNs d) = true) ∧
      (ea = .Asleep → Threads.passed w'.clock (untilNs d) = false) ∧ 0 ≤ w'.clock := by
  obtain ⟨pt, up, rfl, rfl, hu, hg, h0, hD⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | wait_u0 k t s _ hn | wait_u0_asleep k t s _ hn | wait_u1 k t s _ hn
    | wait_u1_asleep k t s _ hn | wait_uD k t s _ hn | wait_uD_woken k t s _ hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    obtain ⟨eb, hb⟩ := unpark_ends _ w
    have hend := fun (v : Int) (hv : w.cells[0]? = some v) =>
      ended_exec_wait_until (who := 1) (t := 0) (k := 0) (e := (4294967295 : U32).val)
        (dl := untilNs d) (next := fun got => parkAt d (.after k t s (Woke.ofGot got))) hv
    refine ⟨_, _, _, two_nil_ok ((congrArg (fun p => (ended 1 p).exec w) (wait_shape d k t s)).trans
      (hend _ (World.get_single hc))) hb, fun he => ?_, fun he => ?_, h0⟩
    · split at he <;> cases he
    · split at he
      · rename_i hcnd; exact hcnd.2.2
      · cases he
  | ret_0u0 r _ | ret_0u1 r _ | ret_0uD r _ | ret_1uD r _ =>
    obtain ⟨eb, hb⟩ := unpark_ends _ w
    refine ⟨_, _, _, two_nil_ok rfl hb, fun he => ?_, (fun he => nomatch he), h0⟩
    cases he
    exact hD rfl
  | _ =>
    obtain ⟨eb, hb⟩ := unpark_ends _ w
    exact ⟨_, _, _, two_nil_ok (park_ends d w _ (fun _ _ _ h => nomatch h) (fun _ h => nomatch h))
      hb, (fun he => nomatch he), (fun he => nomatch he), h0⟩

/-- The run of `keeps`: from the world `keeps` makes before `two` (the word, cell 0, empty;
nobody asleep or woken; the clock at 0 and the script `readings`), the park with fuel one more
than the schedule's steps and the unpark, exactly as `keeps`'s closures call them, end; a park
that returned `Ok(Deadline)` has a deadline the clock has reached; a park asleep has not
reached its deadline; and the clock is not negative. -/
theorem keeps_run (d : Option Deadline) (schedule : List Step) (readings : List Int) :
    ∃ ea eb w',
      (two schedule
          (keeps.Timed.closure0.call (schedule.length + 1) { c0 := W, c1 := KT, c2 := d })
          (keeps.Timed.closure1.call { c0 := W, c1 := KT })).exec
        ⟨[0], [], [], [], false, 0, readings⟩ = .ok ((ea, eb), w') ∧
      (ea = .Done (.Ok .Deadline) →
        ∃ dl, d = some dl ∧ Threads.passed w'.clock (some (Deadline.get dl).toNanos) = true) ∧
      (ea = .Asleep → Threads.passed w'.clock (untilNs d) = false) ∧
      0 ≤ w'.clock := by
  have hstart : KInv d schedule.length
      (keeps.Timed.closure0.call (schedule.length + 1) { c0 := W, c1 := KT, c2 := d })
      (keeps.Timed.closure1.call { c0 := W, c1 := KT })
      ⟨[0], [], [], [], false, 0, readings⟩ :=
    ⟨.start _, .start, rfl, rfl, Or.inl rfl, .start_e _ _ (by omega), Int.le_refl 0,
      by simp [marked]⟩
  obtain ⟨ea, eb, w', he, h1, h2, h3⟩ := two_ends (KInv d) (keeps_first d) (keeps_second d)
    (KPost d) (keeps_last d) schedule _ _ _ hstart
  refine ⟨ea, eb, w', he, fun hd => ?_, h2, h3⟩
  have hp := h1 hd
  cases d with
  | none => simp [untilNs, Threads.passed] at hp
  | some dl => exact ⟨dl, rfl, hp⟩

end BlazeCore.Wait.ParkingDeadlineProof

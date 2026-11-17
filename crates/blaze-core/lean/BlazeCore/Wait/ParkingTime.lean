import BlazeCore.Wait.ParkingTimeLib

/-!
A park left alone returns or sleeps (`blaze_core::wait::parking::a_park_left_alone_returns_or_sleeps`),
on every schedule, every script of clock readings, every deadline and with or without a token
already in the word, over the models blaze emits from the real `park` (on the timed kernel and
the world's clock) and `unpark`. The program points and their shapes are in `ParkingTimeLib`;
`Good` lists every reachable combination of the two points, the word, who sleeps and who is
woken, with the park's loop fuel above the steps left and its retry count below `65536`, and a
sleeper's deadline not yet reached on the world's clock; each thread's step keeps it. `left` is
how many steps of the park alone, with the world's pick 0, still bring it to a return or a sleep
(at most 6: compare, compare, the clock, a wait that times out at once, compare, swap); a step of
the park alone with pick 0 lowers it, so the 8 steps alone at the end leave it at 0.
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic

namespace BlazeCore.Wait.ParkingTimeProof

open blaze_core.wait.retry blaze_core.wait.parking
open BlazeCore.Wait.RetryProofs (passedAt bump bump_val)
open BlazeCore.Wait.ParkingProof (W whatP Res readyR u32_0 u32_1 u32_P wrap_E wrap_N wrap_P)

/-- The park, thread 1, asleep or woken on the word, cell 0. -/
abbrev S : List (Nat × Nat) := [(1, 0)]

/-- Every reachable combination; `n` the schedule steps left, `c` the world's clock. -/
inductive Good (d : Option Deadline) (n : Nat) :
    Pt → UPt → List Int → List (Nat × Nat) → List (Nat × Nat) → Int → Prop
  | start_e (k : Nat) (c : Int) : n < k → Good d n (.start k) .start [0] [] [] c
  | start_t (k : Nat) (c : Int) : n < k → Good d n (.start k) .start [1] [] [] c
  | start_d (k : Nat) (c : Int) : n < k → Good d n (.start k) .ret [1] [] [] c
  | second_e (k : Nat) (c : Int) : n < k → Good d n (.second k) .start [0] [] [] c
  | second_d (k : Nat) (c : Int) : n < k → Good d n (.second k) .ret [1] [] [] c
  | store_d (c : Int) : Good d n .store .ret [1] [] [] c
  | now0_u0 (k : Nat) (c : Int) : n < k → Good d n (.now0 k) .start [Pk] [] [] c
  | now0_u1 (k : Nat) (c : Int) : n < k → Good d n (.now0 k) .wake [1] [] [] c
  | now0_uD (k : Nat) (c : Int) : n < k → Good d n (.now0 k) .ret [1] [] [] c
  | wait_u0 (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k → s.val < 65536 →
      Good d n (.wait k t s) .start [Pk] [] [] c
  | wait_u0_asleep (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k →
      s.val < 65536 → passed c (untilNs d) = false → Good d n (.wait k t s) .start [Pk] S [] c
  | wait_u1 (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k → s.val < 65536 →
      Good d n (.wait k t s) .wake [1] [] [] c
  | wait_u1_asleep (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k →
      s.val < 65536 → passed c (untilNs d) = false → Good d n (.wait k t s) .wake [1] S [] c
  | wait_uD (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k → s.val < 65536 →
      Good d n (.wait k t s) .ret [1] [] [] c
  | wait_uD_woken (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k →
      s.val < 65536 → Good d n (.wait k t s) .ret [1] [] S c
  | after_u0 (k : Nat) (t : core.time.Duration) (s : U32) (wk : Woke) (c : Int) : n < k →
      s.val < 65536 → Good d n (.after k t s wk) .start [Pk] [] [] c
  | after_u1 (k : Nat) (t : core.time.Duration) (s : U32) (wk : Woke) (c : Int) : n < k →
      s.val < 65536 → Good d n (.after k t s wk) .wake [1] [] [] c
  | after_uD (k : Nat) (t : core.time.Duration) (s : U32) (wk : Woke) (c : Int) : n < k →
      s.val < 65536 → Good d n (.after k t s wk) .ret [1] [] [] c
  | now1_u0 (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k → s.val < 65536 →
      Good d n (.now1 k t s) .start [Pk] [] [] c
  | now1_u1 (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k → s.val < 65536 →
      Good d n (.now1 k t s) .wake [1] [] [] c
  | now1_uD (k : Nat) (t : core.time.Duration) (s : U32) (c : Int) : n < k → s.val < 65536 →
      Good d n (.now1 k t s) .ret [1] [] [] c
  | swapD_u0 (c : Int) : Good d n .swapD .start [Pk] [] [] c
  | swapD_u1 (c : Int) : Good d n .swapD .wake [1] [] [] c
  | swapD_uD (c : Int) : Good d n .swapD .ret [1] [] [] c
  | swapE_u0 (e : WaitError) (c : Int) : Good d n (.swapE e) .start [Pk] [] [] c
  | swapE_u1 (e : WaitError) (c : Int) : Good d n (.swapE e) .wake [1] [] [] c
  | swapE_uD (e : WaitError) (c : Int) : Good d n (.swapE e) .ret [1] [] [] c
  | ret_0u0 (r : Res) (c : Int) : Good d n (.ret r) .start [0] [] [] c
  | ret_0u1 (r : Res) (c : Int) : Good d n (.ret r) .wake [0] [] [] c
  | ret_0uD (r : Res) (c : Int) : Good d n (.ret r) .ret [0] [] [] c
  | ret_1uD (r : Res) (c : Int) : Good d n (.ret r) .ret [1] [] [] c

/-- How many steps of the park alone, with the world's pick 0, bring it from a point to a return
or a sleep: the word `cs` and the sleepers `sl` decide. -/
def left : Pt → List Int → List (Nat × Nat) → Nat
  | .start _, cs, _ => if cs = [0] then 6 else 1
  | .second _, cs, _ => if cs = [0] then 5 else 2
  | .store, _, _ => 1
  | .now0 _, _, _ => 4
  | .wait _ _ _, cs, sl => if sl = [] then (if cs = [4294967295] then 3 else 2) else 0
  | .after _ _ _ wk, cs, _ => if cs = [4294967295] then (if wk = .TimedOut then 2 else 5) else 1
  | .now1 _ _ _, _, _ => 4
  | .swapD, _, _ => 1
  | .swapE _, _, _ => 1
  | .ret _, _, _ => 0

theorem left_le (pt : Pt) (cs : List Int) (sl : List (Nat × Nat)) : left pt cs sl ≤ 6 := by
  cases pt <;> simp only [left] <;> (repeat' split) <;> omega

def Inv (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World) : Prop :=
  ∃ pt up, a = parkAt d pt ∧ b = unparkAt up ∧ w.OneCell 0 ∧
    Good d n pt up w.cells w.asleep w.woken w.clock ∧ left pt w.cells w.asleep ≤ n

set_option hygiene false in
/-- The combination after a step, once the thread's point after it is named. -/
local macro "fin" : tactic => `(tactic| (
  unfold Inv
  refine ⟨_, _, rfl, rfl, ?_, ?_, ?_⟩
  · first | exact World.OneCell.touched hu | exact hu
  · simp [hc, hs, hk, hcl, u32_0, u32_1, u32_P]
    constructor <;> first | omega | assumption | (simp only [u32_0]; omega)
  · simp [left, hc, hs, hk, u32_0, u32_1, u32_P] at hl ⊢ <;> omega))

set_option hygiene false in
/-- After a step: the point the thread is at and the combination after it. -/
local macro "close" : tactic => `(tactic| (
  simp [wrap_E, wrap_N, wrap_P, u32_0, u32_1, u32_P, Op.sleeps, afterNext, at6_N d, at6_P d,
    at10_N d, at10_P d, uat1_E, uat1_N, uat1_P]
  fin))

set_option hygiene false in
/-- A thread that has returned stays. -/
local macro "keep" : tactic => `(tactic| (
  unfold Inv
  refine ⟨_, _, rfl, rfl, hu, ?_, ?_⟩
  · rw [hc, hs, hk, hcl]
    constructor <;> first | omega | assumption
  · first | exact Nat.le_trans (left_le _ _ _) (by omega) | simp [left]))

theorem first_step (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World)
    (pick : Nat) (h : Inv d (n + 1) a b w) (hp : pick = 0 ∨ 7 ≤ n) :
    Advance 1 pick a w (fun a' w' => Inv d n a' b w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg, hl⟩ := h
  generalize hc : w.cells = cs at hg hl
  generalize hs : w.asleep = sl at hg hl
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | start_e k _ hn =>
    exact .of_step (start_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by close)
  | start_t k _ hn =>
    exact .of_step (start_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | start_d k _ hn =>
    exact .of_step (start_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | second_e k _ hn =>
    exact .of_step (second_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | second_d k _ hn =>
    exact .of_step (second_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by close)
  | store_d _ =>
    exact .of_step (store_shape ..) (World.step_store (World.get_single_length hc) hu.fits)
      (by close)
  | now0_u0 k _ hn | now0_u1 k _ hn | now0_uD k _ hn =>
    obtain ⟨c', hst⟩ := World.step_now_move (w := w) (who := 1) (pick := pick)
    exact .of_step (now0_shape ..) hst (by close)
  | wait_u0 k t s _ hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    by_cases hpd : passed c (untilNs d) = true
    · exact .of_step (wait_shape ..) (World.step_wait_until_timed_out_at_once (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) u32_P.symm (by rw [hcl]; exact hpd)) (by close)
    · have hpd' : passed c (untilNs d) = false := by simpa using hpd
      exact .of_step (wait_shape ..) (World.step_wait_until_sleeps (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) u32_P.symm (by rw [hcl]; exact hpd')) (by close)
  | wait_u0_asleep k t s _ hn hs' hpd | wait_u1_asleep k t s _ hn hs' hpd =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    have hpd' : passed w.clock (untilNs d) = false := by rw [hcl]; exact hpd
    by_cases h1 : pick = 1
    · exact .of_step (wait_shape ..) (World.step_wait_until_spurious (World.get_single hc)
        hu.fits (by simp [hk]) (by simp [hs]) hpd' h1) (by close)
    · cases hd : d with
      | none =>
        have hpc : passed c (untilNs none) = false := by rw [← hd]; exact hpd
        have hpw : passed w.clock (untilNs none) = false := by rw [hcl]; exact hpc
        exact .of_step (wait_shape ..) (World.step_wait_until_stays (World.get_single hc)
          hu.fits (by simp [hk]) (by simp [hs]) hpw h1 (fun _ => rfl)) (by close)
      | some dd =>
        have hpc : passed c (untilNs (some dd)) = false := by rw [← hd]; exact hpd
        have hpw : passed w.clock (untilNs (some dd)) = false := by rw [hcl]; exact hpc
        by_cases h2 : pick = 2
        · have hlt : w.clock < (Deadline.get dd).toNanos := by
            have := hpw
            simp only [untilNs, passed, decide_eq_false_iff_not] at this
            omega
          exact .of_step (wait_shape ..) (World.step_wait_until_told (World.get_single hc)
            hu.fits (by simp [hk]) (by simp [hs]) hlt h2) (by close)
        · exact .of_step (wait_shape ..) (World.step_wait_until_stays (World.get_single hc)
            hu.fits (by simp [hk]) (by simp [hs]) hpw h1 (fun h => absurd h h2)) (by close)
  | wait_u1 k t s _ hn hs' | wait_uD k t s _ hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_until_changed (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by rw [u32_P]; decide)) (by close)
  | wait_uD_woken k t s _ hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_until_woken (World.get_single hc) hu.fits
      (by simp [hk])) (by close)
  | after_u0 k t s wkk _ hn hs' =>
    cases wkk <;>
      exact .of_step (after_shape ..) (World.step_cas_err (World.get_single hc) hu.fits
        (by decide)) (by close)
  | after_u1 k t s wkk _ hn hs' | after_uD k t s wkk _ hn hs' =>
    exact .of_step (after_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | now1_u0 k t s _ hn hs' | now1_u1 k t s _ hn hs' | now1_uD k t s _ hn hs' =>
    obtain ⟨c', hst⟩ := World.step_now_move (w := w) (who := 1) (pick := pick)
    refine .of_step (now1_shape ..) hst ?_
    simp only [Op.sleeps, Bool.false_eq_true, ite_false]
    rw [now1_next d k t s _ hs']
    generalize core.time.Duration.ofNanosSat c' = t6
    by_cases h1 : passedAt d t6 = true
    · simp only [nowNext, h1, ↓reduceIte]; fin
    · by_cases h2 : core.time.Duration.gt t6 t = true
      · simp only [nowNext, h1, h2, Bool.false_eq_true, ↓reduceIte]; fin
      · by_cases h3 : 65536 ≤ s.val + 1
        · simp only [nowNext, h1, h2, h3, Bool.false_eq_true, ↓reduceIte]; fin
        · have hb : (bump s).val < 65536 := by rw [bump_val _ hs']; omega
          simp only [nowNext, h1, h2, h3, Bool.false_eq_true, ↓reduceIte]; fin
  | swapD_u0 _ | swapD_u1 _ | swapD_uD _ =>
    exact .of_step (swapD_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swapE_u0 e _ | swapE_u1 e _ | swapE_uD e _ =>
    exact .of_step (swapE_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | ret_0u0 r _ | ret_0u1 r _ | ret_0uD r _ | ret_1uD r _ =>
    exact .of_done (ret_shape ..) (by keep)

theorem second_step (d : Option Deadline) (n : Nat) (a : Prog Res) (b : Prog Unit) (w : World)
    (pick : Nat) (h : Inv d (n + 1) a b w) (hn7 : 7 ≤ n) :
    Advance 2 pick b w (fun b' w' => Inv d n a b' w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg, hl⟩ := h
  generalize hc : w.cells = cs at hg hl
  generalize hs : w.asleep = sl at hg hl
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | start_e | start_t | second_e | now0_u0 | wait_u0 | wait_u0_asleep | after_u0 | now1_u0
    | swapD_u0 | swapE_u0 | ret_0u0 =>
    exact .of_step ustart_shape (World.step_swap (World.get_single hc) hu.fits) (by close)
  | wait_u1_asleep =>
    exact .of_step uwake_shape (World.step_wake_single (one := (1, 0))
      (World.get_single_length hc) hu.fits (by simp [hs])) (by close)
  | now0_u1 | wait_u1 | after_u1 | now1_u1 | swapD_u1 | swapE_u1 | ret_0u1 =>
    exact .of_step uwake_shape (World.step_wake_none
      (World.get_single_length hc) hu.fits (by simp [hs])) (by close)
  | start_d | second_d | store_d | now0_uD | wait_uD | wait_uD_woken | after_uD | now1_uD
    | swapD_uD | swapE_uD | ret_0uD | ret_1uD =>
    exact .of_done uret_shape (by keep)

/-- At the end, with no steps left over (`left` is 0): the park has returned or is asleep. -/
theorem last_alone (d : Option Deadline) (a : Prog Res) (b : Prog Unit) (w : World)
    (h : Inv d 0 a b w) :
    ∃ ea eb w', (two [] a b).exec w = .ok ((ea, eb), w') ∧ ea ≠ .Running := by
  obtain ⟨pt, up, rfl, rfl, hu, hg, hl⟩ := h
  generalize hc : w.cells = cs at hg hl
  generalize hs : w.asleep = sl at hg hl
  generalize hk : w.woken = wk at hg
  generalize hcl : w.clock = c at hg
  cases hg with
  | wait_u0_asleep k t s _ hn hs' hpd | wait_u1_asleep k t s _ hn hs' hpd =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    have hend := fun (v : Int) (hv : w.cells[0]? = some v) =>
      ended_exec_wait_until (who := 1) (t := 0) (k := 0) (e := (4294967295 : U32).val)
        (dl := untilNs d) (next := fun got => parkAt d (.after k t s (Woke.ofGot got))) hv
    refine ⟨_, _, _, two_nil_ok ((congrArg (fun p => (ended 1 p).exec w) (wait_shape d k t s)).trans
      (hend _ (World.get_single hc))) rfl, ?_⟩
    simp [hk, hs, hcl, hpd]
  | ret_0u0 | ret_0u1 | ret_0uD | ret_1uD => exact ⟨_, _, _, two_nil_ok rfl rfl, by simp⟩
  | _ => exfalso; simp only [left] at hl; (repeat' split at hl) <;> simp_all

theorem u8_0 : (0 : U8).val = 0 :=
  Scalar.ofNat_val .U8 0 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
theorem usize_0 : (0 : Usize).val = 0 :=
  Scalar.ofNat_val .Usize 0 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
theorem usize_1 : (1 : Usize).val = 1 :=
  Scalar.ofNat_val .Usize 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
theorem usize_8 : (8 : Usize).val = 8 :=
  Scalar.ofNat_val .Usize 8 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
theorem usize_max : (18446744073709551615 : Usize).val = 18446744073709551615 :=
  Scalar.ofNat_val .Usize 18446744073709551615 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- The 8 steps of the park alone, pick 0, from any reachable combination with `left ≤ 8`. -/
theorem alone_run (d : Option Deadline) (a : Prog Res) (b : Prog Unit) (w : World)
    (h : Inv d 8 a b w) :
    ∃ e w', (two (List.replicate 8 ⟨false, 0⟩) a b).exec w = .ok (e, w') ∧ e.1 ≠ .Running := by
  obtain ⟨a', b', w', hI, he⟩ := two_exec_alone (Inv d) 0
    (fun n a b w h => first_step d n a b w _ h (Or.inl (by rw [u8_0]; rfl))) 8 a b w h
  obtain ⟨ea, eb, w'', he', hne⟩ := last_alone d a' b' w' hI
  exact ⟨(ea, eb), w'', he.trans he', hne⟩

/-- Any schedule, then the 8 steps of the park alone. -/
theorem run (d : Option Deadline) (sched : List Step) (a : Prog Res) (b : Prog Unit) (w : World)
    (h : Inv d (sched.length + 8) a b w) :
    ∃ e w', (two (sched ++ List.replicate 8 ⟨false, 0⟩) a b).exec w = .ok (e, w') ∧
      e.1 ≠ .Running := by
  obtain ⟨a', b', w', hI, he⟩ := two_exec_append (fun n => Inv d (n + 8))
    (fun n a b w pick h => first_step d (n + 8) a b w pick
      (by rwa [show n + 8 + 1 = n + 1 + 8 by omega]) (Or.inr (by omega)))
    (fun n a b w pick h => second_step d (n + 8) a b w pick
      (by rwa [show n + 8 + 1 = n + 1 + 8 by omega]) (by omega))
    sched _ a b w h
  obtain ⟨e, w'', he', hne⟩ := alone_run d a' b' w' hI
  exact ⟨e, w'', he.trans he', hne⟩

/-- `settles`'s loop: it puts the `i` steps alone that are left after the schedule. -/
theorem loop0_eq : ∀ (i fuel : Nat) (steps : Vec Step) (alone : Usize), alone.val + i = 8 →
    i < fuel → steps.val.length + i ≤ 18446744073709551615 →
    ∃ v : Vec Step, settles.Timed.loop_0 fuel steps alone = .ok v ∧
      v.val = steps.val ++ List.replicate i ⟨false, 0⟩
  | _, 0, _, _, _, hf, _ => absurd hf (Nat.not_lt_zero _)
  | 0, fuel + 1, steps, alone, ha, _, _ => by
    refine ⟨steps, ?_, by simp⟩
    have hlt : Scalar.lt alone (8 : Usize) = false := by
      simp only [Scalar.lt, usize_8, decide_eq_false_iff_not]; omega
    simp only [settles.Timed.loop_0, hlt, Bool.false_eq_true, ↓reduceIte]
  | i + 1, fuel + 1, steps, alone, ha, hf, hl => by
    have hlt : Scalar.lt alone (8 : Usize) = true := by
      simp only [Scalar.lt, usize_8, decide_eq_true_eq]; omega
    have hpush : steps.val.length < 18446744073709551615 := by omega
    have h0 := alone.hmin
    simp only [ScalarTy.min] at h0
    have hadd : Scalar.add alone (1 : Usize) =
        .ok ⟨alone.val + 1, by simp only [ScalarTy.min]; omega,
          by simp only [ScalarTy.max]; omega⟩ := by
      unfold Scalar.add
      rw [usize_1]
      exact Scalar.checked_ok _ _ ⟨by simp only [ScalarTy.min]; omega,
        by simp only [ScalarTy.max]; omega⟩
    obtain ⟨v, hv, hvv⟩ := loop0_eq i fuel
      ⟨steps.val ++ [⟨false, 0⟩], by simp only [List.length_append, List.length_singleton]; omega⟩
      ⟨alone.val + 1, by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩
      (by simp only; omega) (by omega) (by simp only [List.length_append, List.length_singleton]; omega)
    refine ⟨v, ?_, ?_⟩
    · simp only [settles.Timed.loop_0, hlt, ↓reduceIte, Vec.push_ok _ _ hpush, hadd,
        Result.bind_ok]
      exact hv
    · rw [hvv]; simp [List.replicate_succ]

theorem new_exec (v : U32) :
    (Atomic.new v).exec World.new = .ok (⟨0⟩, ⟨[v.val], [], [], [], false, 0, []⟩) := rfl

theorem settles_true (d : Option Deadline) (schedule : Vec Step)
    (readings : Slice core.time.Duration) (token : Bool) :
    (settles.Timed (schedule.val.length + 9) KT schedule readings d token).value = .ok true := by
  obtain ⟨m, hsub, hm⟩ : ∃ m : Usize, Scalar.sub (18446744073709551615 : Usize) (8 : Usize) =
      .ok m ∧ m.val = 18446744073709551615 - 8 :=
    ⟨_, by
      unfold Scalar.sub
      rw [usize_max, usize_8]
      exact Scalar.checked_ok _ _ ⟨by simp only [ScalarTy.min]; omega,
        by simp only [ScalarTy.max]; omega⟩, rfl⟩
  by_cases hL : 18446744073709551615 - 8 < (schedule.val.length : Int)
  · have hgt : Scalar.gt (Slice.len schedule) m = true := by
      simp only [Scalar.gt, Slice.len_val, hm, decide_eq_true_eq]; omega
    simp only [settles.Timed, Prog.lift_done, hsub, Prog.bind_done_ok, hgt, ↓reduceIte]
    rfl
  · have hgt : Scalar.gt (Slice.len schedule) m = false := by
      simp only [Scalar.gt, Slice.len_val, hm, decide_eq_false_iff_not]; omega
    obtain ⟨v, hv, hvv⟩ := loop0_eq 8 (schedule.val.length + 9) schedule 0
      (by rw [usize_0]; rfl) (by omega) (by omega)
    have hrun : ∀ x : Int, (x = 0 ∨ x = 1) → ∀ e w',
        (two (schedule.val ++ List.replicate 8 ⟨false, 0⟩)
          (settles.Timed.closure0.call (schedule.val.length + 9) { c0 := W, c1 := KT, c2 := d })
          (settles.Timed.closure1.call { c0 := W, c1 := KT })).exec
          ⟨[x], [], [], [], false, 0, readings.val.map core.time.Duration.toNanos⟩ = .ok (e, w') →
        e.1 ≠ .Running →
        (settles.Timed.at_1 (schedule.val.length + 9) readings v KT d W).exec
          ⟨[x], [], [], [], false, 0, []⟩ = .ok (true, w') := by
      intro x hx e w' he hne
      simp only [settles.Timed.at_1, settles.Timed.at_2, Prog.exec_bind, Threads.two_of, hvv]
      have hcl : (Threads.clock readings).exec ⟨[x], [], [], [], false, 0, []⟩ =
          .ok ((), ⟨[x], [], [], [], false, 0, readings.val.map core.time.Duration.toNanos⟩) := rfl
      rw [hcl]
      simp only []
      rw [he]
      simp only []
      obtain ⟨e1, e2⟩ := e
      cases e1 with
      | Running => exact absurd rfl hne
      | _ => rfl
    have hstart : ∀ x : Int, (x = 0 ∨ x = 1) → Inv d (schedule.val.length + 8)
        (settles.Timed.closure0.call (schedule.val.length + 9) { c0 := W, c1 := KT, c2 := d })
        (settles.Timed.closure1.call { c0 := W, c1 := KT })
        ⟨[x], [], [], [], false, 0, readings.val.map core.time.Duration.toNanos⟩ := by
      intro x hx
      refine ⟨.start _, .start, rfl, rfl, Or.inl rfl, ?_, Nat.le_trans (left_le _ _ _) (by omega)⟩
      rcases hx with rfl | rfl
      · exact .start_e _ _ (by omega)
      · exact .start_t _ _ (by omega)
    obtain ⟨x, hx, hxv⟩ : ∃ x : Int, (x = 0 ∨ x = 1) ∧
        (if token then (1 : U32) else 0).val = x :=
      ⟨_, by cases token <;> simp [u32_0, u32_1], rfl⟩
    obtain ⟨e, w', he, hne⟩ := run d schedule.val _ _ _ (hstart x hx)
    apply Prog.value_of_exec (w := w')
    simp only [settles.Timed, Prog.lift_done, hsub, Prog.bind_done_ok, hgt, Bool.false_eq_true,
      ↓reduceIte, hv]
    have hnew : (Atomic.new (if token then (1 : U32) else 0)).exec World.new =
        .ok (⟨0⟩, ⟨[x], [], [], [], false, 0, []⟩) := by rw [new_exec, hxv]
    have key := hrun x hx e w' he hne
    cases token
    · simp only [Bool.true_beq, Bool.false_beq, Bool.not_false, Bool.false_eq_true, ↓reduceIte,
        Prog.exec_bind]
      simp only [Bool.false_eq_true, ↓reduceIte] at hnew
      rw [hnew]
      exact key
    · simp only [Bool.true_beq, ↓reduceIte, Prog.exec_bind]
      simp only [↓reduceIte] at hnew
      rw [hnew]
      exact key

end BlazeCore.Wait.ParkingTimeProof

open BlazeCore.Wait.ParkingTimeProof in
/-- Fuel: the schedule's steps, the 8 steps alone and one more: each turn of the park's loop
takes a step, and `settles`'s own loop takes 9 turns. -/
theorem blaze_core.wait.parking.a_park_left_alone_returns_or_sleeps.spec.proof :
    blaze_core.wait.parking.a_park_left_alone_returns_or_sleeps.spec.statement := by
  intro schedule readings deadline token
  exact ⟨schedule.val.length + 9, settles_true _ schedule readings token⟩


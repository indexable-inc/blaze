import BlazeCore.Wait.ParkingLib
import BlazeCore.Wait.ParkingTime
import BlazeCore.Wait.ParkingDeadline

/-!
No unpark is lost (`blaze_core::wait::parking::no_unpark_is_lost`), on every schedule, over the
models blaze emits from the real `park` and `unpark`: the proof of
lean/Blaze/Tests/ThreadsParkProof.lean, ported. The program points and their shapes are in
`ParkingLib`; `Good` lists every reachable combination of the two points, the word, who sleeps
and who is woken, with the park's loop fuel above the schedule steps left and its retry count
below `65536`; each thread's step keeps it, and the oracle `parks` is read off its ends.
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic

namespace BlazeCore.Wait.ParkingProof

open blaze_core.wait.retry blaze_core.wait.parking
open BlazeCore.Wait.RetryProofs (passedAt bump bump_val)

def p (pick : Nat := 0) : Step := ⟨false, Scalar.lit .U8 pick⟩
def u : Step := ⟨true, Scalar.lit .U8 0⟩
def ns (n : Nat) : core.time.Duration := core.time.Duration.from_nanos (Scalar.lit .U64 n)

/-! Evaluation first. -/

-- A deadline already passed at `now`: the spurious return is followed by the deadline.
example : (parks 8 .None ⟨[p, p, p, p 1, p, p, u], by decide⟩ (ns 5) (some ⟨ns 0⟩)).value =
    .ok true := by rfl
-- A timeout, then the unpark before the park's last swap.
example : (parks 8 .None ⟨[p, p, p, p 2, p, u, p, u], by decide⟩ (ns 5) (some ⟨ns 20⟩)).value =
    .ok true := by rfl
-- The unpark between the park's two compares.
example : (parks 8 .None ⟨[p, u, p, p], by decide⟩ (ns 5) none).value = .ok true := by rfl

/-- The park, thread 1, asleep or woken on the word, cell 0. -/
abbrev S : List (Nat × Nat) := [(1, 0)]

/-- Whether the park has a deadline: its waits are timed. -/
abbrev Tm (d : Option Deadline) : Prop := core.option.Option.is_some d = true

/-- Every reachable combination; `n` the schedule steps left. -/
inductive Good (d : Option Deadline) (n : Nat) :
    Pt → UPt → List Int → List (Nat × Nat) → List (Nat × Nat) → Prop
  | start_u0 (k : Nat) : n < k → Good d n (.start k) .start [0] [] []
  | start_uD (k : Nat) : n < k → Good d n (.start k) .ret [1] [] []
  | second_u0 (k : Nat) : n < k → Good d n (.second k) .start [0] [] []
  | second_uD (k : Nat) : n < k → Good d n (.second k) .ret [1] [] []
  | store_uD : Good d n .store .ret [1] [] []
  | wait_u0 (k : Nat) (s : U32) : n < k → s.val < 65536 →
      Good d n (.wait k s) .start [4294967295] [] []
  | wait_u0_asleep (k : Nat) (s : U32) : n < k → s.val < 65536 →
      Good d n (.wait k s) .start [4294967295] S []
  | wait_u1 (k : Nat) (s : U32) : n < k → s.val < 65536 → Good d n (.wait k s) .wake [1] [] []
  | wait_u1_asleep (k : Nat) (s : U32) : n < k → s.val < 65536 →
      Good d n (.wait k s) .wake [1] S []
  | wait_uD (k : Nat) (s : U32) : n < k → s.val < 65536 → Good d n (.wait k s) .ret [1] [] []
  | wait_uD_woken (k : Nat) (s : U32) : n < k → s.val < 65536 →
      Good d n (.wait k s) .ret [1] [] S
  | after_u0 (k : Nat) (s : U32) (wk : Woke) : n < k → s.val < 65536 → (wk = .TimedOut → Tm d) →
      Good d n (.after k s wk) .start [4294967295] [] []
  | after_u1 (k : Nat) (s : U32) (wk : Woke) : n < k → s.val < 65536 → (wk = .TimedOut → Tm d) →
      Good d n (.after k s wk) .wake [1] [] []
  | after_uD (k : Nat) (s : U32) (wk : Woke) : n < k → s.val < 65536 → (wk = .TimedOut → Tm d) →
      Good d n (.after k s wk) .ret [1] [] []
  | swapD_u0 : Tm d → Good d n .swapD .start [4294967295] [] []
  | swapD_u1 : Tm d → Good d n .swapD .wake [1] [] []
  | swapD_uD : Tm d → Good d n .swapD .ret [1] [] []
  | swapE_u0 (e : WaitError) : Good d n (.swapE e) .start [4294967295] [] []
  | swapE_u1 (e : WaitError) : Good d n (.swapE e) .wake [1] [] []
  | swapE_uD (e : WaitError) : Good d n (.swapE e) .ret [1] [] []
  | ready_u1 : Good d n (.ret readyR) .wake [0] [] []
  | ready_uD : Good d n (.ret readyR) .ret [0] [] []
  | dead_u0 : Tm d → Good d n (.ret (.Ok .Deadline)) .start [0] [] []
  | dead_uD : Tm d → Good d n (.ret (.Ok .Deadline)) .ret [1] [] []
  | err_u0 (e : WaitError) : Good d n (.ret (.Err e)) .start [0] [] []
  | err_uD (e : WaitError) : Good d n (.ret (.Err e)) .ret [1] [] []

def Inv (d : Option Deadline) (now : core.time.Duration) (n : Nat) (a : Prog Res) (b : Prog Unit)
    (w : World) : Prop :=
  ∃ pt up, a = parkAt d now pt ∧ b = unparkAt up ∧ w.OneCell 0 ∧
    Good d n pt up w.cells w.asleep w.woken

/-- The world the threads start in: the word, `EMPTY`. -/
def start : World := ⟨[0], [], [], [], false, 0, []⟩

set_option hygiene false in
/-- After a step: the point the thread is at and the combination after it. -/
local macro "close" : tactic => `(tactic| (
  simp [wrap_E, wrap_N, wrap_P, u32_0, u32_1, u32_P, Op.sleeps, at6_N d now, at6_P d now,
    at9_N d now, at9_P d now, uat1_E, uat1_P]
  unfold Inv
  refine ⟨_, _, rfl, rfl, World.OneCell.touched hu, ?_⟩
  simp [hc, hs, hk, u32_0, u32_1, u32_P]
  constructor <;> first | omega | assumption | (intro _; assumption) | (intro h; nomatch h)))

set_option hygiene false in
/-- A thread that has returned stays. -/
local macro "keep" : tactic => `(tactic| (
  unfold Inv
  refine ⟨_, _, rfl, rfl, hu, ?_⟩
  rw [hc, hs, hk]
  constructor <;> first | omega | assumption | (intro _; assumption) | (intro h; nomatch h)))

theorem first_step (d : Option Deadline) (now : core.time.Duration) (n : Nat) (a : Prog Res)
    (b : Prog Unit) (w : World) (pick : Nat) (h : Inv d now (n + 1) a b w) :
    Advance 1 pick a w (fun a' w' => Inv d now n a' b w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  cases hg with
  | start_u0 k hn =>
    exact .of_step (start_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by close)
  | start_uD k hn =>
    exact .of_step (start_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | second_u0 k hn =>
    have h0 : (0 : U32).val < 65536 := by rw [u32_0]; omega
    exact .of_step (second_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | second_uD k hn =>
    exact .of_step (second_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by close)
  | store_uD =>
    exact .of_step (store_shape ..) (World.step_store (World.get_single_length hc) hu.fits)
      (by close)
  | wait_u0 k s hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_sleeps (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by decide)) (by close)
  | wait_u0_asleep k s hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    by_cases h1 : pick = 1
    · exact .of_step (wait_shape ..) (World.step_wait_spurious (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1) (by close)
    by_cases h2 : pick = 2 ∧ core.option.Option.is_some d = true
    · obtain ⟨h2, ht⟩ := h2
      exact .of_step (wait_shape ..) (World.step_wait_timed_out (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h2 ht) (by close)
    · exact .of_step (wait_shape ..) (World.step_wait_stays (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1 h2) (by close)
  | wait_u1 k s hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_changed (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by decide)) (by close)
  | wait_u1_asleep k s hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    by_cases h1 : pick = 1
    · exact .of_step (wait_shape ..) (World.step_wait_spurious (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1) (by close)
    by_cases h2 : pick = 2 ∧ core.option.Option.is_some d = true
    · obtain ⟨h2, ht⟩ := h2
      exact .of_step (wait_shape ..) (World.step_wait_timed_out (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h2 ht) (by close)
    · exact .of_step (wait_shape ..) (World.step_wait_stays (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1 h2) (by close)
  | wait_uD k s hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_changed (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by decide)) (by close)
  | wait_uD_woken k s hn hs' =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_woken (World.get_single hc) hu.fits
      (by simp [hk])) (by close)
  | after_u0 k s wkk hn hs' hto =>
    refine .of_step (after_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      ?_
    simp only [Op.sleeps, Bool.false_eq_true, ite_false]
    rw [afterFail_eq d now k s wkk hs']
    refine ⟨_, _, rfl, rfl, World.OneCell.touched hu, ?_⟩
    simp only [World.touched_cells, World.touched_asleep, World.touched_woken, hc, hs, hk]
    cases wkk with
    | TimedOut => exact .swapD_u0 (hto rfl)
    | _ =>
      simp only [afterNext]
      by_cases hp : passedAt d now = true
      · simp only [hp, ite_true]; exact .swapD_u0 (is_some_of_passed d now hp)
      · by_cases hb : 65536 ≤ s.val + 1
        · simp only [hp, hb, Bool.false_eq_true, ite_false, ite_true]; exact .swapE_u0 _
        · simp only [hp, hb, Bool.false_eq_true, ite_false]
          exact .wait_u0 _ _ (by omega) (by rw [bump_val _ hs']; omega)
  | after_u1 k s wkk hn hs' hto =>
    exact .of_step (after_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | after_uD k s wkk hn hs' hto =>
    exact .of_step (after_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | swapD_u0 ht =>
    exact .of_step (swapD_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swapD_u1 ht =>
    exact .of_step (swapD_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swapD_uD ht =>
    exact .of_step (swapD_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swapE_u0 e =>
    exact .of_step (swapE_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swapE_u1 e =>
    exact .of_step (swapE_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swapE_uD e =>
    exact .of_step (swapE_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | ready_u1 => exact .of_done (ret_shape ..) (by keep)
  | ready_uD => exact .of_done (ret_shape ..) (by keep)
  | dead_u0 ht => exact .of_done (ret_shape ..) (by keep)
  | dead_uD ht => exact .of_done (ret_shape ..) (by keep)
  | err_u0 e => exact .of_done (ret_shape ..) (by keep)
  | err_uD e => exact .of_done (ret_shape ..) (by keep)

theorem second_step (d : Option Deadline) (now : core.time.Duration) (n : Nat) (a : Prog Res)
    (b : Prog Unit) (w : World) (pick : Nat) (h : Inv d now (n + 1) a b w) :
    Advance 2 pick b w (fun b' w' => Inv d now n a b' w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  cases hg with
  | start_u0 | second_u0 | wait_u0 | wait_u0_asleep | after_u0 | swapD_u0 | swapE_u0 | dead_u0
    | err_u0 =>
    exact .of_step ustart_shape (World.step_swap (World.get_single hc) hu.fits) (by close)
  | wait_u1_asleep =>
    exact .of_step uwake_shape (World.step_wake_single (one := (1, 0))
      (World.get_single_length hc) hu.fits (by simp [hs])) (by close)
  | wait_u1 | after_u1 | swapD_u1 | swapE_u1 | ready_u1 =>
    exact .of_step uwake_shape (World.step_wake_none
      (World.get_single_length hc) hu.fits (by simp [hs])) (by close)
  | start_uD | second_uD | store_uD | wait_uD | wait_uD_woken | after_uD | swapD_uD | swapE_uD
    | ready_uD | dead_uD | err_uD =>
    exact .of_done uret_shape (by keep)

/-- What the oracle asks of an end, with the word's last value `v`. -/
def Post (d : Option Deadline) (ea : Ended Res) (eb : Ended Unit) (w : World) : Prop :=
  w.OneCell 0 ∧ ∃ v, w.cells = [v] ∧
    (eb = .Done () → ea ≠ .Asleep) ∧
    (ea = .Done readyR → eb = .Done () → v = 0) ∧
    (ea = .Done (.Ok .Deadline) → Tm d ∧ (eb = .Done () → v = 1)) ∧
    (∀ e, ea = .Done (.Err e) → eb = .Done () → v = 1) ∧
    (ea = .Running ∨ ea = .Asleep ∨ ea = .Done readyR ∨ ea = .Done (.Ok .Deadline) ∨
      ∃ e, ea = .Done (.Err e))

set_option hygiene false in
local macro "post" : tactic => `(tactic| (
  simp only [Post]
  refine ⟨hu, _, hc, ?_, ?_, ?_, ?_, ?_⟩ <;> simp_all [u32_P, Tm]))

theorem last_step (d : Option Deadline) (now : core.time.Duration) (a : Prog Res) (b : Prog Unit)
    (w : World) (h : Inv d now 0 a b w) :
    ∃ ea eb w', (two [] a b).exec w = .ok ((ea, eb), w') ∧ Post d ea eb w' := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  cases hg with
  | wait_u0 k s hn | wait_u0_asleep k s hn | wait_u1 k s hn | wait_u1_asleep k s hn
    | wait_uD k s hn | wait_uD_woken k s hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact ⟨_, _, _, two_nil_ok (by rw [wait_shape]; exact ended_exec_wait (World.get_single hc))
      rfl, by post⟩
  | _ => exact ⟨_, _, _, two_nil_ok rfl rfl, by post⟩

/-- The park and the unpark, for every schedule, deadline and reading. -/
theorem parks_true (d : Option Deadline) (now : core.time.Duration) (schedule : Vec Step) :
    (parks (schedule.val.length + 1) .None schedule now d).value = .ok true := by
  have hnew : (Atomic.new (0 : U32)).exec World.new = .ok (⟨0⟩, start) := by rfl
  have hinit : Inv d now schedule.val.length
      (parks.closure0.call (schedule.val.length + 1) { c0 := ⟨0⟩, c1 := d, c2 := now, c3 := .None })
      (parks.closure1.call { c0 := ⟨0⟩ }) start :=
    ⟨.start _, .start, rfl, rfl, Or.inl rfl, .start_u0 _ (by omega)⟩
  obtain ⟨ea, eb, w', he, hu, v, hc, h1, h2, h3, h4, h5⟩ :=
    two_ends (Inv d now) (first_step d now) (second_step d now) (Post d) (last_step d now)
      schedule.val _ _ _ hinit
  have hl : (Atomic.load (⟨0⟩ : Atomic .U32) .SeqCst).exec w' =
      .ok (Scalar.wrap .U32 v, w'.touched 0 0 [.SeqCst]) := by
    rw [Atomic.load_eq _ rfl, Prog.exec_op_awake (World.step_load (World.get_single hc) hu.fits) rfl]
    rfl
  apply Prog.value_of_exec (w := w'.touched 0 0 [.SeqCst])
  simp only [parks, parks.at_1, parks.at_2, Threads.two_of, Prog.exec_bind, hnew, he, hl]
  rcases h5 with rfl | rfl | rfl | rfl | ⟨e, rfl⟩
  · rfl
  · cases eb with
    | Done x => exact absurd rfl (h1 rfl)
    | _ => rfl
  · cases eb with
    | Done x => have := h2 rfl rfl; subst this; rfl
    | _ => rfl
  · obtain ⟨ht, hd⟩ := h3 rfl
    cases d with
    | none => cases ht
    | some x =>
      cases eb with
      | Done y => have hv : v = 1 := hd rfl; subst hv; rfl
      | _ => rfl
  · cases eb with
    | Done x => have := h4 e rfl rfl; subst this; rfl
    | _ => rfl

end BlazeCore.Wait.ParkingProof

open BlazeCore.Wait.ParkingProof in
/-- Fuel: one more than the schedule's steps: each turn of the park's loop takes a step. -/
theorem blaze_core.wait.parking.no_unpark_is_lost.spec.proof :
    blaze_core.wait.parking.no_unpark_is_lost.spec.statement := by
  intro schedule now deadline
  exact ⟨schedule.val.length + 1, parks_true _ now schedule⟩

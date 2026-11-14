import Blaze.ThreadsAuto
import Blaze.Tests.ThreadsPark

/-!
No unpark is lost, on every schedule: the proof over the hand-written park and unpark of
`Blaze.Tests.ThreadsPark`, written with `Blaze.ThreadsAuto`.

The park's program points (`Pt`) and the unpark's (`UPt`) name the continuation after each of
their operations; `Good` lists every reachable combination of the two points, the word, who
sleeps and who is woken, with the fuel of the park's loop above the schedule steps left; each
thread's step keeps it (`first_step`, `second_step`), and the theorem is read off its ends.
-/

namespace Blaze.Tests.ThreadsPark

open Blaze Blaze.Threads Blaze.core.sync.atomic

/-! Evaluation first: a park that times out, fails its compare, and is then overtaken by the
unpark before its final swap takes the token the unpark left. -/

-- The unpark swaps and wakes nobody before the park's swap: the park's swap takes the token.
example : (run park 8 true [p, p, p, p 2, p, u, u, p]).value = .ok (.Done true, .Done (), EMPTY) := by
  rfl
-- The unpark swaps, the park swaps (takes the token), then the unpark's wake finds nobody.
example : (run park 8 true [p, p, p, p 2, p, u, p, u]).value = .ok (.Done true, .Done (), EMPTY) := by
  rfl
-- The park leaves first: the unpark's token stays.
example : (run park 8 true [p, p, p, p 2, p, p, u]).value =
    .ok (.Done false, .Done (), NOTIFIED) := by
  rfl
-- The unpark comes between the timeout and the compare: the compare takes the token.
example : (run park 8 true [p, p, p, p 2, u, p, u]).value = .ok (.Done true, .Done (), EMPTY) := by
  rfl
-- The unpark wakes the sleeper; it takes the token.
example : (run park 8 false [p, p, p, u, u, p, p]).value = .ok (.Done true, .Done (), EMPTY) := by
  rfl

namespace Proof

theorem EMPTY_val : EMPTY.val = 0 := by decide
theorem NOTIFIED_val : NOTIFIED.val = 1 := by decide
theorem PARKED_val : PARKED.val = 4294967295 := by decide
theorem wrap_E : (Scalar.wrap .U32 0).val = 0 := by decide
theorem wrap_N : (Scalar.wrap .U32 1).val = 1 := by decide
theorem wrap_P : (Scalar.wrap .U32 4294967295).val = 4294967295 := by decide

/-- The park's program points. -/
inductive Pt where
  /-- Before the first compare (the park as called). -/
  | start (fuel : Nat)
  /-- Before the compare of `EMPTY` to `PARKED`. -/
  | second (fuel : Nat)
  /-- Before the store of `EMPTY`, a token taken. -/
  | store
  /-- In the loop before the wait, or asleep in it (the world says which). -/
  | wait (fuel : Nat)
  /-- After the wait returned with `woke`, before the compare. -/
  | after (fuel : Nat) (woke : Woke)
  /-- Before the swap at a timeout. -/
  | swap
  /-- Returned. -/
  | ret (took : Bool)

/-- The unpark's program points. -/
inductive UPt where
  /-- Before the swap. -/
  | start
  /-- Before the wake. -/
  | wake
  /-- Returned. -/
  | ret

/-- What the park still runs at each point: the same code as `park` and `park.loop`. -/
def parkAt (word : Atomic .U32) (timed : Bool) : Pt → Prog Bool
  | .start fuel => park fuel word timed
  | .second fuel => do
    let parked ← Atomic.compare_exchange word EMPTY PARKED .AcqRel .Acquire
    match parked with
    | .Err _ => do
      Atomic.store word EMPTY .Relaxed
      pure true
    | .Ok _ => park.loop fuel word timed
  | .store => do
    Atomic.store word EMPTY .Relaxed
    pure true
  | .wait fuel => park.loop fuel word timed
  | .after fuel woke => do
    let took ← Atomic.compare_exchange word NOTIFIED EMPTY .Acquire .Relaxed
    match took with
    | .Ok _ => pure true
    | .Err _ =>
      match woke with
      | .TimedOut => do
        let old ← Atomic.swap word EMPTY .Acquire
        pure (decide (old.val = NOTIFIED.val))
      | _ => park.loop fuel word timed
  | .swap => do
    let old ← Atomic.swap word EMPTY .Acquire
    pure (decide (old.val = NOTIFIED.val))
  | .ret took => pure took

/-- What the unpark still runs at each point. -/
def unparkAt (word : Atomic .U32) : UPt → Prog Unit
  | .start => unpark word
  | .wake => do
    let _ ← wake word
    pure ()
  | .ret => pure ()

/-! Each point's shape: one operation, and the point each answer leads to. -/

section shapes

variable (word : Atomic .U32) (t : Bool)

theorem start_shape (k : Nat) : parkAt word t (.start k) =
    .op 0 0 (.update word.cell (fun x => if x = NOTIFIED.val then some EMPTY.val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt word t (.ret true) else parkAt word t (.second k)) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem second_shape (k : Nat) : parkAt word t (.second k) =
    .op 0 0 (.update word.cell (fun x => if x = EMPTY.val then some PARKED.val else none) false
      .AcqRel .Acquire)
      (fun got => if got.flag then parkAt word t (.wait k) else parkAt word t .store) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem store_shape : parkAt word t .store =
    .op 0 0 (.store word.cell EMPTY.val .Relaxed) (fun _ => parkAt word t (.ret true)) :=
  Prog.op_ext fun _ => rfl

theorem wait_shape (k : Nat) : parkAt word t (.wait (k + 1)) =
    .op 0 0 (.wait word.cell PARKED.val t) (fun got => parkAt word t (.after k (Woke.ofGot got))) :=
  Prog.op_ext fun _ => rfl

theorem after_shape (k : Nat) (woke : Woke) : parkAt word t (.after k woke) =
    .op 0 0 (.update word.cell (fun x => if x = NOTIFIED.val then some EMPTY.val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt word t (.ret true)
        else parkAt word t (if woke = .TimedOut then .swap else .wait k)) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> cases woke <;> rfl

theorem swap_shape : parkAt word t .swap =
    .op 0 0 (.update word.cell (fun _ => some EMPTY.val) false .Acquire .Acquire)
      (fun got => parkAt word t (.ret (decide ((Scalar.wrap .U32 got.value).val = NOTIFIED.val)))) :=
  Prog.op_ext fun _ => rfl

theorem ret_shape (b : Bool) : parkAt word t (.ret b) = .done (.ok b) := rfl

theorem ustart_shape : unparkAt word .start =
    .op 0 0 (.update word.cell (fun _ => some NOTIFIED.val) false .Release .Release)
      (fun got => if (Scalar.wrap .U32 got.value).val = PARKED.val then unparkAt word .wake
        else unparkAt word .ret) :=
  Prog.op_ext fun _ => rfl

theorem uwake_shape : unparkAt word .wake =
    .op 0 0 (.wake word.cell false) (fun _ => unparkAt word .ret) :=
  Prog.op_ext fun _ => rfl

theorem uret_shape : unparkAt word .ret = .done (.ok ()) := rfl

end shapes

/-! The invariant: every reachable combination of the park's point, the unpark's point, the
word, who sleeps and who is woken. `n` is the schedule steps left; the park's loop has more fuel
than that, so it never runs out. -/

/-- One thread asleep or woken: the park, thread 1, on the word, cell 0. -/
abbrev S : List (Nat × Nat) := [(1, 0)]

inductive Good (t : Bool) (n : Nat) : Pt → UPt → List Int → List (Nat × Nat) → List (Nat × Nat) → Prop
  | start_u0 (k : Nat) : n < k → Good t n (.start k) .start [0] [] []
  | start_uD (k : Nat) : n < k → Good t n (.start k) .ret [1] [] []
  | second_u0 (k : Nat) : n < k → Good t n (.second k) .start [0] [] []
  | second_uD (k : Nat) : n < k → Good t n (.second k) .ret [1] [] []
  | store_uD : Good t n .store .ret [1] [] []
  | wait_u0 (k : Nat) : n < k → Good t n (.wait k) .start [4294967295] [] []
  | wait_u0_asleep (k : Nat) : n < k → Good t n (.wait k) .start [4294967295] S []
  | wait_u1 (k : Nat) : n < k → Good t n (.wait k) .wake [1] [] []
  | wait_u1_asleep (k : Nat) : n < k → Good t n (.wait k) .wake [1] S []
  | wait_uD (k : Nat) : n < k → Good t n (.wait k) .ret [1] [] []
  | wait_uD_woken (k : Nat) : n < k → Good t n (.wait k) .ret [1] [] S
  | after_u0 (k : Nat) (woke : Woke) : n < k → (woke = .TimedOut → t = true) →
      Good t n (.after k woke) .start [4294967295] [] []
  | after_u1 (k : Nat) (woke : Woke) : n < k → (woke = .TimedOut → t = true) →
      Good t n (.after k woke) .wake [1] [] []
  | after_uD (k : Nat) (woke : Woke) : n < k → (woke = .TimedOut → t = true) →
      Good t n (.after k woke) .ret [1] [] []
  | swap_u0 : t = true → Good t n .swap .start [4294967295] [] []
  | swap_u1 : t = true → Good t n .swap .wake [1] [] []
  | swap_uD : t = true → Good t n .swap .ret [1] [] []
  | took_u1 : Good t n (.ret true) .wake [0] [] []
  | took_uD : Good t n (.ret true) .ret [0] [] []
  | left_u0 : t = true → Good t n (.ret false) .start [0] [] []
  | left_uD : t = true → Good t n (.ret false) .ret [1] [] []

def Inv (t : Bool) (n : Nat) (a : Prog Bool) (b : Prog Unit) (w : World) : Prop :=
  ∃ pt up, a = parkAt ⟨0⟩ t pt ∧ b = unparkAt ⟨0⟩ up ∧ w.OneCell 0 ∧
    Good t n pt up w.cells w.asleep w.woken

/-- The world the threads start in: the word, `EMPTY`. -/
def start : World := ⟨[0], [], [], [], false⟩

set_option hygiene false in
/-- After a step: the point the thread is at and the combination after it. -/
local macro "close" : tactic => `(tactic| (
  simp [wrap_E, wrap_N, wrap_P, NOTIFIED_val, PARKED_val, EMPTY_val, Op.sleeps]
  unfold Inv
  refine ⟨_, _, rfl, rfl, World.OneCell.touched hu, ?_⟩
  simp [hc, hs, hk]
  constructor <;> first | omega | assumption | (intro _; assumption) | (intro h; nomatch h)))

set_option hygiene false in
/-- A thread that has returned stays. -/
local macro "keep" : tactic => `(tactic| (
  unfold Inv
  refine ⟨_, _, rfl, rfl, hu, ?_⟩
  rw [hc, hs, hk]
  constructor <;> first | omega | assumption | (intro _; assumption) | (intro h; nomatch h)))

theorem first_step (t : Bool) (n : Nat) (a : Prog Bool) (b : Prog Unit) (w : World) (pick : Nat)
    (h : Inv t (n + 1) a b w) : Advance 1 pick a w (fun a' w' => Inv t n a' b w') := by
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
    exact .of_step (second_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | second_uD k hn =>
    exact .of_step (second_shape ..) (World.step_cas_err (World.get_single hc) hu.fits (by decide))
      (by close)
  | store_uD =>
    exact .of_step (store_shape ..) (World.step_store (World.get_single_length hc) hu.fits)
      (by close)
  | wait_u0 k hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_sleeps (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by decide)) (by close)
  | wait_u0_asleep k hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    by_cases h1 : pick = 1
    · exact .of_step (wait_shape ..) (World.step_wait_spurious (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1) (by close)
    by_cases h2 : pick = 2 ∧ t = true
    · obtain ⟨h2, ht⟩ := h2
      exact .of_step (wait_shape ..) (World.step_wait_timed_out (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h2 ht) (by close)
    · exact .of_step (wait_shape ..) (World.step_wait_stays (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1 h2) (by close)
  | wait_u1 k hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_changed (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by decide)) (by close)
  | wait_u1_asleep k hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    by_cases h1 : pick = 1
    · exact .of_step (wait_shape ..) (World.step_wait_spurious (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1) (by close)
    by_cases h2 : pick = 2 ∧ t = true
    · obtain ⟨h2, ht⟩ := h2
      exact .of_step (wait_shape ..) (World.step_wait_timed_out (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h2 ht) (by close)
    · exact .of_step (wait_shape ..) (World.step_wait_stays (World.get_single hc) hu.fits
        (by simp [hk]) (by simp [hs]) h1 h2) (by close)
  | wait_uD k hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_changed (World.get_single hc) hu.fits
      (by simp [hk]) (by simp [hs]) (by decide)) (by close)
  | wait_uD_woken k hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact .of_step (wait_shape ..) (World.step_wait_woken (World.get_single hc) hu.fits
      (by simp [hk])) (by close)
  | after_u0 k woke hn hto =>
    cases woke <;> (try have ht := hto rfl) <;>
      exact .of_step (after_shape ..)
        (World.step_cas_err (World.get_single hc) hu.fits (by decide)) (by close)
  | after_u1 k woke hn hto =>
    exact .of_step (after_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | after_uD k woke hn hto =>
    exact .of_step (after_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | swap_u0 ht =>
    exact .of_step (swap_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swap_u1 ht =>
    exact .of_step (swap_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | swap_uD ht =>
    exact .of_step (swap_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | took_u1 => exact .of_done (ret_shape ..) (by keep)
  | took_uD => exact .of_done (ret_shape ..) (by keep)
  | left_u0 ht => exact .of_done (ret_shape ..) (by keep)
  | left_uD ht => exact .of_done (ret_shape ..) (by keep)

theorem second_step (t : Bool) (n : Nat) (a : Prog Bool) (b : Prog Unit) (w : World) (pick : Nat)
    (h : Inv t (n + 1) a b w) : Advance 2 pick b w (fun b' w' => Inv t n a b' w') := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  cases hg with
  | start_u0 | second_u0 | wait_u0 | wait_u0_asleep | after_u0 | swap_u0 | left_u0 =>
    exact .of_step (ustart_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | wait_u1_asleep =>
    exact .of_step (uwake_shape ..) (World.step_wake_single (one := (1, 0))
      (World.get_single_length hc) hu.fits (by simp [hs])) (by close)
  | wait_u1 | after_u1 | swap_u1 | took_u1 =>
    exact .of_step (uwake_shape ..) (World.step_wake_none
      (World.get_single_length hc) hu.fits (by simp [hs])) (by close)
  | start_uD | second_uD | store_uD | wait_uD | wait_uD_woken | after_uD | swap_uD | took_uD
    | left_uD =>
    exact .of_done (uret_shape ..) (by keep)

/-- What the theorem asks of an end, with the word's last value `v`. -/
def Post (t : Bool) (ea : Ended Bool) (eb : Ended Unit) (w : World) : Prop :=
  w.OneCell 0 ∧ ∃ v, w.cells = [v] ∧
    (eb = .Done () → ea ≠ .Asleep) ∧
    (∀ took, ea = .Done took → eb = .Done () →
      (took = true ∧ v = 0) ∨ (took = false ∧ t = true ∧ v = 1)) ∧
    (∀ took, ea = .Done took → t = false → took = true)

set_option hygiene false in
local macro "post" : tactic => `(tactic| (
  simp only [Post]
  refine ⟨hu, _, hc, ?_, ?_, ?_⟩ <;> simp_all [PARKED_val]))

theorem last_step (t : Bool) (a : Prog Bool) (b : Prog Unit) (w : World) (h : Inv t 0 a b w) :
    ∃ ea eb w', (two [] a b).exec w = .ok ((ea, eb), w') ∧ Post t ea eb w' := by
  obtain ⟨pt, up, rfl, rfl, hu, hg⟩ := h
  generalize hc : w.cells = cs at hg
  generalize hs : w.asleep = sl at hg
  generalize hk : w.woken = wk at hg
  cases hg with
  | wait_u0 k hn | wait_u0_asleep k hn | wait_u1 k hn | wait_u1_asleep k hn | wait_uD k hn
    | wait_uD_woken k hn =>
    obtain ⟨k, rfl⟩ : ∃ j, k = j + 1 := ⟨k - 1, by omega⟩
    exact ⟨_, _, _, two_nil_ok (by rw [wait_shape]; exact ended_exec_wait (World.get_single hc))
      rfl, by post⟩
  | _ => exact ⟨_, _, _, two_nil_ok rfl rfl, by post⟩

end Proof

open Proof in
theorem no_unpark_is_lost (timed : Bool) (schedule : List Step) :
    ∃ fuel parked unparked last,
      (run park fuel timed schedule).value = .ok (parked, unparked, last) ∧
      -- the unpark returned: the park is not asleep with no wake coming
      (unparked = .Done () → parked ≠ .Asleep) ∧
      -- both returned: the token was taken once, or the park left at its timeout before it came and it is still there
      (∀ took, parked = .Done took → unparked = .Done () →
          (took = true ∧ last.val = EMPTY.val) ∨ (took = false ∧ timed = true ∧ last.val = NOTIFIED.val)) ∧
      -- a park with no time limit returns only with a token
      (∀ took, parked = .Done took → timed = false → took = true) := by
  have hnew : (Atomic.new EMPTY).exec World.new = .ok (⟨0⟩, Proof.start) := by rfl
  have hinit : Inv timed schedule.length (park (schedule.length + 1) ⟨0⟩ timed) (unpark ⟨0⟩)
      Proof.start := ⟨.start _, .start, rfl, rfl, Or.inl rfl, .start_u0 _ (by omega)⟩
  obtain ⟨ea, eb, w', he, hu, v, hc, h1, h2, h3⟩ :=
    two_ends (Inv timed) (first_step timed) (second_step timed) (Post timed) (last_step timed)
      schedule _ _ _ hinit
  have hl : (Atomic.load (⟨0⟩ : Atomic .U32) .SeqCst).exec w' =
      .ok (Scalar.wrap .U32 v, w'.touched 0 0 [.SeqCst]) := by
    rw [Atomic.load_eq _ rfl, Prog.exec_op_awake (World.step_load (World.get_single hc) hu.fits) rfl]
    rfl
  refine ⟨schedule.length + 1, ea, eb, Scalar.wrap .U32 v, ?_, h1, ?_, h3⟩
  · apply Prog.value_of_exec (w := w'.touched 0 0 [.SeqCst])
    simp only [run, Prog.exec_bind, hnew, he, hl]
    rfl
  · intro took hp hd
    rcases h2 took hp hd with ⟨rfl, rfl⟩ | ⟨rfl, ht, rfl⟩
    · exact .inl ⟨rfl, by rw [wrap_E, EMPTY_val]⟩
    · exact .inr ⟨rfl, ht, by rw [wrap_N, NOTIFIED_val]⟩

end Blaze.Tests.ThreadsPark

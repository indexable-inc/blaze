-- Hand-written, not generated: the proof of the `atomics` fixture's `parks_once`.
import Blaze.ThreadsAuto
import BlazeModel.atomics.parks_once

/-!
No unpark is lost, on every schedule, of the park and unpark blaze translates from the fixture
(crates/hir-ty-lean/src/tests.rs, `ATOMICS`, shaped as crates/blaze-core/src/world/threads.rs's
test park): the port of `Blaze.Tests.ThreadsParkProof` to the emitted models. Each of the
park's and unpark's points is a definition the translator named (`atomics.park.at_1`,
`atomics.park_loop.loop_0.at_2`, `atomics.unpark.at_1`, ...): a point here is that definition
at its arguments, and no model text is copied.
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic

/-! The models are the programs they were before their points were named. -/

example (k : Nat) (w : Atomic .U32) (t : Bool) : atomics.park k w t = (do
    let t2 ← Atomic.compare_exchange w (1 : U32) (0 : U32) .Acquire .Relaxed
    match t2 with
    | .Ok _ => Prog.done (.ok true)
    | .Err _ => do
      let t0 ← Atomic.compare_exchange w (0 : U32) (4294967295 : U32) .AcqRel .Acquire
      match t0 with
      | .Err _ => do
        let _ ← Atomic.store w (0 : U32) .Relaxed
        Prog.done (.ok true)
      | .Ok _ => atomics.park_loop k w t) := rfl

example (k : Nat) (w : Atomic .U32) (t : Bool) : atomics.park_loop.loop_0 (k + 1) w t = (do
    let woke ← wait w (4294967295 : U32) t
    let t0 ← Atomic.compare_exchange w (1 : U32) (0 : U32) .Acquire .Relaxed
    match t0 with
    | .Ok _ => Prog.done (.ok true)
    | .Err _ =>
      match woke with
      | .TimedOut => do
        let t1 ← Atomic.swap w (0 : U32) .Acquire
        Prog.done (.ok (Scalar.eq t1 (1 : U32)))
      | _ => atomics.park_loop.loop_0 k w t) := rfl

example (w : Atomic .U32) : atomics.unpark w = (do
    let t0 ← Atomic.swap w (1 : U32) .Release
    if Scalar.eq t0 (4294967295 : U32) then do
      let _ ← wake w
      Prog.done (.ok ())
    else Prog.done (.ok ())) := rfl

namespace atomics.parks_once.Proof

theorem v0 : (0 : U32).val = 0 := by decide
theorem v1 : (1 : U32).val = 1 := by decide
theorem vP : (4294967295 : U32).val = 4294967295 := by decide
theorem wrap_E : (Scalar.wrap .U32 0).val = 0 := by decide
theorem wrap_N : (Scalar.wrap .U32 1).val = 1 := by decide
theorem wrap_P : (Scalar.wrap .U32 4294967295).val = 4294967295 := by decide

/-- The park's program points. -/
inductive Pt where
  | start (fuel : Nat)
  | second (fuel : Nat)
  | store
  | wait (fuel : Nat)
  | after (fuel : Nat) (woke : Woke)
  | swap
  | ret (took : Bool)

/-- The unpark's program points. -/
inductive UPt where
  | start
  | wake
  | ret

/-- The park at each point: the definition the translator named for it. A point's value that
its code does not read (the failed compare's, the fuel past the loop) is any one. -/
def parkAt (word : Atomic .U32) (t : Bool) : Pt → Prog Bool
  | .start k => atomics.park k word t
  | .second k => atomics.park.at_1 k word t (.Err 0)
  | .store => atomics.park.at_2 0 word t (.Err 0)
  | .wait k => atomics.park_loop.loop_0 k word t
  | .after k woke => atomics.park_loop.loop_0.at_1 (atomics.park_loop.loop_0 k) word t woke
  | .swap => atomics.park_loop.loop_0.at_2 (atomics.park_loop.loop_0 0) .TimedOut word t (.Err 0)
  | .ret took => .done (.ok took)

def unparkAt (word : Atomic .U32) : UPt → Prog Unit
  | .start => atomics.unpark word
  | .wake => atomics.unpark.at_1 word 4294967295
  | .ret => .done (.ok ())

section shapes

variable (word : Atomic .U32) (t : Bool)

theorem start_shape (k : Nat) : parkAt word t (.start k) =
    .op 0 0 (.update word.cell (fun x => if x = (1 : U32).val then some (0 : U32).val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt word t (.ret true) else parkAt word t (.second k)) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem second_shape (k : Nat) : parkAt word t (.second k) =
    .op 0 0 (.update word.cell (fun x => if x = (0 : U32).val then some (4294967295 : U32).val else none)
      false .AcqRel .Acquire)
      (fun got => if got.flag then parkAt word t (.wait k) else parkAt word t .store) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem store_shape : parkAt word t .store =
    .op 0 0 (.store word.cell (0 : U32).val .Relaxed) (fun _ => parkAt word t (.ret true)) :=
  Prog.op_ext fun _ => rfl

theorem wait_shape (k : Nat) : parkAt word t (.wait (k + 1)) =
    .op 0 0 (.wait word.cell (4294967295 : U32).val t)
      (fun got => parkAt word t (.after k (Woke.ofGot got))) :=
  Prog.op_ext fun _ => rfl

theorem after_shape (k : Nat) (woke : Woke) : parkAt word t (.after k woke) =
    .op 0 0 (.update word.cell (fun x => if x = (1 : U32).val then some (0 : U32).val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt word t (.ret true)
        else parkAt word t (if woke = .TimedOut then .swap else .wait k)) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> cases woke <;> rfl

theorem swap_shape : parkAt word t .swap =
    .op 0 0 (.update word.cell (fun _ => some (0 : U32).val) false .Acquire .Acquire)
      (fun got => parkAt word t (.ret (Scalar.eq (Scalar.wrap .U32 got.value) (1 : U32)))) :=
  Prog.op_ext fun _ => rfl

theorem ret_shape (b : Bool) : parkAt word t (.ret b) = .done (.ok b) := rfl

theorem ustart_shape : unparkAt word .start =
    .op 0 0 (.update word.cell (fun _ => some (1 : U32).val) false .Release .Release)
      (fun got => if Scalar.eq (Scalar.wrap .U32 got.value) (4294967295 : U32) then unparkAt word .wake
        else unparkAt word .ret) :=
  Prog.op_ext fun got => by
    show atomics.unpark.at_1 word (Scalar.wrap .U32 got.value) = _
    cases h : Scalar.eq (Scalar.wrap .U32 got.value) (4294967295 : U32) <;>
      simp only [unparkAt, atomics.unpark.at_1, h, ite_true, ite_false, Bool.false_eq_true] <;> rfl

theorem uwake_shape : unparkAt word .wake =
    .op 0 0 (.wake word.cell false) (fun _ => unparkAt word .ret) :=
  Prog.op_ext fun _ => rfl

theorem uret_shape : unparkAt word .ret = .done (.ok ()) := rfl

end shapes

/-- One thread asleep or woken: the park, thread 1, on the word, cell 0. -/
abbrev S : List (Nat × Nat) := [(1, 0)]

/-- Every reachable combination of the park's point, the unpark's point, the word, who sleeps
and who is woken; `n` the schedule steps left, below the park's loop's fuel. -/
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

/-- The world after the word is made: `EMPTY`. -/
def start : World := ⟨[0], [], [], [], false, 0, []⟩

set_option hygiene false in
local macro "close" : tactic => `(tactic| (
  simp [wrap_E, wrap_N, wrap_P, v0, v1, vP, Scalar.eq, Op.sleeps]
  unfold Inv
  refine ⟨_, _, rfl, rfl, World.OneCell.touched hu, ?_⟩
  simp [hc, hs, hk]
  constructor <;> first | omega | assumption | (intro _; assumption) | (intro h; nomatch h)))

set_option hygiene false in
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
  | wait_u0_asleep k hn | wait_u1_asleep k hn =>
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
  | wait_u1 k hn | wait_uD k hn =>
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
  | after_u1 k woke hn hto | after_uD k woke hn hto =>
    exact .of_step (after_shape ..) (World.step_cas_ok (World.get_single hc) hu.fits (by decide))
      (by close)
  | swap_u0 ht | swap_u1 ht | swap_uD ht =>
    exact .of_step (swap_shape ..) (World.step_swap (World.get_single hc) hu.fits) (by close)
  | took_u1 | took_uD => exact .of_done (ret_shape ..) (by keep)
  | left_u0 ht | left_uD ht => exact .of_done (ret_shape ..) (by keep)

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
  refine ⟨hu, _, hc, ?_, ?_, ?_⟩ <;> simp_all [Scalar.eq, wrap_E, wrap_N, wrap_P, v0, v1, vP]))

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

end atomics.parks_once.Proof

open atomics.parks_once.Proof in
/-- No unpark is lost: for every schedule, the park and the unpark the translator emitted end
as `parks_once` asks. -/
theorem atomics.parks_once.spec.proof : atomics.parks_once.spec.statement := by
  intro schedule timed
  have hnew : (Atomic.new (0 : U32)).exec World.new = .ok (⟨0⟩, start) := by rfl
  have hinit : Inv timed schedule.val.length (atomics.park (schedule.val.length + 1) ⟨0⟩ timed)
      (atomics.unpark ⟨0⟩) start :=
    ⟨.start _, .start, rfl, rfl, Or.inl rfl, .start_u0 _ (by omega)⟩
  obtain ⟨ea, eb, w', he, hu, v, hc, h1, h2, h3⟩ :=
    two_ends (Inv timed) (first_step timed) (second_step timed) (Post timed) (last_step timed)
      schedule.val _ _ _ hinit
  have hl : (Atomic.load (⟨0⟩ : Atomic .U32) .SeqCst).exec w' =
      .ok (Scalar.wrap .U32 v, w'.touched 0 0 [.SeqCst]) := by
    rw [Atomic.load_eq _ rfl, Prog.exec_op_awake (World.step_load (World.get_single hc) hu.fits) rfl]
    rfl
  have hb : atomics.parks_once.closure1.call ⟨⟨0⟩⟩ = atomics.unpark ⟨0⟩ := Prog.bind_unit _
  refine ⟨schedule.val.length + 1, Prog.value_of_exec (w := w'.touched 0 0 [.SeqCst]) ?_⟩
  simp only [atomics.parks_once, atomics.parks_once.at_1, Prog.exec_bind, hnew]
  rw [hb]
  simp only [two_of, atomics.parks_once.closure0.call, Prog.exec_bind, he, atomics.parks_once.at_2, hl]
  rcases ea with took | _ | _ <;> rcases eb with _ | _ | _ <;>
    (try cases took) <;> cases timed <;>
    simp_all [atomics.parks_once.at_3, Scalar.eq, wrap_E, wrap_N, v0, v1]

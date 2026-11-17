import Blaze.ThreadsAuto
import BlazeCore.Wait.ParkingLib
import BlazeModel.blaze_core.wait.parking.a_park_left_alone_returns_or_sleeps

/-!
The program points of blaze-core's real `park` and `unpark` on the timed kernel and the world's
clock (`park.Timed_Shown`, `unpark.Timed`, lean/BlazeModel/blaze_core/wait/parking) at the
instance `a_park_left_alone_returns_or_sleeps` runs, and each point's shape: one operation and
the point each answer leads to. Each point is a definition the translator named
(`park.Timed_Shown.at_3`, `park.Timed_Shown.loop_0.at_7`, `unpark.Timed.at_1`, ...) at its
arguments; no model text is copied. Unlike `ParkingLib`'s park, this one reads the world's clock
(`Op.now`) before its first wait and after each wait that returned for something other than its
deadline, and its retry holds that reading.
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic
set_option linter.unusedVariables false

namespace BlazeCore.Wait.ParkingTimeProof

open blaze_core.wait.retry blaze_core.wait.parking
open BlazeCore.Wait.RetryProofs (passedAt after_eq bump bump_val gt_self)
open BlazeCore.Wait.ParkingProof (W whatP Res readyR u32_0 u32_1 u32_P wrap_E wrap_N wrap_P
  waitedOf)

/-- The kernel the park and the unpark are given. -/
abbrev KT : Timed := {}

/-- The park's clock: the world's. -/
abbrev SH : Shown := {}

/-- `PARKED`. -/
abbrev Pk : Int := 4294967295

/-- The deadline in nanoseconds, as the park's wait gives it to the world. -/
def untilNs : Option Deadline → Option Int
  | some v => some (Deadline.get v).toNanos
  | none => none

section points

variable (d : Option Deadline)

/-- The park's retry in its loop: its last reading `t`, `s` returns for nothing in a row. -/
def mkR (t : core.time.Duration) (s : U32) : Retry :=
  { what := whatP, deadline := d, «at» := t, still := s }

/-- The error of the `65536`th return for nothing in a row at the reading `t`. -/
def stuck (s : U32) (t : core.time.Duration) : WaitError :=
  { what := whatP, why := Why.Stuck (bump s) t }

/-- The park's program points. -/
inductive Pt where
  /-- Before the first compare (the park as called), the loop's fuel `k`. -/
  | start (k : Nat)
  /-- Before the compare of `EMPTY` to `PARKED`. -/
  | second (k : Nat)
  /-- Before the store of `EMPTY`, a token taken. -/
  | store
  /-- Before the first reading of the clock, for the retry. -/
  | now0 (k : Nat)
  /-- In the loop before the wait, or asleep in it: the retry's reading `t`, its count `s`. -/
  | wait (k : Nat) (t : core.time.Duration) (s : U32)
  /-- The wait returned with `wk`, before the compare. -/
  | after (k : Nat) (t : core.time.Duration) (s : U32) (wk : Woke)
  /-- Before the reading of the clock after a wait that did not time out. -/
  | now1 (k : Nat) (t : core.time.Duration) (s : U32)
  /-- Before the swap at the deadline. -/
  | swapD
  /-- Before the swap at the retry's error `e`. -/
  | swapE (e : WaitError)
  /-- Returned `r`. -/
  | ret (r : Res)

/-- What the park still runs at each point: the model's definition there. -/
def parkAt : Pt → Prog Res
  | .start k => park.Timed_Shown k W KT whatP d SH .None
  | .second k => park.Timed_Shown.at_1 k .None W whatP d SH KT (.Err 0)
  | .store => park.Timed_Shown.at_2 0 W whatP d SH KT (.Err 0)
  | .now0 k => park.Timed_Shown.at_3 k whatP d SH W KT
  | .wait k t s => park.Timed_Shown.loop_0 k W KT whatP d SH (mkR d t s)
  | .after k t s wk =>
    park.Timed_Shown.loop_0.at_1 (park.Timed_Shown.loop_0 k) W KT whatP d SH (mkR d t s)
      (.Ok (waitedOf wk))
  | .now1 k t s =>
    Shown.Clock.now SH >>=
      park.Timed_Shown.loop_0.at_7 (park.Timed_Shown.loop_0 k) (mkR d t s) W KT whatP d SH
  | .swapD => park.Timed_Shown.loop_0.at_5 W
  | .swapE e => park.Timed_Shown.loop_0.at_9 (.Err e) W
  | .ret r => Prog.done (.ok r)

/-- Where the park goes after the compare that follows a wait fails. -/
def afterNext (k : Nat) (t : core.time.Duration) (s : U32) : Woke → Pt
  | .TimedOut => .swapD
  | _ => .now1 k t s

/-- Where the park goes after it reads `t6` from the clock in its loop. -/
def nowNext (k : Nat) (t : core.time.Duration) (s : U32) (t6 : core.time.Duration) : Pt :=
  if passedAt d t6 = true then .swapD
  else if core.time.Duration.gt t6 t = true then .wait k t6 0
  else if 65536 ≤ s.val + 1 then .swapE (stuck s t6)
  else .wait k t6 (bump s)

/-! Each point's shape. -/

theorem start_shape (k : Nat) : parkAt d (.start k) =
    .op 0 0 (.update 0 (fun x => if x = (1 : U32).val then some (0 : U32).val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt d (.ret readyR) else parkAt d (.second k)) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem second_shape (k : Nat) : parkAt d (.second k) =
    .op 0 0 (.update 0 (fun x => if x = (0 : U32).val then some (4294967295 : U32).val else none)
      false .AcqRel .Acquire)
      (fun got => if got.flag then parkAt d (.now0 k) else parkAt d .store) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem store_shape : parkAt d .store =
    .op 0 0 (.store 0 (0 : U32).val .Relaxed) (fun _ => parkAt d (.ret readyR)) :=
  Prog.op_ext fun _ => rfl

theorem now0_shape (k : Nat) : parkAt d (.now0 k) =
    .op 0 0 (.now true)
      (fun got => parkAt d (.wait k (core.time.Duration.ofNanosSat got.value) 0)) :=
  Prog.op_ext fun _ => Prog.bind_ok _

theorem wait_shape (k : Nat) (t : core.time.Duration) (s : U32) :
    parkAt d (.wait (k + 1) t s) =
      .op 0 0 (.wait_until 0 (4294967295 : U32).val (untilNs d))
        (fun got => parkAt d (.after k t s (Woke.ofGot got))) := by
  have key : ∀ wk : Woke,
      (Timed.Futex.wait.Shown.at_1 wk >>= park.Timed_Shown.loop_0.at_1
        (park.Timed_Shown.loop_0 k) W KT whatP d SH (mkR d t s)) = parkAt d (.after k t s wk) := by
    intro wk; cases wk <;> rfl
  cases d <;> exact Prog.op_ext fun got => key (Woke.ofGot got)

theorem after_shape (k : Nat) (t : core.time.Duration) (s : U32) (wk : Woke) :
    parkAt d (.after k t s wk) =
      .op 0 0 (.update 0 (fun x => if x = (1 : U32).val then some (0 : U32).val else none) false
        .Acquire .Relaxed)
        (fun got => if got.flag then parkAt d (.ret readyR) else parkAt d (afterNext k t s wk)) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> cases wk <;> rfl

theorem now1_shape (k : Nat) (t : core.time.Duration) (s : U32) : parkAt d (.now1 k t s) =
    .op 0 0 (.now true)
      (fun got => park.Timed_Shown.loop_0.at_7 (park.Timed_Shown.loop_0 k) (mkR d t s) W KT whatP
        d SH (core.time.Duration.ofNanosSat got.value)) :=
  Prog.op_ext fun _ => rfl

/-- After the reading in the loop: where the park goes, from a count below `FUEL`. -/
theorem now1_next (k : Nat) (t : core.time.Duration) (s : U32) (t6 : core.time.Duration)
    (hs : s.val < 65536) :
    park.Timed_Shown.loop_0.at_7 (park.Timed_Shown.loop_0 k) (mkR d t s) W KT whatP d SH t6 =
      parkAt d (nowNext d k t s t6) := by
  show (Prog.done (Retry.after (mkR d t s) t6) >>= _) = _
  rw [after_eq (mkR d t s) t6 hs]
  simp only [mkR]
  by_cases hp : passedAt d t6 = true
  · simp only [hp, ite_true, nowNext]; rfl
  · by_cases hg : core.time.Duration.gt t6 t = true
    · simp only [hp, hg, ite_true, nowNext, Bool.false_eq_true, ite_false]; rfl
    · by_cases hb : 65536 ≤ s.val + 1
      · simp only [hp, hg, hb, ite_true, nowNext, Bool.false_eq_true, ite_false]; rfl
      · simp only [hp, hg, hb, nowNext, Bool.false_eq_true, ite_false]; rfl

theorem swapD_shape : parkAt d .swapD =
    .op 0 0 (.update 0 (fun _ => some (0 : U32).val) false .Acquire .Acquire)
      (fun got => park.Timed_Shown.loop_0.at_6 (Scalar.wrap .U32 got.value)) :=
  Prog.op_ext fun _ => rfl

theorem swapE_shape (e : WaitError) : parkAt d (.swapE e) =
    .op 0 0 (.update 0 (fun _ => some (0 : U32).val) false .Acquire .Acquire)
      (fun got => park.Timed_Shown.loop_0.at_10 e (Scalar.wrap .U32 got.value)) :=
  Prog.op_ext fun _ => rfl

theorem ret_shape (r : Res) : parkAt d (.ret r) = .done (.ok r) := rfl

theorem at6_N : park.Timed_Shown.loop_0.at_6 (Scalar.wrap .U32 1) = parkAt d (.ret readyR) := rfl
theorem at6_P :
    park.Timed_Shown.loop_0.at_6 (Scalar.wrap .U32 4294967295) = parkAt d (.ret (.Ok .Deadline)) :=
  rfl
theorem at10_N (e : WaitError) :
    park.Timed_Shown.loop_0.at_10 e (Scalar.wrap .U32 1) = parkAt d (.ret readyR) := rfl
theorem at10_P (e : WaitError) :
    park.Timed_Shown.loop_0.at_10 e (Scalar.wrap .U32 4294967295) = parkAt d (.ret (.Err e)) :=
  rfl

end points

/-- The unpark's program points. -/
inductive UPt where
  | start
  | wake
  | ret

/-- What the unpark still runs at each point: the model's definition there. -/
def unparkAt : UPt → Prog Unit
  | .start => settles.Timed.closure1.call { c0 := W, c1 := KT }
  | .wake => unpark.Timed.at_1 KT W (Scalar.wrap .U32 4294967295)
  | .ret => .done (.ok ())

theorem ustart_shape : unparkAt .start =
    .op 0 0 (.update 0 (fun _ => some (1 : U32).val) false .Release .Release)
      (fun got => unpark.Timed.at_1 KT W (Scalar.wrap .U32 got.value)) :=
  (Prog.bind_unit (unpark.Timed W KT)).trans (Prog.op_ext fun _ => rfl)

theorem uat1_E : unpark.Timed.at_1 KT W (Scalar.wrap .U32 0) = unparkAt .ret := rfl
theorem uat1_N : unpark.Timed.at_1 KT W (Scalar.wrap .U32 1) = unparkAt .ret := rfl
theorem uat1_P : unpark.Timed.at_1 KT W (Scalar.wrap .U32 4294967295) = unparkAt .wake := rfl

theorem uwake_shape : unparkAt .wake = .op 0 0 (.wake 0 false) (fun _ => unparkAt .ret) :=
  Prog.op_ext fun _ => rfl
theorem uret_shape : unparkAt .ret = .done (.ok ()) := rfl

end BlazeCore.Wait.ParkingTimeProof

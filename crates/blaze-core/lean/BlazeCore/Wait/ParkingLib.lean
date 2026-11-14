import Blaze.ThreadsAuto
import BlazeCore.Wait.Retry
import BlazeModel.blaze_core.wait.parking.no_unpark_is_lost

/-!
The program points of blaze-core's real `park` and `unpark` (their generated models,
lean/BlazeModel/blaze_core/wait/parking) at the instance `no_unpark_is_lost` runs, and each
point's shape: one operation and the point each answer leads to. Each point is a definition the
translator named (`park.Shared_Still.at_1`, `park.Shared_Still.loop_0.at_5`,
`unpark.Shared.at_1`, ...) at its arguments; no model text is copied. A point's argument that
its code does not read (a failed compare's value, the fuel past the store) is any one.
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic
set_option linter.unusedVariables false

namespace BlazeCore.Wait.ParkingProof

open blaze_core.wait.retry blaze_core.wait.parking
open BlazeCore.Wait.RetryProofs (passedAt after_eq bump bump_val gt_self)

/-- The word: cell 0. -/
abbrev W : Atomic .U32 := ⟨0⟩

/-- The kernel the park and the unpark are given. -/
abbrev K : Shared := {}

/-- The park's `what`. -/
def whatP : Str := Str.ofBytes [97, 32, 112, 97, 114, 107]

/-- What the park returns. -/
abbrev Res := core.result.Result Waited.unit WaitError

/-- The park's answer with the token. -/
abbrev readyR : Res := .Ok (.Ready ())

theorem u32_0 : (0 : U32).val = 0 := Scalar.ofNat_val .U32 0 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
theorem u32_1 : (1 : U32).val = 1 := Scalar.ofNat_val .U32 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
theorem u32_P : (4294967295 : U32).val = 4294967295 :=
  Scalar.ofNat_val .U32 4294967295 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
theorem wrap_E : (Scalar.wrap .U32 0).val = 0 := by decide
theorem wrap_N : (Scalar.wrap .U32 1).val = 1 := by decide
theorem wrap_P : (Scalar.wrap .U32 4294967295).val = 4294967295 := by decide

/-- Why a wait returned, as the park's kernel says it (`Shared::wait`). -/
def waitedOf : Woke → Waited.unit
  | .Woken => .Ready ()
  | .Changed => .Ready ()
  | .Spurious => .Ready ()
  | .TimedOut => .Deadline

section points

variable (d : Option Deadline) (now : core.time.Duration)

/-- The park's clock: it stands still at `now`. -/
abbrev clk : Still := { f0 := now }

/-- The park's retry in its loop: the clock stands still at `now`, `s` returns for nothing in
a row. -/
def mkR (s : U32) : Retry := { what := whatP, deadline := d, «at» := now, still := s }

/-- The error of the `65536`th return for nothing in a row, the count `s` before it. -/
def stuck (s : U32) : WaitError := { what := whatP, why := Why.Stuck (bump s) now }

/-- The park's program points. -/
inductive Pt where
  /-- Before the first compare (the park as called), the loop's fuel `k`. -/
  | start (k : Nat)
  /-- Before the compare of `EMPTY` to `PARKED`. -/
  | second (k : Nat)
  /-- Before the store of `EMPTY`, a token taken. -/
  | store
  /-- In the loop before the wait, or asleep in it, `s` returns for nothing in a row. -/
  | wait (k : Nat) (s : U32)
  /-- The wait returned with `wk`, before the compare. -/
  | after (k : Nat) (s : U32) (wk : Woke)
  /-- Before the swap at the deadline. -/
  | swapD
  /-- Before the swap at the retry's error `e`. -/
  | swapE (e : WaitError)
  /-- Returned `r`. -/
  | ret (r : Res)

/-- What the park still runs at each point: the model's definition there. -/
def parkAt : Pt → Prog Res
  | .start k => park.Shared_Still k W K whatP d (clk now) .None
  | .second k => park.Shared_Still.at_1 k .None W whatP d (clk now) K (.Err 0)
  | .store => park.Shared_Still.at_2 0 W whatP d (clk now) K (.Err 0)
  | .wait k s => park.Shared_Still.loop_0 k W K whatP d (clk now) (mkR d now s)
  | .after k s wk =>
    park.Shared_Still.loop_0.at_1 (park.Shared_Still.loop_0 k) W K whatP d (clk now) (mkR d now s)
      (.Ok (waitedOf wk))
  | .swapD => park.Shared_Still.loop_0.at_5 W
  | .swapE e => park.Shared_Still.loop_0.at_8 (.Err e) W
  | .ret r => Prog.done (.ok r)

/-- After the compare that follows a wait fails: the loop's point there. -/
abbrev afterFail (k : Nat) (s : U32) (wk : Woke) : Prog Res :=
  park.Shared_Still.loop_0.at_2 (park.Shared_Still.loop_0 k) (waitedOf wk) W K whatP d (clk now)
    (mkR d now s) (.Err 0)

/-- Where the park goes after the compare that follows a wait fails. -/
def afterNext (k : Nat) (s : U32) : Woke → Pt
  | .TimedOut => .swapD
  | _ =>
    if passedAt d now = true then .swapD
    else if 65536 ≤ s.val + 1 then .swapE (stuck now s)
    else .wait k (bump s)

/-! Each point's shape. -/

theorem start_shape (k : Nat) : parkAt d now (.start k) =
    .op 0 0 (.update 0 (fun x => if x = (1 : U32).val then some (0 : U32).val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt d now (.ret readyR) else parkAt d now (.second k)) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

/-- The loop's entry (`at_3`) binds the loop's value and returns it: the loop itself. -/
theorem at3_eq (k : Nat) :
    park.Shared_Still.at_3 k whatP d (clk now) W K = parkAt d now (.wait k 0) :=
  Prog.bind_ok _

theorem second_shape (k : Nat) : parkAt d now (.second k) =
    .op 0 0 (.update 0 (fun x => if x = (0 : U32).val then some (4294967295 : U32).val else none)
      false .AcqRel .Acquire)
      (fun got => if got.flag then parkAt d now (.wait k 0) else parkAt d now .store) :=
  Prog.op_ext fun ⟨_, f⟩ => by
    cases f
    · rfl
    · exact at3_eq d now k

theorem store_shape : parkAt d now .store =
    .op 0 0 (.store 0 (0 : U32).val .Relaxed) (fun _ => parkAt d now (.ret readyR)) :=
  Prog.op_ext fun _ => rfl

theorem wait_shape (k : Nat) (s : U32) : parkAt d now (.wait (k + 1) s) =
    .op 0 0 (.wait 0 (4294967295 : U32).val (core.option.Option.is_some d))
      (fun got => parkAt d now (.after k s (Woke.ofGot got))) :=
  Prog.op_ext fun got => by
    show (Shared.Futex.wait.Still.at_1 (Woke.ofGot got) >>= park.Shared_Still.loop_0.at_1
      (park.Shared_Still.loop_0 k) W K whatP d (clk now) (mkR d now s)) =
      parkAt d now (.after k s (Woke.ofGot got))
    generalize Woke.ofGot got = wk
    cases wk <;> rfl

theorem after_shape (k : Nat) (s : U32) (wk : Woke) : parkAt d now (.after k s wk) =
    .op 0 0 (.update 0 (fun x => if x = (1 : U32).val then some (0 : U32).val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt d now (.ret readyR) else afterFail d now k s wk) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem swapD_shape : parkAt d now .swapD =
    .op 0 0 (.update 0 (fun _ => some (0 : U32).val) false .Acquire .Acquire)
      (fun got => park.Shared_Still.loop_0.at_6 (Scalar.wrap .U32 got.value)) :=
  Prog.op_ext fun _ => rfl

theorem swapE_shape (e : WaitError) : parkAt d now (.swapE e) =
    .op 0 0 (.update 0 (fun _ => some (0 : U32).val) false .Acquire .Acquire)
      (fun got => park.Shared_Still.loop_0.at_9 e (Scalar.wrap .U32 got.value)) :=
  Prog.op_ext fun _ => rfl

theorem ret_shape (r : Res) : parkAt d now (.ret r) = .done (.ok r) := rfl

theorem at6_N : park.Shared_Still.loop_0.at_6 (Scalar.wrap .U32 1) = parkAt d now (.ret readyR) :=
  rfl
theorem at6_P :
    park.Shared_Still.loop_0.at_6 (Scalar.wrap .U32 4294967295) = parkAt d now (.ret (.Ok .Deadline)) :=
  rfl
theorem at9_N (e : WaitError) :
    park.Shared_Still.loop_0.at_9 e (Scalar.wrap .U32 1) = parkAt d now (.ret readyR) := rfl
theorem at9_P (e : WaitError) :
    park.Shared_Still.loop_0.at_9 e (Scalar.wrap .U32 4294967295) = parkAt d now (.ret (.Err e)) :=
  rfl

/-- A deadline that has passed is one. -/
theorem is_some_of_passed (h : passedAt d now = true) : core.option.Option.is_some d = true := by
  cases d with
  | none => simp [passedAt] at h
  | some _ => rfl

/-- After a failed compare: where the park goes, from a count below `FUEL`. -/
theorem afterFail_eq (k : Nat) (s : U32) (wk : Woke) (hs : s.val < 65536) :
    afterFail d now k s wk = parkAt d now (afterNext d now k s wk) := by
  have ha := after_eq (mkR d now s) now hs
  have hr : afterFail d now k s .Woken = parkAt d now (afterNext d now k s .Woken) := by
    show (Prog.done (Retry.after (mkR d now s) now) >>= _) = _
    rw [ha]
    simp only [mkR, gt_self]
    by_cases hp : passedAt d now = true
    · simp only [hp, ite_true, afterNext]; rfl
    · by_cases hb : 65536 ≤ s.val + 1
      · simp only [hp, hb, ite_true, afterNext, Bool.false_eq_true, ite_false]; rfl
      · simp only [hp, hb, afterNext, Bool.false_eq_true, ite_false]; rfl
  cases wk with
  | TimedOut => rfl
  | Woken => exact hr
  | Changed => exact hr
  | Spurious => exact hr

end points

/-- The unpark's program points. -/
inductive UPt where
  | start
  | wake
  | ret

/-- What the unpark still runs at each point: the model's definition there. -/
def unparkAt : UPt → Prog Unit
  | .start => parks.closure1.call { c0 := W }
  | .wake => unpark.Shared.at_1 K W (Scalar.wrap .U32 4294967295)
  | .ret => .done (.ok ())

theorem ustart_shape : unparkAt .start =
    .op 0 0 (.update 0 (fun _ => some (1 : U32).val) false .Release .Release)
      (fun got => unpark.Shared.at_1 K W (Scalar.wrap .U32 got.value)) :=
  (Prog.bind_unit (unpark.Shared W K)).trans (Prog.op_ext fun _ => rfl)

theorem uat1_E : unpark.Shared.at_1 K W (Scalar.wrap .U32 0) = unparkAt .ret := rfl
theorem uat1_P : unpark.Shared.at_1 K W (Scalar.wrap .U32 4294967295) = unparkAt .wake := rfl

theorem uwake_shape : unparkAt .wake = .op 0 0 (.wake 0 false) (fun _ => unparkAt .ret) :=
  Prog.op_ext fun _ => rfl
theorem uret_shape : unparkAt .ret = .done (.ok ()) := rfl

end BlazeCore.Wait.ParkingProof

import Blaze.ThreadsAuto
import BlazeCore.Wait.Retry
import BlazeModel.blaze_core.wait.parking.no_unpark_is_lost

/-!
The program points of blaze-core's real `park` and `unpark` (their generated models,
lean/BlazeModel/blaze_core/wait/parking) at the instance `no_unpark_is_lost` runs, and each
point's shape: one operation and the point each answer leads to. Where the model's
continuation is no nameable function, the point's program is a copy of the model's text at
that point (`afterProg`, `readyNext`, `swapDProg`, `swapEProg`), equal to it by `rfl`.
-/

open Blaze Blaze.Threads Blaze.core.sync.atomic
set_option linter.unusedVariables false

namespace BlazeCore.Wait.ParkingProof

open blaze_core.wait.retry blaze_core.wait.parking
open BlazeCore.Wait.RetryProofs (passedAt after_eq bump bump_val gt_self)

/-- The word: cell 0. -/
abbrev W : Atomic .U32 := ⟨0⟩

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

/-- The swap at the deadline and its answer: the model's text. -/
def swapDProg : Prog Res := do
  let t2 ←
    core.sync.atomic.Atomic.swap W (0 : U32) (core.sync.atomic.Ordering.Acquire : core.sync.atomic.Ordering)
  let t3 := Scalar.eq (1 : U32) t2
  let t4 :=
    if t3 then
      (blaze_core.wait.retry.Waited.unit.Ready () : blaze_core.wait.retry.Waited.unit)
    else (blaze_core.wait.retry.Waited.unit.Deadline : blaze_core.wait.retry.Waited.unit)
  Threads.Prog.done (Result.ok (core.result.Result.Ok t4 : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))

/-- What the deadline's swap returns, from what it read. -/
def swapDk (t2 : U32) : Prog Res :=
  let t3 := Scalar.eq (1 : U32) t2
  let t4 :=
    if t3 then
      (blaze_core.wait.retry.Waited.unit.Ready () : blaze_core.wait.retry.Waited.unit)
    else (blaze_core.wait.retry.Waited.unit.Deadline : blaze_core.wait.retry.Waited.unit)
  Threads.Prog.done (Result.ok (core.result.Result.Ok t4 : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))

/-- The swap at the retry's error and its answer: the model's text. -/
def swapEProg (e : WaitError) : Prog Res := do
  let t6 ←
    core.sync.atomic.Atomic.swap W (0 : U32) (core.sync.atomic.Ordering.Acquire : core.sync.atomic.Ordering)
  if Scalar.eq (1 : U32) t6 then
    Threads.Prog.done (Result.ok (core.result.Result.Ok (blaze_core.wait.retry.Waited.unit.Ready () : blaze_core.wait.retry.Waited.unit) : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))
  else
    Threads.Prog.done (Result.ok (core.result.Result.Err e : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))

/-- What the error's swap returns, from what it read. -/
def swapEk (e : WaitError) (t6 : U32) : Prog Res :=
  if Scalar.eq (1 : U32) t6 then
    Threads.Prog.done (Result.ok (core.result.Result.Ok (blaze_core.wait.retry.Waited.unit.Ready () : blaze_core.wait.retry.Waited.unit) : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))
  else
    Threads.Prog.done (Result.ok (core.result.Result.Err e : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))

/-- After a return for nothing and a failed compare: the retry's step, the model's text. -/
def readyNext (k : Nat) (s : U32) : Prog Res := do
  let (t5, retry) ←
    blaze_core.wait.retry.Retry.after (mkR d now s) now
  match t5 with
  | core.result.Result.Ok v2 =>
    match v2 with
    | blaze_core.wait.retry.Next.Deadline =>
      let passed := true
      if passed then swapDProg
      else
        blaze_core.wait.parking.park.Shared_Still.loop_0 k W ({  } : blaze_core.wait.parking.Shared) whatP d ({ f0 := now } : blaze_core.wait.parking.Still) retry
    | blaze_core.wait.retry.Next.Again =>
      let passed := false
      if passed then swapDProg
      else
        blaze_core.wait.parking.park.Shared_Still.loop_0 k W ({  } : blaze_core.wait.parking.Shared) whatP d ({ f0 := now } : blaze_core.wait.parking.Still) retry
  | core.result.Result.Err v3 => swapEProg v3

/-- After a failed compare, by why the wait returned. -/
def afterFail (k : Nat) (s : U32) (wk : Woke) : Prog Res :=
  match waitedOf wk with
  | blaze_core.wait.retry.Waited.unit.Deadline =>
    let passed := true
    if passed then swapDProg
    else
      blaze_core.wait.parking.park.Shared_Still.loop_0 k W ({  } : blaze_core.wait.parking.Shared) whatP d ({ f0 := now } : blaze_core.wait.parking.Still) (mkR d now s)
  | blaze_core.wait.retry.Waited.unit.Ready v1 => readyNext d now k s

/-- The loop after the wait returned with `wk`: the model's text. -/
def afterProg (k : Nat) (s : U32) (wk : Woke) : Prog Res := do
  let t1 ←
    core.sync.atomic.Atomic.compare_exchange W (1 : U32) (0 : U32) (core.sync.atomic.Ordering.Acquire : core.sync.atomic.Ordering) (core.sync.atomic.Ordering.Relaxed : core.sync.atomic.Ordering)
  if core.result.Result.is_ok t1 then
    Threads.Prog.done (Result.ok (core.result.Result.Ok (blaze_core.wait.retry.Waited.unit.Ready () : blaze_core.wait.retry.Waited.unit) : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))
  else afterFail d now k s wk

/-- What the park still runs at each point. -/
def parkAt : Pt → Prog Res
  | .start k =>
    park.Shared_Still k W ({  } : blaze_core.wait.parking.Shared) whatP d
      ({ f0 := now } : blaze_core.wait.parking.Still) .None
  | .second k => do
    let t9 ←
      core.sync.atomic.Atomic.compare_exchange W (0 : U32) (4294967295 : U32) (core.sync.atomic.Ordering.AcqRel : core.sync.atomic.Ordering) (core.sync.atomic.Ordering.Acquire : core.sync.atomic.Ordering)
    if core.result.Result.is_err t9 then do
      let t10 ←
        core.sync.atomic.Atomic.store W (0 : U32) (core.sync.atomic.Ordering.Relaxed : core.sync.atomic.Ordering)
      Threads.Prog.done (Result.ok (core.result.Result.Ok (blaze_core.wait.retry.Waited.unit.Ready () : blaze_core.wait.retry.Waited.unit) : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))
    else
      let retry :=
        blaze_core.wait.retry.Retry.new whatP d (blaze_core.wait.parking.Still.Clock.now ({ f0 := now } : blaze_core.wait.parking.Still))
      blaze_core.wait.parking.park.Shared_Still.loop_0 k W ({  } : blaze_core.wait.parking.Shared) whatP d ({ f0 := now } : blaze_core.wait.parking.Still) retry
  | .store => do
    let t10 ←
      core.sync.atomic.Atomic.store W (0 : U32) (core.sync.atomic.Ordering.Relaxed : core.sync.atomic.Ordering)
    Threads.Prog.done (Result.ok (core.result.Result.Ok (blaze_core.wait.retry.Waited.unit.Ready () : blaze_core.wait.retry.Waited.unit) : core.result.Result blaze_core.wait.retry.Waited.unit blaze_core.wait.retry.WaitError))
  | .wait k s =>
    park.Shared_Still.loop_0 k W ({  } : blaze_core.wait.parking.Shared) whatP d
      ({ f0 := now } : blaze_core.wait.parking.Still) (mkR d now s)
  | .after k s wk => afterProg d now k s wk
  | .swapD => swapDProg
  | .swapE e => swapEProg e
  | .ret r => Prog.done (.ok r)

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

theorem second_shape (k : Nat) : parkAt d now (.second k) =
    .op 0 0 (.update 0 (fun x => if x = (0 : U32).val then some (4294967295 : U32).val else none)
      false .AcqRel .Acquire)
      (fun got => if got.flag then parkAt d now (.wait k 0) else parkAt d now .store) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem store_shape : parkAt d now .store =
    .op 0 0 (.store 0 (0 : U32).val .Relaxed) (fun _ => parkAt d now (.ret readyR)) :=
  Prog.op_ext fun _ => rfl

theorem wait_shape (k : Nat) (s : U32) : parkAt d now (.wait (k + 1) s) =
    .op 0 0 (.wait 0 (4294967295 : U32).val (core.option.Option.is_some d))
      (fun got => parkAt d now (.after k s (Woke.ofGot got))) :=
  Prog.op_ext fun _ => rfl

theorem after_shape (k : Nat) (s : U32) (wk : Woke) : parkAt d now (.after k s wk) =
    .op 0 0 (.update 0 (fun x => if x = (1 : U32).val then some (0 : U32).val else none) false
      .Acquire .Relaxed)
      (fun got => if got.flag then parkAt d now (.ret readyR) else afterFail d now k s wk) :=
  Prog.op_ext fun ⟨_, f⟩ => by cases f <;> rfl

theorem swapD_shape : parkAt d now .swapD =
    .op 0 0 (.update 0 (fun _ => some (0 : U32).val) false .Acquire .Acquire)
      (fun got => swapDk (Scalar.wrap .U32 got.value)) :=
  Prog.op_ext fun _ => rfl

theorem swapE_shape (e : WaitError) : parkAt d now (.swapE e) =
    .op 0 0 (.update 0 (fun _ => some (0 : U32).val) false .Acquire .Acquire)
      (fun got => swapEk e (Scalar.wrap .U32 got.value)) :=
  Prog.op_ext fun _ => rfl

theorem ret_shape (r : Res) : parkAt d now (.ret r) = .done (.ok r) := rfl

theorem swapDk_N : swapDk (Scalar.wrap .U32 1) = parkAt d now (.ret readyR) := rfl
theorem swapDk_P : swapDk (Scalar.wrap .U32 4294967295) = parkAt d now (.ret (.Ok .Deadline)) :=
  rfl
theorem swapEk_N (e : WaitError) : swapEk e (Scalar.wrap .U32 1) = parkAt d now (.ret readyR) :=
  rfl
theorem swapEk_P (e : WaitError) :
    swapEk e (Scalar.wrap .U32 4294967295) = parkAt d now (.ret (.Err e)) := rfl

/-- A deadline that has passed is one. -/
theorem is_some_of_passed (h : passedAt d now = true) : core.option.Option.is_some d = true := by
  cases d with
  | none => simp [passedAt] at h
  | some _ => rfl

/-- After a failed compare: where the park goes, from a count below `FUEL`. -/
theorem afterFail_eq (k : Nat) (s : U32) (wk : Woke) (hs : s.val < 65536) :
    afterFail d now k s wk = parkAt d now (afterNext d now k s wk) := by
  have hr : readyNext d now k s = parkAt d now (afterNext d now k s .Woken) := by
    unfold readyNext
    rw [after_eq _ _ hs]
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

/-- What the unpark still runs at each point. -/
def unparkAt : UPt → Prog Unit
  | .start => parks.closure1.call { c0 := W }
  | .wake => .op 0 0 (.wake 0 false) (fun _ => .done (.ok ()))
  | .ret => .done (.ok ())

/-- What the unpark does after its swap, from what it read: the model's text. -/
def unparkK (t0 : U32) : Prog Unit :=
  (do
    let t1 := Scalar.eq t0 (4294967295 : U32)
    let _ ←
      if t1 then do
        let _ ← blaze_core.wait.parking.Shared.Futex.wake ({  } : blaze_core.wait.parking.Shared) W
        Threads.Prog.done (Result.ok ())
      else Threads.Prog.done (Result.ok ())
    Threads.Prog.done (Result.ok ()) : Prog Unit) >>= fun _ => Threads.Prog.done (Result.ok ())

theorem ustart_shape : unparkAt .start =
    .op 0 0 (.update 0 (fun _ => some (1 : U32).val) false .Release .Release)
      (fun got => unparkK (Scalar.wrap .U32 got.value)) :=
  Prog.op_ext fun _ => rfl

theorem unparkK_E : unparkK (Scalar.wrap .U32 0) = unparkAt .ret := rfl
theorem unparkK_P : unparkK (Scalar.wrap .U32 4294967295) = unparkAt .wake := rfl

theorem uwake_shape : unparkAt .wake = .op 0 0 (.wake 0 false) (fun _ => unparkAt .ret) := rfl
theorem uret_shape : unparkAt .ret = .done (.ok ()) := rfl

end BlazeCore.Wait.ParkingProof

import Blaze
import Blaze.Mir.Lemmas
import BlazeModel.early.clamp
import BlazeMir.early.clamp

/-!
`early::clamp`: the functional model (`BlazeModel.early.clamp`) agrees with the semantics of the
function's MIR (`BlazeMir.early.clamp`), for every argument: objective V7
(docs/v2/plan/design/formal-verification.md). The proof is the lemmas of `Blaze.Mir.Lemmas`
applied in the body's block order, one theorem per block; it is the shape the translator is
to emit with each model. The embedded body is a prediction of the encoder's output until its
golden test has run: when it changes, the block theorems change with it.
-/

open Blaze Blaze.Mir

namespace BlazeAgrees.early.clamp

set_option maxRecDepth 8192

abbrev P : Program := early.clamp.mir

/-- The one frame: the block to run, `x` and `limit` (locals 1 and 2), and what locals 0
and 3 hold. -/
def at_ (block : BlockId) (s0 s3 : Slot) (x limit : U32) : State :=
  { base := [],
    stack :=
      [({ id := 1, fn := 0, block := block,
          locals :=
            [s0, Slot.init (Value.int ScalarTy.U32 x),
             Slot.init (Value.int ScalarTy.U32 limit), s3],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- The state a call starts in. -/
theorem start_entry (x limit : U32) :
    P.start 0 [] [Value.int ScalarTy.U32 x, Value.int ScalarTy.U32 limit] =
      Outcome.ok (at_ 0 Slot.uninit Slot.uninit x limit) := rfl

def cmpStmt : Statement :=
  Statement.assign (Place.mk 3 [])
    (Rvalue.binary BinOp.gt (Operand.copy (Place.mk 1 [])) (Operand.copy (Place.mk 2 [])))

def switchTerm : Terminator :=
  Terminator.switchInt (Operand.move (Place.mk 3 [])) [(0, 1)] 2

/-- Block 0's statement: `x > limit` is in local 3. -/
theorem compared (s0 s3 : Slot) (x limit : U32) :
    execStmt P.adts (at_ 0 s0 s3 x limit) cmpStmt =
      Outcome.ok (at_ 0 s0 (Slot.init (Value.bool (Scalar.gt x limit))) x limit) :=
  execStmt_assign P.adts (at_ 0 s0 s3 x limit) (Place.mk 3 [])
    (Rvalue.binary BinOp.gt (Operand.copy (Place.mk 1 [])) (Operand.copy (Place.mk 2 [])))
    (Value.bool (Scalar.gt x limit))
    (at_ 0 s0 s3 x limit) { frame := 1, slot := 3, path := [] }
    (at_ 0 s0 (Slot.init (Value.bool (Scalar.gt x limit))) x limit)
    (evalTwo_ok (at_ 0 s0 s3 x limit) (Operand.copy (Place.mk 1 []))
      (Operand.copy (Place.mk 2 [])) (binaryOp false BinOp.gt)
      (Value.int ScalarTy.U32 x) (at_ 0 s0 s3 x limit)
      (Value.int ScalarTy.U32 limit) (at_ 0 s0 s3 x limit)
      (Value.bool (Scalar.gt x limit))
      rfl rfl
      ((binaryOp_same BinOp.gt x limit (by decide) (by decide)).trans rfl))
    rfl rfl

theorem switch_true (s0 : Slot) (x limit : U32) :
    execBlockFrom P.adts P.sig (at_ 0 s0 (Slot.init (Value.bool true)) x limit) []
        switchTerm =
      Outcome.ok (Next.state (at_ 2 s0 Slot.moved x limit)) := rfl

theorem switch_false (s0 : Slot) (x limit : U32) :
    execBlockFrom P.adts P.sig (at_ 0 s0 (Slot.init (Value.bool false)) x limit) []
        switchTerm =
      Outcome.ok (Next.state (at_ 1 s0 Slot.moved x limit)) := rfl

/-- Block 0 when `x > limit`: on to the early return. -/
theorem step0_true (s0 s3 : Slot) (x limit : U32) (hgt : Scalar.gt x limit = true) :
    step P (at_ 0 s0 s3 x limit) = Outcome.ok (Next.state (at_ 2 s0 Slot.moved x limit)) := by
  have hs :
      execBlockFrom P.adts P.sig (at_ 0 s0 s3 x limit) [cmpStmt] switchTerm =
        execBlockFrom P.adts P.sig
          (at_ 0 s0 (Slot.init (Value.bool (Scalar.gt x limit))) x limit) [] switchTerm :=
    execBlockFrom_step P.adts P.sig _ cmpStmt [] switchTerm _ (compared s0 s3 x limit)
  rw [hgt] at hs
  show execBlockFrom P.adts P.sig (at_ 0 s0 s3 x limit) [cmpStmt] switchTerm = _
  exact hs.trans (switch_true s0 x limit)

/-- Block 0 when `x <= limit`: on to the end. -/
theorem step0_false (s0 s3 : Slot) (x limit : U32) (hgt : Scalar.gt x limit = false) :
    step P (at_ 0 s0 s3 x limit) = Outcome.ok (Next.state (at_ 1 s0 Slot.moved x limit)) := by
  have hs :
      execBlockFrom P.adts P.sig (at_ 0 s0 s3 x limit) [cmpStmt] switchTerm =
        execBlockFrom P.adts P.sig
          (at_ 0 s0 (Slot.init (Value.bool (Scalar.gt x limit))) x limit) [] switchTerm :=
    execBlockFrom_step P.adts P.sig _ cmpStmt [] switchTerm _ (compared s0 s3 x limit)
  rw [hgt] at hs
  show execBlockFrom P.adts P.sig (at_ 0 s0 s3 x limit) [cmpStmt] switchTerm = _
  exact hs.trans (switch_false s0 x limit)

/-- Block 1: the function returns `x`. -/
theorem step1 (s3 : Slot) (x limit : U32) :
    step P (at_ 1 Slot.uninit s3 x limit) =
      Outcome.ok (Next.done (Value.int ScalarTy.U32 x)) := rfl

/-- Block 2: the function returns `limit`. -/
theorem step2 (s3 : Slot) (x limit : U32) :
    step P (at_ 2 Slot.uninit s3 x limit) =
      Outcome.ok (Next.done (Value.int ScalarTy.U32 limit)) := rfl

/-- The model of `clamp` is what the semantics of its MIR returns, for every `x` and
`limit` and two steps of fuel or more. -/
theorem agrees (x limit : U32) (extra : Nat) :
    run P (extra + 1 + 1) (at_ 0 Slot.uninit Slot.uninit x limit) =
      Outcome.ok (Value.int ScalarTy.U32 (early.clamp x limit)) := by
  by_cases hgt : Scalar.gt x limit = true
  · have hm : early.clamp x limit = limit := if_pos hgt
    rw [run_step P (extra + 1) _ _ (step0_true Slot.uninit Slot.uninit x limit hgt), hm]
    exact run_done P extra _ _ (step2 Slot.moved x limit)
  · have hf : Scalar.gt x limit = false := by
      cases hb : Scalar.gt x limit with
      | true => exact absurd hb hgt
      | false => rfl
    have hm : early.clamp x limit = x := if_neg hgt
    rw [run_step P (extra + 1) _ _ (step0_false Slot.uninit Slot.uninit x limit hf), hm]
    exact run_done P extra _ _ (step1 Slot.moved x limit)

end BlazeAgrees.early.clamp

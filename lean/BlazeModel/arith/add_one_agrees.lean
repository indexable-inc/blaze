import Blaze
import Blaze.Mir.Lemmas
import BlazeModel.arith.add_one
import BlazeMir.arith.add_one

/-!
`arith::add_one`: the functional model (`BlazeModel.arith.add_one`) agrees with the semantics of the
function's MIR (`BlazeMir.arith.add_one`), for every argument: objective V7
(docs/v2/plan/design/formal-verification.md). The proof is the lemmas of `Blaze.Mir.Lemmas`
applied in the body's block order, one theorem per block; it is the shape the translator is
to emit with each model. The embedded body is a prediction of the encoder's output until its
golden test has run: when it changes, the block theorems change with it.
-/

open Blaze Blaze.Mir

namespace BlazeAgrees.arith.add_one

set_option maxRecDepth 8192

abbrev P : Program := arith.add_one.mir

/-- The one frame: what local 0 holds, and `x` (local 1). -/
def at_ (s0 : Slot) (x : U32) : State :=
  { base := [],
    stack :=
      [({ id := 1, fn := 0, block := 0,
          locals := [s0, Slot.init (Value.int ScalarTy.U32 x)],
          dest := { frame := 0, slot := 0, path := [] }, target := none } : Frame)],
    next := 2 }

/-- The state a call starts in. -/
theorem start_entry (x : U32) :
    P.start 0 [] [Value.int ScalarTy.U32 x] = Outcome.ok (at_ Slot.uninit x) := rfl

def addStmt : Statement :=
  Statement.assign (Place.mk 0 [])
    (Rvalue.checked BinOp.add (Operand.copy (Place.mk 1 []))
      (Operand.const (Const.int ScalarTy.U32 1)))

/-- `x + 1` in range: the function returns it, as the model does. -/
theorem agrees_ok (x : U32) (extra : Nat)
    (h : ScalarTy.min ScalarTy.U32 ≤ x.val + (1 : U32).val ∧
      x.val + (1 : U32).val ≤ ScalarTy.max ScalarTy.U32) :
    run P (extra + 1) (at_ Slot.uninit x) =
      Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult (arith.add_one x)) := by
  have hs : execStmt P.adts (at_ Slot.uninit x) addStmt =
      Outcome.ok
        (at_ (Slot.init (Value.int ScalarTy.U32 ⟨x.val + (1 : U32).val, h.1, h.2⟩)) x) :=
    execStmt_assign P.adts (at_ Slot.uninit x) (Place.mk 0 [])
      (Rvalue.checked BinOp.add (Operand.copy (Place.mk 1 []))
        (Operand.const (Const.int ScalarTy.U32 1)))
      (Value.int ScalarTy.U32 ⟨x.val + (1 : U32).val, h.1, h.2⟩)
      (at_ Slot.uninit x) { frame := 1, slot := 0, path := [] }
      (at_ (Slot.init (Value.int ScalarTy.U32 ⟨x.val + (1 : U32).val, h.1, h.2⟩)) x)
      (evalTwo_ok (at_ Slot.uninit x) (Operand.copy (Place.mk 1 []))
        (Operand.const (Const.int ScalarTy.U32 1)) (binaryOp true BinOp.add)
        (Value.int ScalarTy.U32 x) (at_ Slot.uninit x)
        (Value.int ScalarTy.U32 (1 : U32)) (at_ Slot.uninit x)
        (Value.int ScalarTy.U32 ⟨x.val + (1 : U32).val, h.1, h.2⟩)
        rfl rfl (binaryOp_checked_add x (1 : U32) h))
      rfl rfl
  have hret : execBlockFrom P.adts P.sig
      (at_ (Slot.init (Value.int ScalarTy.U32 ⟨x.val + (1 : U32).val, h.1, h.2⟩)) x) []
      Terminator.ret =
        Outcome.ok (Next.done (Value.int ScalarTy.U32 ⟨x.val + (1 : U32).val, h.1, h.2⟩)) :=
    rfl
  have hstep : step P (at_ Slot.uninit x) =
      Outcome.ok (Next.done (Value.int ScalarTy.U32 ⟨x.val + (1 : U32).val, h.1, h.2⟩)) := by
    show execBlockFrom P.adts P.sig (at_ Slot.uninit x) [addStmt] Terminator.ret = _
    exact (execBlockFrom_step P.adts P.sig _ addStmt [] Terminator.ret _ hs).trans hret
  have hm : arith.add_one x = Result.ok ⟨x.val + (1 : U32).val, h.1, h.2⟩ :=
    Scalar.add_ok x (1 : U32) h
  exact (run_done P extra _ _ hstep).trans
    (congrArg (fun r => Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult r)) hm).symm

/-- `x + 1` out of range: the function panics with an arithmetic overflow, as the model
fails. -/
theorem agrees_overflow (x : U32) (extra : Nat)
    (h : ¬ (ScalarTy.min ScalarTy.U32 ≤ x.val + (1 : U32).val ∧
      x.val + (1 : U32).val ≤ ScalarTy.max ScalarTy.U32)) :
    run P (extra + 1) (at_ Slot.uninit x) =
      Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult (arith.add_one x)) := by
  have hs : execStmt P.adts (at_ Slot.uninit x) addStmt = Outcome.panic Error.arithmetic :=
    execStmt_assign_panic P.adts (at_ Slot.uninit x) (Place.mk 0 [])
      (Rvalue.checked BinOp.add (Operand.copy (Place.mk 1 []))
        (Operand.const (Const.int ScalarTy.U32 1)))
      Error.arithmetic
      (evalTwo_panic (at_ Slot.uninit x) (Operand.copy (Place.mk 1 []))
        (Operand.const (Const.int ScalarTy.U32 1)) (binaryOp true BinOp.add)
        (Value.int ScalarTy.U32 x) (at_ Slot.uninit x)
        (Value.int ScalarTy.U32 (1 : U32)) (at_ Slot.uninit x) Error.arithmetic
        rfl rfl (binaryOp_checked_add_fail x (1 : U32) h))
  have hstep : step P (at_ Slot.uninit x) = Outcome.panic Error.arithmetic := by
    show execBlockFrom P.adts P.sig (at_ Slot.uninit x) [addStmt] Terminator.ret = _
    exact execBlockFrom_panic P.adts P.sig _ addStmt [] Terminator.ret Error.arithmetic hs
  have hm : arith.add_one x = Result.fail Error.arithmetic :=
    Scalar.checked_fail ScalarTy.U32 (x.val + (1 : U32).val) h
  exact (run_panic P extra _ Error.arithmetic hstep).trans
    (congrArg (fun r => Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult r)) hm).symm

/-- The model of `add_one` and the semantics of its MIR give the same outcome, a value or
the overflow panic, for every `x` and one step of fuel or more. -/
theorem agrees (x : U32) (extra : Nat) :
    run P (extra + 1) (at_ Slot.uninit x) =
      Outcome.map (Value.int ScalarTy.U32) (Outcome.ofResult (arith.add_one x)) := by
  by_cases h : ScalarTy.min ScalarTy.U32 ≤ x.val + (1 : U32).val ∧
      x.val + (1 : U32).val ≤ ScalarTy.max ScalarTy.U32
  · exact agrees_ok x extra h
  · exact agrees_overflow x extra h

end BlazeAgrees.arith.add_one

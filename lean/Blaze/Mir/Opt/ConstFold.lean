import Blaze.Mir.Opt.Local

/-!
Constant folding: a binary or checked binary operation, a unary operation or a cast whose
operands are constants is the constant it evaluates to, when it evaluates to one. An
operation that panics (a checked addition that overflows, a division by zero) or is ill
typed is left as it is, so it still panics when it runs.

blaze runs no such pass today (rustc's `GVN`, which folds constants, is not ported:
`passes_not_ported`), so no pair is checked against this; it is the specification for when
one is, and one of the rewrites the validator's normal form applies (`Opt/Validate.lean`).
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- The constant a value is, when it is an integer, a `bool` or `()`. -/
def valueConst : Value → Option Const
  | Value.unit => some Const.unit
  | Value.bool b => some (Const.bool b)
  | Value.int ty v => some (Const.int ty v.val)
  | _ => none

/-- A value's constant evaluates to the value. -/
theorem constValue_of (v : Value) (c : Const) (h : valueConst v = some c) :
    constValue c = v := by
  cases v with
  | unit =>
    have h' : some Const.unit = some c := h
    cases h'
    rfl
  | bool b =>
    have h' : some (Const.bool b) = some c := h
    cases h'
    rfl
  | int ty x =>
    have h' : some (Const.int ty x.val) = some c := h
    cases h'
    exact congrArg (Value.int ty) (Scalar.lit_self x)
  | adt variant fields =>
    have h' : (none : Option Const) = some c := h
    cases h'
  | array elems =>
    have h' : (none : Option Const) = some c := h
    cases h'
  | ref p =>
    have h' : (none : Option Const) = some c := h
    cases h'
  | dead =>
    have h' : (none : Option Const) = some c := h
    cases h'

def foldConst (rv : Rvalue) : Option Const → Rvalue
  | some c => Rvalue.use (Operand.const c)
  | none => rv

/-- `rv`, which evaluates to `r` in every state, as the constant `r` is when it is one. -/
def foldTo (rv : Rvalue) : Outcome Value → Rvalue
  | Outcome.ok v => foldConst rv (valueConst v)
  | _ => rv

def foldRvalue : Rvalue → Rvalue
  | Rvalue.binary op (Operand.const a) (Operand.const b) =>
    foldTo (Rvalue.binary op (Operand.const a) (Operand.const b))
      (binaryOp false op (constValue a) (constValue b))
  | Rvalue.checked op (Operand.const a) (Operand.const b) =>
    foldTo (Rvalue.checked op (Operand.const a) (Operand.const b))
      (binaryOp true op (constValue a) (constValue b))
  | Rvalue.unary op (Operand.const a) =>
    foldTo (Rvalue.unary op (Operand.const a)) (unaryOp op (constValue a))
  | Rvalue.cast (Operand.const a) target =>
    foldTo (Rvalue.cast (Operand.const a) target) (castTo target (constValue a))
  | rv => rv

def foldStmt : Statement → Statement
  | Statement.assign p rv => Statement.assign p (foldRvalue rv)
  | s => s

def foldBlock (blk : BasicBlock) : BasicBlock :=
  { stmts := blk.stmts.map foldStmt, terminator := blk.terminator }

/-- The pass. -/
def constFold (prog : Program) : Program := prog.mapBlocks foldBlock

/-- An rvalue that evaluates to `r` without touching the state, folded, evaluates the same. -/
theorem evalRvalue_foldTo (adts : List AdtDecl) (st : State) (rv : Rvalue) (r : Outcome Value)
    (h : evalRvalue adts st rv = r.bind (fun v => Outcome.ok (v, st))) :
    evalRvalue adts st (foldTo rv r) = evalRvalue adts st rv := by
  cases r with
  | ok v =>
    cases hc : valueConst v with
    | none =>
      have e : foldTo rv (Outcome.ok v) = rv := congrArg (foldConst rv) hc
      rw [e]
    | some c =>
      have e : foldTo rv (Outcome.ok v) = Rvalue.use (Operand.const c) :=
        congrArg (foldConst rv) hc
      have hv : constValue c = v := constValue_of v c hc
      have folded : evalRvalue adts st (Rvalue.use (Operand.const c)) = Outcome.ok (v, st) :=
        congrArg (fun x => Outcome.ok (x, st)) hv
      have original : evalRvalue adts st rv = Outcome.ok (v, st) := h
      rw [e, folded, original]
  | panic e => rfl
  | undefined why => rfl
  | diverge => rfl

theorem evalRvalue_foldRvalue (adts : List AdtDecl) (st : State) (rv : Rvalue) :
    evalRvalue adts st (foldRvalue rv) = evalRvalue adts st rv := by
  cases rv with
  | use o => rfl
  | binary op l r =>
    cases l with
    | copy p => rfl
    | move p => rfl
    | const a =>
      cases r with
      | copy p => rfl
      | move p => rfl
      | const b =>
        exact evalRvalue_foldTo adts st (Rvalue.binary op (Operand.const a) (Operand.const b))
          (binaryOp false op (constValue a) (constValue b)) rfl
  | checked op l r =>
    cases l with
    | copy p => rfl
    | move p => rfl
    | const a =>
      cases r with
      | copy p => rfl
      | move p => rfl
      | const b =>
        exact evalRvalue_foldTo adts st (Rvalue.checked op (Operand.const a) (Operand.const b))
          (binaryOp true op (constValue a) (constValue b)) rfl
  | withOverflow op l r => rfl
  | unary op o =>
    cases o with
    | copy p => rfl
    | move p => rfl
    | const a =>
      exact evalRvalue_foldTo adts st (Rvalue.unary op (Operand.const a))
        (unaryOp op (constValue a)) rfl
  | cast o target =>
    cases o with
    | copy p => rfl
    | move p => rfl
    | const a =>
      exact evalRvalue_foldTo adts st (Rvalue.cast (Operand.const a) target)
        (castTo target (constValue a)) rfl
  | aggregate kind fields => rfl
  | replicate o count => rfl
  | discriminant adt p => rfl
  | ref mutable p => rfl
  | len p => rfl

theorem execStmt_foldStmt (adts : List AdtDecl) (st : State) (s : Statement) :
    execStmt adts st (foldStmt s) = execStmt adts st s := by
  cases s with
  | assign p rv =>
    exact congrArg
      (fun x : Outcome (Value × State) =>
        x.bind (fun r => (evalPlace r.2 p).bind (fun ptr => r.2.write ptr r.1)))
      (evalRvalue_foldRvalue adts st rv)
  | storageLive l => rfl
  | storageDead l => rfl
  | nop => rfl

theorem foldBlock_preserving : BlockPreserving foldBlock := by
  unfold BlockPreserving
  intro adts sig st blk
  exact execBlockFrom_mapStmts foldStmt execStmt_foldStmt adts sig blk.terminator blk.stmts st

/-- Constant folding is correct: for every program, fuel and state, the folded program runs
as the program does. An operation on constants that panics is not folded, so it panics in
both. -/
theorem constFold_correct (prog : Program) (fuel : Nat) (st : State) :
    run (constFold prog) fuel st = run prog fuel st :=
  run_mapBlocks foldBlock foldBlock_preserving prog fuel st

/-- The same as a refinement. -/
theorem constFold_refines (prog : Program) (fuel : Nat) (st : State) :
    (run (constFold prog) fuel st).Refines (run prog fuel st) :=
  Outcome.Refines.of_eq (constFold_correct prog fuel st)

end Blaze.Mir.Opt

import Blaze.Mir.Opt.Local

/-!
`SimplifyConstCondition` (crates/hir-ty-mir/src/mir/optimize/cfg.rs,
`simplify_const_condition`; rustc's pass of the name): a `switchInt` on a constant is a
`goto` to the arm it takes, and an `assert` of a constant that holds is a `goto`.

This is the specification of the pass's direct form. The Rust pass also takes a condition
that reads a place the statement just before assigned a constant; that form is not here: it
turns a `move` of the place into no read at all, so the two programs' states differ in a
moved-out local, and its proof needs a simulation up to moved-out locals (the doc says so).
`harness/v2/mir_semantics.py validate` reports a body where the Rust pass did more.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

def simplifySwitch (c : Const) (arms : List (Int × BlockId)) (otherwise : BlockId) :
    Terminator :=
  match switchKey (constValue c) with
  | some key => Terminator.goto (switchTarget key arms otherwise)
  | none => Terminator.switchInt (Operand.const c) arms otherwise

def simplifyAssert (b expected : Bool) (kind : Error) (target : BlockId) : Terminator :=
  if b = expected then Terminator.goto target
  else Terminator.assert (Operand.const (Const.bool b)) expected kind target

def simplifyTerminator : Terminator → Terminator
  | Terminator.switchInt (Operand.const c) arms otherwise => simplifySwitch c arms otherwise
  | Terminator.assert (Operand.const (Const.bool b)) expected kind target =>
    simplifyAssert b expected kind target
  | t => t

def simplifyBlock (blk : BasicBlock) : BasicBlock :=
  { stmts := blk.stmts, terminator := simplifyTerminator blk.terminator }

/-- The pass. -/
def simplifyConstCondition (prog : Program) : Program := prog.mapBlocks simplifyBlock

theorem execTerm_switch_const (sig : FnId → Option FnSig) (st : State) (c : Const)
    (arms : List (Int × BlockId)) (otherwise : BlockId) :
    execTerm sig st (Terminator.switchInt (Operand.const c) arms otherwise) =
      switchOn st arms otherwise (switchKey (constValue c)) := rfl

theorem execTerm_simplifySwitch (sig : FnId → Option FnSig) (st : State) (c : Const)
    (arms : List (Int × BlockId)) (otherwise : BlockId) :
    execTerm sig st (simplifySwitch c arms otherwise) =
      execTerm sig st (Terminator.switchInt (Operand.const c) arms otherwise) := by
  unfold simplifySwitch
  cases hk : switchKey (constValue c) with
  | none => rfl
  | some key =>
    rw [execTerm_switch_const, hk]
    rfl

theorem execTerm_assert_const (sig : FnId → Option FnSig) (st : State) (b expected : Bool)
    (kind : Error) (target : BlockId) :
    execTerm sig st (Terminator.assert (Operand.const (Const.bool b)) expected kind target) =
      assertOn st expected kind target (Value.bool b) := rfl

theorem execTerm_simplifyAssert (sig : FnId → Option FnSig) (st : State) (b expected : Bool)
    (kind : Error) (target : BlockId) :
    execTerm sig st (simplifyAssert b expected kind target) =
      execTerm sig st
        (Terminator.assert (Operand.const (Const.bool b)) expected kind target) := by
  unfold simplifyAssert
  by_cases h : b = expected
  · rw [if_pos h, execTerm_assert_const]
    show st.jump target = if b = expected then st.jump target else Outcome.panic kind
    rw [if_pos h]
  · rw [if_neg h]

/-- The rewrite of a terminator does what the terminator does, from every state. -/
theorem execTerm_simplifyTerminator (sig : FnId → Option FnSig) (st : State)
    (t : Terminator) : execTerm sig st (simplifyTerminator t) = execTerm sig st t := by
  cases t with
  | goto target => rfl
  | switchInt discr arms otherwise =>
    cases discr with
    | copy p => rfl
    | move p => rfl
    | const c => exact execTerm_simplifySwitch sig st c arms otherwise
  | call callee args dest target => rfl
  | ret => rfl
  | assert cond expected kind target =>
    cases cond with
    | copy p => rfl
    | move p => rfl
    | const c =>
      cases c with
      | unit => rfl
      | bool b => exact execTerm_simplifyAssert sig st b expected kind target
      | int ty v => rfl
  | drop p target => rfl
  | unreachable => rfl

theorem simplifyBlock_preserving : BlockPreserving simplifyBlock := by
  unfold BlockPreserving
  intro adts sig st blk
  exact execBlockFrom_terminator simplifyTerminator execTerm_simplifyTerminator adts sig
    blk.terminator blk.stmts st

/-- `SimplifyConstCondition` is correct: for every program, fuel and state, the simplified
program's run is the program's run. -/
theorem simplifyConstCondition_correct (prog : Program) (fuel : Nat) (st : State) :
    run (simplifyConstCondition prog) fuel st = run prog fuel st :=
  run_mapBlocks simplifyBlock simplifyBlock_preserving prog fuel st

/-- The same as a refinement. -/
theorem simplifyConstCondition_refines (prog : Program) (fuel : Nat) (st : State) :
    (run (simplifyConstCondition prog) fuel st).Refines (run prog fuel st) :=
  Outcome.Refines.of_eq (simplifyConstCondition_correct prog fuel st)

end Blaze.Mir.Opt

import Blaze.Mir.Lemmas

/-!
`mir_walk`: run the semantics forward by `simp` over a state whose frames and locals are
lists of known spine. It unfolds `run` (on fuel of the form `n + k`) and `step` through a
block's statements and its terminator: assignments, storage markers and `nop`; copies, moves
and constants; references, derefs and indexing by a local; `use`, casts, and binary and
checked operators other than shifts on two integers of one type; `goto`, `switchInt`, calls
of intrinsics, and `return` from the entry function. Unary operators, aggregates,
discriminants, shifts, asserts and calls of functions are not in it yet.

What the walk cannot decide from the shape of the state (an index into a list it does not
know, an overflow check of a value it does not know) stays as a term, and the caller passes
the fact that decides it: `mir_walk [P, at_, hn, hadd]`, with the program's and the state's
definitions so that they unfold. Or the caller cases on the stuck term after the walk.
Either way no state between two statements or two blocks is written down, so a proof
survives a change of the body's temporaries and storage markers.
-/

namespace Blaze.Mir

/-- A bind of a bind is one bind: the walk pushes a stuck value (an overflow check of a value
it does not know) out to where the caller can case on it. -/
theorem Outcome.bind_assoc {α β γ : Type} (x : Outcome α) (f : α → Outcome β)
    (g : β → Outcome γ) : (x.bind f).bind g = x.bind (fun a => (f a).bind g) := by
  cases x <;> rfl

/-- An operator other than a shift on two integers of one type, checked or not. -/
theorem binaryOp_int {ty : ScalarTy} (checked : Bool) (op : BinOp) (a b : Scalar ty)
    (hl : op ≠ BinOp.shl) (hr : op ≠ BinOp.shr) :
    binaryOp checked op (Value.int ty a) (Value.int ty b) =
      if checked then sameChecked op a b else sameBinary op a b := by
  cases checked <;> cases op <;>
    first
    | exact absurd rfl hl
    | exact absurd rfl hr
    | exact withSame_self a b _

end Blaze.Mir

open Lean Parser Tactic in
/-- Step the semantics by `simp` with the lemmas of `Blaze.Mir.Walk` and the extra facts
given. -/
syntax "mir_walk" (" [" simpLemma,* "]")? : tactic

macro_rules
  | `(tactic| mir_walk) => `(tactic| mir_walk [])
  | `(tactic| mir_walk [$extra,*]) =>
    `(tactic| simp only [Blaze.Mir.run, Blaze.Mir.afterStep, Blaze.Mir.step,
        Blaze.Mir.stepFrame, Blaze.Mir.execBlock?, Blaze.Mir.execBlock, Blaze.Mir.Program.block,
        Blaze.Mir.blockOf, Blaze.Mir.Program.sig, Blaze.Mir.nth?, Blaze.Mir.setNth,
        Blaze.Mir.execBlockFrom, Blaze.Mir.execStmt, Blaze.Mir.evalRvalue,
        Blaze.Mir.evalOperand, Blaze.Mir.evalOperands, Blaze.Mir.evalTwo, Blaze.Mir.evalOne,
        Blaze.Mir.evalAt, Blaze.Mir.evalPlace, Blaze.Mir.evalProj, Blaze.Mir.projStep,
        Blaze.Mir.State.read, Blaze.Mir.State.write, Blaze.Mir.State.slot,
        Blaze.Mir.State.slots?, Blaze.Mir.State.setSlot, Blaze.Mir.State.setSlotIn,
        Blaze.Mir.State.setSlots, Blaze.Mir.State.top, Blaze.Mir.State.moveOut,
        Blaze.Mir.slotIn, Blaze.Mir.slotOf, Blaze.Mir.findFrame, Blaze.Mir.setFrame,
        Blaze.Mir.Frame.withLocals, Blaze.Mir.Frame.withBlock, Blaze.Mir.Slot.store,
        Blaze.Mir.Slot.load, Blaze.Mir.Value.project, Blaze.Mir.Value.checkLive,
        Blaze.Mir.Value.child, Blaze.Mir.found, Blaze.Mir.Outcome.bind_ok,
        Blaze.Mir.Outcome.bind_assoc, Blaze.Mir.constValue, Blaze.Mir.execTerm,
        Blaze.Mir.State.jump, Blaze.Mir.State.jump?, Blaze.Mir.switchOn, Blaze.Mir.switchKey,
        Blaze.Mir.switchTarget, Blaze.Mir.callOn, Blaze.Mir.Intrinsic.eval, Blaze.Mir.State.ret,
        Blaze.Mir.lenOf, Blaze.Mir.inBounds, Blaze.Mir.indexAt, Blaze.Mir.indexBy,
        Blaze.Mir.asRef, Blaze.Mir.Pointer.push, Blaze.Mir.stateOr, Blaze.Mir.castTo,
        Blaze.Mir.binaryOp_int, Blaze.Mir.sameBinary, Blaze.Mir.sameChecked,
        Blaze.Mir.intResult, Blaze.Mir.Outcome.ofResult, Blaze.Mir.Outcome.map,
        Option.map_some, Option.map_none, List.nil_append, List.cons_append, ne_eq,
        reduceCtorEq, not_false_eq_true, ite_true, ite_false, dite_true, dite_false,
        ↓reduceIte, ↓reduceDIte, Nat.reduceEqDiff, Int.reduceEq, $extra,*])

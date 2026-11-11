import Blaze.Mir.Run

/-!
One lemma per construct: what a statement, an rvalue or an operand does, from what its parts
do. A proof that a function's MIR runs to what its functional model gives (objective V7) is
these lemmas applied along the body, block by block; the first is
crates/blaze-graph/lean/BlazeGraph/LedgerMir.lean. Each hypothesis is an equation of the
semantics, closed by `rfl` where the state is known and by a fact about the data where it is
not (an index in bounds, an addition in range).
-/

namespace Blaze.Mir

set_option maxRecDepth 4096

/-- An assignment: the rvalue, the place, the write. -/
theorem execStmt_assign (adts : List AdtDecl) (st : State) (p : Place) (rv : Rvalue)
    (v : Value) (st1 : State) (ptr : Pointer) (st2 : State)
    (hr : evalRvalue adts st rv = Outcome.ok (v, st1))
    (hp : evalPlace st1 p = Outcome.ok ptr)
    (hw : st1.write ptr v = Outcome.ok st2) :
    execStmt adts st (Statement.assign p rv) = Outcome.ok st2 := by
  show (evalRvalue adts st rv).bind (fun r =>
    (evalPlace r.2 p).bind (fun ptr => r.2.write ptr r.1)) = Outcome.ok st2
  rw [hr]
  show (evalPlace st1 p).bind (fun ptr => st1.write ptr v) = Outcome.ok st2
  rw [hp]
  exact hw

/-- A statement that runs to `st1` is one step of its block. -/
theorem execBlockFrom_step (adts : List AdtDecl) (sig : FnId → Option FnSig) (st : State)
    (s : Statement) (rest : List Statement) (t : Terminator) (st1 : State)
    (h : execStmt adts st s = Outcome.ok st1) :
    execBlockFrom adts sig st (s :: rest) t = execBlockFrom adts sig st1 rest t := by
  rw [execBlockFrom_cons, h]
  rfl

/-- A copy reads the place and leaves the state. -/
theorem evalOperand_copy (st : State) (p : Place) (ptr : Pointer) (v : Value)
    (hp : evalPlace st p = Outcome.ok ptr) (hv : st.read ptr = Outcome.ok v) :
    evalOperand st (Operand.copy p) = Outcome.ok (v, st) := by
  show (evalPlace st p).bind (fun ptr =>
    (st.read ptr).bind (fun v => Outcome.ok (v, st))) = Outcome.ok (v, st)
  rw [hp]
  show (st.read ptr).bind (fun v => Outcome.ok (v, st)) = Outcome.ok (v, st)
  rw [hv]
  rfl

/-- A move reads the place and moves out of it. -/
theorem evalOperand_move (st : State) (p : Place) (ptr : Pointer) (v : Value)
    (hp : evalPlace st p = Outcome.ok ptr) (hv : st.read ptr = Outcome.ok v) :
    evalOperand st (Operand.move p) = Outcome.ok (v, st.moveOut ptr) := by
  show (evalPlace st p).bind (fun ptr =>
    (st.read ptr).bind (fun v => Outcome.ok (v, st.moveOut ptr))) = Outcome.ok (v, st.moveOut ptr)
  rw [hp]
  show (st.read ptr).bind (fun v => Outcome.ok (v, st.moveOut ptr)) =
    Outcome.ok (v, st.moveOut ptr)
  rw [hv]
  rfl

theorem evalOperand_const (st : State) (c : Const) :
    evalOperand st (Operand.const c) = Outcome.ok (constValue c, st) := rfl

/-- A binary rvalue: the left operand, the right, the operator. -/
theorem evalTwo_ok (st : State) (l r : Operand) (f : Value → Value → Outcome Value)
    (a : Value) (st1 : State) (b : Value) (st2 : State) (v : Value)
    (hl : evalOperand st l = Outcome.ok (a, st1))
    (hr : evalOperand st1 r = Outcome.ok (b, st2))
    (hv : f a b = Outcome.ok v) :
    evalTwo st l r f = Outcome.ok (v, st2) := by
  unfold evalTwo
  rw [hl]
  show (evalOperand st1 r).bind (fun b =>
    (f a b.1).bind (fun v => Outcome.ok (v, b.2))) = Outcome.ok (v, st2)
  rw [hr]
  show (f a b).bind (fun v => Outcome.ok (v, st2)) = Outcome.ok (v, st2)
  rw [hv]
  rfl

/-- A unary rvalue or a cast: the operand, the operator. -/
theorem evalOne_ok (st : State) (o : Operand) (f : Value → Outcome Value) (a : Value)
    (st1 : State) (v : Value)
    (ho : evalOperand st o = Outcome.ok (a, st1)) (hv : f a = Outcome.ok v) :
    evalOne st o f = Outcome.ok (v, st1) := by
  unfold evalOne
  rw [ho]
  show (f a).bind (fun v => Outcome.ok (v, st1)) = Outcome.ok (v, st1)
  rw [hv]
  rfl

/-- A checked addition in range is the sum: `Scalar.add`'s own fact, at the level of values. -/
theorem binaryOp_checked_add {ty : ScalarTy} (a b : Scalar ty)
    (h : ty.min ≤ a.val + b.val ∧ a.val + b.val ≤ ty.max) :
    binaryOp true BinOp.add (Value.int ty a) (Value.int ty b) =
      Outcome.ok (Value.int ty ⟨a.val + b.val, h.1, h.2⟩) := by
  show withSame a b (sameChecked BinOp.add) = Outcome.ok (Value.int ty ⟨a.val + b.val, h.1, h.2⟩)
  rw [withSame_self]
  show intResult (Scalar.add a b) = Outcome.ok (Value.int ty ⟨a.val + b.val, h.1, h.2⟩)
  rw [Scalar.add_ok a b h]
  rfl

/-- A checked subtraction in range is the difference. -/
theorem binaryOp_checked_sub {ty : ScalarTy} (a b : Scalar ty)
    (h : ty.min ≤ a.val - b.val ∧ a.val - b.val ≤ ty.max) :
    binaryOp true BinOp.sub (Value.int ty a) (Value.int ty b) =
      Outcome.ok (Value.int ty ⟨a.val - b.val, h.1, h.2⟩) := by
  show withSame a b (sameChecked BinOp.sub) = Outcome.ok (Value.int ty ⟨a.val - b.val, h.1, h.2⟩)
  rw [withSame_self]
  show intResult (Scalar.sub a b) = Outcome.ok (Value.int ty ⟨a.val - b.val, h.1, h.2⟩)
  rw [Scalar.sub_ok a b h]
  rfl

/-- An operator on two integers of one type is `Scalar`'s. -/
theorem binaryOp_same {ty : ScalarTy} (op : BinOp) (a b : Scalar ty)
    (hl : op ≠ BinOp.shl) (hr : op ≠ BinOp.shr) :
    binaryOp false op (Value.int ty a) (Value.int ty b) = sameBinary op a b := by
  cases op with
  | shl => exact absurd rfl hl
  | shr => exact absurd rfl hr
  | add => exact withSame_self a b (sameBinary BinOp.add)
  | sub => exact withSame_self a b (sameBinary BinOp.sub)
  | mul => exact withSame_self a b (sameBinary BinOp.mul)
  | div => exact withSame_self a b (sameBinary BinOp.div)
  | rem => exact withSame_self a b (sameBinary BinOp.rem)
  | bitXor => exact withSame_self a b (sameBinary BinOp.bitXor)
  | bitAnd => exact withSame_self a b (sameBinary BinOp.bitAnd)
  | bitOr => exact withSame_self a b (sameBinary BinOp.bitOr)
  | eq => exact withSame_self a b (sameBinary BinOp.eq)
  | lt => exact withSame_self a b (sameBinary BinOp.lt)
  | le => exact withSame_self a b (sameBinary BinOp.le)
  | ne => exact withSame_self a b (sameBinary BinOp.ne)
  | ge => exact withSame_self a b (sameBinary BinOp.ge)
  | gt => exact withSame_self a b (sameBinary BinOp.gt)

/-- An index in bounds is the place of that element. -/
theorem inBounds_some (p : Pointer) (n : Nat) (v : Value) :
    inBounds p n (some v) = Outcome.ok (p.push (PathElem.index n)) := rfl

/-- A step is the running frame's block. -/
theorem step_block (prog : Program) (base : List Slot) (fr : Frame) (rest : List Frame)
    (blk : BasicBlock) (hb : prog.block fr.fn fr.block = some blk) :
    step prog { base := base, stack := fr :: rest } =
      execBlock prog.adts prog.sig { base := base, stack := fr :: rest } blk := by
  show execBlock? prog.adts prog.sig { base := base, stack := fr :: rest }
    (prog.block fr.fn fr.block) = _
  rw [hb]
  rfl

end Blaze.Mir

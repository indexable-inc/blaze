import Blaze.Mir.Opt.Ctl

/-!
Which frames there are: each frame's id and function (`State.ids`), which a statement, an
operand, a place and a write leave as they were. The lemmas are those of `Opt/Ctl.lean`, for
the id and the function in place of the function, block and return target.

What a pointer may name is said by the frame its id names (`Opt/Borrow.lean`), so that is
fixed while a block's statements run.
-/

namespace Blaze.Mir

/-- A frame's id and the function it runs. -/
def idFnOf (fr : Frame) : Nat × FnId := (fr.id, fr.fn)

def State.ids (st : State) : List (Nat × FnId) := st.stack.map idFnOf

/-! Memory writes. -/

theorem setFrame_ids (id : Nat) (slots : List Slot) :
    ∀ (l : List Frame), (setFrame l id slots).map idFnOf = l.map idFnOf := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have e : setFrame (fr :: rest) id slots = fr.withLocals slots :: rest :=
        (setFrame_cons fr rest id slots).trans (if_pos h)
      exact congrTo (List.map idFnOf) e
    · have e : setFrame (fr :: rest) id slots = fr :: setFrame rest id slots :=
        (setFrame_cons fr rest id slots).trans (if_neg h)
      exact (congrTo (List.map idFnOf) e).trans (congrTo (fun t => idFnOf fr :: t) ih)

theorem State.setSlots_ids (st : State) (f : Nat) (slots : List Slot) :
    (st.setSlots f slots).ids = st.ids := by
  by_cases hf : f = 0
  · have e : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_pos hf)
    exact congrTo State.ids e
  · have e : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_neg hf)
    exact (congrTo State.ids e).trans (setFrame_ids f slots st.stack)

theorem State.setSlotIn_ids (st : State) (p : Pointer) (slot : Slot) :
    ∀ (o : Option (List Slot)), (st.setSlotIn p slot o).ids = st.ids
  | none => rfl
  | some slots => State.setSlots_ids st p.frame (setNth slots p.slot slot)

theorem State.setSlot_ids (st : State) (p : Pointer) (slot : Slot) :
    (st.setSlot p slot).ids = st.ids :=
  State.setSlotIn_ids st p slot (st.slots? p.frame)

theorem State.write_ids (st : State) (p : Pointer) (v : Value) :
    Outcome.All (fun (st' : State) => st'.ids = st.ids) (st.write p v) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun slot' _ =>
      Outcome.all_ok (State.setSlot_ids st p slot')))

theorem stateOr_ids (st : State) :
    ∀ (x : Outcome State), Outcome.All (fun (st' : State) => st'.ids = st.ids) x →
      (stateOr st x).ids = st.ids
  | Outcome.ok st', h => h st' rfl
  | Outcome.panic _, _ => rfl
  | Outcome.undefined _, _ => rfl
  | Outcome.diverge, _ => rfl

theorem State.moveOut_ids (st : State) : ∀ (p : Pointer), (st.moveOut p).ids = st.ids
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_ids st { frame := frame, slot := slot, path := [] } Slot.moved
  | { frame := frame, slot := slot, path := e :: rest } =>
    stateOr_ids st _
      (State.write_ids st { frame := frame, slot := slot, path := e :: rest } Value.dead)

/-! Operands, rvalues, statements. -/

theorem evalOperand_ids (st : State) :
    ∀ (o : Operand), Outcome.All (fun (r : Value × State) => r.2.ids = st.ids) (evalOperand st o)
  | Operand.copy _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl))
  | Operand.move _ =>
    Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
        Outcome.all_ok (State.moveOut_ids st ptr)))
  | Operand.const _ => Outcome.all_ok rfl

theorem evalOperands_ids :
    ∀ (os : List Operand) (st : State),
      Outcome.All (fun (r : List Value × State) => r.2.ids = st.ids) (evalOperands st os)
  | [], _ => Outcome.all_ok rfl
  | o :: rest, st =>
    Outcome.all_bind (evalOperand_ids st o) (fun a ha =>
      Outcome.all_bind (evalOperands_ids rest a.2) (fun _ hb =>
        Outcome.all_ok (Eq.trans hb ha)))

theorem evalTwo_ids (st : State) (l r : Operand) (f : Value → Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.ids = st.ids) (evalTwo st l r f) :=
  Outcome.all_bind (evalOperand_ids st l) (fun a ha =>
    Outcome.all_bind (evalOperand_ids a.2 r) (fun _ hb =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok (Eq.trans hb ha))))

theorem evalOne_ids (st : State) (o : Operand) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.ids = st.ids) (evalOne st o f) :=
  Outcome.all_bind (evalOperand_ids st o) (fun _ ha =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok ha))

theorem evalAt_ids (st : State) (p : Place) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.ids = st.ids) (evalAt st p f) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)))

theorem evalRvalue_ids (adts : List AdtDecl) (st : State) :
    ∀ (rv : Rvalue),
      Outcome.All (fun (x : Value × State) => x.2.ids = st.ids) (evalRvalue adts st rv)
  | Rvalue.use o => evalOperand_ids st o
  | Rvalue.binary op l r => evalTwo_ids st l r (binaryOp false op)
  | Rvalue.checked op l r => evalTwo_ids st l r (binaryOp true op)
  | Rvalue.withOverflow op l r => evalTwo_ids st l r (overflowingOp op)
  | Rvalue.unary op o => evalOne_ids st o (unaryOp op)
  | Rvalue.cast o target => evalOne_ids st o (castTo target)
  | Rvalue.aggregate _ fields =>
    Outcome.all_bind (evalOperands_ids fields st) (fun _ ha => Outcome.all_ok ha)
  | Rvalue.replicate o count =>
    evalOne_ids st o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt_ids st p (discriminantOf adts adt)
  | Rvalue.ref _ _ => Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)
  | Rvalue.len p => evalAt_ids st p lenOf

/-- A statement leaves the frames' ids and functions as they were. -/
theorem execStmt_ids (adts : List AdtDecl) (st : State) :
    ∀ (s : Statement), Outcome.All (fun (st' : State) => st'.ids = st.ids) (execStmt adts st s)
  | Statement.assign p rv =>
    Outcome.all_bind (evalRvalue_ids adts st rv) (fun r hr =>
      Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
        Outcome.All.imp (State.write_ids r.2 ptr r.1) (fun _ h => Eq.trans h hr)))
  | Statement.storageLive _ => Outcome.all_ok rfl
  | Statement.storageDead _ => Outcome.all_ok rfl
  | Statement.nop => Outcome.all_ok rfl

end Blaze.Mir

import Blaze.Mir.Step
import Blaze.Mir.Opt.Bind

/-!
The frames but for what their locals hold: each frame's id, function, block, destination and
return target (`State.skel`), which a statement, an operand, a place and a write leave as
they were. The lemmas are those of `Opt/Ctl.lean`, for the whole frame with its locals
emptied in place of the function, block and return target.

Global dead-store elimination (ledger V11) relates two states with the same frames and
different slots; what it says of a frame (where it resumes, where its call writes) is said of
this.
-/

namespace Blaze.Mir

/-- A frame without what its locals hold. -/
def bare (fr : Frame) : Frame := fr.withLocals []

/-- The frames, the running one first, without what their locals hold. -/
def State.skel (st : State) : List Frame := st.stack.map bare

/-! Memory writes. -/

theorem setFrame_skel (id : Nat) (slots : List Slot) :
    ∀ (l : List Frame), (setFrame l id slots).map bare = l.map bare := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have e : setFrame (fr :: rest) id slots = fr.withLocals slots :: rest :=
        (setFrame_cons fr rest id slots).trans (if_pos h)
      exact congrTo (List.map bare) e
    · have e : setFrame (fr :: rest) id slots = fr :: setFrame rest id slots :=
        (setFrame_cons fr rest id slots).trans (if_neg h)
      exact (congrTo (List.map bare) e).trans (congrTo (fun t => bare fr :: t) ih)

theorem State.setSlots_skel (st : State) (f : Nat) (slots : List Slot) :
    (st.setSlots f slots).skel = st.skel := by
  by_cases hf : f = 0
  · have e : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_pos hf)
    exact congrTo State.skel e
  · have e : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_neg hf)
    exact (congrTo State.skel e).trans (setFrame_skel f slots st.stack)

theorem State.setSlotIn_skel (st : State) (p : Pointer) (slot : Slot) :
    ∀ (o : Option (List Slot)), (st.setSlotIn p slot o).skel = st.skel
  | none => rfl
  | some slots => State.setSlots_skel st p.frame (setNth slots p.slot slot)

theorem State.setSlot_skel (st : State) (p : Pointer) (slot : Slot) :
    (st.setSlot p slot).skel = st.skel :=
  State.setSlotIn_skel st p slot (st.slots? p.frame)

theorem State.write_skel (st : State) (p : Pointer) (v : Value) :
    Outcome.All (fun (st' : State) => st'.skel = st.skel) (st.write p v) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun slot' _ =>
      Outcome.all_ok (State.setSlot_skel st p slot')))

theorem stateOr_skel (st : State) :
    ∀ (x : Outcome State), Outcome.All (fun (st' : State) => st'.skel = st.skel) x →
      (stateOr st x).skel = st.skel
  | Outcome.ok st', h => h st' rfl
  | Outcome.panic _, _ => rfl
  | Outcome.undefined _, _ => rfl
  | Outcome.diverge, _ => rfl

theorem State.moveOut_skel (st : State) : ∀ (p : Pointer), (st.moveOut p).skel = st.skel
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_skel st { frame := frame, slot := slot, path := [] } Slot.moved
  | { frame := frame, slot := slot, path := e :: rest } =>
    stateOr_skel st _
      (State.write_skel st { frame := frame, slot := slot, path := e :: rest } Value.dead)

/-! Operands, rvalues, statements. -/

theorem evalOperand_skel (st : State) :
    ∀ (o : Operand), Outcome.All (fun (r : Value × State) => r.2.skel = st.skel) (evalOperand st o)
  | Operand.copy _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl))
  | Operand.move _ =>
    Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
        Outcome.all_ok (State.moveOut_skel st ptr)))
  | Operand.const _ => Outcome.all_ok rfl

theorem evalOperands_skel :
    ∀ (os : List Operand) (st : State),
      Outcome.All (fun (r : List Value × State) => r.2.skel = st.skel) (evalOperands st os)
  | [], _ => Outcome.all_ok rfl
  | o :: rest, st =>
    Outcome.all_bind (evalOperand_skel st o) (fun a ha =>
      Outcome.all_bind (evalOperands_skel rest a.2) (fun _ hb =>
        Outcome.all_ok (Eq.trans hb ha)))

theorem evalTwo_skel (st : State) (l r : Operand) (f : Value → Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.skel = st.skel) (evalTwo st l r f) :=
  Outcome.all_bind (evalOperand_skel st l) (fun a ha =>
    Outcome.all_bind (evalOperand_skel a.2 r) (fun _ hb =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok (Eq.trans hb ha))))

theorem evalOne_skel (st : State) (o : Operand) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.skel = st.skel) (evalOne st o f) :=
  Outcome.all_bind (evalOperand_skel st o) (fun _ ha =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok ha))

theorem evalAt_skel (st : State) (p : Place) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.skel = st.skel) (evalAt st p f) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)))

theorem evalRvalue_skel (adts : List AdtDecl) (st : State) :
    ∀ (rv : Rvalue),
      Outcome.All (fun (x : Value × State) => x.2.skel = st.skel) (evalRvalue adts st rv)
  | Rvalue.use o => evalOperand_skel st o
  | Rvalue.binary op l r => evalTwo_skel st l r (binaryOp false op)
  | Rvalue.checked op l r => evalTwo_skel st l r (binaryOp true op)
  | Rvalue.withOverflow op l r => evalTwo_skel st l r (overflowingOp op)
  | Rvalue.unary op o => evalOne_skel st o (unaryOp op)
  | Rvalue.cast o target => evalOne_skel st o (castTo target)
  | Rvalue.aggregate _ fields =>
    Outcome.all_bind (evalOperands_skel fields st) (fun _ ha => Outcome.all_ok ha)
  | Rvalue.replicate o count =>
    evalOne_skel st o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt_skel st p (discriminantOf adts adt)
  | Rvalue.ref _ _ => Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)
  | Rvalue.len p => evalAt_skel st p lenOf

/-- A statement leaves the frames, but for what their locals hold, as they were. -/
theorem execStmt_skel (adts : List AdtDecl) (st : State) :
    ∀ (s : Statement), Outcome.All (fun (st' : State) => st'.skel = st.skel) (execStmt adts st s)
  | Statement.assign p rv =>
    Outcome.all_bind (evalRvalue_skel adts st rv) (fun r hr =>
      Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
        Outcome.All.imp (State.write_skel r.2 ptr r.1) (fun _ h => Eq.trans h hr)))
  | Statement.storageLive _ => Outcome.all_ok rfl
  | Statement.storageDead _ => Outcome.all_ok rfl
  | Statement.nop => Outcome.all_ok rfl

end Blaze.Mir

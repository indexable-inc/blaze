import Blaze.Mir.Opt.Ctl

/-!
The id the next call's frame gets (`State.next`) is left as it was by a statement, an
operand, a place and a write: only a call moves it. The lemmas are those of `Opt/Ctl.lean`
for that field.
-/

namespace Blaze.Mir

theorem State.setSlots_next (st : State) (f : Nat) (slots : List Slot) :
    (st.setSlots f slots).next = st.next := by
  by_cases hf : f = 0
  · have e : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_pos hf)
    exact congrTo State.next e
  · have e : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_neg hf)
    exact congrTo State.next e

theorem State.setSlotIn_next (st : State) (p : Pointer) (slot : Slot) :
    ∀ (o : Option (List Slot)), (st.setSlotIn p slot o).next = st.next
  | none => rfl
  | some slots => State.setSlots_next st p.frame (setNth slots p.slot slot)

theorem State.setSlot_next (st : State) (p : Pointer) (slot : Slot) :
    (st.setSlot p slot).next = st.next :=
  State.setSlotIn_next st p slot (st.slots? p.frame)

theorem State.write_next (st : State) (p : Pointer) (v : Value) :
    Outcome.All (fun (st' : State) => st'.next = st.next) (st.write p v) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun slot' _ =>
      Outcome.all_ok (State.setSlot_next st p slot')))

theorem stateOr_next (st : State) :
    ∀ (x : Outcome State), Outcome.All (fun (st' : State) => st'.next = st.next) x →
      (stateOr st x).next = st.next
  | Outcome.ok st', h => h st' rfl
  | Outcome.panic _, _ => rfl
  | Outcome.undefined _, _ => rfl
  | Outcome.diverge, _ => rfl

theorem State.moveOut_next (st : State) : ∀ (p : Pointer), (st.moveOut p).next = st.next
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_next st { frame := frame, slot := slot, path := [] } Slot.moved
  | { frame := frame, slot := slot, path := e :: rest } =>
    stateOr_next st _
      (State.write_next st { frame := frame, slot := slot, path := e :: rest } Value.dead)

/-! Operands, rvalues, statements. -/

theorem evalOperand_next (st : State) :
    ∀ (o : Operand), Outcome.All (fun (r : Value × State) => r.2.next = st.next) (evalOperand st o)
  | Operand.copy _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl))
  | Operand.move _ =>
    Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
        Outcome.all_ok (State.moveOut_next st ptr)))
  | Operand.const _ => Outcome.all_ok rfl

theorem evalOperands_next :
    ∀ (os : List Operand) (st : State),
      Outcome.All (fun (r : List Value × State) => r.2.next = st.next) (evalOperands st os)
  | [], _ => Outcome.all_ok rfl
  | o :: rest, st =>
    Outcome.all_bind (evalOperand_next st o) (fun a ha =>
      Outcome.all_bind (evalOperands_next rest a.2) (fun _ hb =>
        Outcome.all_ok (Eq.trans hb ha)))

theorem evalTwo_next (st : State) (l r : Operand) (f : Value → Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.next = st.next) (evalTwo st l r f) :=
  Outcome.all_bind (evalOperand_next st l) (fun a ha =>
    Outcome.all_bind (evalOperand_next a.2 r) (fun _ hb =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok (Eq.trans hb ha))))

theorem evalOne_next (st : State) (o : Operand) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.next = st.next) (evalOne st o f) :=
  Outcome.all_bind (evalOperand_next st o) (fun _ ha =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok ha))

theorem evalAt_next (st : State) (p : Place) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.next = st.next) (evalAt st p f) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)))

theorem evalRvalue_next (adts : List AdtDecl) (st : State) :
    ∀ (rv : Rvalue),
      Outcome.All (fun (x : Value × State) => x.2.next = st.next) (evalRvalue adts st rv)
  | Rvalue.use o => evalOperand_next st o
  | Rvalue.binary op l r => evalTwo_next st l r (binaryOp false op)
  | Rvalue.checked op l r => evalTwo_next st l r (binaryOp true op)
  | Rvalue.withOverflow op l r => evalTwo_next st l r (overflowingOp op)
  | Rvalue.unary op o => evalOne_next st o (unaryOp op)
  | Rvalue.cast o target => evalOne_next st o (castTo target)
  | Rvalue.aggregate _ fields =>
    Outcome.all_bind (evalOperands_next fields st) (fun _ ha => Outcome.all_ok ha)
  | Rvalue.replicate o count =>
    evalOne_next st o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt_next st p (discriminantOf adts adt)
  | Rvalue.ref _ _ => Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)
  | Rvalue.len p => evalAt_next st p lenOf

/-- A statement leaves the next frame id as it was. -/
theorem execStmt_next (adts : List AdtDecl) (st : State) :
    ∀ (s : Statement), Outcome.All (fun (st' : State) => st'.next = st.next) (execStmt adts st s)
  | Statement.assign p rv =>
    Outcome.all_bind (evalRvalue_next adts st rv) (fun r hr =>
      Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
        Outcome.All.imp (State.write_next r.2 ptr r.1) (fun _ h => Eq.trans h hr)))
  | Statement.storageLive _ => Outcome.all_ok rfl
  | Statement.storageDead _ => Outcome.all_ok rfl
  | Statement.nop => Outcome.all_ok rfl

end Blaze.Mir

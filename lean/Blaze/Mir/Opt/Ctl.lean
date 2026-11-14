import Blaze.Mir.Step
import Blaze.Mir.Opt.Bind

/-!
Control is not memory: a statement, an operand, a place and a write change what frames hold
and never which function a frame runs, the block it is at, or the block its caller goes on
at. `State.ctl` is that part of a state (per frame: function, block, return target), and each
function of `Step.lean` up to `execStmt` is proved to leave it as it was.

Every proof about a pass that depends on where control is rests on this: a renumbering needs
the running function to rename a jump's target, dead-block removal needs the blocks control
can reach, goto threading needs the function whose body a target is looked up in.
-/

namespace Blaze.Mir

/-- A frame's control: its function, its block, the block its caller goes on at. -/
abbrev Ctl := FnId × BlockId × Option BlockId

def ctlOf (fr : Frame) : Ctl := (fr.fn, fr.block, fr.target)

/-- The function of the first frame; 0 when there is none. -/
def headFn : List Ctl → FnId
  | [] => 0
  | c :: _ => c.1

def State.ctl (st : State) : List Ctl := st.stack.map ctlOf

/-- The function the running frame runs. -/
def State.topFn (st : State) : FnId := headFn st.ctl

theorem State.topFn_of_ctl {st st' : State} (e : st'.ctl = st.ctl) : st'.topFn = st.topFn :=
  congrTo headFn e

/-! Memory writes. -/

theorem setFrame_ctl (id : Nat) (slots : List Slot) :
    ∀ (l : List Frame), (setFrame l id slots).map ctlOf = l.map ctlOf := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have e : setFrame (fr :: rest) id slots = fr.withLocals slots :: rest :=
        (setFrame_cons fr rest id slots).trans (ite_eq_left h)
      exact congrTo (List.map ctlOf) e
    · have e : setFrame (fr :: rest) id slots = fr :: setFrame rest id slots :=
        (setFrame_cons fr rest id slots).trans (ite_eq_right h)
      exact (congrTo (List.map ctlOf) e).trans (congrTo (fun t => ctlOf fr :: t) ih)

theorem State.setSlots_ctl (st : State) (f : Nat) (slots : List Slot) :
    (st.setSlots f slots).ctl = st.ctl := by
  by_cases hf : f = 0
  · have e : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (ite_eq_left hf)
    exact congrTo State.ctl e
  · have e : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (ite_eq_right hf)
    exact (congrTo State.ctl e).trans (setFrame_ctl f slots st.stack)

theorem State.setSlotIn_ctl (st : State) (p : Pointer) (slot : Slot) :
    ∀ (o : Option (List Slot)), (st.setSlotIn p slot o).ctl = st.ctl
  | none => rfl
  | some slots => State.setSlots_ctl st p.frame (setNth slots p.slot slot)

theorem State.setSlot_ctl (st : State) (p : Pointer) (slot : Slot) :
    (st.setSlot p slot).ctl = st.ctl :=
  State.setSlotIn_ctl st p slot (st.slots? p.frame)

theorem State.write_ctl (st : State) (p : Pointer) (v : Value) :
    Outcome.All (fun (st' : State) => st'.ctl = st.ctl) (st.write p v) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun slot' _ =>
      Outcome.all_ok (State.setSlot_ctl st p slot')))

theorem stateOr_ctl (st : State) :
    ∀ (x : Outcome State), Outcome.All (fun (st' : State) => st'.ctl = st.ctl) x →
      (stateOr st x).ctl = st.ctl
  | Outcome.ok st', h => h st' rfl
  | Outcome.panic _, _ => rfl
  | Outcome.undefined _, _ => rfl
  | Outcome.diverge, _ => rfl

theorem State.moveOut_ctl (st : State) : ∀ (p : Pointer), (st.moveOut p).ctl = st.ctl
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_ctl st { frame := frame, slot := slot, path := [] } Slot.moved
  | { frame := frame, slot := slot, path := e :: rest } =>
    stateOr_ctl st _
      (State.write_ctl st { frame := frame, slot := slot, path := e :: rest } Value.dead)

/-! Operands, rvalues, statements. -/

theorem evalOperand_ctl (st : State) :
    ∀ (o : Operand), Outcome.All (fun (r : Value × State) => r.2.ctl = st.ctl) (evalOperand st o)
  | Operand.copy _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl))
  | Operand.move _ =>
    Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
        Outcome.all_ok (State.moveOut_ctl st ptr)))
  | Operand.const _ => Outcome.all_ok rfl

theorem evalOperands_ctl :
    ∀ (os : List Operand) (st : State),
      Outcome.All (fun (r : List Value × State) => r.2.ctl = st.ctl) (evalOperands st os)
  | [], _ => Outcome.all_ok rfl
  | o :: rest, st =>
    Outcome.all_bind (evalOperand_ctl st o) (fun a ha =>
      Outcome.all_bind (evalOperands_ctl rest a.2) (fun _ hb =>
        Outcome.all_ok (Eq.trans hb ha)))

theorem evalTwo_ctl (st : State) (l r : Operand) (f : Value → Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.ctl = st.ctl) (evalTwo st l r f) :=
  Outcome.all_bind (evalOperand_ctl st l) (fun a ha =>
    Outcome.all_bind (evalOperand_ctl a.2 r) (fun _ hb =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok (Eq.trans hb ha))))

theorem evalOne_ctl (st : State) (o : Operand) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.ctl = st.ctl) (evalOne st o f) :=
  Outcome.all_bind (evalOperand_ctl st o) (fun _ ha =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok ha))

theorem evalAt_ctl (st : State) (p : Place) (f : Value → Outcome Value) :
    Outcome.All (fun (x : Value × State) => x.2.ctl = st.ctl) (evalAt st p f) :=
  Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
    Outcome.all_bind (Outcome.all_true _) (fun _ _ =>
      Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)))

theorem evalRvalue_ctl (adts : List AdtDecl) (st : State) :
    ∀ (rv : Rvalue),
      Outcome.All (fun (x : Value × State) => x.2.ctl = st.ctl) (evalRvalue adts st rv)
  | Rvalue.use o => evalOperand_ctl st o
  | Rvalue.binary op l r => evalTwo_ctl st l r (binaryOp false op)
  | Rvalue.checked op l r => evalTwo_ctl st l r (binaryOp true op)
  | Rvalue.withOverflow op l r => evalTwo_ctl st l r (overflowingOp op)
  | Rvalue.unary op o => evalOne_ctl st o (unaryOp op)
  | Rvalue.cast o target => evalOne_ctl st o (castTo target)
  | Rvalue.aggregate _ fields =>
    Outcome.all_bind (evalOperands_ctl fields st) (fun _ ha => Outcome.all_ok ha)
  | Rvalue.replicate o count =>
    evalOne_ctl st o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt_ctl st p (discriminantOf adts adt)
  | Rvalue.ref _ _ => Outcome.all_bind (Outcome.all_true _) (fun _ _ => Outcome.all_ok rfl)
  | Rvalue.len p => evalAt_ctl st p lenOf

/-- A statement leaves control as it was. -/
theorem execStmt_ctl (adts : List AdtDecl) (st : State) :
    ∀ (s : Statement), Outcome.All (fun (st' : State) => st'.ctl = st.ctl) (execStmt adts st s)
  | Statement.assign _ rv =>
    Outcome.all_bind (evalRvalue_ctl adts st rv) (fun r hr =>
      Outcome.all_bind (Outcome.all_true _) (fun ptr _ =>
        Outcome.All.imp (State.write_ctl r.2 ptr r.1) (fun _ h => Eq.trans h hr)))
  | Statement.storageLive _ => Outcome.all_ok rfl
  | Statement.storageDead _ => Outcome.all_ok rfl
  | Statement.nop => Outcome.all_ok rfl

end Blaze.Mir

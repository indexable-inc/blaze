import Blaze.Mir.Opt.Rename

/-!
The commutation lemmas of `Opt/Rename.lean`, for any map on the stack that leaves memory
alone (`MemMap`): each frame's locals as found by its id, the frames a write replaces, and
the running frame's id. A read from the mapped state is the read, a write gives the mapped
state, and so on up to `execStmt_mapStack`.

`withTopBlock` (the running frame at another block), which block merging needs, is such a
map. The proofs are those of `Opt/Rename.lean`, with the three facts about `renameStack`
taken as hypotheses.
-/

namespace Blaze.Mir

/-- The running frame's id; 0 when there is none. -/
def stackTop : List Frame → Nat
  | [] => 0
  | fr :: _ => fr.id

theorem State.top_eq (st : State) : st.top = stackTop st.stack := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest => rfl

/-- A map on the stack that leaves memory alone. -/
structure MemMap (M : List Frame → List Frame) : Prop where
  locals : ∀ (l : List Frame) (id : Nat),
    (findFrame (M l) id).map (fun fr => fr.locals) = (findFrame l id).map (fun fr => fr.locals)
  set : ∀ (l : List Frame) (id : Nat) (slots : List Slot),
    setFrame (M l) id slots = M (setFrame l id slots)
  top : ∀ (l : List Frame), stackTop (M l) = stackTop l

def State.mapStack (M : List Frame → List Frame) (st : State) : State :=
  { base := st.base, stack := M st.stack, next := st.next }

theorem State.top_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    (st.mapStack M).top = st.top :=
  (State.top_eq (st.mapStack M)).trans ((hM.top st.stack).trans (State.top_eq st).symm)

theorem State.slots?_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (frame : Nat) :
    (st.mapStack M).slots? frame = st.slots? frame := by
  show (if frame = 0 then some st.base
      else (findFrame (M st.stack) frame).map (fun fr => fr.locals)) =
    (if frame = 0 then some st.base else (findFrame st.stack frame).map (fun fr => fr.locals))
  rw [hM.locals st.stack frame]

theorem State.slot_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (p : Pointer) :
    (st.mapStack M).slot p = st.slot p :=
  congrTo (slotIn p.slot) (State.slots?_mapStack hM st p.frame)

theorem State.read_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (p : Pointer) :
    (st.mapStack M).read p = st.read p :=
  Outcome.bind_congr (State.slot_mapStack hM st p) (fun _ _ => rfl)

theorem State.read_mapStack_fn {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    State.read (st.mapStack M) = State.read st :=
  funext (State.read_mapStack hM st)

theorem State.setSlots_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (f : Nat)
    (slots : List Slot) : (st.mapStack M).setSlots f slots = (st.setSlots f slots).mapStack M := by
  by_cases hf : f = 0
  · have left : (st.mapStack M).setSlots f slots =
        { base := slots, stack := M st.stack, next := st.next } :=
      (State.setSlots_def (st.mapStack M) f slots).trans (if_pos hf)
    have right : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_pos hf)
    exact left.trans (congrTo (State.mapStack M) right).symm
  · have left : (st.mapStack M).setSlots f slots =
        { base := st.base, stack := setFrame (M st.stack) f slots,
          next := st.next } :=
      (State.setSlots_def (st.mapStack M) f slots).trans (if_neg hf)
    have right : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_neg hf)
    have mid : State.mk st.base (setFrame (M st.stack) f slots) st.next =
        State.mapStack M (State.mk st.base (setFrame st.stack f slots) st.next) :=
      congrTo (fun s => State.mk st.base s st.next) (hM.set st.stack f slots)
    exact left.trans (mid.trans (congrTo (State.mapStack M) right).symm)

theorem State.setSlotIn_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (p : Pointer)
    (slot : Slot) :
    ∀ (o : Option (List Slot)),
      (st.mapStack M).setSlotIn p slot o = (st.setSlotIn p slot o).mapStack M
  | none => rfl
  | some slots => State.setSlots_mapStack hM st p.frame (setNth slots p.slot slot)

theorem State.setSlot_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (p : Pointer)
    (slot : Slot) : (st.mapStack M).setSlot p slot = (st.setSlot p slot).mapStack M :=
  (congrTo ((st.mapStack M).setSlotIn p slot) (State.slots?_mapStack hM st p.frame)).trans
    (State.setSlotIn_mapStack hM st p slot (st.slots? p.frame))

theorem State.write_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (p : Pointer)
    (v : Value) : (st.mapStack M).write p v = Outcome.map (State.mapStack M) (st.write p v) :=
  Outcome.bind_commutes (State.slot_mapStack hM st p) (fun _ _ =>
    Outcome.bind_commutes rfl (fun slot' _ =>
      congrTo Outcome.ok (State.setSlot_mapStack hM st p slot')))

theorem stateOr_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    ∀ (x : Outcome State),
      stateOr (st.mapStack M) (Outcome.map (State.mapStack M) x) = (stateOr st x).mapStack M
  | Outcome.ok _ => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

theorem State.moveOut_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    ∀ (p : Pointer), (st.mapStack M).moveOut p = (st.moveOut p).mapStack M
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_mapStack hM st { frame := frame, slot := slot, path := [] } Slot.moved
  | { frame := frame, slot := slot, path := e :: rest } =>
    (congrTo (stateOr (st.mapStack M))
        (State.write_mapStack hM st { frame := frame, slot := slot, path := e :: rest }
          Value.dead)).trans
      (stateOr_mapStack hM st
        (st.write { frame := frame, slot := slot, path := e :: rest } Value.dead))

/-! `projStep`, `evalProj`, `evalPlace`: a place is where it was. -/

theorem projStep_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (p : Pointer) :
    ∀ (e : ProjElem), projStep (st.mapStack M) p e = projStep st p e
  | ProjElem.deref => Outcome.bind_congr (State.read_mapStack hM st p) (fun _ _ => rfl)
  | ProjElem.field _ => rfl
  | ProjElem.downcast _ => rfl
  | ProjElem.index l =>
    have top : (st.mapStack M).read { frame := (st.mapStack M).top, slot := l, path := [] } =
        st.read { frame := st.top, slot := l, path := [] } :=
      (congrTo (fun f => (st.mapStack M).read { frame := f, slot := l, path := [] })
          (State.top_mapStack hM st)).trans
        (State.read_mapStack hM st { frame := st.top, slot := l, path := [] })
    Outcome.bind_congr top (fun _ _ =>
      Outcome.bind_congr (State.read_mapStack hM st p) (fun _ _ => rfl))
  | ProjElem.constIndex _ => Outcome.bind_congr (State.read_mapStack hM st p) (fun _ _ => rfl)

theorem evalProj_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    ∀ (proj : List ProjElem) (p : Pointer), evalProj (st.mapStack M) p proj = evalProj st p proj
  | [], _ => rfl
  | e :: rest, p =>
    Outcome.bind_congr (projStep_mapStack hM st p e) (fun q _ => evalProj_mapStack hM st rest q)

theorem evalPlace_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (place : Place) :
    evalPlace (st.mapStack M) place = evalPlace st place :=
  (congrTo (fun f => evalProj (st.mapStack M) { frame := f, slot := place.base, path := [] }
      place.proj) (State.top_mapStack hM st)).trans
    (evalProj_mapStack hM st place.proj { frame := st.top, slot := place.base, path := [] })

/-! `evalOperand`, `evalOperands`, `evalTwo`, `evalOne`, `evalAt`, `evalRvalue`, `execStmt`:
the same value, and the mapped state. -/

theorem evalOperand_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    ∀ (o : Operand),
      evalOperand (st.mapStack M) o = Outcome.map (keepState (State.mapStack M)) (evalOperand st o)
  | Operand.copy p =>
    Outcome.bind_commutes (evalPlace_mapStack hM st p) (fun ptr _ =>
      Outcome.bind_commutes (State.read_mapStack hM st ptr) (fun _ _ => rfl))
  | Operand.move p =>
    Outcome.bind_commutes (evalPlace_mapStack hM st p) (fun ptr _ =>
      Outcome.bind_commutes (State.read_mapStack hM st ptr) (fun v _ =>
        congrTo (fun s => Outcome.ok (v, s)) (State.moveOut_mapStack hM st ptr)))
  | Operand.const _ => rfl

theorem evalOperands_mapStack {M : List Frame → List Frame} (hM : MemMap M) :
    ∀ (os : List Operand) (st : State),
      evalOperands (st.mapStack M) os =
        Outcome.map (keepState (State.mapStack M)) (evalOperands st os)
  | [], _ => rfl
  | o :: rest, st =>
    Outcome.bind_commutes_map (evalOperand_mapStack hM st o) (fun a _ =>
      Outcome.bind_commutes_map (evalOperands_mapStack hM rest a.2) (fun _ _ => rfl))

theorem evalTwo_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (l r : Operand)
    (f : Value → Value → Outcome Value) :
    evalTwo (st.mapStack M) l r f = Outcome.map (keepState (State.mapStack M)) (evalTwo st l r f) :=
  Outcome.bind_commutes_map (evalOperand_mapStack hM st l) (fun a _ =>
    Outcome.bind_commutes_map (evalOperand_mapStack hM a.2 r) (fun _ _ =>
      Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalOne_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (o : Operand)
    (f : Value → Outcome Value) :
    evalOne (st.mapStack M) o f = Outcome.map (keepState (State.mapStack M)) (evalOne st o f) :=
  Outcome.bind_commutes_map (evalOperand_mapStack hM st o) (fun _ _ =>
    Outcome.bind_commutes rfl (fun _ _ => rfl))

theorem evalAt_mapStack {M : List Frame → List Frame} (hM : MemMap M) (st : State) (p : Place)
    (f : Value → Outcome Value) :
    evalAt (st.mapStack M) p f = Outcome.map (keepState (State.mapStack M)) (evalAt st p f) :=
  Outcome.bind_commutes (evalPlace_mapStack hM st p) (fun ptr _ =>
    Outcome.bind_commutes (State.read_mapStack hM st ptr) (fun _ _ =>
      Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalRvalue_mapStack (adts : List AdtDecl) {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    ∀ (rv : Rvalue),
      evalRvalue adts (st.mapStack M) rv =
        Outcome.map (keepState (State.mapStack M)) (evalRvalue adts st rv)
  | Rvalue.use o => evalOperand_mapStack hM st o
  | Rvalue.binary op l r => evalTwo_mapStack hM st l r (binaryOp false op)
  | Rvalue.checked op l r => evalTwo_mapStack hM st l r (binaryOp true op)
  | Rvalue.withOverflow op l r => evalTwo_mapStack hM st l r (overflowingOp op)
  | Rvalue.unary op o => evalOne_mapStack hM st o (unaryOp op)
  | Rvalue.cast o target => evalOne_mapStack hM st o (castTo target)
  | Rvalue.aggregate _ fields =>
    Outcome.bind_commutes_map (evalOperands_mapStack hM fields st) (fun _ _ => rfl)
  | Rvalue.replicate o count =>
    evalOne_mapStack hM st o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt_mapStack hM st p (discriminantOf adts adt)
  | Rvalue.ref _ p => Outcome.bind_commutes (evalPlace_mapStack hM st p) (fun _ _ => rfl)
  | Rvalue.len p => evalAt_mapStack hM st p lenOf

/-- A statement from the mapped state gives the mapped state. -/
theorem execStmt_mapStack (adts : List AdtDecl) {M : List Frame → List Frame} (hM : MemMap M) (st : State) :
    ∀ (s : Statement),
      execStmt adts (st.mapStack M) s = Outcome.map (State.mapStack M) (execStmt adts st s)
  | Statement.assign p rv =>
    Outcome.bind_commutes_map (evalRvalue_mapStack adts hM st rv) (fun r _ =>
      Outcome.bind_commutes (evalPlace_mapStack hM r.2 p) (fun ptr _ =>
        State.write_mapStack hM r.2 ptr r.1))
  | Statement.storageLive _ => rfl
  | Statement.storageDead _ => rfl
  | Statement.nop => rfl

end Blaze.Mir

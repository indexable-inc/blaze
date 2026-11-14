import Blaze.Mir.Opt.Sim
import Blaze.Mir.Opt.Ctl

/-!
A renumbering of blocks, and the semantics commuting with it
(docs/v2/plan/design/formal-verification.md, "The first proved transformations").

`ρ fn b` is the new number of block `b` of function `fn`. `State.rename ρ` is the state of
the renumbered program that stands for a state of the program: each frame's `block` is
renamed by its own function's map, and its `target` (the block its caller goes on at) by its
caller's. Nothing else changes.

Each function of `Step.lean`, in the file's order, is proved to commute with the map: a read
gives the same value (`State.read_rename`), a write the renamed state (`State.write_rename`),
and so on up to `execTerm_rename` and `step_rename`: when the renumbered program has, at
`ρ fn b`, block `b` with its targets renamed (`Renumbers`), one step of it from the renamed
state is the renamed step. `Renumbers.sim_total` is then a `Sim`, for a renumbering that
keeps every block; `Opt/Reach.lean` has the one that drops blocks control never reaches, and
`Opt/Renumber.lean` the passes.

The lemmas are applied as terms through `Opt/Bind.lean`: each says which `bind` of the
definition commutes and why its continuation does.
-/

namespace Blaze.Mir

def mapTarget (r : BlockId → BlockId) : Option BlockId → Option BlockId
  | some t => some (r t)
  | none => none

/-- A frame with its block renamed by its function's map and its return target by its
caller's. -/
def Frame.rename (ρ : FnId → BlockId → BlockId) (caller : FnId) (fr : Frame) : Frame :=
  { id := fr.id, fn := fr.fn, block := ρ fr.fn fr.block, locals := fr.locals, dest := fr.dest,
    target := mapTarget (ρ caller) fr.target }

/-- A frame's caller is the frame under it. -/
def renameStack (ρ : FnId → BlockId → BlockId) : List Frame → List Frame
  | [] => []
  | fr :: rest => fr.rename ρ (headFn (rest.map ctlOf)) :: renameStack ρ rest

/-- The state of the renumbered program that stands for `st`. -/
def State.rename (ρ : FnId → BlockId → BlockId) (st : State) : State :=
  { base := st.base, stack := renameStack ρ st.stack, next := st.next }

/-- A value with the state it leaves, renamed. -/
def keepState (g : State → State) {α : Type} (r : α × State) : α × State := (r.1, g r.2)

/-! `findFrame`, `State.slots?`, `State.slot`, `State.read`, `State.top`: a read is not
changed. -/

theorem findFrame_renameStack (ρ : FnId → BlockId → BlockId) (id : Nat) :
    ∀ (l : List Frame),
      (findFrame (renameStack ρ l) id).map (fun fr => fr.locals) =
        (findFrame l id).map (fun fr => fr.locals) := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have h' : (fr.rename ρ (headFn (rest.map ctlOf))).id = id := h
      have left : findFrame (renameStack ρ (fr :: rest)) id =
          some (fr.rename ρ (headFn (rest.map ctlOf))) :=
        (findFrame_cons (fr.rename ρ (headFn (rest.map ctlOf))) (renameStack ρ rest) id).trans
          (ite_eq_left h')
      have right : findFrame (fr :: rest) id = some fr :=
        (findFrame_cons fr rest id).trans (ite_eq_left h)
      rw [left, right]
      try rfl
    · have h' : ¬ (fr.rename ρ (headFn (rest.map ctlOf))).id = id := h
      have left : findFrame (renameStack ρ (fr :: rest)) id =
          findFrame (renameStack ρ rest) id :=
        (findFrame_cons (fr.rename ρ (headFn (rest.map ctlOf))) (renameStack ρ rest) id).trans
          (ite_eq_right h')
      have right : findFrame (fr :: rest) id = findFrame rest id :=
        (findFrame_cons fr rest id).trans (ite_eq_right h)
      rw [left, right]
      exact ih

theorem State.slots?_rename (ρ : FnId → BlockId → BlockId) (st : State) (frame : Nat) :
    (st.rename ρ).slots? frame = st.slots? frame := by
  show (if frame = 0 then some st.base
      else (findFrame (renameStack ρ st.stack) frame).map (fun fr => fr.locals)) =
    (if frame = 0 then some st.base else (findFrame st.stack frame).map (fun fr => fr.locals))
  rw [findFrame_renameStack ρ frame st.stack]

theorem State.slot_rename (ρ : FnId → BlockId → BlockId) (st : State) (p : Pointer) :
    (st.rename ρ).slot p = st.slot p :=
  congrTo (slotIn p.slot) (State.slots?_rename ρ st p.frame)

theorem State.read_rename (ρ : FnId → BlockId → BlockId) (st : State) (p : Pointer) :
    (st.rename ρ).read p = st.read p :=
  Outcome.bind_congr (State.slot_rename ρ st p) (fun _ _ => rfl)

theorem State.read_rename_fn (ρ : FnId → BlockId → BlockId) (st : State) :
    State.read (st.rename ρ) = State.read st :=
  funext (State.read_rename ρ st)

theorem State.top_rename (ρ : FnId → BlockId → BlockId) (st : State) :
    (st.rename ρ).top = st.top := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest => rfl

/-! `setFrame`, `State.setSlots`, `State.setSlot`, `State.write`, `State.moveOut`: a write
gives the renamed state. -/

theorem setFrame_renameStack (ρ : FnId → BlockId → BlockId) (id : Nat) (slots : List Slot) :
    ∀ (l : List Frame),
      setFrame (renameStack ρ l) id slots = renameStack ρ (setFrame l id slots) := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have h' : (fr.rename ρ (headFn (rest.map ctlOf))).id = id := h
      have left : setFrame (renameStack ρ (fr :: rest)) id slots =
          (fr.rename ρ (headFn (rest.map ctlOf))).withLocals slots :: renameStack ρ rest :=
        (setFrame_cons (fr.rename ρ (headFn (rest.map ctlOf))) (renameStack ρ rest) id
          slots).trans (ite_eq_left h')
      have right : setFrame (fr :: rest) id slots = fr.withLocals slots :: rest :=
        (setFrame_cons fr rest id slots).trans (ite_eq_left h)
      exact left.trans (congrTo (renameStack ρ) right).symm
    · have h' : ¬ (fr.rename ρ (headFn (rest.map ctlOf))).id = id := h
      have left : setFrame (renameStack ρ (fr :: rest)) id slots =
          fr.rename ρ (headFn (rest.map ctlOf)) :: setFrame (renameStack ρ rest) id slots :=
        (setFrame_cons (fr.rename ρ (headFn (rest.map ctlOf))) (renameStack ρ rest) id
          slots).trans (ite_eq_right h')
      have right : setFrame (fr :: rest) id slots = fr :: setFrame rest id slots :=
        (setFrame_cons fr rest id slots).trans (ite_eq_right h)
      have hd : headFn ((setFrame rest id slots).map ctlOf) = headFn (rest.map ctlOf) :=
        congrTo headFn (setFrame_ctl id slots rest)
      have last : fr.rename ρ (headFn (rest.map ctlOf)) :: renameStack ρ (setFrame rest id slots) =
          renameStack ρ (fr :: setFrame rest id slots) :=
        congrTo (fun c => fr.rename ρ c :: renameStack ρ (setFrame rest id slots)) hd.symm
      exact left.trans
        ((congrTo (fun t => fr.rename ρ (headFn (rest.map ctlOf)) :: t) ih).trans
          (last.trans (congrTo (renameStack ρ) right).symm))

theorem State.setSlots_rename (ρ : FnId → BlockId → BlockId) (st : State) (f : Nat)
    (slots : List Slot) : (st.rename ρ).setSlots f slots = (st.setSlots f slots).rename ρ := by
  by_cases hf : f = 0
  · have left : (st.rename ρ).setSlots f slots =
        { base := slots, stack := renameStack ρ st.stack, next := st.next } :=
      (State.setSlots_def (st.rename ρ) f slots).trans (ite_eq_left hf)
    have right : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (ite_eq_left hf)
    exact left.trans (congrTo (State.rename ρ) right).symm
  · have left : (st.rename ρ).setSlots f slots =
        { base := st.base, stack := setFrame (renameStack ρ st.stack) f slots,
          next := st.next } :=
      (State.setSlots_def (st.rename ρ) f slots).trans (ite_eq_right hf)
    have right : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (ite_eq_right hf)
    have mid : State.mk st.base (setFrame (renameStack ρ st.stack) f slots) st.next =
        State.rename ρ (State.mk st.base (setFrame st.stack f slots) st.next) :=
      congrTo (fun s => State.mk st.base s st.next) (setFrame_renameStack ρ f slots st.stack)
    exact left.trans (mid.trans (congrTo (State.rename ρ) right).symm)

theorem State.setSlotIn_rename (ρ : FnId → BlockId → BlockId) (st : State) (p : Pointer)
    (slot : Slot) :
    ∀ (o : Option (List Slot)),
      (st.rename ρ).setSlotIn p slot o = (st.setSlotIn p slot o).rename ρ
  | none => rfl
  | some slots => State.setSlots_rename ρ st p.frame (setNth slots p.slot slot)

theorem State.setSlot_rename (ρ : FnId → BlockId → BlockId) (st : State) (p : Pointer)
    (slot : Slot) : (st.rename ρ).setSlot p slot = (st.setSlot p slot).rename ρ :=
  (congrTo ((st.rename ρ).setSlotIn p slot) (State.slots?_rename ρ st p.frame)).trans
    (State.setSlotIn_rename ρ st p slot (st.slots? p.frame))

theorem State.write_rename (ρ : FnId → BlockId → BlockId) (st : State) (p : Pointer)
    (v : Value) : (st.rename ρ).write p v = Outcome.map (State.rename ρ) (st.write p v) :=
  Outcome.bind_commutes (State.slot_rename ρ st p) (fun _ _ =>
    Outcome.bind_commutes rfl (fun slot' _ =>
      congrTo Outcome.ok (State.setSlot_rename ρ st p slot')))

theorem stateOr_rename (ρ : FnId → BlockId → BlockId) (st : State) :
    ∀ (x : Outcome State),
      stateOr (st.rename ρ) (Outcome.map (State.rename ρ) x) = (stateOr st x).rename ρ
  | Outcome.ok _ => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

theorem State.moveOut_rename (ρ : FnId → BlockId → BlockId) (st : State) :
    ∀ (p : Pointer), (st.rename ρ).moveOut p = (st.moveOut p).rename ρ
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_rename ρ st { frame := frame, slot := slot, path := [] } Slot.moved
  | { frame := frame, slot := slot, path := e :: rest } =>
    (congrTo (stateOr (st.rename ρ))
        (State.write_rename ρ st { frame := frame, slot := slot, path := e :: rest }
          Value.dead)).trans
      (stateOr_rename ρ st
        (st.write { frame := frame, slot := slot, path := e :: rest } Value.dead))

/-! `projStep`, `evalProj`, `evalPlace`: a place is where it was. -/

theorem projStep_rename (ρ : FnId → BlockId → BlockId) (st : State) (p : Pointer) :
    ∀ (e : ProjElem), projStep (st.rename ρ) p e = projStep st p e
  | ProjElem.deref => Outcome.bind_congr (State.read_rename ρ st p) (fun _ _ => rfl)
  | ProjElem.field _ => rfl
  | ProjElem.downcast _ => rfl
  | ProjElem.index l =>
    have top : (st.rename ρ).read { frame := (st.rename ρ).top, slot := l, path := [] } =
        st.read { frame := st.top, slot := l, path := [] } :=
      (congrTo (fun f => (st.rename ρ).read { frame := f, slot := l, path := [] })
          (State.top_rename ρ st)).trans
        (State.read_rename ρ st { frame := st.top, slot := l, path := [] })
    Outcome.bind_congr top (fun _ _ =>
      Outcome.bind_congr (State.read_rename ρ st p) (fun _ _ => rfl))
  | ProjElem.constIndex _ => Outcome.bind_congr (State.read_rename ρ st p) (fun _ _ => rfl)

theorem evalProj_rename (ρ : FnId → BlockId → BlockId) (st : State) :
    ∀ (proj : List ProjElem) (p : Pointer), evalProj (st.rename ρ) p proj = evalProj st p proj
  | [], _ => rfl
  | e :: rest, p =>
    Outcome.bind_congr (projStep_rename ρ st p e) (fun q _ => evalProj_rename ρ st rest q)

theorem evalPlace_rename (ρ : FnId → BlockId → BlockId) (st : State) (place : Place) :
    evalPlace (st.rename ρ) place = evalPlace st place :=
  (congrTo (fun f => evalProj (st.rename ρ) { frame := f, slot := place.base, path := [] }
      place.proj) (State.top_rename ρ st)).trans
    (evalProj_rename ρ st place.proj { frame := st.top, slot := place.base, path := [] })

/-! `evalOperand`, `evalOperands`, `evalTwo`, `evalOne`, `evalAt`, `evalRvalue`, `execStmt`:
the same value, and the renamed state. -/

theorem evalOperand_rename (ρ : FnId → BlockId → BlockId) (st : State) :
    ∀ (o : Operand),
      evalOperand (st.rename ρ) o = Outcome.map (keepState (State.rename ρ)) (evalOperand st o)
  | Operand.copy p =>
    Outcome.bind_commutes (evalPlace_rename ρ st p) (fun ptr _ =>
      Outcome.bind_commutes (State.read_rename ρ st ptr) (fun _ _ => rfl))
  | Operand.move p =>
    Outcome.bind_commutes (evalPlace_rename ρ st p) (fun ptr _ =>
      Outcome.bind_commutes (State.read_rename ρ st ptr) (fun v _ =>
        congrTo (fun s => Outcome.ok (v, s)) (State.moveOut_rename ρ st ptr)))
  | Operand.const _ => rfl

theorem evalOperands_rename (ρ : FnId → BlockId → BlockId) :
    ∀ (os : List Operand) (st : State),
      evalOperands (st.rename ρ) os =
        Outcome.map (keepState (State.rename ρ)) (evalOperands st os)
  | [], _ => rfl
  | o :: rest, st =>
    Outcome.bind_commutes_map (evalOperand_rename ρ st o) (fun a _ =>
      Outcome.bind_commutes_map (evalOperands_rename ρ rest a.2) (fun _ _ => rfl))

theorem evalTwo_rename (ρ : FnId → BlockId → BlockId) (st : State) (l r : Operand)
    (f : Value → Value → Outcome Value) :
    evalTwo (st.rename ρ) l r f = Outcome.map (keepState (State.rename ρ)) (evalTwo st l r f) :=
  Outcome.bind_commutes_map (evalOperand_rename ρ st l) (fun a _ =>
    Outcome.bind_commutes_map (evalOperand_rename ρ a.2 r) (fun _ _ =>
      Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalOne_rename (ρ : FnId → BlockId → BlockId) (st : State) (o : Operand)
    (f : Value → Outcome Value) :
    evalOne (st.rename ρ) o f = Outcome.map (keepState (State.rename ρ)) (evalOne st o f) :=
  Outcome.bind_commutes_map (evalOperand_rename ρ st o) (fun _ _ =>
    Outcome.bind_commutes rfl (fun _ _ => rfl))

theorem evalAt_rename (ρ : FnId → BlockId → BlockId) (st : State) (p : Place)
    (f : Value → Outcome Value) :
    evalAt (st.rename ρ) p f = Outcome.map (keepState (State.rename ρ)) (evalAt st p f) :=
  Outcome.bind_commutes (evalPlace_rename ρ st p) (fun ptr _ =>
    Outcome.bind_commutes (State.read_rename ρ st ptr) (fun _ _ =>
      Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalRvalue_rename (adts : List AdtDecl) (ρ : FnId → BlockId → BlockId) (st : State) :
    ∀ (rv : Rvalue),
      evalRvalue adts (st.rename ρ) rv =
        Outcome.map (keepState (State.rename ρ)) (evalRvalue adts st rv)
  | Rvalue.use o => evalOperand_rename ρ st o
  | Rvalue.binary op l r => evalTwo_rename ρ st l r (binaryOp false op)
  | Rvalue.checked op l r => evalTwo_rename ρ st l r (binaryOp true op)
  | Rvalue.withOverflow op l r => evalTwo_rename ρ st l r (overflowingOp op)
  | Rvalue.unary op o => evalOne_rename ρ st o (unaryOp op)
  | Rvalue.cast o target => evalOne_rename ρ st o (castTo target)
  | Rvalue.aggregate _ fields =>
    Outcome.bind_commutes_map (evalOperands_rename ρ fields st) (fun _ _ => rfl)
  | Rvalue.replicate o count =>
    evalOne_rename ρ st o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt_rename ρ st p (discriminantOf adts adt)
  | Rvalue.ref _ p => Outcome.bind_commutes (evalPlace_rename ρ st p) (fun _ _ => rfl)
  | Rvalue.len p => evalAt_rename ρ st p lenOf

/-- A statement from the renamed state gives the renamed state. -/
theorem execStmt_rename (adts : List AdtDecl) (ρ : FnId → BlockId → BlockId) (st : State) :
    ∀ (s : Statement),
      execStmt adts (st.rename ρ) s = Outcome.map (State.rename ρ) (execStmt adts st s)
  | Statement.assign p rv =>
    Outcome.bind_commutes_map (evalRvalue_rename adts ρ st rv) (fun r _ =>
      Outcome.bind_commutes (evalPlace_rename ρ r.2 p) (fun ptr _ =>
        State.write_rename ρ r.2 ptr r.1))
  | Statement.storageLive _ => rfl
  | Statement.storageDead _ => rfl
  | Statement.nop => rfl

/-! Terminators: a jump goes to the renamed block. Each lemma takes the function the running
frame runs, `fn`, with `st.topFn = fn`: it is the map a target is renamed by. -/

theorem mapNext_eq (f : State → State) :
    ∀ (x : Outcome Next), mapNext f x = Outcome.map (Next.map f) x
  | Outcome.ok _ => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

theorem State.jump_rename (ρ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (t : BlockId) :
    (st.rename ρ).jump (ρ fn t) = Outcome.map (Next.map (State.rename ρ)) (st.jump t) := by
  subst hfn
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest => rfl

theorem State.jump?_rename (ρ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) :
    ∀ (target : Option BlockId),
      (st.rename ρ).jump? (mapTarget (ρ fn) target) =
        Outcome.map (Next.map (State.rename ρ)) (st.jump? target)
  | some t => State.jump_rename ρ st fn hfn t
  | none => rfl

def renameArms (r : BlockId → BlockId) : List (Int × BlockId) → List (Int × BlockId)
  | [] => []
  | arm :: rest => (arm.1, r arm.2) :: renameArms r rest

theorem switchTarget_cons (key : Int) (arm : Int × BlockId) (rest : List (Int × BlockId))
    (otherwise : BlockId) :
    switchTarget key (arm :: rest) otherwise =
      if key = arm.1 then arm.2 else switchTarget key rest otherwise := rfl

/-- The arm a key takes among renamed arms is the renamed arm. -/
theorem switchTarget_rename (r : BlockId → BlockId) (key : Int) (otherwise : BlockId) :
    ∀ (arms : List (Int × BlockId)),
      switchTarget key (renameArms r arms) (r otherwise) = r (switchTarget key arms otherwise) := by
  intro arms
  induction arms with
  | nil => rfl
  | cons arm rest ih =>
    have left : switchTarget key (renameArms r (arm :: rest)) (r otherwise) =
        if key = arm.1 then r arm.2 else switchTarget key (renameArms r rest) (r otherwise) :=
      switchTarget_cons key (arm.1, r arm.2) (renameArms r rest) (r otherwise)
    have right : switchTarget key (arm :: rest) otherwise =
        if key = arm.1 then arm.2 else switchTarget key rest otherwise :=
      switchTarget_cons key arm rest otherwise
    by_cases h : key = arm.1
    · exact (left.trans (ite_eq_left h)).trans (congrTo r (right.trans (ite_eq_left h))).symm
    · exact (left.trans (ite_eq_right h)).trans (ih.trans (congrTo r (right.trans (ite_eq_right h))).symm)

theorem switchOn_rename (ρ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (arms : List (Int × BlockId)) (otherwise : BlockId) :
    ∀ (key : Option Int),
      switchOn (st.rename ρ) (renameArms (ρ fn) arms) (ρ fn otherwise) key =
        Outcome.map (Next.map (State.rename ρ)) (switchOn st arms otherwise key)
  | some key =>
    (congrTo (State.jump (st.rename ρ)) (switchTarget_rename (ρ fn) key otherwise arms)).trans
      (State.jump_rename ρ st fn hfn (switchTarget key arms otherwise))
  | none => rfl

theorem assertOn_bool (st : State) (expected : Bool) (kind : Error) (target : BlockId)
    (b : Bool) :
    assertOn st expected kind target (Value.bool b) =
      if b = expected then st.jump target else Outcome.panic kind := rfl

theorem assertOn_rename (ρ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (expected : Bool) (kind : Error) (target : BlockId) :
    ∀ (v : Value),
      assertOn (st.rename ρ) expected kind (ρ fn target) v =
        Outcome.map (Next.map (State.rename ρ)) (assertOn st expected kind target v)
  | Value.bool b => by
    by_cases h : b = expected
    · have left : assertOn (st.rename ρ) expected kind (ρ fn target) (Value.bool b) =
          (st.rename ρ).jump (ρ fn target) :=
        (assertOn_bool (st.rename ρ) expected kind (ρ fn target) b).trans (ite_eq_left h)
      have right : assertOn st expected kind target (Value.bool b) = st.jump target :=
        (assertOn_bool st expected kind target b).trans (ite_eq_left h)
      exact left.trans ((State.jump_rename ρ st fn hfn target).trans
        (congrTo (Outcome.map (Next.map (State.rename ρ))) right).symm)
    · have left : assertOn (st.rename ρ) expected kind (ρ fn target) (Value.bool b) =
          Outcome.panic kind :=
        (assertOn_bool (st.rename ρ) expected kind (ρ fn target) b).trans (ite_eq_right h)
      have right : assertOn st expected kind target (Value.bool b) = Outcome.panic kind :=
        (assertOn_bool st expected kind target b).trans (ite_eq_right h)
      exact left.trans (congrTo (Outcome.map (Next.map (State.rename ρ))) right).symm
  | Value.unit => rfl
  | Value.int _ _ => rfl
  | Value.adt _ _ => rfl
  | Value.array _ => rfl
  | Value.ref _ => rfl
  | Value.dead => rfl

/-- The state a call enters, when the arguments are as many as the parameters. -/
def enterState (st : State) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) : State :=
  { base := st.base,
    stack :=
      ({ id := st.next, fn := id, block := 0,
         locals :=
           Slot.uninit ::
             (args.map Slot.init ++ List.replicate (sig.locals - 1 - sig.args) Slot.uninit),
         dest := dest, target := target } : Frame) :: st.stack,
    next := st.next + 1 }

theorem State.enter_eq (st : State) (id : FnId) (sig : FnSig) (args : List Value)
    (dest : Pointer) (target : Option BlockId) :
    st.enter id sig args dest target =
      if args.length = sig.args then
        Outcome.ok (Next.state (enterState st id sig args dest target))
      else Outcome.undefined Undefined.illFormed := rfl

/-- The entry block keeps its number, so the frame a call enters is the renamed frame. -/
theorem enterState_rename (ρ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) (h0 : ρ id 0 = 0) :
    enterState (st.rename ρ) id sig args dest (mapTarget (ρ fn) target) =
      (enterState st id sig args dest target).rename ρ := by
  subst hfn
  exact congrTo
    (fun b =>
      State.mk st.base
        (Frame.mk st.next id b
            (Slot.uninit ::
              (args.map Slot.init ++ List.replicate (sig.locals - 1 - sig.args) Slot.uninit))
            dest (mapTarget (ρ st.topFn) target) ::
          renameStack ρ st.stack)
        (st.next + 1))
    h0.symm

theorem State.enter_rename (ρ : FnId → BlockId → BlockId) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) (h0 : ρ id 0 = 0) :
    (st.rename ρ).enter id sig args dest (mapTarget (ρ fn) target) =
      Outcome.map (Next.map (State.rename ρ)) (st.enter id sig args dest target) := by
  by_cases hl : args.length = sig.args
  · have left : (st.rename ρ).enter id sig args dest (mapTarget (ρ fn) target) =
        Outcome.ok (Next.state
          (enterState (st.rename ρ) id sig args dest (mapTarget (ρ fn) target))) :=
      (State.enter_eq (st.rename ρ) id sig args dest (mapTarget (ρ fn) target)).trans (ite_eq_left hl)
    have right : st.enter id sig args dest target =
        Outcome.ok (Next.state (enterState st id sig args dest target)) :=
      (State.enter_eq st id sig args dest target).trans (ite_eq_left hl)
    have mid : Outcome.map (Next.map (State.rename ρ))
          (Outcome.ok (Next.state (enterState st id sig args dest target))) =
        Outcome.ok (Next.state
          (enterState (st.rename ρ) id sig args dest (mapTarget (ρ fn) target))) :=
      (congrTo (fun s => Outcome.ok (Next.state s))
        (enterState_rename ρ st fn hfn id sig args dest target h0)).symm
    exact left.trans
      (mid.symm.trans (congrTo (Outcome.map (Next.map (State.rename ρ))) right).symm)
  · have left : (st.rename ρ).enter id sig args dest (mapTarget (ρ fn) target) =
        Outcome.undefined Undefined.illFormed :=
      (State.enter_eq (st.rename ρ) id sig args dest (mapTarget (ρ fn) target)).trans (ite_eq_right hl)
    have right : st.enter id sig args dest target = Outcome.undefined Undefined.illFormed :=
      (State.enter_eq st id sig args dest target).trans (ite_eq_right hl)
    exact left.trans (congrTo (Outcome.map (Next.map (State.rename ρ))) right).symm

/-- The running frame returns: its caller goes on at the renamed target. -/
theorem State.ret_rename (ρ : FnId → BlockId → BlockId) (st : State) :
    (st.rename ρ).ret = Outcome.map (Next.map (State.rename ρ)) st.ret := by
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest =>
      cases fr with
      | mk fid ffn fblock locals dest target =>
        cases locals with
        | nil => rfl
        | cons slot locals =>
          cases rest with
          | nil => exact Outcome.bind_commutes rfl (fun _ _ => rfl)
          | cons caller rest =>
            exact Outcome.bind_commutes rfl (fun v _ =>
              Outcome.bind_commutes_map
                (State.write_rename ρ { base := base, stack := caller :: rest, next := next }
                  dest v)
                (fun st1 h1 =>
                  State.jump?_rename ρ st1 caller.fn
                    (State.topFn_of_ctl
                      (State.write_ctl { base := base, stack := caller :: rest, next := next }
                        dest v st1 h1))
                    target))

/-- An intrinsic reads memory and nothing else of the state. -/
theorem Intrinsic.eval_rename (ρ : FnId → BlockId → BlockId) (st : State) (i : Intrinsic)
    (args : List Value) : i.eval (st.rename ρ) args = i.eval st args := by
  unfold Intrinsic.eval
  rw [State.read_rename_fn ρ st]

theorem callOn_rename (ρ : FnId → BlockId → BlockId) (sig : FnId → Option FnSig)
    (h0 : ∀ (id : FnId) (s : FnSig), sig id = some s → ρ id 0 = 0) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (args : List Value) (dest : Pointer) (target : Option BlockId) :
    ∀ (callee : Callee),
      callOn sig (st.rename ρ) callee args dest (mapTarget (ρ fn) target) =
        Outcome.map (Next.map (State.rename ρ)) (callOn sig st callee args dest target)
  | Callee.fn id => by
    simp only [callOn]
    cases hs : sig id with
    | none => rfl
    | some s => exact State.enter_rename ρ st fn hfn id s args dest target (h0 id s hs)
  | Callee.intrinsic i =>
    Outcome.bind_commutes (Intrinsic.eval_rename ρ st i args) (fun v _ =>
      Outcome.bind_commutes_map (State.write_rename ρ st dest v) (fun st1 h1 =>
        State.jump?_rename ρ st1 fn
          ((State.topFn_of_ctl (State.write_ctl st dest v st1 h1)).trans hfn) target))

/-- A terminator with its targets renamed by `r`. -/
def Terminator.rename (r : BlockId → BlockId) : Terminator → Terminator
  | Terminator.goto t => Terminator.goto (r t)
  | Terminator.switchInt discr arms otherwise =>
    Terminator.switchInt discr (renameArms r arms) (r otherwise)
  | Terminator.call callee args dest target => Terminator.call callee args dest (mapTarget r target)
  | Terminator.ret => Terminator.ret
  | Terminator.assert cond expected kind target => Terminator.assert cond expected kind (r target)
  | Terminator.drop p target => Terminator.drop p (r target)
  | Terminator.unreachable => Terminator.unreachable

def BasicBlock.rename (r : BlockId → BlockId) (blk : BasicBlock) : BasicBlock :=
  { stmts := blk.stmts, terminator := blk.terminator.rename r }

/-- A renamed terminator from the renamed state gives the renamed next state. -/
theorem execTerm_rename (ρ : FnId → BlockId → BlockId) (sig : FnId → Option FnSig)
    (h0 : ∀ (id : FnId) (s : FnSig), sig id = some s → ρ id 0 = 0) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) :
    ∀ (term : Terminator),
      execTerm sig (st.rename ρ) (term.rename (ρ fn)) =
        Outcome.map (Next.map (State.rename ρ)) (execTerm sig st term)
  | Terminator.goto t => State.jump_rename ρ st fn hfn t
  | Terminator.switchInt discr arms otherwise =>
    Outcome.bind_commutes_map (evalOperand_rename ρ st discr) (fun r hr =>
      switchOn_rename ρ r.2 fn
        ((State.topFn_of_ctl (evalOperand_ctl st discr r hr)).trans hfn) arms otherwise
        (switchKey r.1))
  | Terminator.call callee args dest target =>
    Outcome.bind_commutes_map (evalOperands_rename ρ args st) (fun r hr =>
      Outcome.bind_commutes (evalPlace_rename ρ r.2 dest) (fun ptr _ =>
        callOn_rename ρ sig h0 r.2 fn
          ((State.topFn_of_ctl (evalOperands_ctl args st r hr)).trans hfn) r.1 ptr target
          callee))
  | Terminator.ret => State.ret_rename ρ st
  | Terminator.assert cond expected kind target =>
    Outcome.bind_commutes_map (evalOperand_rename ρ st cond) (fun r hr =>
      assertOn_rename ρ r.2 fn
        ((State.topFn_of_ctl (evalOperand_ctl st cond r hr)).trans hfn) expected kind target
        r.1)
  | Terminator.drop _ t => State.jump_rename ρ st fn hfn t
  | Terminator.unreachable => rfl

/-! Blocks and the step. -/

theorem execBlockFrom_rename (ρ : FnId → BlockId → BlockId) (adts : List AdtDecl)
    (sig : FnId → Option FnSig)
    (h0 : ∀ (id : FnId) (s : FnSig), sig id = some s → ρ id 0 = 0) (fn : FnId)
    (term : Terminator) :
    ∀ (stmts : List Statement) (st : State), st.topFn = fn →
      execBlockFrom adts sig (st.rename ρ) stmts (term.rename (ρ fn)) =
        Outcome.map (Next.map (State.rename ρ)) (execBlockFrom adts sig st stmts term)
  | [], st, hfn => execTerm_rename ρ sig h0 st fn hfn term
  | s :: rest, st, hfn =>
    Outcome.bind_commutes_map (execStmt_rename adts ρ st s) (fun st1 h1 =>
      execBlockFrom_rename ρ adts sig h0 fn term rest st1
        ((State.topFn_of_ctl (execStmt_ctl adts st s st1 h1)).trans hfn))

theorem execBlock_rename (ρ : FnId → BlockId → BlockId) (adts : List AdtDecl)
    (sig : FnId → Option FnSig)
    (h0 : ∀ (id : FnId) (s : FnSig), sig id = some s → ρ id 0 = 0) (st : State) (fn : FnId)
    (hfn : st.topFn = fn) (blk : BasicBlock) :
    execBlock adts sig (st.rename ρ) (blk.rename (ρ fn)) =
      Outcome.map (Next.map (State.rename ρ)) (execBlock adts sig st blk) :=
  execBlockFrom_rename ρ adts sig h0 fn blk.terminator blk.stmts st hfn

/-- The running frame's block is one `L` holds of. -/
def TopLive (L : FnId → BlockId → Prop) : List Ctl → Prop
  | [] => True
  | c :: _ => L c.1 c.2.1

theorem topLive_true : ∀ (l : List Ctl), TopLive (fun _ _ => True) l
  | [] => True.intro
  | _ :: _ => True.intro

/-- `tgt` is `src` with its blocks renumbered by `ρ`, on the blocks `L` holds of: the same
types and signatures, every entry block still block 0, and at `ρ fn b` the block `b` of `src`
with its targets renamed. -/
structure Renumbers (src tgt : Program) (ρ : FnId → BlockId → BlockId)
    (L : FnId → BlockId → Prop) : Prop where
  adts : tgt.adts = src.adts
  sig : tgt.sig = src.sig
  entry : ∀ (id : FnId) (s : FnSig), src.sig id = some s → ρ id 0 = 0
  block : ∀ (fn : FnId) (b : BlockId), L fn b →
    tgt.block fn (ρ fn b) = (src.block fn b).map (BasicBlock.rename (ρ fn))

/-- One step of the renumbered program from the renamed state is the renamed step. -/
theorem step_rename {src tgt : Program} {ρ : FnId → BlockId → BlockId}
    {L : FnId → BlockId → Prop} (h : Renumbers src tgt ρ L) :
    ∀ (st : State), TopLive L st.ctl →
      step tgt (st.rename ρ) = Outcome.map (Next.map (State.rename ρ)) (step src st) := by
  intro st hlive
  cases st with
  | mk base stack next =>
    cases stack with
    | nil => rfl
    | cons fr rest =>
      have hb : tgt.block fr.fn (ρ fr.fn fr.block) =
          (src.block fr.fn fr.block).map (BasicBlock.rename (ρ fr.fn)) :=
        h.block fr.fn fr.block hlive
      show execBlock? tgt.adts tgt.sig
          (State.rename ρ { base := base, stack := fr :: rest, next := next })
          (tgt.block fr.fn (ρ fr.fn fr.block)) =
        Outcome.map (Next.map (State.rename ρ))
          (execBlock? src.adts src.sig { base := base, stack := fr :: rest, next := next }
            (src.block fr.fn fr.block))
      rw [hb, h.adts, h.sig]
      cases src.block fr.fn fr.block with
      | none => rfl
      | some blk =>
        exact execBlock_rename ρ src.adts src.sig h.entry
          { base := base, stack := fr :: rest, next := next } fr.fn rfl blk

/-- A renumbering that keeps every block is a lockstep simulation, with no invariant. -/
theorem Renumbers.sim_total {src tgt : Program} {ρ : FnId → BlockId → BlockId}
    (h : Renumbers src tgt ρ (fun _ _ => True)) :
    Sim src tgt (State.rename ρ) (fun _ => True) where
  commutes := fun st _ =>
    (step_rename h st (topLive_true st.ctl)).trans
      (mapNext_eq (State.rename ρ) (step src st)).symm
  preserved := fun _ _ _ _ => True.intro

end Blaze.Mir

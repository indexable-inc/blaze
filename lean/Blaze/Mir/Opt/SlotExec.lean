import Blaze.Mir.Opt.BorrowStep
import Blaze.Mir.Opt.SlotAlgebra

/-!
Statements that do not mention a local do the same whatever that local holds: dead-store
elimination's proof, fifth piece, second part.

`st.put f l sl` is the state with slot `l` of frame `f` replaced by `sl`. When `f` is the
running frame, `l` a local its function does not borrow (so no pointer in memory names the
slot: `Hides`), and a statement mentions `l` neither as a place's base nor as an index
(`stmtFine`), the statement from `st.put f l sl` gives what it gives from `st`, with the
slot replaced the same way (`execStmt_put`). Each function of `Step.lean` up to `execStmt`,
in the file's order, as in `Opt/Rename.lean`.
-/

namespace Blaze.Mir

/-- The state with slot `l` of frame `f` replaced. -/
def State.put (st : State) (f l : Nat) (sl : Slot) : State :=
  st.setSlot { frame := f, slot := l, path := [] } sl

/-- The pointer is not into slot `l` of frame `f`. -/
def Off (f l : Nat) (q : Pointer) : Prop := ¬ f = q.frame ∨ ¬ l = q.slot

theorem off_same {f l : Nat} {p q : Pointer} (h : Off f l p)
    (s : q.frame = p.frame ∧ q.slot = p.slot) : Off f l q := by
  have h' : ¬ f = p.frame ∨ ¬ l = p.slot := h
  cases h' with
  | inl a =>
    have a' : ¬ f = q.frame := by
      rw [And.left s]
      exact a
    exact Or.inl a'
  | inr b =>
    have b' : ¬ l = q.slot := by
      rw [And.right s]
      exact b
    exact Or.inr b'

/-- No pointer `PtrOk` allows is into slot `l` of frame `f`, when `f` is a call's frame and
its function does not borrow `l`. -/
theorem ptrOk_off (B : FnId → Local → Prop) (g : List (Nat × FnId)) (f l : Nat) (fn : FnId)
    (hf : fnOfIds g f = some fn) (hb : ¬ B fn l) (hpos : ¬ f = 0) (q : Pointer)
    (hq : PtrOk B g q) : Off f l q := by
  by_cases e1 : f = q.frame
  · by_cases e2 : l = q.slot
    · have hq' : q.frame = 0 ∨ FrameHas B q.slot (fnOfIds g q.frame) := hq
      cases hq' with
      | inl zero => exact absurd (e1.trans zero) hpos
      | inr has =>
        rw [← e1, hf, ← e2] at has
        exact absurd has hb
    · exact Or.inr e2
  · exact Or.inl e1

/-! The running frame's id. -/

theorem State.top_of_ids :
    ∀ (st st' : State), st'.ids = st.ids → st'.top = st.top
  | { base := _, stack := [], next := _ }, { base := _, stack := [], next := _ }, _ => rfl
  | { base := _, stack := [], next := _ }, { base := _, stack := b :: l, next := _ }, h => by
    have h' : idFnOf b :: l.map idFnOf = ([] : List (Nat × FnId)) := h
    cases h'
  | { base := _, stack := a :: l, next := _ }, { base := _, stack := [], next := _ }, h => by
    have h' : ([] : List (Nat × FnId)) = idFnOf a :: l.map idFnOf := h
    cases h'
  | { base := _, stack := a :: la, next := _ }, { base := _, stack := b :: lb, next := _ }, h => by
    have h' : idFnOf b :: lb.map idFnOf = idFnOf a :: la.map idFnOf := h
    exact congrTo Prod.fst (And.left (List.cons.inj h'))

theorem State.put_ids (st : State) (f l : Nat) (sl : Slot) : (st.put f l sl).ids = st.ids :=
  State.setSlot_ids st { frame := f, slot := l, path := [] } sl

theorem State.put_top (st : State) (f l : Nat) (sl : Slot) : (st.put f l sl).top = st.top :=
  State.top_of_ids st (st.put f l sl) (State.put_ids st f l sl)

theorem State.read_put (st : State) (f l : Nat) (sl : Slot) (q : Pointer) (hq : Off f l q) :
    (st.put f l sl).read q = st.read q :=
  State.read_setSlot_other st { frame := f, slot := l, path := [] } q sl hq

/-! A projection other than a dereference stays in its slot. -/

theorem inBounds_same (p : Pointer) (n : Nat) :
    ∀ (o : Option Value) (q : Pointer), inBounds p n o = Outcome.ok q →
      q.frame = p.frame ∧ q.slot = p.slot
  | some _, q, h => by
    have e : p.push (PathElem.index n) = q := Outcome.ok.inj h
    rw [← e]
    exact And.intro rfl rfl
  | none, _, h => panic_ne_ok h

theorem indexAt_same (p : Pointer) (n : Nat) :
    ∀ (arr : Value) (q : Pointer), indexAt p n arr = Outcome.ok q →
      q.frame = p.frame ∧ q.slot = p.slot
  | Value.array elems, q, h => inBounds_same p n (nth? elems n) q h
  | Value.unit, _, h => undefined_ne_ok h
  | Value.bool _, _, h => undefined_ne_ok h
  | Value.int _ _, _, h => undefined_ne_ok h
  | Value.adt _ _, _, h => undefined_ne_ok h
  | Value.ref _, _, h => undefined_ne_ok h
  | Value.dead, _, h => undefined_ne_ok h

theorem indexBy_same (p : Pointer) (arr : Value) :
    ∀ (i : Value) (q : Pointer), indexBy p arr i = Outcome.ok q →
      q.frame = p.frame ∧ q.slot = p.slot
  | Value.int _ i, q, h => indexAt_same p i.val.toNat arr q h
  | Value.unit, _, h => undefined_ne_ok h
  | Value.bool _, _, h => undefined_ne_ok h
  | Value.adt _ _, _, h => undefined_ne_ok h
  | Value.array _, _, h => undefined_ne_ok h
  | Value.ref _, _, h => undefined_ne_ok h
  | Value.dead, _, h => undefined_ne_ok h

/-! What a statement must not mention. -/

/-- An index is not by the local `l`. -/
def elemFine (l : Local) : ProjElem → Bool
  | ProjElem.deref => true
  | ProjElem.field _ => true
  | ProjElem.downcast _ => true
  | ProjElem.index i => decide (¬ l = i)
  | ProjElem.constIndex _ => true

def projFine (l : Local) : List ProjElem → Bool
  | [] => true
  | e :: rest => elemFine l e && projFine l rest

/-- The place is not based at `l` and does not index by it. -/
def placeFine (l : Local) (p : Place) : Bool := decide (¬ l = p.base) && projFine l p.proj

def operandFine (l : Local) : Operand → Bool
  | Operand.copy p => placeFine l p
  | Operand.move p => placeFine l p
  | Operand.const _ => true

def operandsFine (l : Local) : List Operand → Bool
  | [] => true
  | o :: rest => operandFine l o && operandsFine l rest

def rvalueFine (l : Local) : Rvalue → Bool
  | Rvalue.use o => operandFine l o
  | Rvalue.binary _ a b => operandFine l a && operandFine l b
  | Rvalue.checked _ a b => operandFine l a && operandFine l b
  | Rvalue.withOverflow _ a b => operandFine l a && operandFine l b
  | Rvalue.unary _ o => operandFine l o
  | Rvalue.cast o _ => operandFine l o
  | Rvalue.aggregate _ fields => operandsFine l fields
  | Rvalue.replicate o _ => operandFine l o
  | Rvalue.discriminant _ p => placeFine l p
  | Rvalue.ref _ p => placeFine l p
  | Rvalue.len p => placeFine l p

/-- The statement does not mention `l`. -/
def stmtFine (l : Local) : Statement → Bool
  | Statement.assign p rv => placeFine l p && rvalueFine l rv
  | Statement.storageLive _ => true
  | Statement.storageDead _ => true
  | Statement.nop => true

/-! The slot is hidden: it is the running frame's, and no pointer in memory is into it. -/

structure Hides (B : FnId → Local → Prop) (g : List (Nat × FnId)) (f l : Nat) (st : State) :
    Prop where
  okAt : OkAt B g st
  top : st.top = f
  off : ∀ (q : Pointer), PtrOk B g q → Off f l q

theorem Hides.next {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st st' : State}
    (h : Hides B g f l st) (hs : OkAt B g st') (e : st'.ids = st.ids) : Hides B g f l st' where
  okAt := hs
  top := (State.top_of_ids st st' e).trans h.top
  off := h.off

/-! Places. -/

theorem projStep_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (_h : Hides B g f l st) (sl : Slot) (p : Pointer) (hp : Off f l p) :
    ∀ (e : ProjElem), elemFine l e = true → projStep (st.put f l sl) p e = projStep st p e
  | ProjElem.deref, _ => Outcome.bind_congr (State.read_put st f l sl p hp) (fun _ _ => rfl)
  | ProjElem.field _, _ => rfl
  | ProjElem.downcast _, _ => rfl
  | ProjElem.index i, hfine => by
    have hi : ¬ l = i := of_decide_eq_true hfine
    have offI : Off f l { frame := st.top, slot := i, path := [] } := Or.inr hi
    have readI : (st.put f l sl).read { frame := (st.put f l sl).top, slot := i, path := [] } =
        st.read { frame := st.top, slot := i, path := [] } :=
      (congrTo (fun t => (st.put f l sl).read { frame := t, slot := i, path := [] })
          (State.put_top st f l sl)).trans
        (State.read_put st f l sl { frame := st.top, slot := i, path := [] } offI)
    exact Outcome.bind_congr readI (fun _ _ =>
      Outcome.bind_congr (State.read_put st f l sl p hp) (fun _ _ => rfl))
  | ProjElem.constIndex _, _ =>
    Outcome.bind_congr (State.read_put st f l sl p hp) (fun _ _ => rfl)

theorem projStep_off {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (h : Hides B g f l st) (p : Pointer) (hp : Off f l p) :
    ∀ (e : ProjElem) (q : Pointer), projStep st p e = Outcome.ok q → Off f l q
  | ProjElem.deref, q, hq => h.off q (projStep_deref (And.right h.okAt) p q hq)
  | ProjElem.field i, q, hq => by
    have e : p.push (PathElem.field i) = q := Outcome.ok.inj hq
    rw [← e]
    exact hp
  | ProjElem.downcast variant, q, hq => by
    have e : p.push (PathElem.downcast variant) = q := Outcome.ok.inj hq
    rw [← e]
    exact hp
  | ProjElem.index i, q, hq => by
    have h' : (st.read { frame := st.top, slot := i, path := [] }).bind (fun x =>
        (st.read p).bind (fun arr => indexBy p arr x)) = Outcome.ok q := hq
    have ⟨x, _, r⟩ := Outcome.bind_eq_ok _ _ _ h'
    have r' : (st.read p).bind (fun arr => indexBy p arr x) = Outcome.ok q := r
    have ⟨arr, _, r2⟩ := Outcome.bind_eq_ok _ _ _ r'
    exact off_same hp (indexBy_same p arr x q r2)
  | ProjElem.constIndex n, q, hq => by
    have h' : (st.read p).bind (indexAt p n) = Outcome.ok q := hq
    have ⟨arr, _, r⟩ := Outcome.bind_eq_ok _ _ _ h'
    exact off_same hp (indexAt_same p n arr q r)

/-- A projection from a pointer off the slot is the same with the slot replaced, and ends
off the slot. -/
theorem evalProj_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (h : Hides B g f l st) (sl : Slot) :
    ∀ (proj : List ProjElem) (p : Pointer), Off f l p → projFine l proj = true →
      evalProj (st.put f l sl) p proj = evalProj st p proj ∧
        ∀ (q : Pointer), evalProj st p proj = Outcome.ok q → Off f l q
  | [], p, hp, _ =>
    And.intro rfl (fun q hq => by
      have e : p = q := Outcome.ok.inj hq
      rw [← e]
      exact hp)
  | e :: rest, p, hp, hfine =>
    And.intro
      (Outcome.bind_congr (projStep_put h sl p hp e (and_true_left hfine)) (fun p1 h1 =>
        And.left
          (evalProj_put h sl rest p1 (projStep_off h p hp e p1 h1) (and_true_right hfine))))
      (fun q hq => by
        have h' : (projStep st p e).bind (fun x => evalProj st x rest) = Outcome.ok q := hq
        have ⟨p1, h1, r⟩ := Outcome.bind_eq_ok _ _ _ h'
        exact And.right
          (evalProj_put h sl rest p1 (projStep_off h p hp e p1 h1) (and_true_right hfine)) q r)

theorem evalPlace_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (h : Hides B g f l st) (sl : Slot) (place : Place) (hfine : placeFine l place = true) :
    evalPlace (st.put f l sl) place = evalPlace st place ∧
      ∀ (q : Pointer), evalPlace st place = Outcome.ok q → Off f l q := by
  have hbase : ¬ l = place.base := of_decide_eq_true (and_true_left hfine)
  have start : Off f l { frame := st.top, slot := place.base, path := [] } := Or.inr hbase
  have both := evalProj_put h sl place.proj { frame := st.top, slot := place.base, path := [] }
    start (and_true_right hfine)
  have first : evalPlace (st.put f l sl) place =
      evalProj (st.put f l sl) { frame := st.top, slot := place.base, path := [] } place.proj :=
    congrTo
      (fun t => evalProj (st.put f l sl) { frame := t, slot := place.base, path := [] } place.proj)
      (State.put_top st f l sl)
  exact And.intro (first.trans (And.left both)) (And.right both)

/-! Moves. -/

theorem stateOr_map (g : State → State) (st : State) :
    ∀ (x : Outcome State), stateOr (g st) (Outcome.map g x) = g (stateOr st x)
  | Outcome.ok _ => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

theorem State.moveOut_put (st : State) (f l : Nat) (sl : Slot) :
    ∀ (q : Pointer), Off f l q → (st.put f l sl).moveOut q = (st.moveOut q).put f l sl
  | { frame := qf, slot := qs, path := [] }, hq =>
    State.setSlot_comm st { frame := f, slot := l, path := [] }
      { frame := qf, slot := qs, path := [] } sl Slot.moved hq
  | { frame := qf, slot := qs, path := e :: rest }, hq =>
    (congrTo (stateOr (st.put f l sl))
        (State.write_setSlot_other st { frame := f, slot := l, path := [] }
          { frame := qf, slot := qs, path := e :: rest } sl Value.dead hq)).trans
      (stateOr_map (fun s => State.put s f l sl) st
        (st.write { frame := qf, slot := qs, path := e :: rest } Value.dead))

/-! Operands, rvalues, statements. -/

theorem evalOperand_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : Hides B g f l st) (sl : Slot) :
    ∀ (o : Operand), operandFine l o = true →
      evalOperand (st.put f l sl) o =
        Outcome.map (keepState (fun s => State.put s f l sl)) (evalOperand st o)
  | Operand.copy p, hfine =>
    Outcome.bind_commutes (And.left (evalPlace_put h sl p hfine)) (fun ptr hptr =>
      Outcome.bind_commutes
        (State.read_put st f l sl ptr (And.right (evalPlace_put h sl p hfine) ptr hptr))
        (fun _ _ => rfl))
  | Operand.move p, hfine =>
    Outcome.bind_commutes (And.left (evalPlace_put h sl p hfine)) (fun ptr hptr =>
      Outcome.bind_commutes
        (State.read_put st f l sl ptr (And.right (evalPlace_put h sl p hfine) ptr hptr))
        (fun v _ =>
          congrTo (fun s => Outcome.ok (v, s))
            (State.moveOut_put st f l sl ptr
              (And.right (evalPlace_put h sl p hfine) ptr hptr))))
  | Operand.const _, _ => rfl

/-- The state an operand leaves hides the slot as the state before did. -/
theorem Hides.operand {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (h : Hides B g f l st) (o : Operand) (a : Value × State)
    (ha : evalOperand st o = Outcome.ok a) : Hides B g f l a.2 :=
  h.next (And.right (evalOperand_okAt st h.okAt o a ha)) (evalOperand_ids st o a ha)

theorem evalOperands_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (sl : Slot) :
    ∀ (os : List Operand) (st : State), Hides B g f l st → operandsFine l os = true →
      evalOperands (st.put f l sl) os =
        Outcome.map (keepState (fun s => State.put s f l sl)) (evalOperands st os)
  | [], _, _, _ => rfl
  | o :: rest, _, h, hfine =>
    Outcome.bind_commutes_map (evalOperand_put h sl o (and_true_left hfine)) (fun a ha =>
      Outcome.bind_commutes_map
        (evalOperands_put sl rest a.2 (h.operand o a ha) (and_true_right hfine))
        (fun _ _ => rfl))

theorem evalTwo_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (h : Hides B g f l st) (sl : Slot) (a b : Operand) (op : Value → Value → Outcome Value)
    (ha : operandFine l a = true) (hb : operandFine l b = true) :
    evalTwo (st.put f l sl) a b op =
      Outcome.map (keepState (fun s => State.put s f l sl)) (evalTwo st a b op) :=
  Outcome.bind_commutes_map (evalOperand_put h sl a ha) (fun x hx =>
    Outcome.bind_commutes_map (evalOperand_put (h.operand a x hx) sl b hb) (fun _ _ =>
      Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalOne_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (h : Hides B g f l st) (sl : Slot) (o : Operand) (op : Value → Outcome Value)
    (ho : operandFine l o = true) :
    evalOne (st.put f l sl) o op =
      Outcome.map (keepState (fun s => State.put s f l sl)) (evalOne st o op) :=
  Outcome.bind_commutes_map (evalOperand_put h sl o ho) (fun _ _ =>
    Outcome.bind_commutes rfl (fun _ _ => rfl))

theorem evalAt_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat} {st : State}
    (h : Hides B g f l st) (sl : Slot) (p : Place) (op : Value → Outcome Value)
    (hp : placeFine l p = true) :
    evalAt (st.put f l sl) p op =
      Outcome.map (keepState (fun s => State.put s f l sl)) (evalAt st p op) :=
  Outcome.bind_commutes (And.left (evalPlace_put h sl p hp)) (fun ptr hptr =>
    Outcome.bind_commutes
      (State.read_put st f l sl ptr (And.right (evalPlace_put h sl p hp) ptr hptr))
      (fun _ _ => Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalRvalue_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) {st : State} (h : Hides B g f l st) (sl : Slot) :
    ∀ (rv : Rvalue), rvalueFine l rv = true →
      evalRvalue adts (st.put f l sl) rv =
        Outcome.map (keepState (fun s => State.put s f l sl)) (evalRvalue adts st rv)
  | Rvalue.use o, hfine => evalOperand_put h sl o hfine
  | Rvalue.binary op a b, hfine =>
    evalTwo_put h sl a b (binaryOp false op) (and_true_left hfine) (and_true_right hfine)
  | Rvalue.checked op a b, hfine =>
    evalTwo_put h sl a b (binaryOp true op) (and_true_left hfine) (and_true_right hfine)
  | Rvalue.withOverflow op a b, hfine =>
    evalTwo_put h sl a b (overflowingOp op) (and_true_left hfine) (and_true_right hfine)
  | Rvalue.unary op o, hfine => evalOne_put h sl o (unaryOp op) hfine
  | Rvalue.cast o target, hfine => evalOne_put h sl o (castTo target) hfine
  | Rvalue.aggregate _ fields, hfine =>
    Outcome.bind_commutes_map (evalOperands_put sl fields st h hfine) (fun _ _ => rfl)
  | Rvalue.replicate o count, hfine =>
    evalOne_put h sl o (fun v => Outcome.ok (Value.array (List.replicate count v))) hfine
  | Rvalue.discriminant adt p, hfine => evalAt_put h sl p (discriminantOf adts adt) hfine
  | Rvalue.ref _ p, hfine =>
    Outcome.bind_commutes (And.left (evalPlace_put h sl p hfine)) (fun _ _ => rfl)
  | Rvalue.len p, hfine => evalAt_put h sl p lenOf hfine

/-- A statement that does not mention `l` does the same whatever the hidden slot holds: from
the state with the slot replaced it gives its state with the slot replaced. -/
theorem execStmt_put {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) {st : State} (h : Hides B g f l st) (sl : Slot) (fn : FnId)
    (hfn : st.topFn = fn) :
    ∀ (s : Statement), StmtFine B fn s → stmtFine l s = true →
      execStmt adts (st.put f l sl) s =
        Outcome.map (fun x => State.put x f l sl) (execStmt adts st s)
  | Statement.assign p rv, hB, hfine =>
    Outcome.bind_commutes_map (evalRvalue_put adts h sl rv (and_true_right hfine)) (fun r hr =>
      have hr2 : Hides B g f l r.2 :=
        h.next (And.right (evalRvalue_okAt adts st h.okAt fn hfn rv hB r hr))
          (evalRvalue_ids adts st rv r hr)
      Outcome.bind_commutes (And.left (evalPlace_put hr2 sl p (and_true_left hfine)))
        (fun ptr hptr =>
          State.write_setSlot_other r.2 { frame := f, slot := l, path := [] } ptr sl r.1
            (And.right (evalPlace_put hr2 sl p (and_true_left hfine)) ptr hptr)))
  | Statement.storageLive _, _, _ => rfl
  | Statement.storageDead _, _, _ => rfl
  | Statement.nop, _, _ => rfl

end Blaze.Mir

import Blaze.Mir.Opt.SlotExec

/-!
A statement does the same whatever a slot of another frame holds: global dead-store
elimination (ledger V11), piece (a).

`Opt/SlotExec.lean` is about a slot of the running frame, which a statement must not mention.
A slot of a frame that is not running needs no such condition: every place of a statement is
in the running frame, and no pointer in memory is into the slot (`HidesFar`). So
`execStmt_putFar`: any statement from `st.put f l sl` gives what it gives from `st`, with
the slot replaced the same way. The proofs are those of `Opt/SlotExec.lean`, with "another
frame" where it has "another local".
-/

namespace Blaze.Mir

/-- The slot is of a frame that is not running, and no pointer in memory is into it. -/
structure HidesFar (B : FnId → Local → Prop) (g : List (Nat × FnId)) (f l : Nat) (st : State) :
    Prop where
  okAt : OkAt B g st
  top : ¬ f = st.top
  off : ∀ (q : Pointer), PtrOk B g q → Off f l q

theorem HidesFar.next {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st st' : State} (h : HidesFar B g f l st) (hs : OkAt B g st') (e : st'.ids = st.ids) :
    HidesFar B g f l st' where
  okAt := hs
  top := fun e' => h.top (e'.trans (State.top_of_ids st st' e))
  off := h.off

theorem projStep_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (sl : Slot) (p : Pointer) (hp : Off f l p) :
    ∀ (e : ProjElem), projStep (st.put f l sl) p e = projStep st p e
  | ProjElem.deref => Outcome.bind_congr (State.read_put st f l sl p hp) (fun _ _ => rfl)
  | ProjElem.field _ => rfl
  | ProjElem.downcast _ => rfl
  | ProjElem.index i => by
    have offI : Off f l { frame := st.top, slot := i, path := [] } := Or.inl h.top
    have readI : (st.put f l sl).read { frame := (st.put f l sl).top, slot := i, path := [] } =
        st.read { frame := st.top, slot := i, path := [] } :=
      (congrTo (fun t => (st.put f l sl).read { frame := t, slot := i, path := [] })
          (State.put_top st f l sl)).trans
        (State.read_put st f l sl { frame := st.top, slot := i, path := [] } offI)
    exact Outcome.bind_congr readI (fun _ _ =>
      Outcome.bind_congr (State.read_put st f l sl p hp) (fun _ _ => rfl))
  | ProjElem.constIndex _ =>
    Outcome.bind_congr (State.read_put st f l sl p hp) (fun _ _ => rfl)

theorem projStep_offFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (p : Pointer) (hp : Off f l p) :
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

theorem evalProj_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (sl : Slot) :
    ∀ (proj : List ProjElem) (p : Pointer), Off f l p →
      evalProj (st.put f l sl) p proj = evalProj st p proj ∧
        ∀ (q : Pointer), evalProj st p proj = Outcome.ok q → Off f l q
  | [], p, hp =>
    And.intro rfl (fun q hq => by
      have e : p = q := Outcome.ok.inj hq
      rw [← e]
      exact hp)
  | e :: rest, p, hp =>
    And.intro
      (Outcome.bind_congr (projStep_putFar h sl p hp e) (fun p1 h1 =>
        And.left (evalProj_putFar h sl rest p1 (projStep_offFar h p hp e p1 h1))))
      (fun q hq => by
        have h' : (projStep st p e).bind (fun x => evalProj st x rest) = Outcome.ok q := hq
        have ⟨p1, h1, r⟩ := Outcome.bind_eq_ok _ _ _ h'
        exact And.right (evalProj_putFar h sl rest p1 (projStep_offFar h p hp e p1 h1)) q r)

theorem evalPlace_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (sl : Slot) (place : Place) :
    evalPlace (st.put f l sl) place = evalPlace st place ∧
      ∀ (q : Pointer), evalPlace st place = Outcome.ok q → Off f l q := by
  have start : Off f l { frame := st.top, slot := place.base, path := [] } := Or.inl h.top
  have both := evalProj_putFar h sl place.proj
    { frame := st.top, slot := place.base, path := [] } start
  have first : evalPlace (st.put f l sl) place =
      evalProj (st.put f l sl) { frame := st.top, slot := place.base, path := [] } place.proj :=
    congrTo
      (fun t => evalProj (st.put f l sl) { frame := t, slot := place.base, path := [] } place.proj)
      (State.put_top st f l sl)
  exact And.intro (first.trans (And.left both)) (And.right both)

theorem evalOperand_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (sl : Slot) :
    ∀ (o : Operand),
      evalOperand (st.put f l sl) o =
        Outcome.map (keepState (fun s => State.put s f l sl)) (evalOperand st o)
  | Operand.copy p =>
    Outcome.bind_commutes (And.left (evalPlace_putFar h sl p)) (fun ptr hptr =>
      Outcome.bind_commutes
        (State.read_put st f l sl ptr (And.right (evalPlace_putFar h sl p) ptr hptr))
        (fun _ _ => rfl))
  | Operand.move p =>
    Outcome.bind_commutes (And.left (evalPlace_putFar h sl p)) (fun ptr hptr =>
      Outcome.bind_commutes
        (State.read_put st f l sl ptr (And.right (evalPlace_putFar h sl p) ptr hptr))
        (fun v _ =>
          congrTo (fun s => Outcome.ok (v, s))
            (State.moveOut_put st f l sl ptr (And.right (evalPlace_putFar h sl p) ptr hptr))))
  | Operand.const _ => rfl

theorem HidesFar.operand {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (o : Operand) (a : Value × State)
    (ha : evalOperand st o = Outcome.ok a) : HidesFar B g f l a.2 :=
  h.next (And.right (evalOperand_okAt st h.okAt o a ha)) (evalOperand_ids st o a ha)

theorem evalOperands_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (sl : Slot) :
    ∀ (os : List Operand) (st : State), HidesFar B g f l st →
      evalOperands (st.put f l sl) os =
        Outcome.map (keepState (fun s => State.put s f l sl)) (evalOperands st os)
  | [], _, _ => rfl
  | o :: rest, st, h =>
    Outcome.bind_commutes_map (evalOperand_putFar h sl o) (fun a ha =>
      Outcome.bind_commutes_map (evalOperands_putFar sl rest a.2 (h.operand o a ha))
        (fun _ _ => rfl))

theorem evalTwo_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (sl : Slot) (a b : Operand)
    (op : Value → Value → Outcome Value) :
    evalTwo (st.put f l sl) a b op =
      Outcome.map (keepState (fun s => State.put s f l sl)) (evalTwo st a b op) :=
  Outcome.bind_commutes_map (evalOperand_putFar h sl a) (fun x hx =>
    Outcome.bind_commutes_map (evalOperand_putFar (h.operand a x hx) sl b) (fun _ _ =>
      Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalOne_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (sl : Slot) (o : Operand)
    (op : Value → Outcome Value) :
    evalOne (st.put f l sl) o op =
      Outcome.map (keepState (fun s => State.put s f l sl)) (evalOne st o op) :=
  Outcome.bind_commutes_map (evalOperand_putFar h sl o) (fun _ _ =>
    Outcome.bind_commutes rfl (fun _ _ => rfl))

theorem evalAt_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    {st : State} (h : HidesFar B g f l st) (sl : Slot) (p : Place)
    (op : Value → Outcome Value) :
    evalAt (st.put f l sl) p op =
      Outcome.map (keepState (fun s => State.put s f l sl)) (evalAt st p op) :=
  Outcome.bind_commutes (And.left (evalPlace_putFar h sl p)) (fun ptr hptr =>
    Outcome.bind_commutes
      (State.read_put st f l sl ptr (And.right (evalPlace_putFar h sl p) ptr hptr))
      (fun _ _ => Outcome.bind_commutes rfl (fun _ _ => rfl)))

theorem evalRvalue_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) {st : State} (h : HidesFar B g f l st) (sl : Slot) :
    ∀ (rv : Rvalue),
      evalRvalue adts (st.put f l sl) rv =
        Outcome.map (keepState (fun s => State.put s f l sl)) (evalRvalue adts st rv)
  | Rvalue.use o => evalOperand_putFar h sl o
  | Rvalue.binary op a b => evalTwo_putFar h sl a b (binaryOp false op)
  | Rvalue.checked op a b => evalTwo_putFar h sl a b (binaryOp true op)
  | Rvalue.withOverflow op a b => evalTwo_putFar h sl a b (overflowingOp op)
  | Rvalue.unary op o => evalOne_putFar h sl o (unaryOp op)
  | Rvalue.cast o target => evalOne_putFar h sl o (castTo target)
  | Rvalue.aggregate _ fields =>
    Outcome.bind_commutes_map (evalOperands_putFar sl fields st h) (fun _ _ => rfl)
  | Rvalue.replicate o count =>
    evalOne_putFar h sl o (fun v => Outcome.ok (Value.array (List.replicate count v)))
  | Rvalue.discriminant adt p => evalAt_putFar h sl p (discriminantOf adts adt)
  | Rvalue.ref _ p => Outcome.bind_commutes (And.left (evalPlace_putFar h sl p)) (fun _ _ => rfl)
  | Rvalue.len p => evalAt_putFar h sl p lenOf

/-- A statement does the same whatever a hidden slot of another frame holds. -/
theorem execStmt_putFar {B : FnId → Local → Prop} {g : List (Nat × FnId)} {f l : Nat}
    (adts : List AdtDecl) {st : State} (h : HidesFar B g f l st) (sl : Slot) (fn : FnId)
    (hfn : st.topFn = fn) :
    ∀ (s : Statement), StmtFine B fn s →
      execStmt adts (st.put f l sl) s =
        Outcome.map (fun x => State.put x f l sl) (execStmt adts st s)
  | Statement.assign p rv, hB =>
    Outcome.bind_commutes_map (evalRvalue_putFar adts h sl rv) (fun r hr =>
      have hr2 : HidesFar B g f l r.2 :=
        h.next (And.right (evalRvalue_okAt adts st h.okAt fn hfn rv hB r hr))
          (evalRvalue_ids adts st rv r hr)
      Outcome.bind_commutes (And.left (evalPlace_putFar hr2 sl p)) (fun ptr hptr =>
        State.write_setSlot_other r.2 { frame := f, slot := l, path := [] } ptr sl r.1
          (And.right (evalPlace_putFar hr2 sl p) ptr hptr)))
  | Statement.storageLive _, _ => rfl
  | Statement.storageDead _, _ => rfl
  | Statement.nop, _ => rfl

end Blaze.Mir

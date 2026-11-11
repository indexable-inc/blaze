import Blaze.Mir.Opt.Puts
import Blaze.Mir.Opt.Skel
import Blaze.Mir.Opt.Merge

/-!
Operands, places and control through a list of replaced slots: global dead-store elimination
(ledger V11), piece (d), first and second parts.

`Opt/Puts.lean` carries a statement through `puts st reps`. A terminator evaluates operands
and a place, and then jumps, calls or returns. Here, first: an operand, a list of operands
and a place from `puts st reps` give what they give from `st`, with the same slots replaced
(`evalOperand_puts`, `evalOperands_puts`, `evalPlace_puts`), when each entry is of another
frame or of a local the thing evaluated does not mention (`RepCan`); and a call's
destination, which may be the whole local of an entry (`evalDest_puts`).
-/

namespace Blaze.Mir

/-- The entry can be carried through something the frame `top` evaluates: its slot holds a
value the invariant allows, no allowed pointer is into it, and it is of another frame or of
a local `fine` says the thing does not mention. -/
def RepCan (B : FnId → Local → Prop) (g : List (Nat × FnId)) (top : Nat) (fine : Local → Bool)
    (r : Rep) : Prop :=
  SlotOk (PtrOk B g) r.2.2 ∧ (∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q) ∧
    ((top = r.1 ∧ fine r.2.1 = true) ∨ ¬ r.1 = top)

theorem Outcome.map_keep_same {α : Type} :
    ∀ (x : Outcome (α × State)), Outcome.map (keepState (fun s => s)) x = x
  | Outcome.ok (_, _) => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

/-! Reads off the replaced slots. -/

/-- A slot no entry is of is the slot it was. -/
theorem slot_puts :
    ∀ (reps : List Rep) (st : State) (q : Pointer),
      (∀ (r : Rep), r ∈ reps → Off r.1 r.2.1 q) → (puts st reps).slot q = st.slot q
  | [], _, _, _ => rfl
  | r :: rest, st, q, h =>
    (slot_puts rest (st.put r.1 r.2.1 r.2.2) q
        (fun r' m => h r' (List.mem_cons.mpr (Or.inr m)))).trans
      (State.slot_setSlot_other st { frame := r.1, slot := r.2.1, path := [] } q r.2.2
        (h r (List.mem_cons.mpr (Or.inl rfl))))

/-- A read at a pointer off every entry is the read it was. -/
theorem read_puts :
    ∀ (reps : List Rep) (st : State) (q : Pointer),
      (∀ (r : Rep), r ∈ reps → Off r.1 r.2.1 q) → (puts st reps).read q = st.read q
  | [], _, _, _ => rfl
  | r :: rest, st, q, h =>
    (read_puts rest (st.put r.1 r.2.1 r.2.2) q
        (fun r' m => h r' (List.mem_cons.mpr (Or.inr m)))).trans
      (State.read_put st r.1 r.2.1 r.2.2 q (h r (List.mem_cons.mpr (Or.inl rfl))))

/-! Operands. -/

/-- One replaced slot through an operand. -/
theorem evalOperand_rep {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (hs : OkAt B g st) (o : Operand) (r : Rep)
    (hr : RepCan B g st.top (fun l => operandFine l o) r) :
    evalOperand (st.put r.1 r.2.1 r.2.2) o =
      Outcome.map (keepState (fun s => State.put s r.1 r.2.1 r.2.2)) (evalOperand st o) := by
  have off : ∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q := And.left (And.right hr)
  cases And.right (And.right hr) with
  | inl here =>
    have hides : Hides B g r.1 r.2.1 st := { okAt := hs, top := And.left here, off := off }
    have fine : operandFine r.2.1 o = true := And.right here
    exact evalOperand_put hides r.2.2 o fine
  | inr far =>
    have hides : HidesFar B g r.1 r.2.1 st := { okAt := hs, top := far, off := off }
    exact evalOperand_putFar hides r.2.2 o

/-- An operand through a list of replaced slots: its value, and its state with the same
slots replaced. -/
theorem evalOperand_puts {B : FnId → Local → Prop} {g : List (Nat × FnId)} (o : Operand) :
    ∀ (reps : List Rep) (st : State), OkAt B g st →
      (∀ (r : Rep), r ∈ reps → RepCan B g st.top (fun l => operandFine l o) r) →
        evalOperand (puts st reps) o =
          Outcome.map (keepState (fun s => puts s reps)) (evalOperand st o)
  | [], st, _, _ => (Outcome.map_keep_same (evalOperand st o)).symm
  | r :: rest, st, hs, hreps => by
    have hr : RepCan B g st.top (fun l => operandFine l o) r :=
      hreps r (List.mem_cons.mpr (Or.inl rfl))
    have single : evalOperand (st.put r.1 r.2.1 r.2.2) o =
        Outcome.map (keepState (fun s => State.put s r.1 r.2.1 r.2.2)) (evalOperand st o) :=
      evalOperand_rep hs o r hr
    have hs1 : OkAt B g (st.put r.1 r.2.1 r.2.2) :=
      State.put_okAt hs r.1 r.2.1 r.2.2 (And.left hr)
    have hrest : ∀ (r' : Rep), r' ∈ rest →
        RepCan B g (st.put r.1 r.2.1 r.2.2).top (fun l => operandFine l o) r' := by
      intro r' m
      rw [State.put_top]
      exact hreps r' (List.mem_cons.mpr (Or.inr m))
    have inner : evalOperand (puts (st.put r.1 r.2.1 r.2.2) rest) o =
        Outcome.map (keepState (fun s => puts s rest))
          (evalOperand (st.put r.1 r.2.1 r.2.2) o) :=
      evalOperand_puts o rest (st.put r.1 r.2.1 r.2.2) hs1 hrest
    have both : Outcome.map (keepState (fun s => puts s rest))
          (evalOperand (st.put r.1 r.2.1 r.2.2) o) =
        Outcome.map (keepState (fun s => puts (State.put s r.1 r.2.1 r.2.2) rest))
          (evalOperand st o) :=
      (congrTo (Outcome.map (keepState (fun s => puts s rest))) single).trans
        (Outcome.map_map
          (keepState (fun s => State.put s r.1 r.2.1 r.2.2) : Value × State → Value × State)
          (keepState (fun s => puts s rest) : Value × State → Value × State)
          (evalOperand st o))
    exact inner.trans both

/-- One replaced slot through a list of operands. -/
theorem evalOperands_rep {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (hs : OkAt B g st) (os : List Operand) (r : Rep)
    (hr : RepCan B g st.top (fun l => operandsFine l os) r) :
    evalOperands (st.put r.1 r.2.1 r.2.2) os =
      Outcome.map (keepState (fun s => State.put s r.1 r.2.1 r.2.2)) (evalOperands st os) := by
  have off : ∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q := And.left (And.right hr)
  cases And.right (And.right hr) with
  | inl here =>
    have hides : Hides B g r.1 r.2.1 st := { okAt := hs, top := And.left here, off := off }
    have fine : operandsFine r.2.1 os = true := And.right here
    exact evalOperands_put r.2.2 os st hides fine
  | inr far =>
    have hides : HidesFar B g r.1 r.2.1 st := { okAt := hs, top := far, off := off }
    exact evalOperands_putFar r.2.2 os st hides

/-- A list of operands through a list of replaced slots. -/
theorem evalOperands_puts {B : FnId → Local → Prop} {g : List (Nat × FnId)}
    (os : List Operand) :
    ∀ (reps : List Rep) (st : State), OkAt B g st →
      (∀ (r : Rep), r ∈ reps → RepCan B g st.top (fun l => operandsFine l os) r) →
        evalOperands (puts st reps) os =
          Outcome.map (keepState (fun s => puts s reps)) (evalOperands st os)
  | [], st, _, _ => (Outcome.map_keep_same (evalOperands st os)).symm
  | r :: rest, st, hs, hreps => by
    have hr : RepCan B g st.top (fun l => operandsFine l os) r :=
      hreps r (List.mem_cons.mpr (Or.inl rfl))
    have single : evalOperands (st.put r.1 r.2.1 r.2.2) os =
        Outcome.map (keepState (fun s => State.put s r.1 r.2.1 r.2.2)) (evalOperands st os) :=
      evalOperands_rep hs os r hr
    have hs1 : OkAt B g (st.put r.1 r.2.1 r.2.2) :=
      State.put_okAt hs r.1 r.2.1 r.2.2 (And.left hr)
    have hrest : ∀ (r' : Rep), r' ∈ rest →
        RepCan B g (st.put r.1 r.2.1 r.2.2).top (fun l => operandsFine l os) r' := by
      intro r' m
      rw [State.put_top]
      exact hreps r' (List.mem_cons.mpr (Or.inr m))
    have inner : evalOperands (puts (st.put r.1 r.2.1 r.2.2) rest) os =
        Outcome.map (keepState (fun s => puts s rest))
          (evalOperands (st.put r.1 r.2.1 r.2.2) os) :=
      evalOperands_puts os rest (st.put r.1 r.2.1 r.2.2) hs1 hrest
    have both : Outcome.map (keepState (fun s => puts s rest))
          (evalOperands (st.put r.1 r.2.1 r.2.2) os) =
        Outcome.map (keepState (fun s => puts (State.put s r.1 r.2.1 r.2.2) rest))
          (evalOperands st os) :=
      (congrTo (Outcome.map (keepState (fun s => puts s rest))) single).trans
        (Outcome.map_map
          (keepState (fun s => State.put s r.1 r.2.1 r.2.2) :
            List Value × State → List Value × State)
          (keepState (fun s => puts s rest) : List Value × State → List Value × State)
          (evalOperands st os))
    exact inner.trans both

/-! Places. -/

/-- One replaced slot through a place: the place is where it was, off the slot. -/
theorem evalPlace_rep {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (hs : OkAt B g st) (p : Place) (r : Rep)
    (hr : RepCan B g st.top (fun l => placeFine l p) r) :
    evalPlace (st.put r.1 r.2.1 r.2.2) p = evalPlace st p ∧
      ∀ (q : Pointer), evalPlace st p = Outcome.ok q → Off r.1 r.2.1 q := by
  have off : ∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q := And.left (And.right hr)
  cases And.right (And.right hr) with
  | inl here =>
    have hides : Hides B g r.1 r.2.1 st := { okAt := hs, top := And.left here, off := off }
    have fine : placeFine r.2.1 p = true := And.right here
    exact evalPlace_put hides r.2.2 p fine
  | inr far =>
    have hides : HidesFar B g r.1 r.2.1 st := { okAt := hs, top := far, off := off }
    exact evalPlace_putFar hides r.2.2 p

/-- A place through a list of replaced slots: it is where it was, off every entry. -/
theorem evalPlace_puts {B : FnId → Local → Prop} {g : List (Nat × FnId)} (p : Place) :
    ∀ (reps : List Rep) (st : State), OkAt B g st →
      (∀ (r : Rep), r ∈ reps → RepCan B g st.top (fun l => placeFine l p) r) →
        evalPlace (puts st reps) p = evalPlace st p ∧
          ∀ (q : Pointer), evalPlace st p = Outcome.ok q →
            ∀ (r : Rep), r ∈ reps → Off r.1 r.2.1 q
  | [], _, _, _ => And.intro rfl (fun _ _ _ m => mem_nil_elim m)
  | r :: rest, st, hs, hreps => by
    have hr : RepCan B g st.top (fun l => placeFine l p) r :=
      hreps r (List.mem_cons.mpr (Or.inl rfl))
    have single : evalPlace (st.put r.1 r.2.1 r.2.2) p = evalPlace st p ∧
        ∀ (q : Pointer), evalPlace st p = Outcome.ok q → Off r.1 r.2.1 q :=
      evalPlace_rep hs p r hr
    have hs1 : OkAt B g (st.put r.1 r.2.1 r.2.2) :=
      State.put_okAt hs r.1 r.2.1 r.2.2 (And.left hr)
    have hrest : ∀ (r' : Rep), r' ∈ rest →
        RepCan B g (st.put r.1 r.2.1 r.2.2).top (fun l => placeFine l p) r' := by
      intro r' m
      rw [State.put_top]
      exact hreps r' (List.mem_cons.mpr (Or.inr m))
    have inner : evalPlace (puts (st.put r.1 r.2.1 r.2.2) rest) p =
          evalPlace (st.put r.1 r.2.1 r.2.2) p ∧
        ∀ (q : Pointer), evalPlace (st.put r.1 r.2.1 r.2.2) p = Outcome.ok q →
          ∀ (r' : Rep), r' ∈ rest → Off r'.1 r'.2.1 q :=
      evalPlace_puts p rest (st.put r.1 r.2.1 r.2.2) hs1 hrest
    have eq : evalPlace (puts (st.put r.1 r.2.1 r.2.2) rest) p = evalPlace st p :=
      (And.left inner).trans (And.left single)
    have offs : ∀ (q : Pointer), evalPlace st p = Outcome.ok q →
        ∀ (r' : Rep), r' ∈ r :: rest → Off r'.1 r'.2.1 q := by
      intro q hq r' m
      cases List.mem_cons.mp m with
      | inl here =>
        rw [here]
        exact And.right single q hq
      | inr there => exact And.right inner q ((And.left single).trans hq) r' there
    exact And.intro eq offs

/-- A place with no projection is the running frame's local. -/
theorem evalPlace_local (st : State) (l : Local) :
    evalPlace st (Place.mk l []) = Outcome.ok { frame := st.top, slot := l, path := [] } := rfl

theorem place_cons_ne {base l : Local} {e : ProjElem} {rest : List ProjElem} {C : Prop}
    (h : Place.mk base (e :: rest) = Place.mk l []) : C := by
  cases h

/-- The entry lets the frame `top` evaluate the destination `p`: as `RepCan`, but an entry
of the running frame may also be the local `p` is, whole. -/
def RepDest (B : FnId → Local → Prop) (g : List (Nat × FnId)) (top : Nat) (p : Place)
    (r : Rep) : Prop :=
  SlotOk (PtrOk B g) r.2.2 ∧ (∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q) ∧
    ((top = r.1 ∧ (placeFine r.2.1 p = true ∨ p = Place.mk r.2.1 [])) ∨ ¬ r.1 = top)

/-- A destination through a list of replaced slots: it is where it was, and that is off each
entry or the entry's whole slot. -/
theorem evalDest_puts {B : FnId → Local → Prop} {g : List (Nat × FnId)} (reps : List Rep)
    (st : State) (hs : OkAt B g st) :
    ∀ (p : Place), (∀ (r : Rep), r ∈ reps → RepDest B g st.top p r) →
      evalPlace (puts st reps) p = evalPlace st p ∧
        ∀ (q : Pointer), evalPlace st p = Outcome.ok q → ∀ (r : Rep), r ∈ reps →
          q = { frame := r.1, slot := r.2.1, path := [] } ∨ Off r.1 r.2.1 q
  | { base := base, proj := [] }, hreps => by
    have eq : evalPlace (puts st reps) (Place.mk base []) = evalPlace st (Place.mk base []) :=
      congrTo (fun t => Outcome.ok (Pointer.mk t base [])) (puts_top reps st)
    have where_ : ∀ (q : Pointer), evalPlace st (Place.mk base []) = Outcome.ok q →
        ∀ (r : Rep), r ∈ reps →
          q = { frame := r.1, slot := r.2.1, path := [] } ∨ Off r.1 r.2.1 q := by
      intro q hq r m
      have e : Pointer.mk st.top base [] = q := Outcome.ok.inj hq
      have hr : RepDest B g st.top (Place.mk base []) r := hreps r m
      rw [← e]
      cases And.right (And.right hr) with
      | inl here =>
        cases And.right here with
        | inl fine =>
          have hb : ¬ r.2.1 = base := of_decide_eq_true (and_true_left fine)
          exact Or.inr (Or.inr hb)
        | inr whole =>
          have eb : base = r.2.1 := congrTo Place.base whole
          have e1 : Pointer.mk st.top base [] = Pointer.mk r.1 base [] :=
            congrTo (fun t => Pointer.mk t base []) (And.left here)
          have e2 : Pointer.mk r.1 base [] = Pointer.mk r.1 r.2.1 [] :=
            congrTo (fun b => Pointer.mk r.1 b []) eb
          exact Or.inl (e1.trans e2)
      | inr far => exact Or.inr (Or.inl far)
    exact And.intro eq where_
  | { base := base, proj := e :: rest }, hreps => by
    have conv : ∀ (r : Rep), r ∈ reps →
        RepCan B g st.top (fun l => placeFine l (Place.mk base (e :: rest))) r := by
      intro r m
      have hr : RepDest B g st.top (Place.mk base (e :: rest)) r := hreps r m
      refine And.intro (And.left hr) (And.intro (And.left (And.right hr)) ?_)
      cases And.right (And.right hr) with
      | inl here =>
        cases And.right here with
        | inl fine => exact Or.inl (And.intro (And.left here) fine)
        | inr whole => exact place_cons_ne whole
      | inr far => exact Or.inr far
    have both := evalPlace_puts (Place.mk base (e :: rest)) reps st hs conv
    exact And.intro (And.left both) (fun q hq r m => Or.inr (And.right both q hq r m))

end Blaze.Mir

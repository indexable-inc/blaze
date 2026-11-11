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

Second, control. The two states have the same frames but for what their locals hold
(`puts_skel`). A jump keeps the list (`jump_puts`, and `switchOn_puts`, `assertOn_puts`). A
write at a pointer that is an entry's whole slot forgets the entry, and commutes with the
others (`write_puts`, `dropAt`). A call's new frame has an id no entry has, so entering
commutes with the list (`enter_puts`); an intrinsic reads only through its arguments
(`Intrinsic.eval_same`); so `callOn_puts`. A return pops the running frame, after which its
entries replace nothing, then writes the destination in the caller and jumps (`ret_puts`).
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

/-! Second part: control. The frames are those of the state. -/

theorem State.put_skel (st : State) (f l : Nat) (sl : Slot) : (st.put f l sl).skel = st.skel :=
  State.setSlot_skel st { frame := f, slot := l, path := [] } sl

theorem State.put_next (st : State) (f l : Nat) (sl : Slot) : (st.put f l sl).next = st.next :=
  State.setSlot_next st { frame := f, slot := l, path := [] } sl

theorem puts_skel : ∀ (reps : List Rep) (st : State), (puts st reps).skel = st.skel
  | [], _ => rfl
  | r :: rest, st =>
    (puts_skel rest (st.put r.1 r.2.1 r.2.2)).trans (State.put_skel st r.1 r.2.1 r.2.2)

theorem puts_next : ∀ (reps : List Rep) (st : State), (puts st reps).next = st.next
  | [], _ => rfl
  | r :: rest, st =>
    (puts_next rest (st.put r.1 r.2.1 r.2.2)).trans (State.put_next st r.1 r.2.1 r.2.2)

/-- The running frame's id is the first frame's. -/
theorem State.top_of_skel :
    ∀ (st : State) (fr : Frame) (rest : List Frame), st.skel = fr :: rest → st.top = fr.id
  | { base := _, stack := [], next := _ }, fr, rest, h => by
    have h' : ([] : List Frame) = fr :: rest := h
    cases h'
  | { base := _, stack := a :: l, next := _ }, fr, rest, h => by
    have h' : bare a :: l.map bare = fr :: rest := h
    exact congrTo Frame.id (And.left (List.cons.inj h'))

theorem State.topFn_of_skel :
    ∀ (st : State) (fr : Frame) (rest : List Frame), st.skel = fr :: rest → st.topFn = fr.fn
  | { base := _, stack := [], next := _ }, fr, rest, h => by
    have h' : ([] : List Frame) = fr :: rest := h
    cases h'
  | { base := _, stack := a :: l, next := _ }, fr, rest, h => by
    have h' : bare a :: l.map bare = fr :: rest := h
    exact congrTo Frame.fn (And.left (List.cons.inj h'))

/-! Jumps. -/

/-- A map on the stack that leaves memory alone commutes with the list. -/
theorem puts_mapStack {M : List Frame → List Frame} (hM : MemMap M) :
    ∀ (reps : List Rep) (st : State), puts (st.mapStack M) reps = (puts st reps).mapStack M
  | [], _ => rfl
  | r :: rest, st => by
    have one : (st.mapStack M).put r.1 r.2.1 r.2.2 = (st.put r.1 r.2.1 r.2.2).mapStack M :=
      State.setSlot_mapStack hM st { frame := r.1, slot := r.2.1, path := [] } r.2.2
    have step : puts (st.mapStack M) (r :: rest) =
        puts ((st.put r.1 r.2.1 r.2.2).mapStack M) rest :=
      congrTo (fun s => puts s rest) one
    exact step.trans (puts_mapStack hM rest (st.put r.1 r.2.1 r.2.2))

theorem State.jump_nil (x : BlockId) :
    ∀ (st : State), st.skel = [] → st.jump x = Outcome.undefined Undefined.illFormed
  | { base := _, stack := [], next := _ }, _ => rfl
  | { base := _, stack := fr :: rest, next := _ }, h => by
    have h' : bare fr :: rest.map bare = ([] : List Frame) := h
    cases h'

/-- A jump, on a stack that is not empty, is the running frame at the block. -/
theorem State.jump_cons (x : BlockId) :
    ∀ (st : State) (fr : Frame) (rest : List Frame), st.skel = fr :: rest →
      st.jump x = Outcome.ok (Next.state (st.mapStack (withTopBlock x)))
  | { base := _, stack := [], next := _ }, fr, rest, h => by
    have h' : ([] : List Frame) = fr :: rest := h
    cases h'
  | { base := _, stack := _ :: _, next := _ }, _, _, _ => rfl

/-- The frames after a jump: the first at the block. -/
theorem State.skel_jump (x : BlockId) :
    ∀ (st : State) (fr : Frame) (rest : List Frame), st.skel = fr :: rest →
      (st.mapStack (withTopBlock x)).skel = fr.withBlock x :: rest
  | { base := _, stack := [], next := _ }, fr, rest, h => by
    have h' : ([] : List Frame) = fr :: rest := h
    cases h'
  | { base := _, stack := a :: l, next := _ }, fr, rest, h => by
    have h' : bare a :: l.map bare = fr :: rest := h
    have e1 : bare a = fr := And.left (List.cons.inj h')
    have e2 : l.map bare = rest := And.right (List.cons.inj h')
    have e3 : bare (a.withBlock x) = fr.withBlock x := congrTo (fun f => Frame.withBlock f x) e1
    have s1 : bare (a.withBlock x) :: l.map bare = bare (a.withBlock x) :: rest :=
      congrTo (fun t => bare (a.withBlock x) :: t) e2
    have s2 : bare (a.withBlock x) :: rest = fr.withBlock x :: rest :=
      congrTo (fun f => f :: rest) e3
    exact s1.trans s2

/-- A jump through the list. -/
theorem jump_puts (x : BlockId) (reps : List Rep) (st : State) :
    (puts st reps).jump x = Outcome.map (Next.map (fun s => puts s reps)) (st.jump x) := by
  cases hsk : st.skel with
  | nil =>
    have e1 : (puts st reps).jump x = Outcome.undefined Undefined.illFormed :=
      State.jump_nil x (puts st reps) ((puts_skel reps st).trans hsk)
    have e2 : st.jump x = Outcome.undefined Undefined.illFormed := State.jump_nil x st hsk
    exact e1.trans (congrTo (Outcome.map (Next.map (fun s => puts s reps))) e2).symm
  | cons fr rest =>
    have e1 : (puts st reps).jump x =
        Outcome.ok (Next.state ((puts st reps).mapStack (withTopBlock x))) :=
      State.jump_cons x (puts st reps) fr rest ((puts_skel reps st).trans hsk)
    have e2 : st.jump x = Outcome.ok (Next.state (st.mapStack (withTopBlock x))) :=
      State.jump_cons x st fr rest hsk
    have mid : Outcome.ok (Next.state ((puts st reps).mapStack (withTopBlock x))) =
        Outcome.map (Next.map (fun s => puts s reps))
          (Outcome.ok (Next.state (st.mapStack (withTopBlock x)))) :=
      congrTo (fun s => Outcome.ok (Next.state s))
        (puts_mapStack (withTopBlock_memMap x) reps st).symm
    exact e1.trans (mid.trans (congrTo (Outcome.map (Next.map (fun s => puts s reps))) e2).symm)

theorem jump?_puts (reps : List Rep) (st : State) :
    ∀ (target : Option BlockId),
      (puts st reps).jump? target =
        Outcome.map (Next.map (fun s => puts s reps)) (st.jump? target)
  | some t => jump_puts t reps st
  | none => rfl

theorem switchOn_puts (arms : List (Int × BlockId)) (otherwise : BlockId) (reps : List Rep)
    (st : State) :
    ∀ (key : Option Int),
      switchOn (puts st reps) arms otherwise key =
        Outcome.map (Next.map (fun s => puts s reps)) (switchOn st arms otherwise key)
  | some key => jump_puts (switchTarget key arms otherwise) reps st
  | none => rfl

theorem assertOn_puts (expected : Bool) (kind : Error) (target : BlockId) (reps : List Rep)
    (st : State) :
    ∀ (v : Value),
      assertOn (puts st reps) expected kind target v =
        Outcome.map (Next.map (fun s => puts s reps)) (assertOn st expected kind target v)
  | Value.bool b => by
    by_cases h : b = expected
    · have left : assertOn (puts st reps) expected kind target (Value.bool b) =
          (puts st reps).jump target :=
        (assertOn_bool (puts st reps) expected kind target b).trans (if_pos h)
      have right : assertOn st expected kind target (Value.bool b) = st.jump target :=
        (assertOn_bool st expected kind target b).trans (if_pos h)
      exact left.trans ((jump_puts target reps st).trans
        (congrTo (Outcome.map (Next.map (fun s => puts s reps))) right).symm)
    · have left : assertOn (puts st reps) expected kind target (Value.bool b) =
          Outcome.panic kind :=
        (assertOn_bool (puts st reps) expected kind target b).trans (if_neg h)
      have right : assertOn st expected kind target (Value.bool b) = Outcome.panic kind :=
        (assertOn_bool st expected kind target b).trans (if_neg h)
      exact left.trans (congrTo (Outcome.map (Next.map (fun s => puts s reps))) right).symm
  | Value.unit => rfl
  | Value.int _ _ => rfl
  | Value.adt _ _ => rfl
  | Value.array _ => rfl
  | Value.ref _ => rfl
  | Value.dead => rfl

/-! Writes: an entry whose whole slot is written leaves the list. -/

/-- The pointer is the entry's whole slot. -/
def atPtr (q : Pointer) (r : Rep) : Bool :=
  decide (q = { frame := r.1, slot := r.2.1, path := [] })

/-- The list without the entries whose whole slot the pointer is. -/
def dropAt (q : Pointer) : List Rep → List Rep
  | [] => []
  | r :: rest => if atPtr q r = true then dropAt q rest else r :: dropAt q rest

theorem dropAt_cons (q : Pointer) (r : Rep) (rest : List Rep) :
    dropAt q (r :: rest) = if atPtr q r = true then dropAt q rest else r :: dropAt q rest := rfl

/-- What is left in the list was in it, and is not the pointer's whole slot. -/
theorem mem_dropAt (q : Pointer) :
    ∀ (reps : List Rep) (r : Rep), r ∈ dropAt q reps → r ∈ reps ∧ ¬ atPtr q r = true
  | [], _, m => mem_nil_elim m
  | x :: rest, r, m => by
    by_cases ha : atPtr q x = true
    · have kept : dropAt q (x :: rest) = dropAt q rest := (dropAt_cons q x rest).trans (if_pos ha)
      have m' : r ∈ dropAt q rest := by
        rw [kept] at m
        exact m
      have ih := mem_dropAt q rest r m'
      exact And.intro (List.mem_cons.mpr (Or.inr (And.left ih))) (And.right ih)
    · have kept : dropAt q (x :: rest) = x :: dropAt q rest :=
        (dropAt_cons q x rest).trans (if_neg ha)
      have m' : r ∈ x :: dropAt q rest := by
        rw [kept] at m
        exact m
      cases List.mem_cons.mp m' with
      | inl here =>
        rw [here]
        exact And.intro (List.mem_cons.mpr (Or.inl rfl)) ha
      | inr there =>
        have ih := mem_dropAt q rest r there
        exact And.intro (List.mem_cons.mpr (Or.inr (And.left ih))) (And.right ih)

/-- A write through the list: an entry that is the written pointer's whole slot is
forgotten, and the write commutes with every other. -/
theorem write_puts (q : Pointer) (v : Value) :
    ∀ (reps : List Rep) (st : State),
      (∀ (r : Rep), r ∈ reps → atPtr q r = true ∨ Off r.1 r.2.1 q) →
        (puts st reps).write q v = Outcome.map (fun x => puts x (dropAt q reps)) (st.write q v)
  | [], st, _ => (Outcome.map_same (st.write q v)).symm
  | r :: rest, st, h => by
    have inner : (puts (st.put r.1 r.2.1 r.2.2) rest).write q v =
        Outcome.map (fun x => puts x (dropAt q rest)) ((st.put r.1 r.2.1 r.2.2).write q v) :=
      write_puts q v rest (st.put r.1 r.2.1 r.2.2)
        (fun r' m => h r' (List.mem_cons.mpr (Or.inr m)))
    by_cases ha : atPtr q r = true
    · have eq : q = { frame := r.1, slot := r.2.1, path := [] } := of_decide_eq_true ha
      have whole : (st.put r.1 r.2.1 r.2.2).write { frame := r.1, slot := r.2.1, path := [] } v =
          st.write { frame := r.1, slot := r.2.1, path := [] } v :=
        State.write_setSlot_same st r.1 r.2.1 r.2.2 v
      have same : (st.put r.1 r.2.1 r.2.2).write q v = st.write q v :=
        (congrTo (fun p => State.write (st.put r.1 r.2.1 r.2.2) p v) eq).trans
          (whole.trans (congrTo (fun p => State.write st p v) eq).symm)
      have kept : dropAt q (r :: rest) = dropAt q rest := (dropAt_cons q r rest).trans (if_pos ha)
      rw [kept]
      exact inner.trans (congrTo (Outcome.map (fun x => puts x (dropAt q rest))) same)
    · have kept : dropAt q (r :: rest) = r :: dropAt q rest :=
        (dropAt_cons q r rest).trans (if_neg ha)
      have off : Off r.1 r.2.1 q :=
        (h r (List.mem_cons.mpr (Or.inl rfl))).elim (fun a => absurd a ha) (fun o => o)
      have single : (st.put r.1 r.2.1 r.2.2).write q v =
          Outcome.map (fun s => State.put s r.1 r.2.1 r.2.2) (st.write q v) :=
        State.write_setSlot_other st { frame := r.1, slot := r.2.1, path := [] } q r.2.2 v off
      have both : Outcome.map (fun x => puts x (dropAt q rest))
            ((st.put r.1 r.2.1 r.2.2).write q v) =
          Outcome.map (fun x => puts (State.put x r.1 r.2.1 r.2.2) (dropAt q rest))
            (st.write q v) :=
        (congrTo (Outcome.map (fun x => puts x (dropAt q rest))) single).trans
          (Outcome.map_map (fun s => State.put s r.1 r.2.1 r.2.2)
            (fun x => puts x (dropAt q rest)) (st.write q v))
      rw [kept]
      exact inner.trans both

/-! Calls: the new frame's id is one no entry has. -/

/-- The frame a call makes. -/
def newFrame (st : State) (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) : Frame :=
  Frame.mk st.next id 0
    (Slot.uninit :: (args.map Slot.init ++ List.replicate (sig.locals - 1 - sig.args) Slot.uninit))
    dest target

theorem enterState_stack (st : State) (id : FnId) (sig : FnSig) (args : List Value)
    (dest : Pointer) (target : Option BlockId) :
    enterState st id sig args dest target =
      State.mk st.base (newFrame st id sig args dest target :: st.stack) (st.next + 1) := rfl

/-- The frames after a call: the new one, then those there were. -/
theorem enterState_skel (st : State) (id : FnId) (sig : FnSig) (args : List Value)
    (dest : Pointer) (target : Option BlockId) :
    (enterState st id sig args dest target).skel =
      bare (newFrame st id sig args dest target) :: st.skel := rfl

theorem enterState_slots? (st : State) (id : FnId) (sig : FnSig) (args : List Value)
    (dest : Pointer) (target : Option BlockId) (f : Nat) (hf : ¬ f = st.next) :
    (enterState st id sig args dest target).slots? f = st.slots? f := by
  by_cases h0 : f = 0
  · have left : (enterState st id sig args dest target).slots? f = some st.base :=
      (State.slots?_def (enterState st id sig args dest target) f).trans (if_pos h0)
    have right : st.slots? f = some st.base := (State.slots?_def st f).trans (if_pos h0)
    exact left.trans right.symm
  · have hne : ¬ (newFrame st id sig args dest target).id = f := fun e => hf e.symm
    have found : findFrame (newFrame st id sig args dest target :: st.stack) f =
        findFrame st.stack f :=
      (findFrame_cons (newFrame st id sig args dest target) st.stack f).trans (if_neg hne)
    have left : (enterState st id sig args dest target).slots? f =
        (findFrame st.stack f).map (fun fr => fr.locals) :=
      ((State.slots?_def (enterState st id sig args dest target) f).trans (if_neg h0)).trans
        (congrTo (Option.map (fun (fr : Frame) => fr.locals)) found)
    have right : st.slots? f = (findFrame st.stack f).map (fun fr => fr.locals) :=
      (State.slots?_def st f).trans (if_neg h0)
    exact left.trans right.symm

theorem enterState_setSlots (st : State) (id : FnId) (sig : FnSig) (args : List Value)
    (dest : Pointer) (target : Option BlockId) (f : Nat) (hf : ¬ f = st.next)
    (slots : List Slot) :
    (enterState st id sig args dest target).setSlots f slots =
      enterState (st.setSlots f slots) id sig args dest target := by
  by_cases h0 : f = 0
  · have left : (enterState st id sig args dest target).setSlots f slots =
        State.mk slots (newFrame st id sig args dest target :: st.stack) (st.next + 1) :=
      State.setSlots_zero (enterState st id sig args dest target) f h0 slots
    have right : enterState (st.setSlots f slots) id sig args dest target =
        State.mk slots (newFrame st id sig args dest target :: st.stack) (st.next + 1) :=
      congrTo (fun s => enterState s id sig args dest target) (State.setSlots_zero st f h0 slots)
    exact left.trans right.symm
  · have hne : ¬ (newFrame st id sig args dest target).id = f := fun e => hf e.symm
    have e1 : setFrame (newFrame st id sig args dest target :: st.stack) f slots =
        newFrame st id sig args dest target :: setFrame st.stack f slots :=
      (setFrame_cons (newFrame st id sig args dest target) st.stack f slots).trans (if_neg hne)
    have left : (enterState st id sig args dest target).setSlots f slots =
        State.mk st.base (newFrame st id sig args dest target :: setFrame st.stack f slots)
          (st.next + 1) :=
      (State.setSlots_frame (enterState st id sig args dest target) f h0 slots).trans
        (congrTo (fun s => State.mk st.base s (st.next + 1)) e1)
    have right : enterState (st.setSlots f slots) id sig args dest target =
        State.mk st.base (newFrame st id sig args dest target :: setFrame st.stack f slots)
          (st.next + 1) :=
      congrTo (fun s => enterState s id sig args dest target) (State.setSlots_frame st f h0 slots)
    exact left.trans right.symm

theorem enterState_setSlot (st : State) (id : FnId) (sig : FnSig) (args : List Value)
    (dest : Pointer) (target : Option BlockId) (p : Pointer) (hp : ¬ p.frame = st.next)
    (slot : Slot) :
    (enterState st id sig args dest target).setSlot p slot =
      enterState (st.setSlot p slot) id sig args dest target := by
  have hs : (enterState st id sig args dest target).slots? p.frame = st.slots? p.frame :=
    enterState_slots? st id sig args dest target p.frame hp
  cases h : st.slots? p.frame with
  | none =>
    have left : (enterState st id sig args dest target).setSlot p slot =
        enterState st id sig args dest target :=
      State.setSlot_none (enterState st id sig args dest target) p slot (hs.trans h)
    have right : enterState (st.setSlot p slot) id sig args dest target =
        enterState st id sig args dest target :=
      congrTo (fun s => enterState s id sig args dest target) (State.setSlot_none st p slot h)
    exact left.trans right.symm
  | some slots =>
    have left : (enterState st id sig args dest target).setSlot p slot =
        enterState (st.setSlots p.frame (setNth slots p.slot slot)) id sig args dest target :=
      (State.setSlot_some (enterState st id sig args dest target) p slot slots (hs.trans h)).trans
        (enterState_setSlots st id sig args dest target p.frame hp (setNth slots p.slot slot))
    have right : enterState (st.setSlot p slot) id sig args dest target =
        enterState (st.setSlots p.frame (setNth slots p.slot slot)) id sig args dest target :=
      congrTo (fun s => enterState s id sig args dest target) (State.setSlot_some st p slot slots h)
    exact left.trans right.symm

/-- Entering a call commutes with the list, when no entry is of the new frame's id. -/
theorem enterState_puts (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) :
    ∀ (reps : List Rep) (st : State), (∀ (r : Rep), r ∈ reps → ¬ r.1 = st.next) →
      puts (enterState st id sig args dest target) reps =
        enterState (puts st reps) id sig args dest target
  | [], _, _ => rfl
  | r :: rest, st, h => by
    have one : (enterState st id sig args dest target).put r.1 r.2.1 r.2.2 =
        enterState (st.put r.1 r.2.1 r.2.2) id sig args dest target :=
      enterState_setSlot st id sig args dest target { frame := r.1, slot := r.2.1, path := [] }
        (h r (List.mem_cons.mpr (Or.inl rfl))) r.2.2
    have hrest : ∀ (r' : Rep), r' ∈ rest → ¬ r'.1 = (st.put r.1 r.2.1 r.2.2).next := by
      intro r' m
      rw [State.put_next]
      exact h r' (List.mem_cons.mpr (Or.inr m))
    have step : puts (enterState st id sig args dest target) (r :: rest) =
        puts (enterState (st.put r.1 r.2.1 r.2.2) id sig args dest target) rest :=
      congrTo (fun s => puts s rest) one
    exact step.trans
      (enterState_puts id sig args dest target rest (st.put r.1 r.2.1 r.2.2) hrest)

theorem enter_puts (id : FnId) (sig : FnSig) (args : List Value) (dest : Pointer)
    (target : Option BlockId) (reps : List Rep) (st : State)
    (h : ∀ (r : Rep), r ∈ reps → ¬ r.1 = st.next) :
    (puts st reps).enter id sig args dest target =
      Outcome.map (Next.map (fun s => puts s reps)) (st.enter id sig args dest target) := by
  by_cases hl : args.length = sig.args
  · have left : (puts st reps).enter id sig args dest target =
        Outcome.ok (Next.state (enterState (puts st reps) id sig args dest target)) :=
      (State.enter_eq (puts st reps) id sig args dest target).trans (if_pos hl)
    have right : st.enter id sig args dest target =
        Outcome.ok (Next.state (enterState st id sig args dest target)) :=
      (State.enter_eq st id sig args dest target).trans (if_pos hl)
    have mid : Outcome.ok (Next.state (enterState (puts st reps) id sig args dest target)) =
        Outcome.map (Next.map (fun s => puts s reps))
          (Outcome.ok (Next.state (enterState st id sig args dest target))) :=
      congrTo (fun s => Outcome.ok (Next.state s))
        (enterState_puts id sig args dest target reps st h).symm
    exact left.trans (mid.trans (congrTo (Outcome.map (Next.map (fun s => puts s reps))) right).symm)
  · have left : (puts st reps).enter id sig args dest target =
        Outcome.undefined Undefined.illFormed :=
      (State.enter_eq (puts st reps) id sig args dest target).trans (if_neg hl)
    have right : st.enter id sig args dest target = Outcome.undefined Undefined.illFormed :=
      (State.enter_eq st id sig args dest target).trans (if_neg hl)
    exact left.trans (congrTo (Outcome.map (Next.map (fun s => puts s reps))) right).symm

/-- An intrinsic depends on the state only through the reads at the pointers its arguments
are. -/
theorem Intrinsic.eval_same (st st' : State) :
    ∀ (i : Intrinsic) (args : List Value),
      (∀ (p : Pointer), Value.ref p ∈ args → st'.read p = st.read p) →
        i.eval st' args = i.eval st args
  | Intrinsic.panic, _, _ => rfl
  | Intrinsic.sliceLen, [], _ => rfl
  | Intrinsic.sliceLen, [Value.ref p], h =>
    congrTo (fun x => Outcome.bind x lenOf) (h p (List.mem_cons.mpr (Or.inl rfl)))
  | Intrinsic.sliceLen, [Value.unit], _ => rfl
  | Intrinsic.sliceLen, [Value.bool _], _ => rfl
  | Intrinsic.sliceLen, [Value.int _ _], _ => rfl
  | Intrinsic.sliceLen, [Value.adt _ _], _ => rfl
  | Intrinsic.sliceLen, [Value.array _], _ => rfl
  | Intrinsic.sliceLen, [Value.dead], _ => rfl
  | Intrinsic.sliceLen, Value.ref _ :: _ :: _, _ => rfl
  | Intrinsic.sliceLen, Value.unit :: _ :: _, _ => rfl
  | Intrinsic.sliceLen, Value.bool _ :: _ :: _, _ => rfl
  | Intrinsic.sliceLen, Value.int _ _ :: _ :: _, _ => rfl
  | Intrinsic.sliceLen, Value.adt _ _ :: _ :: _, _ => rfl
  | Intrinsic.sliceLen, Value.array _ :: _ :: _, _ => rfl
  | Intrinsic.sliceLen, Value.dead :: _ :: _, _ => rfl
  | Intrinsic.wrappingAdd, [], _ => rfl
  | Intrinsic.wrappingAdd, [_], _ => rfl
  | Intrinsic.wrappingAdd, [_, _], _ => rfl
  | Intrinsic.wrappingAdd, _ :: _ :: _ :: _, _ => rfl
  | Intrinsic.wrappingSub, [], _ => rfl
  | Intrinsic.wrappingSub, [_], _ => rfl
  | Intrinsic.wrappingSub, [_, _], _ => rfl
  | Intrinsic.wrappingSub, _ :: _ :: _ :: _, _ => rfl
  | Intrinsic.wrappingMul, [], _ => rfl
  | Intrinsic.wrappingMul, [_], _ => rfl
  | Intrinsic.wrappingMul, [_, _], _ => rfl
  | Intrinsic.wrappingMul, _ :: _ :: _ :: _, _ => rfl
  | Intrinsic.saturatingAdd, [], _ => rfl
  | Intrinsic.saturatingAdd, [_], _ => rfl
  | Intrinsic.saturatingAdd, [_, _], _ => rfl
  | Intrinsic.saturatingAdd, _ :: _ :: _ :: _, _ => rfl
  | Intrinsic.saturatingSub, [], _ => rfl
  | Intrinsic.saturatingSub, [_], _ => rfl
  | Intrinsic.saturatingSub, [_, _], _ => rfl
  | Intrinsic.saturatingSub, _ :: _ :: _ :: _, _ => rfl
  | Intrinsic.construct _, _, _ => rfl

/-- The list after a call's terminator: a call of a function keeps it; an intrinsic writes
the destination at once, so the entries that are its whole slot leave. -/
def callReps : Callee → Pointer → List Rep → List Rep
  | Callee.fn _, _, reps => reps
  | Callee.intrinsic _, dest, reps => dropAt dest reps

/-- A call through the list. -/
theorem callOn_puts (sig : FnId → Option FnSig) (reps : List Rep) (st : State)
    (args : List Value) (dest : Pointer) (target : Option BlockId)
    (hnext : ∀ (r : Rep), r ∈ reps → ¬ r.1 = st.next)
    (hargs : ∀ (p : Pointer), Value.ref p ∈ args → ∀ (r : Rep), r ∈ reps → Off r.1 r.2.1 p)
    (hd : ∀ (r : Rep), r ∈ reps → atPtr dest r = true ∨ Off r.1 r.2.1 dest) :
    ∀ (callee : Callee),
      callOn sig (puts st reps) callee args dest target =
        Outcome.map (Next.map (fun x => puts x (callReps callee dest reps)))
          (callOn sig st callee args dest target)
  | Callee.fn id => by
    simp only [callOn]
    cases hs : sig id with
    | none => rfl
    | some s => exact enter_puts id s args dest target reps st hnext
  | Callee.intrinsic i =>
    Outcome.bind_commutes
      (Intrinsic.eval_same st (puts st reps) i args (fun p m => read_puts reps st p (hargs p m)))
      (fun v _ =>
        Outcome.bind_commutes_map (write_puts dest v reps st hd) (fun st1 _ =>
          jump?_puts (dropAt dest reps) st1 target))

/-! Returns: the running frame goes, and its entries replace nothing. -/

def dropTop : List Frame → List Frame
  | [] => []
  | _ :: rest => rest

/-- The state without its running frame. -/
def State.pop (st : State) : State :=
  { base := st.base, stack := dropTop st.stack, next := st.next }

theorem State.pop_skel :
    ∀ (st : State) (fr : Frame) (rest : List Frame), st.skel = fr :: rest → st.pop.skel = rest
  | { base := _, stack := [], next := _ }, fr, rest, h => by
    have h' : ([] : List Frame) = fr :: rest := h
    cases h'
  | { base := _, stack := a :: l, next := _ }, fr, rest, h => by
    have h' : bare a :: l.map bare = fr :: rest := h
    exact And.right (List.cons.inj h')

/-- What a return does once the returned value is read, from the frames before it (`sk`)
and the state without the running frame (`below`): the run's answer for the entry function,
else the write to the destination in the caller and the jump. -/
def retVia (v : Value) (below : State) : List Frame → Outcome Next
  | [] => Outcome.undefined Undefined.illFormed
  | [_] => Outcome.ok (Next.done v)
  | fr :: _ :: _ => (below.write fr.dest v).bind (fun st1 => st1.jump? fr.target)

theorem State.ret_cons (base : List Slot) (fr : Frame) (stack : List Frame) (next : Nat)
    (hpos : ¬ fr.id = 0) :
    State.ret (State.mk base (fr :: stack) next) =
      (State.slot (State.mk base (fr :: stack) next)
          { frame := fr.id, slot := 0, path := [] }).bind (fun slot =>
        slot.load.bind (fun v =>
          retVia v (State.mk base stack next) (bare fr :: stack.map bare))) := by
  have found : findFrame (fr :: stack) fr.id = some fr :=
    (findFrame_cons fr stack fr.id).trans (if_pos rfl)
  have a : State.slots? (State.mk base (fr :: stack) next) fr.id = some fr.locals :=
    ((State.slots?_def (State.mk base (fr :: stack) next) fr.id).trans (if_neg hpos)).trans
      (congrTo (Option.map (fun (x : Frame) => x.locals)) found)
  have hs : State.slot (State.mk base (fr :: stack) next)
      { frame := fr.id, slot := 0, path := [] } = slotOf (nth? fr.locals 0) :=
    congrTo (slotIn 0) a
  rw [hs]
  cases fr with
  | mk fid ffn fblock locals dest target =>
    cases locals with
    | nil => rfl
    | cons slot locals =>
      cases stack with
      | nil => rfl
      | cons caller stack => rfl

/-- A return, by the frames: the read of the running frame's local 0, then `retVia`. -/
theorem State.ret_skel :
    ∀ (st : State) (c : Frame) (rest : List Frame), st.skel = c :: rest → ¬ c.id = 0 →
      st.ret =
        (st.slot { frame := c.id, slot := 0, path := [] }).bind (fun slot =>
          slot.load.bind (fun v => retVia v st.pop (c :: rest)))
  | { base := _, stack := [], next := _ }, c, rest, h, _ => by
    have h' : ([] : List Frame) = c :: rest := h
    cases h'
  | { base := base, stack := a :: l, next := next }, c, rest, h, hpos => by
    have h' : bare a :: l.map bare = c :: rest := h
    have e1 : bare a = c := And.left (List.cons.inj h')
    have e2 : l.map bare = rest := And.right (List.cons.inj h')
    subst e1
    subst e2
    exact State.ret_cons base a l next hpos

theorem findFrame_none_of (id : Nat) :
    ∀ (l : List Frame), (∀ (x : Frame), x ∈ l → ¬ x.id = id) → findFrame l id = none
  | [], _ => rfl
  | fr :: rest, h =>
    ((findFrame_cons fr rest id).trans (if_neg (h fr (List.mem_cons.mpr (Or.inl rfl))))).trans
      (findFrame_none_of id rest (fun x m => h x (List.mem_cons.mpr (Or.inr m))))

/-- Popping the running frame commutes with a replacement, when no frame under it has its
id: a replacement in the popped frame is then none. -/
theorem State.pop_put_cons (base : List Slot) (c : Frame) (rest : List Frame) (next : Nat)
    (hpos : ¬ c.id = 0) (hu : findFrame rest c.id = none) (f l : Nat) (sl : Slot) :
    (State.put (State.mk base (c :: rest) next) f l sl).pop =
      (State.mk base rest next).put f l sl := by
  by_cases h0 : f = 0
  · have sa : State.slots? (State.mk base (c :: rest) next) f = some base :=
      (State.slots?_def (State.mk base (c :: rest) next) f).trans (if_pos h0)
    have sb : State.slots? (State.mk base rest next) f = some base :=
      (State.slots?_def (State.mk base rest next) f).trans (if_pos h0)
    have a : State.put (State.mk base (c :: rest) next) f l sl =
        State.mk (setNth base l sl) (c :: rest) next :=
      (State.setSlot_some (State.mk base (c :: rest) next) { frame := f, slot := l, path := [] }
          sl base sa).trans
        (State.setSlots_zero (State.mk base (c :: rest) next) f h0 (setNth base l sl))
    have b : State.put (State.mk base rest next) f l sl =
        State.mk (setNth base l sl) rest next :=
      (State.setSlot_some (State.mk base rest next) { frame := f, slot := l, path := [] }
          sl base sb).trans
        (State.setSlots_zero (State.mk base rest next) f h0 (setNth base l sl))
    have pa : (State.put (State.mk base (c :: rest) next) f l sl).pop =
        State.mk (setNth base l sl) rest next := congrTo State.pop a
    exact pa.trans b.symm
  · by_cases hc : c.id = f
    · have found : findFrame (c :: rest) f = some c :=
        (findFrame_cons c rest f).trans (if_pos hc)
      have sa : State.slots? (State.mk base (c :: rest) next) f = some c.locals :=
        ((State.slots?_def (State.mk base (c :: rest) next) f).trans (if_neg h0)).trans
          (congrTo (Option.map (fun (x : Frame) => x.locals)) found)
      have set : setFrame (c :: rest) f (setNth c.locals l sl) =
          c.withLocals (setNth c.locals l sl) :: rest :=
        (setFrame_cons c rest f (setNth c.locals l sl)).trans (if_pos hc)
      have a : State.put (State.mk base (c :: rest) next) f l sl =
          State.mk base (c.withLocals (setNth c.locals l sl) :: rest) next :=
        ((State.setSlot_some (State.mk base (c :: rest) next)
              { frame := f, slot := l, path := [] } sl c.locals sa).trans
            (State.setSlots_frame (State.mk base (c :: rest) next) f h0
              (setNth c.locals l sl))).trans
          (congrTo (fun s => State.mk base s next) set)
      have hf : findFrame rest f = none := (congrTo (findFrame rest) hc.symm).trans hu
      have sb : State.slots? (State.mk base rest next) f = none :=
        ((State.slots?_def (State.mk base rest next) f).trans (if_neg h0)).trans
          (congrTo (Option.map (fun (x : Frame) => x.locals)) hf)
      have b : State.put (State.mk base rest next) f l sl = State.mk base rest next :=
        State.setSlot_none (State.mk base rest next) { frame := f, slot := l, path := [] } sl sb
      have pa : (State.put (State.mk base (c :: rest) next) f l sl).pop =
          State.mk base rest next := congrTo State.pop a
      exact pa.trans b.symm
    · have found : findFrame (c :: rest) f = findFrame rest f :=
        (findFrame_cons c rest f).trans (if_neg hc)
      have sa : State.slots? (State.mk base (c :: rest) next) f =
          State.slots? (State.mk base rest next) f :=
        ((State.slots?_def (State.mk base (c :: rest) next) f).trans (if_neg h0)).trans
          ((congrTo (Option.map (fun (x : Frame) => x.locals)) found).trans
            ((State.slots?_def (State.mk base rest next) f).trans (if_neg h0)).symm)
      cases hs : State.slots? (State.mk base rest next) f with
      | none =>
        have a : State.put (State.mk base (c :: rest) next) f l sl =
            State.mk base (c :: rest) next :=
          State.setSlot_none (State.mk base (c :: rest) next)
            { frame := f, slot := l, path := [] } sl (sa.trans hs)
        have b : State.put (State.mk base rest next) f l sl = State.mk base rest next :=
          State.setSlot_none (State.mk base rest next) { frame := f, slot := l, path := [] } sl hs
        have pa : (State.put (State.mk base (c :: rest) next) f l sl).pop =
            State.mk base rest next := congrTo State.pop a
        exact pa.trans b.symm
      | some slots =>
        have set : setFrame (c :: rest) f (setNth slots l sl) =
            c :: setFrame rest f (setNth slots l sl) :=
          (setFrame_cons c rest f (setNth slots l sl)).trans (if_neg hc)
        have a : State.put (State.mk base (c :: rest) next) f l sl =
            State.mk base (c :: setFrame rest f (setNth slots l sl)) next :=
          ((State.setSlot_some (State.mk base (c :: rest) next)
                { frame := f, slot := l, path := [] } sl slots (sa.trans hs)).trans
              (State.setSlots_frame (State.mk base (c :: rest) next) f h0
                (setNth slots l sl))).trans
            (congrTo (fun s => State.mk base s next) set)
        have b : State.put (State.mk base rest next) f l sl =
            State.mk base (setFrame rest f (setNth slots l sl)) next :=
          (State.setSlot_some (State.mk base rest next) { frame := f, slot := l, path := [] }
              sl slots hs).trans
            (State.setSlots_frame (State.mk base rest next) f h0 (setNth slots l sl))
        have pa : (State.put (State.mk base (c :: rest) next) f l sl).pop =
            State.mk base (setFrame rest f (setNth slots l sl)) next := congrTo State.pop a
        exact pa.trans b.symm

theorem State.pop_put :
    ∀ (st : State) (c : Frame) (rest : List Frame), st.skel = c :: rest → ¬ c.id = 0 →
      (∀ (x : Frame), x ∈ rest → ¬ x.id = c.id) →
        ∀ (f l : Nat) (sl : Slot), (st.put f l sl).pop = st.pop.put f l sl
  | { base := _, stack := [], next := _ }, c, rest, h, _, _, _, _, _ => by
    have h' : ([] : List Frame) = c :: rest := h
    cases h'
  | { base := base, stack := a :: stack, next := next }, c, rest, h, hpos, hu, f, l, sl => by
    have h' : bare a :: stack.map bare = c :: rest := h
    have e1 : bare a = c := And.left (List.cons.inj h')
    have e2 : stack.map bare = rest := And.right (List.cons.inj h')
    have cid : c.id = a.id := (congrTo Frame.id e1).symm
    have hpos' : ¬ a.id = 0 := fun e => hpos (cid.trans e)
    have hu' : findFrame stack a.id = none :=
      findFrame_none_of a.id stack (fun x m e => by
        have m1 : bare x ∈ stack.map bare := List.mem_map.mpr ⟨x, m, rfl⟩
        have m2 : bare x ∈ rest := by
          rw [← e2]
          exact m1
        have same : (bare x).id = c.id := e.trans cid.symm
        exact hu (bare x) m2 same)
    exact State.pop_put_cons base a stack next hpos' hu' f l sl

/-- Popping the running frame commutes with the list. -/
theorem pop_puts (c : Frame) (rest : List Frame) (hpos : ¬ c.id = 0)
    (hu : ∀ (x : Frame), x ∈ rest → ¬ x.id = c.id) :
    ∀ (reps : List Rep) (st : State), st.skel = c :: rest → (puts st reps).pop = puts st.pop reps
  | [], _, _ => rfl
  | r :: reps, st, h => by
    have inner : (puts (st.put r.1 r.2.1 r.2.2) reps).pop =
        puts (st.put r.1 r.2.1 r.2.2).pop reps :=
      pop_puts c rest hpos hu reps (st.put r.1 r.2.1 r.2.2)
        ((State.put_skel st r.1 r.2.1 r.2.2).trans h)
    have one : (st.put r.1 r.2.1 r.2.2).pop = st.pop.put r.1 r.2.1 r.2.2 :=
      State.pop_put st c rest h hpos hu r.1 r.2.1 r.2.2
    have step : puts (st.put r.1 r.2.1 r.2.2).pop reps = puts st.pop (r :: reps) :=
      congrTo (fun s => puts s reps) one
    exact inner.trans step

theorem retVia_puts (v : Value) (c : Frame) (reps : List Rep) (st : State)
    (hd : ∀ (r : Rep), r ∈ reps → atPtr c.dest r = true ∨ Off r.1 r.2.1 c.dest) :
    ∀ (rest : List Frame),
      retVia v (puts st reps) (c :: rest) =
        Outcome.map (Next.map (fun x => puts x (dropAt c.dest reps))) (retVia v st (c :: rest))
  | [] => rfl
  | _ :: _ =>
    Outcome.bind_commutes_map (write_puts c.dest v reps st hd) (fun st1 _ =>
      jump?_puts (dropAt c.dest reps) st1 c.target)

/-- A return through the list: the entries that are the destination's whole slot leave it;
those of the frame that returned stay and replace nothing. -/
theorem ret_puts (st : State) (c : Frame) (rest : List Frame) (hsk : st.skel = c :: rest)
    (hpos : ¬ c.id = 0) (hu : ∀ (x : Frame), x ∈ rest → ¬ x.id = c.id) (reps : List Rep)
    (h0 : ∀ (r : Rep), r ∈ reps → Off r.1 r.2.1 { frame := c.id, slot := 0, path := [] })
    (hd : ∀ (r : Rep), r ∈ reps → atPtr c.dest r = true ∨ Off r.1 r.2.1 c.dest) :
    (puts st reps).ret =
      Outcome.map (Next.map (fun x => puts x (dropAt c.dest reps))) st.ret := by
  have es : (puts st reps).ret =
      ((puts st reps).slot { frame := c.id, slot := 0, path := [] }).bind (fun slot =>
        slot.load.bind (fun v => retVia v (puts st reps).pop (c :: rest))) :=
    State.ret_skel (puts st reps) c rest ((puts_skel reps st).trans hsk) hpos
  have et : st.ret =
      (st.slot { frame := c.id, slot := 0, path := [] }).bind (fun slot =>
        slot.load.bind (fun v => retVia v st.pop (c :: rest))) :=
    State.ret_skel st c rest hsk hpos
  have ep : (puts st reps).pop = puts st.pop reps := pop_puts c rest hpos hu reps st hsk
  rw [es, et, ep]
  exact Outcome.bind_commutes
    (slot_puts reps st { frame := c.id, slot := 0, path := [] } h0) (fun slot _ =>
      Outcome.bind_commutes rfl (fun v _ => retVia_puts v c reps st.pop hd rest))

end Blaze.Mir

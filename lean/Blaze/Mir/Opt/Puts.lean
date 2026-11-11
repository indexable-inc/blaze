import Blaze.Mir.Opt.SlotExecFar

/-!
A list of replaced slots, and a statement through it: global dead-store elimination (ledger
V11), piece (b), first part.

The two programs' states differ in the slots of dead locals, of the running frame and of the
frames under it. `puts st reps` is `st` with each slot of `reps` replaced, an entry being a
frame id, a local and what the slot holds. `execStmt_puts`: a statement from `puts st reps`
gives what it gives from `st` with the same slots replaced, when each entry is a slot no
pointer is into, holds a value the borrow invariant allows, and is either of another frame
or of a local the statement does not mention (`RepOk`).

Still to do in piece (b): a store to a replaced local drops its entry, and a removed store
adds one.
-/

namespace Blaze.Mir

/-- A replaced slot: the frame's id, the local, and what the slot holds. -/
abbrev Rep := Nat × Nat × Slot

/-- The state with each slot of the list replaced, in order. -/
def puts : State → List Rep → State
  | st, [] => st
  | st, r :: rest => puts (st.put r.1 r.2.1 r.2.2) rest

theorem Outcome.map_map {α β γ : Type} (f : α → β) (g : β → γ) :
    ∀ (x : Outcome α), Outcome.map g (Outcome.map f x) = Outcome.map (fun a => g (f a)) x
  | Outcome.ok _ => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

theorem Outcome.map_same {α : Type} : ∀ (x : Outcome α), Outcome.map (fun a => a) x = x
  | Outcome.ok _ => rfl
  | Outcome.panic _ => rfl
  | Outcome.undefined _ => rfl
  | Outcome.diverge => rfl

/-- The entry can be carried through the statement `s` run by the frame `top`: its slot
holds a value the invariant allows, no allowed pointer is into it, and it is of another
frame or of a local `s` does not mention. -/
def RepOk (B : FnId → Local → Prop) (g : List (Nat × FnId)) (top : Nat) (s : Statement)
    (r : Rep) : Prop :=
  SlotOk (PtrOk B g) r.2.2 ∧ (∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q) ∧
    ((top = r.1 ∧ stmtFine r.2.1 s = true) ∨ ¬ r.1 = top)

/-- A replacement keeps the invariant when what it puts is allowed. -/
theorem State.put_okAt {B : FnId → Local → Prop} {g : List (Nat × FnId)} {st : State}
    (hs : OkAt B g st) (f l : Nat) (sl : Slot) (hsl : SlotOk (PtrOk B g) sl) :
    OkAt B g (st.put f l sl) :=
  And.intro (Covers.of_ids (State.put_ids st f l sl) (And.left hs))
    (State.setSlot_ok (And.right hs) { frame := f, slot := l, path := [] } sl hsl)

theorem State.put_topFn (st : State) (f l : Nat) (sl : Slot) :
    (st.put f l sl).topFn = st.topFn :=
  State.topFn_of_ctl (State.setSlot_ctl st { frame := f, slot := l, path := [] } sl)

/-- One replaced slot through a statement. -/
theorem execStmt_rep {B : FnId → Local → Prop} {g : List (Nat × FnId)} (adts : List AdtDecl)
    {st : State} (hs : OkAt B g st) (fn : FnId) (hfn : st.topFn = fn) (s : Statement)
    (hB : StmtFine B fn s) (r : Rep) (hr : RepOk B g st.top s r) :
    execStmt adts (st.put r.1 r.2.1 r.2.2) s =
      Outcome.map (fun x => State.put x r.1 r.2.1 r.2.2) (execStmt adts st s) := by
  have off : ∀ (q : Pointer), PtrOk B g q → Off r.1 r.2.1 q := And.left (And.right hr)
  cases And.right (And.right hr) with
  | inl here =>
    have hides : Hides B g r.1 r.2.1 st := { okAt := hs, top := And.left here, off := off }
    exact execStmt_put adts hides r.2.2 fn hfn s hB (And.right here)
  | inr far =>
    have hides : HidesFar B g r.1 r.2.1 st := { okAt := hs, top := far, off := off }
    exact execStmt_putFar adts hides r.2.2 fn hfn s hB

/-- A statement through a list of replaced slots: from the state with the slots replaced it
gives its state with the same slots replaced. -/
theorem execStmt_puts {B : FnId → Local → Prop} {g : List (Nat × FnId)} (adts : List AdtDecl)
    (fn : FnId) (s : Statement) (hB : StmtFine B fn s) :
    ∀ (reps : List Rep) (st : State), OkAt B g st → st.topFn = fn →
      (∀ (r : Rep), r ∈ reps → RepOk B g st.top s r) →
        execStmt adts (puts st reps) s =
          Outcome.map (fun x => puts x reps) (execStmt adts st s)
  | [], st, _, _, _ => (Outcome.map_same (execStmt adts st s)).symm
  | r :: rest, st, hs, hfn, hreps => by
    have hr : RepOk B g st.top s r := hreps r (List.mem_cons.mpr (Or.inl rfl))
    have single : execStmt adts (st.put r.1 r.2.1 r.2.2) s =
        Outcome.map (fun x => State.put x r.1 r.2.1 r.2.2) (execStmt adts st s) :=
      execStmt_rep adts hs fn hfn s hB r hr
    have hs1 : OkAt B g (st.put r.1 r.2.1 r.2.2) :=
      State.put_okAt hs r.1 r.2.1 r.2.2 (And.left hr)
    have hfn1 : (st.put r.1 r.2.1 r.2.2).topFn = fn :=
      (State.put_topFn st r.1 r.2.1 r.2.2).trans hfn
    have hrest : ∀ (r' : Rep), r' ∈ rest →
        RepOk B g (st.put r.1 r.2.1 r.2.2).top s r' := by
      intro r' m
      rw [State.put_top]
      exact hreps r' (List.mem_cons.mpr (Or.inr m))
    have inner : execStmt adts (puts (st.put r.1 r.2.1 r.2.2) rest) s =
        Outcome.map (fun x => puts x rest) (execStmt adts (st.put r.1 r.2.1 r.2.2) s) :=
      execStmt_puts adts fn s hB rest (st.put r.1 r.2.1 r.2.2) hs1 hfn1 hrest
    have both : Outcome.map (fun x => puts x rest)
          (execStmt adts (st.put r.1 r.2.1 r.2.2) s) =
        Outcome.map (fun x => puts (State.put x r.1 r.2.1 r.2.2) rest) (execStmt adts st s) :=
      (congrTo (Outcome.map (fun x => puts x rest)) single).trans
        (Outcome.map_map (fun x => State.put x r.1 r.2.1 r.2.2) (fun x => puts x rest)
          (execStmt adts st s))
    exact inner.trans both

end Blaze.Mir

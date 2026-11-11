import Blaze.Mir.Opt.Ids
import Blaze.Mir.Opt.Ptr

/-!
What a pointer may name: the invariant dead-store elimination rests on, second piece.

`B fn l` says function `fn` may have a pointer to its local `l` (the locals it borrows).
`PtrOk B ids p`: the pointer names frame 0 (what the entry function's references point to),
or a local that the function of its frame, one of `ids`, may have a pointer to. `StateOk B st`:
every pointer in the state's memory is such a pointer (`MemOk`, `Opt/Ptr.lean`).

Here: writes and moves keep `StateOk`, and a place evaluates to such a pointer when its base
local may be borrowed or the place goes through a dereference (`evalPlace_ptr`): the pointer
is then the running frame's borrowed local, or one read from memory. So a local outside `B`
is never read or written through a pointer.

Operands, rvalues and statements, then calls and returns, are the next pieces.
-/

namespace Blaze.Mir

theorem panic_ne_ok {α : Type} {e : Error} {a : α} {C : Prop}
    (h : (Outcome.panic e : Outcome α) = Outcome.ok a) : C := by
  cases h

/-- The function of the frame with this id. -/
def fnOfIds : List (Nat × FnId) → Nat → Option FnId
  | [], _ => none
  | e :: rest, id => if e.1 = id then some e.2 else fnOfIds rest id

theorem fnOfIds_cons (e : Nat × FnId) (rest : List (Nat × FnId)) (id : Nat) :
    fnOfIds (e :: rest) id = if e.1 = id then some e.2 else fnOfIds rest id := rfl

def FrameHas (B : FnId → Local → Prop) (slot : Nat) : Option FnId → Prop
  | some fn => B fn slot
  | none => False

/-- The pointer names frame 0, or a local that the function of its frame may have a pointer
to, the frame being one `ids` lists. A list that keeps the frames that returned, and has none
with an id not yet given, says so of every pointer a run can make (`Opt/BorrowStep.lean`). -/
def PtrOk (B : FnId → Local → Prop) (ids : List (Nat × FnId)) (p : Pointer) : Prop :=
  p.frame = 0 ∨ FrameHas B p.slot (fnOfIds ids p.frame)

/-- Every pointer in the state's memory is one `PtrOk` allows. -/
def StateOk (B : FnId → Local → Prop) (st : State) : Prop := MemOk (PtrOk B st.ids) st

theorem StateOk.of_ids {B : FnId → Local → Prop} {st st' : State} (e : st'.ids = st.ids)
    (h : MemOk (PtrOk B st.ids) st') : StateOk B st' := by
  show MemOk (PtrOk B st'.ids) st'
  rw [e]
  exact h

/-! Writes and moves. -/

theorem State.write_stateOk {B : FnId → Local → Prop} {st : State} (hs : StateOk B st)
    (p : Pointer) (v : Value) (hv : Value.Ok (PtrOk B st.ids) v) (st' : State)
    (h : st.write p v = Outcome.ok st') : StateOk B st' :=
  StateOk.of_ids (State.write_ids st p v st' h) (State.write_ok hs p v hv st' h)

theorem State.setSlot_stateOk {B : FnId → Local → Prop} {st : State} (hs : StateOk B st)
    (p : Pointer) (slot : Slot) (hslot : SlotOk (PtrOk B st.ids) slot) :
    StateOk B (st.setSlot p slot) :=
  StateOk.of_ids (State.setSlot_ids st p slot) (State.setSlot_ok hs p slot hslot)

theorem stateOr_inv (Q : State → Prop) (st : State) (hq : Q st) :
    ∀ (x : Outcome State), (∀ (st' : State), x = Outcome.ok st' → Q st') → Q (stateOr st x)
  | Outcome.ok st', h => h st' rfl
  | Outcome.panic _, _ => hq
  | Outcome.undefined _, _ => hq
  | Outcome.diverge, _ => hq

theorem State.moveOut_stateOk {B : FnId → Local → Prop} {st : State} (hs : StateOk B st) :
    ∀ (p : Pointer), StateOk B (st.moveOut p)
  | { frame := frame, slot := slot, path := [] } =>
    State.setSlot_stateOk hs { frame := frame, slot := slot, path := [] } Slot.moved True.intro
  | { frame := frame, slot := slot, path := e :: rest } =>
    stateOr_inv (StateOk B) st hs
      (st.write { frame := frame, slot := slot, path := e :: rest } Value.dead)
      (fun st' h =>
        State.write_stateOk hs { frame := frame, slot := slot, path := e :: rest } Value.dead
          Value.Ok.dead st' h)

/-! Places. -/

theorem asRef_ok (P : Pointer → Prop) (v : Value) (q : Pointer) (hv : Value.Ok P v)
    (h : asRef v = Outcome.ok q) : P q := by
  cases hv with
  | unit => exact undefined_ne_ok h
  | bool b => exact undefined_ne_ok h
  | int ty x => exact undefined_ne_ok h
  | adt variant fields hf => exact undefined_ne_ok h
  | array elems hf => exact undefined_ne_ok h
  | ref p hp =>
    have e : p = q := Outcome.ok.inj h
    rw [← e]
    exact hp
  | dead => exact undefined_ne_ok h

theorem inBounds_ptr {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {p : Pointer}
    (hp : PtrOk B ids p) (n : Nat) :
    ∀ (o : Option Value) (q : Pointer), inBounds p n o = Outcome.ok q → PtrOk B ids q
  | some _, q, h => by
    have e : p.push (PathElem.index n) = q := Outcome.ok.inj h
    rw [← e]
    exact hp
  | none, _, h => panic_ne_ok h

theorem indexAt_ptr {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {p : Pointer}
    (hp : PtrOk B ids p) (n : Nat) :
    ∀ (arr : Value) (q : Pointer), indexAt p n arr = Outcome.ok q → PtrOk B ids q
  | Value.array elems, q, h => inBounds_ptr hp n (nth? elems n) q h
  | Value.unit, _, h => undefined_ne_ok h
  | Value.bool _, _, h => undefined_ne_ok h
  | Value.int _ _, _, h => undefined_ne_ok h
  | Value.adt _ _, _, h => undefined_ne_ok h
  | Value.ref _, _, h => undefined_ne_ok h
  | Value.dead, _, h => undefined_ne_ok h

theorem indexBy_ptr {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {p : Pointer}
    (hp : PtrOk B ids p) (arr : Value) :
    ∀ (i : Value) (q : Pointer), indexBy p arr i = Outcome.ok q → PtrOk B ids q
  | Value.int _ i, q, h => indexAt_ptr hp i.val.toNat arr q h
  | Value.unit, _, h => undefined_ne_ok h
  | Value.bool _, _, h => undefined_ne_ok h
  | Value.adt _ _, _, h => undefined_ne_ok h
  | Value.array _, _, h => undefined_ne_ok h
  | Value.ref _, _, h => undefined_ne_ok h
  | Value.dead, _, h => undefined_ne_ok h

/-- A dereference gives a pointer read from memory. -/
theorem projStep_deref {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {st : State}
    (hm : MemOk (PtrOk B ids) st) (p q : Pointer)
    (h : projStep st p ProjElem.deref = Outcome.ok q) : PtrOk B ids q := by
  have h' : (st.read p).bind asRef = Outcome.ok q := h
  have ⟨v, hv, r⟩ := Outcome.bind_eq_ok _ _ _ h'
  exact asRef_ok (PtrOk B ids) v q (State.read_ok hm p v hv) r

/-- Any other projection stays in the slot it started in. -/
theorem projStep_keep {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {st : State}
    (hm : MemOk (PtrOk B ids) st) {p : Pointer} (hp : PtrOk B ids p) :
    ∀ (e : ProjElem) (q : Pointer), projStep st p e = Outcome.ok q → PtrOk B ids q
  | ProjElem.deref, q, h => projStep_deref hm p q h
  | ProjElem.field i, q, h => by
    have e : p.push (PathElem.field i) = q := Outcome.ok.inj h
    rw [← e]
    exact hp
  | ProjElem.downcast variant, q, h => by
    have e : p.push (PathElem.downcast variant) = q := Outcome.ok.inj h
    rw [← e]
    exact hp
  | ProjElem.index l, q, h => by
    have h' : (st.read { frame := st.top, slot := l, path := [] }).bind (fun i =>
        (st.read p).bind (fun arr => indexBy p arr i)) = Outcome.ok q := h
    have ⟨i, _, r⟩ := Outcome.bind_eq_ok _ _ _ h'
    have r' : (st.read p).bind (fun arr => indexBy p arr i) = Outcome.ok q := r
    have ⟨arr, _, r2⟩ := Outcome.bind_eq_ok _ _ _ r'
    exact indexBy_ptr hp arr i q r2
  | ProjElem.constIndex n, q, h => by
    have h' : (st.read p).bind (indexAt p n) = Outcome.ok q := h
    have ⟨arr, _, r⟩ := Outcome.bind_eq_ok _ _ _ h'
    exact indexAt_ptr hp n arr q r

/-- Whether the projection goes through a dereference. -/
def hasDeref : List ProjElem → Bool
  | [] => false
  | ProjElem.deref :: _ => true
  | ProjElem.field _ :: rest => hasDeref rest
  | ProjElem.downcast _ :: rest => hasDeref rest
  | ProjElem.index _ :: rest => hasDeref rest
  | ProjElem.constIndex _ :: rest => hasDeref rest

theorem evalProj_step {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {st : State}
    (hm : MemOk (PtrOk B ids) st) (e : ProjElem) (rest : List ProjElem) (p q : Pointer)
    (h : evalProj st p (e :: rest) = Outcome.ok q) :
    ∃ p1, projStep st p e = Outcome.ok p1 ∧ evalProj st p1 rest = Outcome.ok q ∧
      (PtrOk B ids p → PtrOk B ids p1) := by
  have h' : (projStep st p e).bind (fun x => evalProj st x rest) = Outcome.ok q := h
  have ⟨p1, h1, r⟩ := Outcome.bind_eq_ok _ _ _ h'
  exact ⟨p1, h1, r, fun hp => projStep_keep hm hp e p1 h1⟩

/-- A projection from a pointer `PtrOk` allows, or through a dereference, gives a pointer
`PtrOk` allows. -/
theorem evalProj_ptr {B : FnId → Local → Prop} {ids : List (Nat × FnId)} {st : State}
    (hm : MemOk (PtrOk B ids) st) :
    ∀ (proj : List ProjElem) (p q : Pointer), PtrOk B ids p ∨ hasDeref proj = true →
      evalProj st p proj = Outcome.ok q → PtrOk B ids q
  | [], p, q, h0, h => by
    have e : p = q := Outcome.ok.inj h
    rw [← e]
    exact h0.elim (fun hp => hp) (fun hd => false_eq_true_elim hd)
  | ProjElem.deref :: rest, p, q, _, h => by
    have ⟨p1, h1, r, _⟩ := evalProj_step hm ProjElem.deref rest p q h
    exact evalProj_ptr hm rest p1 q (Or.inl (projStep_deref hm p p1 h1)) r
  | ProjElem.field i :: rest, p, q, h0, h => by
    have ⟨p1, _, r, keep⟩ := evalProj_step hm (ProjElem.field i) rest p q h
    exact evalProj_ptr hm rest p1 q
      (h0.elim (fun hp => Or.inl (keep hp)) (fun hd => Or.inr hd)) r
  | ProjElem.downcast variant :: rest, p, q, h0, h => by
    have ⟨p1, _, r, keep⟩ := evalProj_step hm (ProjElem.downcast variant) rest p q h
    exact evalProj_ptr hm rest p1 q
      (h0.elim (fun hp => Or.inl (keep hp)) (fun hd => Or.inr hd)) r
  | ProjElem.index l :: rest, p, q, h0, h => by
    have ⟨p1, _, r, keep⟩ := evalProj_step hm (ProjElem.index l) rest p q h
    exact evalProj_ptr hm rest p1 q
      (h0.elim (fun hp => Or.inl (keep hp)) (fun hd => Or.inr hd)) r
  | ProjElem.constIndex n :: rest, p, q, h0, h => by
    have ⟨p1, _, r, keep⟩ := evalProj_step hm (ProjElem.constIndex n) rest p q h
    exact evalProj_ptr hm rest p1 q
      (h0.elim (fun hp => Or.inl (keep hp)) (fun hd => Or.inr hd)) r

/-- The running frame's local `b` is one `PtrOk` allows when its function may borrow it. -/
theorem top_ptr (B : FnId → Local → Prop) (b : Local) :
    ∀ (st : State), B st.topFn b → PtrOk B st.ids { frame := st.top, slot := b, path := [] }
  | { base := _, stack := [], next := _ }, _ => Or.inl rfl
  | { base := _, stack := fr :: rest, next := _ }, hb => by
    have found : fnOfIds ((fr.id, fr.fn) :: rest.map idFnOf) fr.id = some fr.fn :=
      (fnOfIds_cons (fr.id, fr.fn) (rest.map idFnOf) fr.id).trans (if_pos rfl)
    have has : FrameHas B b (fnOfIds ((fr.id, fr.fn) :: rest.map idFnOf) fr.id) := by
      rw [found]
      exact hb
    exact Or.inr has

/-- A place whose base local may be borrowed, or that goes through a dereference, is where
`PtrOk` allows a pointer. -/
theorem evalPlace_ptr {B : FnId → Local → Prop} {st : State} (hs : StateOk B st) (place : Place)
    (q : Pointer) (hb : B st.topFn place.base ∨ hasDeref place.proj = true)
    (h : evalPlace st place = Outcome.ok q) : PtrOk B st.ids q :=
  evalProj_ptr hs place.proj { frame := st.top, slot := place.base, path := [] } q
    (hb.elim (fun b => Or.inl (top_ptr B place.base st b)) (fun hd => Or.inr hd)) h

end Blaze.Mir

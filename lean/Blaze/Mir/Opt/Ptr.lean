import Blaze.Mir.Step
import Blaze.Mir.Opt.Bind

/-!
What the pointers in memory point to: the invariant dead-store elimination rests on, first
piece.

Two states that differ on a dead local agree on every read only if no read reaches that local
through a pointer. So the proof needs to know which places a pointer in memory can name:
`Value.Ok P v` says every pointer inside `v` satisfies `P`, and `MemOk P st` says so of every
value in every slot of the state. Here, for any `P`: a read gives a value whose pointers
satisfy `P` (`State.read_ok`), and a write of such a value keeps the invariant
(`State.write_ok`).

The `P` dead-store elimination uses (a pointer names frame 0 or a local its function
borrows), and the invariant through operands, calls and returns, are the next pieces.
-/

namespace Blaze.Mir

/-- Every pointer inside the value satisfies `P`. -/
inductive Value.Ok (P : Pointer → Prop) : Value → Prop where
  | unit : Value.Ok P Value.unit
  | bool (b : Bool) : Value.Ok P (Value.bool b)
  | int (ty : ScalarTy) (x : Scalar ty) : Value.Ok P (Value.int ty x)
  | adt (variant : Nat) (fields : List Value)
      (h : ∀ (v : Value), v ∈ fields → Value.Ok P v) : Value.Ok P (Value.adt variant fields)
  | array (elems : List Value)
      (h : ∀ (v : Value), v ∈ elems → Value.Ok P v) : Value.Ok P (Value.array elems)
  | ref (p : Pointer) (h : P p) : Value.Ok P (Value.ref p)
  | dead : Value.Ok P Value.dead

theorem Value.Ok.of_eq {P : Pointer → Prop} {x y : Value} (e : y = x) (h : Value.Ok P x) :
    Value.Ok P y := by
  rw [e]
  exact h

def SlotOk (P : Pointer → Prop) : Slot → Prop
  | Slot.init v => Value.Ok P v
  | Slot.uninit => True
  | Slot.moved => True

def SlotsOk (P : Pointer → Prop) (slots : List Slot) : Prop :=
  ∀ (s : Slot), s ∈ slots → SlotOk P s

/-- Every pointer in every slot of the state satisfies `P`. -/
def MemOk (P : Pointer → Prop) (st : State) : Prop :=
  SlotsOk P st.base ∧ ∀ (fr : Frame), fr ∈ st.stack → SlotsOk P fr.locals

/-! Equations that cannot hold, and what an `ok` came from. -/

theorem undefined_ne_ok {α : Type} {why : Undefined} {a : α} {C : Prop}
    (h : (Outcome.undefined why : Outcome α) = Outcome.ok a) : C := by
  cases h

theorem found_ok (missing : Undefined) :
    ∀ (o : Option Value) (w : Value), found missing o = Outcome.ok w → o = some w
  | some _, _, h => congrTo some (Outcome.ok.inj h)
  | none, _, h => undefined_ne_ok h

theorem ite_ok {α : Type} {c : Prop} [Decidable c] {a w : α} {why : Undefined}
    (h : (if c then Outcome.ok a else Outcome.undefined why) = Outcome.ok w) : a = w := by
  by_cases hc : c
  · rw [if_pos hc] at h
    exact Outcome.ok.inj h
  · rw [if_neg hc] at h
    cases h

/-! Lists. -/

theorem mem_nil_elim {α : Type} {a : α} {C : Prop} (h : a ∈ ([] : List α)) : C := by
  cases h

theorem nth?_mem {α : Type} : ∀ (l : List α) (i : Nat) (x : α), nth? l i = some x → x ∈ l
  | [], _, _, h => none_eq_some_elim h
  | _ :: _, 0, _, h => List.mem_cons.mpr (Or.inl (Option.some.inj h).symm)
  | _ :: l, i + 1, x, h => List.mem_cons.mpr (Or.inr (nth?_mem l i x h))

theorem mem_setNth {α : Type} :
    ∀ (l : List α) (i : Nat) (x y : α), y ∈ setNth l i x → y = x ∨ y ∈ l
  | [], _, _, _, h => Or.inr h
  | _ :: _, 0, _, _, h =>
    (List.mem_cons.mp h).elim (fun e => Or.inl e)
      (fun m => Or.inr (List.mem_cons.mpr (Or.inr m)))
  | _ :: l, i + 1, x, y, h =>
    (List.mem_cons.mp h).elim (fun e => Or.inr (List.mem_cons.mpr (Or.inl e)))
      (fun m =>
        (mem_setNth l i x y m).elim (fun e => Or.inl e)
          (fun m' => Or.inr (List.mem_cons.mpr (Or.inr m'))))

theorem findFrame_mem :
    ∀ (l : List Frame) (id : Nat) (fr : Frame), findFrame l id = some fr → fr ∈ l
  | [], _, _, h => none_eq_some_elim h
  | a :: l, id, fr, h => by
    by_cases hc : a.id = id
    · have e : findFrame (a :: l) id = some a := (findFrame_cons a l id).trans (if_pos hc)
      have e2 : a = fr := Option.some.inj (e.symm.trans h)
      exact List.mem_cons.mpr (Or.inl e2.symm)
    · have e : findFrame (a :: l) id = findFrame l id :=
        (findFrame_cons a l id).trans (if_neg hc)
      exact List.mem_cons.mpr (Or.inr (findFrame_mem l id fr (e.symm.trans h)))

/-! Reads. -/

theorem Value.child_ok (P : Pointer → Prop) (v : Value) (e : PathElem) (w : Value)
    (hv : Value.Ok P v) (h : v.child e = Outcome.ok w) : Value.Ok P w := by
  cases hv with
  | unit => cases e <;> exact undefined_ne_ok h
  | bool b => cases e <;> exact undefined_ne_ok h
  | int ty x => cases e <;> exact undefined_ne_ok h
  | ref p hp => cases e <;> exact undefined_ne_ok h
  | dead => cases e <;> exact undefined_ne_ok h
  | adt variant fields hf =>
    cases e with
    | field i =>
      exact hf w (nth?_mem fields i w (found_ok Undefined.illFormed (nth? fields i) w h))
    | downcast want => exact Value.Ok.of_eq (ite_ok h).symm (Value.Ok.adt variant fields hf)
    | index i => exact undefined_ne_ok h
  | array elems hf =>
    cases e with
    | field i => exact undefined_ne_ok h
    | downcast want => exact undefined_ne_ok h
    | index i =>
      exact hf w (nth?_mem elems i w (found_ok Undefined.dangling (nth? elems i) w h))

theorem Value.project_ok (P : Pointer → Prop) :
    ∀ (path : List PathElem) (v w : Value), Value.Ok P v → v.project path = Outcome.ok w →
      Value.Ok P w
  | [], _, _, hv, h => Value.Ok.of_eq (Outcome.ok.inj h).symm hv
  | e :: rest, v, w, hv, h => by
    have h' : (Value.child v e).bind (fun inner => Value.project inner rest) = Outcome.ok w := h
    have ⟨x, hx, hr⟩ := Outcome.bind_eq_ok _ _ _ h'
    have hr' : Value.project x rest = Outcome.ok w := hr
    exact Value.project_ok P rest x w (Value.child_ok P v e x hv hx) hr'

theorem Value.checkLive_ok (v w : Value) (h : v.checkLive = Outcome.ok w) : v = w := by
  cases v <;> first | exact Outcome.ok.inj h | exact undefined_ne_ok h

theorem State.slots?_ok {P : Pointer → Prop} {st : State} (hm : MemOk P st) (f : Nat)
    (slots : List Slot) (h : st.slots? f = some slots) : SlotsOk P slots := by
  by_cases hf : f = 0
  · have e : st.slots? f = some st.base := (State.slots?_def st f).trans (if_pos hf)
    have e2 : st.base = slots := Option.some.inj (e.symm.trans h)
    rw [← e2]
    exact And.left hm
  · have e : st.slots? f = (findFrame st.stack f).map (fun fr => fr.locals) :=
      (State.slots?_def st f).trans (if_neg hf)
    have h2 : (findFrame st.stack f).map (fun fr => fr.locals) = some slots := e.symm.trans h
    cases hfr : findFrame st.stack f with
    | none =>
      rw [hfr] at h2
      exact none_eq_some_elim h2
    | some fr =>
      rw [hfr] at h2
      have e3 : fr.locals = slots := Option.some.inj h2
      rw [← e3]
      exact And.right hm fr (findFrame_mem st.stack f fr hfr)

theorem State.slot_ok {P : Pointer → Prop} {st : State} (hm : MemOk P st) (p : Pointer)
    (s : Slot) (h : st.slot p = Outcome.ok s) : SlotOk P s := by
  have h' : slotIn p.slot (st.slots? p.frame) = Outcome.ok s := h
  cases hs : st.slots? p.frame with
  | none =>
    rw [hs] at h'
    exact undefined_ne_ok h'
  | some slots =>
    rw [hs] at h'
    have h2 : slotOf (nth? slots p.slot) = Outcome.ok s := h'
    cases hn : nth? slots p.slot with
    | none =>
      rw [hn] at h2
      exact undefined_ne_ok h2
    | some x =>
      rw [hn] at h2
      have e : x = s := Outcome.ok.inj h2
      rw [← e]
      exact State.slots?_ok hm p.frame slots hs x (nth?_mem slots p.slot x hn)

theorem Slot.load_ok (P : Pointer → Prop) :
    ∀ (slot : Slot) (v : Value), SlotOk P slot → slot.load = Outcome.ok v → Value.Ok P v
  | Slot.init _, _, hs, h => Value.Ok.of_eq (Outcome.ok.inj h).symm hs
  | Slot.uninit, _, _, h => undefined_ne_ok h
  | Slot.moved, _, _, h => undefined_ne_ok h

/-- A read gives a value whose pointers satisfy `P`. -/
theorem State.read_ok {P : Pointer → Prop} {st : State} (hm : MemOk P st) (p : Pointer)
    (v : Value) (h : st.read p = Outcome.ok v) : Value.Ok P v := by
  have h' : (st.slot p).bind (fun slot =>
      slot.load.bind (fun v => (v.project p.path).bind Value.checkLive)) = Outcome.ok v := h
  have ⟨slot, h1, r1⟩ := Outcome.bind_eq_ok _ _ _ h'
  have r1' : slot.load.bind (fun v => (v.project p.path).bind Value.checkLive) =
      Outcome.ok v := r1
  have ⟨x, h2, r2⟩ := Outcome.bind_eq_ok _ _ _ r1'
  have r2' : (x.project p.path).bind Value.checkLive = Outcome.ok v := r2
  have ⟨y, h3, r3⟩ := Outcome.bind_eq_ok _ _ _ r2'
  have e : y = v := Value.checkLive_ok y v r3
  rw [← e]
  exact Value.project_ok P p.path x y (Slot.load_ok P slot x (State.slot_ok hm p slot h1) h2) h3

/-! Writes. -/

theorem Value.setChild_ok (P : Pointer → Prop) (old : Value) (e : PathElem) (new r : Value)
    (ho : Value.Ok P old) (hn : Value.Ok P new) (h : old.setChild e new = Outcome.ok r) :
    Value.Ok P r := by
  cases ho with
  | unit => cases e <;> exact undefined_ne_ok h
  | bool b => cases e <;> exact undefined_ne_ok h
  | int ty x => cases e <;> exact undefined_ne_ok h
  | ref p hp => cases e <;> exact undefined_ne_ok h
  | dead => cases e <;> exact undefined_ne_ok h
  | adt variant fields hf =>
    cases e with
    | field i =>
      exact Value.Ok.of_eq (ite_ok h).symm
        (Value.Ok.adt variant (setNth fields i new) (fun y hy =>
          (mem_setNth fields i new y hy).elim (fun ey => Value.Ok.of_eq ey hn) (hf y)))
    | downcast want => exact Value.Ok.of_eq (ite_ok h).symm hn
    | index i => exact undefined_ne_ok h
  | array elems hf =>
    cases e with
    | field i => exact undefined_ne_ok h
    | downcast want => exact undefined_ne_ok h
    | index i =>
      exact Value.Ok.of_eq (ite_ok h).symm
        (Value.Ok.array (setNth elems i new) (fun y hy =>
          (mem_setNth elems i new y hy).elim (fun ey => Value.Ok.of_eq ey hn) (hf y)))

theorem Value.update_ok (P : Pointer → Prop) :
    ∀ (path : List PathElem) (old new r : Value), Value.Ok P old → Value.Ok P new →
      old.update path new = Outcome.ok r → Value.Ok P r
  | [], _, _, _, _, hn, h => Value.Ok.of_eq (Outcome.ok.inj h).symm hn
  | e :: rest, old, new, r, ho, hn, h => by
    have h' : (Value.child old e).bind (fun inner =>
        (Value.update inner rest new).bind (fun inner' => Value.setChild old e inner')) =
          Outcome.ok r := h
    have ⟨inner, h1, r1⟩ := Outcome.bind_eq_ok _ _ _ h'
    have r1' : (Value.update inner rest new).bind (fun inner' => Value.setChild old e inner') =
        Outcome.ok r := r1
    have ⟨inner', h2, r2⟩ := Outcome.bind_eq_ok _ _ _ r1'
    have r2' : Value.setChild old e inner' = Outcome.ok r := r2
    exact Value.setChild_ok P old e inner' r ho
      (Value.update_ok P rest inner new inner' (Value.child_ok P old e inner ho h1) hn h2) r2'

theorem Slot.store_ok (P : Pointer → Prop) (v : Value) (hv : Value.Ok P v) :
    ∀ (path : List PathElem) (slot slot' : Slot), SlotOk P slot →
      slot.store path v = Outcome.ok slot' → SlotOk P slot'
  | [], _, slot', _, h => by
    have e : Slot.init v = slot' := Outcome.ok.inj h
    rw [← e]
    exact hv
  | e :: rest, Slot.init old, slot', hs, h => by
    have h' : (Value.update old (e :: rest) v).bind (fun new => Outcome.ok (Slot.init new)) =
        Outcome.ok slot' := h
    have ⟨new, h1, r1⟩ := Outcome.bind_eq_ok _ _ _ h'
    have e2 : Slot.init new = slot' := Outcome.ok.inj r1
    rw [← e2]
    exact Value.update_ok P (e :: rest) old v new hs hv h1
  | _ :: _, Slot.uninit, _, _, h => undefined_ne_ok h
  | _ :: _, Slot.moved, _, _, h => undefined_ne_ok h

theorem setFrame_ok (Q : List Slot → Prop) (id : Nat) (slots : List Slot) (hq : Q slots) :
    ∀ (l : List Frame), (∀ (fr : Frame), fr ∈ l → Q fr.locals) →
      ∀ (fr : Frame), fr ∈ setFrame l id slots → Q fr.locals
  | [], _, _, h => mem_nil_elim h
  | a :: l, hl, fr, h => by
    by_cases hc : a.id = id
    · have e : setFrame (a :: l) id slots = a.withLocals slots :: l :=
        (setFrame_cons a l id slots).trans (if_pos hc)
      rw [e] at h
      cases List.mem_cons.mp h with
      | inl here =>
        rw [here]
        exact hq
      | inr there => exact hl fr (List.mem_cons.mpr (Or.inr there))
    · have e : setFrame (a :: l) id slots = a :: setFrame l id slots :=
        (setFrame_cons a l id slots).trans (if_neg hc)
      rw [e] at h
      cases List.mem_cons.mp h with
      | inl here =>
        rw [here]
        exact hl a (List.mem_cons.mpr (Or.inl rfl))
      | inr there =>
        exact setFrame_ok Q id slots hq l (fun fr' m => hl fr' (List.mem_cons.mpr (Or.inr m))) fr
          there

theorem State.setSlots_ok {P : Pointer → Prop} {st : State} (hm : MemOk P st) (f : Nat)
    (slots : List Slot) (hs : SlotsOk P slots) : MemOk P (st.setSlots f slots) := by
  by_cases hf : f = 0
  · have e : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_pos hf)
    rw [e]
    exact And.intro hs (And.right hm)
  · have e : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_neg hf)
    rw [e]
    exact And.intro (And.left hm) (setFrame_ok (SlotsOk P) f slots hs st.stack (And.right hm))

theorem State.setSlot_ok {P : Pointer → Prop} {st : State} (hm : MemOk P st) (p : Pointer)
    (slot : Slot) (hs : SlotOk P slot) : MemOk P (st.setSlot p slot) := by
  have e : st.setSlot p slot = st.setSlotIn p slot (st.slots? p.frame) := rfl
  rw [e]
  cases hsl : st.slots? p.frame with
  | none => exact hm
  | some slots =>
    have old : SlotsOk P slots := State.slots?_ok hm p.frame slots hsl
    exact State.setSlots_ok hm p.frame (setNth slots p.slot slot) (fun s m =>
      (mem_setNth slots p.slot slot s m).elim
        (fun es => by
          rw [es]
          exact hs)
        (old s))

/-- A write of a value whose pointers satisfy `P` keeps the invariant. -/
theorem State.write_ok {P : Pointer → Prop} {st : State} (hm : MemOk P st) (p : Pointer)
    (v : Value) (hv : Value.Ok P v) (st' : State) (h : st.write p v = Outcome.ok st') :
    MemOk P st' := by
  have h' : (st.slot p).bind (fun slot =>
      (slot.store p.path v).bind (fun slot' => Outcome.ok (st.setSlot p slot'))) =
        Outcome.ok st' := h
  have ⟨slot, h1, r1⟩ := Outcome.bind_eq_ok _ _ _ h'
  have r1' : (slot.store p.path v).bind (fun slot' => Outcome.ok (st.setSlot p slot')) =
      Outcome.ok st' := r1
  have ⟨slot', h2, r2⟩ := Outcome.bind_eq_ok _ _ _ r1'
  have e : st.setSlot p slot' = st' := Outcome.ok.inj r2
  rw [← e]
  exact State.setSlot_ok hm p slot'
    (Slot.store_ok P v hv p.path slot slot' (State.slot_ok hm p slot h1) h2)

end Blaze.Mir

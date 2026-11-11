import Blaze.Mir.State
import Blaze.Mir.Opt.Bind

/-!
Replacing slots: the algebra dead-store elimination's proof computes with (fifth piece,
first part).

A dead store leaves the two programs' states the same but in one slot. That is stated with a
map, not a relation: the one state is the other with that slot replaced (`State.setSlot`).
So the proof needs how replacements compose: two replacements of different slots commute
(`State.setSlot_comm`), and a replacement after one of the same slot is the later one
(`State.setSlot_setSlot`); and with them, a write elsewhere commutes with the replacement
(`State.write_setSlot_other`) and a write of the whole slot forgets it
(`State.write_setSlot_same`).
-/

namespace Blaze.Mir

/-! Lists. -/

theorem setNth_comm {α : Type} :
    ∀ (l : List α) (i j : Nat) (a b : α), ¬ i = j →
      setNth (setNth l i a) j b = setNth (setNth l j b) i a
  | [], _, _, _, _, _ => rfl
  | _ :: _, 0, 0, _, _, h => absurd rfl h
  | _ :: _, 0, _ + 1, _, _, _ => rfl
  | _ :: _, _ + 1, 0, _, _, _ => rfl
  | x :: l, i + 1, j + 1, a, b, h =>
    congrTo (fun t => x :: t)
      (setNth_comm l i j a b (fun e => h (congrTo (fun k => k + 1) e)))

theorem setNth_setNth {α : Type} :
    ∀ (l : List α) (i : Nat) (a b : α), setNth (setNth l i a) i b = setNth l i b
  | [], _, _, _ => rfl
  | _ :: _, 0, _, _ => rfl
  | x :: l, i + 1, a, b => congrTo (fun t => x :: t) (setNth_setNth l i a b)

theorem nth?_setNth_same {α : Type} :
    ∀ (l : List α) (i : Nat) (x : α), nth? (setNth l i x) i = (nth? l i).map (fun _ => x)
  | [], _, _ => rfl
  | _ :: _, 0, _ => rfl
  | _ :: l, i + 1, x => nth?_setNth_same l i x

/-! Frames. -/

theorem setFrame_setFrame (id : Nat) (x y : List Slot) :
    ∀ (l : List Frame), setFrame (setFrame l id x) id y = setFrame l id y := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have h' : (fr.withLocals x).id = id := h
      have e1 : setFrame (fr :: rest) id x = fr.withLocals x :: rest :=
        (setFrame_cons fr rest id x).trans (if_pos h)
      have e2 : setFrame (fr.withLocals x :: rest) id y = (fr.withLocals x).withLocals y :: rest :=
        (setFrame_cons (fr.withLocals x) rest id y).trans (if_pos h')
      have e3 : setFrame (fr :: rest) id y = fr.withLocals y :: rest :=
        (setFrame_cons fr rest id y).trans (if_pos h)
      exact (congrTo (fun t => setFrame t id y) e1).trans (e2.trans e3.symm)
    · have e1 : setFrame (fr :: rest) id x = fr :: setFrame rest id x :=
        (setFrame_cons fr rest id x).trans (if_neg h)
      have e2 : setFrame (fr :: setFrame rest id x) id y =
          fr :: setFrame (setFrame rest id x) id y :=
        (setFrame_cons fr (setFrame rest id x) id y).trans (if_neg h)
      have e3 : setFrame (fr :: rest) id y = fr :: setFrame rest id y :=
        (setFrame_cons fr rest id y).trans (if_neg h)
      exact (congrTo (fun t => setFrame t id y) e1).trans
        (e2.trans ((congrTo (fun t => fr :: t) ih).trans e3.symm))

theorem setFrame_comm (id id' : Nat) (hne : ¬ id = id') (x y : List Slot) :
    ∀ (l : List Frame),
      setFrame (setFrame l id x) id' y = setFrame (setFrame l id' y) id x := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have h2 : ¬ fr.id = id' := fun e => hne (h.symm.trans e)
      have h2' : ¬ (fr.withLocals x).id = id' := h2
      have a1 : setFrame (fr :: rest) id x = fr.withLocals x :: rest :=
        (setFrame_cons fr rest id x).trans (if_pos h)
      have a2 : setFrame (fr.withLocals x :: rest) id' y =
          fr.withLocals x :: setFrame rest id' y :=
        (setFrame_cons (fr.withLocals x) rest id' y).trans (if_neg h2')
      have b1 : setFrame (fr :: rest) id' y = fr :: setFrame rest id' y :=
        (setFrame_cons fr rest id' y).trans (if_neg h2)
      have b2 : setFrame (fr :: setFrame rest id' y) id x =
          fr.withLocals x :: setFrame rest id' y :=
        (setFrame_cons fr (setFrame rest id' y) id x).trans (if_pos h)
      exact ((congrTo (fun t => setFrame t id' y) a1).trans a2).trans
        ((congrTo (fun t => setFrame t id x) b1).trans b2).symm
    · by_cases h2 : fr.id = id'
      · have h' : ¬ (fr.withLocals y).id = id := h
        have a1 : setFrame (fr :: rest) id x = fr :: setFrame rest id x :=
          (setFrame_cons fr rest id x).trans (if_neg h)
        have a2 : setFrame (fr :: setFrame rest id x) id' y =
            fr.withLocals y :: setFrame rest id x :=
          (setFrame_cons fr (setFrame rest id x) id' y).trans (if_pos h2)
        have b1 : setFrame (fr :: rest) id' y = fr.withLocals y :: rest :=
          (setFrame_cons fr rest id' y).trans (if_pos h2)
        have b2 : setFrame (fr.withLocals y :: rest) id x =
            fr.withLocals y :: setFrame rest id x :=
          (setFrame_cons (fr.withLocals y) rest id x).trans (if_neg h')
        exact ((congrTo (fun t => setFrame t id' y) a1).trans a2).trans
          ((congrTo (fun t => setFrame t id x) b1).trans b2).symm
      · have a1 : setFrame (fr :: rest) id x = fr :: setFrame rest id x :=
          (setFrame_cons fr rest id x).trans (if_neg h)
        have a2 : setFrame (fr :: setFrame rest id x) id' y =
            fr :: setFrame (setFrame rest id x) id' y :=
          (setFrame_cons fr (setFrame rest id x) id' y).trans (if_neg h2)
        have b1 : setFrame (fr :: rest) id' y = fr :: setFrame rest id' y :=
          (setFrame_cons fr rest id' y).trans (if_neg h2)
        have b2 : setFrame (fr :: setFrame rest id' y) id x =
            fr :: setFrame (setFrame rest id' y) id x :=
          (setFrame_cons fr (setFrame rest id' y) id x).trans (if_neg h)
        exact ((congrTo (fun t => setFrame t id' y) a1).trans a2).trans
          ((congrTo (fun t => fr :: t) ih).trans
            ((congrTo (fun t => setFrame t id x) b1).trans b2).symm)

/-! The slots of a frame, replaced. -/

theorem State.setSlots_zero (st : State) (f : Nat) (hf : f = 0) (slots : List Slot) :
    st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
  (State.setSlots_def st f slots).trans (if_pos hf)

theorem State.setSlots_frame (st : State) (f : Nat) (hf : ¬ f = 0) (slots : List Slot) :
    st.setSlots f slots =
      { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
  (State.setSlots_def st f slots).trans (if_neg hf)

theorem State.setSlots_setSlots (st : State) (f : Nat) (x y : List Slot) :
    (st.setSlots f x).setSlots f y = st.setSlots f y := by
  by_cases hf : f = 0
  · have e1 := State.setSlots_zero st f hf x
    have e2 := State.setSlots_zero st f hf y
    have e3 := State.setSlots_zero { base := x, stack := st.stack, next := st.next } f hf y
    exact (congrTo (fun s => State.setSlots s f y) e1).trans (e3.trans e2.symm)
  · have e1 := State.setSlots_frame st f hf x
    have e2 := State.setSlots_frame st f hf y
    have e3 := State.setSlots_frame
      { base := st.base, stack := setFrame st.stack f x, next := st.next } f hf y
    have mid : State.mk st.base (setFrame (setFrame st.stack f x) f y) st.next =
        State.mk st.base (setFrame st.stack f y) st.next :=
      congrTo (fun s => State.mk st.base s st.next) (setFrame_setFrame f x y st.stack)
    exact (congrTo (fun s => State.setSlots s f y) e1).trans (e3.trans (mid.trans e2.symm))

theorem State.setSlots_comm (st : State) (f f' : Nat) (hne : ¬ f = f') (x y : List Slot) :
    (st.setSlots f x).setSlots f' y = (st.setSlots f' y).setSlots f x := by
  by_cases hf : f = 0
  · have hf' : ¬ f' = 0 := fun e => hne (hf.trans e.symm)
    have a1 := State.setSlots_zero st f hf x
    have a2 := State.setSlots_frame { base := x, stack := st.stack, next := st.next } f' hf' y
    have b1 := State.setSlots_frame st f' hf' y
    have b2 := State.setSlots_zero
      { base := st.base, stack := setFrame st.stack f' y, next := st.next } f hf x
    exact ((congrTo (fun s => State.setSlots s f' y) a1).trans a2).trans
      ((congrTo (fun s => State.setSlots s f x) b1).trans b2).symm
  · by_cases hf' : f' = 0
    · have a1 := State.setSlots_frame st f hf x
      have a2 := State.setSlots_zero
        { base := st.base, stack := setFrame st.stack f x, next := st.next } f' hf' y
      have b1 := State.setSlots_zero st f' hf' y
      have b2 := State.setSlots_frame { base := y, stack := st.stack, next := st.next } f hf x
      exact ((congrTo (fun s => State.setSlots s f' y) a1).trans a2).trans
        ((congrTo (fun s => State.setSlots s f x) b1).trans b2).symm
    · have a1 := State.setSlots_frame st f hf x
      have a2 := State.setSlots_frame
        { base := st.base, stack := setFrame st.stack f x, next := st.next } f' hf' y
      have b1 := State.setSlots_frame st f' hf' y
      have b2 := State.setSlots_frame
        { base := st.base, stack := setFrame st.stack f' y, next := st.next } f hf x
      have mid : State.mk st.base (setFrame (setFrame st.stack f x) f' y) st.next =
          State.mk st.base (setFrame (setFrame st.stack f' y) f x) st.next :=
        congrTo (fun s => State.mk st.base s st.next) (setFrame_comm f f' hne x y st.stack)
      exact ((congrTo (fun s => State.setSlots s f' y) a1).trans a2).trans
        (mid.trans ((congrTo (fun s => State.setSlots s f x) b1).trans b2).symm)

/-! One slot, replaced. -/

theorem State.setSlot_some (st : State) (p : Pointer) (slot : Slot) (slots : List Slot)
    (h : st.slots? p.frame = some slots) :
    st.setSlot p slot = st.setSlots p.frame (setNth slots p.slot slot) :=
  congrTo (st.setSlotIn p slot) h

theorem State.setSlot_none (st : State) (p : Pointer) (slot : Slot)
    (h : st.slots? p.frame = none) : st.setSlot p slot = st :=
  congrTo (st.setSlotIn p slot) h

/-- The slots of the frame a replacement was made in. -/
theorem State.slots?_setSlots_some (st : State) (f : Nat) (slots new : List Slot)
    (h : st.slots? f = some slots) : (st.setSlots f new).slots? f = some new :=
  (State.slots?_setSlots_same st f new).trans (congrTo (Option.map (fun _ => new)) h)

/-- A replacement after one of the same slot is the later one. -/
theorem State.setSlot_setSlot (st : State) (p : Pointer) (a b : Slot) :
    (st.setSlot p a).setSlot p b = st.setSlot p b := by
  cases hs : st.slots? p.frame with
  | none =>
    have e : st.setSlot p a = st := State.setSlot_none st p a hs
    exact congrTo (fun s => State.setSlot s p b) e
  | some slots =>
    have e1 : st.setSlot p a = st.setSlots p.frame (setNth slots p.slot a) :=
      State.setSlot_some st p a slots hs
    have hs' : (st.setSlots p.frame (setNth slots p.slot a)).slots? p.frame =
        some (setNth slots p.slot a) :=
      State.slots?_setSlots_some st p.frame slots (setNth slots p.slot a) hs
    have e2 : (st.setSlots p.frame (setNth slots p.slot a)).setSlot p b =
        (st.setSlots p.frame (setNth slots p.slot a)).setSlots p.frame
          (setNth (setNth slots p.slot a) p.slot b) :=
      State.setSlot_some _ p b (setNth slots p.slot a) hs'
    have e3 : (st.setSlots p.frame (setNth slots p.slot a)).setSlots p.frame
          (setNth (setNth slots p.slot a) p.slot b) =
        st.setSlots p.frame (setNth (setNth slots p.slot a) p.slot b) :=
      State.setSlots_setSlots st p.frame _ _
    have e4 : st.setSlots p.frame (setNth (setNth slots p.slot a) p.slot b) =
        st.setSlots p.frame (setNth slots p.slot b) :=
      congrTo (State.setSlots st p.frame) (setNth_setNth slots p.slot a b)
    have e5 : st.setSlot p b = st.setSlots p.frame (setNth slots p.slot b) :=
      State.setSlot_some st p b slots hs
    exact (congrTo (fun s => State.setSlot s p b) e1).trans
      (e2.trans (e3.trans (e4.trans e5.symm)))

/-- Replacements of two different slots of one frame commute. -/
theorem State.setSlot_comm_frame (st : State) (p q : Pointer) (a b : Slot)
    (hf : p.frame = q.frame) (hs : ¬ p.slot = q.slot) :
    (st.setSlot p a).setSlot q b = (st.setSlot q b).setSlot p a := by
  cases hp : st.slots? p.frame with
  | none =>
    have hq : st.slots? q.frame = none := (congrTo st.slots? hf).symm.trans hp
    have e1 : st.setSlot p a = st := State.setSlot_none st p a hp
    have e2 : st.setSlot q b = st := State.setSlot_none st q b hq
    exact (congrTo (fun s => State.setSlot s q b) e1).trans
      (e2.trans ((congrTo (fun s => State.setSlot s p a) e2).trans e1).symm)
  | some slots =>
    have hq : st.slots? q.frame = some slots := (congrTo st.slots? hf).symm.trans hp
    have p1 : st.setSlot p a = st.setSlots p.frame (setNth slots p.slot a) :=
      State.setSlot_some st p a slots hp
    have q1 : st.setSlot q b = st.setSlots q.frame (setNth slots q.slot b) :=
      State.setSlot_some st q b slots hq
    have hq' : (st.setSlots p.frame (setNth slots p.slot a)).slots? q.frame =
        some (setNth slots p.slot a) := by
      rw [← hf]
      exact State.slots?_setSlots_some st p.frame slots (setNth slots p.slot a) hp
    have hp' : (st.setSlots q.frame (setNth slots q.slot b)).slots? p.frame =
        some (setNth slots q.slot b) := by
      rw [hf]
      exact State.slots?_setSlots_some st q.frame slots (setNth slots q.slot b) hq
    have p2 : (st.setSlots p.frame (setNth slots p.slot a)).setSlot q b =
        (st.setSlots p.frame (setNth slots p.slot a)).setSlots q.frame
          (setNth (setNth slots p.slot a) q.slot b) :=
      State.setSlot_some _ q b (setNth slots p.slot a) hq'
    have q2 : (st.setSlots q.frame (setNth slots q.slot b)).setSlot p a =
        (st.setSlots q.frame (setNth slots q.slot b)).setSlots p.frame
          (setNth (setNth slots q.slot b) p.slot a) :=
      State.setSlot_some _ p a (setNth slots q.slot b) hp'
    have left : (st.setSlot p a).setSlot q b =
        st.setSlots p.frame (setNth (setNth slots p.slot a) q.slot b) := by
      rw [p1, p2, ← hf]
      exact State.setSlots_setSlots st p.frame _ _
    have right : (st.setSlot q b).setSlot p a =
        st.setSlots p.frame (setNth (setNth slots q.slot b) p.slot a) := by
      rw [q1, q2, ← hf]
      exact State.setSlots_setSlots st p.frame _ _
    have mid : st.setSlots p.frame (setNth (setNth slots p.slot a) q.slot b) =
        st.setSlots p.frame (setNth (setNth slots q.slot b) p.slot a) :=
      congrTo (State.setSlots st p.frame) (setNth_comm slots p.slot q.slot a b hs)
    exact left.trans (mid.trans right.symm)

/-- Replacements of slots of two different frames commute. -/
theorem State.setSlot_comm_frames (st : State) (p q : Pointer) (a b : Slot)
    (hf : ¬ p.frame = q.frame) :
    (st.setSlot p a).setSlot q b = (st.setSlot q b).setSlot p a := by
  have hf' : ¬ q.frame = p.frame := fun e => hf e.symm
  cases hp : st.slots? p.frame with
  | none =>
    have e1 : st.setSlot p a = st := State.setSlot_none st p a hp
    cases hq : st.slots? q.frame with
    | none =>
      have e2 : st.setSlot q b = st := State.setSlot_none st q b hq
      exact (congrTo (fun s => State.setSlot s q b) e1).trans
        (e2.trans ((congrTo (fun s => State.setSlot s p a) e2).trans e1).symm)
    | some sq =>
      have q1 : st.setSlot q b = st.setSlots q.frame (setNth sq q.slot b) :=
        State.setSlot_some st q b sq hq
      have hp' : (st.setSlots q.frame (setNth sq q.slot b)).slots? p.frame = none :=
        (State.slots?_setSlots_ne st q.frame p.frame (setNth sq q.slot b) hf').trans hp
      have q2 : (st.setSlots q.frame (setNth sq q.slot b)).setSlot p a =
          st.setSlots q.frame (setNth sq q.slot b) :=
        State.setSlot_none _ p a hp'
      exact (congrTo (fun s => State.setSlot s q b) e1).trans
        ((congrTo (fun s => State.setSlot s p a) q1).trans (q2.trans q1.symm)).symm
  | some sp =>
    have p1 : st.setSlot p a = st.setSlots p.frame (setNth sp p.slot a) :=
      State.setSlot_some st p a sp hp
    cases hq : st.slots? q.frame with
    | none =>
      have e2 : st.setSlot q b = st := State.setSlot_none st q b hq
      have hq' : (st.setSlots p.frame (setNth sp p.slot a)).slots? q.frame = none :=
        (State.slots?_setSlots_ne st p.frame q.frame (setNth sp p.slot a) hf).trans hq
      have p2 : (st.setSlots p.frame (setNth sp p.slot a)).setSlot q b =
          st.setSlots p.frame (setNth sp p.slot a) :=
        State.setSlot_none _ q b hq'
      exact ((congrTo (fun s => State.setSlot s q b) p1).trans (p2.trans p1.symm)).trans
        (congrTo (fun s => State.setSlot s p a) e2).symm
    | some sq =>
      have q1 : st.setSlot q b = st.setSlots q.frame (setNth sq q.slot b) :=
        State.setSlot_some st q b sq hq
      have hq' : (st.setSlots p.frame (setNth sp p.slot a)).slots? q.frame = some sq :=
        (State.slots?_setSlots_ne st p.frame q.frame (setNth sp p.slot a) hf).trans hq
      have hp' : (st.setSlots q.frame (setNth sq q.slot b)).slots? p.frame = some sp :=
        (State.slots?_setSlots_ne st q.frame p.frame (setNth sq q.slot b) hf').trans hp
      have p2 : (st.setSlots p.frame (setNth sp p.slot a)).setSlot q b =
          (st.setSlots p.frame (setNth sp p.slot a)).setSlots q.frame (setNth sq q.slot b) :=
        State.setSlot_some _ q b sq hq'
      have q2 : (st.setSlots q.frame (setNth sq q.slot b)).setSlot p a =
          (st.setSlots q.frame (setNth sq q.slot b)).setSlots p.frame (setNth sp p.slot a) :=
        State.setSlot_some _ p a sp hp'
      have mid := State.setSlots_comm st p.frame q.frame hf (setNth sp p.slot a)
        (setNth sq q.slot b)
      exact ((congrTo (fun s => State.setSlot s q b) p1).trans p2).trans
        (mid.trans ((congrTo (fun s => State.setSlot s p a) q1).trans q2).symm)

/-- Replacements of two different slots commute. -/
theorem State.setSlot_comm (st : State) (p q : Pointer) (a b : Slot)
    (hne : ¬ p.frame = q.frame ∨ ¬ p.slot = q.slot) :
    (st.setSlot p a).setSlot q b = (st.setSlot q b).setSlot p a := by
  by_cases hf : p.frame = q.frame
  · exact State.setSlot_comm_frame st p q a b hf
      (hne.elim (fun h => absurd hf h) (fun h => h))
  · exact State.setSlot_comm_frames st p q a b hf

/-- A write to another slot commutes with the replacement. -/
theorem State.write_setSlot_other (st : State) (p q : Pointer) (slot : Slot) (v : Value)
    (hne : ¬ p.frame = q.frame ∨ ¬ p.slot = q.slot) :
    (st.setSlot p slot).write q v =
      Outcome.map (fun s => State.setSlot s p slot) (st.write q v) :=
  Outcome.bind_commutes (State.slot_setSlot_other st p q slot hne) (fun _ _ =>
    Outcome.bind_commutes rfl (fun slot' _ =>
      congrTo Outcome.ok (State.setSlot_comm st p q slot slot' hne)))

/-- A write of the whole slot forgets what the slot was replaced by. -/
theorem State.write_setSlot_same (st : State) (f l : Nat) (slot : Slot) (v : Value) :
    (st.setSlot { frame := f, slot := l, path := [] } slot).write
        { frame := f, slot := l, path := [] } v =
      st.write { frame := f, slot := l, path := [] } v := by
  cases hs : st.slots? f with
  | none =>
    have e : st.setSlot { frame := f, slot := l, path := [] } slot = st :=
      State.setSlot_none st { frame := f, slot := l, path := [] } slot hs
    exact congrTo (fun s => State.write s { frame := f, slot := l, path := [] } v) e
  | some slots =>
    have e1 : st.setSlot { frame := f, slot := l, path := [] } slot =
        st.setSlots f (setNth slots l slot) :=
      State.setSlot_some st { frame := f, slot := l, path := [] } slot slots hs
    have hs' : (st.setSlots f (setNth slots l slot)).slots? f = some (setNth slots l slot) :=
      State.slots?_setSlots_some st f slots (setNth slots l slot) hs
    have over : ∀ (new : Slot),
        (st.setSlot { frame := f, slot := l, path := [] } slot).setSlot
            { frame := f, slot := l, path := [] } new =
          st.setSlot { frame := f, slot := l, path := [] } new :=
      fun new => State.setSlot_setSlot st { frame := f, slot := l, path := [] } slot new
    have slotNew : (st.setSlot { frame := f, slot := l, path := [] } slot).slot
          { frame := f, slot := l, path := [] } =
        slotOf ((nth? slots l).map (fun _ => slot)) := by
      rw [e1]
      show slotIn l ((st.setSlots f (setNth slots l slot)).slots? f) = _
      rw [hs']
      exact congrTo slotOf (nth?_setNth_same slots l slot)
    have slotOld : st.slot { frame := f, slot := l, path := [] } = slotOf (nth? slots l) :=
      congrTo (slotIn l) hs
    have new : (st.setSlot { frame := f, slot := l, path := [] } slot).write
          { frame := f, slot := l, path := [] } v =
        (slotOf ((nth? slots l).map (fun _ => slot))).bind (fun _ =>
          Outcome.ok (st.setSlot { frame := f, slot := l, path := [] } (Slot.init v))) :=
      Outcome.bind_congr slotNew (fun _ _ => congrTo Outcome.ok (over (Slot.init v)))
    have old : st.write { frame := f, slot := l, path := [] } v =
        (slotOf (nth? slots l)).bind (fun _ =>
          Outcome.ok (st.setSlot { frame := f, slot := l, path := [] } (Slot.init v))) :=
      Outcome.bind_congr slotOld (fun _ _ => rfl)
    rw [new, old]
    cases nth? slots l with
    | none => rfl
    | some s => rfl

end Blaze.Mir

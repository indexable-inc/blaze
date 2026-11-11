import Blaze.Mir.Syntax
import Blaze.Mir.Value

/-!
The machine's state: the frames of the calls in progress, and under them the slots the entry
function's reference arguments point to (frame 0).

A frame is named by an id from a counter (`State.next`), never by its depth, so a reference
into a frame that returned is dangling for the rest of the run.

Proved here, the frame lemma: a write at a place leaves the read of every place in another
slot as it was (`State.read_write_other`). It is what a pass that removes or moves a write
rests on.
-/

namespace Blaze.Mir

structure Frame where
  /-- Its id: what a `Pointer` into it names. -/
  id : Nat
  fn : FnId
  /-- The block to run next. -/
  block : BlockId
  locals : List Slot
  /-- Where the caller wants what this call returns. -/
  dest : Pointer
  /-- The caller's block to go on at. -/
  target : Option BlockId

structure State where
  base : List Slot
  /-- The frames, the running one first. -/
  stack : List Frame
  /-- The id the next call's frame gets. -/
  next : Nat

def Frame.withLocals (fr : Frame) (locals : List Slot) : Frame :=
  { id := fr.id, fn := fr.fn, block := fr.block, locals := locals, dest := fr.dest,
    target := fr.target }

def Frame.withBlock (fr : Frame) (block : BlockId) : Frame :=
  { id := fr.id, fn := fr.fn, block := block, locals := fr.locals, dest := fr.dest,
    target := fr.target }

/-- The frame with this id. -/
def findFrame : List Frame → Nat → Option Frame
  | [], _ => none
  | fr :: rest, id => if fr.id = id then some fr else findFrame rest id

/-- The frames with the locals of the frame with this id replaced. -/
def setFrame : List Frame → Nat → List Slot → List Frame
  | [], _, _ => []
  | fr :: rest, id, slots =>
    if fr.id = id then fr.withLocals slots :: rest else fr :: setFrame rest id slots

theorem findFrame_cons (fr : Frame) (rest : List Frame) (id : Nat) :
    findFrame (fr :: rest) id = if fr.id = id then some fr else findFrame rest id := rfl

theorem setFrame_cons (fr : Frame) (rest : List Frame) (id : Nat) (slots : List Slot) :
    setFrame (fr :: rest) id slots =
      if fr.id = id then fr.withLocals slots :: rest else fr :: setFrame rest id slots := rfl

theorem findFrame_setFrame_ne (id id' : Nat) (slots : List Slot) (hne : ¬ id = id') :
    ∀ (l : List Frame), findFrame (setFrame l id slots) id' = findFrame l id' := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    rw [setFrame_cons]
    by_cases h : fr.id = id
    · rw [if_pos h, findFrame_cons, findFrame_cons]
      have hne1 : ¬ fr.id = id' := fun h' => hne (h.symm.trans h')
      have hne2 : ¬ (fr.withLocals slots).id = id' := hne1
      exact (if_neg hne2).trans (if_neg hne1).symm
    · rw [if_neg h, findFrame_cons, findFrame_cons, ih]

theorem findFrame_setFrame_same (id : Nat) (slots : List Slot) :
    ∀ (l : List Frame),
      findFrame (setFrame l id slots) id =
        (findFrame l id).map (fun fr => fr.withLocals slots) := by
  intro l
  induction l with
  | nil => rfl
  | cons fr rest ih =>
    by_cases h : fr.id = id
    · have e : setFrame (fr :: rest) id slots = fr.withLocals slots :: rest :=
        (setFrame_cons fr rest id slots).trans (if_pos h)
      have h2 : (fr.withLocals slots).id = id := h
      have left : findFrame (fr.withLocals slots :: rest) id = some (fr.withLocals slots) :=
        (findFrame_cons (fr.withLocals slots) rest id).trans (if_pos h2)
      have right : findFrame (fr :: rest) id = some fr :=
        (findFrame_cons fr rest id).trans (if_pos h)
      rw [e, left, right]
      try rfl
    · have e : setFrame (fr :: rest) id slots = fr :: setFrame rest id slots :=
        (setFrame_cons fr rest id slots).trans (if_neg h)
      have left : findFrame (fr :: setFrame rest id slots) id =
          findFrame (setFrame rest id slots) id :=
        (findFrame_cons fr (setFrame rest id slots) id).trans (if_neg h)
      have right : findFrame (fr :: rest) id = findFrame rest id :=
        (findFrame_cons fr rest id).trans (if_neg h)
      rw [e, left, right]
      exact ih

/-- The running frame's id, as a `Pointer` names it. -/
def State.top (st : State) : Nat :=
  match st.stack with
  | [] => 0
  | fr :: _ => fr.id

/-- The slots of the frame with this id. -/
def State.slots? (st : State) (frame : Nat) : Option (List Slot) :=
  if frame = 0 then some st.base else (findFrame st.stack frame).map (fun fr => fr.locals)

/-- The state with that frame's slots replaced. -/
def State.setSlots (st : State) (frame : Nat) (slots : List Slot) : State :=
  if frame = 0 then { base := slots, stack := st.stack, next := st.next }
  else { base := st.base, stack := setFrame st.stack frame slots, next := st.next }

theorem State.slots?_def (st : State) (frame : Nat) :
    st.slots? frame =
      if frame = 0 then some st.base
      else (findFrame st.stack frame).map (fun fr => fr.locals) := rfl

theorem State.setSlots_def (st : State) (frame : Nat) (slots : List Slot) :
    st.setSlots frame slots =
      if frame = 0 then { base := slots, stack := st.stack, next := st.next }
      else { base := st.base, stack := setFrame st.stack frame slots, next := st.next } := rfl

def slotOf : Option Slot → Outcome Slot
  | none => Outcome.undefined Undefined.illFormed
  | some slot => Outcome.ok slot

def slotIn (slot : Nat) : Option (List Slot) → Outcome Slot
  | none => Outcome.undefined Undefined.dangling
  | some slots => slotOf (nth? slots slot)

/-- The slot a pointer names. A frame that is not there (it returned) is `dangling`. -/
def State.slot (st : State) (p : Pointer) : Outcome Slot :=
  slotIn p.slot (st.slots? p.frame)

/-- The value at a pointer. -/
def State.read (st : State) (p : Pointer) : Outcome Value :=
  (st.slot p).bind (fun slot =>
    slot.load.bind (fun v => (v.project p.path).bind Value.checkLive))

def State.setSlotIn (st : State) (p : Pointer) (slot : Slot) : Option (List Slot) → State
  | some slots => st.setSlots p.frame (setNth slots p.slot slot)
  | none => st

/-- The state with the slot a pointer names replaced. -/
def State.setSlot (st : State) (p : Pointer) (slot : Slot) : State :=
  st.setSlotIn p slot (st.slots? p.frame)

/-- The state after a write of `v` at a pointer. -/
def State.write (st : State) (p : Pointer) (v : Value) : Outcome State :=
  (st.slot p).bind (fun slot =>
    (slot.store p.path v).bind (fun slot' => Outcome.ok (st.setSlot p slot')))

def stateOr (st : State) : Outcome State → State
  | Outcome.ok st' => st'
  | _ => st

/-- The state after a move out of a pointer: a whole local becomes `moved`; a part becomes
the hole `Value.dead`, so a read of the part after is undefined. -/
def State.moveOut (st : State) (p : Pointer) : State :=
  match p.path with
  | [] => st.setSlot p Slot.moved
  | _ :: _ => stateOr st (st.write p Value.dead)

/-! The frame lemma. -/

theorem State.slots?_setSlots_ne (st : State) (f f' : Nat) (slots : List Slot)
    (hne : ¬ f = f') : (st.setSlots f slots).slots? f' = st.slots? f' := by
  by_cases hf : f = 0
  · have e : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_pos hf)
    have hf' : ¬ f' = 0 := fun h0 => hne (hf.trans h0.symm)
    have left : State.slots? { base := slots, stack := st.stack, next := st.next } f' =
        (findFrame st.stack f').map (fun fr => fr.locals) :=
      (State.slots?_def _ f').trans (if_neg hf')
    have right : st.slots? f' = (findFrame st.stack f').map (fun fr => fr.locals) :=
      (State.slots?_def st f').trans (if_neg hf')
    rw [e, left, right]
  · have e : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_neg hf)
    by_cases hf' : f' = 0
    · have left :
          State.slots? { base := st.base, stack := setFrame st.stack f slots, next := st.next }
            f' = some st.base := (State.slots?_def _ f').trans (if_pos hf')
      have right : st.slots? f' = some st.base := (State.slots?_def st f').trans (if_pos hf')
      rw [e, left, right]
    · have left :
          State.slots? { base := st.base, stack := setFrame st.stack f slots, next := st.next }
            f' = (findFrame (setFrame st.stack f slots) f').map (fun fr => fr.locals) :=
        (State.slots?_def _ f').trans (if_neg hf')
      have right : st.slots? f' = (findFrame st.stack f').map (fun fr => fr.locals) :=
        (State.slots?_def st f').trans (if_neg hf')
      rw [e, left, right, findFrame_setFrame_ne f f' slots hne st.stack]

theorem State.slots?_setSlots_same (st : State) (f : Nat) (slots : List Slot) :
    (st.setSlots f slots).slots? f = (st.slots? f).map (fun _ => slots) := by
  by_cases hf : f = 0
  · have e : st.setSlots f slots = { base := slots, stack := st.stack, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_pos hf)
    have left : State.slots? { base := slots, stack := st.stack, next := st.next } f =
        some slots := (State.slots?_def _ f).trans (if_pos hf)
    have right : st.slots? f = some st.base := (State.slots?_def st f).trans (if_pos hf)
    rw [e, left, right]
    try rfl
  · have e : st.setSlots f slots =
        { base := st.base, stack := setFrame st.stack f slots, next := st.next } :=
      (State.setSlots_def st f slots).trans (if_neg hf)
    have left :
        State.slots? { base := st.base, stack := setFrame st.stack f slots, next := st.next } f =
          (findFrame (setFrame st.stack f slots) f).map (fun fr => fr.locals) :=
      (State.slots?_def _ f).trans (if_neg hf)
    have right : st.slots? f = (findFrame st.stack f).map (fun fr => fr.locals) :=
      (State.slots?_def st f).trans (if_neg hf)
    rw [e, left, right, findFrame_setFrame_same f slots st.stack]
    cases findFrame st.stack f with
    | none => rfl
    | some fr => rfl

/-- Replacing a slot leaves every other slot, of any frame, as it was. -/
theorem State.slot_setSlot_other (st : State) (p q : Pointer) (slot : Slot)
    (hne : ¬ p.frame = q.frame ∨ ¬ p.slot = q.slot) :
    (st.setSlot p slot).slot q = st.slot q := by
  cases hsl : st.slots? p.frame with
  | none =>
    have e : st.setSlot p slot = st := congrArg (st.setSlotIn p slot) hsl
    rw [e]
  | some slots =>
    have e : st.setSlot p slot = st.setSlots p.frame (setNth slots p.slot slot) :=
      congrArg (st.setSlotIn p slot) hsl
    rw [e]
    by_cases hf : p.frame = q.frame
    · have hs : ¬ p.slot = q.slot := fun h => hne.elim (fun h' => h' hf) (fun h' => h' h)
      have h1a : (st.setSlots p.frame (setNth slots p.slot slot)).slots? p.frame =
          (st.slots? p.frame).map (fun _ => setNth slots p.slot slot) :=
        State.slots?_setSlots_same st p.frame (setNth slots p.slot slot)
      have h1b : (st.slots? p.frame).map (fun _ => setNth slots p.slot slot) =
          some (setNth slots p.slot slot) :=
        congrArg (Option.map (fun _ => setNth slots p.slot slot)) hsl
      have h1 := h1a.trans h1b
      show slotIn q.slot ((st.setSlots p.frame (setNth slots p.slot slot)).slots? q.frame) =
        slotIn q.slot (st.slots? q.frame)
      rw [← hf, h1, hsl]
      exact congrArg slotOf (nth?_setNth_ne slots p.slot q.slot slot hs)
    · have h1 := State.slots?_setSlots_ne st p.frame q.frame (setNth slots p.slot slot) hf
      show slotIn q.slot ((st.setSlots p.frame (setNth slots p.slot slot)).slots? q.frame) =
        slotIn q.slot (st.slots? q.frame)
      rw [h1]

/-- The frame lemma: a write at `p` leaves the read of every place in another slot as it
was. Two places in one slot (two fields of one local) are not covered. -/
theorem State.read_write_other (st st' : State) (p q : Pointer) (v : Value)
    (hw : st.write p v = Outcome.ok st')
    (hne : ¬ p.frame = q.frame ∨ ¬ p.slot = q.slot) : st'.read q = st.read q := by
  have hw' : (st.slot p).bind (fun slot =>
      (slot.store p.path v).bind (fun slot' => Outcome.ok (st.setSlot p slot'))) =
        Outcome.ok st' := hw
  have ⟨slot, _, h1⟩ := Outcome.bind_eq_ok _ _ _ hw'
  have h1' : (slot.store p.path v).bind (fun slot' => Outcome.ok (st.setSlot p slot')) =
      Outcome.ok st' := h1
  have ⟨slot', _, h2⟩ := Outcome.bind_eq_ok _ _ _ h1'
  have h2' : Outcome.ok (st.setSlot p slot') = Outcome.ok st' := h2
  have h3 : st.setSlot p slot' = st' := Outcome.ok.inj h2'
  have hs := State.slot_setSlot_other st p q slot' hne
  rw [← h3]
  show ((st.setSlot p slot').slot q).bind (fun slot =>
      slot.load.bind (fun v => (v.project q.path).bind Value.checkLive)) =
    (st.slot q).bind (fun slot =>
      slot.load.bind (fun v => (v.project q.path).bind Value.checkLive))
  rw [hs]

/-- A move out of a whole local leaves the read of every place in another slot as it was. -/
theorem State.read_setSlot_other (st : State) (p q : Pointer) (slot : Slot)
    (hne : ¬ p.frame = q.frame ∨ ¬ p.slot = q.slot) :
    (st.setSlot p slot).read q = st.read q := by
  have hs := State.slot_setSlot_other st p q slot hne
  show ((st.setSlot p slot).slot q).bind (fun slot =>
      slot.load.bind (fun v => (v.project q.path).bind Value.checkLive)) =
    (st.slot q).bind (fun slot =>
      slot.load.bind (fun v => (v.project q.path).bind Value.checkLive))
  rw [hs]

end Blaze.Mir

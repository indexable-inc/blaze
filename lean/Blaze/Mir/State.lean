import Blaze.Mir.Syntax
import Blaze.Mir.Value

/-!
The machine's state: the frames of the calls in progress, and under them the slots the entry
function's reference arguments point to.
-/

namespace Blaze.Mir

structure Frame where
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

/-- The running frame's number, as a `Pointer` names it. -/
def State.top (st : State) : Nat := st.stack.length

/-- The slots of frame `frame`. -/
def State.slots? (st : State) : Nat → Option (List Slot)
  | 0 => some st.base
  | d + 1 => (nth? st.stack.reverse d).map (fun fr => fr.locals)

def Frame.withLocals (fr : Frame) (locals : List Slot) : Frame :=
  { fn := fr.fn, block := fr.block, locals := locals, dest := fr.dest, target := fr.target }

def Frame.withBlock (fr : Frame) (block : BlockId) : Frame :=
  { fn := fr.fn, block := block, locals := fr.locals, dest := fr.dest, target := fr.target }

/-- The state with frame `frame`'s slots replaced. -/
def State.setSlots (st : State) : Nat → List Slot → State
  | 0, slots => { base := slots, stack := st.stack }
  | d + 1, slots =>
    match nth? st.stack.reverse d with
    | some fr =>
      { base := st.base, stack := (setNth st.stack.reverse d (fr.withLocals slots)).reverse }
    | none => st

/-- The slot a pointer names. -/
def State.slot (st : State) (p : Pointer) : Outcome Slot :=
  match st.slots? p.frame with
  | none => Outcome.undefined Undefined.dangling
  | some slots =>
    match nth? slots p.slot with
    | none => Outcome.undefined Undefined.illFormed
    | some slot => Outcome.ok slot

/-- The value at a pointer. -/
def State.read (st : State) (p : Pointer) : Outcome Value :=
  (st.slot p).bind (fun slot => slot.load.bind (fun v => v.project p.path))

/-- The state with frame `p.frame`'s slot `p.slot` replaced. -/
def State.setSlot (st : State) (p : Pointer) (slot : Slot) : State :=
  match st.slots? p.frame with
  | some slots => st.setSlots p.frame (setNth slots p.slot slot)
  | none => st

/-- The state after a write of `v` at a pointer. -/
def State.write (st : State) (p : Pointer) (v : Value) : Outcome State :=
  (st.slot p).bind (fun slot =>
    (slot.store p.path v).bind (fun slot' => Outcome.ok (st.setSlot p slot')))

/-- The state after a move out of a pointer: a whole local becomes `moved`; a part stays (the
borrow checker forbids its reuse; the model does not track it). -/
def State.moveOut (st : State) (p : Pointer) : State :=
  match p.path with
  | [] => st.setSlot p Slot.moved
  | _ :: _ => st

end Blaze.Mir

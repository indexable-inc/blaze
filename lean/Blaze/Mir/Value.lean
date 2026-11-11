import Blaze.Mir.Outcome
import Blaze.Scalar

/-!
The values and the memory of the first subset: structured, not bytes.

A local holds a structured value (`Slot`); a reference is a `Pointer`, a path to a place: a
frame, a local of it, and the fields and indices down to the part. This is enough for safe
code that borrows locals and their parts, and it is what the functional models read a `&mut`
as. It cannot express raw pointers, transmutes, unions, the heap (`Box`, `Vec`) or anything
that depends on layout; docs/v2/plan/design/formal-verification.md says how it grows.
-/

namespace Blaze.Mir

/-- The element at `n`, by structural recursion (so it reduces in the kernel on a list whose
spine is known). -/
def nth? {α : Type} : List α → Nat → Option α
  | [], _ => none
  | a :: _, 0 => some a
  | _ :: as, n + 1 => nth? as n

/-- The list with the element at `n` replaced; the list itself when it has none. -/
def setNth {α : Type} : List α → Nat → α → List α
  | [], _, _ => []
  | _ :: as, 0, v => v :: as
  | a :: as, n + 1, v => a :: setNth as n v

theorem nth?_map {α β : Type} (f : α → β) :
    ∀ (l : List α) (n : Nat), nth? (l.map f) n = (nth? l n).map f
  | [], _ => rfl
  | _ :: _, 0 => rfl
  | _ :: l, n + 1 => nth?_map f l n

/-- An index in bounds reads the element there. -/
theorem nth?_map_get {α β : Type} (f : α → β) :
    ∀ (l : List α) (n : Nat) (h : n < l.length), nth? (l.map f) n = some (f (l.get ⟨n, h⟩))
  | [], _, h => absurd h (Nat.not_lt_zero _)
  | _ :: _, 0, _ => rfl
  | _ :: l, n + 1, h => nth?_map_get f l n (Nat.lt_of_succ_lt_succ h)

/-- A step from a value to a part of it. -/
inductive PathElem where
  | field (i : Nat)
  /-- The value itself, which must be of this variant. -/
  | downcast (variant : Nat)
  | index (i : Nat)
  deriving DecidableEq, Repr

/-- A place: frame 0 is what the entry function's reference arguments point to, frame `d + 1`
the `d`-th frame from the bottom of the stack. -/
structure Pointer where
  frame : Nat
  slot : Nat
  path : List PathElem
  deriving DecidableEq, Repr

def Pointer.push (p : Pointer) (e : PathElem) : Pointer :=
  { frame := p.frame, slot := p.slot, path := p.path ++ [e] }

inductive Value where
  | unit
  | bool (b : Bool)
  | int (ty : ScalarTy) (v : Scalar ty)
  /-- A struct or a tuple (variant 0), or a variant of an enum. -/
  | adt (variant : Nat) (fields : List Value)
  /-- An array, or what a slice reference points to. -/
  | array (elems : List Value)
  | ref (p : Pointer)

/-- What a local holds. -/
inductive Slot where
  | uninit
  | moved
  | init (v : Value)

def found (missing : Undefined) : Option Value → Outcome Value
  | some v => Outcome.ok v
  | none => Outcome.undefined missing

/-- The part of a value one step down. -/
def Value.child : Value → PathElem → Outcome Value
  | Value.adt _ fields, PathElem.field i => found Undefined.illFormed (nth? fields i)
  | Value.adt variant fields, PathElem.downcast want =>
    if variant = want then Outcome.ok (Value.adt variant fields)
    else Outcome.undefined Undefined.illTyped
  | Value.array elems, PathElem.index i => found Undefined.dangling (nth? elems i)
  | _, _ => Outcome.undefined Undefined.illTyped

/-- The value with the part one step down replaced. -/
def Value.setChild : Value → PathElem → Value → Outcome Value
  | Value.adt variant fields, PathElem.field i, v =>
    if i < fields.length then Outcome.ok (Value.adt variant (setNth fields i v))
    else Outcome.undefined Undefined.illFormed
  | Value.adt variant _, PathElem.downcast want, v =>
    if variant = want then Outcome.ok v else Outcome.undefined Undefined.illTyped
  | Value.array elems, PathElem.index i, v =>
    if i < elems.length then Outcome.ok (Value.array (setNth elems i v))
    else Outcome.undefined Undefined.dangling
  | _, _, _ => Outcome.undefined Undefined.illTyped

/-- The part of a value at a path. -/
def Value.project : Value → List PathElem → Outcome Value
  | v, [] => Outcome.ok v
  | v, e :: rest => (Value.child v e).bind (fun inner => Value.project inner rest)

/-- The value with the part at a path replaced. -/
def Value.update : Value → List PathElem → Value → Outcome Value
  | _, [], new => Outcome.ok new
  | old, e :: rest, new =>
    (Value.child old e).bind (fun inner =>
      (Value.update inner rest new).bind (fun inner' => Value.setChild old e inner'))

/-- A slot after a write at a path: a write of the whole initializes it, whatever it held; a
write of a part needs a value there. The path is matched first, so a write of a whole slot
reduces for a slot that is a variable. -/
def Slot.store (slot : Slot) (path : List PathElem) (v : Value) : Outcome Slot :=
  match path with
  | [] => Outcome.ok (Slot.init v)
  | e :: rest =>
    match slot with
    | Slot.init old =>
      (Value.update old (e :: rest) v).bind (fun new => Outcome.ok (Slot.init new))
    | Slot.uninit => Outcome.undefined Undefined.uninit
    | Slot.moved => Outcome.undefined Undefined.moved

/-- What a read of a slot gives. -/
def Slot.load : Slot → Outcome Value
  | Slot.init v => Outcome.ok v
  | Slot.uninit => Outcome.undefined Undefined.uninit
  | Slot.moved => Outcome.undefined Undefined.moved

end Blaze.Mir

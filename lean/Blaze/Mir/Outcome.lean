import Blaze.Result

/-!
What a run of MIR ends in (docs/v2/plan/design/formal-verification.md, "The semantics of
MIR"): a value, a panic, undefined behaviour, or no answer within the fuel given.

`Blaze.Result` is the same type without `undefined`: the functional models assume a
borrow-checked safe program has none. `Outcome.ofResult` and `Outcome.toResult?` relate the two.
-/

namespace Blaze.Mir

/-- Why a run has no defined behaviour. A panic is not one of these. -/
inductive Undefined where
  /-- A read of a local nothing was assigned to. -/
  | uninit
  /-- A read of a local that was moved out of. -/
  | moved
  /-- A reference to a frame that returned, or to a part that is not there. -/
  | dangling
  /-- An operation on a value of the wrong kind: the program is not well typed. -/
  | illTyped
  /-- A block, local or function the program does not have. -/
  | illFormed
  /-- MIR's `Unreachable` was reached. -/
  | unreachable
  deriving Repr, DecidableEq

inductive Outcome (α : Type) : Type where
  | ok (v : α) : Outcome α
  | panic (e : Error) : Outcome α
  | undefined (why : Undefined) : Outcome α
  /-- Out of fuel. -/
  | diverge : Outcome α

/-- `x`, then `f` of its value; anything else ends the run as it is. -/
def Outcome.bind {α β : Type} (x : Outcome α) (f : α → Outcome β) : Outcome β :=
  match x with
  | Outcome.ok v => f v
  | Outcome.panic e => Outcome.panic e
  | Outcome.undefined why => Outcome.undefined why
  | Outcome.diverge => Outcome.diverge

theorem Outcome.bind_ok {α β : Type} (v : α) (f : α → Outcome β) :
    (Outcome.ok v).bind f = f v := rfl

/-- A bind that gives a value had a value to bind. -/
theorem Outcome.bind_eq_ok {α β : Type} (x : Outcome α) (f : α → Outcome β) (b : β)
    (h : x.bind f = Outcome.ok b) : ∃ a, x = Outcome.ok a ∧ f a = Outcome.ok b := by
  cases x with
  | ok v => exact ⟨v, rfl, h⟩
  | panic e =>
    have h' : (Outcome.panic e : Outcome β) = Outcome.ok b := h
    cases h'
  | undefined why =>
    have h' : (Outcome.undefined why : Outcome β) = Outcome.ok b := h
    cases h'
  | diverge =>
    have h' : (Outcome.diverge : Outcome β) = Outcome.ok b := h
    cases h'

def Outcome.map {α β : Type} (f : α → β) (x : Outcome α) : Outcome β :=
  x.bind (fun v => Outcome.ok (f v))

/-- A model's result as an outcome: `fail` is a panic, `div` is out of fuel. -/
def Outcome.ofResult {α : Type} : Result α → Outcome α
  | Result.ok v => Outcome.ok v
  | Result.fail e => Outcome.panic e
  | Result.div => Outcome.diverge

/-- An outcome as a model's result; undefined behaviour is none. -/
def Outcome.toResult? {α : Type} : Outcome α → Option (Result α)
  | Outcome.ok v => some (Result.ok v)
  | Outcome.panic e => some (Result.fail e)
  | Outcome.undefined _ => none
  | Outcome.diverge => some Result.div

/-- Every result is an outcome, and comes back as itself. -/
theorem Outcome.toResult?_ofResult {α : Type} (r : Result α) :
    (Outcome.ofResult r).toResult? = some r := by
  cases r <;> rfl

/-- The outcomes with a result are the ones without undefined behaviour. -/
theorem Outcome.toResult?_eq_none {α : Type} (x : Outcome α) :
    x.toResult? = none ↔ ∃ why, x = Outcome.undefined why := by
  cases x with
  | ok v => exact ⟨(fun h => nomatch h), (fun ⟨_, h⟩ => nomatch h)⟩
  | panic e => exact ⟨(fun h => nomatch h), (fun ⟨_, h⟩ => nomatch h)⟩
  | undefined why => exact ⟨fun _ => ⟨why, rfl⟩, fun _ => rfl⟩
  | diverge => exact ⟨(fun h => nomatch h), (fun ⟨_, h⟩ => nomatch h)⟩

/-- The refinement of compiler correctness, CompCert's direction: every behaviour of the
target `a` is a behaviour of the source `b`, where a source with undefined behaviour allows
any. -/
def Outcome.Refines {α : Type} (a b : Outcome α) : Prop :=
  (∃ why, b = Outcome.undefined why) ∨ a = b

theorem Outcome.Refines.of_eq {α : Type} {a b : Outcome α} (h : a = b) : a.Refines b :=
  Or.inr h

end Blaze.Mir

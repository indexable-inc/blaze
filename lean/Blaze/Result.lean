/-!
The monad of blaze's models (docs/v2/plan/design/formal-verification.md, v0): a computation
gives a value (`ok`), panics (`fail`), or runs out of the fuel its loops were given (`div`).
As Aeneas's `Result`.
-/

namespace Blaze

/-- Why a model failed: each is a panic of the Rust program. -/
inductive Error where
  /-- `panic!`, a failed `assert!`. -/
  | panic
  /-- Arithmetic overflow, a division by zero, a shift too far. -/
  | arithmetic
  /-- An index out of bounds. -/
  | bounds
  /-- MIR's `Unreachable` was reached. -/
  | unreachable
  deriving Repr, DecidableEq

inductive Result (α : Type) : Type where
  | ok (v : α) : Result α
  | fail (e : Error) : Result α
  | div : Result α
  deriving Repr

namespace Result

def bind {α β : Type} (x : Result α) (f : α → Result β) : Result β :=
  match x with
  | ok v => f v
  | fail e => fail e
  | div => div

end Result

instance : Monad Result where
  pure := Result.ok
  bind := Result.bind

@[simp] theorem Result.bind_ok {α β : Type} (v : α) (f : α → Result β) :
    (Result.ok v >>= f) = f v := rfl

@[simp] theorem Result.bind_fail {α β : Type} (e : Error) (f : α → Result β) :
    (Result.fail e >>= f) = Result.fail e := rfl

@[simp] theorem Result.bind_div {α β : Type} (f : α → Result β) :
    ((Result.div : Result α) >>= f) = Result.div := rfl

@[simp] theorem Result.pure_eq {α : Type} (v : α) : (pure v : Result α) = Result.ok v := rfl

/-- A failed assert panics. -/
def massert (b : Bool) : Result Unit :=
  match b with
  | true => Result.ok ()
  | false => Result.fail Error.panic

@[simp] theorem massert_true : massert true = Result.ok () := rfl

@[simp] theorem massert_false : massert false = Result.fail Error.panic := rfl

end Blaze

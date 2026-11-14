import Blaze.Result

/-!
The model of core's types that are not Lean's own, and of their closure-free methods. Each is
named by its Rust path under `Blaze`. `core::option::Option` is Lean's `Option`.

A method that panics in Rust gives a `Blaze.Result` and fails with `Error.panic` exactly where
it panics; `expect`'s message is not kept. A method that takes `&mut self` gives back the new
value of `self` after its result, as a translated function does.
-/

namespace Blaze.core.result

/-- `core::result::Result`. -/
inductive Result (T E : Type) : Type where
  | Ok (f0 : T) : Result T E
  | Err (f0 : E) : Result T E

end Blaze.core.result

namespace Blaze.core.num

/-- `core::num::TryFromIntError`: a `TryFrom` between integers whose value does not fit. Its one
field in core is private and `()`. -/
structure TryFromIntError : Type where

end Blaze.core.num

namespace Blaze.core.option.Option

variable {T E : Type}

@[simp] def is_some : Option T → Bool
  | .some _ => true
  | .none => false

@[simp] def is_none : Option T → Bool
  | .some _ => false
  | .none => true

/-- `Option::unwrap`: panics on `None`. -/
@[simp] def unwrap : Option T → Blaze.Result T
  | .some v => .ok v
  | .none => .fail .panic

/-- `Option::expect`: `unwrap`, with a message the model drops. -/
@[simp] def expect (o : Option T) : Blaze.Result T := unwrap o

@[simp] def unwrap_or : Option T → T → T
  | .some v, _ => v
  | .none, d => d

@[simp] def ok_or : Option T → E → Blaze.core.result.Result T E
  | .some v, _ => .Ok v
  | .none, e => .Err e

/-- `Option::take`: the value, and `None` left in its place. -/
@[simp] def take (o : Option T) : Option T × Option T := (o, .none)

/-- `Option::replace`: the value, and `Some value` left in its place. -/
@[simp] def replace (o : Option T) (value : T) : Option T × Option T := (o, .some value)

theorem unwrap_eq_ok (o : Option T) (v : T) : unwrap o = .ok v ↔ o = .some v := by
  cases o <;> simp

theorem expect_eq_ok (o : Option T) (v : T) : expect o = .ok v ↔ o = .some v :=
  unwrap_eq_ok o v

end Blaze.core.option.Option

namespace Blaze.core.result.Result

variable {T E : Type}

@[simp] def is_ok : Blaze.core.result.Result T E → Bool
  | .Ok _ => true
  | .Err _ => false

@[simp] def is_err : Blaze.core.result.Result T E → Bool
  | .Ok _ => false
  | .Err _ => true

@[simp] def ok : Blaze.core.result.Result T E → Option T
  | .Ok v => .some v
  | .Err _ => .none

@[simp] def err : Blaze.core.result.Result T E → Option E
  | .Ok _ => .none
  | .Err e => .some e

/-- `Result::unwrap`: panics on `Err`. -/
@[simp] def unwrap : Blaze.core.result.Result T E → Blaze.Result T
  | .Ok v => .ok v
  | .Err _ => .fail .panic

/-- `Result::expect`: `unwrap`, with a message the model drops. -/
@[simp] def expect (r : Blaze.core.result.Result T E) : Blaze.Result T := unwrap r

@[simp] def unwrap_or : Blaze.core.result.Result T E → T → T
  | .Ok v, _ => v
  | .Err _, d => d

theorem unwrap_eq_ok (r : Blaze.core.result.Result T E) (v : T) :
    unwrap r = .ok v ↔ r = .Ok v := by
  cases r <;> simp

theorem expect_eq_ok (r : Blaze.core.result.Result T E) (v : T) :
    expect r = .ok v ↔ r = .Ok v :=
  unwrap_eq_ok r v

end Blaze.core.result.Result

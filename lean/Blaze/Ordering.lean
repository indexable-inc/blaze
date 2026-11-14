import Blaze.ScalarOps

/-!
`core::cmp`: the `Ordering` a comparison gives, its methods, `Ord::cmp` and
`PartialOrd::partial_cmp` of the integers, and `==` and `!=` of a type whose `PartialEq` is
structural equality.

Each definition quotes the line of the pinned nightly's `library/core/src/cmp.rs` it models.

`==` here is `decide (a = b)`: the model of a `PartialEq` that is derived, or is the standard
library's on a type whose equality is its value's (integers, `bool`, `()`, tuples, slices,
arrays, `Vec`, `Option`, `Result`, `str`, `Duration`, `Ordering`) and whose elements' equality
is too. The translator gives a call this model only for such a type: a hand-written `impl
PartialEq` is translated as its own function, and a type whose equality is not its value's (a
float, a field with a hand-written impl) is refused.
-/

namespace Blaze.core.cmp

/-- `core::cmp::Ordering`: `Less = -1`, `Equal = 0`, `Greater = 1` (cmp.rs:420,
`pub enum Ordering`, `#[repr(i8)]`). -/
inductive Ordering where
  | Less
  | Equal
  | Greater
  deriving Repr, DecidableEq

namespace Ordering

/-- The discriminant, `as_raw` (cmp.rs:430: `crate::intrinsics::discriminant_value(&self)`). -/
def as_raw : Ordering → Int
  | .Less => -1
  | .Equal => 0
  | .Greater => 1

/-- `is_eq` (cmp.rs:454): `self.as_raw() == 0`. -/
def is_eq (o : Ordering) : Bool := decide (o.as_raw = 0)
/-- `is_ne` (cmp.rs:477): `self.as_raw() != 0`. -/
def is_ne (o : Ordering) : Bool := decide (o.as_raw ≠ 0)
/-- `is_lt` (cmp.rs:496): `self.as_raw() < 0`. -/
def is_lt (o : Ordering) : Bool := decide (o.as_raw < 0)
/-- `is_gt` (cmp.rs:515): `self.as_raw() > 0`. -/
def is_gt (o : Ordering) : Bool := decide (0 < o.as_raw)
/-- `is_le` (cmp.rs:534): `self.as_raw() <= 0`. -/
def is_le (o : Ordering) : Bool := decide (o.as_raw ≤ 0)
/-- `is_ge` (cmp.rs:553): `self.as_raw() >= 0`. -/
def is_ge (o : Ordering) : Bool := decide (0 ≤ o.as_raw)

/-- `reverse` (cmp.rs:590): `Less => Greater, Equal => Equal, Greater => Less`. -/
def reverse : Ordering → Ordering
  | .Less => .Greater
  | .Equal => .Equal
  | .Greater => .Less

/-- `then` (cmp.rs:629): `Equal => other, _ => self`. -/
def «then» (o other : Ordering) : Ordering :=
  match o with
  | .Equal => other
  | _ => o

-- Rust's documentation: `Ordering::Less.is_lt()`, `!Ordering::Equal.is_lt()`,
-- `Ordering::Less.reverse() == Ordering::Greater`,
-- `Ordering::Equal.then(Ordering::Less) == Ordering::Less`,
-- `Ordering::Less.then(Ordering::Equal) == Ordering::Less`.
example : Ordering.Less.is_lt = true := by decide
example : Ordering.Equal.is_lt = false := by decide
example : Ordering.Equal.is_le = true := by decide
example : Ordering.Greater.is_ge = true := by decide
example : Ordering.Less.is_gt = false := by decide
example : Ordering.Equal.is_eq = true := by decide
example : Ordering.Greater.is_ne = true := by decide
example : Ordering.Less.reverse = .Greater := rfl
example : Ordering.Equal.then .Less = .Less := rfl
example : Ordering.Less.then .Equal = .Less := rfl

@[simp] theorem is_eq_iff (o : Ordering) : o.is_eq = true ↔ o = .Equal := by
  cases o <;> decide
@[simp] theorem is_ne_iff (o : Ordering) : o.is_ne = true ↔ o ≠ .Equal := by
  cases o <;> decide
@[simp] theorem is_lt_iff (o : Ordering) : o.is_lt = true ↔ o = .Less := by
  cases o <;> decide
@[simp] theorem is_gt_iff (o : Ordering) : o.is_gt = true ↔ o = .Greater := by
  cases o <;> decide
@[simp] theorem is_le_iff (o : Ordering) : o.is_le = true ↔ o ≠ .Greater := by
  cases o <;> decide
@[simp] theorem is_ge_iff (o : Ordering) : o.is_ge = true ↔ o ≠ .Less := by
  cases o <;> decide

@[simp] theorem reverse_reverse (o : Ordering) : o.reverse.reverse = o := by
  cases o <;> rfl

end Ordering

/-- `PartialEq::eq` of a type whose equality is its value's: see the module. -/
def PartialEq.eq {α : Type} [DecidableEq α] (a b : α) : Bool := decide (a = b)

/-- `PartialEq::ne` (cmp.rs, the trait's default: `!self.eq(other)`). -/
def PartialEq.ne {α : Type} [DecidableEq α] (a b : α) : Bool := !PartialEq.eq a b

@[simp] theorem PartialEq.eq_iff {α : Type} [DecidableEq α] (a b : α) :
    PartialEq.eq a b = true ↔ a = b := by
  simp [PartialEq.eq]

@[simp] theorem PartialEq.ne_iff {α : Type} [DecidableEq α] (a b : α) :
    PartialEq.ne a b = true ↔ a ≠ b := by
  simp [PartialEq.ne, PartialEq.eq]

@[simp] theorem PartialEq.eq_self {α : Type} [DecidableEq α] (a : α) :
    PartialEq.eq a a = true := by
  simp [PartialEq.eq]

end Blaze.core.cmp

namespace Blaze.Scalar

variable {ty : ScalarTy}

/-- `Ord::cmp` of an integer (cmp.rs:2316): `three_way_compare(*self, *other)`. -/
def cmp (a b : Scalar ty) : core.cmp.Ordering :=
  if a.val < b.val then .Less else if a.val = b.val then .Equal else .Greater

/-- `PartialOrd::partial_cmp` of an integer (cmp.rs:2305):
`Some(three_way_compare(*self, *other))`. -/
def partial_cmp (a b : Scalar ty) : Option core.cmp.Ordering := some (cmp a b)

-- Rust's documentation of `Ord::cmp`: `1.cmp(&2) == Less`, `1.cmp(&1) == Equal`,
-- `2.cmp(&1) == Greater`.
example : cmp (1 : I32) 2 = .Less := by decide
example : cmp (1 : I32) 1 = .Equal := by decide
example : cmp (2 : I32) 1 = .Greater := by decide
example : partial_cmp (lit .I8 (-1)) 0 = some .Less := by decide

theorem cmp_eq_Less (a b : Scalar ty) : cmp a b = .Less ↔ a.val < b.val := by
  unfold cmp
  by_cases h1 : a.val < b.val
  · rw [ite_eq_left h1]
    exact ⟨fun _ => h1, fun _ => rfl⟩
  · rw [ite_eq_right h1]
    by_cases h2 : a.val = b.val
    · rw [ite_eq_left h2]
      exact ⟨fun e => (nomatch e), fun e => absurd e h1⟩
    · rw [ite_eq_right h2]
      exact ⟨fun e => (nomatch e), fun e => absurd e h1⟩

theorem cmp_eq_Equal (a b : Scalar ty) : cmp a b = .Equal ↔ a.val = b.val := by
  unfold cmp
  by_cases h1 : a.val < b.val
  · rw [ite_eq_left h1]
    exact ⟨fun e => (nomatch e), fun e => absurd h1 (by omega)⟩
  · rw [ite_eq_right h1]
    by_cases h2 : a.val = b.val
    · rw [ite_eq_left h2]
      exact ⟨fun _ => h2, fun _ => rfl⟩
    · rw [ite_eq_right h2]
      exact ⟨fun e => (nomatch e), fun e => absurd e h2⟩

theorem cmp_eq_Greater (a b : Scalar ty) : cmp a b = .Greater ↔ b.val < a.val := by
  unfold cmp
  by_cases h1 : a.val < b.val
  · rw [ite_eq_left h1]
    exact ⟨fun e => (nomatch e), fun e => absurd h1 (by omega)⟩
  · rw [ite_eq_right h1]
    by_cases h2 : a.val = b.val
    · rw [ite_eq_left h2]
      exact ⟨fun e => (nomatch e), fun e => absurd h2 (by omega)⟩
    · rw [ite_eq_right h2]
      exact ⟨fun _ => by omega, fun _ => rfl⟩

end Blaze.Scalar

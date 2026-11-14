import Blaze.Duration
import Blaze.Std
import Blaze.Slice

/-!
The order `#[derive(PartialOrd, Ord)]` gives a struct or an enum, and the orders of the
standard library's types it compares fields by.

A derive compares the fields in declaration order and the first that is not `Equal` decides
(crates/hir-expand/src/builtin/derive_macro.rs, `write_ordering`, as rustc's
`rustc_builtin_macros/src/deriving/cmp/ord.rs`: `match Ord::cmp(&self.f0, &other.f0) {
Equal => .., c => return c }`, then `Equal`); an enum of several variants first compares
`discriminant_value(self)` with `discriminant_value(other)` by the discriminant type's order,
and only two values of one variant go on to their fields. The translator writes each type's
`cmp` by that rule (crates/hir-ty-lean/src/lib.rs, `ord_module`), comparing each field by one
of the orders here or by a derived one of the crate's types, and gives a call this model only
then. Each of these orders is total, so a derived `partial_cmp` is `Some` of `cmp`.
-/

namespace Blaze.core.cmp.Order

/-- Two integers' order: two `discriminant_value`s compared by their type's order (a
discriminant read as the signed or unsigned value its type holds). -/
def int (a b : Int) : Ordering :=
  if a < b then .Less else if a = b then .Equal else .Greater

/-- `bool`'s `Ord` (cmp.rs:2356): `(*self as i8) - (*other as i8)`, `-1` `Less`, `0` `Equal`,
`1` `Greater`, so `false < true`. -/
def bool (a b : Bool) : Ordering := int (if a then 1 else 0) (if b then 1 else 0)

/-- `()`'s `Ord` (cmp.rs:2347): `Equal`. -/
def unit (_ _ : Unit) : Ordering := .Equal

/-- `PhantomData`'s `Ord` (marker.rs:837): `Equal`. -/
def phantom (_ _ : core.marker.PhantomData) : Ordering := .Equal

/-- `Ordering`'s `Ord`, derived on a `#[repr(i8)]` enum (cmp.rs:420): its discriminants,
`as_raw`, compared. -/
def ordering (a b : Ordering) : Ordering := int a.as_raw b.as_raw

/-- `Option`'s `Ord` (option.rs:2474), of the elements' order `f`: `(Some(l), Some(r)) =>
l.cmp(r)`, `(Some(_), None) => Greater`, `(None, Some(_)) => Less`, `(None, None) => Equal`. -/
def option {α : Type} (f : α → α → Ordering) (a b : Option α) : Ordering :=
  match a, b with
  | some l, some r => f l r
  | some _, none => .Greater
  | none, some _ => .Less
  | none, none => .Equal

/-- A tuple's `Ord` (tuple.rs:106, `lexical_cmp!` at tuple.rs:221), of the first element's
order `f` and the rest's `g`: `f`'s, then `g`'s when that is `Equal`. A Lean tuple of more than
two is a pair whose second is the rest, so this is the order element by element. -/
def pair {α β : Type} (f : α → α → Ordering) (g : β → β → Ordering) (a b : α × β) : Ordering :=
  (f a.1 b.1).then (g a.2 b.2)

/-- A slice's `Ord` (slice/cmp.rs:38, `SliceOrd::compare`), of the elements' order `f`: the
elements in turn, the first pair not `Equal` deciding, then the lengths (a prefix is less). -/
def list {α : Type} (f : α → α → Ordering) : List α → List α → Ordering
  | [], [] => .Equal
  | [], _ :: _ => .Less
  | _ :: _, [] => .Greater
  | x :: xs, y :: ys => (f x y).then (list f xs ys)

/-- An array's `Ord` (array/mod.rs:445: `Ord::cmp(&&self[..], &&other[..])`): its elements' as
a slice, of one length. -/
def array {α : Type} {n : Nat} (f : α → α → Ordering) (a b : Arr α n) : Ordering :=
  list f a.val b.val

example : bool false true = .Less := rfl
example : option Scalar.cmp (some (1 : U32)) none = .Greater := rfl
example : pair Scalar.cmp Scalar.cmp ((1 : U32), (2 : U32)) ((1 : U32), (1 : U32)) = .Greater := by decide

end Blaze.core.cmp.Order

-- Hand-written, not generated: the proofs of the `orders` fixture's statements.
import Blaze
import BlazeModel.orders.pair_order_is_lexicographic
import BlazeModel.orders.key_order_is_by_variant_then_fields
import BlazeModel.orders.id_order_is_by_bytes

/-!
Derived `PartialOrd` and `Ord` proved through their models (crates/hir-ty-lean/src/tests.rs,
`ORDERS`): the model of a derived order agrees with Rust's on a struct of two fields (the
first field's order, then the second's) and on an enum with payloads (the variants by their
discriminants, which need not be in declaration order, then one variant's fields in turn).
-/

open Blaze

/-- An integer's order says what its comparisons do. -/
private theorem cmp_cases {ty : ScalarTy} (a b : Scalar ty) :
    (Scalar.cmp a b = .Less ∧ a.val < b.val) ∨ (Scalar.cmp a b = .Equal ∧ a.val = b.val)
      ∨ (Scalar.cmp a b = .Greater ∧ b.val < a.val) := by
  unfold Scalar.cmp
  by_cases h1 : a.val < b.val
  · exact Or.inl ⟨by simp [h1], h1⟩
  · by_cases h2 : a.val = b.val
    · exact Or.inr (Or.inl ⟨by simp [h2], h2⟩)
    · exact Or.inr (Or.inr ⟨by simp [h1, h2], by omega⟩)

/-- `Pair`'s derived order: `(a, b) < (c, d)` exactly when `a < c`, or `a == c` and `b < d`;
and a value is `Equal` to itself. -/
theorem orders.pair_order_is_lexicographic.spec.proof :
    orders.pair_order_is_lexicographic.spec.statement := by
  intro a b c d
  have hself : ∀ {ty : ScalarTy} (x : Scalar ty), Scalar.cmp x x = .Equal := by
    intro _ x; simp [Scalar.cmp]
  rcases cmp_cases a c with ⟨h1, h1'⟩ | ⟨h1, h1'⟩ | ⟨h1, h1'⟩ <;>
  rcases cmp_cases b d with ⟨h2, h2'⟩ | ⟨h2, h2'⟩ | ⟨h2, h2'⟩ <;>
  simp [orders.pair_order_is_lexicographic, orders.pair_lt, orders.pair_cmp, orders.Pair.impl.lt,
    orders.Pair.impl.cmp, h1, h2, hself, Scalar.lt, Scalar.eq, core.cmp.Ordering.«then»,
    core.cmp.PartialEq.eq, core.cmp.Ordering.is_lt, core.cmp.Ordering.as_raw] <;>
  omega

/-- `Key`'s derived order: `Small < Named < Missing` by the discriminants whatever the
payloads; two `Named`s by their number, then their `Pair`; `max` and `min` by it; and
`Signed`'s variants by their discriminants (`High = -5 < Low = -1 < Zero = 0`), not by the
order they are declared in. -/
theorem orders.key_order_is_by_variant_then_fields.spec.proof :
    orders.key_order_is_by_variant_then_fields.spec.statement := by
  intro x n m p q
  have hthen : ∀ o1 o2 : core.cmp.Ordering,
      (o1.then o2).is_ge = (o1.is_gt || (o1.is_eq && o2.is_ge)) := by
    intro o1 o2; cases o1 <;> cases o2 <;> rfl
  have hnot : ∀ o : core.cmp.Ordering, (!o.is_lt) = o.is_ge := by
    intro o; cases o <;> rfl
  have hgt : Scalar.gt n m = (Scalar.cmp n m).is_gt := by
    rcases cmp_cases n m with ⟨h, h'⟩ | ⟨h, h'⟩ | ⟨h, h'⟩ <;>
      simp [h, Scalar.gt, core.cmp.Ordering.is_gt, core.cmp.Ordering.as_raw] <;> omega
  have heq : Scalar.eq n m = (Scalar.cmp n m).is_eq := by
    rcases cmp_cases n m with ⟨h, h'⟩ | ⟨h, h'⟩ | ⟨h, h'⟩ <;>
      simp [h, Scalar.eq, core.cmp.Ordering.is_eq, core.cmp.Ordering.as_raw] <;> omega
  have hge : orders.key_ge (.Named n p) (.Named m q)
      = (Scalar.gt n m || (Scalar.eq n m && !orders.pair_lt p q)) := by
    simp only [orders.key_ge, orders.Key.impl.ge, orders.Key.impl.cmp, orders.pair_lt,
      orders.Pair.impl.lt, hthen, hnot, hgt, heq]
  simp [orders.key_order_is_by_variant_then_fields, hge, orders.key_cmp, orders.key_max,
    orders.key_min, orders.signed_lt, orders.Key.impl.cmp, orders.Key.impl.max,
    orders.Key.impl.min, orders.Key.impl.lt, orders.Key.impl.discriminant,
    orders.Signed.impl.lt, orders.Signed.impl.cmp, orders.Signed.impl.discriminant,
    core.cmp.Order.int, core.cmp.PartialEq.eq, core.cmp.Ordering.is_lt,
    core.cmp.Ordering.as_raw]
  cases Scalar.gt n m <;> cases Scalar.eq n m <;> rfl

/-- `Id`'s derived order, over its `[u8; 4]`: the arrays' bytes compared in turn from the
first, as `[T; N]`'s `Ord` (`Ord::cmp(&&self[..], &&other[..])`, the slices' lexicographic
order): the first byte that differs decides, whatever the bytes after it. -/
theorem orders.id_order_is_by_bytes.spec.proof : orders.id_order_is_by_bytes.spec.statement := by
  unfold orders.id_order_is_by_bytes.spec.statement
  decide

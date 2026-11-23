//! Unified Row-Polymorphic Product Algebra (`blaze-ty::product_row`).
//!
//! Unifies Blaze's two brace-delimited product forms — named structural records
//! (`{ name: &str, depth: u32 }`, [`crate::record_ty`]) and type-keyed type sets
//! (`{ Health, MaxSpeed, ..R }`, [`crate::type_set`]) — into a single canonical product-row model
//! with zero `{ ... }` syntax collision:
//!
//! - Every slot is keyed by [`SlotKey`]: either a named field (`SlotKey::Named("name")`) or a
//!   type-keyed slot (`SlotKey::ByType(erased_ty)`). Mixed products like
//!   `{ name: &str, Health, ..R }` work seamlessly.
//! - **Open row polymorphism (`..R` + `Lacks`)** works identically for **both** named records
//!   (`fn render<R>(p: { name: &str, ..R })`) and type sets (`fn damage<R>(e: { Health, ..R })`),
//!   solving `R` in O(N + M) sorted merge while checking `Lacks` bounds and field types.
//! - **Disjoint field borrows** (`(&uniq r.name, &r.[Health])`) are checked in O(log N).
//! - **Deterministic alignment-descending layout** guarantees identical byte offsets independent
//!   of source declaration order.

use core::cmp::Ordering;
use core::fmt;

use blaze_own::{AVec, Alloc};

use crate::cx::TyCx;
use crate::ids::Ty;
use crate::record_ty::RecordTy;
use crate::type_set::{RowVar, TypeSet, erase_regions};

/// Canonical key of one slot in a [`ProductRow`]: either a named record field (`name: T`) or a
/// type-keyed slot (`T` with lifetimes erased).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SlotKey<'s> {
    /// Named record field (`name: T`).
    Named(&'s str),
    /// Type-keyed slot (`Health`, `&'a str` erased to `&'_ str`).
    ByType(Ty),
}

impl Ord for SlotKey<'_> {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        match (*self, *other) {
            (SlotKey::Named(a), SlotKey::Named(b)) => a.as_bytes().cmp(b.as_bytes()),
            (SlotKey::Named(_), SlotKey::ByType(_)) => Ordering::Less,
            (SlotKey::ByType(_), SlotKey::Named(_)) => Ordering::Greater,
            (SlotKey::ByType(a), SlotKey::ByType(b)) => a.cmp(&b),
        }
    }
}

impl PartialOrd for SlotKey<'_> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One slot in a canonical [`ProductRow`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProductSlot<'s> {
    /// Canonical key (`Named` or lifetime-erased `ByType`).
    pub key: SlotKey<'s>,
    /// Lifetime-erased value type (equals `k` for `SlotKey::ByType(k)`).
    pub erased_ty: Ty,
    /// Full value type preserving concrete lifetimes.
    pub ty: Ty,
}

/// Errors produced by unified product-row construction, extension, unification, or borrow checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProductRowError<'s> {
    /// Named field identifier was empty or contained `,`.
    InvalidFieldName { name: &'s str },
    /// Two slots had the same canonical [`SlotKey`].
    DuplicateSlot { key: SlotKey<'s> },
    /// Extending an open row `{ .., ..R }` requires a `R: Lacks<slot>` bound.
    MissingLacksBound { row: RowVar, slot: SlotKey<'s> },
    /// Concrete product violated a `R: Lacks<slot>` constraint during row unification.
    LacksViolated { row: RowVar, slot: SlotKey<'s> },
    /// Expected slot was missing from the concrete product.
    MissingSlot { key: SlotKey<'s> },
    /// Closed unification found an unexpected extra slot.
    UnexpectedSlot { key: SlotKey<'s> },
    /// Two slots with the same [`SlotKey`] had incompatible lifetime-erased types.
    TypeMismatch { key: SlotKey<'s>, expected: Ty, found: Ty },
    /// Unifying two open rows requires introducing a fresh join row variable.
    BothOpen { lhs: RowVar, rhs: RowVar },
    /// Disjoint borrow query referenced a slot not present in the row.
    UnknownSlot { key: SlotKey<'s> },
}

impl fmt::Display for ProductRowError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ProductRowError::InvalidFieldName { name } => {
                write!(f, "invalid product field name '{name}'")
            }
            ProductRowError::DuplicateSlot { key } => {
                write!(f, "duplicate product slot '{key:?}'")
            }
            ProductRowError::MissingLacksBound { row, slot } => {
                write!(f, "extending open row '{row:?}' with '{slot:?}' requires `Lacks` bound")
            }
            ProductRowError::LacksViolated { row, slot } => {
                write!(f, "row '{row:?}' violated `Lacks` constraint for '{slot:?}'")
            }
            ProductRowError::MissingSlot { key } => {
                write!(f, "missing required product slot '{key:?}'")
            }
            ProductRowError::UnexpectedSlot { key } => {
                write!(f, "unexpected extra product slot '{key:?}'")
            }
            ProductRowError::TypeMismatch { key, expected, found } => {
                write!(f, "slot '{key:?}' type mismatch: expected {expected:?}, found {found:?}")
            }
            ProductRowError::BothOpen { lhs, rhs } => {
                write!(f, "cannot unify two open rows '{lhs:?}' and '{rhs:?}' directly")
            }
            ProductRowError::UnknownSlot { key } => {
                write!(f, "unknown product slot '{key:?}'")
            }
        }
    }
}

/// Deterministic byte layout of a [`ProductRow`].
#[derive(Debug)]
pub struct ProductLayout<'a> {
    /// Byte offset per canonical slot index.
    pub offsets: AVec<'a, u32>,
    /// Total padded size in bytes.
    pub size: u32,
    /// Alignment in bytes (at least 1).
    pub align: u32,
}

/// Canonical, order-independent product row supporting named fields (`name: T`), type-keyed
/// slots (`Health`), mixed rows (`{ name: &str, Health, ..R }`), and open row tails `..R` with
/// `Lacks` constraints.
#[derive(Debug)]
pub struct ProductRow<'a, 's> {
    /// Strictly ascending by `slot.key`.
    slots: AVec<'a, ProductSlot<'s>>,
    /// Optional open row variable `..R`.
    tail: Option<RowVar>,
    /// Sorted deduplicated keys proven absent from `tail` (`R: Lacks<K>`).
    lacks: AVec<'a, SlotKey<'s>>,
}

impl<'a, 's> ProductRow<'a, 's> {
    /// Empty closed product `{}`.
    pub fn empty(alloc: Alloc<'a>) -> Self {
        Self {
            slots: AVec::new_in(alloc),
            tail: None,
            lacks: AVec::new_in(alloc),
        }
    }

    /// Canonicalizes an arbitrary-order slice of `(Option<name>, ty)` slots into a [`ProductRow`]:
    /// `Some(name)` creates a named field (`SlotKey::Named(name)`); `None` creates a type-keyed
    /// slot (`SlotKey::ByType(erase(ty))`).
    pub fn from_entries(
        cx: &mut TyCx<'_>,
        raw: &[(Option<&'s str>, Ty)],
        tail: Option<RowVar>,
        lacks_raw: &[SlotKey<'s>],
        alloc: Alloc<'a>,
    ) -> Result<Self, ProductRowError<'s>> {
        let mut slots = AVec::with_capacity_in(raw.len(), alloc);
        for &(name_opt, ty) in raw {
            let erased_ty = erase_regions(cx, ty, alloc);
            let key = match name_opt {
                Some(name) => {
                    if name.is_empty() || name.contains(',') {
                        return Err(ProductRowError::InvalidFieldName { name });
                    }
                    SlotKey::Named(name)
                }
                None => SlotKey::ByType(erased_ty),
            };
            slots.push(ProductSlot { key, erased_ty, ty });
        }
        slots.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        for w in slots.windows(2) {
            if let [a, b] = w
                && a.key == b.key
            {
                return Err(ProductRowError::DuplicateSlot { key: a.key });
            }
        }

        let mut lacks = AVec::with_capacity_in(lacks_raw.len(), alloc);
        for &lk in lacks_raw {
            let norm = match lk {
                SlotKey::Named(name) => SlotKey::Named(name),
                SlotKey::ByType(t) => SlotKey::ByType(erase_regions(cx, t, alloc)),
            };
            lacks.push(norm);
        }
        lacks.sort_unstable();
        lacks.dedup();

        Ok(Self { slots, tail, lacks })
    }

    /// Lifts a closed [`RecordTy`] directly into a [`ProductRow`] with zero loss of information.
    pub fn from_record(rec: &RecordTy<'_, 's>, tail: Option<RowVar>, alloc: Alloc<'a>) -> Self {
        let mut slots = AVec::with_capacity_in(rec.len(), alloc);
        for f in rec.fields() {
            slots.push(ProductSlot {
                key: SlotKey::Named(f.name),
                erased_ty: f.key,
                ty: f.ty,
            });
        }
        Self { slots, tail, lacks: AVec::new_in(alloc) }
    }

    /// Lifts a [`TypeSet`] (including its open row tail and `Lacks` bounds) directly into a [`ProductRow`].
    pub fn from_type_set(set: &TypeSet<'_>, alloc: Alloc<'a>) -> Self {
        let mut slots = AVec::with_capacity_in(set.len(), alloc);
        for m in set.members() {
            slots.push(ProductSlot {
                key: SlotKey::ByType(m.key),
                erased_ty: m.key,
                ty: m.ty,
            });
        }
        let mut lacks = AVec::with_capacity_in(set.lacks_bounds().len(), alloc);
        for &k in set.lacks_bounds() {
            lacks.push(SlotKey::ByType(k));
        }
        Self { slots, tail: set.tail(), lacks }
    }

    /// Canonical sorted slots.
    #[inline]
    pub fn slots(&self) -> &[ProductSlot<'s>] {
        &self.slots
    }

    /// Optional open row variable `..R`.
    #[inline]
    pub fn tail(&self) -> Option<RowVar> {
        self.tail
    }

    /// Sorted `Lacks` constraints on `tail`.
    #[inline]
    pub fn lacks(&self) -> &[SlotKey<'s>] {
        &self.lacks
    }

    /// Number of explicit slots.
    #[inline]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns `true` iff there are no explicit slots and no open tail.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.tail.is_none()
    }

    /// O(log N) lookup by canonical [`SlotKey`].
    #[inline]
    pub fn slot_index(&self, key: SlotKey<'_>) -> Option<usize> {
        self.slots.binary_search_by(|s| s.key.cmp(&key)).ok()
    }

    /// Canonical structural equality independent of original source field order.
    pub fn eq_canonical(&self, other: &ProductRow<'_, '_>) -> bool {
        self.tail == other.tail
            && self.lacks.as_slice() == other.lacks.as_slice()
            && self.slots.len() == other.slots.len()
            && self
                .slots
                .iter()
                .zip(other.slots.iter())
                .all(|(a, b)| a.key == b.key && a.erased_ty == b.erased_ty)
    }

    /// Extends `self` with a named or type-keyed slot (`{ extra: i32, ..r }` or `{ Burning, ..e }`),
    /// enforcing `R: Lacks<key>` when `self` has an open row tail `..R`.
    pub fn extend<'b>(
        &self,
        cx: &mut TyCx<'_>,
        name: Option<&'s str>,
        ty: Ty,
        alloc: Alloc<'b>,
    ) -> Result<ProductRow<'b, 's>, ProductRowError<'s>> {
        let erased_ty = erase_regions(cx, ty, alloc);
        let key = match name {
            Some(n) => {
                if n.is_empty() || n.contains(',') {
                    return Err(ProductRowError::InvalidFieldName { name: n });
                }
                SlotKey::Named(n)
            }
            None => SlotKey::ByType(erased_ty),
        };
        match self.slots.binary_search_by(|s| s.key.cmp(&key)) {
            Ok(_) => Err(ProductRowError::DuplicateSlot { key }),
            Err(pos) => {
                if let Some(row) = self.tail
                    && self.lacks.binary_search(&key).is_err()
                {
                    return Err(ProductRowError::MissingLacksBound { row, slot: key });
                }
                let mut slots = AVec::with_capacity_in(self.slots.len() + 1, alloc);
                if let Some(before) = self.slots.get(..pos) {
                    slots.extend_from_slice(before);
                }
                slots.push(ProductSlot { key, erased_ty, ty });
                if let Some(after) = self.slots.get(pos..) {
                    slots.extend_from_slice(after);
                }
                let mut lacks = AVec::with_capacity_in(self.lacks.len(), alloc);
                lacks.extend_from_slice(&self.lacks);
                Ok(ProductRow { slots, tail: self.tail, lacks })
            }
        }
    }

    /// Unifies an open or closed pattern row `self` (e.g. `{ name: &str, ..R }` or
    /// `{ name: &str, Health, ..R }`) against a concrete closed product `concrete` in O(N + M)
    /// sorted merge, solving `R` to the exact residual [`ProductRow`] and verifying all `Lacks`
    /// bounds and slot types.
    pub fn unify_with_concrete<'b>(
        &self,
        concrete: &ProductRow<'_, 's>,
        alloc: Alloc<'b>,
    ) -> Result<Option<(RowVar, ProductRow<'b, 's>)>, ProductRowError<'s>> {
        if let (Some(lhs), Some(rhs)) = (self.tail, concrete.tail) {
            return Err(ProductRowError::BothOpen { lhs, rhs });
        }
        if let Some(row) = self.tail {
            for &lk in self.lacks.iter() {
                if concrete.slot_index(lk).is_some() {
                    return Err(ProductRowError::LacksViolated { row, slot: lk });
                }
            }
        }

        let mut residual = AVec::new_in(alloc);
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.slots.len() && j < concrete.slots.len() {
            let (Some(ps), Some(cs)) = (self.slots.get(i), concrete.slots.get(j)) else {
                break;
            };
            match ps.key.cmp(&cs.key) {
                Ordering::Equal => {
                    if ps.erased_ty != cs.erased_ty {
                        return Err(ProductRowError::TypeMismatch {
                            key: ps.key,
                            expected: ps.erased_ty,
                            found: cs.erased_ty,
                        });
                    }
                    i += 1;
                    j += 1;
                }
                Ordering::Less => {
                    return Err(ProductRowError::MissingSlot { key: ps.key });
                }
                Ordering::Greater => {
                    if self.tail.is_none() {
                        return Err(ProductRowError::UnexpectedSlot { key: cs.key });
                    }
                    residual.push(*cs);
                    j += 1;
                }
            }
        }

        if let Some(ps) = self.slots.get(i) {
            return Err(ProductRowError::MissingSlot { key: ps.key });
        }
        while let Some(cs) = concrete.slots.get(j) {
            if self.tail.is_none() {
                return Err(ProductRowError::UnexpectedSlot { key: cs.key });
            }
            residual.push(*cs);
            j += 1;
        }

        Ok(self.tail.map(|row| {
            (row, ProductRow { slots: residual, tail: None, lacks: AVec::new_in(alloc) })
        }))
    }

    /// Checks whether two slot borrows (e.g. `(&uniq r.name, &r.[Health])`) are disjoint.
    pub fn can_disjoint_borrow(&self, a: SlotKey<'s>, b: SlotKey<'s>) -> Result<bool, ProductRowError<'s>> {
        if self.slot_index(a).is_none() {
            return Err(ProductRowError::UnknownSlot { key: a });
        }
        if self.slot_index(b).is_none() {
            return Err(ProductRowError::UnknownSlot { key: b });
        }
        Ok(a != b)
    }

    /// Computes deterministic alignment-descending byte layout (identical regardless of source
    /// field declaration order).
    pub fn compute_layout<'b>(
        &self,
        size_align: impl Fn(Ty) -> (u32, u32),
        alloc: Alloc<'b>,
    ) -> ProductLayout<'b> {
        let n = self.slots.len();
        let mut order: AVec<'b, (usize, u32, u32)> = AVec::with_capacity_in(n, alloc);
        for (idx, s) in self.slots.iter().enumerate() {
            let (sz, al) = size_align(s.ty);
            let align = if al == 0 { 1 } else { al };
            order.push((idx, sz, align));
        }
        order.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

        let mut offsets = AVec::with_capacity_in(n, alloc);
        offsets.resize(n, 0u32);
        let mut cursor = 0u32;
        let mut max_align = 1u32;
        for &(idx, sz, align) in order.iter() {
            if align > max_align {
                max_align = align;
            }
            let rem = cursor % align;
            if rem != 0 {
                cursor = cursor.saturating_add(align - rem);
            }
            if let Some(off) = offsets.get_mut(idx) {
                *off = cursor;
            }
            cursor = cursor.saturating_add(sz);
        }
        let rem = cursor % max_align;
        let total = if rem == 0 { cursor } else { cursor.saturating_add(max_align - rem) };
        ProductLayout { offsets, size: total, align: max_align }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;
    use crate::ids::{Def, IntTy, List, UintTy};

    #[test]
    fn open_row_polymorphism_works_on_named_records_and_mixed_type_sets() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);

        let u32_ty = Ty::uint(UintTy::U32);
        let i64_ty = Ty::int(IntTy::I64);
        let health = cx.tys.adt(Def(1), List::EMPTY);
        let armor = cx.tys.adt(Def(2), List::EMPTY);
        let row_r = RowVar(0);

        // Open named+type-keyed row: `{ name: &str, Health, ..R }` where `R: Lacks<"deleted">`
        let pattern = ProductRow::from_entries(
            &mut cx,
            &[(Some("name"), Ty::STR), (None, health)],
            Some(row_r),
            &[SlotKey::Named("deleted")],
            a,
        )
        .expect("valid open mixed row");

        // Concrete product in arbitrary order: `{ Armor, depth: u32, Health, name: &str }`
        let concrete = ProductRow::from_entries(
            &mut cx,
            &[(None, armor), (Some("depth"), u32_ty), (None, health), (Some("name"), Ty::STR)],
            None,
            &[],
            a,
        )
        .expect("valid concrete mixed row");

        // Unification solves `R = { depth: u32, Armor }`!
        let (solved_var, residual) = pattern
            .unify_with_concrete(&concrete, a)
            .expect("row unifies")
            .expect("open row produces binding");
        assert_eq!(solved_var, row_r);
        let expected_res = ProductRow::from_entries(
            &mut cx,
            &[(Some("depth"), u32_ty), (None, armor)],
            None,
            &[],
            a,
        )
        .unwrap();
        assert!(residual.eq_canonical(&expected_res));

        // Extending `pattern` with `deleted: bool` succeeds (proven by `Lacks<"deleted">`),
        // while extending with `extra: i64` fails (`MissingLacksBound`)!
        let ext = pattern.extend(&mut cx, Some("deleted"), Ty::BOOL, a).expect("lacks satisfied");
        assert_eq!(ext.len(), 3);
        assert_eq!(
            pattern.extend(&mut cx, Some("extra"), i64_ty, a).unwrap_err(),
            ProductRowError::MissingLacksBound { row: row_r, slot: SlotKey::Named("extra") }
        );

        // Concrete product containing `deleted` violates `R: Lacks<"deleted">`.
        let violating = concrete.extend(&mut cx, Some("deleted"), Ty::BOOL, a).unwrap();
        assert_eq!(
            pattern.unify_with_concrete(&violating, a).unwrap_err(),
            ProductRowError::LacksViolated { row: row_r, slot: SlotKey::Named("deleted") }
        );

        // Disjoint borrows: `(&uniq p.name, &p.[Health])` ok; `(&uniq p.name, &p.name)` false!
        assert_eq!(
            concrete.can_disjoint_borrow(SlotKey::Named("name"), SlotKey::ByType(health)),
            Ok(true)
        );
        assert_eq!(
            concrete.can_disjoint_borrow(SlotKey::Named("name"), SlotKey::Named("name")),
            Ok(false)
        );
    }

    #[test]
    fn bridges_cleanly_from_record_ty_and_type_set_with_identical_layout() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let u32_ty = Ty::uint(UintTy::U32);
        let i64_ty = Ty::int(IntTy::I64);

        let rec = RecordTy::from_fields(&mut cx, &[("flag", Ty::BOOL), ("count", u32_ty), ("big", i64_ty)], a)
            .unwrap();
        let lifted_rec = ProductRow::from_record(&rec, None, a);
        let layout = lifted_rec.compute_layout(
            |t| if t == i64_ty { (8, 8) } else if t == u32_ty { (4, 4) } else { (1, 1) },
            a,
        );
        assert_eq!(layout.size, 16);
        assert_eq!(layout.align, 8);

        let ts = TypeSet::from_members(&mut cx, &[u32_ty, i64_ty], Some(RowVar(1)), &[Ty::BOOL], a).unwrap();
        let lifted_ts = ProductRow::from_type_set(&ts, a);
        assert_eq!(lifted_ts.len(), 2);
        assert_eq!(lifted_ts.tail(), Some(RowVar(1)));
        assert_eq!(lifted_ts.lacks(), &[SlotKey::ByType(Ty::BOOL)]);
    }
}

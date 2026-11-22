//! Canonical Type-Set Algebra & Row-Polymorphism Engine (`blaze-ty::type_set`).
//!
//! First-class compiler representation for anonymous product types keyed by member type
//! (`{MaxSpeed, Health}` == `{Health, MaxSpeed}`), implementing the rules of Blaze's type-set
//! design:
//!
//! 1. **Order-independent canonicalization**: members are sorted by their lifetime-erased [`Ty`]
//!    key, so `{MaxSpeed, Health}` and `{Health, MaxSpeed}` are structurally identical.
//! 2. **Lifetime erasure for keys**: `&'a T` and `&'b T` share the same erased key (`Region::ERASED`),
//!    so `{&'a T, &'b T}` is rejected as a duplicate while each valid member preserves its full
//!    region-bearing [`Ty`].
//! 3. **Open rows & `Lacks<T>` constraints**: `{Health, Armor, ..R}` unifies against any concrete
//!    superset `{Cargo, MaxSpeed, Health, Armor}` in O(N + M) sorted merge, solving
//!    `R = {Cargo, MaxSpeed}` and checking `Lacks<T>` bounds on extension (`{Burning, ..e}`).
//! 4. **Disjoint field borrows**: `(&mut s.[A], &s.[B])` is sound iff `erase(A) != erase(B)` and
//!    both exist in the set.
//! 5. **Deterministic alignment-descending layout**: `{MaxSpeed, Health}` and `{Health, MaxSpeed}`
//!    produce identical byte offsets and minimal padding.

use core::convert::Infallible;

use blaze_own::{Alloc, Vec};

use crate::cx::TyCx;
use crate::flags::Flags;
use crate::fold::{AllInvariant, Engine, Hook, Step};
use crate::ids::{Node, Region, TAG_REGION, Ty};
use crate::infer::Vars;
use crate::tys::Tys;

/// Row-polymorphism tail variable (`..R`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RowVar(pub u32);

/// One member of a canonical [`TypeSet`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeSetMember {
    /// Lifetime-erased canonical key used for ordering, deduplication, lookup, and disjointness.
    pub key: Ty,
    /// Full member type preserving concrete lifetimes.
    pub ty: Ty,
}

/// Errors produced by type-set construction, extension, or row unification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeSetError {
    /// Two members have the same lifetime-erased key (e.g., `{Health, Health}` or `{&'a T, &'b T}`).
    DuplicateMember { key: Ty },
    /// Extending an open row `{.., ..R}` with `member` requires a `R: Lacks<member>` constraint.
    MissingLacksBound { row: RowVar, member: Ty },
    /// A concrete set violated an open row's `Lacks<member>` constraint.
    LacksViolated { row: RowVar, member: Ty },
    /// Expected member `key` was missing during row unification or projection.
    MissingMember { key: Ty },
    /// Closed set unification failed because an unexpected extra member `key` was present.
    UnexpectedMember { key: Ty },
    /// Cannot unify two sets that both have unresolved open row tails without a fresh join row.
    BothOpen { lhs: RowVar, rhs: RowVar },
    /// Disjoint borrow query named a member not in the set.
    UnknownMember { key: Ty },
}

/// Hook that replaces every region in a type with [`Region::ERASED`].
struct EraseRegions;

impl Hook for EraseRegions {
    type Error = Infallible;

    #[inline(always)]
    fn skip(&self, f: Flags, _: u32) -> bool {
        !f.has(Flags::FREE_REGIONS | Flags::RE_INFER | Flags::PARAM | Flags::PLACEHOLDER | Flags::CANONICAL)
            && f.outer_binder() == 0
    }

    #[inline]
    fn pre(&mut self, tys: &mut Tys<'_>, _vars: &mut Vars<'_>, n: Node, _: u32, _: u8) -> Result<Step, Infallible> {
        if n.tag() == TAG_REGION {
            if n == Region::ERASED.node() {
                return Ok(Step::Keep);
            }
            return Ok(Step::Replace(Region::ERASED.node()));
        }
        if self.skip(tys.node_flags(n), 0) {
            return Ok(Step::Keep);
        }
        Ok(Step::Descend)
    }
}

/// Returns `ty` with all lifetimes replaced by [`Region::ERASED`].
pub fn erase_regions<'a>(cx: &mut TyCx<'_>, ty: Ty, scratch: Alloc<'a>) -> Ty {
    let mut engine = Engine::new_in(scratch);
    let mut vo = AllInvariant;
    let mut hook = EraseRegions;
    let Ok(node) = engine.run(&mut cx.tys, &mut cx.vars, &mut vo, &mut hook, ty.node(), 0);
    node.as_ty().unwrap_or(ty)
}

/// Canonical, order-independent type set with optional open row tail `..R` and `Lacks<T>` bounds.
#[derive(Debug)]
pub struct TypeSet<'a> {
    /// Sorted strictly ascending by `member.key`.
    members: Vec<TypeSetMember, Alloc<'a>>,
    /// Optional open row variable `..R`.
    tail: Option<RowVar>,
    /// Sorted strictly ascending list of erased keys proven absent from `tail` (`R: Lacks<K>`).
    lacks: Vec<Ty, Alloc<'a>>,
}

/// Computed field placement in a canonical type-set struct layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FieldLayout {
    pub key: Ty,
    pub ty: Ty,
    pub offset: u32,
    pub size: u32,
    pub align: u32,
}

/// Total size and alignment of a canonical type-set layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SetLayout {
    pub size: u32,
    pub align: u32,
}

impl<'a> TypeSet<'a> {
    /// Constructs an empty closed type set `{}`.
    pub fn empty(alloc: Alloc<'a>) -> Self {
        Self {
            members: Vec::new_in(alloc),
            tail: None,
            lacks: Vec::new_in(alloc),
        }
    }

    /// Canonicalizes an unordered list of member types into a sorted, duplicate-free [`TypeSet`].
    /// Lifetimes are erased for key identity (`&'a T` and `&'b T` collide).
    pub fn from_members(
        cx: &mut TyCx<'_>,
        raw_members: &[Ty],
        tail: Option<RowVar>,
        lacks_raw: &[Ty],
        alloc: Alloc<'a>,
    ) -> Result<Self, TypeSetError> {
        let mut members = Vec::with_capacity_in(raw_members.len(), alloc);
        for &ty in raw_members {
            let key = erase_regions(cx, ty, alloc);
            members.push(TypeSetMember { key, ty });
        }
        members.sort_unstable_by_key(|m| m.key);
        for w in members.windows(2) {
            if w[0].key == w[1].key {
                return Err(TypeSetError::DuplicateMember { key: w[0].key });
            }
        }

        let mut lacks = Vec::with_capacity_in(lacks_raw.len(), alloc);
        for &ty in lacks_raw {
            lacks.push(erase_regions(cx, ty, alloc));
        }
        lacks.sort_unstable();
        lacks.dedup();

        Ok(Self { members, tail, lacks })
    }

    /// Canonical sorted member slice.
    #[inline]
    pub fn members(&self) -> &[TypeSetMember] {
        &self.members
    }

    /// Optional open row variable `..R`.
    #[inline]
    pub fn tail(&self) -> Option<RowVar> {
        self.tail
    }

    /// Sorted erased keys proven absent from `tail` (`R: Lacks<K>`).
    #[inline]
    pub fn lacks_bounds(&self) -> &[Ty] {
        &self.lacks
    }

    /// Number of explicit members.
    #[inline]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Returns `true` if there are no explicit members and no open row tail.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.tail.is_none()
    }

    /// Returns `true` iff `self` and `other` have identical canonical member keys, tail, and
    /// `Lacks` constraints — regardless of original declaration order.
    pub fn eq_canonical(&self, other: &TypeSet<'_>) -> bool {
        self.tail == other.tail
            && self.lacks.as_slice() == other.lacks.as_slice()
            && self.members.len() == other.members.len()
            && self
                .members
                .iter()
                .zip(other.members.iter())
                .all(|(a, b)| a.key == b.key && a.ty == b.ty)
    }

    /// O(log N) lookup of a member by its lifetime-erased key (`s.[Health]`).
    #[inline]
    pub fn lookup_key(&self, erased_key: Ty) -> Option<usize> {
        self.members.binary_search_by_key(&erased_key, |m| m.key).ok()
    }

    /// O(log N) lookup of a member by type (erasing lifetimes automatically).
    pub fn lookup(&self, cx: &mut TyCx<'_>, ty: Ty, scratch: Alloc<'_>) -> Option<usize> {
        let key = erase_regions(cx, ty, scratch);
        self.lookup_key(key)
    }

    /// Returns `true` iff `self` is proven not to contain `erased_key`.
    #[inline]
    pub fn is_proven_lacking(&self, erased_key: Ty) -> bool {
        if self.lookup_key(erased_key).is_some() {
            return false;
        }
        match self.tail {
            None => true,
            Some(_) => self.lacks.binary_search(&erased_key).is_ok(),
        }
    }

    /// Extends `self` with `ty` (`{Burning, ..e}` or `type Frigate = {Cargo, ..Ship}`).
    ///
    /// Rejects duplicates in closed sets and requires a `R: Lacks<ty>` bound when `self` has an
    /// open row tail `..R`.
    pub fn extend(&self, cx: &mut TyCx<'_>, ty: Ty, alloc: Alloc<'a>) -> Result<Self, TypeSetError> {
        let key = erase_regions(cx, ty, alloc);
        match self.members.binary_search_by_key(&key, |m| m.key) {
            Ok(_) => return Err(TypeSetError::DuplicateMember { key }),
            Err(pos) => {
                if let Some(row) = self.tail {
                    if self.lacks.binary_search(&key).is_err() {
                        return Err(TypeSetError::MissingLacksBound { row, member: key });
                    }
                }
                let mut members = Vec::with_capacity_in(self.members.len() + 1, alloc);
                members.extend_from_slice(&self.members[..pos]);
                members.push(TypeSetMember { key, ty });
                members.extend_from_slice(&self.members[pos..]);

                let mut lacks = Vec::with_capacity_in(self.lacks.len(), alloc);
                lacks.extend_from_slice(&self.lacks);
                Ok(Self { members, tail: self.tail, lacks })
            }
        }
    }

    /// Disjoint union of two closed type sets in O(N + M) sorted merge.
    pub fn union_disjoint(&self, other: &TypeSet<'_>, alloc: Alloc<'a>) -> Result<Self, TypeSetError> {
        if let (Some(lhs), Some(rhs)) = (self.tail, other.tail) {
            return Err(TypeSetError::BothOpen { lhs, rhs });
        }
        let mut members = Vec::with_capacity_in(self.members.len() + other.members.len(), alloc);
        let (mut i, mut j) = (0, 0);
        while i < self.members.len() && j < other.members.len() {
            let a = self.members[i];
            let b = other.members[j];
            if a.key == b.key {
                return Err(TypeSetError::DuplicateMember { key: a.key });
            } else if a.key < b.key {
                members.push(a);
                i += 1;
            } else {
                members.push(b);
                j += 1;
            }
        }
        members.extend_from_slice(&self.members[i..]);
        members.extend_from_slice(&other.members[j..]);

        let tail = self.tail.or(other.tail);
        let mut lacks = Vec::with_capacity_in(self.lacks.len() + other.lacks.len(), alloc);
        lacks.extend_from_slice(&self.lacks);
        lacks.extend_from_slice(&other.lacks);
        lacks.sort_unstable();
        lacks.dedup();

        // If one side was open, every explicit member from the other side must be proven lacked.
        if let Some(row) = self.tail {
            for m in &other.members {
                if self.lacks.binary_search(&m.key).is_err() {
                    return Err(TypeSetError::MissingLacksBound { row, member: m.key });
                }
            }
        } else if let Some(row) = other.tail {
            for m in &self.members {
                if other.lacks.binary_search(&m.key).is_err() {
                    return Err(TypeSetError::MissingLacksBound { row, member: m.key });
                }
            }
        }

        Ok(Self { members, tail, lacks })
    }

    /// Proves whether `(&mut s.[a], &s.[b])` or `(&mut s.[a], &mut s.[b])` are disjoint fields.
    pub fn can_borrow_disjoint(&self, cx: &mut TyCx<'_>, a: Ty, b: Ty, scratch: Alloc<'_>) -> Result<bool, TypeSetError> {
        let ka = erase_regions(cx, a, scratch);
        let kb = erase_regions(cx, b, scratch);
        if self.lookup_key(ka).is_none() {
            return Err(TypeSetError::UnknownMember { key: ka });
        }
        if self.lookup_key(kb).is_none() {
            return Err(TypeSetError::UnknownMember { key: kb });
        }
        Ok(ka != kb)
    }

    /// Unifies a pattern type set (`self`, e.g. `{Health, Armor, ..R}` with `R: Lacks<Burning>`)
    /// against a concrete closed type set `concrete` (e.g. `{Cargo, MaxSpeed, Health, Armor}`),
    /// returning the residual set bound to `..R` (`{Cargo, MaxSpeed}`).
    pub fn unify_open_row(&self, concrete: &TypeSet<'_>, alloc: Alloc<'a>) -> Result<Self, TypeSetError> {
        if let (Some(lhs), Some(rhs)) = (self.tail, concrete.tail) {
            return Err(TypeSetError::BothOpen { lhs, rhs });
        }

        let mut residual = Vec::new_in(alloc);
        let (mut i, mut j) = (0, 0);
        while i < self.members.len() && j < concrete.members.len() {
            let p = self.members[i];
            let c = concrete.members[j];
            if p.key == c.key {
                i += 1;
                j += 1;
            } else if p.key < c.key {
                return Err(TypeSetError::MissingMember { key: p.key });
            } else {
                // Member present in `concrete` but not explicitly named in `self` -> belongs to `..R`.
                if self.tail.is_none() {
                    return Err(TypeSetError::UnexpectedMember { key: c.key });
                }
                residual.push(c);
                j += 1;
            }
        }
        if i < self.members.len() {
            return Err(TypeSetError::MissingMember { key: self.members[i].key });
        }
        while j < concrete.members.len() {
            let c = concrete.members[j];
            if self.tail.is_none() {
                return Err(TypeSetError::UnexpectedMember { key: c.key });
            }
            residual.push(c);
            j += 1;
        }

        if let Some(row) = self.tail {
            for &forbidden in &self.lacks {
                if residual.binary_search_by_key(&forbidden, |m| m.key).is_ok() {
                    return Err(TypeSetError::LacksViolated { row, member: forbidden });
                }
            }
        }

        Ok(Self {
            members: residual,
            tail: None,
            lacks: Vec::new_in(alloc),
        })
    }

    /// Computes a deterministic, order-independent, alignment-descending physical struct layout
    /// from `(member_key, member_ty, size, align)` triples.
    pub fn canonical_layout(
        fields: &[(Ty, Ty, u32, u32)],
        alloc: Alloc<'a>,
    ) -> (Vec<FieldLayout, Alloc<'a>>, SetLayout) {
        let mut order = Vec::with_capacity_in(fields.len(), alloc);
        for &(key, ty, size, align) in fields {
            order.push((key, ty, size, align.max(1)));
        }
        // Sort by descending alignment, breaking ties by canonical key so layout is 100%
        // independent of member declaration order.
        order.sort_unstable_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));

        let mut out = Vec::with_capacity_in(order.len(), alloc);
        let mut offset = 0u32;
        let mut max_align = 1u32;
        for (key, ty, size, align) in order {
            max_align = max_align.max(align);
            let rem = offset % align;
            if rem != 0 {
                offset = offset.saturating_add(align - rem);
            }
            out.push(FieldLayout { key, ty, offset, size, align });
            offset = offset.saturating_add(size);
        }
        let rem = offset % max_align;
        if rem != 0 {
            offset = offset.saturating_add(max_align - rem);
        }
        (out, SetLayout { size: offset, align: max_align })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{Def, IntTy, List, Mutability, Region, UintTy};

    #[test]
    fn canonical_order_lifetime_erasure_row_unification_and_lacks() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        // Distinct nominal types via ADT defs.
        let health = cx.tys.adt(Def(1), List::EMPTY);
        let max_speed = cx.tys.adt(Def(2), List::EMPTY);
        let armor = cx.tys.adt(Def(3), List::EMPTY);
        let burning = cx.tys.adt(Def(4), List::EMPTY);
        let cargo = cx.tys.adt(Def(5), List::EMPTY);

        // 1. Order independence: {MaxSpeed, Health} == {Health, MaxSpeed}.
        let ship_a = TypeSet::from_members(&mut cx, &[max_speed, health], None, &[], alloc).unwrap();
        let ship_b = TypeSet::from_members(&mut cx, &[health, max_speed], None, &[], alloc).unwrap();
        assert!(ship_a.eq_canonical(&ship_b));

        // 2. Duplicate rejection: {Health, Health} fails.
        assert_eq!(
            TypeSet::from_members(&mut cx, &[health, health], None, &[], alloc).unwrap_err(),
            TypeSetError::DuplicateMember { key: health }
        );

        // 3. Lifetime erasure: {&'static u32, &'param0 u32} collide as duplicates!
        let u32_ty = Ty::uint(UintTy::U32);
        let ref_static = cx.tys.reference(Region::STATIC, u32_ty, Mutability::Not);
        let re0 = cx.tys.re_early_param(0);
        let ref_param = cx.tys.reference(re0, u32_ty, Mutability::Not);
        let erased_ref = erase_regions(&mut cx, ref_static, alloc);
        assert_eq!(erased_ref, erase_regions(&mut cx, ref_param, alloc));
        assert_eq!(
            TypeSet::from_members(&mut cx, &[ref_static, ref_param], None, &[], alloc).unwrap_err(),
            TypeSetError::DuplicateMember { key: erased_ref }
        );

        // 4. Disjoint borrow check: Health vs MaxSpeed is disjoint; Health vs Health is NOT.
        assert_eq!(ship_a.can_borrow_disjoint(&mut cx, health, max_speed, alloc), Ok(true));
        assert_eq!(ship_a.can_borrow_disjoint(&mut cx, health, health, alloc), Ok(false));

        // 5. Type extension: Frigate = {Cargo, ..Ship}.
        let frigate = ship_a.extend(&mut cx, cargo, alloc).unwrap();
        assert_eq!(frigate.len(), 3);
        assert!(frigate.lookup(&mut cx, cargo, alloc).is_some());

        // 6. Open row unification & Lacks<Burning> bound.
        let r = RowVar(0);
        let open_no_lacks = TypeSet::from_members(&mut cx, &[health], Some(r), &[], alloc).unwrap();
        assert_eq!(
            open_no_lacks.extend(&mut cx, burning, alloc).unwrap_err(),
            TypeSetError::MissingLacksBound { row: r, member: burning }
        );

        let open_with_lacks = TypeSet::from_members(&mut cx, &[health, armor], Some(r), &[burning], alloc).unwrap();
        let ignited_open = open_with_lacks.extend(&mut cx, burning, alloc).unwrap();
        assert_eq!(ignited_open.len(), 3);

        // Unify {Health, Armor, ..R} (where R: Lacks<Burning>) against {Cargo, MaxSpeed, Health, Armor}.
        let cruiser = TypeSet::from_members(&mut cx, &[cargo, max_speed, health, armor], None, &[], alloc).unwrap();
        let solved_r = open_with_lacks.unify_open_row(&cruiser, alloc).unwrap();
        assert!(solved_r.eq_canonical(&TypeSet::from_members(&mut cx, &[cargo, max_speed], None, &[], alloc).unwrap()));

        // Concrete set that already contains Burning violates `R: Lacks<Burning>`.
        let already_burning = cruiser.extend(&mut cx, burning, alloc).unwrap();
        assert_eq!(
            open_with_lacks.unify_open_row(&already_burning, alloc).unwrap_err(),
            TypeSetError::LacksViolated { row: r, member: burning }
        );

        // 7. Canonical alignment-descending layout is order-independent and zero-waste.
        let i64_ty = Ty::int(IntTy::I64);
        let u8_ty = Ty::uint(UintTy::U8);
        let (lay1, tot1) = TypeSet::canonical_layout(
            &[(u8_ty, u8_ty, 1, 1), (i64_ty, i64_ty, 8, 8), (u32_ty, u32_ty, 4, 4)],
            alloc,
        );
        let (lay2, tot2) = TypeSet::canonical_layout(
            &[(u32_ty, u32_ty, 4, 4), (u8_ty, u8_ty, 1, 1), (i64_ty, i64_ty, 8, 8)],
            alloc,
        );
        assert_eq!(lay1.as_slice(), lay2.as_slice());
        assert_eq!(tot1, tot2);
        assert_eq!(tot1, SetLayout { size: 16, align: 8 });
        assert_eq!(lay1[0].offset, 0); // i64 at 0..8
        assert_eq!(lay1[1].offset, 8); // u32 at 8..12
        assert_eq!(lay1[2].offset, 12); // u8 at 12..13, padded to 16
    }
}

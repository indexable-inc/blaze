//! Ergonomic Compile-Time Type Descriptions (`blaze-ty::type_desc`).
//!
//! Replaces positional `Arg(0)` / numeric `FieldOf(&FieldOf(&Arg(0), t), c)` indirection in
//! `type_of!` (`docs/v2/plan/design/type-of.md`) with direct opaque compiler-interned type
//! handles (`TypeDescNode::Of(Ty)` / `TypeDesc::of::<T>()`), named field projections
//! (`FieldByName`), and first-class construction of records, unions, and pattern types.
//!
//! Evaluation is strictly iterative (zero recursion) and bounded by [`MAX_DESC_NODES`], with
//! structured [`TypeDescError`] diagnostics naming the exact failing field or node.

use core::fmt;

use blaze_own::{Alloc, Vec};

use crate::cx::TyCx;
use crate::ids::{Arg, Mutability, Region, Ty};
use crate::pat_ty::{IntInterval, IntPatTy, PatTyError};
use crate::record_ty::{RecordError, RecordTy};
use crate::union_ty::{UnionError, UnionTy};

/// Hard node-visit bound matching `VALTREE_MAX_NODES` so malformed/cyclic descriptions terminate.
pub const MAX_DESC_NODES: usize = 4096;

/// Index into a [`TypeDescArena`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DescId(pub u32);

/// One named field inside a [`TypeDescNode::Record`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FieldDesc<'s> {
    pub name: &'s str,
    pub ty: DescId,
}

/// One node in an arena-allocated compile-time type description graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeDescNode<'s> {
    /// Direct opaque type handle (`TypeDesc::of::<T>()`): eliminates positional `Arg(0)` tuples!
    Of(Ty),
    /// Named field lookup on a struct or record (`FieldByName(&desc, "age")`), replacing brittle
    /// declaration-order indices.
    FieldByName { base: DescId, name: &'s str },
    /// Reference `&'r T` or `&'r uniq T`.
    Ref { mutbl: Mutability, inner: DescId },
    /// Slice `[T]` (valid directly under `Ref`).
    Slice(DescId),
    /// Tuple `(T0, T1, ...)`.
    Tuple { start: u32, len: u32 },
    /// Structural record `{ name: T, ... }` (arbitrary arity, canonicalized by field name).
    Record { start: u32, len: u32 },
    /// Structural union `A | B | ...` (arbitrary arity, canonicalized and deduplicated).
    Union { start: u32, len: u32 },
    /// Integer pattern type `base is LO..=HI`.
    IntPat { base: DescId, bit_width: u8, signed: bool, lo: i128, hi: i128 },
}

/// Resolved output of evaluating a [`TypeDescArena`] root node.
#[derive(Debug)]
pub enum EvaluatedDesc<'a, 's> {
    /// Plain interned type (including tuples, refs, slices, and `ty::PAT`).
    Ty(Ty),
    /// Canonical structural record type (sorted by field name, arbitrary arity).
    Record(RecordTy<'a, 's>),
    /// Canonical structural union type (sorted, deduplicated, arbitrary arity).
    Union(UnionTy<'a>),
    /// Canonical integer pattern type (also interned into `cx.tys` as `ty::PAT`).
    IntPat { ty: Ty, pat: IntPatTy<'a> },
}

/// Typed diagnostic returned when a [`TypeDescArena`] description is invalid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeDescError<'s> {
    /// Referenced [`DescId`] does not exist in the arena.
    InvalidNode(DescId),
    /// Evaluation exceeded [`MAX_DESC_NODES`] (cycle or oversized tree).
    NodeLimitExceeded,
    /// `FieldByName` target did not resolve to a struct or record containing `name`.
    FieldNotFound { field: &'s str },
    /// Bare unsized `str` or `[T]` appeared outside a `&` / `&uniq` reference.
    BareUnsized(DescId),
    /// Record construction failed (duplicate/invalid field name).
    Record(RecordError<'s>),
    /// Union construction failed (overlapping generic params).
    Union(UnionError),
    /// Pattern-type construction failed (inverted or out-of-domain bounds).
    Pat(PatTyError),
}

impl fmt::Display for TypeDescError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TypeDescError::InvalidNode(id) => write!(f, "invalid TypeDesc node id {}", id.0),
            TypeDescError::NodeLimitExceeded => write!(f, "TypeDesc evaluation exceeded node limit"),
            TypeDescError::FieldNotFound { field } => {
                write!(f, "TypeDesc `FieldByName`: field '{field}' not found on base type")
            }
            TypeDescError::BareUnsized(id) => {
                write!(f, "unsized `str` or `[T]` at TypeDesc node {} must appear under `&` or `&uniq`", id.0)
            }
            TypeDescError::Record(e) => write!(f, "TypeDesc record error: {e}"),
            TypeDescError::Union(e) => write!(f, "TypeDesc union error: {e:?}"),
            TypeDescError::Pat(e) => write!(f, "TypeDesc pattern-type error: {e}"),
        }
    }
}

/// Arena storing a compile-time `TypeDesc` tree cleanly without heap-allocated recursive enums.
#[derive(Debug)]
pub struct TypeDescArena<'a, 's> {
    nodes: Vec<TypeDescNode<'s>, Alloc<'a>>,
    children: Vec<DescId, Alloc<'a>>,
    fields: Vec<FieldDesc<'s>, Alloc<'a>>,
}

impl<'a, 's> TypeDescArena<'a, 's> {
    /// Creates an empty [`TypeDescArena`] backed by `alloc`.
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self {
            nodes: Vec::new_in(alloc),
            children: Vec::new_in(alloc),
            fields: Vec::new_in(alloc),
        }
    }

    fn push_node(&mut self, node: TypeDescNode<'s>) -> DescId {
        let id = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(node);
        DescId(id)
    }

    /// Direct opaque type handle (`TypeDesc::of::<T>()`): no `Arg(i)` index needed.
    pub fn of_ty(&mut self, ty: Ty) -> DescId {
        self.push_node(TypeDescNode::Of(ty))
    }

    /// Named field lookup on a struct or record (`FieldByName(base, "age")`).
    pub fn field_by_name(&mut self, base: DescId, name: &'s str) -> DescId {
        self.push_node(TypeDescNode::FieldByName { base, name })
    }

    /// Reference `&T` or `&uniq T`.
    pub fn reference(&mut self, mutbl: Mutability, inner: DescId) -> DescId {
        self.push_node(TypeDescNode::Ref { mutbl, inner })
    }

    /// Slice `[T]`.
    pub fn slice(&mut self, inner: DescId) -> DescId {
        self.push_node(TypeDescNode::Slice(inner))
    }

    /// Tuple `(T0, T1, ...)`.
    pub fn tuple(&mut self, elems: &[DescId]) -> DescId {
        let start = u32::try_from(self.children.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(elems.len()).unwrap_or(u32::MAX);
        self.children.extend_from_slice(elems);
        self.push_node(TypeDescNode::Tuple { start, len })
    }

    /// Structural record `{ name: T, ... }`.
    pub fn record(&mut self, fields: &[FieldDesc<'s>]) -> DescId {
        let start = u32::try_from(self.fields.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(fields.len()).unwrap_or(u32::MAX);
        self.fields.extend_from_slice(fields);
        self.push_node(TypeDescNode::Record { start, len })
    }

    /// Structural union `A | B | ...`.
    pub fn union(&mut self, members: &[DescId]) -> DescId {
        let start = u32::try_from(self.children.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(members.len()).unwrap_or(u32::MAX);
        self.children.extend_from_slice(members);
        self.push_node(TypeDescNode::Union { start, len })
    }

    /// Integer pattern type `base is lo..=hi`.
    pub fn int_pat(&mut self, base: DescId, bit_width: u8, signed: bool, lo: i128, hi: i128) -> DescId {
        self.push_node(TypeDescNode::IntPat { base, bit_width, signed, lo, hi })
    }

    fn node(&self, id: DescId) -> Result<TypeDescNode<'s>, TypeDescError<'s>> {
        let idx = usize::try_from(id.0).map_err(|_| TypeDescError::InvalidNode(id))?;
        self.nodes.get(idx).copied().ok_or(TypeDescError::InvalidNode(id))
    }

    fn child_slice(&self, start: u32, len: u32, owner: DescId) -> Result<&[DescId], TypeDescError<'s>> {
        let s = usize::try_from(start).map_err(|_| TypeDescError::InvalidNode(owner))?;
        let l = usize::try_from(len).map_err(|_| TypeDescError::InvalidNode(owner))?;
        let e = s.checked_add(l).ok_or(TypeDescError::InvalidNode(owner))?;
        self.children.get(s..e).ok_or(TypeDescError::InvalidNode(owner))
    }

    fn field_slice(&self, start: u32, len: u32, owner: DescId) -> Result<&[FieldDesc<'s>], TypeDescError<'s>> {
        let s = usize::try_from(start).map_err(|_| TypeDescError::InvalidNode(owner))?;
        let l = usize::try_from(len).map_err(|_| TypeDescError::InvalidNode(owner))?;
        let e = s.checked_add(l).ok_or(TypeDescError::InvalidNode(owner))?;
        self.fields.get(s..e).ok_or(TypeDescError::InvalidNode(owner))
    }

    /// Evaluates a single [`DescId`] to a plain interned [`Ty`] iteratively (no recursion),
    /// resolving `FieldByName` against `lookup_struct_field` (for named ADTs) and nested records.
    fn eval_ty(
        &self,
        cx: &mut TyCx<'_>,
        root: DescId,
        ref_region: Region,
        lookup_struct_field: &impl Fn(Ty, &str) -> Option<Ty>,
        scratch: Alloc<'_>,
    ) -> Result<Ty, TypeDescError<'s>> {
        // Post-order iterative evaluation with memoized `Option<Ty>` per node.
        let mut results: Vec<Option<Ty>, Alloc<'_>> = Vec::with_capacity_in(self.nodes.len(), scratch);
        results.resize(self.nodes.len(), None);
        let mut stack: Vec<(DescId, bool), Alloc<'_>> = Vec::with_capacity_in(16, scratch);
        stack.push((root, false));
        let mut visits = 0usize;

        while let Some((id, ready)) = stack.pop() {
            visits += 1;
            if visits > MAX_DESC_NODES {
                return Err(TypeDescError::NodeLimitExceeded);
            }
            let idx = usize::try_from(id.0).map_err(|_| TypeDescError::InvalidNode(id))?;
            if results.get(idx).copied().flatten().is_some() {
                continue;
            }
            let node = self.node(id)?;
            if !ready {
                stack.push((id, true));
                match node {
                    TypeDescNode::Of(_) => {}
                    TypeDescNode::FieldByName { base, .. }
                    | TypeDescNode::Ref { inner: base, .. }
                    | TypeDescNode::Slice(base)
                    | TypeDescNode::IntPat { base, .. } => {
                        stack.push((base, false));
                    }
                    TypeDescNode::Tuple { start, len } | TypeDescNode::Union { start, len } => {
                        for &c in self.child_slice(start, len, id)? {
                            stack.push((c, false));
                        }
                    }
                    TypeDescNode::Record { start, len } => {
                        for f in self.field_slice(start, len, id)? {
                            stack.push((f.ty, false));
                        }
                    }
                }
                continue;
            }

            let resolved = match node {
                TypeDescNode::Of(ty) => ty,
                TypeDescNode::FieldByName { base, name } => {
                    // Support querying a field directly from a nested `Record` description or an ADT!
                    if let TypeDescNode::Record { start, len } = self.node(base)? {
                        let fields = self.field_slice(start, len, base)?;
                        let Some(found) = fields.iter().find(|f| f.name == name) else {
                            return Err(TypeDescError::FieldNotFound { field: name });
                        };
                        let fidx = usize::try_from(found.ty.0).map_err(|_| TypeDescError::InvalidNode(found.ty))?;
                        results.get(fidx).copied().flatten().ok_or(TypeDescError::InvalidNode(found.ty))?
                    } else {
                        let bidx = usize::try_from(base.0).map_err(|_| TypeDescError::InvalidNode(base))?;
                        let base_ty = results.get(bidx).copied().flatten().ok_or(TypeDescError::InvalidNode(base))?;
                        lookup_struct_field(base_ty, name).ok_or(TypeDescError::FieldNotFound { field: name })?
                    }
                }
                TypeDescNode::Ref { mutbl, inner } => {
                    let iidx = usize::try_from(inner.0).map_err(|_| TypeDescError::InvalidNode(inner))?;
                    let inner_ty = results.get(iidx).copied().flatten().ok_or(TypeDescError::InvalidNode(inner))?;
                    cx.tys.reference(ref_region, inner_ty, mutbl)
                }
                TypeDescNode::Slice(inner) => {
                    let iidx = usize::try_from(inner.0).map_err(|_| TypeDescError::InvalidNode(inner))?;
                    let inner_ty = results.get(iidx).copied().flatten().ok_or(TypeDescError::InvalidNode(inner))?;
                    cx.tys.slice(inner_ty)
                }
                TypeDescNode::Tuple { start, len } => {
                    let cs = self.child_slice(start, len, id)?;
                    let mut args: Vec<Arg, Alloc<'_>> = Vec::with_capacity_in(cs.len(), scratch);
                    for &c in cs {
                        let cidx = usize::try_from(c.0).map_err(|_| TypeDescError::InvalidNode(c))?;
                        let ty = results.get(cidx).copied().flatten().ok_or(TypeDescError::InvalidNode(c))?;
                        if ty == Ty::STR || matches!(cx.tys.ty_kind(ty), crate::tys::TyKind::Slice(_)) {
                            return Err(TypeDescError::BareUnsized(c));
                        }
                        args.push(ty.arg());
                    }
                    let list = cx.tys.mk_list(&args);
                    cx.tys.tuple(list)
                }
                TypeDescNode::Record { start, len } => {
                    let fs = self.field_slice(start, len, id)?;
                    let mut pairs: Vec<(&'s str, Ty), Alloc<'_>> = Vec::with_capacity_in(fs.len(), scratch);
                    for f in fs {
                        let fidx = usize::try_from(f.ty.0).map_err(|_| TypeDescError::InvalidNode(f.ty))?;
                        let ty = results.get(fidx).copied().flatten().ok_or(TypeDescError::InvalidNode(f.ty))?;
                        if ty == Ty::STR || matches!(cx.tys.ty_kind(ty), crate::tys::TyKind::Slice(_)) {
                            return Err(TypeDescError::BareUnsized(f.ty));
                        }
                        pairs.push((f.name, ty));
                    }
                    let rec = RecordTy::from_fields(cx, &pairs, scratch).map_err(TypeDescError::Record)?;
                    let mut args: Vec<Arg, Alloc<'_>> = Vec::with_capacity_in(rec.len(), scratch);
                    for rf in rec.fields() {
                        args.push(rf.ty.arg());
                    }
                    let list = cx.tys.mk_list(&args);
                    cx.tys.tuple(list)
                }
                TypeDescNode::Union { start, len } => {
                    let cs = self.child_slice(start, len, id)?;
                    let mut mems: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(cs.len(), scratch);
                    for &c in cs {
                        let cidx = usize::try_from(c.0).map_err(|_| TypeDescError::InvalidNode(c))?;
                        let ty = results.get(cidx).copied().flatten().ok_or(TypeDescError::InvalidNode(c))?;
                        mems.push(ty);
                    }
                    let u = UnionTy::from_members(cx, &mems, scratch).map_err(TypeDescError::Union)?;
                    let mut args: Vec<Arg, Alloc<'_>> = Vec::with_capacity_in(u.len(), scratch);
                    for m in u.members() {
                        args.push(m.ty.arg());
                    }
                    let list = cx.tys.mk_list(&args);
                    cx.tys.tuple(list)
                }
                TypeDescNode::IntPat { base, bit_width, signed, lo, hi } => {
                    let bidx = usize::try_from(base.0).map_err(|_| TypeDescError::InvalidNode(base))?;
                    let base_ty = results.get(bidx).copied().flatten().ok_or(TypeDescError::InvalidNode(base))?;
                    let iv = IntInterval::new(lo, hi).map_err(TypeDescError::Pat)?;
                    let pat = IntPatTy::from_ranges(base_ty, bit_width, signed, &[iv], scratch)
                        .map_err(TypeDescError::Pat)?;
                    pat.intern_into(cx, scratch)
                }
            };

            if let Some(slot) = results.get_mut(idx) {
                *slot = Some(resolved);
            }
        }

        let ridx = usize::try_from(root.0).map_err(|_| TypeDescError::InvalidNode(root))?;
        let out = results.get(ridx).copied().flatten().ok_or(TypeDescError::InvalidNode(root))?;
        if out == Ty::STR || matches!(cx.tys.ty_kind(out), crate::tys::TyKind::Slice(_)) {
            return Err(TypeDescError::BareUnsized(root));
        }
        Ok(out)
    }

    /// Evaluates `root` into a rich [`EvaluatedDesc`] (preserving full [`RecordTy`], [`UnionTy`],
    /// or [`IntPatTy`] structure at top level).
    pub fn eval<'b>(
        &self,
        cx: &mut TyCx<'_>,
        root: DescId,
        ref_region: Region,
        lookup_struct_field: impl Fn(Ty, &str) -> Option<Ty>,
        alloc: Alloc<'b>,
    ) -> Result<EvaluatedDesc<'b, 's>, TypeDescError<'s>> {
        match self.node(root)? {
            TypeDescNode::Record { start, len } => {
                let fs = self.field_slice(start, len, root)?;
                let mut pairs: Vec<(&'s str, Ty), Alloc<'b>> = Vec::with_capacity_in(fs.len(), alloc);
                for f in fs {
                    let ty = self.eval_ty(cx, f.ty, ref_region, &lookup_struct_field, alloc)?;
                    pairs.push((f.name, ty));
                }
                let rec = RecordTy::from_fields(cx, &pairs, alloc).map_err(TypeDescError::Record)?;
                Ok(EvaluatedDesc::Record(rec))
            }
            TypeDescNode::Union { start, len } => {
                let cs = self.child_slice(start, len, root)?;
                let mut mems: Vec<Ty, Alloc<'b>> = Vec::with_capacity_in(cs.len(), alloc);
                for &c in cs {
                    mems.push(self.eval_ty(cx, c, ref_region, &lookup_struct_field, alloc)?);
                }
                let u = UnionTy::from_members(cx, &mems, alloc).map_err(TypeDescError::Union)?;
                Ok(EvaluatedDesc::Union(u))
            }
            TypeDescNode::IntPat { base, bit_width, signed, lo, hi } => {
                let base_ty = self.eval_ty(cx, base, ref_region, &lookup_struct_field, alloc)?;
                let iv = IntInterval::new(lo, hi).map_err(TypeDescError::Pat)?;
                let pat = IntPatTy::from_ranges(base_ty, bit_width, signed, &[iv], alloc)
                    .map_err(TypeDescError::Pat)?;
                let ty = pat.intern_into(cx, alloc);
                Ok(EvaluatedDesc::IntPat { ty, pat })
            }
            _ => {
                let ty = self.eval_ty(cx, root, ref_region, &lookup_struct_field, alloc)?;
                Ok(EvaluatedDesc::Ty(ty))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;
    use crate::ids::{Def, IntTy, List, UintTy};

    #[test]
    fn direct_of_ty_and_named_field_lookup_eliminate_positional_arg_indices() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);

        let person = cx.tys.adt(Def(1), List::EMPTY);
        let string_ty = cx.tys.adt(Def(2), List::EMPTY);
        let i64_ty = Ty::int(IntTy::I64);
        let u32_ty = Ty::uint(UintTy::U32);

        let mut arena = TypeDescArena::new(a);
        // Zero `Arg(0)` or numeric `FieldOf(&Arg(0), 2)` — use direct `of_ty` + `field_by_name`!
        let d_person = arena.of_ty(person);
        let d_name = arena.field_by_name(d_person, "name");
        let d_age = arena.field_by_name(d_person, "age");
        let d_u32 = arena.of_ty(u32_ty);
        let d_score = arena.int_pat(d_u32, 32, false, 0, 100);
        let d_row = arena.record(&[
            FieldDesc { name: "name", ty: d_name },
            FieldDesc { name: "age", ty: d_age },
            FieldDesc { name: "score", ty: d_score },
        ]);

        let lookup = |ty: Ty, field: &str| -> Option<Ty> {
            if ty == person {
                match field {
                    "name" => Some(string_ty),
                    "age" => Some(i64_ty),
                    _ => None,
                }
            } else {
                None
            }
        };

        let EvaluatedDesc::Record(row) = arena.eval(&mut cx, d_row, Region::STATIC, lookup, a).unwrap() else {
            panic!("expected Record");
        };
        assert_eq!(row.canonical_fields_str(a).as_str(), "age,name,score");
        assert_eq!(row.field("age").unwrap().ty, i64_ty);
        assert_eq!(row.field("name").unwrap().ty, string_ty);
        assert!(matches!(cx.tys.ty_kind(row.field("score").unwrap().ty), crate::tys::TyKind::Pat(b, _) if b == u32_ty));

        // Missing field reports exact field name in diagnostic.
        let d_bad = arena.field_by_name(d_person, "email");
        assert_eq!(
            arena.eval(&mut cx, d_bad, Region::STATIC, lookup, a).unwrap_err(),
            TypeDescError::FieldNotFound { field: "email" }
        );

        // Bare `str` outside `&` is refused; `&str` succeeds.
        let d_bare_str = arena.of_ty(Ty::STR);
        assert_eq!(
            arena.eval(&mut cx, d_bare_str, Region::STATIC, lookup, a).unwrap_err(),
            TypeDescError::BareUnsized(d_bare_str)
        );
        let d_ref_str = arena.reference(Mutability::Not, d_bare_str);
        assert!(matches!(
            arena.eval(&mut cx, d_ref_str, Region::STATIC, lookup, a).unwrap(),
            EvaluatedDesc::Ty(_)
        ));
    }

    #[test]
    fn type_desc_builds_canonical_unions_and_pattern_types() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);

        let fs_err = cx.tys.adt(Def(10), List::EMPTY);
        let net_err = cx.tys.adt(Def(11), List::EMPTY);

        let mut arena = TypeDescArena::new(a);
        let d_net = arena.of_ty(net_err);
        let d_fs = arena.of_ty(fs_err);
        let d_u = arena.union(&[d_net, d_fs, d_net]);
        let EvaluatedDesc::Union(u) = arena.eval(&mut cx, d_u, Region::STATIC, |_, _| None, a).unwrap() else {
            panic!("expected Union");
        };
        assert_eq!(u.len(), 2);

        let d_u8 = arena.of_ty(Ty::uint(UintTy::U8));
        let d_pat = arena.int_pat(d_u8, 8, false, 1, 100);
        let EvaluatedDesc::IntPat { pat, .. } =
            arena.eval(&mut cx, d_pat, Region::STATIC, |_, _| None, a).unwrap()
        else {
            panic!("expected IntPat");
        };
        assert_eq!(pat.niche_count(), 156);
    }
}

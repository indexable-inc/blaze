//! Comptime field unrolling (`inline for pat in iter { body }`) in `front2`.
//!
//! Unrolls `inline for` over structural records, tuples, and single-variant structs (both by
//! value and through `&T` / `&mut T`) into straight-line blocks so every iteration sees its
//! field's exact concrete type and compiles to zero-overhead native code.
//!
//! - Concrete functions: unrolled iteratively before final checking (`unroll_concrete_body`).
//! - Generic functions (`fn f<T>(bundle: T)`): monomorphized per concrete call argument tuple
//!   (`specialize_generic_inline_fors`), creating a specialized `FnHead` + unrolled `Body` +
//!   `Checked` and reaching every newly discovered callee.

use core::fmt::Write as _;

use blaze_hash::HashMap;
use blaze_ids::Hasher;
use blaze_lower::scoped::Prepares;
use blaze_lower::typeck::{InlineForKind, InlineForSite, TypeckBody};
use blaze_own::{AString, AVec, Alloc, String as OwnString, Vec as OwnVec};
use blaze_typeck::{
    self as tc, Checked, ItemKey, List, Mutbl, PNode, PTy, PTys, PortableTys,
};

use super::body::{Body, lower_body_with_text};
use super::check::{CheckFail, Records, check_body, check_rounds_probe, check_rounds_strict, resolve_instance_full};
use super::heads::{FnHead, Want};
use super::heads_regions::{Scope as RegionScope, Src};
use super::preds::Preds;
use super::reach::{Census, First, reach};
use super::source::NamesSource;
use super::spell::Spelling;
use super::types::{Heads as _, SelfTy};

/// Maximum nested `inline for` unrolling rounds per body.
pub const MAX_UNROLL_ROUNDS: usize = 8;

/// Maximum specialization worklist rounds across generic `inline for` functions.
pub const MAX_SPEC_ROUNDS: usize = 32;

/// Maximum nodes compared/walked in one type traversal (bounds iterative worklists).
const MAX_TY_WALK: usize = 4096;

/// One tabled specialization of a generic `inline for` function on `Front2Program`.
pub struct SpecRow {
    pub orig: ItemKey,
    pub args: OwnVec<PTy, Alloc<'static>>,
    pub row: usize,
}

/// One specialized generic `inline for` body before it is tabled on `Front2Program`.
pub struct SpecializedBody<'n> {
    pub orig: ItemKey,
    pub args: OwnVec<PTy, Alloc<'static>>,
    pub spec_key: ItemKey,
    pub body: Body<'n>,
    pub checked: Checked<'static>,
}

/// Substitutes `Param(i)` / `ConstParam(i)` with `args[i]` across every `PTy` held in `typeck`.
pub fn subst_typeck_body(
    typeck: &mut TypeckBody<'_>,
    tys: &mut PortableTys<'_>,
    args: &[PTy],
) -> Result<(), tc::CheckError> {
    for t in typeck.type_lists.iter_mut() {
        *t = tys.subst(*t, args)?;
    }
    for slot in typeck.opt_types.iter_mut() {
        if let Some(t) = slot {
            *t = tys.subst(*t, args)?;
        }
    }
    for (_, t) in typeck.legacy_args.iter_mut() {
        *t = tys.subst(*t, args)?;
    }
    for expr in typeck.exprs.iter_mut() {
        match expr {
            tc::Expr::Cast { ty, .. } | tc::Expr::OffsetOf { ty, .. } => {
                *ty = tys.subst(*ty, args)?;
            }
            tc::Expr::Repeat { len, .. } => {
                *len = tys.subst(*len, args)?;
            }
            tc::Expr::Closure { ret: Some(t), .. } => {
                *t = tys.subst(*t, args)?;
            }
            _ => {}
        }
    }
    for pat in typeck.pats.iter_mut() {
        if let tc::Pat::Bind { ty: Some(t), .. } = pat {
            *t = tys.subst(*t, args)?;
        }
    }
    for stmt in typeck.stmts.iter_mut() {
        if let tc::Stmt::Let { ty: Some(t), .. } = stmt {
            *t = tys.subst(*t, args)?;
        }
    }
    if let Some(tc::CoroutineKind::Async { output }) = &mut typeck.coroutine {
        *output = tys.subst(*output, args)?;
    }
    Ok(())
}

/// Structural equality of two `PTy` argument slices stored in possibly distinct `PTys` tables.
pub fn args_equal(
    a_tys: PTys<'_>,
    a_args: &[PTy],
    b_tys: PTys<'_>,
    b_args: &[PTy],
    alloc: Alloc<'_>,
) -> bool {
    if a_args.len() != b_args.len() {
        return false;
    }
    let mut stack: AVec<'_, (PTy, PTy)> = OwnVec::with_capacity_in(a_args.len(), alloc);
    for (&a, &b) in a_args.iter().zip(b_args.iter()) {
        stack.push((a, b));
    }
    let mut steps = 0usize;
    while let Some((a, b)) = stack.pop() {
        steps += 1;
        if steps > MAX_TY_WALK {
            return false;
        }
        let (Ok(na), Ok(nb)) = (a_tys.node(a), b_tys.node(b)) else {
            return false;
        };
        let push_lists = |la: List, lb: List, st: &mut AVec<'_, (PTy, PTy)>| -> bool {
            let (Ok(xs), Ok(ys)) = (a_tys.list(la), b_tys.list(lb)) else {
                return false;
            };
            if xs.len() != ys.len() {
                return false;
            }
            for (&x, &y) in xs.iter().zip(ys.iter()) {
                st.push((x, y));
            }
            true
        };
        match (na, nb) {
            (PNode::Bool, PNode::Bool)
            | (PNode::Char, PNode::Char)
            | (PNode::Str, PNode::Str)
            | (PNode::Never, PNode::Never)
            | (PNode::Infer, PNode::Infer)
            | (PNode::Error, PNode::Error) => {}
            (PNode::Int(x), PNode::Int(y)) if x == y => {}
            (PNode::Uint(x), PNode::Uint(y)) if x == y => {}
            (PNode::Float(x), PNode::Float(y)) if x == y => {}
            (PNode::Param(x), PNode::Param(y)) if x == y => {}
            (PNode::ConstParam(x), PNode::ConstParam(y)) if x == y => {}
            (PNode::ExAuto(x), PNode::ExAuto(y)) if x == y => {}
            (PNode::Foreign(x), PNode::Foreign(y)) if x == y => {}
            (PNode::ConstValue { ty: ta, bits: ba }, PNode::ConstValue { ty: tb, bits: bb })
                if ba == bb =>
            {
                stack.push((ta, tb));
            }
            (PNode::Ref(ma, xa), PNode::Ref(mb, xb)) if ma == mb => stack.push((xa, xb)),
            (PNode::Ptr(ma, xa), PNode::Ptr(mb, xb)) if ma == mb => stack.push((xa, xb)),
            (PNode::Slice(xa), PNode::Slice(xb)) => stack.push((xa, xb)),
            (PNode::Array(ea, la), PNode::Array(eb, lb)) => {
                stack.push((ea, eb));
                stack.push((la, lb));
            }
            (PNode::Tuple(la), PNode::Tuple(lb)) | (PNode::Union(la), PNode::Union(lb)) => {
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (PNode::Record(sa, la), PNode::Record(sb, lb)) if sa == sb => {
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (PNode::Pat(ba, la), PNode::Pat(bb, lb)) => {
                stack.push((ba, bb));
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (PNode::Adt(ka, la), PNode::Adt(kb, lb))
            | (PNode::FnDef(ka, la), PNode::FnDef(kb, lb))
            | (PNode::Opaque(ka, la), PNode::Opaque(kb, lb))
            | (PNode::ExTrait(ka, la), PNode::ExTrait(kb, lb))
                if ka == kb =>
            {
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (PNode::FnPtr(la, ha), PNode::FnPtr(lb, hb)) if ha == hb => {
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (
                PNode::Closure { owner: oa, ordinal: ia, args: la },
                PNode::Closure { owner: ob, ordinal: ib, args: lb },
            )
            | (
                PNode::Coroutine { owner: oa, ordinal: ia, args: la },
                PNode::Coroutine { owner: ob, ordinal: ib, args: lb },
            ) if oa == ob && ia == ib => {
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (
                PNode::Alias { trait_: ta, name: na, args: la },
                PNode::Alias { trait_: tb, name: nb, args: lb },
            ) if ta == tb && na == nb => {
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (PNode::Dynamic(la), PNode::Dynamic(lb)) => {
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            (
                PNode::ExProj { trait_: ta, name: na, args: la, term: xa },
                PNode::ExProj { trait_: tb, name: nb, args: lb, term: xb },
            ) if ta == tb && na == nb => {
                stack.push((xa, xb));
                if !push_lists(la, lb, &mut stack) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Checks whether `root` in `tys` contains no generic parameters, inference holes, or errors.
pub fn is_ground_ty(tys: PTys<'_>, root: PTy, alloc: Alloc<'_>) -> bool {
    let mut stack: AVec<'_, PTy> = OwnVec::with_capacity_in(16, alloc);
    let mut kids: AVec<'_, PTy> = OwnVec::new_in(alloc);
    stack.push(root);
    let mut steps = 0usize;
    while let Some(at) = stack.pop() {
        steps += 1;
        if steps > MAX_TY_WALK {
            return false;
        }
        let Ok(node) = tys.node(at) else { return false };
        if matches!(
            node,
            PNode::Param(_) | PNode::ConstParam(_) | PNode::Infer | PNode::Error | PNode::Future(..)
        ) {
            return false;
        }
        kids.clear();
        if tys.children_of(node, &mut kids).is_err() {
            return false;
        }
        for &c in kids.iter() {
            stack.push(c);
        }
    }
    true
}

/// Copies `root` from `from` into `into`, optionally substituting `Param(i)` / `ConstParam(i)`
/// with `subst[i]` (already in `into`), registering record schemas as met.
pub fn copy_ty_into(
    from: PTys<'_>,
    root: PTy,
    into: &mut PortableTys<'static>,
    subst: Option<&[PTy]>,
    alloc: Alloc<'_>,
) -> Option<PTy> {
    let total = from.len();
    let mut need: AVec<'_, bool> = OwnVec::with_capacity_in(total, alloc);
    need.resize(total, false);
    let mut stack: AVec<'_, PTy> = OwnVec::with_capacity_in(16, alloc);
    let mut kids: AVec<'_, PTy> = OwnVec::new_in(alloc);
    stack.push(root);
    let mut steps = 0usize;
    while let Some(at) = stack.pop() {
        steps += 1;
        if steps > MAX_TY_WALK {
            return None;
        }
        let idx = usize::try_from(at.0).ok()?;
        let slot = need.get_mut(idx)?;
        if *slot {
            continue;
        }
        *slot = true;
        let node = from.node(at).ok()?;
        kids.clear();
        from.children_of(node, &mut kids).ok()?;
        for &c in kids.iter() {
            stack.push(c);
        }
    }
    let mut mapped: AVec<'_, Option<PTy>> = OwnVec::with_capacity_in(total, alloc);
    mapped.resize(total, None);
    for i in 0..total {
        if !need.get(i).copied().unwrap_or(false) {
            continue;
        }
        let idx = u32::try_from(i).ok()?;
        let node = from.node(PTy(idx)).ok()?;
        let m = |c: PTy, mp: &[Option<PTy>]| -> Option<PTy> {
            usize::try_from(c.0).ok().and_then(|at| mp.get(at).copied().flatten())
        };
        let mut l = |list: List, mp: &[Option<PTy>], out: &mut PortableTys<'static>| -> Option<List> {
            kids.clear();
            for &c in from.list(list).ok()? {
                kids.push(m(c, mp)?);
            }
            out.intern_list(&kids).ok()
        };
        if let PNode::Param(pi) | PNode::ConstParam(pi) = node
            && let Some(sub) = subst
        {
            let p_idx = usize::try_from(pi).ok()?;
            let &rep = sub.get(p_idx)?;
            if let Some(slot) = mapped.get_mut(i) {
                *slot = Some(rep);
            }
            continue;
        }
        let copied = match node {
            PNode::Infer | PNode::Error | PNode::Future(..) => return None,
            p @ (PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::ConstParam(_)
            | PNode::ExAuto(_)
            | PNode::Foreign(_)) => p,
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: m(ty, &mapped)?, bits },
            PNode::Tuple(a) => PNode::Tuple(l(a, &mapped, into)?),
            PNode::Record(s, a) => {
                if let Some(names) = from.record_names(s) {
                    into.register_record_schema(s, names).ok()?;
                }
                PNode::Record(s, l(a, &mapped, into)?)
            }
            PNode::Union(a) => PNode::Union(l(a, &mapped, into)?),
            PNode::Ref(mu, a) => PNode::Ref(mu, m(a, &mapped)?),
            PNode::Ptr(mu, a) => PNode::Ptr(mu, m(a, &mapped)?),
            PNode::Array(a, len) => PNode::Array(m(a, &mapped)?, m(len, &mapped)?),
            PNode::Slice(a) => PNode::Slice(m(a, &mapped)?),
            PNode::Pat(a, r) => PNode::Pat(m(a, &mapped)?, l(r, &mapped, into)?),
            PNode::Adt(k, a) => PNode::Adt(k, l(a, &mapped, into)?),
            PNode::FnDef(k, a) => PNode::FnDef(k, l(a, &mapped, into)?),
            PNode::FnPtr(a, h) => PNode::FnPtr(l(a, &mapped, into)?, h),
            PNode::Closure { owner, ordinal, args } => {
                PNode::Closure { owner, ordinal, args: l(args, &mapped, into)? }
            }
            PNode::Coroutine { owner, ordinal, args } => {
                PNode::Coroutine { owner, ordinal, args: l(args, &mapped, into)? }
            }
            PNode::Alias { trait_, name, args } => {
                PNode::Alias { trait_, name, args: l(args, &mapped, into)? }
            }
            PNode::Opaque(k, a) => PNode::Opaque(k, l(a, &mapped, into)?),
            PNode::Dynamic(a) => PNode::Dynamic(l(a, &mapped, into)?),
            PNode::ExTrait(k, a) => PNode::ExTrait(k, l(a, &mapped, into)?),
            PNode::ExProj { trait_, name, args, term } => {
                PNode::ExProj { trait_, name, args: l(args, &mapped, into)?, term: m(term, &mapped)? }
            }
        };
        if let Some(slot) = mapped.get_mut(i) {
            *slot = Some(into.intern(copied).ok()?);
        }
    }
    usize::try_from(root.0).ok().and_then(|at| mapped.get(at).copied().flatten())
}

/// One field's display name and accessor spelling (e.g. `("hp", "hp")` or `("0", "0")`).
struct FieldSpec<'a> {
    name: AString<'a>,
    access: AString<'a>,
}

/// Structural or nominal aggregate identity (after peeling one `&` / `&mut`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AggShape {
    Record(tc::RecordSchema),
    Tuple(usize),
    Struct(ItemKey),
}

/// Classification of an `inline for` iterable's type.
enum IterKind<'a> {
    /// Depends on a generic type parameter (`T`, `&T`, `&mut T`): defer to monomorphic specialization.
    Generic,
    /// Concrete record, tuple, or single-variant struct (optionally behind `&` or `&mut`).
    Concrete { shape: AggShape, ref_mutbl: Option<Mutbl>, fields: AVec<'a, FieldSpec<'a>> },
    /// Not a record, tuple, or single-variant struct.
    NotIterable(&'static str),
}

/// Extracts the concrete field list of `ty` (peeling one `&` / `&mut` if present).
fn extract_fields<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    tys: PTys<'_>,
    ty: PTy,
    body_spell: &Spelling<'_>,
    alloc: Alloc<'a>,
) -> IterKind<'a> {
    let (ref_mutbl, inner) = match tys.get(ty) {
        Some(PNode::Ref(mutbl, inner)) => (Some(*mutbl), *inner),
        Some(_) => (None, ty),
        None => return IterKind::NotIterable("iterable expression has no resolved type"),
    };
    let Some(node) = tys.get(inner) else {
        return IterKind::NotIterable("iterable expression has no resolved inner type");
    };
    match *node {
        PNode::Param(_) => IterKind::Generic,
        PNode::Record(schema, _) => {
            let names = match tys.record_names(schema) {
                Some(n) => n,
                None => match r.store.tys.view().record_names(schema) {
                    Some(n) => n,
                    None => {
                        return IterKind::NotIterable("structural record schema has no field names");
                    }
                },
            };
            let mut fields: AVec<'a, FieldSpec<'a>> = OwnVec::with_capacity_in(names.len(), alloc);
            for &nm in names {
                let text = match r.store.maps.record_field_name(nm) {
                    Some(t) => t,
                    None => match body_spell.get(nm) {
                        Ok(t) => t,
                        Err(_) => {
                            return IterKind::NotIterable("structural record field name is unspelled");
                        }
                    },
                };
                let mut name = OwnString::with_capacity_in(text.len(), alloc);
                name.push_str(text);
                let mut access = OwnString::with_capacity_in(text.len(), alloc);
                access.push_str(text);
                fields.push(FieldSpec { name, access });
            }
            IterKind::Concrete { shape: AggShape::Record(schema), ref_mutbl, fields }
        }
        PNode::Tuple(elems) => {
            let count = usize::try_from(elems.len).unwrap_or(0);
            let mut fields: AVec<'a, FieldSpec<'a>> = OwnVec::with_capacity_in(count, alloc);
            for idx in 0..count {
                let mut name = OwnString::new_in(alloc);
                let _ = write!(name, "{idx}");
                let mut access = OwnString::new_in(alloc);
                let _ = write!(access, "{idx}");
                fields.push(FieldSpec { name, access });
            }
            IterKind::Concrete { shape: AggShape::Tuple(count), ref_mutbl, fields }
        }
        PNode::Adt(adt_key, _) => {
            if !r.store.adts.contains_key(&adt_key) {
                if let Some(lib) = r.keys.library_in(r.names, adt_key) {
                    if r.store.ensure(r.names, r.keys, Want::Adt(lib)).is_err() {
                        return IterKind::NotIterable("struct definition could not be loaded");
                    }
                }
            }
            let Some(adt) = r.store.adts.get(&adt_key) else {
                return IterKind::NotIterable("struct definition is not available");
            };
            if adt.kind != tc::AdtKind::Struct {
                return IterKind::NotIterable("inline for supports records, tuples, and structs (not enums or unions)");
            }
            let Some(variant) = adt.variants.first() else {
                return IterKind::Concrete {
                    shape: AggShape::Struct(adt_key),
                    ref_mutbl,
                    fields: OwnVec::new_in(alloc),
                };
            };
            let Some(adt_fields) = adt.fields.get(variant.fields.range()) else {
                return IterKind::NotIterable("struct field range is out of bounds");
            };
            match variant.ctor {
                tc::CtorKind::Unit => IterKind::Concrete {
                    shape: AggShape::Struct(adt_key),
                    ref_mutbl,
                    fields: OwnVec::new_in(alloc),
                },
                tc::CtorKind::Tuple => {
                    let mut fields: AVec<'a, FieldSpec<'a>> =
                        OwnVec::with_capacity_in(adt_fields.len(), alloc);
                    for idx in 0..adt_fields.len() {
                        let mut name = OwnString::new_in(alloc);
                        let _ = write!(name, "{idx}");
                        let mut access = OwnString::new_in(alloc);
                        let _ = write!(access, "{idx}");
                        fields.push(FieldSpec { name, access });
                    }
                    IterKind::Concrete { shape: AggShape::Struct(adt_key), ref_mutbl, fields }
                }
                tc::CtorKind::Braces => {
                    let Some(lib) = r.keys.library_in(r.names, adt_key) else {
                        return IterKind::NotIterable("named struct has no library key for field spellings");
                    };
                    let Some(it) = r.names.text_of(lib) else {
                        return IterKind::NotIterable("named struct has no source text");
                    };
                    let Ok(low) = super::heads::lower(r.names, lib, it.text) else {
                        return IterKind::NotIterable("named struct head did not lower");
                    };
                    let head_view = low.view();
                    let Ok(head_spell) = Spelling::of(&head_view) else {
                        return IterKind::NotIterable("named struct field names could not be spelled");
                    };
                    let mut fields: AVec<'a, FieldSpec<'a>> =
                        OwnVec::with_capacity_in(adt_fields.len(), alloc);
                    for f in adt_fields {
                        let Ok(text) = head_spell.get(f.name) else {
                            return IterKind::NotIterable("named struct field name is unspelled");
                        };
                        let mut name = OwnString::with_capacity_in(text.len(), alloc);
                        name.push_str(text);
                        let mut access = OwnString::with_capacity_in(text.len(), alloc);
                        access.push_str(text);
                        fields.push(FieldSpec { name, access });
                    }
                    IterKind::Concrete { shape: AggShape::Struct(adt_key), ref_mutbl, fields }
                }
            }
        }
        _ => IterKind::NotIterable(
            "inline for requires a structural record, tuple, or struct (or a reference to one)",
        ),
    }
}

/// Checks that two concrete `zip_fields` iterables share the same aggregate shape and field list.
fn zip_fields_match(
    lhs_shape: AggShape,
    lhs_fields: &[FieldSpec<'_>],
    rhs_shape: AggShape,
    rhs_fields: &[FieldSpec<'_>],
) -> bool {
    if lhs_shape != rhs_shape || lhs_fields.len() != rhs_fields.len() {
        return false;
    }
    for (lf, rf) in lhs_fields.iter().zip(rhs_fields.iter()) {
        if lf.name.as_str() != rf.name.as_str() || lf.access.as_str() != rf.access.as_str() {
            return false;
        }
    }
    true
}

/// One byte-range replacement for an `inline for` expression inside an item's source text.
struct Rewrite<'a> {
    start: usize,
    end: usize,
    replacement: AString<'a>,
}

/// Appends one field access expression for a bound aggregate temporary (`binding_round_site`).
fn write_field_access(
    out: &mut AString<'_>,
    binding: &str,
    round: usize,
    site_idx: usize,
    ref_mutbl: Option<Mutbl>,
    access: &str,
) {
    match ref_mutbl {
        None => {
            let _ = write!(out, "{binding}_{round}_{site_idx}.{access}");
        }
        Some(Mutbl::Not) => {
            let _ = write!(out, "&(*{binding}_{round}_{site_idx}).{access}");
        }
        Some(Mutbl::Mut) => {
            let _ = write!(out, "&mut (*{binding}_{round}_{site_idx}).{access}");
        }
    }
}

/// Formats one unrolled `inline for` expansion into `out`.
fn render_unrolled<'a>(
    view: &blaze_lower::View<'_>,
    site: InlineForSite,
    ref_mutbl: Option<Mutbl>,
    fields: &[FieldSpec<'a>],
    round: usize,
    site_idx: usize,
    alloc: Alloc<'a>,
) -> Option<AString<'a>> {
    let iter_src = view.text_at(view.expr_range(site.iter).ok()?).ok()?;
    let body_src = view.text_at(view.expr_range(site.body).ok()?).ok()?;
    let mut out = OwnString::new_in(alloc);
    if fields.is_empty() {
        let _ = write!(out, "{{ let _ = ({iter_src}); }}");
        return Some(out);
    }
    let two_tuple = match view.pat(site.pat).ok()? {
        blaze_lower::Pat::Tuple { elems, rest: None } if elems.len == 2 => {
            let ids = view.ids(elems).ok()?;
            let p0 = tc::PatId(*ids.first()?);
            let p1 = tc::PatId(*ids.get(1)?);
            let k_src = view.text_at(view.pat_range(p0).ok()?).ok()?;
            let v_src = view.text_at(view.pat_range(p1).ok()?).ok()?;
            Some((k_src, v_src))
        }
        _ => None,
    };
    let pat_src = view.text_at(view.pat_range(site.pat).ok()?).ok()?;
    let _ = write!(out, "{{ let __blaze_inline_iter_{round}_{site_idx} = ({iter_src});");
    for field in fields {
        match two_tuple {
            Some((k_src, v_src)) => {
                let _ = write!(
                    out,
                    " {{ let {k_src}: &'static str = \"{}\"; let {v_src} = ",
                    field.name.as_str()
                );
            }
            None => {
                let _ = write!(out, " {{ let {pat_src} = ");
            }
        }
        write_field_access(
            &mut out,
            "__blaze_inline_iter",
            round,
            site_idx,
            ref_mutbl,
            field.access.as_str(),
        );
        let _ = write!(out, "; {body_src} }}");
    }
    out.push_str(" }");
    Some(out)
}

/// Binding form for `inline for pat in zip_fields(lhs, rhs) { body }`.
enum ZipPat<'s> {
    /// `(a, b)`
    Pair(&'s str, &'s str),
    /// `(name, a, b)`
    Named(&'s str, &'s str, &'s str),
}

/// Typed failure while parsing or rendering a `zip_fields(lhs, rhs)` unrolling site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZipRenderError {
    BadPat,
    ReadPatElems,
    SlicePat,
    SliceLhs,
    SliceRhs,
    SliceBody,
}

impl ZipRenderError {
    const fn message(self) -> &'static str {
        match self {
            ZipRenderError::BadPat => "zip_fields requires a `(a, b)` or `(name, a, b)` tuple pattern",
            ZipRenderError::ReadPatElems => "could not read zip_fields pattern elements",
            ZipRenderError::SlicePat => "could not slice zip_fields pattern",
            ZipRenderError::SliceLhs => "could not slice lhs source text for zip_fields expansion",
            ZipRenderError::SliceRhs => "could not slice rhs source text for zip_fields expansion",
            ZipRenderError::SliceBody => "could not slice body source text for zip_fields expansion",
        }
    }
}

/// Parses a `zip_fields` loop pattern as `(a, b)` or `(name, a, b)`.
fn parse_zip_pat<'v>(view: &blaze_lower::View<'v>, pat: tc::PatId) -> Result<ZipPat<'v>, ZipRenderError> {
    let Ok(blaze_lower::Pat::Tuple { elems, rest: None }) = view.pat(pat) else {
        return Err(ZipRenderError::BadPat);
    };
    let Ok(ids) = view.ids(elems) else {
        return Err(ZipRenderError::ReadPatElems);
    };
    match ids {
        [p0, p1] => {
            let a_src = view
                .pat_range(tc::PatId(*p0))
                .ok()
                .and_then(|r| view.text_at(r).ok())
                .ok_or(ZipRenderError::SlicePat)?;
            let b_src = view
                .pat_range(tc::PatId(*p1))
                .ok()
                .and_then(|r| view.text_at(r).ok())
                .ok_or(ZipRenderError::SlicePat)?;
            Ok(ZipPat::Pair(a_src, b_src))
        }
        [p0, p1, p2] => {
            let name_src = view
                .pat_range(tc::PatId(*p0))
                .ok()
                .and_then(|r| view.text_at(r).ok())
                .ok_or(ZipRenderError::SlicePat)?;
            let a_src = view
                .pat_range(tc::PatId(*p1))
                .ok()
                .and_then(|r| view.text_at(r).ok())
                .ok_or(ZipRenderError::SlicePat)?;
            let b_src = view
                .pat_range(tc::PatId(*p2))
                .ok()
                .and_then(|r| view.text_at(r).ok())
                .ok_or(ZipRenderError::SlicePat)?;
            Ok(ZipPat::Named(name_src, a_src, b_src))
        }
        _ => Err(ZipRenderError::BadPat),
    }
}

/// Formats one unrolled `inline for pat in zip_fields(lhs, rhs) { body }` expansion into `out`.
fn render_zip_unrolled<'a>(
    view: &blaze_lower::View<'_>,
    site: InlineForSite,
    lhs: tc::ExprId,
    rhs: tc::ExprId,
    lhs_mutbl: Option<Mutbl>,
    rhs_mutbl: Option<Mutbl>,
    fields: &[FieldSpec<'a>],
    round: usize,
    site_idx: usize,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, ZipRenderError> {
    let zip_pat = parse_zip_pat(view, site.pat)?;
    let lhs_src = view
        .expr_range(lhs)
        .ok()
        .and_then(|r| view.text_at(r).ok())
        .ok_or(ZipRenderError::SliceLhs)?;
    let rhs_src = view
        .expr_range(rhs)
        .ok()
        .and_then(|r| view.text_at(r).ok())
        .ok_or(ZipRenderError::SliceRhs)?;
    let body_src = view
        .expr_range(site.body)
        .ok()
        .and_then(|r| view.text_at(r).ok())
        .ok_or(ZipRenderError::SliceBody)?;
    let mut out = OwnString::new_in(alloc);
    if fields.is_empty() {
        let _ = write!(out, "{{ let _ = ({lhs_src}); let _ = ({rhs_src}); }}");
        return Ok(out);
    }
    let _ = write!(
        out,
        "{{ let __blaze_inline_lhs_{round}_{site_idx} = ({lhs_src}); let __blaze_inline_rhs_{round}_{site_idx} = ({rhs_src});"
    );
    for field in fields {
        match zip_pat {
            ZipPat::Pair(a_src, b_src) => {
                let _ = write!(out, " {{ let {a_src} = ");
                write_field_access(
                    &mut out,
                    "__blaze_inline_lhs",
                    round,
                    site_idx,
                    lhs_mutbl,
                    field.access.as_str(),
                );
                let _ = write!(out, "; let {b_src} = ");
                write_field_access(
                    &mut out,
                    "__blaze_inline_rhs",
                    round,
                    site_idx,
                    rhs_mutbl,
                    field.access.as_str(),
                );
                let _ = write!(out, "; {body_src} }}");
            }
            ZipPat::Named(name_src, a_src, b_src) => {
                let _ = write!(
                    out,
                    " {{ let {name_src}: &'static str = \"{}\"; let {a_src} = ",
                    field.name.as_str()
                );
                write_field_access(
                    &mut out,
                    "__blaze_inline_lhs",
                    round,
                    site_idx,
                    lhs_mutbl,
                    field.access.as_str(),
                );
                let _ = write!(out, "; let {b_src} = ");
                write_field_access(
                    &mut out,
                    "__blaze_inline_rhs",
                    round,
                    site_idx,
                    rhs_mutbl,
                    field.access.as_str(),
                );
                let _ = write!(out, "; {body_src} }}");
            }
        }
    }
    out.push_str(" }");
    Ok(out)
}

/// Iteratively unrolls all concrete `inline for` loops in `body` in place, without running the
/// final `check_body`. Generic `inline for` loops whose iterable depends on a type parameter are
/// left intact for later monomorphic specialization.
pub fn expand_concrete_body<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    prepared: &Prepares<'n>,
    body: &mut Body<'n>,
) -> Result<(), CheckFail> {
    let alloc = r.store.tys.alloc();
    for round in 0..MAX_UNROLL_ROUNDS {
        if body.typeck.inline_fors.is_empty() {
            break;
        }
        let probe = {
            let view = body.lowered.view();
            let spell = Spelling::of(&view).map_err(|_| CheckFail::InlineFor {
                owner: body.owner,
                at: tc::Range { start: 0, end: 0 },
                why: "could not spell names in body containing inline for",
            })?;
            let scope = body.scope(&spell);
            check_rounds_probe(r, body, &scope)?
        };
        let mut raw_sites: AVec<'_, (InlineForSite, usize, usize)> =
            OwnVec::with_capacity_in(body.typeck.inline_fors.len(), alloc);
        {
            let view = body.lowered.view();
            for &site in body.typeck.inline_fors.iter() {
                let Ok(br) = view.expr_range(site.expr) else { continue };
                let Ok(sr) = view.site(br) else { continue };
                let (Ok(start), Ok(end)) = (usize::try_from(sr.start), usize::try_from(sr.end)) else {
                    continue;
                };
                raw_sites.push((site, start, end));
            }
        }
        // Only expand outermost (non-nested) `inline for` sites on this round.
        let mut top_sites: AVec<'_, (InlineForSite, usize, usize)> =
            OwnVec::with_capacity_in(raw_sites.len(), alloc);
        for &(site, start, end) in raw_sites.iter() {
            let nested = raw_sites.iter().any(|&(_, os, oe)| {
                os <= start && end <= oe && (os < start || end < oe)
            });
            if !nested {
                top_sites.push((site, start, end));
            }
        }
        let mut rewrites: AVec<'_, Rewrite<'static>> =
            OwnVec::with_capacity_in(top_sites.len(), alloc);
        let mut any_generic = false;
        let probe_tys = probe.types.view();
        for (site_idx, &(site, start, end)) in top_sites.iter().enumerate() {
            match site.kind {
                InlineForKind::Single { iter } => {
                    let iter_ty = probe
                        .expr_ty
                        .get(iter.ix())
                        .copied()
                        .ok_or(CheckFail::InlineFor {
                            owner: body.owner,
                            at: body.typeck.expr_ranges.get(site.expr.ix()).copied().unwrap_or_default(),
                            why: "inline for iterable expression has no recorded type",
                        })?;
                    let field_res = {
                        let view = body.lowered.view();
                        let spell = Spelling::of(&view).map_err(|_| CheckFail::InlineFor {
                            owner: body.owner,
                            at: body.typeck.expr_ranges.get(site.expr.ix()).copied().unwrap_or_default(),
                            why: "could not spell names for inline for expansion",
                        })?;
                        extract_fields(r, probe_tys, iter_ty, &spell, alloc)
                    };
                    match field_res {
                        IterKind::Generic => {
                            any_generic = true;
                        }
                        IterKind::NotIterable(why) => {
                            let at = body.typeck.expr_ranges.get(iter.ix()).copied().unwrap_or_default();
                            return Err(CheckFail::InlineFor { owner: body.owner, at, why });
                        }
                        IterKind::Concrete { ref_mutbl, fields, .. } => {
                            let view = body.lowered.view();
                            let Some(replacement) =
                                render_unrolled(&view, site, ref_mutbl, &fields, round, site_idx, alloc)
                            else {
                                let at = body.typeck.expr_ranges.get(site.expr.ix()).copied().unwrap_or_default();
                                return Err(CheckFail::InlineFor {
                                    owner: body.owner,
                                    at,
                                    why: "could not slice source text for inline for expansion",
                                });
                            };
                            rewrites.push(Rewrite { start, end, replacement });
                        }
                    }
                }
                InlineForKind::Zip { lhs, rhs } => {
                    let lhs_ty = probe
                        .expr_ty
                        .get(lhs.ix())
                        .copied()
                        .ok_or(CheckFail::InlineFor {
                            owner: body.owner,
                            at: body.typeck.expr_ranges.get(lhs.ix()).copied().unwrap_or_default(),
                            why: "zip_fields left expression has no recorded type",
                        })?;
                    let rhs_ty = probe
                        .expr_ty
                        .get(rhs.ix())
                        .copied()
                        .ok_or(CheckFail::InlineFor {
                            owner: body.owner,
                            at: body.typeck.expr_ranges.get(rhs.ix()).copied().unwrap_or_default(),
                            why: "zip_fields right expression has no recorded type",
                        })?;
                    let (lhs_res, rhs_res) = {
                        let view = body.lowered.view();
                        let spell = Spelling::of(&view).map_err(|_| CheckFail::InlineFor {
                            owner: body.owner,
                            at: body.typeck.expr_ranges.get(site.expr.ix()).copied().unwrap_or_default(),
                            why: "could not spell names for zip_fields expansion",
                        })?;
                        (
                            extract_fields(r, probe_tys, lhs_ty, &spell, alloc),
                            extract_fields(r, probe_tys, rhs_ty, &spell, alloc),
                        )
                    };
                    match (lhs_res, rhs_res) {
                        (IterKind::NotIterable(why), _) => {
                            let at = body.typeck.expr_ranges.get(lhs.ix()).copied().unwrap_or_default();
                            return Err(CheckFail::InlineFor { owner: body.owner, at, why });
                        }
                        (_, IterKind::NotIterable(why)) => {
                            let at = body.typeck.expr_ranges.get(rhs.ix()).copied().unwrap_or_default();
                            return Err(CheckFail::InlineFor { owner: body.owner, at, why });
                        }
                        (IterKind::Generic, _) | (_, IterKind::Generic) => {
                            any_generic = true;
                        }
                        (
                            IterKind::Concrete { shape: lhs_shape, ref_mutbl: lhs_mutbl, fields: lhs_fields },
                            IterKind::Concrete { shape: rhs_shape, ref_mutbl: rhs_mutbl, fields: rhs_fields },
                        ) => {
                            if !zip_fields_match(lhs_shape, &lhs_fields, rhs_shape, &rhs_fields) {
                                let at = body.typeck.expr_ranges.get(site.iter.ix()).copied().unwrap_or_default();
                                return Err(CheckFail::InlineFor {
                                    owner: body.owner,
                                    at,
                                    why: "zip_fields requires two values of the same record, tuple, or struct type",
                                });
                            }
                            let view = body.lowered.view();
                            let replacement = render_zip_unrolled(
                                &view,
                                site,
                                lhs,
                                rhs,
                                lhs_mutbl,
                                rhs_mutbl,
                                &lhs_fields,
                                round,
                                site_idx,
                                alloc,
                            )
                            .map_err(|why| CheckFail::InlineFor {
                                owner: body.owner,
                                at: body.typeck.expr_ranges.get(site.expr.ix()).copied().unwrap_or_default(),
                                why: why.message(),
                            })?;
                            rewrites.push(Rewrite { start, end, replacement });
                        }
                    }
                }
            }
        }
        if rewrites.is_empty() {
            if any_generic {
                check_for_fields_generic_bodies(r, prepared, body, &probe, &top_sites)?;
                break;
            }
            break;
        }
        // Splice replacements in one forward pass by ascending byte offset.
        rewrites.sort_unstable_by_key(|rw| rw.start);
        let expanded: &'n str = {
            let view = body.lowered.view();
            let orig_text = view.item();
            let mut buf = OwnString::with_capacity_in(orig_text.len() + 128, alloc);
            let mut cursor = 0usize;
            for rw in rewrites.iter() {
                if rw.start >= cursor && rw.start <= rw.end && rw.end <= orig_text.len() {
                    if let Some(prefix) = orig_text.get(cursor..rw.start) {
                        buf.push_str(prefix);
                    }
                    buf.push_str(rw.replacement.as_str());
                    cursor = rw.end;
                }
            }
            if let Some(tail) = orig_text.get(cursor..) {
                buf.push_str(tail);
            }
            r.store.locals.keep_text(buf.as_str())
        };
        let subst_owned: Option<(ItemKey, OwnVec<PTy, Alloc<'static>>)> =
            body.param_subst.as_ref().map(|(orig, args)| {
                let mut v = OwnVec::with_capacity_in(args.len(), alloc);
                v.extend_from_slice(args);
                (*orig, v)
            });
        let subst_ref = subst_owned.as_ref().map(|(orig, v)| (*orig, v.as_slice()));
        let new_body = lower_body_with_text(
            r.names,
            prepared,
            r.keys,
            r.store,
            body.item,
            body.owner,
            expanded,
            subst_ref,
            None,
        )
        .map_err(|_| CheckFail::InlineFor {
            owner: body.owner,
            at: tc::Range { start: 0, end: 0 },
            why: "re-lowering expanded inline for body failed",
        })?;
        *body = new_body;
    }
    Ok(())
}

/// Checks `body`, iteratively unrolling any concrete `inline for` loops before running the final
/// `check_body`. If all remaining `inline for` iterables depend on generic parameters, checks
/// the body as-is so generic specialization can expand each concrete call site later.
pub fn unroll_concrete_body<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    prepared: &Prepares<'n>,
    body: &mut Body<'n>,
) -> Result<Checked<'static>, CheckFail> {
    expand_concrete_body(r, prepared, body)?;
    let view = body.lowered.view();
    let spell = Spelling::of(&view).map_err(|_| CheckFail::InlineFor {
        owner: body.owner,
        at: tc::Range { start: 0, end: 0 },
        why: "could not spell names in expanded body",
    })?;
    let scope = body.scope(&spell);
    check_body(r, body, &scope)
}

/// Peels one optional `&` / `&mut` reference layer from `ty`.
fn peel_ref_ty(tys: PTys<'_>, ty: PTy) -> (Option<Mutbl>, PTy) {
    match tys.node(ty) {
        Ok(PNode::Ref(m, inner)) => (Some(m), inner),
        _ => (None, ty),
    }
}

/// Reference prefix for a synthetic `ForFields` field binding type annotation.
const fn ref_prefix(mutbl: Option<Mutbl>) -> &'static str {
    match mutbl {
        None => "",
        Some(Mutbl::Not) => "&",
        Some(Mutbl::Mut) => "&mut ",
    }
}

/// Formats a single-iterable `inline for` as a strict definition-time type-check block binding
/// the field pattern to a rigid synthetic field type parameter `__BlazeForField{synth_idx}`.
fn render_for_fields_single<'a>(
    view: &blaze_lower::View<'_>,
    site: InlineForSite,
    ref_mutbl: Option<Mutbl>,
    synth_idx: usize,
    alloc: Alloc<'a>,
) -> Option<AString<'a>> {
    let iter_src = view.text_at(view.expr_range(site.iter).ok()?).ok()?;
    let body_src = view.text_at(view.expr_range(site.body).ok()?).ok()?;
    let two_tuple = match view.pat(site.pat).ok()? {
        blaze_lower::Pat::Tuple { elems, rest: None } if elems.len == 2 => {
            let ids = view.ids(elems).ok()?;
            let p0 = tc::PatId(*ids.first()?);
            let p1 = tc::PatId(*ids.get(1)?);
            let k_src = view.text_at(view.pat_range(p0).ok()?).ok()?;
            let v_src = view.text_at(view.pat_range(p1).ok()?).ok()?;
            Some((k_src, v_src))
        }
        _ => None,
    };
    let pat_src = view.text_at(view.pat_range(site.pat).ok()?).ok()?;
    let prefix = ref_prefix(ref_mutbl);
    let mut out = OwnString::new_in(alloc);
    let _ = write!(out, "{{ let _ = ({iter_src}); while false {{ ");
    match two_tuple {
        Some((k_src, v_src)) => {
            let _ = write!(
                out,
                "let {k_src}: &'static str = \"\"; let {v_src}: {prefix}__BlazeForField{synth_idx} = loop {{}}; "
            );
        }
        None => {
            let _ = write!(
                out,
                "let {pat_src}: {prefix}__BlazeForField{synth_idx} = loop {{}}; "
            );
        }
    }
    let _ = write!(out, "{body_src} }} }}");
    Some(out)
}

/// Formats a `zip_fields(lhs, rhs)` `inline for` as a strict definition-time type-check block
/// binding both field patterns to the SAME rigid synthetic field type parameter
/// `__BlazeForField{synth_idx}` (with their respective `&` / `&mut` / by-value wrappers).
#[expect(clippy::too_many_arguments, reason = "mirrors render_zip_unrolled parameters")]
fn render_for_fields_zip<'a>(
    view: &blaze_lower::View<'_>,
    site: InlineForSite,
    lhs: tc::ExprId,
    rhs: tc::ExprId,
    lhs_mutbl: Option<Mutbl>,
    rhs_mutbl: Option<Mutbl>,
    synth_idx: usize,
    alloc: Alloc<'a>,
) -> Result<AString<'a>, ZipRenderError> {
    let zip_pat = parse_zip_pat(view, site.pat)?;
    let lhs_src = view
        .expr_range(lhs)
        .ok()
        .and_then(|r| view.text_at(r).ok())
        .ok_or(ZipRenderError::SliceLhs)?;
    let rhs_src = view
        .expr_range(rhs)
        .ok()
        .and_then(|r| view.text_at(r).ok())
        .ok_or(ZipRenderError::SliceRhs)?;
    let body_src = view
        .expr_range(site.body)
        .ok()
        .and_then(|r| view.text_at(r).ok())
        .ok_or(ZipRenderError::SliceBody)?;
    let lpre = ref_prefix(lhs_mutbl);
    let rpre = ref_prefix(rhs_mutbl);
    let mut out = OwnString::new_in(alloc);
    let _ = write!(out, "{{ let _ = ({lhs_src}); let _ = ({rhs_src}); while false {{ ");
    match zip_pat {
        ZipPat::Pair(a_src, b_src) => {
            let _ = write!(
                out,
                "let {a_src}: {lpre}__BlazeForField{synth_idx} = loop {{}}; let {b_src}: {rpre}__BlazeForField{synth_idx} = loop {{}}; "
            );
        }
        ZipPat::Named(name_src, a_src, b_src) => {
            let _ = write!(
                out,
                "let {name_src}: &'static str = \"\"; let {a_src}: {lpre}__BlazeForField{synth_idx} = loop {{}}; let {b_src}: {rpre}__BlazeForField{synth_idx} = loop {{}}; "
            );
        }
    }
    let _ = write!(out, "{body_src} }} }}");
    Ok(out)
}

/// Checks generic `inline for` bodies strictly at definition time whenever their container type
/// has one or more `where for<F in fields(T)> F: Bound` clauses in the enclosing `ParamEnv`.
/// Unbounded generic `inline for` loops are skipped so existing monomorphization-time checking
/// behavior is preserved.
fn check_for_fields_generic_bodies<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    prepared: &Prepares<'n>,
    body: &Body<'n>,
    probe: &Checked<'static>,
    top_sites: &[(InlineForSite, usize, usize)],
) -> Result<(), CheckFail> {
    let alloc = r.store.tys.alloc();
    let owner = body.owner;
    let mut ff_clauses: AVec<'_, (PTy, ItemKey, List)> = OwnVec::new_in(alloc);
    let mut sized_for_container: AVec<'_, (PTy, ItemKey)> = OwnVec::new_in(alloc);
    let sized_trait = super::langs::find(r.index, crate::resolve::Paths { names: r.names }, tc::LangItem::Sized)
        .ok()
        .flatten()
        .and_then(|lib| r.keys.item(lib).ok());
    if let Some(preds) = r.store.preds.get(&owner) {
        let store_tys = r.store.tys.view();
        for &c in preds.clauses.iter() {
            match c {
                tc::PClause::ForFields { container, trait_, args } => {
                    ff_clauses.push((container, trait_, args));
                }
                tc::PClause::Trait { trait_, args } => {
                    if sized_trait == Some(trait_)
                        && let Ok([only]) = store_tys.list(args)
                    {
                        sized_for_container.push((*only, trait_));
                    }
                }
                _ => {}
            }
        }
    }
    if ff_clauses.is_empty() {
        return Ok(());
    }

    struct SynthField<'a> {
        param_idx: u32,
        bounds: AVec<'a, (ItemKey, List)>,
        sized: Option<ItemKey>,
    }

    let Some(base_generics) = r.store.fns.get(&owner).map(|h| h.parent.saturating_add(h.own)) else {
        return Ok(());
    };
    let probe_tys = probe.types.view();
    let store_tys = r.store.tys.view();
    let mut synth_fields: AVec<'_, SynthField<'_>> = OwnVec::new_in(alloc);
    let mut rewrites: AVec<'_, Rewrite<'static>> = OwnVec::new_in(alloc);
    let view = body.lowered.view();

    for &(site, start, end) in top_sites.iter() {
        let collect_bounds = |inner_ty: PTy| -> Option<(AVec<'_, (ItemKey, List)>, Option<ItemKey>)> {
            let mut matched: AVec<'_, (ItemKey, List)> = OwnVec::new_in(alloc);
            let mut sized = None;
            for &(container, trait_, args) in ff_clauses.iter() {
                if args_equal(store_tys, &[container], probe_tys, &[inner_ty], alloc) {
                    matched.push((trait_, args));
                    if sized.is_none() {
                        for &(sc, st) in sized_for_container.iter() {
                            if sc == container {
                                sized = Some(st);
                                break;
                            }
                        }
                    }
                }
            }
            if matched.is_empty() { None } else { Some((matched, sized)) }
        };

        match site.kind {
            InlineForKind::Single { iter } => {
                let Some(&iter_ty) = probe.expr_ty.get(iter.ix()) else { continue };
                let (ref_mutbl, inner_ty) = peel_ref_ty(probe_tys, iter_ty);
                let Some((bounds, sized)) = collect_bounds(inner_ty) else { continue };
                let synth_idx = synth_fields.len();
                let Ok(offset) = u32::try_from(synth_idx) else { continue };
                let Some(replacement) = render_for_fields_single(&view, site, ref_mutbl, synth_idx, alloc) else {
                    let at = body.typeck.expr_ranges.get(site.expr.ix()).copied().unwrap_or_default();
                    return Err(CheckFail::InlineFor {
                        owner,
                        at,
                        why: "could not slice source text for for-fields body check",
                    });
                };
                synth_fields.push(SynthField {
                    param_idx: base_generics.saturating_add(offset),
                    bounds,
                    sized,
                });
                rewrites.push(Rewrite { start, end, replacement });
            }
            InlineForKind::Zip { lhs, rhs } => {
                let Some(&lhs_ty) = probe.expr_ty.get(lhs.ix()) else { continue };
                let Some(&rhs_ty) = probe.expr_ty.get(rhs.ix()) else { continue };
                let (lhs_mutbl, lhs_inner) = peel_ref_ty(probe_tys, lhs_ty);
                let (rhs_mutbl, rhs_inner) = peel_ref_ty(probe_tys, rhs_ty);
                if !args_equal(probe_tys, &[lhs_inner], probe_tys, &[rhs_inner], alloc) {
                    continue;
                }
                let Some((bounds, sized)) = collect_bounds(lhs_inner) else { continue };
                let synth_idx = synth_fields.len();
                let Ok(offset) = u32::try_from(synth_idx) else { continue };
                let replacement =
                    render_for_fields_zip(&view, site, lhs, rhs, lhs_mutbl, rhs_mutbl, synth_idx, alloc).map_err(
                        |why| CheckFail::InlineFor {
                            owner,
                            at: body.typeck.expr_ranges.get(site.expr.ix()).copied().unwrap_or_default(),
                            why: why.message(),
                        },
                    )?;
                synth_fields.push(SynthField {
                    param_idx: base_generics.saturating_add(offset),
                    bounds,
                    sized,
                });
                rewrites.push(Rewrite { start, end, replacement });
            }
        }
    }

    if synth_fields.is_empty() {
        return Ok(());
    }

    rewrites.sort_unstable_by_key(|rw| rw.start);
    let check_text: &'n str = {
        let orig_text = view.item();
        let mut buf = OwnString::with_capacity_in(orig_text.len() + 128, alloc);
        let mut cursor = 0usize;
        for rw in rewrites.iter() {
            if rw.start >= cursor && rw.start <= rw.end && rw.end <= orig_text.len() {
                if let Some(prefix) = orig_text.get(cursor..rw.start) {
                    buf.push_str(prefix);
                }
                buf.push_str(rw.replacement.as_str());
                cursor = rw.end;
            }
        }
        if let Some(tail) = orig_text.get(cursor..) {
            buf.push_str(tail);
        }
        r.store.locals.keep_text(buf.as_str())
    };

    let Ok(synth_count) = u32::try_from(synth_fields.len()) else {
        return Ok(());
    };
    let (lib, head) = {
        let Some(orig_head) = r.store.fns.get(&owner) else {
            return Ok(());
        };
        let lib = orig_head.lib;
        let mut head = FnHead {
            lib,
            text: check_text,
            text_prefix: orig_head.text_prefix,
            container: orig_head.container,
            parent: orig_head.parent,
            own: orig_head.own.saturating_add(synth_count),
            declared: orig_head.declared,
            self_param: orig_head.self_param,
            header: orig_head.header,
            c_variadic: orig_head.c_variadic,
            qualifiers: orig_head.qualifiers,
            legacy_const_generics: Default::default(),
            inputs: Default::default(),
            output: orig_head.output,
            has_body: true,
            module: orig_head.module,
            params: Default::default(),
            consts: Default::default(),
            bounds: Default::default(),
            self_: orig_head.self_,
            self_assoc: Default::default(),
        };
        head.legacy_const_generics.extend_from_slice(&orig_head.legacy_const_generics);
        head.inputs.extend_from_slice(&orig_head.inputs);
        head.params.extend_from_slice(&orig_head.params);
        head.consts.extend_from_slice(&orig_head.consts);
        head.bounds.extend_from_slice(&orig_head.bounds);
        head.self_assoc.extend_from_slice(&orig_head.self_assoc);
        let mut name_buf = OwnString::new_in(alloc);
        for (idx, sf) in synth_fields.iter().enumerate() {
            name_buf.clear();
            let _ = write!(name_buf, "__BlazeForField{idx}");
            head.params.push((tc::Name::of(name_buf.as_str()), sf.param_idx));
        }
        (lib, head)
    };

    let mut shadow_preds = Preds::new_in(RegionScope::new_in(alloc));
    if let Some(op) = r.store.preds.get(&owner) {
        for &c in op.clauses.iter() {
            shadow_preds.push(c, Src::Derived);
        }
    }
    let mut arg_buf: AVec<'_, PTy> = OwnVec::new_in(alloc);
    for sf in synth_fields.iter() {
        let Ok(field_param) = r.store.tys.intern(PNode::Param(sf.param_idx)) else {
            return Ok(());
        };
        for &(trait_, args_after_self) in sf.bounds.iter() {
            arg_buf.clear();
            arg_buf.push(field_param);
            if let Ok(rest) = r.store.tys.view().list(args_after_self) {
                arg_buf.extend_from_slice(rest);
            }
            if let Ok(full_args) = r.store.tys.intern_list(&arg_buf) {
                shadow_preds.push(tc::PClause::Trait { trait_, args: full_args }, Src::Derived);
            }
        }
        if let Some(sized_key) = sf.sized
            && let Ok(sized_args) = r.store.tys.intern_list(&[field_param])
        {
            shadow_preds.push(tc::PClause::Trait { trait_: sized_key, args: sized_args }, Src::Derived);
        }
    }
    let prev_fn = r.store.fns.push_override(owner, head);
    let prev_preds = r.store.preds.push_override(owner, shadow_preds);

    let shadow_lower = lower_body_with_text(
        r.names,
        prepared,
        r.keys,
        r.store,
        lib,
        owner,
        check_text,
        None,
        None,
    );
    let res = match shadow_lower {
        Ok(shadow_body) => {
            let shadow_view = shadow_body.lowered.view();
            match Spelling::of(&shadow_view) {
                Ok(spell) => {
                    let shadow_scope = shadow_body.scope(&spell);
                    check_rounds_strict(r, &shadow_body, &shadow_scope).map(|_| ())
                }
                Err(_) => Err(CheckFail::InlineFor {
                    owner,
                    at: tc::Range { start: 0, end: 0 },
                    why: "could not spell names in for-fields checked body",
                }),
            }
        }
        Err(_) => Err(CheckFail::InlineFor {
            owner,
            at: tc::Range { start: 0, end: 0 },
            why: "lowering for-fields definition-checked body failed",
        }),
    };

    r.store.fns.restore_override(owner, prev_fn);
    r.store.preds.restore_override(owner, prev_preds);

    match res {
        Ok(()) => Ok(()),
        Err(CheckFail::Diags { diags, filed, .. }) => Err(CheckFail::Diags { owner, diags, filed }),
        Err(other) => Err(other),
    }
}

/// Deterministic synthetic checker key for a monomorphized `inline for` specialization.
fn synth_spec_key(orig: ItemKey, args: &[PTy], tys: PTys<'_>, alloc: Alloc<'_>) -> ItemKey {
    let mut h = Hasher::new("blaze-ladder inline-for specialization 1");
    h.update(&orig.0.to_le_bytes());
    let arg_len = u64::try_from(args.len()).unwrap_or(0);
    h.update(&arg_len.to_le_bytes());
    let mut stack: AVec<'_, PTy> = OwnVec::with_capacity_in(args.len(), alloc);
    let mut kids: AVec<'_, PTy> = OwnVec::new_in(alloc);
    for &a in args.iter().rev() {
        stack.push(a);
    }
    let mut steps = 0usize;
    while let Some(at) = stack.pop() {
        steps += 1;
        if steps > MAX_TY_WALK {
            break;
        }
        let Ok(node) = tys.node(at) else { break };
        match node {
            PNode::Bool => { h.update(&[1]); }
            PNode::Char => { h.update(&[2]); }
            PNode::Int(i) => {
                let tag = match i {
                    tc::IntTy::Isize => 0u8,
                    tc::IntTy::I8 => 1,
                    tc::IntTy::I16 => 2,
                    tc::IntTy::I32 => 3,
                    tc::IntTy::I64 => 4,
                    tc::IntTy::I128 => 5,
                };
                h.update(&[3, tag]);
            }
            PNode::Uint(u) => {
                let tag = match u {
                    tc::UintTy::Usize => 0u8,
                    tc::UintTy::U8 => 1,
                    tc::UintTy::U16 => 2,
                    tc::UintTy::U32 => 3,
                    tc::UintTy::U64 => 4,
                    tc::UintTy::U128 => 5,
                };
                h.update(&[4, tag]);
            }
            PNode::Float(f) => {
                let tag = match f {
                    tc::FloatTy::F16 => 0u8,
                    tc::FloatTy::F32 => 1,
                    tc::FloatTy::F64 => 2,
                    tc::FloatTy::F128 => 3,
                };
                h.update(&[5, tag]);
            }
            PNode::Str => { h.update(&[6]); }
            PNode::Never => { h.update(&[7]); }
            PNode::Tuple(l) => {
                h.update(&[8]);
                h.update(&l.len.to_le_bytes());
            }
            PNode::Record(s, l) => {
                h.update(&[9]);
                h.update(&s.0.to_le_bytes());
                h.update(&l.len.to_le_bytes());
            }
            PNode::Union(l) => {
                h.update(&[10]);
                h.update(&l.len.to_le_bytes());
            }
            PNode::Ref(m, _) => { h.update(&[11, u8::from(matches!(m, Mutbl::Mut))]); }
            PNode::Ptr(m, _) => { h.update(&[12, u8::from(matches!(m, Mutbl::Mut))]); }
            PNode::Array(_, _) => { h.update(&[13]); }
            PNode::Slice(_) => { h.update(&[14]); }
            PNode::Adt(k, l) => {
                h.update(&[15]);
                h.update(&k.0.to_le_bytes());
                h.update(&l.len.to_le_bytes());
            }
            PNode::FnDef(k, l) => {
                h.update(&[16]);
                h.update(&k.0.to_le_bytes());
                h.update(&l.len.to_le_bytes());
            }
            PNode::FnPtr(l, _) => {
                h.update(&[17]);
                h.update(&l.len.to_le_bytes());
            }
            PNode::ConstValue { bits, .. } => {
                h.update(&[18]);
                h.update(&bits.to_le_bytes());
            }
            PNode::Opaque(k, _) | PNode::ExAuto(k) | PNode::Foreign(k) | PNode::ExTrait(k, _) => {
                h.update(&[19]);
                h.update(&k.0.to_le_bytes());
            }
            PNode::Closure { owner, ordinal, .. } | PNode::Coroutine { owner, ordinal, .. } => {
                h.update(&[20]);
                h.update(&owner.0.to_le_bytes());
                h.update(&ordinal.to_le_bytes());
            }
            PNode::Alias { trait_, name, .. } | PNode::ExProj { trait_, name, .. } => {
                h.update(&[21]);
                h.update(&trait_.0.to_le_bytes());
                h.update(&name.0.to_le_bytes());
            }
            PNode::Pat(_, _) | PNode::Dynamic(_) | PNode::Param(_) | PNode::ConstParam(_)
            | PNode::Infer | PNode::Error | PNode::Future(..) => {
                h.update(&[22]);
            }
        }
        kids.clear();
        if tys.children_of(node, &mut kids).is_ok() {
            for &c in kids.iter().rev() {
                stack.push(c);
            }
        }
    }
    let id = h.finish();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&id.0[..16]);
    ItemKey(u128::from_le_bytes(bytes))
}

/// Inserts a monomorphized `FnHead` + empty `Preds` into `r.store` under `spec_key`.
fn install_spec_head<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    orig_key: ItemKey,
    spec_key: ItemKey,
    concrete_args: &[PTy],
    override_text: Option<&'n str>,
) -> Result<blaze_id::ItemKey, CheckFail> {
    let alloc = r.store.tys.alloc();
    if override_text.is_none() && r.store.fns.contains_key(&spec_key) {
        let lib = r
            .store
            .fns
            .get(&spec_key)
            .map(|h| h.lib)
            .ok_or(CheckFail::InlineFor {
                owner: orig_key,
                at: tc::Range { start: 0, end: 0 },
                why: "missing specialized fn head",
            })?;
        return Ok(lib);
    }
    let (lib, text, text_prefix, container, self_param, header, c_variadic, qualifiers, module, raw_inputs, raw_output, raw_self) = {
        let Some(orig) = r.store.fns.get(&orig_key) else {
            return Err(CheckFail::InlineFor {
                owner: orig_key,
                at: tc::Range { start: 0, end: 0 },
                why: "generic inline for function has no head in store",
            });
        };
        let mut raw_inputs: OwnVec<PTy, Alloc<'static>> =
            OwnVec::with_capacity_in(orig.inputs.len(), alloc);
        raw_inputs.extend_from_slice(&orig.inputs);
        (
            orig.lib,
            override_text.unwrap_or(orig.text),
            orig.text_prefix,
            orig.container,
            orig.self_param,
            orig.header,
            orig.c_variadic,
            orig.qualifiers,
            orig.module,
            raw_inputs,
            orig.output,
            orig.self_,
        )
    };
    let mut head = FnHead {
        lib,
        text,
        text_prefix,
        container,
        parent: 0,
        own: 0,
        declared: 0,
        self_param,
        header,
        c_variadic,
        qualifiers,
        legacy_const_generics: Default::default(),
        inputs: Default::default(),
        output: raw_output,
        has_body: true,
        module,
        params: Default::default(),
        consts: Default::default(),
        bounds: Default::default(),
        self_: SelfTy::None,
        self_assoc: Default::default(),
    };
    for &inp in raw_inputs.iter() {
        let sub = r.store.tys.subst(inp, concrete_args).map_err(|error| CheckFail::Check {
            owner: orig_key,
            error,
        })?;
        head.inputs.push(sub);
    }
    head.output = r
        .store
        .tys
        .subst(raw_output, concrete_args)
        .map_err(|error| CheckFail::Check { owner: orig_key, error })?;
    head.self_ = match raw_self {
        SelfTy::None => SelfTy::None,
        SelfTy::Trait(_) => match concrete_args.first().copied() {
            Some(self_ty) => SelfTy::Ty(self_ty, None),
            None => SelfTy::None,
        },
        SelfTy::Ty(t, tr) => {
            let sub_t = r
                .store
                .tys
                .subst(t, concrete_args)
                .map_err(|error| CheckFail::Check { owner: orig_key, error })?;
            let sub_tr = match tr {
                None => None,
                Some((k, list)) => {
                    let mut raw_tr: OwnVec<PTy, Alloc<'static>> = OwnVec::new_in(alloc);
                    if let Ok(slice) = r.store.tys.view().list(list) {
                        raw_tr.extend_from_slice(slice);
                    }
                    let mut out_tr: OwnVec<PTy, Alloc<'static>> =
                        OwnVec::with_capacity_in(raw_tr.len(), alloc);
                    for &item_ty in raw_tr.iter() {
                        let s = r
                            .store
                            .tys
                            .subst(item_ty, concrete_args)
                            .map_err(|error| CheckFail::Check { owner: orig_key, error })?;
                        out_tr.push(s);
                    }
                    let interned = r
                        .store
                        .tys
                        .intern_list(&out_tr)
                        .map_err(|error| CheckFail::Check { owner: orig_key, error })?;
                    Some((k, interned))
                }
            };
            SelfTy::Ty(sub_t, sub_tr)
        }
    };
    r.store.fns.insert(spec_key, head);
    r.store.preds.insert(spec_key, Preds::new_in(RegionScope::new_in(alloc)));
    Ok(lib)
}

/// Specializes a single generic `inline for` function `orig_key` at already-imported
/// `args_copy` (interned in `r.store.tys`), unrolling and checking its monomorphized body.
fn specialize_one_imported<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    prepared: &Prepares<'n>,
    orig_key: ItemKey,
    args_copy: OwnVec<PTy, Alloc<'static>>,
) -> Result<SpecializedBody<'n>, CheckFail> {
    let alloc = r.store.tys.alloc();
    let spec_key = synth_spec_key(orig_key, &args_copy, r.store.tys.view(), alloc);
    let lib = install_spec_head(r, orig_key, spec_key, &args_copy, None)?;
    let orig_text = r
        .store
        .fns
        .get(&orig_key)
        .map(|h| h.text)
        .ok_or(CheckFail::InlineFor {
            owner: orig_key,
            at: tc::Range { start: 0, end: 0 },
            why: "generic inline for function has no source text",
        })?;
    let mut spec_body = lower_body_with_text(
        r.names,
        prepared,
        r.keys,
        r.store,
        lib,
        spec_key,
        orig_text,
        Some((orig_key, &args_copy)),
        None,
    )
    .map_err(|_| CheckFail::InlineFor {
        owner: orig_key,
        at: tc::Range { start: 0, end: 0 },
        why: "lowering specialized generic inline for body failed",
    })?;
    let spec_checked = unroll_concrete_body(r, prepared, &mut spec_body)?;
    // Update `spec_head.text` so `FnFacts::text` and `symbols_of` see the expanded text.
    if let Some(exp) = spec_body.expanded_text {
        let _ = install_spec_head(r, orig_key, spec_key, &args_copy, Some(exp));
    }
    Ok(SpecializedBody {
        orig: orig_key,
        args: args_copy,
        spec_key,
        body: spec_body,
        checked: spec_checked,
    })
}

/// Specializes a single generic `inline for` function `orig_key` on demand at concrete
/// `ground_args` (interned in `ground_tys`), unrolling all now-concrete `inline for` loops
/// and checking the resulting monomorphic body. Returns `Ok(None)` if `ground_args` is empty
/// or contains a non-ground type.
pub fn specialize_one_generic_inline_for<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    prepared: &Prepares<'n>,
    orig_key: ItemKey,
    ground_tys: PTys<'_>,
    ground_args: &[PTy],
) -> Result<Option<SpecializedBody<'n>>, CheckFail> {
    if ground_args.is_empty() {
        return Ok(None);
    }
    let alloc = r.store.tys.alloc();
    let mut args_copy: OwnVec<PTy, Alloc<'static>> = OwnVec::with_capacity_in(ground_args.len(), alloc);
    for &raw in ground_args {
        let Some(copied) = copy_ty_into(ground_tys, raw, &mut r.store.tys, None, alloc) else {
            return Ok(None);
        };
        if !is_ground_ty(r.store.tys.view(), copied, alloc) {
            return Ok(None);
        }
        args_copy.push(copied);
    }
    specialize_one_imported(r, prepared, orig_key, args_copy).map(Some)
}

/// Discovers every concrete call `(callee, concrete_args)` targeting any reached generic
/// `inline for` function, specializes & unrolls each concrete instance, and reaches all newly
/// discovered callees into `census`.
pub fn specialize_generic_inline_fors<'n>(
    r: &mut Records<'_, 'n>,
    prepared: &Prepares<'n>,
    mains: &[Body<'n>],
    main_checked: &[Option<Checked<'static>>],
    census: &mut Census<'n>,
) -> Result<OwnVec<SpecializedBody<'n>, Alloc<'static>>, CheckFail> {
    let alloc = r.store.tys.alloc();
    let mut generic_inline: HashMap<ItemKey, (), Alloc<'static>> = HashMap::new_in(alloc);
    for m in mains.iter() {
        if !m.typeck.inline_fors.is_empty() {
            generic_inline.insert(m.owner, ());
        }
    }
    for (b, _) in census.bodies.iter() {
        if !b.typeck.inline_fors.is_empty() {
            generic_inline.insert(b.owner, ());
        }
    }
    let mut out: OwnVec<SpecializedBody<'n>, Alloc<'static>> = OwnVec::new_in(alloc);
    if generic_inline.is_empty() {
        return Ok(out);
    }

    // Track already-seen reached fns & bodies so merging sub-censuses never duplicates rows.
    let mut seen_fns: HashMap<ItemKey, (), Alloc<'static>> = HashMap::new_in(alloc);
    let mut seen_bodies: HashMap<ItemKey, (), Alloc<'static>> = HashMap::new_in(alloc);
    for m in mains.iter() {
        seen_fns.insert(m.owner, ());
        seen_bodies.insert(m.owner, ());
    }
    for (k, _) in census.fns.iter() {
        seen_fns.insert(*k, ());
    }
    for (b, _) in census.bodies.iter() {
        seen_bodies.insert(b.owner, ());
    }

    // Concrete instances of generic functions already queued/processed (including transitive wrappers).
    let mut concrete_calls: OwnVec<(ItemKey, OwnVec<PTy, Alloc<'static>>), Alloc<'static>> =
        OwnVec::new_in(alloc);

    let add_call = |calls: &mut OwnVec<(ItemKey, OwnVec<PTy, Alloc<'static>>), Alloc<'static>>,
                        tys: PTys<'_>,
                        callee: ItemKey,
                        args: OwnVec<PTy, Alloc<'static>>| {
        if args.is_empty() {
            return;
        }
        for &(k, ref existing) in calls.iter() {
            if k == callee && args_equal(tys, &args, tys, existing, alloc) {
                return;
            }
        }
        calls.push((callee, args));
    };

    // Helper: extract all ground calls from one checked body (optionally substituting caller_args).
    let collect_calls = |store_tys: &mut PortableTys<'static>,
                         checked: &Checked<'static>,
                         caller_args: Option<&[PTy]>,
                         calls: &mut OwnVec<(ItemKey, OwnVec<PTy, Alloc<'static>>), Alloc<'static>>| {
        let items = checked.callees.column::<1>();
        let arg_lists = checked.callees.column::<2>();
        let from_tys = checked.types.view();
        for (&callee, &list) in items.iter().zip(arg_lists.iter()) {
            let Ok(raw_args) = from_tys.list(list) else { continue };
            if raw_args.is_empty() {
                continue;
            }
            let mut imported: OwnVec<PTy, Alloc<'static>> =
                OwnVec::with_capacity_in(raw_args.len(), alloc);
            let mut all_ground = true;
            for &raw in raw_args {
                let Some(copied) = copy_ty_into(from_tys, raw, store_tys, caller_args, alloc) else {
                    all_ground = false;
                    break;
                };
                if !is_ground_ty(store_tys.view(), copied, alloc) {
                    all_ground = false;
                    break;
                }
                imported.push(copied);
            }
            if all_ground {
                add_call(calls, store_tys.view(), callee, imported);
            }
        }
    };

    // Seed from concrete roots and concrete reached bodies.
    for (m, c) in mains.iter().zip(main_checked.iter()) {
        if let Some(c) = c {
            let is_generic = r.store.fns.get(&m.owner).is_some_and(|h| h.parent + h.own > 0);
            if !is_generic {
                collect_calls(&mut r.store.tys, c, None, &mut concrete_calls);
            }
        }
    }
    for (b, c) in census.bodies.iter() {
        let is_generic = r.store.fns.get(&b.owner).is_some_and(|h| h.parent + h.own > 0);
        if !is_generic {
            collect_calls(&mut r.store.tys, c, None, &mut concrete_calls);
        }
    }

    let mut processed = 0usize;
    for _round in 0..MAX_SPEC_ROUNDS {
        if processed >= concrete_calls.len() {
            break;
        }
        let batch_end = concrete_calls.len();
        for idx in processed..batch_end {
            let (callee, args_copy): (ItemKey, OwnVec<PTy, Alloc<'static>>) = {
                let Some((k, args)) = concrete_calls.get(idx) else { continue };
                let mut v = OwnVec::with_capacity_in(args.len(), alloc);
                v.extend_from_slice(args);
                (*k, v)
            };
            if generic_inline.contains_key(&callee) {
                let already = out.iter().any(|s| {
                    s.orig == callee
                        && args_equal(r.store.tys.view(), &s.args, r.store.tys.view(), &args_copy, alloc)
                });
                if already {
                    continue;
                }
                let SpecializedBody { orig: _, args: args_copy, spec_key, body: spec_body, checked: mut spec_checked } =
                    specialize_one_imported(r, prepared, callee, args_copy)?;
                // Reach every newly discovered callee from the specialized unrolled body.
                let sub_census = {
                    let view = spec_body.lowered.view();
                    let spell = Spelling::of(&view).map_err(|_| CheckFail::InlineFor {
                        owner: callee,
                        at: tc::Range { start: 0, end: 0 },
                        why: "spelling specialized inline for body failed",
                    })?;
                    let scope = spec_body.scope(&spell);
                    reach(
                        r,
                        prepared,
                        spec_body.owner,
                        First::CheckedBody(&spec_body, &mut spec_checked),
                        &scope,
                    )
                };
                collect_calls(&mut r.store.tys, &spec_checked, None, &mut concrete_calls);
                for (k, outcome) in sub_census.fns {
                    if k == spec_key {
                        continue;
                    }
                    if seen_fns.insert(k, ()).is_none() {
                        census.fns.push((k, outcome));
                    }
                }
                for (b, c) in sub_census.bodies {
                    if b.owner == spec_key {
                        continue;
                    }
                    if !b.typeck.inline_fors.is_empty() {
                        generic_inline.insert(b.owner, ());
                    }
                    let is_generic = r.store.fns.get(&b.owner).is_some_and(|h| h.parent + h.own > 0);
                    if !is_generic {
                        collect_calls(&mut r.store.tys, &c, None, &mut concrete_calls);
                    }
                    if seen_bodies.insert(b.owner, ()).is_none() {
                        census.bodies.push((b, c));
                    }
                }
                census.recipes.extend(sub_census.recipes);
                census.edges.extend(sub_census.edges);
                out.push(SpecializedBody {
                    orig: callee,
                    args: args_copy,
                    spec_key,
                    body: spec_body,
                    checked: spec_checked,
                });
            } else {
                // If `callee` is a trait method at concrete ground args, resolve it to its
                // selected impl method so blanket impls with `inline for` specialize cleanly.
                if let Some((impl_item, impl_args)) = resolve_instance_full(r, callee, &args_copy)? {
                    if !impl_args.is_empty() {
                        add_call(&mut concrete_calls, r.store.tys.view(), impl_item, impl_args);
                    }
                }
                // Propagate concrete args through generic wrapper functions (e.g. `wrapper<U>` -> `score_all<U>`).
                let found_checked = census
                    .bodies
                    .iter()
                    .find(|(b, _)| b.owner == callee)
                    .map(|(_, c)| c);
                if let Some(c) = found_checked {
                    collect_calls(&mut r.store.tys, c, Some(&args_copy), &mut concrete_calls);
                }
            }
        }
        processed = batch_end;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refusal::SourceFile;
    use crate::run::{FrontChoice, NewFront, RunError, run_with};
    use blaze_lower::typeck::{Refused, Types, no_const_arg, to_typeck};
    use blaze_lower::{
        Cfg, CfgAnswer, ConstIdx, Edition, ExprId, NoModule, TypeId, View, lower_body,
    };

    struct NoCfg;

    impl Cfg for NoCfg {
        fn atom(&mut self, _: &str, _: Option<&str>) -> CfgAnswer {
            CfgAnswer::No
        }
    }

    struct DummyTypes;

    impl Types for DummyTypes {
        fn ty(&mut self, _view: &View<'_>, _t: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
            tys.intern(PNode::Infer).map_err(Refused::Check)
        }

        fn repeat_len(&mut self, _view: &View<'_>, _len: ExprId) -> Result<u64, Refused> {
            Ok(0)
        }

        fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _tys: &mut PortableTys<'_>) -> Result<PTy, Refused> {
            Err(no_const_arg(view, c))
        }
    }

    fn make_field<'a>(name: &str, alloc: Alloc<'a>) -> FieldSpec<'a> {
        let mut n = OwnString::with_capacity_in(name.len(), alloc);
        n.push_str(name);
        let mut a = OwnString::with_capacity_in(name.len(), alloc);
        a.push_str(name);
        FieldSpec { name: n, access: a }
    }

    const NEW_FRONT: NewFront<'static> = NewFront {
        package: "ladder",
        edition: "blaze-2026.11",
        shelf: None,
        overflow: blaze_build::OverflowChecks::Off,
    };

    const ZIP_HEAD: &str = "#![blaze::records]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_index: usize, _len: usize) -> ! {
    loop {}
}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for usize {}
impl Copy for bool {}
";

    fn run_zip(body: &str) -> Result<u8, RunError> {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let mut src = OwnString::with_capacity_in(ZIP_HEAD.len() + body.len(), alloc);
        src.push_str(ZIP_HEAD);
        src.push_str(body);
        let files = [SourceFile { path: "src/main.rs", text: src.as_str() }];
        run_with(&files, "src/main.rs", None, FrontChoice::New(NEW_FRONT), heap.shared())
    }

    #[test]
    fn zip_fields_lowers_and_renders_all_forms() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let text = "fn f(lhs: i32, rhs: i32) {
    inline for (a, b) in zip_fields(lhs, rhs) { a; b; }
    inline for (name, x, y) in zip_fields(&mut lhs, &rhs) { name; x; y; }
    inline for bad in zip_fields(lhs, rhs) { bad; }
}";
        let mut cfg = NoCfg;
        let mut values = NoModule;
        let Ok(lowered) = lower_body(text, Edition::E2024, &mut cfg, &mut values, alloc) else {
            assert!(false, "lower_body failed");
            return;
        };
        let view = lowered.view();
        let mut tys = PortableTys::new_in(alloc);
        let mut dt = DummyTypes;
        let Ok(tb) = to_typeck(&view, ItemKey(1), &mut tys, &mut dt, alloc) else {
            assert!(false, "to_typeck failed");
            return;
        };
        assert_eq!(tb.inline_fors.len(), 3);

        // Site 0: `(a, b)` in `zip_fields(lhs, rhs)`
        let Some(&site0) = tb.inline_fors.first() else {
            assert!(false, "missing site 0");
            return;
        };
        let InlineForKind::Zip { lhs: lhs0, rhs: rhs0 } = site0.kind else {
            assert!(false, "expected Zip kind for site 0");
            return;
        };
        assert!(matches!(tb.exprs.get(site0.iter.ix()), Some(tc::Expr::Missing)));
        let Some(tc::Expr::Block { stmts, tail: None, .. }) = tb.exprs.get(site0.expr.ix()) else {
            assert!(false, "expected wired 2-stmt probe block");
            return;
        };
        assert_eq!(stmts.len, 2);

        let mut fields = OwnVec::with_capacity_in(2, alloc);
        fields.push(make_field("x", alloc));
        fields.push(make_field("y", alloc));

        // `&T` + `&T`
        let rendered_ref = render_zip_unrolled(
            &view,
            site0,
            lhs0,
            rhs0,
            Some(Mutbl::Not),
            Some(Mutbl::Not),
            &fields,
            0,
            0,
            alloc,
        );
        assert_eq!(
            rendered_ref.as_ref().map(|s| s.as_str()),
            Ok("{ let __blaze_inline_lhs_0_0 = (lhs); let __blaze_inline_rhs_0_0 = (rhs); { let a = &(*__blaze_inline_lhs_0_0).x; let b = &(*__blaze_inline_rhs_0_0).x; { a; b; } } { let a = &(*__blaze_inline_lhs_0_0).y; let b = &(*__blaze_inline_rhs_0_0).y; { a; b; } } }")
        );

        // By-value `T` + `T`
        let rendered_val = render_zip_unrolled(&view, site0, lhs0, rhs0, None, None, &fields, 0, 1, alloc);
        assert_eq!(
            rendered_val.as_ref().map(|s| s.as_str()),
            Ok("{ let __blaze_inline_lhs_0_1 = (lhs); let __blaze_inline_rhs_0_1 = (rhs); { let a = __blaze_inline_lhs_0_1.x; let b = __blaze_inline_rhs_0_1.x; { a; b; } } { let a = __blaze_inline_lhs_0_1.y; let b = __blaze_inline_rhs_0_1.y; { a; b; } } }")
        );

        // Zero-field aggregate
        let rendered_empty = render_zip_unrolled(&view, site0, lhs0, rhs0, None, None, &[], 0, 2, alloc);
        assert_eq!(
            rendered_empty.as_ref().map(|s| s.as_str()),
            Ok("{ let _ = (lhs); let _ = (rhs); }")
        );

        // Site 1: `(name, x, y)` in `zip_fields(&mut lhs, &rhs)` with mixed mutability
        let Some(&site1) = tb.inline_fors.get(1) else {
            assert!(false, "missing site 1");
            return;
        };
        let InlineForKind::Zip { lhs: lhs1, rhs: rhs1 } = site1.kind else {
            assert!(false, "expected Zip kind for site 1");
            return;
        };
        let rendered_named = render_zip_unrolled(
            &view,
            site1,
            lhs1,
            rhs1,
            Some(Mutbl::Mut),
            Some(Mutbl::Not),
            &fields,
            1,
            0,
            alloc,
        );
        assert_eq!(
            rendered_named.as_ref().map(|s| s.as_str()),
            Ok("{ let __blaze_inline_lhs_1_0 = (&mut lhs); let __blaze_inline_rhs_1_0 = (&rhs); { let name: &'static str = \"x\"; let x = &mut (*__blaze_inline_lhs_1_0).x; let y = &(*__blaze_inline_rhs_1_0).x; { name; x; y; } } { let name: &'static str = \"y\"; let x = &mut (*__blaze_inline_lhs_1_0).y; let y = &(*__blaze_inline_rhs_1_0).y; { name; x; y; } } }")
        );

        // Site 2: invalid 1-binding pattern rejected
        let Some(&site2) = tb.inline_fors.get(2) else {
            assert!(false, "missing site 2");
            return;
        };
        let InlineForKind::Zip { lhs: lhs2, rhs: rhs2 } = site2.kind else {
            assert!(false, "expected Zip kind for site 2");
            return;
        };
        let rendered_bad = render_zip_unrolled(&view, site2, lhs2, rhs2, None, None, &fields, 0, 0, alloc);
        assert!(rendered_bad.is_err());

        // `zip_fields_match` checks aggregate shape and field names
        assert!(zip_fields_match(AggShape::Tuple(2), &fields, AggShape::Tuple(2), &fields));
        assert!(!zip_fields_match(AggShape::Tuple(2), &fields, AggShape::Tuple(3), &fields));
        assert!(!zip_fields_match(AggShape::Struct(ItemKey(1)), &fields, AggShape::Struct(ItemKey(2)), &fields));
    }

    #[test]
    fn zip_fields_generic_eq_diff_copy_and_by_value_run() {
        let got = run_zip(
            "
#[repr(C)]
struct Words {
    data: usize,
    len: usize,
}
impl Copy for Words {}
union Wide {
    s: &'static str,
    words: Words,
}
fn str_len(s: &'static str) -> u32 {
    unsafe { Wide { s }.words.len as u32 }
}

trait EqVal {
    fn eq_val(&self, other: &Self) -> bool;
}

trait CopyFrom {
    fn copy_from(&mut self, src: &Self);
}

trait TakeDiff {
    fn take_diff(self, other: Self) -> u32;
}

struct A(u32);
struct B(u32);
struct C(u32);

impl EqVal for A {
    fn eq_val(&self, other: &A) -> bool { self.0 == other.0 }
}
impl EqVal for B {
    fn eq_val(&self, other: &B) -> bool { self.0 == other.0 }
}
impl EqVal for C {
    fn eq_val(&self, other: &C) -> bool { self.0 == other.0 }
}

impl CopyFrom for A {
    fn copy_from(&mut self, src: &A) { self.0 = src.0; }
}
impl CopyFrom for B {
    fn copy_from(&mut self, src: &B) { self.0 = src.0; }
}

impl TakeDiff for A {
    fn take_diff(self, other: A) -> u32 { if self.0 >= other.0 { self.0 - other.0 } else { other.0 - self.0 } }
}
impl TakeDiff for B {
    fn take_diff(self, other: B) -> u32 { if self.0 >= other.0 { self.0 - other.0 } else { other.0 - self.0 } }
}

struct Named {
    alpha: A,
    beta: B,
}

struct Pair(A, B);

struct Empty;

fn eq_all<T>(lhs: &T, rhs: &T) -> bool {
    inline for (a, b) in zip_fields(lhs, rhs) {
        if !a.eq_val(b) {
            return false;
        }
    }
    true
}

fn diff_name_len<T>(lhs: &T, rhs: &T) -> u32 {
    let mut acc: u32 = 0;
    inline for (name, a, b) in zip_fields(lhs, rhs) {
        if !a.eq_val(b) {
            acc = acc + str_len(name);
        }
    }
    acc
}

fn copy_into<T>(dst: &mut T, src: &T) {
    inline for (d, s) in zip_fields(dst, src) {
        d.copy_from(s);
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut score: u32 = 0;
    // 1. Structural records (including swapped literal field order): +1, +0 => 1
    let r1 = { hp: A(10), mp: B(20) };
    let r2 = { mp: B(20), hp: A(10) };
    let r3 = { hp: A(10), mp: B(99) };
    if eq_all(&r1, &r2) { score = score + 1; }
    if !eq_all(&r1, &r3) { score = score + 2; }
    // 2. Heterogeneous tuples: +4 => 7
    let t1 = (A(1), B(2), C(3));
    let t2 = (A(1), B(2), C(3));
    let t3 = (A(1), B(9), C(3));
    if eq_all(&t1, &t2) && !eq_all(&t1, &t3) { score = score + 4; }
    // 3. Named struct & 3-binding diff: \"alpha\" (5) differs -> +5; equal pair -> +8 => 20
    let n1 = Named { alpha: A(5), beta: B(6) };
    let n2 = Named { alpha: A(7), beta: B(6) };
    let n3 = Named { alpha: A(5), beta: B(6) };
    if eq_all(&n1, &n3) { score = score + 8; }
    score = score + diff_name_len(&n1, &n2);
    // 4. Tuple struct: +10 => 30
    let p1 = Pair(A(3), B(4));
    let p2 = Pair(A(3), B(4));
    if eq_all(&p1, &p2) { score = score + 10; }
    // 5. Zero-field aggregates (unit struct and empty tuple): +2 => 32
    let e1 = Empty;
    let e2 = Empty;
    if eq_all(&e1, &e2) { score = score + 1; }
    let u1 = ();
    let u2 = ();
    if eq_all(&u1, &u2) { score = score + 1; }
    // 6. Mixed mutability &mut + & (both generic copy_into and concrete zip_fields):
    //    dst becomes { hp: 10, mp: 20 } -> +30 => 62
    let mut dst = { hp: A(1), mp: B(2) };
    copy_into(&mut dst, &r1);
    let mut dst2 = Named { alpha: A(0), beta: B(0) };
    inline for (d, s) in zip_fields(&mut dst2, &n1) {
        d.copy_from(s);
    }
    score = score + dst.hp.0 + dst.mp.0 + dst2.alpha.0 + dst2.beta.0;
    // 7. By-value non-Copy zip_fields: |9-4| + |8-3| = 10 => 72 + 11 = 83
    let mv1 = { x: A(9), y: B(8) };
    let mv2 = { x: A(4), y: B(3) };
    inline for (a, b) in zip_fields(mv1, mv2) {
        score = score + a.take_diff(b);
    }
    // Total: 1+2+4+8+5+10+1+1 + (10+20+5+6=41) + 10 = 83
    score as i32
}
",
        );
        assert!(matches!(got, Ok(83)));
    }

    #[test]
    fn zip_fields_rejects_mismatched_and_non_aggregate_types() {
        let cases = [
            // Mismatched record schemas
            "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = { x: 1u32 };
    let b = { y: 1u32 };
    inline for (l, r) in zip_fields(&a, &b) { let _ = l; let _ = r; }
    0
}
",
            // Mismatched tuple arities
            "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = (1u32, 2u32);
    let b = (1u32, 2u32, 3u32);
    inline for (l, r) in zip_fields(&a, &b) { let _ = l; let _ = r; }
    0
}
",
            // Mismatched named struct types
            "
struct S1 { x: u32 }
struct S2 { x: u32 }
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = S1 { x: 1 };
    let b = S2 { x: 1 };
    inline for (l, r) in zip_fields(&a, &b) { let _ = l; let _ = r; }
    0
}
",
            // Non-aggregate types
            "
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a: u32 = 1;
    let b: u32 = 2;
    inline for (l, r) in zip_fields(&a, &b) { let _ = l; let _ = r; }
    0
}
",
        ];
        for src in cases {
            assert!(run_zip(src).is_err());
        }
    }
}

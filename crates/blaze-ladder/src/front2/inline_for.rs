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
use blaze_lower::typeck::{InlineForSite, TypeckBody};
use blaze_own::{AString, AVec, Alloc, String as OwnString, Vec as OwnVec};
use blaze_typeck::{
    self as tc, Checked, ItemKey, List, Mutbl, PNode, PTy, PTys, PortableTys,
};

use super::body::{Body, lower_body_with_text};
use super::check::{CheckFail, Records, check_body, check_rounds_probe};
use super::heads::{FnHead, Want};
use super::heads_regions::Scope as RegionScope;
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

/// Classification of an `inline for` iterable's type.
enum IterKind<'a> {
    /// Depends on a generic type parameter (`T`, `&T`, `&mut T`): defer to monomorphic specialization.
    Generic,
    /// Concrete record, tuple, or single-variant struct (optionally behind `&` or `&mut`).
    Concrete { ref_mutbl: Option<Mutbl>, fields: AVec<'a, FieldSpec<'a>> },
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
            IterKind::Concrete { ref_mutbl, fields }
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
            IterKind::Concrete { ref_mutbl, fields }
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
                return IterKind::Concrete { ref_mutbl, fields: OwnVec::new_in(alloc) };
            };
            let Some(adt_fields) = adt.fields.get(variant.fields.range()) else {
                return IterKind::NotIterable("struct field range is out of bounds");
            };
            match variant.ctor {
                tc::CtorKind::Unit => {
                    IterKind::Concrete { ref_mutbl, fields: OwnVec::new_in(alloc) }
                }
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
                    IterKind::Concrete { ref_mutbl, fields }
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
                    IterKind::Concrete { ref_mutbl, fields }
                }
            }
        }
        _ => IterKind::NotIterable(
            "inline for requires a structural record, tuple, or struct (or a reference to one)",
        ),
    }
}

/// One byte-range replacement for an `inline for` expression inside an item's source text.
struct Rewrite<'a> {
    start: usize,
    end: usize,
    replacement: AString<'a>,
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
        match ref_mutbl {
            None => {
                let _ = write!(
                    out,
                    "__blaze_inline_iter_{round}_{site_idx}.{}; {body_src} }}",
                    field.access.as_str()
                );
            }
            Some(Mutbl::Not) => {
                let _ = write!(
                    out,
                    "&(*__blaze_inline_iter_{round}_{site_idx}).{}; {body_src} }}",
                    field.access.as_str()
                );
            }
            Some(Mutbl::Mut) => {
                let _ = write!(
                    out,
                    "&mut (*__blaze_inline_iter_{round}_{site_idx}).{}; {body_src} }}",
                    field.access.as_str()
                );
            }
        }
    }
    out.push_str(" }");
    Some(out)
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
            let iter_ty = probe
                .expr_ty
                .get(site.iter.ix())
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
                    let at = body.typeck.expr_ranges.get(site.iter.ix()).copied().unwrap_or_default();
                    return Err(CheckFail::InlineFor { owner: body.owner, at, why });
                }
                IterKind::Concrete { ref_mutbl, fields } => {
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
        if rewrites.is_empty() {
            if any_generic {
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
fn install_spec_head<'n>(
    r: &mut Records<'_, 'n>,
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
                let spec_key = synth_spec_key(callee, &args_copy, r.store.tys.view(), alloc);
                let lib = install_spec_head(r, callee, spec_key, &args_copy, None)?;
                let orig_text = r
                    .store
                    .fns
                    .get(&callee)
                    .map(|h| h.text)
                    .ok_or(CheckFail::InlineFor {
                        owner: callee,
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
                    Some((callee, &args_copy)),
                    None,
                )
                .map_err(|_| CheckFail::InlineFor {
                    owner: callee,
                    at: tc::Range { start: 0, end: 0 },
                    why: "lowering specialized generic inline for body failed",
                })?;
                let mut spec_checked = unroll_concrete_body(r, prepared, &mut spec_body)?;
                // Update `spec_head.text` so `FnFacts::text` and `symbols_of` see the expanded text.
                if let Some(exp) = spec_body.expanded_text {
                    let _ = install_spec_head(r, callee, spec_key, &args_copy, Some(exp));
                }
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

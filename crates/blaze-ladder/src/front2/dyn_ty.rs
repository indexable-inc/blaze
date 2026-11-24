//! `dyn` in type position: the facts of a trait's written members that dyn compatibility (the
//! Reference, "Dyn compatibility") is decided from, read when its head is made and handed to
//! the checker as `TraitDef.members`; the checker decides and reports E0038
//! (`blaze_typeck::dyn_compat`, the rule's one home). The type itself is built in
//! [`super::types::Builder`] through `PortableTys::dynamic`.

use std::alloc::Global;
use std::format;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_lower::View;
use blaze_lower::form as lf;
use blaze_surface::Range;
use blaze_typeck::dyn_compat::{FnFacts, MemberShape, Receiver, Unread};
use blaze_typeck::{ItemKey, List, Name, PClause, PNode, PTy, PathIdx, PortableTys};

use super::preds::{lifetimes_only, segment_args};
use super::types::{Bounded, Builder, Env, Heads, Why, children, children_of, intern, intern_list, path_texts, path_texts_of, subst_clause};
use super::source::NamesSource;
use crate::resolve::{Ns, Paths};

/// One written member's dyn-compatibility facts (`v` its lowered text), `module` where its
/// names resolve, `sized` the `#[lang = "sized"]` trait, `pointers` the library's `Box`, `Rc`,
/// `Arc` and `Pin` a typed receiver (`self: Pin<&mut Self>`) is read against
/// ([`super::receiver::classify`]). The checker decides from them
/// (`blaze_typeck::dyn_compat`, the rule's one home), through `TraitDef.members`.
pub fn member<'n, S: NamesSource<'n>>(resolver: &Paths<S>, module: LibKey, sized: Option<LibKey>, pointers: &super::receiver::Pointers, detail: Member, v: &View<'_>) -> Result<MemberShape, Why> {
    use blaze_lower::head;
    match detail {
        Member::Const => return Ok(MemberShape::Const),
        Member::Type => return Ok(MemberShape::Type { generic: !v.rows(v.list(head::GENERICS))?.is_empty() }),
        Member::Fn => {}
    }
    let mut receiver = Receiver::Absent;
    // A typed receiver whose type is not read (an alias, a projection, a path that does not
    // resolve): a named gap, unless `where Self: Sized` makes the method non-dispatchable.
    let mut unread = None;
    let mut written = Vec::new_in(Global);
    for (k, &n) in v.rows(v.list(head::PARAMS))?.iter().enumerate() {
        let p = lf::Param::decode(n)?;
        match (k, p.self_kind) {
            (0, lf::SelfKind::Value) => receiver = Receiver::Value,
            (0, lf::SelfKind::Ref(blaze_typeck::Mutbl::Mut)) => receiver = Receiver::RefMut,
            (0, lf::SelfKind::Ref(_)) => receiver = Receiver::Ref,
            (0, lf::SelfKind::Typed) => match p.ty {
                Some(t) => match super::receiver::classify(resolver, module, pointers, v, t)? {
                    Ok(r) => receiver = r,
                    Err(u) => unread = Some(u),
                },
                None => unread = Some(Unread::TypedReceiver),
            },
            _ => written.extend(p.ty.map(|t| (t, false))),
        }
    }
    let mut f = FnFacts {
        receiver,
        own_non_lifetime_params: 0,
        where_self_sized: false,
        is_async: v.head[head::FLAGS] & blaze_lower::machine::sig::ASYNC != 0,
        opaque_return: false,
        self_outside_receiver: false,
    };
    for &n in v.rows(v.list(head::GENERICS))? {
        if !matches!(lf::GenericParam::decode(n)?, lf::GenericParam::Lifetime { .. }) {
            f.own_non_lifetime_params += 1;
        }
    }
    // "Explicitly non-dispatchable functions require: Have a `where Self: Sized` bound": then
    // nothing else of the signature is read.
    'clauses: for &n in v.rows(v.list(head::WHERE))? {
        let lf::WherePred::Type { ty, bounds, .. } = lf::WherePred::decode(n)? else { continue };
        if !is_self(v, ty)? {
            continue;
        }
        for &b in v.rows(bounds)? {
            let lf::Bound::Trait { ty: bt, mods, .. } = lf::Bound::decode(b)? else { continue };
            let lf::TypeRef::Path(p) = v.ty(bt)? else { continue };
            let (abs, texts, _, r) = path_texts(v, p)?;
            let found = resolver.path(module, abs, &texts, Ns::Type).map_err(|e| Why::Resolve(e, r))?;
            if !mods.maybe && Some(found.key) == sized && found.consumed as usize == texts.len() {
                f.where_self_sized = true;
                break 'clauses;
            }
        }
    }
    if f.where_self_sized {
        return Ok(MemberShape::Fn(f));
    }
    if let Some(u) = unread {
        return Ok(MemberShape::Unread(u));
    }
    if let Some(t) = v.head_opt(head::TY) {
        written.push((lf::TypeId(t), true));
    }
    // "Does not use `Self` except in the type of the receiver": `Self::X` is a projection, not
    // `Self` (rustc's `contains_illegal_self_type_reference` passes it). An `impl Trait` is an
    // opaque type in the return and a type parameter in an argument; its bounds are not walked
    // (either already breaks the rule).
    let mut kids = Vec::new_in(Global);
    while let Some((t, in_return)) = written.pop() {
        if is_self(v, t)? {
            f.self_outside_receiver = true;
            continue;
        }
        if let lf::TypeRef::Impl(_) = v.ty(t)? {
            match in_return {
                true => f.opaque_return = true,
                false => f.own_non_lifetime_params += 1,
            }
            continue;
        }
        kids.clear();
        if children(v, t, &mut kids).is_err() {
            return Ok(MemberShape::Unread(Unread::SignatureType));
        }
        written.extend(kids.iter().map(|&k| (k, in_return)));
    }
    Ok(MemberShape::Fn(f))
}

/// The kinds of trait members the verdict reads.
#[derive(Clone, Copy, Debug)]
pub enum Member {
    Fn,
    Type,
    Const,
}

/// One `dyn` bound: its trait path and last segment (`None` for a lifetime, which the checker
/// erases: typeck.md "Trait objects" (f)).
pub(super) fn dyn_bound(v: &View<'_>, b: lf::Bound, r: Range) -> Result<Option<(PathIdx, lf::Segment)>, Why> {
    let lf::Bound::Trait { ty, binder, mods } = b else {
        return match b {
            lf::Bound::Lifetime(_) => Ok(None),
            _ => Err(Why::Unsupported("`use<..>` in a trait object type", r)),
        };
    };
    lifetimes_only(v, binder, r)?;
    if mods.maybe || mods.negative || mods.asyncness || mods.konst || mods.maybe_const {
        return Err(Why::Unsupported("a modified bound (`?Tr`, `!Tr`, `const Tr`, `async Tr`) in a trait object type", r));
    }
    let lf::TypeRef::Path(p) = v.ty(ty)? else { return Err(Why::Program("a trait object bound that is no path".into(), r)) };
    let segs = v.segments(v.path(p)?)?;
    let mut last = None;
    for (i, &n) in segs.iter().enumerate() {
        let s = lf::Segment::decode(n)?;
        if i + 1 < segs.len() && (s.has_args || s.parenthesized) {
            return Err(Why::Unsupported("generic arguments before a trait object's last segment", r));
        }
        last = Some(s);
    }
    Ok(last.map(|s| (p, s)))
}

/// The written types of a `dyn` bound's last segment, in the order [`Builder::dyn_ty`] reads
/// them: `Fn(A, B) -> C` its inputs then `C`; else each type argument and each `Name = T`'s `T`.
pub(super) fn dyn_arg_types(v: &View<'_>, s: &lf::Segment, r: Range, out: &mut Vec<lf::TypeId, Global>) -> Result<(), Why> {
    if s.return_notation {
        return Err(Why::Unsupported("return type notation (`Tr(..)`)", r));
    }
    for &n in v.rows(s.args)? {
        match lf::GenericArg::decode(n)? {
            lf::GenericArg::Type(t) => out.push(t),
            lf::GenericArg::Lifetime(_) if !s.parenthesized => {}
            lf::GenericArg::AssocType { args, ty, .. } if !s.parenthesized && args.len == 0 => out.push(ty),
            _ => return Err(Why::Unsupported("this generic argument in a trait object type", r)),
        }
    }
    if s.parenthesized {
        out.extend(s.ty);
    }
    Ok(())
}

/// `needle` occurs in one of `l` (by a work list).
fn mentions(tys: &PortableTys<'_>, l: &[PTy], needle: PTy, r: Range) -> Result<bool, Why> {
    let mut todo = Vec::new_in(Global);
    todo.extend_from_slice(l);
    while let Some(t) = todo.pop() {
        if t == needle {
            return Ok(true);
        }
        children_of(tys.view(), t, &mut todo).ok_or_else(|| Why::Program("a type refers past the type table".into(), r))?;
    }
    Ok(false)
}

impl<'b, 'n, H: Heads, S: NamesSource<'n>> Builder<'b, H, S> {
    /// `dyn A<X, N = T> + Auto + 'a` (the type node of typeck.md "Trait objects" (a)): the
    /// principal's `ExTrait(A, args after Self)`, an `ExProj` per binding (its owner found
    /// through the supertraits), an `ExAuto` per auto trait (an auto trait by its own head), the
    /// lifetime dropped; `Fn(A, B) -> C` is `Fn<(A, B)>` with `Output = C` (`()` when no `->`).
    pub(super) fn dyn_ty(&self, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, bounds: lf::List, kids: &[PTy], r: Range) -> Result<PTy, Why> {
        // `Self` of the trait object while its arguments are made: no parameter has this index,
        // so a default naming `Self` is seen (E0393) instead of read as some type.
        let erased = intern(tys, PNode::Param(u32::MAX))?;
        let mut at = 0usize;
        let mut take = || {
            at += 1;
            kids.get(at - 1).copied().ok_or_else(|| Why::Program("a trait object's argument types are not all made".into(), r))
        };
        let mut preds = Vec::new_in(Global);
        let mut principal: Option<LibKey> = None;
        for &b in v.rows(bounds)? {
            let Some((p, seg)) = dyn_bound(v, lf::Bound::decode(b)?, r)? else { continue };
            // Its argument types are made already (`kids`, in `dyn_arg_types`'s order).
            let (given, binds, nested) = segment_args(tys, v, &seg, r, &mut |_, _| take())?;
            // `dyn Tr<Assoc: Bound>`: the existential's predicates have no form for a bound on
            // its projection yet.
            if !nested.is_empty() {
                return Err(Why::Unsupported("an associated type bound in a trait object (`dyn Tr<Assoc: Bound>`)", r));
            }
            let (abs, texts, _, pr) = path_texts_of(v, p, true)?;
            // The traits this bound names (`Self` erased) and its `Name = T` projections (the
            // owner's checker key, its arguments `Self` first): one trait and its bindings, or
            // what a trait alias stands for (the unstable book, `trait_alias`: `dyn Alias` is
            // the object of its right-hand side, each trait's dyn-compatibility its own).
            let mut traits: Vec<(LibKey, List), Global> = Vec::new_in(Global);
            let mut projs: Vec<(ItemKey, Name, List, PTy), Global> = Vec::new_in(Global);
            match self.bound_target(env, v, p, abs, &texts, pr)? {
                Bounded::Trait(key) => {
                    let (key, args) = self.trait_args(tys, key, pr, erased, &given)?;
                    if !binds.is_empty() && self.heads.trait_auto(key).ok_or(Why::Need(key))? {
                        return Err(Why::Program("an auto trait in a trait object takes no arguments".into(), pr));
                    }
                    let shown = v.text_at(v.path_range(p)?)?;
                    for (name, term) in binds {
                        let (owner, oargs) = self.assoc_owner(tys, key, args, name, pr, shown)?.ok_or_else(|| Why::AssocNotFound(shown.into(), pr))?;
                        projs.push((self.heads.checker(owner), name, oargs, term));
                    }
                    traits.push((key, args));
                }
                Bounded::Alias(alias) => {
                    if !binds.is_empty() {
                        return Err(Why::Unsupported("an associated type binding on a trait alias (`dyn Alias<Name = T>`)", pr));
                    }
                    let mut clauses = Vec::new_in(Global);
                    self.alias_clauses(tys, alias, pr, erased, &given, &mut clauses)?;
                    let head = self.heads.trait_alias(alias).ok_or(Why::Need(alias))?;
                    let mut sargs = Vec::with_capacity_in(given.len() + 1, Global);
                    sargs.push(erased);
                    sargs.extend_from_slice(&given);
                    for &(k, l) in &head.traits {
                        let PClause::Trait { args, .. } = subst_clause(tys, PClause::Trait { trait_: self.heads.checker(k), args: l }, &sargs)? else { continue };
                        traits.push((k, args));
                    }
                    for c in clauses {
                        let (args, proj) = match c {
                            PClause::Trait { args, .. } => (args, None),
                            PClause::Projection { trait_, args, name, term } => (args, Some((trait_, name, term))),
                            PClause::Const { .. } => return Err(Why::Unsupported("a trait alias with `where const` in a trait object", pr)),
                            PClause::TypeOutlives { .. } => return Err(Why::Unsupported("a trait alias with a type outlives bound (`T: 'a`) in a trait object", pr)),
                            PClause::RegionOutlives { .. } => return Err(Why::Unsupported("a trait alias with a lifetime outlives bound (`'a: 'b`) in a trait object", pr)),
                            PClause::ForFields { .. } => return Err(Why::Unsupported("a trait alias with `for<F in fields(..)>` in a trait object", pr)),
                        };
                        let first = tys.view().list(args).map_err(|_| Why::Program("a list outside the table".into(), pr))?.first().copied();
                        if first != Some(erased) {
                            return Err(Why::Unsupported("a trait alias in a trait object whose clauses bound a type other than `Self`", pr));
                        }
                        if let Some((owner, name, term)) = proj {
                            projs.push((owner, name, args, term));
                        }
                    }
                }
            }
            for (key, args) in traits {
                if self.heads.trait_auto(key).ok_or(Why::Need(key))? {
                    if tys.view().list(args).map_err(|_| Why::Program("a list outside the table".into(), pr))?.len() != 1 {
                        return Err(Why::Program("an auto trait in a trait object takes no arguments".into(), pr));
                    }
                    preds.push(intern(tys, PNode::ExAuto(self.heads.checker(key)))?);
                    continue;
                }
                if let Some(prev) = principal {
                    return Err(Why::TwoPrincipals(prev, key, r));
                }
                principal = Some(key);
                let rest = after_self(tys, args, erased, pr)?;
                preds.push(intern(tys, PNode::ExTrait(self.heads.checker(key), rest))?);
            }
            for (owner, name, oargs, term) in projs {
                let rest = after_self(tys, oargs, erased, pr)?;
                preds.push(intern(tys, PNode::ExProj { trait_: owner, name, args: rest, term })?);
            }
        }
        if at != kids.len() {
            return Err(Why::Program("a trait object's argument types are not all read".into(), r));
        }
        tys.dynamic(&preds).map_err(|e| Why::Program(format!("the trait object's rows: {e:?}").into_boxed_str(), r))
    }

}

/// A trait's arguments after `Self`, refused where one still names the erased `Self` (E0393, or
/// a supertrait's argument written with `Self`).
fn after_self(tys: &mut PortableTys<'_>, args: List, erased: PTy, r: Range) -> Result<List, Why> {
    let l = tys.view().list(args).map_err(|_| Why::Program("a list outside the table".into(), r))?;
    let mut rest = Vec::with_capacity_in(l.len(), Global);
    rest.extend_from_slice(l.get(1..).unwrap_or(&[]));
    if mentions(tys, &rest, erased, r)? {
        return Err(Why::Program("a trait object's trait argument names `Self` (a default of `Self` must be written out: E0393)".into(), r));
    }
    intern_list(tys, &rest)
}

/// `t` is the path `Self` alone.
pub(super) fn is_self(v: &View<'_>, t: lf::TypeId) -> Result<bool, Why> {
    let lf::TypeRef::Path(p) = v.ty(t)? else { return Ok(false) };
    let segs = v.segments(v.path(p)?)?;
    Ok(segs.len() == 1 && lf::Segment::decode(segs[0])?.kind == lf::SegKind::SelfTy)
}

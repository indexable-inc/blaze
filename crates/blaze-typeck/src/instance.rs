//! Instances at ground arguments, for monomorphization: which impl a trait goal selects
//! ([`select_impl`]) and which body a trait method's call runs ([`resolve_instance`]).
//!
//! Both answer through the same solver the checker uses (an [`InferCtx`]), not a second
//! matcher: each impl of the trait in the self type's bucket (and the blanket bucket) is
//! instantiated with fresh variables, its trait reference related to the goal's, and its
//! where-clauses proved; the impl that survives is the answer. Coherence leaves at most one;
//! two is a typed error naming both. Every read goes through `Reads`, the one seam; a question
//! the world cannot answer is [`ResolveError::Missing`], never a guess.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ItemKey, List};
use crate::infer::InferCtx;
use crate::portable::{PNode, PTy, PTys, PortableTys};
use crate::ty::{Cause, Goal, Obligation, Ty, TyView, TypeCtx};
use crate::world::{ImplHeader, PClause, Question, Reads, Shape, World};

/// The deepest portable type imported or exported (rule 18).
const MAX_DEPTH: u32 = 256;

/// The body a call runs and its generic arguments, in the caller's ground table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub item: ItemKey,
    pub args: List,
}

/// Why an instance did not resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The world did not answer this question.
    Missing(Question),
    /// No impl of `trait_` holds for the ground self type.
    NoImpl { trait_: ItemKey, item: ItemKey },
    /// Two impls of `trait_` hold (coherence should have refused one).
    Overlap { trait_: ItemKey, first: ItemKey, second: ItemKey },
    /// An argument still holds an inference variable or a parameter after selection.
    NotGround { item: ItemKey },
    /// A malformed record, or a bound passed.
    Check(CheckError),
}

impl core::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ResolveError::Missing(q) => write!(f, "resolving an instance: the world has no answer to {q:?}"),
            ResolveError::NoImpl { trait_, item } => {
                write!(f, "resolving {item:?}: no impl of trait {trait_:?} holds at these arguments")
            }
            ResolveError::Overlap { trait_, first, second } => {
                write!(f, "selecting an impl of {trait_:?}: both {first:?} and {second:?} hold (overlapping impls)")
            }
            ResolveError::NotGround { item } => write!(f, "resolving {item:?}: an argument is not ground"),
            ResolveError::Check(e) => write!(f, "resolving an instance: {e}"),
        }
    }
}

impl From<CheckError> for ResolveError {
    fn from(e: CheckError) -> ResolveError {
        ResolveError::Check(e)
    }
}

impl From<Stop> for ResolveError {
    fn from(s: Stop) -> ResolveError {
        match s {
            Stop::Need(q) => ResolveError::Missing(q),
            Stop::Error(e) => ResolveError::Check(e),
        }
    }
}

/// Where [`import`] finds a closure's facts (typeck.md, "A closure's identity").
pub(crate) trait ClosureSource {
    /// `Question::Closure(owner, ordinal)`'s answer imported at `args`: `TyView::ClosureOf`.
    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, CheckError>;
}

/// No closure can occur (an impl header, a where-clause): one is refused as a body type.
pub(crate) struct NoClosures;

impl ClosureSource for NoClosures {
    fn closure_of(&mut self, _: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, _: crate::ty::TyList, _: u32) -> Result<Ty, CheckError> {
        Err(CheckError::Malformed(crate::error::Malformed::BodyTypeInRecord(Question::Closure(owner, ordinal))))
    }
}

/// The world, through the read seam; a missing answer is kept in `need` (the caller's
/// `ResolveError::Missing`), since `import` speaks `CheckError`.
pub(crate) struct Asked<'r, 'w, 'a, W: World> {
    pub reads: &'r mut Reads<'w, 'a, W>,
    pub need: Option<Question>,
}

impl<W: World> ClosureSource for Asked<'_, '_, '_, W> {
    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, CheckError> {
        let facts = match self.reads.closure(owner, ordinal) {
            Ok(Some(f)) => f,
            Ok(None) => return Err(CheckError::Malformed(crate::error::Malformed::BodyTypeInRecord(Question::Closure(owner, ordinal)))),
            Err(Stop::Error(e)) => return Err(e),
            Err(Stop::Need(q)) => {
                self.need = Some(q);
                return Err(CheckError::Limit(Limit::Export));
            }
        };
        let n = cx.list_len(args);
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, Alloc::global());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        let sig = import(cx, &mut NoClosures, facts.types, facts.sig, &subst, depth)?;
        cx.mk(TyView::ClosureOf { owner, ordinal, args, sig, kind: facts.kind })
    }
}

/// Imports portable type `t` of `types` into `cx`, `Param(i)` becoming `subst[i]`.
fn import(cx: &mut InferCtx<'_>, src: &mut dyn ClosureSource, types: PTys<'_>, t: PTy, subst: &[Ty], depth: u32) -> Result<Ty, CheckError> {
    if depth > MAX_DEPTH {
        return Err(CheckError::Limit(Limit::Export));
    }
    let d = depth + 1;
    let v = match types.node(t)? {
        PNode::Bool => TyView::Bool,
        PNode::Char => TyView::Char,
        PNode::Int(i) => TyView::Int(i),
        PNode::Uint(u) => TyView::Uint(u),
        PNode::Float(f) => TyView::Float(f),
        PNode::Str => TyView::Str,
        PNode::Never => TyView::Never,
        PNode::Tuple(l) => TyView::Tuple(import_list(cx, src, types,l, subst, d)?),
        PNode::Ref(m, p) => TyView::Ref(m, import(cx, src, types,p, subst, d)?),
        PNode::Ptr(m, p) => TyView::Ptr(m, import(cx, src, types,p, subst, d)?),
        PNode::Array(e, n) => TyView::Array(import(cx, src, types,e, subst, d)?, import(cx, src, types,n, subst, d)?),
        PNode::Slice(e) => TyView::Slice(import(cx, src, types,e, subst, d)?),
        PNode::Pat(b, l) => TyView::Pat(import(cx, src, types,b, subst, d)?, import_list(cx, src, types,l, subst, d)?),
        PNode::Adt(k, l) => TyView::Adt(k, import_list(cx, src, types,l, subst, d)?),
        PNode::FnDef(k, l) => TyView::FnDef(k, import_list(cx, src, types,l, subst, d)?),
        PNode::FnPtr(l, h) => TyView::FnPtr(import_list(cx, src, types,l, subst, d)?, h),
        PNode::Param(i) => match subst.get(i as usize) {
            Some(&s) => return Ok(s),
            None => TyView::Param(i),
        },
        PNode::Alias { trait_, name, args } => {
            TyView::Alias { trait_, name, args: import_list(cx, src, types,args, subst, d)? }
        }
        PNode::Opaque(k, l) => TyView::Opaque(k, import_list(cx, src, types,l, subst, d)?),
        PNode::Error => TyView::Error,
        PNode::ConstParam(i) => match subst.get(i as usize) {
            Some(&s) => return Ok(s),
            None => TyView::ConstParam(i),
        },
        PNode::ConstValue { ty, bits } => TyView::ConstValue { ty: import(cx, src, types,ty, subst, d)?, bits },
        // A trait object's rows keep their order: substitution changes no sort key.
        PNode::Dynamic(l) => TyView::Dynamic(import_list(cx, src, types,l, subst, d)?),
        PNode::ExTrait(k, l) => TyView::ExTrait(k, import_list(cx, src, types,l, subst, d)?),
        PNode::ExProj { trait_, name, args, term } => {
            let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len as usize + 1, Alloc::global());
            for &a in types.list(args)? {
                v.push(import(cx, src, types,a, subst, d)?);
            }
            v.push(import(cx, src, types,term, subst, d)?);
            TyView::ExProj { trait_, name, args_term: cx.mk_list(&v)? }
        }
        PNode::ExAuto(k) => TyView::ExAuto(k),
        // Body-only nodes never reach a ground table or an item's record.
        PNode::Closure { owner, ordinal, args } => {
            let args = import_list(cx, src, types, args, subst, d)?;
            return src.closure_of(cx, owner, ordinal, args, d);
        }
        PNode::Future(..) | PNode::Infer => return Err(CheckError::Limit(Limit::Export)),
    };
    cx.mk(v)
}

fn import_list(
    cx: &mut InferCtx<'_>,
    src: &mut dyn ClosureSource,
    types: PTys<'_>,
    l: List,
    subst: &[Ty],
    depth: u32,
) -> Result<crate::ty::TyList, CheckError> {
    let items = types.list(l)?;
    let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(items.len(), Alloc::global());
    for &p in items {
        v.push(import(cx, src, types,p, subst, depth)?);
    }
    cx.mk_list(&v)
}

/// Exports `t`, fully resolved, into the ground table; `None` when it is not ground.
pub(crate) fn export(cx: &mut InferCtx<'_>, t: Ty, ground: &mut PortableTys<'_>, depth: u32) -> Result<Option<PTy>, CheckError> {
    if depth > MAX_DEPTH {
        return Err(CheckError::Limit(Limit::Export));
    }
    let t = cx.resolve(t)?;
    let d = depth + 1;
    let n = match cx.view(t) {
        TyView::Bool => PNode::Bool,
        TyView::Char => PNode::Char,
        TyView::Int(i) => PNode::Int(i),
        TyView::Uint(u) => PNode::Uint(u),
        TyView::Float(f) => PNode::Float(f),
        TyView::Str => PNode::Str,
        TyView::Never => PNode::Never,
        TyView::Tuple(l) => match export_list(cx, l, ground, d)? {
            Some(l) => PNode::Tuple(l),
            None => return Ok(None),
        },
        TyView::Ref(m, p) => match export(cx, p, ground, d)? {
            Some(p) => PNode::Ref(m, p),
            None => return Ok(None),
        },
        TyView::Ptr(m, p) => match export(cx, p, ground, d)? {
            Some(p) => PNode::Ptr(m, p),
            None => return Ok(None),
        },
        TyView::Array(e, n) => match (export(cx, e, ground, d)?, export(cx, n, ground, d)?) {
            (Some(e), Some(n)) => PNode::Array(e, n),
            _ => return Ok(None),
        },
        TyView::Slice(e) => match export(cx, e, ground, d)? {
            Some(e) => PNode::Slice(e),
            None => return Ok(None),
        },
        TyView::Pat(b, l) => match (export(cx, b, ground, d)?, export_list(cx, l, ground, d)?) {
            (Some(b), Some(l)) => PNode::Pat(b, l),
            _ => return Ok(None),
        },
        TyView::Adt(k, l) => match export_list(cx, l, ground, d)? {
            Some(l) => PNode::Adt(k, l),
            None => return Ok(None),
        },
        TyView::FnDef(k, l) => match export_list(cx, l, ground, d)? {
            Some(l) => PNode::FnDef(k, l),
            None => return Ok(None),
        },
        TyView::FnPtr(l, h) => match export_list(cx, l, ground, d)? {
            Some(l) => PNode::FnPtr(l, h),
            None => return Ok(None),
        },
        TyView::Alias { trait_, name, args } => match export_list(cx, args, ground, d)? {
            Some(args) => PNode::Alias { trait_, name, args },
            None => return Ok(None),
        },
        TyView::Opaque(k, l) => match export_list(cx, l, ground, d)? {
            Some(l) => PNode::Opaque(k, l),
            None => return Ok(None),
        },
        TyView::Error => PNode::Error,
        TyView::ConstValue { ty, bits } => match export(cx, ty, ground, d)? {
            Some(ty) => PNode::ConstValue { ty, bits },
            None => return Ok(None),
        },
        // The view's rows are already in `PortableTys::dynamic`'s order.
        TyView::Dynamic(l) => match export_list(cx, l, ground, d)? {
            Some(l) => PNode::Dynamic(l),
            None => return Ok(None),
        },
        TyView::ExTrait(k, l) => match export_list(cx, l, ground, d)? {
            Some(l) => PNode::ExTrait(k, l),
            None => return Ok(None),
        },
        TyView::ExProj { trait_, name, args_term } => {
            let n = cx.list_len(args_term);
            let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(n as usize, Alloc::global());
            for i in 0..n {
                let t = cx.list_get(args_term, i);
                match export(cx, t, ground, d)? {
                    Some(p) => v.push(p),
                    None => return Ok(None),
                }
            }
            let Some((&term, args)) = v.split_last() else { return Ok(None) };
            PNode::ExProj { trait_, name, args: ground.intern_list(args)?, term }
        }
        TyView::ExAuto(k) => PNode::ExAuto(k),
        TyView::ClosureOf { owner, ordinal, args, .. } => match export_list(cx, args, ground, d)? {
            Some(args) => PNode::Closure { owner, ordinal, args },
            None => return Ok(None),
        },
        TyView::Closure(..)
        | TyView::Future(..)
        | TyView::Param(_)
        | TyView::Infer(_)
        | TyView::ConstParam(_)
        | TyView::ConstInfer(_) => return Ok(None),
    };
    Ok(Some(ground.intern(n)?))
}

fn export_list(
    cx: &mut InferCtx<'_>,
    l: crate::ty::TyList,
    ground: &mut PortableTys<'_>,
    depth: u32,
) -> Result<Option<List>, CheckError> {
    let n = cx.list_len(l);
    let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(n as usize, Alloc::global());
    for i in 0..n {
        let t = cx.list_get(l, i);
        match export(cx, t, ground, depth)? {
            Some(p) => v.push(p),
            None => return Ok(None),
        }
    }
    Ok(Some(ground.intern_list(&v)?))
}

/// The principal trait of a trait object's rows `l` (its first row, if an `ExTrait`).
pub(crate) fn principal(cx: &InferCtx<'_>, l: crate::ty::TyList) -> Option<crate::ids::ItemKey> {
    if cx.list_len(l) == 0 {
        return None;
    }
    match cx.view(cx.list_get(l, 0)) {
        TyView::ExTrait(k, _) => Some(k),
        _ => None,
    }
}

pub(crate) fn shape_of(v: TyView, cx: &InferCtx<'_>) -> Option<Shape> {
    Some(match v {
        TyView::Bool => Shape::Bool,
        TyView::Char => Shape::Char,
        TyView::Int(i) => Shape::Int(i),
        TyView::Uint(u) => Shape::Uint(u),
        TyView::Float(f) => Shape::Float(f),
        TyView::Str => Shape::Str,
        TyView::Never => Shape::Never,
        TyView::Tuple(l) => Shape::Tuple(cx.list_len(l)),
        TyView::Ref(m, _) => Shape::Ref(m),
        TyView::Ptr(m, _) => Shape::Ptr(m),
        TyView::Array(..) => Shape::Array,
        TyView::Slice(_) => Shape::Slice,
        TyView::Pat(..) => Shape::Pat,
        TyView::Adt(k, _) => Shape::Adt(k),
        TyView::FnDef(k, _) => Shape::FnDef(k),
        TyView::FnPtr(l, _) => Shape::FnPtr(cx.list_len(l).saturating_sub(1)),
        TyView::Closure(..) | TyView::ClosureOf { .. } | TyView::Future(..) => Shape::Closure,
        TyView::Opaque(k, _) => Shape::Opaque(k),
        TyView::Dynamic(l) => Shape::Dynamic(principal(cx, l)),
        TyView::Param(_) | TyView::Alias { .. } | TyView::Infer(_) | TyView::Error => return None,
        TyView::ConstValue { .. } | TyView::ConstParam(_) | TyView::ConstInfer(_) => return None,
        TyView::ExTrait(..) | TyView::ExProj { .. } | TyView::ExAuto(_) => return None,
    })
}

/// Instantiates impl header `h` with fresh variables and relates its self type and trait
/// arguments to the goal's `args` (self first); the fresh arguments when all unify. On `None`
/// the unifications made are left for the caller's snapshot to roll back.
pub(crate) fn unify_header(
    cx: &mut InferCtx<'_>,
    h: &ImplHeader<'_>,
    args: crate::ty::TyList,
) -> Result<Option<Vec<Ty, Alloc<'static>>>, CheckError> {
    let rest = h.types.list(h.trait_args.list())?;
    if rest.len() as u32 + 1 != cx.list_len(args) {
        return Ok(None);
    }
    let mut fresh: Vec<Ty, Alloc<'static>> = Vec::with_capacity_in(h.generics as usize, Alloc::global());
    for _ in 0..h.generics {
        fresh.push(cx.fresh(crate::ty::VarKind::Ty)?);
    }
    let self_ty = import(cx, &mut NoClosures, h.types,h.self_ty, &fresh, 0)?;
    if !cx.unify(self_ty, cx.list_get(args, 0))? {
        return Ok(None);
    }
    for (i, &p) in rest.iter().enumerate() {
        let t = import(cx, &mut NoClosures, h.types,p, &fresh, 0)?;
        if !cx.unify(t, cx.list_get(args, i as u32 + 1))? {
            return Ok(None);
        }
    }
    Ok(Some(fresh))
}

/// Whether impl `imp` holds for the goal `args` (in `cx`); its arguments, in `cx`, if so.
fn try_impl<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    imp: ItemKey,
    trait_: ItemKey,
    args: crate::ty::TyList,
) -> Result<Option<crate::ty::TyList>, ResolveError> {
    let Some(h) = reads.impl_header(imp)? else { return Err(ResolveError::Missing(Question::ImplHeader(imp))) };
    if h.trait_ != Some(trait_) || h.polarity != crate::world::ImplPolarity::Positive {
            return Ok(None);
        }
    let s = cx.snapshot();
    let Some(fresh) = unify_header(cx, &h, args)? else {
        cx.rollback_to(s);
        return Ok(None);
    };
    let mut ok = true;
    {
        let wc = reads.predicates(imp)?;
        for c in wc.clauses {
            let goal = match *c {
                PClause::Trait { trait_, args } => Goal::Trait { trait_, args: import_list(cx, &mut NoClosures, wc.types,args, &fresh, 0)? },
                PClause::Projection { trait_, args, name, term } => Goal::Projection {
                    trait_,
                    args: import_list(cx, &mut NoClosures, wc.types,args, &fresh, 0)?,
                    name,
                    term: import(cx, &mut NoClosures, wc.types,term, &fresh, 0)?,
                },
                PClause::Const { cond, args } => Goal::ConstHolds { cond, args: import_list(cx, &mut NoClosures, wc.types,args, &fresh, 0)? },
            };
            cx.register(Obligation { goal, cause: Cause::Body })?;
        }
        cx.select_all(reads)?;
        ok = cx.next_error().is_none();
    }
    if !ok {
        cx.rollback_to(s);
        return Ok(None);
    }
    cx.commit(s);
    Ok(Some(cx.mk_list(&fresh)?))
}

/// The impl of `trait_` that holds at the ground `args` (self first, in `ground`), and the
/// impl's arguments, interned into `ground`; `None` when no impl holds.
pub fn select_impl<W: World>(
    world: &W,
    trait_: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'_>,
) -> Result<Option<(ItemKey, List)>, ResolveError> {
    let mut reads = Reads::new_in(world, alloc);
    let mut cx = InferCtx::new_in(alloc);
    select_in(&mut cx, &mut reads, trait_, args, ground)
}

fn select_in<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    trait_: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
) -> Result<Option<(ItemKey, List)>, ResolveError> {
    if args.is_empty() {
        return Err(ResolveError::NotGround { item: trait_ });
    }
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len(), Alloc::global());
    for &p in args {
        // A closure among the arguments is imported through `Question::Closure`.
        let mut src = Asked { reads: &mut *reads, need: None };
        match import(cx, &mut src, ground.view(), p, &[], 0) {
            Ok(t) => tys.push(t),
            Err(e) => {
                return Err(match src.need {
                    Some(q) => ResolveError::Missing(q),
                    None => ResolveError::Check(e),
                });
            }
        }
    }
    let goal_args = cx.mk_list(&tys)?;
    let self_ty = cx.list_get(goal_args, 0);
    let Some(shape) = shape_of(cx.view(self_ty), cx) else { return Err(ResolveError::NotGround { item: trait_ }) };
    let mut found: Option<(ItemKey, crate::ty::TyList)> = None;
    let buckets = [shape, Shape::Param];
    let mut seen: Vec<ItemKey, Alloc<'_>> = Vec::new_in(Alloc::global());
    for b in buckets {
        for &imp in reads.impls_of(trait_, b)? {
            if seen.contains(&imp) {
                continue;
            }
            seen.push(imp);
            if let Some(impl_args) = try_impl(cx, reads, imp, trait_, goal_args)? {
                if let Some((first, _)) = found {
                    return Err(ResolveError::Overlap { trait_, first, second: imp });
                }
                found = Some((imp, impl_args));
            }
        }
    }
    let Some((imp, impl_args)) = found else { return Ok(None) };
    match export_list(cx, impl_args, ground, 0)? {
        Some(l) => Ok(Some((imp, l))),
        None => Err(ResolveError::NotGround { item: imp }),
    }
}

/// The projection `<args[0] as trait_<args[1..]>>::name` at the ground `args` (self first, in
/// `ground`, no parameter inside), normalized by the solver's own projection step (blaze-solve
/// alias.rs `step_impl`: the selected impl's item, or the trait's default), the answer exported
/// into `ground`. `Ok(None)`: the solver did not normalize it (no impl holds, or the answer
/// still holds a variable). The code builder's question at an instance (blaze-build
/// `LayoutFacts::normalize`): the same selection a body's check makes, in one home.
pub fn normalize_projection<W: World>(
    world: &W,
    trait_: ItemKey,
    name: crate::ids::Name,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'_>,
) -> Result<Option<PTy>, ResolveError> {
    if args.is_empty() {
        return Err(ResolveError::NotGround { item: trait_ });
    }
    let mut reads = Reads::new_in(world, alloc);
    let mut cx = InferCtx::new_in(alloc);
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len(), alloc);
    for &p in args {
        let mut src = Asked { reads: &mut reads, need: None };
        match import(&mut cx, &mut src, ground.view(), p, &[], 0) {
            Ok(t) => tys.push(t),
            Err(e) => {
                return Err(match src.need {
                    Some(q) => ResolveError::Missing(q),
                    None => ResolveError::Check(e),
                });
            }
        }
    }
    let goal_args = cx.mk_list(&tys)?;
    let term = cx.fresh(crate::ty::VarKind::Ty)?;
    cx.register(Obligation { goal: Goal::Projection { trait_, args: goal_args, name, term }, cause: Cause::Body })?;
    cx.select_all(&mut reads)?;
    if cx.next_error().is_some() {
        return Ok(None);
    }
    let t = cx.resolve(term)?;
    Ok(export(&mut cx, t, ground, 0)?)
}

/// The body a call of `item` at the ground `args` runs: for a trait's method, the selected
/// impl's method with the impl's arguments then the method's own, or the trait's provided
/// method (its arguments unchanged) when the impl does not define it; any other item, itself.
pub fn resolve_instance<W: World>(
    world: &W,
    item: ItemKey,
    args: List,
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'_>,
) -> Result<Resolved, ResolveError> {
    let mut reads = Reads::new_in(world, alloc);
    let Some(parent) = reads.assoc_parent(item)? else { return Ok(Resolved { item, args }) };
    let Some(t) = reads.trait_def(parent.container)? else { return Ok(Resolved { item, args }) };
    let trait_ = parent.container;
    let all: Vec<PTy, Alloc<'_>> = {
        let s = ground.view().list(args)?;
        let mut v = Vec::with_capacity_in(s.len(), Alloc::global());
        v.extend_from_slice(s);
        v
    };
    let n = t.generics as usize;
    if all.len() < n || n == 0 {
        return Err(ResolveError::NotGround { item });
    }
    let mut cx = InferCtx::new_in(alloc);
    let Some((imp, impl_args)) = select_in(&mut cx, &mut reads, trait_, &all[..n], ground)? else {
        return Err(ResolveError::NoImpl { trait_, item });
    };
    match reads.assoc_item(imp, parent.name)? {
        Some(a) => {
            let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(all.len(), Alloc::global());
            v.extend_from_slice(ground.view().list(impl_args)?);
            v.extend_from_slice(&all[n..]);
            Ok(Resolved { item: a.item, args: ground.intern_list(&v)? })
        }
        None => Ok(Resolved { item, args }),
    }
}

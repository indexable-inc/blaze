//! Instances at ground arguments, for monomorphization: which impl a trait goal selects
//! ([`select_impl`]) and which body a trait method's call runs ([`resolve_instance`]).
//!
//! Both answer through the same solver the checker uses (an [`InferCtx`]), not a second
//! matcher: each impl of the trait in the self type's bucket (and the blanket bucket) is
//! instantiated with fresh variables, its trait reference related to the goal's, and its
//! where-clauses proved; of the impls that survive, the one that specializes every other is
//! the answer (specialize.rs: the pinned library's `min_specialization`); none such is a typed
//! `Overlap` naming two. Every read goes through `Reads`, the one seam; a question
//! the world cannot answer is [`ResolveError::Missing`], never a guess.
//!
//! Every entry point here asks AFTER type checking, so its context is
//! [`InferCtx::post_analysis_in`] (`TypingMode::PostAnalysis`): an opaque type is its hidden
//! type (rustc resolves instances in its post-analysis typing mode). A body's own selection at
//! ground arguments (writeback's `select_owning`) brings its analysis context instead.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::{ItemKey, List};
use crate::infer::{Exporter, InferCtx};
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
    /// An argument still holds an inference variable or a parameter after selection, or a
    /// where-clause of impl `item` is still ambiguous at the ground arguments (`try_impl`):
    /// undecided either way, never a guessed "no impl".
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
            ResolveError::NotGround { item } => {
                write!(f, "resolving {item:?}: undecided (an argument is not ground, or a where-clause of it is ambiguous at these arguments)")
            }
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
    type Error: From<CheckError>;

    /// `Question::Closure(owner, ordinal)`'s answer imported at `args`: `TyView::ClosureOf`.
    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, Self::Error>;

    /// A named coroutine is read from its defining body's checked facts.
    fn coroutine_of(&mut self, _: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, _: crate::ty::TyList, _: u32) -> Result<Ty, Self::Error> {
        Err(CheckError::Malformed(Malformed::BodyTypeInRecord(Question::Coroutine(owner, ordinal))).into())
    }

    /// A body-local future can only be imported by the source for its defining body.
    #[allow(clippy::too_many_arguments)]
    fn future_of(
        &mut self,
        _cx: &mut InferCtx<'_>,
        _types: PTys<'_>,
        e: crate::ids::ExprId,
        _output: PTy,
        _interior: PTy,
        _subst: &[Ty],
        _depth: u32,
    ) -> Result<Ty, Self::Error> {
        Err(CheckError::Malformed(Malformed::FutureOutsideOwner { block: e, reader: None }).into())
    }
}

/// No closure can occur (an impl header, a where-clause): one is refused as a body type.
pub(crate) struct NoClosures;

impl ClosureSource for NoClosures {
    type Error = CheckError;

    fn closure_of(&mut self, _: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, _: crate::ty::TyList, _: u32) -> Result<Ty, CheckError> {
        Err(CheckError::Malformed(crate::error::Malformed::BodyTypeInRecord(Question::Closure(owner, ordinal))))
    }
}

/// The world, through the read seam. A missing closure propagates as the read's `Stop::Need`.
pub(crate) struct Asked<'r, 'w, 'a, W: World> {
    pub reads: &'r mut Reads<'w, 'a, W>,
}

impl<W: World> ClosureSource for Asked<'_, '_, '_, W> {
    type Error = Stop;

    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, Stop> {
        let Some((facts, env)) = self.reads.closure(owner, ordinal)? else {
            return Err(CheckError::Malformed(Malformed::BodyTypeInRecord(Question::Closure(owner, ordinal))).into());
        };
        let n = cx.list_len(args);
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        let sig = import(cx, self, facts.types, facts.sig, &subst, depth)?;
        let env = env.map(|p| import(cx, self, facts.types, p, &subst, depth)).transpose()?;
        Ok(cx.imported_closure(owner, ordinal, args, sig, facts.kind, env)?)
    }

    fn coroutine_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, Stop> {
        self.import_coroutine(cx, owner, ordinal, args, depth)
    }
}

/// The closures of the body whose check is being written back, by ordinal: closure `i`'s kind
/// is `kinds[i]` and its signature `sigs[i]`, a type of the table the selection imports from
/// (rules/writeback.rs builds `Checked::closures` so: row `i` is ordinal `i`). The world has
/// no record of a body still being checked, so its own closures are never asked of it: a
/// selection at a call of that body (`<Filter<Iter<u8>, {closure#0}> as Iterator>::..`, core's
/// `char_count_general_case`) takes them from here.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OwnClosures<'s> {
    pub owner: ItemKey,
    pub body: crate::body::BodyView<'s>,
    pub params: u32,
    pub consts: &'s [(u32, PTy)],
    pub kinds: &'s [crate::ty::ClosureKind],
    pub sigs: &'s [PTy],
    pub upvars: &'s [Option<PTy>],
}

/// A closure of `own.owner` from `own` (its signature in `types`); any other from the world.
struct OwnFirst<'o, 'p, 'r, 'w, 'a, W: World> {
    own: Option<OwnClosures<'o>>,
    types: PTys<'p>,
    asked: Asked<'r, 'w, 'a, W>,
}

impl<W: World> ClosureSource for OwnFirst<'_, '_, '_, '_, '_, W> {
    type Error = Stop;

    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, Stop> {
        let Some(own) = self.own.filter(|o| o.owner == owner) else {
            return self.asked.closure_of(cx, owner, ordinal, args, depth);
        };
        let i = ordinal as usize;
        let (Some(&kind), Some(&sig), Some(&env)) = (own.kinds.get(i), own.sigs.get(i), own.upvars.get(i)) else {
            return Err(CheckError::Malformed(Malformed::BodyTypeInRecord(Question::Closure(owner, ordinal))).into());
        };
        let n = cx.list_len(args);
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        // A signature may name another closure of the same body (`|| |y| y`): this source.
        let types = self.types;
        let sig = import(cx, self, types, sig, &subst, depth)?;
        let env = env.map(|p| import(cx, self, types, p, &subst, depth)).transpose()?;
        Ok(cx.imported_closure(owner, ordinal, args, sig, kind, env)?)
    }

    fn future_of(
        &mut self, cx: &mut InferCtx<'_>, types: PTys<'_>, e: crate::ids::ExprId,
        output: PTy, interior: PTy, subst: &[Ty], depth: u32,
    ) -> Result<Ty, Stop> {
        let Some(own) = self.own else {
            return Err(CheckError::Malformed(Malformed::FutureOutsideOwner { block: e, reader: None }).into());
        };
        let args = cx.instance_owner_args(own.owner, own.params, own.consts, subst)?;
        cx.import_instance_future(self, &own.body, e, args, types, output, interior, subst, depth)
    }

    fn coroutine_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, Stop> {
        self.asked.coroutine_of(cx, owner, ordinal, args, depth)
    }
}

/// Imports portable type `t` of `types` into `cx`, `Param(i)` becoming `subst[i]`.
pub(crate) fn import<S: ClosureSource>(cx: &mut InferCtx<'_>, src: &mut S, types: PTys<'_>, t: PTy, subst: &[Ty], depth: u32) -> Result<Ty, S::Error> {
    if depth > MAX_DEPTH {
        return Err(CheckError::Limit(Limit::InstanceImportDepth { node: t, depth, limit: MAX_DEPTH }).into());
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
            let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len as usize + 1, cx.alloc());
            for &a in types.list(args)? {
                v.push(import(cx, src, types,a, subst, d)?);
            }
            v.push(import(cx, src, types,term, subst, d)?);
            TyView::ExProj { trait_, name, args_term: cx.mk_list(&v)? }
        }
        PNode::ExAuto(k) => TyView::ExAuto(k),
        PNode::Foreign(k) => TyView::Foreign(k),
        PNode::Closure { owner, ordinal, args } => {
            let args = import_list(cx, src, types, args, subst, d)?;
            return src.closure_of(cx, owner, ordinal, args, d);
        }
        PNode::Future(e, output, interior) => return src.future_of(cx, types, e, output, interior, subst, d),
        PNode::Infer => return Err(CheckError::Malformed(Malformed::InstanceImportInfer { node: t }).into()),
        PNode::Coroutine { owner, ordinal, args } => {
            let args = import_list(cx, src, types, args, subst, d)?;
            return src.coroutine_of(cx, owner, ordinal, args, d);
        }
    };
    cx.mk(v).map_err(Into::into)
}

pub(crate) fn import_list<S: ClosureSource>(
    cx: &mut InferCtx<'_>,
    src: &mut S,
    types: PTys<'_>,
    l: List,
    subst: &[Ty],
    depth: u32,
) -> Result<crate::ty::TyList, S::Error> {
    let items = types.list(l)?;
    let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(items.len(), cx.alloc());
    for &p in items {
        v.push(import(cx, src, types,p, subst, depth)?);
    }
    cx.mk_list(&v).map_err(Into::into)
}

/// Which types [`export_as`] writes into its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    /// Ground types only (an instance's arguments): a parameter, a variable or this body's
    /// closure or async block's future is no answer (`None`).
    Ground,
    /// Also the owner's parameters (`Param`, `ConstParam`) and this body's async blocks'
    /// futures (`Future`, its interior as the context holds it): a type said in the owner's
    /// own terms, as its record says it (stalled.rs, a why-chain's goals). A variable or this
    /// body's closure is still `None`.
    Owner,
}

/// Exports `t`, fully resolved, into the ground table; `None` when it is not ground. Over an
/// [`Exporter`] (`InferCtx::exporter`, or the solver's pull mid-step: infer `fill_const_holds`),
/// the parts of the context an export reads, so it can run while the solver holds the rest.
pub(crate) fn export(cx: &mut Exporter<'_, '_>, t: Ty, ground: &mut PortableTys<'_>, depth: u32) -> Result<Option<PTy>, CheckError> {
    export_as(cx, t, ground, Keep::Ground, depth)
}

/// [`export`] keeping the types `keep` names.
pub(crate) fn export_as(cx: &mut Exporter<'_, '_>, t: Ty, ground: &mut PortableTys<'_>, keep: Keep, depth: u32) -> Result<Option<PTy>, CheckError> {
    if depth > MAX_DEPTH {
        return Err(CheckError::Limit(Limit::InstanceExportDepth { node: t, depth, limit: MAX_DEPTH }));
    }
    let t = cx.resolve(t)?;
    // A closure or async block the solver made revealing an opaque is filed first.
    cx.adopt(t)?;
    let d = depth + 1;
    // The same mode all the way down.
    let export = |cx: &mut Exporter<'_, '_>, t: Ty, ground: &mut PortableTys<'_>, d: u32| export_as(cx, t, ground, keep, d);
    let export_list = |cx: &mut Exporter<'_, '_>, l: crate::ty::TyList, ground: &mut PortableTys<'_>, d: u32| export_list_as(cx, l, ground, keep, d);
    let n = match cx.view(t) {
        TyView::Param(i) if keep == Keep::Owner => PNode::Param(i),
        TyView::ConstParam(i) if keep == Keep::Owner => PNode::ConstParam(i),
        TyView::Future(e, o, i) if keep == Keep::Owner => match (export(cx, o, ground, d)?, export(cx, i, ground, d)?) {
            (Some(o), Some(i)) => PNode::Future(e, o, i),
            _ => return Ok(None),
        },
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
            let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
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
        TyView::Foreign(k) => PNode::Foreign(k),
        TyView::ClosureOf { owner, ordinal, args, .. } => match export_list(cx, args, ground, d)? {
            Some(args) => PNode::Closure { owner, ordinal, args },
            None => return Ok(None),
        },
        TyView::CoroutineOf { owner, ordinal, args, .. } => match export_list(cx, args, ground, d)? {
            Some(args) => PNode::Coroutine { owner, ordinal, args },
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
    cx: &mut Exporter<'_, '_>,
    l: crate::ty::TyList,
    ground: &mut PortableTys<'_>,
    depth: u32,
) -> Result<Option<List>, CheckError> {
    export_list_as(cx, l, ground, Keep::Ground, depth)
}

/// [`export_list`] keeping the types `keep` names ([`export_as`]).
pub(crate) fn export_list_as(
    cx: &mut Exporter<'_, '_>,
    l: crate::ty::TyList,
    ground: &mut PortableTys<'_>,
    keep: Keep,
    depth: u32,
) -> Result<Option<List>, CheckError> {
    let n = cx.list_len(l);
    let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
    for i in 0..n {
        let t = cx.list_get(l, i);
        match export_as(cx, t, ground, keep, depth)? {
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
        TyView::Closure(..) | TyView::ClosureOf { .. } | TyView::Future(..) | TyView::CoroutineOf { .. } => Shape::Closure,
        TyView::Opaque(k, _) => Shape::Opaque(k),
        TyView::Foreign(k) => Shape::Foreign(k),
        TyView::Dynamic(l) => Shape::Dynamic(principal(cx, l)),
        TyView::Param(_) | TyView::Alias { .. } | TyView::Infer(_) | TyView::Error => return None,
        TyView::ConstValue { .. } | TyView::ConstParam(_) | TyView::ConstInfer(_) => return None,
        TyView::ExTrait(..) | TyView::ExProj { .. } | TyView::ExAuto(_) => return None,
    })
}

/// Instantiates impl header `h` with fresh variables and relates its self type and trait
/// arguments to the goal's `args` (self first); the fresh arguments when all unify. On `None`
/// the unifications made are left for the caller's snapshot to roll back.
pub(crate) fn unify_header<'a>(
    cx: &mut InferCtx<'a>,
    h: &ImplHeader<'_>,
    args: crate::ty::TyList,
) -> Result<Option<Vec<Ty, Alloc<'a>>>, CheckError> {
    let rest = h.types.list(h.trait_args.list())?;
    if rest.len() as u32 + 1 != cx.list_len(args) {
        return Ok(None);
    }
    let alloc = cx.alloc();
    let mut fresh: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(h.generics as usize, alloc);
    // A const parameter's argument is a const (the Reference, "Generic parameters"): each
    // generic gets a variable of the kind its header uses it as (walk AJ P1).
    let kinds = crate::impl_kinds::impl_var_kinds(h, alloc)?;
    for &k in kinds.iter() {
        fresh.push(cx.fresh(k)?);
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
                // Outlives clauses are the borrow checker's (D148); the checker takes them as true.
                PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => continue,
            };
            cx.register(Obligation { goal, cause: Cause::Body })?;
        }
        cx.select_all(reads)?;
        // A where-clause still AMBIGUOUS at the ground arguments (an overflow: a cycle the
        // solver could not decide) decides nothing: the impl neither holds nor fails, so the
        // answer is `NotGround` naming the impl (rule 3), never "no impl holds"; one that FAILED
        // is a decided no, whatever else is undecided.
        let mut undecided = false;
        while let Some(e) = cx.next_error() {
            if e.kind == crate::ty::FulfillErrorKind::Ambiguous {
                undecided = true;
            } else {
                ok = false;
            }
        }
        if ok && undecided {
            cx.rollback_to(s);
            return Err(ResolveError::NotGround { item: imp });
        }
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
    let mut cx = InferCtx::post_analysis_in(alloc);
    let out = select_in(&mut cx, &mut reads, trait_, args, ground);
    unholed(&reads, out)
}

/// `out`, the answer of a one-shot question asked through `reads` (this module's entry points),
/// unless the run met a hole. A hole's stand-in (no impls in a bucket, no header) decided
/// nothing: `IsWhitespace: MultiCharEq` read through an unmade bucket of `MultiCharEq` at
/// `IsWhitespace` was "no impl", so a projection over it was a mismatch. So the answer is
/// [`ResolveError::Missing`] naming the first hole, as a body's run with a hole makes no result
/// (lib.rs `check`, world.rs `Hole`); the module's rule, never a guess.
fn unholed<W: World, T>(reads: &Reads<'_, '_, W>, out: Result<T, ResolveError>) -> Result<T, ResolveError> {
    match reads.first_hole() {
        Some(q) => Err(ResolveError::Missing(q)),
        None => out,
    }
}

pub(crate) fn select_in<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    trait_: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
) -> Result<Option<(ItemKey, List)>, ResolveError> {
    select_owning(cx, reads, None, trait_, args, ground)
}

/// [`select_in`] at a call of a body whose check is being written back: a closure of that body
/// among the arguments takes its facts from `own` (its signature a type of `ground`).
pub(crate) fn select_owning<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    own: Option<OwnClosures<'_>>,
    trait_: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
) -> Result<Option<(ItemKey, List)>, ResolveError> {
    if args.is_empty() {
        return Err(ResolveError::NotGround { item: trait_ });
    }
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len(), cx.alloc());
    for &p in args {
        // A closure of another body among the arguments is imported through `Question::Closure`.
        let types = ground.view();
        let mut src = OwnFirst { own, types, asked: Asked { reads: &mut *reads } };
        tys.push(import(cx, &mut src, types, p, &[], 0)?);
    }
    let goal_args = cx.mk_list(&tys)?;
    let self_ty = cx.list_get(goal_args, 0);
    let Some(shape) = shape_of(cx.view(self_ty), cx) else { return Err(ResolveError::NotGround { item: trait_ }) };
    // Every holding impl, in selection order, with its arguments in `cx` (walk AJ P9: the
    // pinned alloc's `min_specialization` pairs, `impl<T: Display + ?Sized> ToString for T`
    // beside `impl ToString for str`, both hold at `str`).
    let mut holding: Vec<ItemKey, Alloc<'_>> = Vec::new_in(cx.alloc());
    let mut holding_args: Vec<crate::ty::TyList, Alloc<'_>> = Vec::new_in(cx.alloc());
    let buckets = [shape, Shape::Param];
    let mut seen: Vec<ItemKey, Alloc<'_>> = Vec::new_in(cx.alloc());
    for b in buckets {
        for &imp in reads.impls_of(trait_, b)? {
            if seen.contains(&imp) {
                continue;
            }
            seen.push(imp);
            if let Some(impl_args) = try_impl(cx, reads, imp, trait_, goal_args)? {
                holding.push(imp);
                holding_args.push(impl_args);
            }
        }
    }
    if holding.is_empty() {
        return Ok(None);
    }
    let i = crate::specialize::most_specific(cx, reads, trait_, &holding)?;
    let (imp, impl_args) = (holding[i], holding_args[i]);
    match export_list(&mut cx.exporter(), impl_args, ground, 0)? {
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
    let mut reads = Reads::new_in(world, alloc);
    let out = normalize_in(&mut reads, trait_, name, args, ground, alloc);
    unholed(&reads, out)
}

fn normalize_in<W: World>(
    reads: &mut Reads<'_, '_, W>,
    trait_: ItemKey,
    name: crate::ids::Name,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'_>,
) -> Result<Option<PTy>, ResolveError> {
    if args.is_empty() {
        return Err(ResolveError::NotGround { item: trait_ });
    }
    let mut cx = InferCtx::post_analysis_in(alloc);
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len(), alloc);
    for &p in args {
        let mut src = Asked { reads: &mut *reads };
        tys.push(import(&mut cx, &mut src, ground.view(), p, &[], 0)?);
    }
    let goal_args = cx.mk_list(&tys)?;
    let term = cx.fresh(crate::ty::VarKind::Ty)?;
    cx.register(Obligation { goal: Goal::Projection { trait_, args: goal_args, name, term }, cause: Cause::Body })?;
    cx.select_all(reads)?;
    if cx.next_error().is_some() {
        return Ok(None);
    }
    let t = cx.resolve(term)?;
    Ok(export(&mut cx.exporter(), t, ground, 0)?)
}

/// Whether the trait goal `args[0]: trait_<args[1..]>` holds at the ground `args` (self first,
/// in `ground`): the solver's own answer, the one a body's check gets, so the impls the
/// LANGUAGE gives count as the written ones do (the Reference, "Special types and traits":
/// `Copy` and `Clone` of a tuple, a closure, a fn item or a fn pointer; the auto traits;
/// `Sized`), where [`select_impl`] finds only written impls. The code builder's question at an
/// instance (blaze-ladder `FrontEnd::holds`: a `Copy` tuple's builtin `clone` copies its bytes).
/// A goal still ambiguous at ground arguments is no answer: `NotGround`, never a guessed `false`.
pub fn holds<W: World>(world: &W, trait_: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<bool, ResolveError> {
    let mut reads = Reads::new_in(world, alloc);
    let out = holds_in(&mut reads, trait_, args, ground, alloc);
    unholed(&reads, out)
}

fn holds_in<W: World>(reads: &mut Reads<'_, '_, W>, trait_: ItemKey, args: &[PTy], ground: &PortableTys<'_>, alloc: Alloc<'_>) -> Result<bool, ResolveError> {
    if args.is_empty() {
        return Err(ResolveError::NotGround { item: trait_ });
    }
    let mut cx = InferCtx::post_analysis_in(alloc);
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len(), alloc);
    for &p in args {
        let mut src = Asked { reads };
        tys.push(import(&mut cx, &mut src, ground.view(), p, &[], 0)?);
    }
    let goal_args = cx.mk_list(&tys)?;
    cx.register(Obligation { goal: Goal::Trait { trait_, args: goal_args }, cause: Cause::Body })?;
    cx.select_all(reads)?;
    match cx.next_error() {
        None => Ok(true),
        Some(e) if e.kind == crate::ty::FulfillErrorKind::Ambiguous => Err(ResolveError::NotGround { item: trait_ }),
        Some(_) => Ok(false),
    }
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
    let out = resolve_in(&mut reads, item, args, ground, alloc);
    unholed(&reads, out)
}

fn resolve_in<W: World>(
    reads: &mut Reads<'_, '_, W>,
    item: ItemKey,
    args: List,
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'_>,
) -> Result<Resolved, ResolveError> {
    let Some(parent) = reads.assoc_parent(item)? else { return Ok(Resolved { item, args }) };
    let Some(t) = reads.trait_def(parent.container)? else { return Ok(Resolved { item, args }) };
    let trait_ = parent.container;
    let all: Vec<PTy, Alloc<'_>> = {
        let s = ground.view().list(args)?;
        let mut v = Vec::with_capacity_in(s.len(), alloc);
        v.extend_from_slice(s);
        v
    };
    let n = t.generics as usize;
    if all.len() < n || n == 0 {
        return Err(ResolveError::NotGround { item });
    }
    let mut cx = InferCtx::post_analysis_in(alloc);
    let Some((imp, impl_args)) = select_in(&mut cx, reads, trait_, &all[..n], ground)? else {
        return Err(ResolveError::NoImpl { trait_, item });
    };
    match reads.assoc_item(imp, parent.name)? {
        Some(a) => {
            let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(all.len(), alloc);
            v.extend_from_slice(ground.view().list(impl_args)?);
            v.extend_from_slice(&all[n..]);
            Ok(Resolved { item: a.item, args: ground.intern_list(&v)? })
        }
        None => Ok(Resolved { item, args }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block in `fn make<const N: u8>() { let _ = async {}; }` has unit output
    /// and empty state at every N, but its type belongs to the instantiation of `make`.
    #[test]
    fn coroutine_instances_keep_const_arguments_absent_from_their_fields() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut world = crate::fixture::Fx::new();
        let unit = world.unit();
        let byte = world.u8();
        let owner = world.func("make", 1, &[], unit, std::vec![]);
        world.const_param(owner, 0, byte);
        world.coroutines.push((owner, 0, unit, unit));
        let roots = [1, 2].map(|bits| {
            let value = world.ty(PNode::ConstValue { ty: byte, bits });
            let args = world.list(&[value]);
            world.ty(PNode::Coroutine { owner, ordinal: 0, args })
        });
        world.finish();
        let mut reads = Reads::new_in(&world, alloc);
        let mut source = Asked { reads: &mut reads };
        let mut cx = InferCtx::post_analysis_in(alloc);
        let instances = roots.map(|root| import(&mut cx, &mut source, world.t.view(), root, &[], 0).expect("the coroutine's checked facts"));
        assert!(!cx.unify(instances[0], instances[1]).expect("relate the instances"), "make::<1> and make::<2> have distinct futures");

        let mut ground = world.t.fork_in(alloc).expect("the source table");
        for (instance, root) in instances.into_iter().zip(roots) {
            assert_eq!(export(&mut cx.exporter(), instance, &mut ground, 0), Ok(Some(root)), "write back this instance's own const argument");
        }
    }

    /// `let x: (_,) = (true,);`: at an instance, the element must already be `bool`.
    /// Plant the original hole in its place; neither reading that hole nor crossing the
    /// import depth boundary may masquerade as failure to export the enclosing tuple.
    #[test]
    fn instance_import_distinguishes_an_unresolved_child_from_depth() {
        let alloc = Alloc::global();
        let mut table = PortableTys::new_in(alloc);
        let boolean = table.intern(PNode::Bool).expect("bool");
        let elements = table.intern_list(&[boolean]).expect("elements");
        let tuple = table.intern(PNode::Tuple(elements)).expect("(bool,)");
        let hole = table.intern(PNode::Infer).expect("inference hole");
        let elements = table.intern_list(&[hole]).expect("elements");
        let unfilled = table.intern(PNode::Tuple(elements)).expect("(_,)");
        let mut cx = InferCtx::post_analysis_in(alloc);

        let value = import(&mut cx, &mut NoClosures, table.view(), tuple, &[], MAX_DEPTH - 1).expect("the child is at the boundary");
        assert_eq!(export(&mut cx.exporter(), value, &mut table, 0), Ok(Some(tuple)));
        assert_eq!(
            import(&mut cx, &mut NoClosures, table.view(), unfilled, &[], 0),
            Err(CheckError::Malformed(Malformed::InstanceImportInfer { node: hole })),
        );
        assert_eq!(
            import(&mut cx, &mut NoClosures, table.view(), tuple, &[], MAX_DEPTH),
            Err(CheckError::Limit(Limit::InstanceImportDepth { node: boolean, depth: MAX_DEPTH + 1, limit: MAX_DEPTH })),
        );
        let TyView::Tuple(children) = cx.view(value) else { panic!("a tuple was imported") };
        let child = cx.list_get(children, 0);
        assert_eq!(
            export(&mut cx.exporter(), value, &mut table, MAX_DEPTH),
            Err(CheckError::Limit(Limit::InstanceExportDepth { node: child, depth: MAX_DEPTH + 1, limit: MAX_DEPTH })),
        );
    }
}

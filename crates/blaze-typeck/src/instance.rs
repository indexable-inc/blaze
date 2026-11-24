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
    enum Kind {
        Tuple,
        Record(crate::ids::RecordSchema),
        Union,
        Ref(crate::ids::Mutbl),
        Ptr(crate::ids::Mutbl),
        Array,
        Slice,
        Pat,
        Adt(ItemKey),
        FnDef(ItemKey),
        FnPtr(crate::ids::FnPtrHeader),
        Alias { trait_: ItemKey, name: crate::ids::Name },
        Opaque(ItemKey),
        ConstValue { bits: u128 },
        Dynamic,
        ExTrait(ItemKey),
        ExProj { trait_: ItemKey, name: crate::ids::Name },
        Closure { owner: ItemKey, ordinal: u32 },
        Coroutine { owner: ItemKey, ordinal: u32 },
    }
    struct Frame {
        kind: Kind,
        depth: u32,
        val_base: usize,
        list: Option<(List, usize)>,
        extra: Option<PTy>,
    }
    fn finish<S: ClosureSource>(cx: &mut InferCtx<'_>, src: &mut S, kind: Kind, slice: &[Ty], depth: u32) -> Result<Ty, S::Error> {
        let v = match kind {
            Kind::Tuple => TyView::Tuple(cx.mk_list(slice)?),
            Kind::Record(s) => TyView::Record(s, cx.mk_list(slice)?),
            Kind::Union => TyView::Union(cx.mk_list(slice)?),
            Kind::Ref(m) => {
                let Some(&p) = slice.first() else { return Ok(cx.mk(TyView::Error)?) };
                TyView::Ref(m, p)
            }
            Kind::Ptr(m) => {
                let Some(&p) = slice.first() else { return Ok(cx.mk(TyView::Error)?) };
                TyView::Ptr(m, p)
            }
            Kind::Array => {
                let [e, n] = *slice else { return Ok(cx.mk(TyView::Error)?) };
                TyView::Array(e, n)
            }
            Kind::Slice => {
                let Some(&e) = slice.first() else { return Ok(cx.mk(TyView::Error)?) };
                TyView::Slice(e)
            }
            Kind::Pat => {
                let Some((&b, rest)) = slice.split_first() else { return Ok(cx.mk(TyView::Error)?) };
                TyView::Pat(b, cx.mk_list(rest)?)
            }
            Kind::Adt(k) => TyView::Adt(k, cx.mk_list(slice)?),
            Kind::FnDef(k) => TyView::FnDef(k, cx.mk_list(slice)?),
            Kind::FnPtr(h) => TyView::FnPtr(cx.mk_list(slice)?, h),
            Kind::Alias { trait_, name } => TyView::Alias { trait_, name, args: cx.mk_list(slice)? },
            Kind::Opaque(k) => TyView::Opaque(k, cx.mk_list(slice)?),
            Kind::ConstValue { bits } => {
                let Some(&ty) = slice.first() else { return Ok(cx.mk(TyView::Error)?) };
                TyView::ConstValue { ty, bits }
            }
            // A trait object's rows keep their order: substitution changes no sort key.
            Kind::Dynamic => TyView::Dynamic(cx.mk_list(slice)?),
            Kind::ExTrait(k) => TyView::ExTrait(k, cx.mk_list(slice)?),
            Kind::ExProj { trait_, name } => TyView::ExProj { trait_, name, args_term: cx.mk_list(slice)? },
            Kind::Closure { owner, ordinal } => {
                let args = cx.mk_list(slice)?;
                return src.closure_of(cx, owner, ordinal, args, depth);
            }
            Kind::Coroutine { owner, ordinal } => {
                let args = cx.mk_list(slice)?;
                return src.coroutine_of(cx, owner, ordinal, args, depth);
            }
        };
        cx.mk(v).map_err(Into::into)
    }

    let mut frames: Vec<Frame, Alloc<'_>> = Vec::new_in(cx.alloc());
    let mut values: Vec<Ty, Alloc<'_>> = Vec::new_in(cx.alloc());
    let mut cur_t = t;
    let mut cur_d = depth;
    loop {
        if cur_d > MAX_DEPTH {
            return Err(CheckError::Limit(Limit::InstanceImportDepth { node: cur_t, depth: cur_d, limit: MAX_DEPTH }).into());
        }
        let d = cur_d + 1;
        let val = match types.node(cur_t)? {
            PNode::Bool => cx.mk(TyView::Bool)?,
            PNode::Char => cx.mk(TyView::Char)?,
            PNode::Int(i) => cx.mk(TyView::Int(i))?,
            PNode::Uint(u) => cx.mk(TyView::Uint(u))?,
            PNode::Float(f) => cx.mk(TyView::Float(f))?,
            PNode::Str => cx.mk(TyView::Str)?,
            PNode::Never => cx.mk(TyView::Never)?,
            PNode::Error => cx.mk(TyView::Error)?,
            PNode::ExAuto(k) => cx.mk(TyView::ExAuto(k))?,
            PNode::Foreign(k) => cx.mk(TyView::Foreign(k))?,
            PNode::Param(i) => match subst.get(i as usize) {
                Some(&s) => s,
                None => cx.mk(TyView::Param(i))?,
            },
            PNode::ConstParam(i) => match subst.get(i as usize) {
                Some(&s) => s,
                None => cx.mk(TyView::ConstParam(i))?,
            },
            PNode::Future(e, output, interior) => src.future_of(cx, types, e, output, interior, subst, d)?,
            PNode::Infer => return Err(CheckError::Malformed(Malformed::InstanceImportInfer { node: cur_t }).into()),
            PNode::Ref(m, p) => {
                frames.push(Frame { kind: Kind::Ref(m), depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = p;
                cur_d = d;
                continue;
            }
            PNode::Ptr(m, p) => {
                frames.push(Frame { kind: Kind::Ptr(m), depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = p;
                cur_d = d;
                continue;
            }
            PNode::Array(e, n) => {
                frames.push(Frame { kind: Kind::Array, depth: d, val_base: values.len(), list: None, extra: Some(n) });
                cur_t = e;
                cur_d = d;
                continue;
            }
            PNode::Slice(e) => {
                frames.push(Frame { kind: Kind::Slice, depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = e;
                cur_d = d;
                continue;
            }
            PNode::Pat(b, l) => {
                frames.push(Frame { kind: Kind::Pat, depth: d, val_base: values.len(), list: Some((l, 0)), extra: None });
                cur_t = b;
                cur_d = d;
                continue;
            }
            PNode::ConstValue { ty, bits } => {
                frames.push(Frame { kind: Kind::ConstValue { bits }, depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = ty;
                cur_d = d;
                continue;
            }
            PNode::ExProj { trait_, name, args, term } => {
                let items = types.list(args)?;
                if let Some(&first) = items.first() {
                    values.reserve(args.len as usize + 1);
                    frames.push(Frame {
                        kind: Kind::ExProj { trait_, name },
                        depth: d,
                        val_base: values.len(),
                        list: Some((args, 1)),
                        extra: Some(term),
                    });
                    cur_t = first;
                } else {
                    frames.push(Frame {
                        kind: Kind::ExProj { trait_, name },
                        depth: d,
                        val_base: values.len(),
                        list: None,
                        extra: None,
                    });
                    cur_t = term;
                }
                cur_d = d;
                continue;
            }
            node => {
                let (kind, l) = match node {
                    PNode::Tuple(l) => (Kind::Tuple, l),
                    PNode::Record(s, l) => (Kind::Record(s), l),
                    PNode::Union(l) => (Kind::Union, l),
                    PNode::Adt(k, l) => (Kind::Adt(k), l),
                    PNode::FnDef(k, l) => (Kind::FnDef(k), l),
                    PNode::FnPtr(l, h) => (Kind::FnPtr(h), l),
                    PNode::Alias { trait_, name, args } => (Kind::Alias { trait_, name }, args),
                    PNode::Opaque(k, l) => (Kind::Opaque(k), l),
                    PNode::Dynamic(l) => (Kind::Dynamic, l),
                    PNode::ExTrait(k, l) => (Kind::ExTrait(k), l),
                    PNode::Closure { owner, ordinal, args } => (Kind::Closure { owner, ordinal }, args),
                    PNode::Coroutine { owner, ordinal, args } => (Kind::Coroutine { owner, ordinal }, args),
                    _ => return Ok(cx.mk(TyView::Error)?),
                };
                let items = types.list(l)?;
                if let Some(&first) = items.first() {
                    frames.push(Frame { kind, depth: d, val_base: values.len(), list: Some((l, 1)), extra: None });
                    cur_t = first;
                    cur_d = d;
                    continue;
                }
                finish(cx, src, kind, &[], d)?
            }
        };
        let mut val = val;
        loop {
            let Some(top) = frames.last_mut() else {
                return Ok(val);
            };
            values.push(val);
            if let Some((l, idx)) = top.list {
                let items = types.list(l)?;
                if let Some(&next_t) = items.get(idx) {
                    top.list = Some((l, idx + 1));
                    cur_t = next_t;
                    cur_d = top.depth;
                    break;
                }
                top.list = None;
            }
            if let Some(extra_t) = top.extra.take() {
                cur_t = extra_t;
                cur_d = top.depth;
                break;
            }
            let Some(frame) = frames.pop() else {
                return Ok(val);
            };
            let slice = values.get(frame.val_base..).unwrap_or(&[]);
            val = finish(cx, src, frame.kind, slice, frame.depth)?;
            values.truncate(frame.val_base);
        }
    }
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
    enum Kind {
        Future(crate::ids::ExprId),
        Tuple,
        Record(crate::ids::RecordSchema),
        Union,
        Ref(crate::ids::Mutbl),
        Ptr(crate::ids::Mutbl),
        Array,
        Slice,
        Pat,
        Adt(ItemKey),
        FnDef(ItemKey),
        FnPtr(crate::ids::FnPtrHeader),
        Alias { trait_: ItemKey, name: crate::ids::Name },
        Opaque(ItemKey),
        ConstValue { bits: u128 },
        Dynamic,
        ExTrait(ItemKey),
        ExProj { trait_: ItemKey, name: crate::ids::Name },
        Closure { owner: ItemKey, ordinal: u32 },
        Coroutine { owner: ItemKey, ordinal: u32 },
    }
    struct Frame {
        kind: Kind,
        depth: u32,
        val_base: usize,
        list: Option<(crate::ty::TyList, u32, u32)>,
        extra: Option<Ty>,
    }
    fn finish(ground: &mut PortableTys<'_>, kind: Kind, slice: &[PTy]) -> Result<Option<PTy>, CheckError> {
        let n = match kind {
            Kind::Future(e) => {
                let [o, i] = *slice else { return Ok(None) };
                PNode::Future(e, o, i)
            }
            Kind::Tuple => PNode::Tuple(ground.intern_list(slice)?),
            Kind::Record(s) => PNode::Record(s, ground.intern_list(slice)?),
            Kind::Union => PNode::Union(ground.intern_list(slice)?),
            Kind::Ref(m) => {
                let Some(&p) = slice.first() else { return Ok(None) };
                PNode::Ref(m, p)
            }
            Kind::Ptr(m) => {
                let Some(&p) = slice.first() else { return Ok(None) };
                PNode::Ptr(m, p)
            }
            Kind::Array => {
                let [e, n] = *slice else { return Ok(None) };
                PNode::Array(e, n)
            }
            Kind::Slice => {
                let Some(&e) = slice.first() else { return Ok(None) };
                PNode::Slice(e)
            }
            Kind::Pat => {
                let Some((&b, rest)) = slice.split_first() else { return Ok(None) };
                PNode::Pat(b, ground.intern_list(rest)?)
            }
            Kind::Adt(k) => PNode::Adt(k, ground.intern_list(slice)?),
            Kind::FnDef(k) => PNode::FnDef(k, ground.intern_list(slice)?),
            Kind::FnPtr(h) => PNode::FnPtr(ground.intern_list(slice)?, h),
            Kind::Alias { trait_, name } => PNode::Alias { trait_, name, args: ground.intern_list(slice)? },
            Kind::Opaque(k) => PNode::Opaque(k, ground.intern_list(slice)?),
            Kind::ConstValue { bits } => {
                let Some(&ty) = slice.first() else { return Ok(None) };
                PNode::ConstValue { ty, bits }
            }
            // The view's rows are already in `PortableTys::dynamic`'s order.
            Kind::Dynamic => PNode::Dynamic(ground.intern_list(slice)?),
            Kind::ExTrait(k) => PNode::ExTrait(k, ground.intern_list(slice)?),
            Kind::ExProj { trait_, name } => {
                let Some((&term, args)) = slice.split_last() else { return Ok(None) };
                PNode::ExProj { trait_, name, args: ground.intern_list(args)?, term }
            }
            Kind::Closure { owner, ordinal } => PNode::Closure { owner, ordinal, args: ground.intern_list(slice)? },
            Kind::Coroutine { owner, ordinal } => PNode::Coroutine { owner, ordinal, args: ground.intern_list(slice)? },
        };
        Ok(Some(ground.intern(n)?))
    }

    let mut frames: Vec<Frame, Alloc<'_>> = Vec::new_in(cx.alloc());
    let mut values: Vec<PTy, Alloc<'_>> = Vec::new_in(cx.alloc());
    let mut cur_t = t;
    let mut cur_d = depth;
    loop {
        if cur_d > MAX_DEPTH {
            return Err(CheckError::Limit(Limit::InstanceExportDepth { node: cur_t, depth: cur_d, limit: MAX_DEPTH }));
        }
        let resolved = cx.resolve(cur_t)?;
        // A closure or async block the solver made revealing an opaque is filed first.
        cx.adopt(resolved)?;
        let d = cur_d + 1;
        let val = match cx.view(resolved) {
            TyView::Param(i) if keep == Keep::Owner => ground.intern(PNode::Param(i))?,
            TyView::ConstParam(i) if keep == Keep::Owner => ground.intern(PNode::ConstParam(i))?,
            TyView::Future(e, o, i) if keep == Keep::Owner => {
                frames.push(Frame { kind: Kind::Future(e), depth: d, val_base: values.len(), list: None, extra: Some(i) });
                cur_t = o;
                cur_d = d;
                continue;
            }
            TyView::Bool => ground.intern(PNode::Bool)?,
            TyView::Char => ground.intern(PNode::Char)?,
            TyView::Int(i) => ground.intern(PNode::Int(i))?,
            TyView::Uint(u) => ground.intern(PNode::Uint(u))?,
            TyView::Float(f) => ground.intern(PNode::Float(f))?,
            TyView::Str => ground.intern(PNode::Str)?,
            TyView::Never => ground.intern(PNode::Never)?,
            TyView::Error => ground.intern(PNode::Error)?,
            TyView::ExAuto(k) => ground.intern(PNode::ExAuto(k))?,
            TyView::Foreign(k) => ground.intern(PNode::Foreign(k))?,
            TyView::Ref(m, p) => {
                frames.push(Frame { kind: Kind::Ref(m), depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = p;
                cur_d = d;
                continue;
            }
            TyView::Ptr(m, p) => {
                frames.push(Frame { kind: Kind::Ptr(m), depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = p;
                cur_d = d;
                continue;
            }
            TyView::Array(e, n) => {
                frames.push(Frame { kind: Kind::Array, depth: d, val_base: values.len(), list: None, extra: Some(n) });
                cur_t = e;
                cur_d = d;
                continue;
            }
            TyView::Slice(e) => {
                frames.push(Frame { kind: Kind::Slice, depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = e;
                cur_d = d;
                continue;
            }
            TyView::Pat(b, l) => {
                let n = cx.list_len(l);
                frames.push(Frame { kind: Kind::Pat, depth: d, val_base: values.len(), list: Some((l, 0, n)), extra: None });
                cur_t = b;
                cur_d = d;
                continue;
            }
            TyView::ConstValue { ty, bits } => {
                frames.push(Frame { kind: Kind::ConstValue { bits }, depth: d, val_base: values.len(), list: None, extra: None });
                cur_t = ty;
                cur_d = d;
                continue;
            }
            TyView::Closure(..)
            | TyView::Future(..)
            | TyView::Param(_)
            | TyView::Infer(_)
            | TyView::ConstParam(_)
            | TyView::ConstInfer(_) => return Ok(None),
            view => {
                let (kind, l) = match view {
                    TyView::Tuple(l) => (Kind::Tuple, l),
                    TyView::Record(s, l) => (Kind::Record(s), l),
                    TyView::Union(l) => (Kind::Union, l),
                    TyView::Adt(k, l) => (Kind::Adt(k), l),
                    TyView::FnDef(k, l) => (Kind::FnDef(k), l),
                    TyView::FnPtr(l, h) => (Kind::FnPtr(h), l),
                    TyView::Alias { trait_, name, args } => (Kind::Alias { trait_, name }, args),
                    TyView::Opaque(k, l) => (Kind::Opaque(k), l),
                    TyView::Dynamic(l) => (Kind::Dynamic, l),
                    TyView::ExTrait(k, l) => (Kind::ExTrait(k), l),
                    TyView::ExProj { trait_, name, args_term } => (Kind::ExProj { trait_, name }, args_term),
                    TyView::ClosureOf { owner, ordinal, args, .. } => (Kind::Closure { owner, ordinal }, args),
                    TyView::CoroutineOf { owner, ordinal, args, .. } => (Kind::Coroutine { owner, ordinal }, args),
                    _ => return Ok(None),
                };
                let n = cx.list_len(l);
                if n > 0 {
                    values.reserve(n as usize);
                    let first = cx.list_get(l, 0);
                    frames.push(Frame { kind, depth: d, val_base: values.len(), list: Some((l, 1, n)), extra: None });
                    cur_t = first;
                    cur_d = d;
                    continue;
                }
                match finish(ground, kind, &[])? {
                    Some(p) => p,
                    None => return Ok(None),
                }
            }
        };
        let mut val = val;
        loop {
            let Some(top) = frames.last_mut() else {
                return Ok(Some(val));
            };
            values.push(val);
            if let Some((l, idx, n)) = top.list {
                if idx < n {
                    top.list = Some((l, idx + 1, n));
                    cur_t = cx.list_get(l, idx);
                    cur_d = top.depth;
                    break;
                }
                top.list = None;
            }
            if let Some(extra_t) = top.extra.take() {
                cur_t = extra_t;
                cur_d = top.depth;
                break;
            }
            let Some(frame) = frames.pop() else {
                return Ok(Some(val));
            };
            let slice = values.get(frame.val_base..).unwrap_or(&[]);
            val = match finish(ground, frame.kind, slice)? {
                Some(p) => p,
                None => return Ok(None),
            };
            values.truncate(frame.val_base);
        }
    }
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
        TyView::Record(s, l) => Shape::Record(s.0, cx.list_len(l)),
        TyView::Union(l) => Shape::Union(cx.list_len(l)),
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
                PClause::ForFields { container, trait_, args } => Goal::ForFields {
                    container: import(cx, &mut NoClosures, wc.types, container, &fresh, 0)?,
                    trait_,
                    args: import_list(cx, &mut NoClosures, wc.types, args, &fresh, 0)?,
                },
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
    let Some(h) = holding_owning(cx, reads, own, trait_, args, ground, false)? else { return Ok(None) };
    let (imp, impl_args) = (h.impls[h.chosen], h.args[h.chosen]);
    match export_list(&mut cx.exporter(), impl_args, ground, 0)? {
        Some(l) => Ok(Some((imp, l))),
        None => Err(ResolveError::NotGround { item: imp }),
    }
}

/// [`select_owning`] preserving the caller's own type and const parameters (`Keep::Owner`):
/// for projections whose self type is a concrete constructor carrying the body's parameters
/// (e.g. `<Windows<'a, T> as Iterator>::Item`), assuming `owner`'s where-clauses.
pub(crate) fn select_owning_owner<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    owner: ItemKey,
    own: Option<OwnClosures<'_>>,
    trait_: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
) -> Result<Option<(ItemKey, List)>, ResolveError> {
    if let Ok(preds) = reads.predicates(owner) {
        for clause in preds.clauses {
            let goal = match *clause {
                PClause::Trait { trait_, args } => Goal::Trait {
                    trait_,
                    args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)?,
                },
                PClause::Projection { trait_, args, name, term } => Goal::Projection {
                    trait_,
                    args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)?,
                    name,
                    term: import(cx, &mut NoClosures, preds.types, term, &[], 0)?,
                },
                PClause::Const { cond, args } => Goal::ConstHolds {
                    cond,
                    args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)?,
                },
                PClause::ForFields { container, trait_, args } => Goal::ForFields {
                    container: import(cx, &mut NoClosures, preds.types, container, &[], 0)?,
                    trait_,
                    args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)?,
                },
                PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => continue,
            };
            cx.assume(goal)?;
        }
    }
    let Some(h) = holding_owning(cx, reads, own, trait_, args, ground, true)? else { return Ok(None) };
    let (imp, impl_args) = (h.impls[h.chosen], h.args[h.chosen]);
    match export_list_as(&mut cx.exporter(), impl_args, ground, Keep::Owner, 0)? {
        Some(l) => Ok(Some((imp, l))),
        None => Err(ResolveError::NotGround { item: imp }),
    }
}

/// Every impl of a trait that holds at a ground goal, in selection order, each with its
/// arguments in the context that tried it, and the one selected: the one that specializes
/// every other (specialize.rs `most_specific`). The others are the selected impl's
/// specialization ancestors (each applies wherever it does, being more general), which an
/// item the selected impl does not define is inherited from ([`resolve_in`]).
struct Holding<'x> {
    impls: Vec<ItemKey, Alloc<'x>>,
    args: Vec<crate::ty::TyList, Alloc<'x>>,
    chosen: usize,
}

/// [`Holding`] at `args` (self first, in `ground`); `None` when no impl holds. When
/// `allow_param` is true, a `TyView::Param` self type searches blanket impls (`Shape::Param`).
fn holding_owning<'x, W: World>(
    cx: &mut InferCtx<'x>,
    reads: &mut Reads<'_, '_, W>,
    own: Option<OwnClosures<'_>>,
    trait_: ItemKey,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    allow_param: bool,
) -> Result<Option<Holding<'x>>, ResolveError> {
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
    let shape = match shape_of(cx.view(self_ty), cx) {
        Some(s) => Some(s),
        None if allow_param
            && matches!(cx.view(self_ty), TyView::Param(_) | TyView::Opaque(..) | TyView::Alias { .. } | TyView::Dynamic(_)) =>
        {
            None
        }
        None => return Err(ResolveError::NotGround { item: trait_ }),
    };
    // Every holding impl, in selection order, with its arguments in `cx` (walk AJ P9: the
    // pinned alloc's `min_specialization` pairs, `impl<T: Display + ?Sized> ToString for T`
    // beside `impl ToString for str`, both hold at `str`).
    let mut holding: Vec<ItemKey, Alloc<'_>> = Vec::new_in(cx.alloc());
    let mut holding_args: Vec<crate::ty::TyList, Alloc<'_>> = Vec::new_in(cx.alloc());
    let buckets: &[Shape] = match shape {
        Some(s) => &[s, Shape::Param],
        None => &[Shape::Param],
    };
    let mut seen: Vec<ItemKey, Alloc<'_>> = Vec::new_in(cx.alloc());
    for &b in buckets {
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
    let chosen = crate::specialize::most_specific(cx, reads, trait_, &holding)?;
    Ok(Some(Holding { impls: holding, args: holding_args, chosen }))
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
    let Some(h) = holding_owning(&mut cx, reads, None, trait_, &all[..n], ground, false)? else {
        return Err(ResolveError::NoImpl { trait_, item });
    };
    // The item's definition: the selected impl's, else the nearest specialization ancestor's
    // that defines it, else the trait's default (rustc_ty_utils instance.rs
    // `resolve_associated_item`: `trait_def.ancestors(tcx, impl_def_id)?.leaf_def(tcx,
    // trait_item_id)`, the walk "starting with that impl itself", then `translate_args` to the
    // defining impl; RFC 1210: an impl that does not define an item inherits it). The pinned
    // core's zip.rs: `impl ZipImpl for Zip<A, B> where A: TrustedRandomAccess, ..` defines no
    // `fold`; it inherits the final `fold` of the `TrustedRandomAccessNoCoerce` impl it
    // specializes (t1800: `Zip<Map<Iter<u8>, ..>, IterMut<u8>>::fold` ran the trait's bodiless
    // `ZipImpl::fold`). Every holding impl but the selected one is its ancestor, and ancestors
    // are a chain, so the nearest that defines the item is the most specific of those; its
    // arguments are its own match at the goal (the translation, ground).
    let mut defining: Vec<usize, Alloc<'_>> = Vec::new_in(alloc);
    let mut members: Vec<ItemKey, Alloc<'_>> = Vec::new_in(alloc);
    let mut keys: Vec<ItemKey, Alloc<'_>> = Vec::new_in(alloc);
    for (i, &imp) in h.impls.iter().enumerate() {
        if let Some(a) = reads.assoc_item(imp, parent.name)? {
            if i == h.chosen {
                defining.clear();
                members.clear();
                keys.clear();
                defining.push(i);
                members.push(a.item);
                keys.push(imp);
                break;
            }
            defining.push(i);
            members.push(a.item);
            keys.push(imp);
        }
    }
    let at = match keys.len() {
        0 => return Ok(Resolved { item, args }),
        1 => 0,
        _ => crate::specialize::most_specific(&mut cx, reads, trait_, &keys)?,
    };
    let Some(impl_args) = export_list(&mut cx.exporter(), h.args[defining[at]], ground, 0)? else {
        return Err(ResolveError::NotGround { item: keys[at] });
    };
    let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(all.len(), alloc);
    v.extend_from_slice(ground.view().list(impl_args)?);
    v.extend_from_slice(&all[n..]);
    Ok(Resolved { item: members[at], args: ground.intern_list(&v)? })
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

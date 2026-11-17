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

    /// An async block's future of the body being decided (`PNode::Future(e, output,
    /// interior)` of `types`): only src/stalled.rs's source answers one (with the block's exact
    /// interior); every other source refuses it, as `import` always did.
    #[allow(clippy::too_many_arguments)]
    fn future_of(
        &mut self,
        cx: &mut InferCtx<'_>,
        types: PTys<'_>,
        e: crate::ids::ExprId,
        output: PTy,
        interior: PTy,
        subst: &[Ty],
        depth: u32,
    ) -> Result<Ty, CheckError> {
        let _ = (cx, types, e, output, interior, subst, depth);
        Err(CheckError::Limit(Limit::Export))
    }
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
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        let sig = import(cx, &mut NoClosures, facts.types, facts.sig, &subst, depth)?;
        cx.mk(TyView::ClosureOf { owner, ordinal, args, sig, kind: facts.kind })
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
    pub kinds: &'s [crate::ty::ClosureKind],
    pub sigs: &'s [PTy],
}

/// A closure of `own.owner` from `own` (its signature in `types`); any other from the world.
struct OwnFirst<'o, 'p, 'r, 'w, 'a, W: World> {
    own: Option<OwnClosures<'o>>,
    types: PTys<'p>,
    asked: Asked<'r, 'w, 'a, W>,
}

impl<W: World> ClosureSource for OwnFirst<'_, '_, '_, '_, '_, W> {
    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, CheckError> {
        let Some(own) = self.own.filter(|o| o.owner == owner) else {
            return self.asked.closure_of(cx, owner, ordinal, args, depth);
        };
        let i = ordinal as usize;
        let (Some(&kind), Some(&sig)) = (own.kinds.get(i), own.sigs.get(i)) else {
            return Err(CheckError::Malformed(crate::error::Malformed::BodyTypeInRecord(Question::Closure(owner, ordinal))));
        };
        let n = cx.list_len(args);
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        // A signature may name another closure of the same body (`|| |y| y`): this source.
        let types = self.types;
        let sig = import(cx, self, types, sig, &subst, depth)?;
        cx.mk(TyView::ClosureOf { owner, ordinal, args, sig, kind })
    }
}

/// Imports portable type `t` of `types` into `cx`, `Param(i)` becoming `subst[i]`.
pub(crate) fn import(cx: &mut InferCtx<'_>, src: &mut dyn ClosureSource, types: PTys<'_>, t: PTy, subst: &[Ty], depth: u32) -> Result<Ty, CheckError> {
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
            let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len as usize + 1, cx.alloc());
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
        PNode::Future(e, output, interior) => return src.future_of(cx, types, e, output, interior, subst, d),
        PNode::Infer => return Err(CheckError::Limit(Limit::Export)),
        // A future of another body imported at ground arguments: its facts are its owner's
        // check, which this import does not read yet: refused by name, never guessed.
        PNode::Coroutine { owner, ordinal, .. } => {
            return Err(CheckError::Malformed(crate::error::Malformed::BodyTypeInRecord(Question::Coroutine(owner, ordinal))));
        }
    };
    cx.mk(v)
}

pub(crate) fn import_list(
    cx: &mut InferCtx<'_>,
    src: &mut dyn ClosureSource,
    types: PTys<'_>,
    l: List,
    subst: &[Ty],
    depth: u32,
) -> Result<crate::ty::TyList, CheckError> {
    let items = types.list(l)?;
    let mut v: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(items.len(), cx.alloc());
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
    // A closure or async block the solver made revealing an opaque is filed first.
    cx.adopt(t)?;
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
    cx: &mut InferCtx<'_>,
    l: crate::ty::TyList,
    ground: &mut PortableTys<'_>,
    depth: u32,
) -> Result<Option<List>, CheckError> {
    let n = cx.list_len(l);
    let mut v: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
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
        TyView::Closure(..) | TyView::ClosureOf { .. } | TyView::Future(..) | TyView::CoroutineOf { .. } => Shape::Closure,
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
        let mut src = OwnFirst { own, types, asked: Asked { reads: &mut *reads, need: None } };
        match import(cx, &mut src, types, p, &[], 0) {
            Ok(t) => tys.push(t),
            Err(e) => {
                return Err(match src.asked.need {
                    Some(q) => ResolveError::Missing(q),
                    None => ResolveError::Check(e),
                });
            }
        }
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
    let mut cx = InferCtx::new_in(alloc);
    let mut tys: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(args.len(), alloc);
    for &p in args {
        let mut src = Asked { reads: &mut *reads, need: None };
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
    cx.select_all(reads)?;
    if cx.next_error().is_some() {
        return Ok(None);
    }
    let t = cx.resolve(term)?;
    Ok(export(&mut cx, t, ground, 0)?)
}

/// Whether the trait goal `args[0]: trait_<args[1..]>` holds at the ground `args` (self first,
/// in `ground`): the solver's own answer, the one a body's check gets, so the impls the
/// LANGUAGE gives count as the written ones do (the Reference, "Special types and traits":
/// `Copy` and `Clone` of a tuple, a closure, a fn item or a fn pointer; the auto traits;
/// `Sized`), where [`select_impl`] finds only written impls. The code builder's question at an
/// instance (blaze-ladder `FrontEnd::holds`: a `Copy` tuple's builtin `clone` copies its bytes).
/// A goal still ambiguous at ground arguments is no answer: `NotGround`, never a guessed `false`.
pub fn holds<W: World>(world: &W, trait_: ItemKey, args: &[PTy], ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<bool, ResolveError> {
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
    cx.register(Obligation { goal: Goal::Trait { trait_, args: goal_args }, cause: Cause::Body })?;
    let solved = cx.select_all(&mut reads);
    // A record the world had no answer to yet is a hole (`Reads::record_with`): the run read a
    // stand-in, so neither its answer nor its error (a trait's record read as none) is one. The
    // first hole is the need the caller fills before it asks again.
    if reads.holed() {
        let needs = reads.finish_need(None, alloc);
        return Err(match needs.first {
            Some(q) => ResolveError::Missing(q),
            None => ResolveError::NotGround { item: trait_ },
        });
    }
    solved?;
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
    let mut cx = InferCtx::new_in(alloc);
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

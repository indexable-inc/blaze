//! Coherence (solve.md, "Candidates and how they merge", helper 3): the orphan check and the
//! knowability rules of vendor/rustc_next_trait_solver/src/coherence.rs, the unknowable candidate
//! of solve/assembly/mod.rs, and the overlap check between two impl headers of
//! rustc_trait_selection's traits/coherence.rs (not vendored: ported from memory, said so on
//! each function).
//!
//! Every walk over a term uses an explicit stack (no recursion on a term's depth).
//!
//! Aliases: the reference normalizes each type the orphan checker meets
//! (`lazily_normalize_ty`, `structurally_normalize_ty` in the solver) and only a rigid alias
//! reaches its `ty::Alias` arm. Here a rule cannot evaluate a nested goal while it decides
//! (trait assembly asks [`unknowable`] synchronously), so a non-rigid alias (`Rigid::No`) is
//! checked exactly as the reference checks a rigid one: with a type variable, placeholder or
//! bound type inside, it may become anything (uncovered here, possibly local remotely); without,
//! it is non-local. That is the conservative reading for knowability (it can only add
//! ambiguity: coherence reports "may overlap", never a wrong "disjoint"), and it rejects an
//! impl whose header names a projection that normalizes to a local type
//! (`impl Foreign for <Local as Id>::Out`), which the reference accepts. The goal's self type
//! is already normalized by trait assembly (`TraitGoal::self_ty`).
//!
//! Not ported (named):
//! - `OrphanCheckMode::Compat` (projections with parameters counted as covered): only the
//!   `Proper` mode the solver and `orphan_check` use.
//! - The diagnostics of `OrphanCheckErr`: the list of non-local input types with
//!   `IsFirstInputType`, and `UncoveredTyParams::local_ty` (the second walk with
//!   `search_first_local_ty`). [`Orphan`] carries the verdict and the first uncovered parameter.
//! - The negative-coherence overlap mode (`impl_intersection_has_negative_obligation`, behind
//!   `#[rustc_strict_coherence]` and `with_negative_coherence`): `TraitFlags` has no bit for it.
//! - The leak check and `involves_placeholder` of `overlap`, and its intercrate ambiguity causes.
//! - `impls_are_allowed_to_overlap`'s rule that impls referencing an error may overlap.

use blaze_own::{Alloc, Vec};

use crate::assembly::{Cand, Cx, Goal, Source, Step};
use crate::error::{Limit, Malformed, SolveError, Stop};
use crate::eval::{Answer, Solver};
use crate::facts::{AdtFlags, Ask, Facts, ImplHeader, ImplPolarity, Log, Portable, Read, TraitFlags};
use crate::fold::{self, Inst, Scratch};
use crate::term::{AliasKind, Certainty, Def, GoalSource, List, Polarity, Pred, Ty, TyKind, TypingMode, Var, VarKind};
use crate::trait_goals::TraitGoal;
use crate::tys::{Relate, Tys};

/// The rules of this module's candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// The goal may hold in a downstream or future upstream crate
    /// (assembly/mod.rs `consider_coherence_unknowable_candidate`).
    Unknowable,
}

/// The most predicates the unknowable candidate's supertrait elaboration holds (rule 18).
const MAX_ELABORATED: usize = 4096;

/// The most rounds `overlap` re-evaluates its pending where-clauses (rule 18). Each round that
/// does not end the loop either binds a variable or settles a goal, so a correct world never
/// comes near it.
const MAX_ROUNDS: u32 = 1 << 16;

/// Whether the orphan check runs relative to this crate or to some other crate
/// (coherence.rs `InCrate`; only `OrphanCheckMode::Proper` for `Local`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InCrate {
    Local,
    Remote,
}

/// How a walk of the trait ref ended (coherence.rs `OrphanCheckEarlyExit`, and `Continue`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
    /// Every input type was non-local (`ControlFlow::Continue`: `NonLocalInputType`).
    NonLocal,
    /// A local type was reached through fundamental types (`LocalTy`).
    LocalTy,
    /// A type parameter (here: its variable) was reached before any local type
    /// (`UncoveredTyParam`).
    Uncovered(Ty),
}

/// The orphan check's verdict for an impl (coherence.rs `orphan_check_trait_ref`'s result, as
/// rustc_trait_selection's `orphan_check` maps it back to the impl's parameters).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Orphan {
    Ok,
    /// No input type is local or reaches a local type through fundamental types
    /// (`OrphanCheckErr::NonLocalInputType`).
    NotLocal,
    /// The impl's type parameter at this index is reachable before the first local type
    /// (`OrphanCheckErr::UncoveredTyParams`: the first parameter met in the uncovered type).
    UncoveredParam(u32),
}

/// Whether two impl headers can apply to one type (rustc_trait_selection traits/coherence.rs
/// `overlap`'s answer, with its ambiguity kept apart).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Overlap {
    /// The headers do not unify, or a where-clause of the intersection has no solution.
    Disjoint,
    /// Every where-clause of the intersection holds.
    Overlap,
    /// Some where-clause is ambiguous (a downstream or upstream crate may make it hold): an
    /// overlap for coherence's purposes, reported as "may overlap".
    MayOverlap,
}

/// Why two impls are allowed to overlap (rustc_middle's `ImplOverlapKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Permit {
    /// Both impls are of a `#[marker]` trait.
    Marker,
    /// One impl is a `#[rustc_reservation_impl]`, which overlaps with nothing.
    Reservation,
}

/// The term is a type variable, placeholder or bound type somewhere inside
/// (`has_type_flags(HAS_TY_PLACEHOLDER | HAS_TY_BOUND | HAS_TY_INFER)` in coherence.rs
/// `OrphanChecker::visit_ty`'s alias arm). Region and const variables do not count.
fn has_ty_var<T: Tys>(tys: &T, work: &mut Vec<(Ty, u32), Alloc<'_>>, t: Ty) -> Result<bool, SolveError> {
    let mut found = false;
    fold::visit(tys, work, t, |_, k| {
        if found {
            return false;
        }
        match k {
            TyKind::Infer(Var { kind: VarKind::Ty | VarKind::Int | VarKind::Float, .. })
            | TyKind::Placeholder(..)
            | TyKind::Bound(..) => {
                found = true;
                false
            }
            _ => true,
        }
    })?;
    Ok(found)
}

/// Walks the trait ref `self_ty: trait<args[1..]>` in generic order, preorder, the way
/// coherence.rs `OrphanChecker` (`visit_ty`, `visit_region`, `visit_const`) visits it, with an
/// explicit stack. `stack` is left as it was found.
fn orphan_walk<T: Tys, F: Facts, L: Log + ?Sized>(
    tys: &T,
    ask: &Ask<'_, F>,
    reads: &mut L,
    stack: &mut Vec<Ty, Alloc<'_>>,
    work: &mut Vec<(Ty, u32), Alloc<'_>>,
    self_ty: Ty,
    args: List,
    in_crate: InCrate,
) -> Result<Walk, Stop> {
    let base = stack.len();
    let r = orphan_walk_inner(tys, ask, reads, stack, work, base, self_ty, args, in_crate);
    stack.truncate(base);
    r
}

fn orphan_walk_inner<T: Tys, F: Facts, L: Log + ?Sized>(
    tys: &T,
    ask: &Ask<'_, F>,
    reads: &mut L,
    stack: &mut Vec<Ty, Alloc<'_>>,
    work: &mut Vec<(Ty, u32), Alloc<'_>>,
    base: usize,
    self_ty: Ty,
    args: List,
    in_crate: InCrate,
) -> Result<Walk, Stop> {
    for i in (1..args.len).rev() {
        stack.push(tys.list_get(args, i));
    }
    stack.push(self_ty);
    let mut visited = 0usize;
    while stack.len() > base {
        let Some(t) = stack.pop() else { break };
        visited += 1;
        if visited > fold::MAX_WALK {
            return Err(Stop::Error(SolveError::Limit(Limit::Walk)));
        }
        let t = tys.shallow_resolve(t);
        let k = tys.kind(t);
        let local = |reads: &mut L, d: Def| -> Result<bool, Stop> {
            match in_crate {
                // `def_id_is_local`: remotely nothing is local.
                InCrate::Local => ask.is_local(reads, d),
                InCrate::Remote => Ok(false),
            }
        };
        // A type variable: uncovered here, possibly local in the remote crate.
        let var_exit = match in_crate {
            InCrate::Local => Walk::Uncovered(t),
            InCrate::Remote => Walk::LocalTy,
        };
        match k {
            // `visit_region` and `visit_const`: ignored.
            TyKind::ReStatic
            | TyKind::ReParam(_)
            | TyKind::ReErased
            | TyKind::ConstValue(..)
            | TyKind::ConstParam(_)
            | TyKind::Infer(Var { kind: VarKind::Region | VarKind::Const, .. })
            | TyKind::Alias(_, AliasKind::Const, ..) => {}
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::FnPtr(..)
            | TyKind::Array(..)
            | TyKind::Slice(_)
            | TyKind::Pat(..)
            | TyKind::RawPtr(..)
            | TyKind::Never
            | TyKind::Tuple(_)
            | TyKind::Record(..)
            | TyKind::Union(_) => {}
            // The reference panics: the orphan check expects only inference variables.
            TyKind::Param(index) => {
                let at = core::panic::Location::caller();
                return Err(Stop::Error(SolveError::Malformed(Malformed::Param { index, args: 0, head: None, at, goal: None })));
            }
            TyKind::Placeholder(..) | TyKind::Bound(..) | TyKind::Infer(_) => return Ok(var_exit),
            // A rigid alias (and, here, a non-rigid one: see the module doc) may normalize to
            // anything: with a type variable inside, to that variable.
            TyKind::Alias(..) => {
                if has_ty_var(tys, work, t)? {
                    return Ok(var_exit);
                }
                // Otherwise non-local, opaque types included (they hide their hidden type).
            }
            // Fundamental types: look inside.
            TyKind::Ref(_, pointee, _) => stack.push(pointee),
            TyKind::Adt(d, l) => {
                if local(reads, d)? {
                    return Ok(Walk::LocalTy);
                }
                let flags = ask.adt(reads, d)?.flags;
                if flags.has(AdtFlags::FUNDAMENTAL) {
                    if flags.has(AdtFlags::BOX) {
                        // `Box<T, A>`: only `T` (the `OwnedBox` lang item's special case).
                        if l.len > 0 {
                            stack.push(tys.list_get(l, 0));
                        }
                    } else {
                        for i in (0..l.len).rev() {
                            stack.push(tys.list_get(l, i));
                        }
                    }
                }
            }
            TyKind::Foreign(d) => {
                if local(reads, d)? {
                    return Ok(Walk::LocalTy);
                }
            }
            TyKind::Dynamic(l, _) => {
                if l.len > 0
                    && let TyKind::ExTrait(principal, _) = tys.kind(tys.list_get(l, 0))
                    && local(reads, principal)?
                {
                    return Ok(Walk::LocalTy);
                }
            }
            TyKind::Error => return Ok(Walk::LocalTy),
            TyKind::Canonical(at) => {
                return Err(Stop::Error(SolveError::Malformed(Malformed::Canonical { at })));
            }
            // `unreachable!("unnameable type in coherence")` in the reference; existential
            // predicates are never a generic argument.
            TyKind::FnDef(..) | TyKind::Closure(..) | TyKind::Coroutine(..) | TyKind::ExTrait(..) | TyKind::ExProj(..) | TyKind::ExAuto(_) => {
                return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred())));
            }
        }
    }
    Ok(Walk::NonLocal)
}

/// Whether every impl that could apply to `self_ty: trait_<args[1..]>` is known to this crate
/// (coherence.rs `trait_ref_is_knowable`, with blaze's sealed-trait rule; `true` is
/// `Ok(())`, `false` either `Conflict`).
fn trait_ref_is_knowable<T: Tys, F: Facts, L: Log + ?Sized>(
    tys: &T,
    ask: &Ask<'_, F>,
    reads: &mut L,
    alloc: Alloc<'_>,
    trait_: Def,
    self_ty: Ty,
    args: List,
) -> Result<bool, Stop> {
    let mut stack: Vec<Ty, Alloc<'_>> = Vec::new_in(alloc);
    let mut work: Vec<(Ty, u32), Alloc<'_>> = Vec::new_in(alloc);
    let def = ask.trait_def(reads, trait_)?;
    // blaze: a sealed trait has no downstream or cousin implementor.
    if !def.flags.has(TraitFlags::SEALED)
        && orphan_walk(tys, ask, reads, &mut stack, &mut work, self_ty, args, InCrate::Remote)? == Walk::LocalTy
    {
        // `Conflict::Downstream`.
        return Ok(false);
    }
    // `trait_ref_is_local_or_fundamental`.
    if ask.is_local(reads, trait_)? || def.flags.has(TraitFlags::FUNDAMENTAL) {
        return Ok(true);
    }
    // A remote non-fundamental trait: knowable only where this crate is an owner, else
    // `Conflict::Upstream`.
    Ok(orphan_walk(tys, ask, reads, &mut stack, &mut work, self_ty, args, InCrate::Local)? == Walk::LocalTy)
}

/// The unknowable candidate (solve/assembly/mod.rs `assemble_and_evaluate_candidates`'s
/// coherence branch and `consider_coherence_unknowable_candidate`): in `TypingMode::Coherence`,
/// when the trait ref is not knowable, push one candidate whose step is [`Rule::Unknowable`]
/// and answer `true`: the caller assembles nothing else, as the reference returns early.
///
/// Called before any other candidate, with the self type resolved and normalized; in any other
/// mode it answers `false` and pushes nothing.
///
/// The reference returns early only when the candidate's probe succeeds: a supertrait goal
/// with no solution makes it fall through to the other candidates. Here the decision is made
/// before the probe, so such a goal has no candidate (no solution). The two differ only where
/// an impl of the trait applies to a type its supertrait does not hold for, which the impl's
/// own well-formedness check refuses.
pub fn unknowable<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, g: TraitGoal, out: &mut Vec<Cand, Alloc<'_>>) -> Result<bool, Stop> {
    if cx.mode != TypingMode::Coherence {
        return Ok(false);
    }
    let alloc = cx.alloc;
    if trait_ref_is_knowable(&*cx.tys, &cx.ask, &mut *cx.reads, alloc, g.trait_, g.self_ty, g.args)? {
        return Ok(false);
    }
    let mut cand = Cand::new(Source::CoherenceUnknowable, crate::assembly::Rule::Coherence(Rule::Unknowable));
    cand.a = g.self_ty;
    cand.d = g.trait_;
    out.push(cand);
    Ok(true)
}

/// A candidate's step for this module's rules.
///
/// `Rule::Unknowable` (assembly/mod.rs `consider_coherence_unknowable_candidate`): the goal's
/// trait ref is elaborated (`elaborate::elaborate`, every supertrait clause transitively, each
/// once) and every clause but the goal itself is added as a `Misc` goal, since a supertrait may
/// still be provably unimplemented; the response is ambiguous. As in the reference the trait
/// ref is elaborated as a positive predicate whatever the goal's polarity.
pub fn step<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, cand: &mut Cand, rule: Rule, stage: u8) -> Result<Step, Stop> {
    match (rule, stage) {
        (Rule::Unknowable, 0) => {
            let Pred::Trait { args, .. } = cx.goal.pred else {
                return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred())));
            };
            if args.len == 0 {
                return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred())));
            }
            let mut items = cx.list_small(args);
            items[0] = cand.a;
            let args = cx.mk_list(&items)?;
            let mut seen: Vec<Pred, Alloc<'_>> = Vec::new_in(cx.alloc);
            seen.push(Pred::Trait { trait_: cand.d, args, polarity: Polarity::Positive });
            let mut i = 0;
            while i < seen.len() {
                let p = seen[i];
                i += 1;
                let Pred::Trait { trait_, args, .. } = p else { continue };
                let def = cx.trait_def(trait_)?;
                for &s in def.supertraits {
                    let q = cx.import_pred(def.portable, s, args)?;
                    if seen.contains(&q) {
                        continue;
                    }
                    if seen.len() >= MAX_ELABORATED {
                        return Err(Stop::Error(SolveError::Limit(Limit::Table)));
                    }
                    seen.push(q);
                }
            }
            for &q in &seen[1..] {
                cx.add(GoalSource::Misc, q);
            }
            Ok(Step::Done(Certainty::Ambiguous))
        }
        (Rule::Unknowable, _) => Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred()))),
    }
}

/// Fresh variables for an impl's generics: a const variable where the impl's OWN terms (its
/// trait arguments, where clauses and associated type values) name `ConstParam(i)`, else a type
/// variable (a region parameter is erased on import, so its variable is never used; a parameter
/// none of them names is never read). `fresh_args_for_item`.
///
/// It takes the header, never a bare table: a record's table is shared (the world answers each
/// record in one table that holds every item built so far), so a scan of the whole table read
/// another item's `ConstParam(i)` as this impl's, and made the impl's type parameter `i` a
/// const as soon as such an item had been built: the result of checking a body depended on
/// what was checked before it (tests/front2_order.rs in blaze-ladder).
pub fn fresh_for<'a, T: Tys>(tys: &mut T, s: &mut Scratch<'_>, h: &ImplHeader<'_>, alloc: Alloc<'a>) -> Result<Vec<Ty, Alloc<'a>>, SolveError> {
    let is_const = const_params(s, h, alloc)?;
    let mut v = Vec::with_capacity_in(h.generics as usize, alloc);
    for c in is_const {
        v.push(tys.fresh(if c { VarKind::Const } else { VarKind::Ty })?);
    }
    Ok(v)
}

/// [`fresh_for`] for the impl `impl_`, its const parameters found once per solver: kept in
/// `s` by the impl and its header's digest (an impl met as a candidate of many questions
/// walked its whole header each time: `fresh_for` was 7.9% of a `blaze run`, runner 2's
/// sample of 2026-11-16). An impl of more than 64 parameters is walked each time.
pub(crate) fn fresh_for_impl<'a, T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    impl_: Def,
    h: &ImplHeader<'_>,
    alloc: Alloc<'a>,
) -> Result<crate::assembly::SmallTys<'a>, SolveError> {
    if h.generics > 64 {
        return Ok(crate::assembly::SmallTys::Heap(fresh_for(tys, s, h, alloc)?));
    }
    let mask = match s.const_mask(impl_, h.digest) {
        Some(m) => m,
        None => {
            let mut m = 0u64;
            for (i, c) in const_params(s, h, alloc)?.into_iter().enumerate() {
                if c {
                    m |= 1 << i;
                }
            }
            s.keep_const_mask(impl_, h.digest, m);
            m
        }
    };
    let mut v = crate::assembly::SmallTys::new_with_len(h.generics as usize, alloc);
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = tys.fresh(if (mask >> i) & 1 == 1 { VarKind::Const } else { VarKind::Ty })?;
    }
    Ok(v)
}

/// Which of the impl's generics its own terms name as `ConstParam(i)` ([`fresh_for`]).
fn const_params<'a>(s: &mut Scratch<'_>, h: &ImplHeader<'_>, alloc: Alloc<'a>) -> Result<Vec<bool, Alloc<'a>>, SolveError> {
    let p = h.portable;
    let (roots, work) = s.params_buffers();
    roots.clear();
    let list = |l: List, roots: &mut Vec<Ty, Alloc<'_>>| -> Result<(), SolveError> {
        for i in 0..l.len {
            let at = l.start + i;
            let Some(&x) = p.lists.get(at as usize) else {
                return Err(SolveError::Malformed(Malformed::PortableIndex { node: at, len: p.lists.len() as u32 }));
            };
            roots.push(x);
        }
        Ok(())
    };
    list(h.args, roots)?;
    for c in h.where_clauses {
        match *c {
            Pred::Trait { args, .. } | Pred::HostEffect { args, .. } | Pred::ConstHolds { args, .. } => list(args, roots)?,
            Pred::ForFields { container, args_after_self, .. } => {
                roots.push(container);
                list(args_after_self, roots)?;
            }
            Pred::Projection { alias: a, term: b }
            | Pred::NormalizesTo { alias: a, term: b }
            | Pred::TypeOutlives { ty: a, region: b }
            | Pred::RegionOutlives { a, b }
            | Pred::Subtype { a, b }
            | Pred::Coerce { a, b }
            | Pred::ConstArgHasType { ct: a, ty: b } => roots.extend([a, b]),
            Pred::WellFormed(a) | Pred::ConstEvaluatable(a) => roots.push(a),
            Pred::DynCompatible(_) => {}
            Pred::Eq { .. } => return Err(SolveError::Malformed(crate::error::bad_pred())),
        }
    }
    roots.extend(h.assoc.iter().map(|a| a.value));
    let mut is_const: Vec<bool, Alloc<'a>> = Vec::from_elem_in(false, h.generics as usize, alloc);
    let konst = |k: TyKind| match k {
        TyKind::ConstParam(x) => Some(x),
        _ => None,
    };
    crate::builtin::params_reached(p, roots, konst, &mut is_const, work)?;
    Ok(is_const)
}

/// The portable list `l` of `p`, imported with `args`; a parameter past `args` names the
/// caller's line.
#[track_caller]
fn import_list<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, p: Portable<'_>, l: List, args: &[Ty], _alloc: Alloc<'_>) -> Result<List, SolveError> {
    let site = core::panic::Location::caller();
    fold::import_list_at(tys, s, p, l, Inst { args, bound: &[] }, site)
}

/// The portable predicate `pred` of `p`, imported with `args` (`Cx::import_pred` outside a
/// context).
#[track_caller]
fn import_pred<T: Tys>(tys: &mut T, s: &mut Scratch<'_>, p: Portable<'_>, pred: Pred, args: &[Ty], _alloc: Alloc<'_>) -> Result<Pred, SolveError> {
    let site = core::panic::Location::caller();
    let inst = Inst { args, bound: &[] };
    Ok(match pred {
        Pred::Trait { trait_, args: a, polarity } => {
            Pred::Trait { trait_, args: fold::import_list_at(tys, s, p, a, inst, site)?, polarity }
        }
        Pred::HostEffect { trait_, args: a, constness } => {
            Pred::HostEffect { trait_, args: fold::import_list_at(tys, s, p, a, inst, site)?, constness }
        }
        Pred::ConstHolds { cond, args: a } => {
            Pred::ConstHolds { cond, args: fold::import_list_at(tys, s, p, a, inst, site)? }
        }
        Pred::ForFields { container, trait_, args_after_self } => Pred::ForFields {
            container: fold::import_at(tys, s, p, container, inst, site)?,
            trait_,
            args_after_self: fold::import_list_at(tys, s, p, args_after_self, inst, site)?,
        },
        Pred::WellFormed(x) => Pred::WellFormed(fold::import_at(tys, s, p, x, inst, site)?),
        Pred::ConstEvaluatable(x) => Pred::ConstEvaluatable(fold::import_at(tys, s, p, x, inst, site)?),
        Pred::Projection { alias: a, term: b }
        | Pred::NormalizesTo { alias: a, term: b }
        | Pred::TypeOutlives { ty: a, region: b }
        | Pred::RegionOutlives { a, b }
        | Pred::Subtype { a, b }
        | Pred::Coerce { a, b }
        | Pred::ConstArgHasType { ct: a, ty: b } => {
            let mut out = [Ty::NONE; 2];
            fold::import_slice(tys, s, p, &[a, b], inst, &mut out, site)?;
            match pred {
                Pred::Projection { .. } => Pred::Projection { alias: out[0], term: out[1] },
                Pred::NormalizesTo { .. } => Pred::NormalizesTo { alias: out[0], term: out[1] },
                Pred::TypeOutlives { .. } => Pred::TypeOutlives { ty: out[0], region: out[1] },
                Pred::RegionOutlives { .. } => Pred::RegionOutlives { a: out[0], b: out[1] },
                Pred::Subtype { .. } => Pred::Subtype { a: out[0], b: out[1] },
                Pred::Coerce { .. } => Pred::Coerce { a: out[0], b: out[1] },
                Pred::ConstArgHasType { .. } => Pred::ConstArgHasType { ct: out[0], ty: out[1] },
                _ => unreachable!(),
            }
        }
        Pred::DynCompatible(d) => Pred::DynCompatible(d),
        Pred::Eq { .. } => return Err(SolveError::Malformed(crate::error::bad_pred())),
    })
}

/// An impl header's trait arguments with fresh variables for its generics
/// (`fresh_impl_header`'s trait ref), and the variables.
fn fresh_header<'a, T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    h: &ImplHeader<'_>,
    alloc: Alloc<'a>,
) -> Result<(Vec<Ty, Alloc<'a>>, List), SolveError> {
    let vars = fresh_for(tys, s, h, alloc)?;
    let args = import_list(tys, s, h.portable, h.args, &vars, alloc)?;
    Ok((vars, args))
}

/// The orphan check of an impl (rustc_trait_selection traits/coherence.rs `orphan_check`, not
/// vendored, from memory, over the vendored coherence.rs `orphan_check_trait_ref` with
/// `InCrate::Local { mode: Proper }`): an impl of a local trait is always allowed; else the
/// impl's parameters become fresh variables and its trait ref is walked, and an uncovered
/// variable is mapped back to the first impl parameter met in it (`UncoveredTyParamCollector`).
/// Leaves `tys` as it found it.
pub fn orphan_check<T: Tys, F: Facts>(tys: &mut T, facts: &Ask<'_, F>, reads: &mut Vec<Read, Alloc<'_>>, impl_: Def) -> Result<Orphan, Stop> {
    let h = facts.impl_header(reads, impl_)?;
    // "If the *trait* is local to the crate, ok."
    if facts.is_local(reads, h.trait_)? {
        return Ok(Orphan::Ok);
    }
    let alloc = *reads.allocator();
    let snap = tys.snapshot();
    let r = orphan_check_inner(tys, facts, reads, &h, alloc);
    tys.rollback_to(snap);
    r
}

fn orphan_check_inner<T: Tys, F: Facts>(
    tys: &mut T,
    facts: &Ask<'_, F>,
    reads: &mut Vec<Read, Alloc<'_>>,
    h: &ImplHeader<'_>,
    alloc: Alloc<'_>,
) -> Result<Orphan, Stop> {
    let mut s = Scratch::new_in(alloc);
    let (vars, args) = fresh_header(tys, &mut s, h, alloc)?;
    if args.len == 0 {
        return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred())));
    }
    let self_ty = tys.list_get(args, 0);
    let mut stack: Vec<Ty, Alloc<'_>> = Vec::new_in(alloc);
    let mut work: Vec<(Ty, u32), Alloc<'_>> = Vec::new_in(alloc);
    match orphan_walk(&*tys, facts, reads, &mut stack, &mut work, self_ty, args, InCrate::Local)? {
        Walk::LocalTy => Ok(Orphan::Ok),
        Walk::NonLocal => Ok(Orphan::NotLocal),
        Walk::Uncovered(t) => {
            let mut first: Option<u32> = None;
            fold::visit(&*tys, &mut work, t, |x, k| {
                if first.is_some() {
                    return false;
                }
                if let TyKind::Infer(_) = k {
                    first = vars.iter().position(|&v| v == x).map(|i| i as u32);
                }
                true
            })?;
            match first {
                Some(i) => Ok(Orphan::UncoveredParam(i)),
                // An uncovered term holds a variable, and the only variables are the impl's.
                None => Err(Stop::Error(SolveError::Malformed(Malformed::Param {
                    index: u32::MAX,
                    args: h.generics,
                    head: None,
                    at: core::panic::Location::caller(),
                    goal: None,
                }))),
            }
        }
    }
}

/// Whether impls `a` and `b` overlap (rustc_trait_selection traits/coherence.rs `overlap`, not
/// vendored, from memory, in its stable mode): both headers are instantiated with fresh
/// variables (`fresh_impl_header`), their trait refs equated (`equate_impl_headers`; a
/// non-rigid alias becomes a `Projection` goal as `Cx::eq` makes one), then in
/// `TypingMode::Coherence` and the empty param env every resulting goal and every where-clause
/// of both impls is evaluated to a fixpoint (`impl_intersection_has_impossible_obligation`
/// through `try_evaluate_obligations`): any goal with no solution makes them disjoint; all
/// holding is an overlap; goals left ambiguous are "may overlap", which coherence counts as an
/// overlap.
///
/// Polarity does not enter: a positive and a negative impl of one header overlap (E0751), and
/// whether an overlap is allowed (marker traits, reservation impls) is [`permitted`]'s. Impls of
/// two different traits are disjoint. Where-clauses are evaluated with no bound variables (an
/// impl header's clauses carry no binder count). Leaves `tys` as it found it.
pub fn overlap<'a, T: Tys, F: Facts>(
    solver: &mut Solver<'a>,
    tys: &mut T,
    facts: &F,
    reads: &mut Vec<Read, Alloc<'a>>,
    a: Def,
    b: Def,
) -> Result<Overlap, Stop> {
    let snap = tys.snapshot();
    let r = overlap_inner(solver, tys, facts, reads, a, b);
    tys.rollback_to(snap);
    r
}

fn overlap_inner<'a, T: Tys, F: Facts>(
    solver: &mut Solver<'a>,
    tys: &mut T,
    facts: &F,
    reads: &mut Vec<Read, Alloc<'a>>,
    a: Def,
    b: Def,
) -> Result<Overlap, Stop> {
    let alloc = solver.alloc();
    let ask = Ask::new(facts);
    let ha = ask.impl_header(reads, a)?;
    let hb = ask.impl_header(reads, b)?;
    if ha.trait_ != hb.trait_ {
        return Ok(Overlap::Disjoint);
    }
    let mut s = Scratch::new_in(alloc);
    let (va, la) = fresh_header(tys, &mut s, &ha, alloc)?;
    let (vb, lb) = fresh_header(tys, &mut s, &hb, alloc)?;
    if la.len != lb.len {
        return Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred())));
    }
    let mut goals: Vec<Pred, Alloc<'a>> = Vec::new_in(alloc);
    let mut deferred: Vec<(Ty, Ty), Alloc<'a>> = Vec::new_in(alloc);
    for i in 0..la.len {
        let (x, y) = (tys.list_get(la, i), tys.list_get(lb, i));
        if !tys.unify(x, y, Relate::Lazy, &mut deferred)? {
            return Ok(Overlap::Disjoint);
        }
        // As `Cx::eq`: each deferred non-rigid alias is `Projection { alias, term: fresh }` and
        // `fresh` relates with the other side.
        while let Some((alias, other)) = deferred.pop() {
            let kind = match tys.kind(alias) {
                TyKind::Alias(_, AliasKind::Const, ..) => VarKind::Const,
                _ => VarKind::Ty,
            };
            let fresh = tys.fresh(kind)?;
            goals.push(Pred::Projection { alias, term: fresh });
            if !tys.unify(fresh, other, Relate::Lazy, &mut deferred)? {
                return Ok(Overlap::Disjoint);
            }
        }
    }
    for &wc in ha.where_clauses {
        goals.push(import_pred(tys, &mut s, ha.portable, wc, &va, alloc)?);
    }
    for &wc in hb.where_clauses {
        goals.push(import_pred(tys, &mut s, hb.portable, wc, &vb, alloc)?);
    }
    let env = solver.env(tys, facts, &[], reads)?;
    let mut pending = goals;
    let mut next: Vec<Pred, Alloc<'a>> = Vec::new_in(alloc);
    for _ in 0..MAX_ROUNDS {
        let bound_before = tys.bindings_len();
        next.clear();
        for &pred in pending.iter() {
            match solver.evaluate(tys, facts, TypingMode::Coherence, Goal { env, pred, bound: 0 }, reads)? {
                Answer::No => return Ok(Overlap::Disjoint),
                Answer::Yes => {}
                Answer::Maybe(_) => next.push(pred),
            }
        }
        if next.is_empty() {
            return Ok(Overlap::Overlap);
        }
        // No goal settled and nothing was bound: evaluating again gives the same answers.
        if next.len() == pending.len() && tys.bindings_len() == bound_before {
            return Ok(Overlap::MayOverlap);
        }
        core::mem::swap(&mut pending, &mut next);
    }
    Err(Stop::Error(SolveError::Limit(Limit::Rounds)))
}

/// Whether impls `a` and `b` may overlap without a coherence error (rustc_middle's
/// `TyCtxt::impls_are_allowed_to_overlap`, not vendored, from memory): a reservation impl
/// overlaps with anything; a positive and a negative impl never; two impls of a `#[marker]`
/// trait of one polarity may. `None`: an overlap of the two is an error.
pub fn permitted<F: Facts>(ask: &Ask<'_, F>, reads: &mut Vec<Read, Alloc<'_>>, a: Def, b: Def) -> Result<Option<Permit>, Stop> {
    let ha = ask.impl_header(reads, a)?;
    let hb = ask.impl_header(reads, b)?;
    match (ha.polarity, hb.polarity) {
        (ImplPolarity::Reservation, _) | (_, ImplPolarity::Reservation) => return Ok(Some(Permit::Reservation)),
        (ImplPolarity::Positive, ImplPolarity::Negative) | (ImplPolarity::Negative, ImplPolarity::Positive) => return Ok(None),
        (ImplPolarity::Positive, ImplPolarity::Positive) | (ImplPolarity::Negative, ImplPolarity::Negative) => {}
    }
    let marker_a = ask.trait_def(reads, ha.trait_)?.flags.has(TraitFlags::MARKER);
    let marker_b = ask.trait_def(reads, hb.trait_)?.flags.has(TraitFlags::MARKER);
    Ok(if marker_a && marker_b { Some(Permit::Marker) } else { None })
}

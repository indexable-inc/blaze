//! Coroutine states laid out from TYPES before any LIR, then each coroutine built into its laid
//! state (ASYNC.md P8, option (b), main's rulings of 2026-11-16). Replaces the build-again path
//! (a build that met a state not made, the state built, the asker built again).
//!
//! 1. ROOTS ([`roots_of_instance`]): the coroutines an instance's own types hold by value
//!    (front2 coroutine_needs.rs `held_coroutines`); for one of a coroutine's three functions,
//!    the coroutine itself.
//! 2. ORDER ([`lay_and_build`]): `layout_order` over each coroutine's own STATE RECIPE (the
//!    owner's check made it: front2 coroutine_recipes.rs, through `FrontEnd::coroutine_recipe`):
//!    its upvars and saved locals, grounded at the owner's arguments. A state holding itself by
//!    value is E0733, refused naming every coroutine of the cycle by its owner's path. Then, by
//!    rounds, the coroutines those bodies USE by value without holding them (their slots are
//!    laid by the build): ordered too, never as held.
//! 3. LAY: per coroutine in order, its upvars' and saved locals' layouts (each held coroutine
//!    already laid), one `u8` drop flag per saved local whose ground type needs drop (main's
//!    ruling (ii)), `lay_state`; the state recorded (`States::record`) BEFORE the build.
//! 4. BUILD: `build_coroutine` into the laid state (blaze-lir `split_in`): what the LIR keeps
//!    must fit the region the flow's saved locals made, or the split refuses it by name
//!    (`PastRegion`, `ValueAcrossSuspend`: a builder bug, never a layout input).
//!
//! The order walk reads each owner's CHECKED body (its upvars' facts, its view): an owner not
//! checked yet ends the walk with [`Lay::Unchecked`]; the maker checks it and the walk starts
//! again (types only: no LIR was built; bounded by the owners of one program, MOST_OWNERS).
//!
//! A root type the grounding refuses is not a root: the builder grounds the same type where it
//! meets it and refuses it there by name, with the expression's range; a coroutine state it then
//! asks that was not laid is refused by name ("a coroutine state not laid before the build"),
//! never built late.

use core::cell::{Cell, RefCell};

use blaze_build::coroutine_state::{LaidState, StateName, StateNames, StatePlan, StatePlant, lay_state_with_conflicts, may_partial_move_bindings, may_partial_move_temps};
use blaze_build::layout::{LayoutCx, Layouts};
use blaze_build::{CoroutineOf, coroutines_in};
use blaze_own::AVec;
use blaze_typeck::BodyView;

use super::make_one::{CachedFront, QueryCache};
use super::*;
use crate::code_records::reads::{ReadLog, ReadPlant, Recorded, laid_answer};
use crate::front2::coroutine_needs::{HOLDS_ITSELF, OrderError, held_coroutines, layout_order, referenced_coroutines};

/// How a lay ended.
pub(super) enum Lay<'a> {
    /// Every state laid and every coroutine built (its three functions in the stash).
    Done,
    /// The walk met a coroutine whose owner fn is not checked yet: check it, lay again.
    Unchecked(ItemKey),
    /// One or more coroutines to build need their owner's constants pulled at `owner_args`
    /// (`const_at::needs_of`, plus `plain_needs_of` when `pull_plain`) before `build_laid`
    /// walks their bodies. States laid in this pass have already been forgotten.
    PullConstants(AVec<'a, (ItemKey, List)>),
}

/// A coroutine the front end gave no upvars' facts or no state recipe for.
fn not_answered() -> BuildError {
    BuildError::UnsupportedSig { what: "a coroutine whose upvars or state recipe the front end did not answer" }
}

/// build_from's answer to [`Lay::Unchecked`]: it checks every body before the build, so an
/// owner still unchecked did not check (or is not in the program), refused naming it.
pub(super) fn unchecked_owner<P: FrontEnd>(program: &P, asking: usize, owner: ItemKey) -> RunError {
    let error = BuildError::UnsupportedSig { what: "an async block whose owner fn has no checked body" };
    RunError::ItemRefused { function: program.function(asking).named(), error, item: owner, path: program.item_named(owner) }
}

/// A build error met ordering, laying or building a state, as the run's refusal: a projection
/// the seam did not normalize names its why (the seam's `last_projection`: the trait, `Self`'s
/// form, the solver's error; rule 19), as make_one.rs `build_refusal` names it.
fn build_error<P: FrontEnd>(program: &P, function: Named, error: BuildError, range: Option<blaze_typeck::Range>) -> RunError {
    if let BuildError::Projection { .. } = error
        && let Some(why) = program.last_projection()
    {
        return RunError::Projection { function, why };
    }
    if let BuildError::CoroutineRecipe { owner, .. } = error {
        return RunError::ItemRefused { function, error, item: owner, path: program.item_named(owner) };
    }
    RunError::Build { function, error, range }
}

fn refuse(function: Named, what: &'static str) -> RunError {
    RunError::Build { function, error: BuildError::UnsupportedSig { what }, range: None }
}

/// Record type `t` of `owner`'s body `view` (table `record`) at the owner's ground arguments:
/// an `async` block of that body still named by its expression is its identity (as blaze-build
/// `Walk::ground_ty` makes it); `wrap` makes the reference the flow holds (a saved local's
/// `SavedLocal::wrap`; `None` for a type of the record itself).
#[expect(clippy::too_many_arguments, reason = "the type, its wrap and the grounding's tables")]
fn ground_saved(
    view: &BodyView<'_>,
    record: PTys<'_>,
    ty: PTy,
    wrap: Option<Mutbl>,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    facts: &dyn LayoutFacts,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let full = |_| BuildError::Malformed { what: "the ground table is full" };
    // In its own body's record a block (at top level OR nested inside an ADT/combinator/tuple)
    // is named by its expression (`PNode::Future`); `ground_own` maps every one to its identity
    // (`PNode::Coroutine`, blaze-build coroutine.rs).
    let g = blaze_build::ground_own(record, ty, args, view, ground, scratch, facts)?;
    match wrap {
        None => Ok(g),
        Some(m) => ground.intern(PNode::Ref(m, g)).map_err(full),
    }
}

/// The coroutines instance `(i, args)`'s arguments and (when `include_body`) body types hold by
/// value or reference through pointers/refs/type args (the module doc says why a refused body type
/// is not a root). Always inspects `args` and `extra_args` even when the instance has no body
/// (e.g. `drop_in_place::<Wrapper<C>>`, `vtable_shim`, `fn_shim`, builtin `Clone`/`From`).
#[expect(clippy::too_many_arguments, reason = "instance grounding tables and optional body walk flag")]
pub(super) fn roots_of_instance<'a, P: FrontEnd>(
    program: &P,
    bodies: &Bodies<'_>,
    adts: &dyn Adts,
    i: usize,
    args: &[PTy],
    extra_args: Option<List>,
    include_body: bool,
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, CoroutineOf>, RunError> {
    let name = || program.function(i).named();
    let mut roots: AVec<'_, PTy> = AVec::with_capacity_in(args.len(), alloc);
    roots.extend_from_slice(args);
    if let Some(list) = extra_args
        && let Ok(slice) = ground.view().list(list)
    {
        roots.extend_from_slice(slice);
    }
    if include_body
        && let (Some((body, _)), Some(done)) = (program.function(i).body, bodies.checked.get(i).and_then(Option::as_ref))
    {
        let view = body.view(program.body_types(i));
        let record = done.types.view();
        let mut tys: AVec<'_, PTy> = AVec::new_in(alloc);
        tys.extend_from_slice(&done.expr_ty);
        tys.extend_from_slice(&done.local_ty);
        tys.sort_unstable();
        tys.dedup();
        for &t in tys.iter() {
            if matches!(
                record.node(t),
                Ok(
                    PNode::Bool
                        | PNode::Char
                        | PNode::Int(_)
                        | PNode::Uint(_)
                        | PNode::Float(_)
                        | PNode::Str
                        | PNode::Never
                        | PNode::ConstValue { .. }
                )
            ) {
                continue;
            }
            if let Ok(g) = ground_saved(&view, record, t, None, args, ground, adts.facts(), alloc) {
                roots.push(g);
            }
        }
    }
    if roots.is_empty() {
        return Ok(AVec::new_in(alloc));
    }
    roots.sort_unstable();
    roots.dedup();
    referenced_coroutines(ground, &roots, adts.facts(), alloc).map_err(|error| build_error(program, name(), error, None))
}

/// The ground arguments of coroutine `c`'s owner, copied out of the ground table.
fn args_of<'a>(ground: &PortableTys<'_>, c: CoroutineOf, alloc: Alloc<'a>) -> Result<AVec<'a, PTy>, BuildError> {
    let list = ground.view().list(c.owner_args).map_err(|_| BuildError::Malformed { what: "a coroutine's arguments past the ground table" })?;
    let mut v = AVec::with_capacity_in(list.len(), alloc);
    v.extend_from_slice(list);
    Ok(v)
}

/// Every state `roots` need that is not laid, laid out, and each of those coroutines built into
/// its state; its three functions go to `stash`. `asking` is the fn whose instance asked (what a
/// refusal names besides the cycle).
///
/// Three phases, so nothing is recorded before every owner is known checked:
/// 1. ORDER, by rounds, types only: round 1 orders `roots` and what each state holds by value
///    (`layout_order`); round k+1 orders the coroutines the bodies of round k's coroutines USE by
///    value without holding them across a suspension (a future made and dropped before an
///    `.await`, the temporary moved into `Box::pin`): their states are not part of the user's
///    state, but its build lays their slots, so they are laid before it is built. A coroutine
///    held by a state is never a use only (it is in that state's own held list), so the
///    concatenated order lays every state after each one it holds.
type Consts<'a> = &'a [(ItemKey, DataRecord<Global>)];

/// 2. LAY each in that order, the state recorded (`States::record`).
/// 3. BUILD each into its laid state.
#[expect(clippy::too_many_arguments, reason = "run.rs's own per-instance tables, borrowed apart")]
pub(super) fn lay_and_build<'a, P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    consts: Consts<'_>,
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    query_cache: &RefCell<QueryCache>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    stash: &mut Vec<BuiltParts, Global>,
    asking: usize,
    roots: &[CoroutineOf],
    pull_plain: bool,
    alloc: Alloc<'a>,
) -> Result<Lay<'a>, RunError> {
    let (all, laid_all) = match order_and_lay(program, consts, langs, bodies, query_cache, ground, layouts, asking, roots, true, alloc)? {
        Ordered::Laid { all, laid } => (all, laid),
        Ordered::Unchecked(owner) => return Ok(Lay::Unchecked(owner)),
    };
    let unchecked: Cell<Option<ItemKey>> = Cell::new(None);
    let owner_of = |c: CoroutineOf| -> Option<usize> {
        let ci = program.fn_index(c.owner)?;
        if bodies.checked.get(ci).and_then(Option::as_ref).is_none() {
            unchecked.set(Some(c.owner));
            return None;
        }
        Some(ci)
    };
    // Ensure every coroutine owner's constants (promoted borrows, const blocks, generic consts,
    // and plain constants when `pull_plain`) are in `bodies.const_at` before `build_laid` walks
    // its body: a callee coroutine laid and built ahead of its own owner fn has not been pulled
    // by the caller's own `pull_constants`.
    if program.evaluates_at_args() {
        let mut seen: AVec<'_, (ItemKey, List)> = AVec::new_in(alloc);
        let mut missing: AVec<'a, (ItemKey, List)> = AVec::new_in(alloc);
        let mut needs = crate::front2::const_at::Needs::new_in(alloc);
        for &c in all.iter() {
            let key = (c.owner, c.owner_args);
            if seen.iter().any(|&x| x == key) {
                continue;
            }
            seen.push(key);
            let Some(ci) = owner_of(c) else {
                forget_laid(bodies, &all, ground.view());
                return Ok(Lay::Unchecked(unchecked.get().unwrap_or(c.owner)));
            };
            let function = program.function(ci);
            let (Some((body, _)), Some(done)) = (function.body, bodies.checked.get(ci).and_then(Option::as_ref)) else {
                continue;
            };
            let has_plain = pull_plain
                && (done.expr_defs.column::<1>().iter().any(|d| matches!(d, blaze_typeck::Def::Const(_)))
                    || done.pat_defs.column::<1>().iter().any(|d| matches!(d, blaze_typeck::Def::Const(_))));
            let has_any = !done.const_args.is_empty()
                || !done.pat_const_args.is_empty()
                || !done.default_fields.is_empty()
                || !done.promoted.is_empty()
                || body.exprs.iter().any(|x| matches!(x, blaze_typeck::Expr::ConstBlock { .. }))
                || has_plain;
            if !has_any {
                continue;
            }
            let view = body.view(program.body_types(ci));
            let adts = CachedFront { front: FrontAdts(program, ci, consts, langs, bodies), cache: query_cache };
            let args = match args_of(ground, c, alloc) {
                Ok(a) => a,
                Err(error) => {
                    forget_laid(bodies, &all, ground.view());
                    return Err(build_error(program, function.named(), error, None));
                }
            };
            let res = crate::front2::const_at::needs_of(&view, done, &args, ground, adts.facts(), alloc, &mut needs)
                .and_then(|()| if pull_plain { crate::front2::const_at::plain_needs_of(done, &mut needs) } else { Ok(()) });
            if let Err(error) = res {
                forget_laid(bodies, &all, ground.view());
                return Err(build_error(program, function.named(), error, None));
            }
            if needs.iter().any(|(need, a)| bodies.const_at.get(need, a).is_none()) {
                missing.push(key);
            }
        }
        if !missing.is_empty() {
            forget_laid(bodies, &all, ground.view());
            return Ok(Lay::PullConstants(missing));
        }
    }
    // 3. BUILD. A build that stops (a refusal, or a `Missing` the run makes and steps again)
    // forgets every state recorded above and not built into `stash`: one left recorded would be
    // skipped as made by the next lay, its functions never built.
    let before = stash.len();
    let out = build_laid(program, consts, langs, bodies, query_cache, ground, stash, &all, &laid_all, &owner_of, &unchecked, alloc);
    if !matches!(out, Ok(Lay::Done)) {
        forget_laid(bodies, &all[stash.len() - before..], ground.view());
    }
    out
}

/// On-demand single-coroutine lay & build for `make_one`: lays the `StateShape` of `of`, any
/// unlaid coroutines held by value inside `of`, and any unlaid coroutines used by `of`'s own
/// body (types only, never building child coroutines' LIR), then pulls constants and builds LIR
/// for **only `of`** into `stash`. Child coroutines are built lazily on their own first call.
#[expect(clippy::too_many_arguments, reason = "run/make_one's per-instance tables, borrowed apart")]
pub(super) fn lay_and_build_one<'a, P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    consts: Consts<'_>,
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    query_cache: &RefCell<QueryCache>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    stash: &mut Vec<BuiltParts, Global>,
    asking: usize,
    of: CoroutineOf,
    pull_plain: bool,
    alloc: Alloc<'a>,
) -> Result<Lay<'a>, RunError> {
    let (all, laid_all) = match order_and_lay(program, consts, langs, bodies, query_cache, ground, layouts, asking, &[of], false, alloc)? {
        Ordered::Laid { all, laid } => (all, laid),
        Ordered::Unchecked(owner) => return Ok(Lay::Unchecked(owner)),
    };
    let Some(of_pos) = all.iter().position(|&c| c == of) else {
        return Err(refuse(program.function(asking).named(), "a coroutine whose own lay produced no state entry"));
    };
    let of_laid = &laid_all[of_pos..of_pos + 1];
    let unchecked: Cell<Option<ItemKey>> = Cell::new(None);
    let owner_of = |c: CoroutineOf| -> Option<usize> {
        let ci = program.fn_index(c.owner)?;
        if bodies.checked.get(ci).and_then(Option::as_ref).is_none() {
            unchecked.set(Some(c.owner));
            return None;
        }
        Some(ci)
    };
    let Some(ci) = owner_of(of) else {
        forget_laid(bodies, &[of], ground.view());
        return Ok(Lay::Unchecked(unchecked.get().unwrap_or(of.owner)));
    };
    // Lay `StateShape`s (types only) of any coroutines `of`'s own body creates or references
    // without holding them in `of`'s state (e.g. a future dropped before `.await` or boxed).
    let uses = {
        let adts = CachedFront { front: FrontAdts(program, ci, consts, langs, bodies), cache: query_cache };
        let args = match args_of(ground, of, alloc) {
            Ok(a) => a,
            Err(error) => {
                forget_laid(bodies, &[of], ground.view());
                return Err(build_error(program, program.function(ci).named(), error, None));
            }
        };
        match roots_of_instance(program, bodies, &adts, ci, &args, None, true, &mut *ground, alloc) {
            Ok(u) => u,
            Err(err) => {
                forget_laid(bodies, &[of], ground.view());
                return Err(err);
            }
        }
    };
    if !uses.is_empty() {
        match order_and_lay(program, consts, langs, bodies, query_cache, ground, layouts, asking, &uses, false, alloc) {
            Ok(Ordered::Laid { .. }) => {}
            Ok(Ordered::Unchecked(owner)) => {
                forget_laid(bodies, &[of], ground.view());
                return Ok(Lay::Unchecked(owner));
            }
            Err(err) => {
                forget_laid(bodies, &[of], ground.view());
                return Err(err);
            }
        }
    }
    if program.evaluates_at_args() {
        let function = program.function(ci);
        if let (Some((body, _)), Some(done)) = (function.body, bodies.checked.get(ci).and_then(Option::as_ref)) {
            let has_plain = pull_plain
                && (done.expr_defs.column::<1>().iter().any(|d| matches!(d, blaze_typeck::Def::Const(_)))
                    || done.pat_defs.column::<1>().iter().any(|d| matches!(d, blaze_typeck::Def::Const(_))));
            let has_any = !done.const_args.is_empty()
                || !done.pat_const_args.is_empty()
                || !done.default_fields.is_empty()
                || !done.promoted.is_empty()
                || body.exprs.iter().any(|x| matches!(x, blaze_typeck::Expr::ConstBlock { .. }))
                || has_plain;
            if has_any {
                let view = body.view(program.body_types(ci));
                let adts = CachedFront { front: FrontAdts(program, ci, consts, langs, bodies), cache: query_cache };
                let args = match args_of(ground, of, alloc) {
                    Ok(a) => a,
                    Err(error) => {
                        forget_laid(bodies, &[of], ground.view());
                        return Err(build_error(program, function.named(), error, None));
                    }
                };
                let mut needs = crate::front2::const_at::Needs::new_in(alloc);
                let res = crate::front2::const_at::needs_of(&view, done, &args, ground, adts.facts(), alloc, &mut needs)
                    .and_then(|()| if pull_plain { crate::front2::const_at::plain_needs_of(done, &mut needs) } else { Ok(()) });
                if let Err(error) = res {
                    forget_laid(bodies, &[of], ground.view());
                    return Err(build_error(program, function.named(), error, None));
                }
                if needs.iter().any(|(need, a)| bodies.const_at.get(need, a).is_none()) {
                    forget_laid(bodies, &[of], ground.view());
                    let mut missing: AVec<'a, (ItemKey, List)> = AVec::with_capacity_in(1, alloc);
                    missing.push((of.owner, of.owner_args));
                    return Ok(Lay::PullConstants(missing));
                }
            }
        }
    }
    let out = build_laid(program, consts, langs, bodies, query_cache, ground, stash, &[of], of_laid, &owner_of, &unchecked, alloc);
    if !matches!(out, Ok(Lay::Done)) {
        forget_laid(bodies, &[of], ground.view());
    }
    out
}

/// What a verification's lay did ([`relay`]).
pub(super) enum Relaid<'a> {
    /// `recorded`: the states this lay recorded, none built, which the caller forgets once it
    /// has asked ([`forget_laid`]: a later make lays and builds them); `laid`: each root's full
    /// lay, digested as the build digested it (code_reads.rs `laid_answer`).
    Done { recorded: AVec<'a, CoroutineOf>, laid: AVec<'a, (CoroutineOf, ContentId)> },
    /// As [`Lay::Unchecked`]: check that owner, lay again.
    Unchecked(ItemKey),
}

/// The verifier's lay (run/make_one.rs `verify_instance`): phases 1 and 2 of
/// [`lay_and_build`] over `roots` (the coroutines a read list's `Laid`, `CoroutineState` and
/// `CoroutineNiche` rows name), so the states are recorded as the build's lay recorded them and
/// a row is asked of the same facts; then each root laid once more in full ([`lay_one`], what
/// a part was built into) and digested. Nothing is built: the build's lay is the one that
/// builds. Cost: one lay per state the rows name and per state those hold, types only.
#[expect(clippy::too_many_arguments, reason = "the lay's tables, borrowed apart")]
pub(super) fn relay<'a, P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    query_cache: &RefCell<QueryCache>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    asking: usize,
    roots: &[CoroutineOf],
    alloc: Alloc<'a>,
) -> Result<Relaid<'a>, RunError> {
    let recorded = match order_and_lay(program, &[], langs, bodies, query_cache, ground, layouts, asking, roots, false, alloc)? {
        Ordered::Laid { all, .. } => all,
        Ordered::Unchecked(owner) => return Ok(Relaid::Unchecked(owner)),
    };
    let mut laid: AVec<'a, (CoroutineOf, ContentId)> = AVec::with_capacity_in(roots.len(), alloc);
    for &c in roots {
        let digest = match program.fn_index(c.owner) {
            Some(ci) if bodies.checked.get(ci).and_then(Option::as_ref).is_some() => {
                let adts = CachedFront { front: FrontAdts(program, ci, &[], langs, bodies), cache: query_cache };
                lay_one(program, bodies, &adts, ci, c, &mut *ground, layouts, alloc).map(|(state, names)| laid_answer(&state, &names[..]))
            }
            _ => Err(not_answered()),
        };
        match digest {
            Ok(d) => laid.push((c, d)),
            Err(error) => {
                forget_laid(bodies, &recorded, ground.view());
                return Err(build_error(program, program.function(asking).named(), error, None));
            }
        }
    }
    Ok(Relaid::Done { recorded, laid })
}

/// How a types-only lay ended ([`lay_only`]: for constant evaluation before the build).
pub(super) enum LaidOnly<'a> {
    /// Every state laid and recorded in `bodies.coroutines.states`; the caller must forget these
    /// states with [`forget_laid_in`] once constant evaluation finishes so the subsequent build
    /// still lays and builds their functions into `stash`.
    Done(AVec<'a, CoroutineOf>),
    /// The walk met a coroutine whose owner fn is not checked yet: check it, lay again.
    Unchecked(ItemKey),
}

/// Phases 1 (ORDER) and 2 (LAY) without Phase 3 (BUILD): lays out the coroutine states needed by
/// constant evaluation (`const_at::pull`: `<{coroutine} as SizedTypeProperties>::LAYOUT` under
/// `Box::new` / `Box::pin`) using the exact same `order_and_lay` / `lay_one` as the build.
#[expect(clippy::too_many_arguments, reason = "the lay's tables, borrowed apart")]
pub(super) fn lay_only<'a, P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    consts: Consts<'_>,
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    query_cache: &RefCell<QueryCache>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    asking: usize,
    roots: &[CoroutineOf],
    alloc: Alloc<'a>,
) -> Result<LaidOnly<'a>, RunError> {
    match order_and_lay(program, consts, langs, bodies, query_cache, ground, layouts, asking, roots, false, alloc)? {
        Ordered::Laid { all, .. } => Ok(LaidOnly::Done(all)),
        Ordered::Unchecked(owner) => Ok(LaidOnly::Unchecked(owner)),
    }
}

/// Forgets the states of `laid` (`States::forget`): recorded by a lay that built none of them.
pub(super) fn forget_laid(bodies: &Bodies<'_>, laid: &[CoroutineOf], ground: PTys<'_>) {
    forget_laid_in(&bodies.coroutines, laid, ground);
}

/// Like [`forget_laid`], taking `&Coroutines` directly.
pub(super) fn forget_laid_in(coroutines: &crate::front2::coroutines::Coroutines, laid: &[CoroutineOf], ground: PTys<'_>) {
    for &c in laid {
        if let Ok(args) = ground.list(c.owner_args) {
            coroutines.states.forget(c.owner, c.ordinal, args);
        }
    }
}

/// Phases 1 and 2 of [`lay_and_build`]'s answer.
enum Ordered<'a> {
    /// Every state laid, in order, each recorded.
    Laid { all: AVec<'a, CoroutineOf>, laid: AVec<'a, (LaidState, AVec<'a, StateName>)> },
    Unchecked(ItemKey),
}

/// Phases 1 (ORDER) and 2 (LAY) of [`lay_and_build`]. A lay that stops forgets the states it
/// recorded: one left recorded would be taken as made by the next lay, never built.
#[expect(clippy::too_many_arguments, reason = "lay_and_build's tables, borrowed apart")]
fn order_and_lay<'a, P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    consts: Consts<'_>,
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    query_cache: &RefCell<QueryCache>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    asking: usize,
    roots: &[CoroutineOf],
    include_uses: bool,
    alloc: Alloc<'a>,
) -> Result<Ordered<'a>, RunError> {
    use crate::front2::coroutine_needs::MOST_HELD;
    let states = &bodies.coroutines.states;
    let unchecked: Cell<Option<ItemKey>> = Cell::new(None);
    // The owner's index and checked body of coroutine `c`, or `None` (and `unchecked` set).
    let owner_of = |c: CoroutineOf| -> Option<usize> {
        let Some(ci) = program.fn_index(c.owner) else {
            unchecked.set(Some(c.owner));
            return None;
        };
        if bodies.checked.get(ci).and_then(Option::as_ref).is_none() {
            unchecked.set(Some(c.owner));
            return None;
        }
        Some(ci)
    };
    let asking_name = || program.function(asking).named();
    // 1. ORDER.
    let mut all: AVec<'a, CoroutineOf> = AVec::new_in(alloc);
    let mut pending: AVec<'a, CoroutineOf> = AVec::with_capacity_in(roots.len(), alloc);
    pending.extend_from_slice(roots);
    let mut rounds = 0usize;
    while !pending.is_empty() {
        rounds += 1;
        if rounds > MOST_HELD {
            return Err(refuse(asking_name(), "more than 4096 rounds of coroutine states used by one another's bodies"));
        }
        let order = {
            let all = &all;
            let mut held_by = |c: CoroutineOf| -> Result<Option<AVec<'a, CoroutineOf>>, BuildError> {
                if all.contains(&c) || ground.view().list(c.owner_args).is_ok_and(|a| states.made(c.owner, c.ordinal, a)) {
                    return Ok(None);
                }
                let Some(ci) = owner_of(c) else { return Err(not_answered()) };
                let adts = CachedFront { front: FrontAdts(program, ci, consts, langs, bodies), cache: query_cache };
                let types = held_types(program, bodies, &adts, ci, c, &mut *ground, alloc)?;
                held_coroutines(&mut *ground, &types, adts.facts(), alloc).map(Some)
            };
            layout_order(&pending, &mut held_by, alloc)
        };
        let order = match order {
            Ok(o) => o,
            Err(_) if let Some(owner) = unchecked.get() => return Ok(Ordered::Unchecked(owner)),
            Err(OrderError::HoldsItself(cycle)) => {
                let mut path = std::string::String::new();
                for (n, c) in cycle.iter().enumerate() {
                    if n > 0 {
                        path.push_str(" -> ");
                    }
                    match program.item_named(c.owner) {
                        Some(p) => path.push_str(&p),
                        None => path.push_str(&std::format!("{:#x}", c.owner.0)),
                    }
                }
                let item = cycle.first().map_or(program.function(asking).key, |c| c.owner);
                let error = BuildError::UnsupportedSig { what: HOLDS_ITSELF };
                return Err(RunError::ItemRefused { function: asking_name(), error, item, path: Some(path.into_boxed_str()) });
            }
            Err(OrderError::Build(error)) => return Err(build_error(program, asking_name(), error, None)),
        };
        pending.clear();
        if include_uses {
            for &c in order.iter() {
                let Some(ci) = owner_of(c) else {
                    return Ok(Ordered::Unchecked(unchecked.get().unwrap_or(c.owner)));
                };
                let adts = CachedFront { front: FrontAdts(program, ci, consts, langs, bodies), cache: query_cache };
                let args = args_of(ground, c, alloc).map_err(|error| build_error(program, program.function(ci).named(), error, None))?;
                let uses = roots_of_instance(program, bodies, &adts, ci, &args, None, true, &mut *ground, alloc)?;
                for &u in uses.iter() {
                    if !all.contains(&u) && !order.contains(&u) && !pending.contains(&u) {
                        pending.push(u);
                    }
                }
            }
        }
        all.extend_from_slice(&order);
        if all.len() > MOST_HELD {
            return Err(refuse(asking_name(), "more than 4096 coroutine states laid for one instance"));
        }
    }
    // 2. LAY.
    let mut laid_all: AVec<'a, (LaidState, AVec<'a, StateName>)> = AVec::with_capacity_in(all.len(), alloc);
    for (n, &c) in all.iter().enumerate() {
        let Some(ci) = owner_of(c) else {
            forget_laid(bodies, &all[..n], ground.view());
            return Ok(Ordered::Unchecked(unchecked.get().unwrap_or(c.owner)));
        };
        let function = program.function(ci);
        let at_owner = |error| build_error(program, function.named(), error, None);
        let adts = CachedFront { front: FrontAdts(program, ci, consts, langs, bodies), cache: query_cache };
        let laid = match lay_one(program, bodies, &adts, ci, c, &mut *ground, layouts, alloc).and_then(|laid| Ok((args_of(ground, c, alloc)?, laid))) {
            Ok((args, laid)) => {
                states.record(c.owner, c.ordinal, &args, laid.0.shape);
                laid
            }
            Err(error) => {
                forget_laid(bodies, &all[..n], ground.view());
                return Err(at_owner(error));
            }
        };
        laid_all.push(laid);
    }
    Ok(Ordered::Laid { all, laid: laid_all })
}

/// Phase 3 of [`lay_and_build`]: each of `all` built into its laid state, in order, its three
/// functions pushed to `stash`.
#[expect(clippy::too_many_arguments, reason = "lay_and_build's tables, borrowed apart")]
fn build_laid<'a, P: FrontEnd>(
    program: &P,
    consts: Consts<'_>,
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    query_cache: &RefCell<QueryCache>,
    ground: &mut PortableTys<'_>,
    stash: &mut Vec<BuiltParts, Global>,
    all: &[CoroutineOf],
    laid_all: &[(LaidState, AVec<'a, StateName>)],
    owner_of: &dyn Fn(CoroutineOf) -> Option<usize>,
    unchecked: &Cell<Option<ItemKey>>,
    alloc: Alloc<'a>,
) -> Result<Lay<'a>, RunError> {
    for (&c, (laid, rows)) in all.iter().zip(laid_all.iter()) {
        let Some(ci) = owner_of(c) else {
            return Ok(Lay::Unchecked(unchecked.get().unwrap_or(c.owner)));
        };
        let function = program.function(ci);
        let at_owner = |error| build_error(program, function.named(), error, None);
        let adts = CachedFront { front: FrontAdts(program, ci, consts, langs, bodies), cache: query_cache };
        let args =args_of(ground, c, alloc).map_err(at_owner)?;
        let (Some((body, _)), Some(done)) = (function.body, bodies.checked.get(ci).and_then(Option::as_ref)) else {
            return Err(refuse(function.named(), "an async block whose owner fn has no checked body"));
        };
        let view = body.view(program.body_types(ci));
        // The flow's saved locals by identity (async-P8-4): what the build names its slots by.
        let names = StateNames::new(&rows[..], laid.region).map_err(at_owner)?;
        let plan = StatePlan { laid: *laid, names, plant: StatePlant::None };
        // Every question of the coroutine's build logged (F20), apart: its three parts' makes
        // each take these rows (BuiltParts::reads).
        let reads = ReadLog::new(ReadPlant::None);
        let built = {
            let recorded = Recorded::new(&adts, &reads);
            let mut subst = Subst { args: &args, ground: &mut *ground, scratch: alloc };
            blaze_build::build_coroutine(&view, done, c.ordinal, &mut subst, &recorded, Some(plan), Global)
        };
        let built = built.map_err(|error| {
            let range = match error {
                BuildError::Unsupported { expr, .. } | BuildError::UnsupportedType { expr, .. } => body.expr_ranges.get(expr.0 as usize).copied(),
                BuildError::UnsupportedPat { pat, .. } => body.pat_ranges.get(pat.0 as usize).copied(),
                _ => None,
            };
            build_error(program, function.named(), error, range)
        })?;
        let mut kept = Vec::with_capacity_in(args.len(), Global);
        kept.extend_from_slice(&args);
        let laid = laid_answer(laid, &rows[..]);
        stash.push(BuiltParts { owner: c.owner, ordinal: c.ordinal, args: kept, parts: [Some(built.poll), Some(built.resume), Some(built.drop)], laid, reads });
    }
    Ok(Lay::Done)
}

/// The ground types coroutine `c`'s state holds past its tag: its upvars' tuple, then each
/// saved local of its recipe, at the owner's arguments.
fn held_types<'a, P: FrontEnd>(
    program: &P,
    bodies: &Bodies<'_>,
    adts: &dyn Adts,
    ci: usize,
    c: CoroutineOf,
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, PTy>, BuildError> {
    let function = program.function(ci);
    let (Some((body, _)), Some(done)) = (function.body, bodies.checked.get(ci).and_then(Option::as_ref)) else {
        return Err(not_answered());
    };
    let view = body.view(program.body_types(ci));
    let facts = bodies.coroutines.table.get(c.owner, c.ordinal).ok_or_else(not_answered)?;
    let recipe = match program.coroutine_recipe(c.owner, c.ordinal) {
        Some(Ok(r)) => r,
        Some(Err(refusal)) => return Err(BuildError::CoroutineRecipe { owner: c.owner, ordinal: c.ordinal, refusal }),
        None => return Err(not_answered()),
    };
    let mut args: AVec<'_, PTy> = AVec::new_in(alloc);
    args.extend_from_slice(ground.view().list(c.owner_args).map_err(|_| BuildError::Malformed { what: "a coroutine's arguments past the ground table" })?);
    let mut out: AVec<'a, PTy> = AVec::with_capacity_in(recipe.saved.len() + 1, alloc);
    out.push(blaze_build::ground(facts.types, facts.env, &args, ground, alloc, adts.facts())?);
    let record = done.types.view();
    for &s in recipe.saved.iter() {
        out.push(ground_saved(&view, record, s.ty, s.wrap, &args, ground, adts.facts(), alloc)?);
    }
    Ok(out)
}

/// Coroutine `c`'s state laid out from its held types' layouts (every coroutine among them
/// already laid: the order says so) and its drop flags, with its saved locals' table by
/// identity (blaze-flow-lower identity.rs: what the build names its slots by), sorted.
#[expect(clippy::too_many_arguments, reason = "the lay's tables, borrowed apart")]
fn lay_one<'a, P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    bodies: &Bodies<'_>,
    adts: &dyn Adts,
    ci: usize,
    c: CoroutineOf,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    alloc: Alloc<'a>,
) -> Result<(LaidState, AVec<'a, StateName>), BuildError> {
    let types = held_types(program, bodies, adts, ci, c, &mut *ground, alloc)?;
    let recipe = match program.coroutine_recipe(c.owner, c.ordinal) {
        Some(Ok(r)) => r,
        Some(Err(refusal)) => return Err(BuildError::CoroutineRecipe { owner: c.owner, ordinal: c.ordinal, refusal }),
        None => return Err(not_answered()),
    };
    let points = recipe.points;
    // Needs the lazy program fills (`NoAdt`, `Select(Missing(_))`, `OpaqueOwnerUnchecked`) and
    // named refusals (`Projection`, `Malformed`) pass through verbatim so the instance loop makes
    // what was asked and tries again (or names the real refusal, rule 19).
    let refused = |e: blaze_build::layout::LayoutError| match e.why {
        blaze_build::layout::Refusal::NoAdt(adt) => BuildError::NoAdt { adt },
        blaze_build::layout::Refusal::Ground(error) | blaze_build::layout::Refusal::FieldGround { error, .. } => error,
        blaze_build::layout::Refusal::Missing(m) => BuildError::MissingRecord { item: m.item },
        _ => BuildError::UnsupportedSig { what: "the layout of an upvar or saved local of a coroutine state was refused" },
    };
    let mut sizes: AVec<'_, (u64, u8)> = AVec::with_capacity_in(types.len(), alloc);
    for &t in types.iter() {
        let mut cx = LayoutCx { ground: &mut *ground, facts: adts.facts(), scratch: alloc };
        let id = layouts.of(t, &mut cx).map_err(refused)?;
        let row = layouts.get(id);
        sizes.push((row.size, row.align));
    }
    // One drop flag per saved local whose ground type needs drop (main's ruling (ii)), plus
    // one drop flag per by-value whole-binding upvar whose ground type needs drop (the
    // unresumed path drops upvars if never polled; once polled, the body owns them and may
    // move one conditionally before a suspension).
    let manually_drop = program.lang_named("manually_drop");
    let mut flags: AVec<'_, bool> = AVec::with_capacity_in(types.len(), alloc);
    for &t in types.iter().skip(1) {
        let mut drop_impl = |ty: PTy, g: &mut PortableTys<'_>| adts.drop_impl(ty, g);
        let needs = blaze_build::drop_facts::needs_drop(t, &mut *ground, adts.facts(), manually_drop, &mut drop_impl, alloc)
            .map_err(|e| match e {
                blaze_build::drop_facts::NeedsDropError::NoAdt { adt } => BuildError::NoAdt { adt },
                blaze_build::drop_facts::NeedsDropError::Ask(error) | blaze_build::drop_facts::NeedsDropError::Ground(error) => error,
                blaze_build::drop_facts::NeedsDropError::Malformed { what } => BuildError::Malformed { what },
                _ => BuildError::UnsupportedSig { what: "whether a saved local of a coroutine state needs drop was not answered" },
            })?;
        flags.push(needs);
    }
    let mut upvar_ids: AVec<'_, blaze_flow_lower::identity::Identity> = AVec::new_in(alloc);
    let mut upvar_has_flag: AVec<'_, bool> = AVec::new_in(alloc);
    let mut upvar_has_rest: AVec<'_, bool> = AVec::new_in(alloc);
    let mut partial: Option<AVec<'_, bool>> = None;
    let mut temp_partial: Option<AVec<'_, bool>> = None;
    if let (Some((body, _)), Some(done)) = (program.function(ci).body, bodies.checked.get(ci).and_then(Option::as_ref)) {
        let view = body.view(program.body_types(ci));
        let p_bits = may_partial_move_bindings(&view, done, alloc);
        let t_bits = may_partial_move_temps(&view, alloc);
        if let Some(block) = coroutines_in(&view).find(|x| x.0 == c.ordinal).map(|x| x.1)
            && let Ok(PNode::Tuple(env_list)) = ground.view().node(types[0])
            && let Ok(env_slice) = ground.view().list(env_list)
        {
            let mut env_tys: AVec<'_, PTy> = AVec::with_capacity_in(env_slice.len(), alloc);
            env_tys.extend_from_slice(env_slice);
            for (f, &ty) in done.env_fields_by::<blaze_build::layout::BuildEnv>(block).zip(env_tys.iter()) {
                if !f.is_whole() || f.how != blaze_typeck::CaptureKind::ByValue {
                    continue;
                }
                let mut drop_impl = |ty: PTy, g: &mut PortableTys<'_>| adts.drop_impl(ty, g);
                let needs = blaze_build::drop_facts::needs_drop(ty, &mut *ground, adts.facts(), manually_drop, &mut drop_impl, alloc)
                    .map_err(|e| match e {
                        blaze_build::drop_facts::NeedsDropError::NoAdt { adt } => BuildError::NoAdt { adt },
                        blaze_build::drop_facts::NeedsDropError::Ask(error) | blaze_build::drop_facts::NeedsDropError::Ground(error) => error,
                        blaze_build::drop_facts::NeedsDropError::Malformed { what } => BuildError::Malformed { what },
                        _ => BuildError::UnsupportedSig { what: "whether an upvar of a coroutine state needs drop was not answered" },
                    })?;
                let needs_rest = p_bits.get(f.base.ix()).copied().unwrap_or(false);
                if needs || needs_rest {
                    let id = blaze_flow_lower::identity::Identity::Binding(f.base);
                    if !recipe.saved.iter().any(|s| s.identity == id) && !upvar_ids.iter().any(|&x| x == id) {
                        upvar_ids.push(id);
                        upvar_has_flag.push(needs);
                        upvar_has_rest.push(needs_rest);
                    }
                }
            }
        }
        partial = Some(p_bits);
        temp_partial = Some(t_bits);
    }
    let (env, saved) = match sizes.split_first() {
        Some((env, saved)) => (*env, saved),
        None => return Err(BuildError::Malformed { what: "a coroutine state with no upvars' row" }),
    };
    if saved.len() != recipe.saved.len() {
        return Err(BuildError::Malformed { what: "a coroutine state's held types not one per saved local of its recipe" });
    }
    let mut needs_rest: AVec<'_, bool> = AVec::with_capacity_in(recipe.saved.len(), alloc);
    for (s, &needs_flag) in recipe.saved.iter().zip(flags.iter()) {
        let want = match s.identity {
            blaze_flow_lower::identity::Identity::Binding(p) => partial.as_deref().and_then(|bits| bits.get(p.ix()).copied()).unwrap_or(false),
            blaze_flow_lower::identity::Identity::Temp(e, _) => needs_flag && temp_partial.as_deref().and_then(|bits| bits.get(e.ix()).copied()).unwrap_or(false),
            _ => false,
        };
        needs_rest.push(want);
    }
    let conflicts = (!recipe.conflicts.is_empty()).then_some(recipe.conflicts.as_slice());
    let laid = lay_state_with_conflicts(env, saved, conflicts, &flags, &needs_rest, &upvar_has_flag, &upvar_has_rest, points, alloc, alloc)?;
    // Every size fits a u32: the state does (`lay_state` refused past 4 GiB).
    let mut rows: AVec<'a, StateName> = AVec::with_capacity_in(saved.len() + upvar_ids.len(), alloc);
    for (s, (&offset, (&(size, _), (&flag, &rest)))) in recipe
        .saved
        .iter()
        .zip(laid.field.iter().zip(saved.iter().zip(laid.flag.iter().zip(laid.rest.iter()))))
    {
        rows.push(StateName { identity: s.identity, offset, size: u32::try_from(size).unwrap_or(u32::MAX), flag, rest });
    }
    for (&id, (&has_flag, (&uflag, &rest))) in upvar_ids
        .iter()
        .zip(upvar_has_flag.iter().zip(laid.upvar_flag.iter().zip(laid.upvar_rest.iter())))
    {
        let flag = has_flag.then_some(uflag);
        let offset = flag.or(rest).unwrap_or(laid.state.region.start);
        rows.push(StateName { identity: id, offset, size: 0, flag, rest });
    }
    // Unique keys (state_recipe.rs refused a saved identity twice): an unstable sort is
    // deterministic.
    rows.sort_unstable_by_key(|r| r.identity);
    Ok((laid.state, rows))
}

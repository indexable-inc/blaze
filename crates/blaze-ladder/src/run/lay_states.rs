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

use core::cell::Cell;

use blaze_build::coroutine_state::{LaidState, StateName, StateNames, StatePlan, StatePlant, lay_state};
use blaze_build::layout::{LayoutCx, Layouts};
use blaze_build::{CoroutineOf, coroutine_identity, coroutines_in};
use blaze_own::AVec;
use blaze_typeck::BodyView;

use super::*;
use crate::code_records::reads::{ReadLog, ReadPlant, Recorded, laid_answer};
use crate::front2::coroutine_needs::{HOLDS_ITSELF, OrderError, held_coroutines, layout_order};

/// How a lay ended.
pub(super) enum Lay {
    /// Every state laid and every coroutine built (its three functions in the stash).
    Done,
    /// The walk met a coroutine whose owner fn is not checked yet: check it, lay again.
    Unchecked(ItemKey),
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
    // In its own body's record a block is named by its expression (`PNode::Future`); its
    // identity is `PNode::Coroutine` (blaze-build coroutine.rs), its output a fact of that.
    let own = match record.node(ty) {
        Ok(PNode::Future(e, ..)) => Some(e),
        _ => None,
    };
    let g = match own {
        Some(e) => {
            let ordinal = coroutines_in(view)
                .find(|c| c.1 == e)
                .map(|c| c.0)
                .ok_or(BuildError::Malformed { what: "a saved local's future that is not an async block of its body" })?;
            let list = ground.intern_list(args).map_err(full)?;
            coroutine_identity(ground, view.owner, ordinal, list)?
        }
        None => blaze_build::ground(record, ty, args, ground, scratch, facts)?,
    };
    match wrap {
        None => Ok(g),
        Some(m) => ground.intern(PNode::Ref(m, g)).map_err(full),
    }
}

/// The coroutines instance `(i, args)`'s body types hold by value (the record's expression and
/// local types, grounded; the module doc says why a refused one is not a root).
pub(super) fn roots_of_instance<P: FrontEnd>(
    program: &P,
    bodies: &Bodies<'_>,
    adts: &dyn Adts,
    i: usize,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'static>,
) -> Result<AVec<'static, CoroutineOf>, RunError> {
    let name = || program.function(i).named();
    let (Some((body, _)), Some(done)) = (program.function(i).body, bodies.checked.get(i).and_then(Option::as_ref)) else {
        return Ok(AVec::new_in(alloc));
    };
    let view = body.view(program.body_types(i));
    let record = done.types.view();
    let mut tys: AVec<'_, PTy> = AVec::new_in(alloc);
    tys.extend_from_slice(&done.expr_ty);
    tys.extend_from_slice(&done.local_ty);
    tys.sort_unstable();
    tys.dedup();
    let mut roots: AVec<'_, PTy> = AVec::new_in(alloc);
    for &t in tys.iter() {
        if let Ok(g) = ground_saved(&view, record, t, None, args, ground, adts.facts(), alloc) {
            roots.push(g);
        }
    }
    held_coroutines(ground, &roots, adts.facts(), alloc).map_err(|error| build_error(program, name(), error, None))
}

/// The ground arguments of coroutine `c`'s owner, copied out of the ground table.
fn args_of(ground: &PortableTys<'_>, c: CoroutineOf, alloc: Alloc<'static>) -> Result<AVec<'static, PTy>, BuildError> {
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
/// 2. LAY each in that order, the state recorded (`States::record`).
/// 3. BUILD each into its laid state.
#[expect(clippy::too_many_arguments, reason = "run.rs's own per-instance tables, borrowed apart")]
pub(super) fn lay_and_build<P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    consts: &[(ItemKey, DataRecord<Global>)],
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    stash: &mut Vec<BuiltParts, Global>,
    asking: usize,
    roots: &[CoroutineOf],
    alloc: Alloc<'static>,
) -> Result<Lay, RunError> {
    let (all, laid_all) = match order_and_lay(program, consts, langs, bodies, ground, layouts, asking, roots, alloc)? {
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
    // 3. BUILD. A build that stops (a refusal, or a `Missing` the run makes and steps again)
    // forgets every state recorded above and not built into `stash`: one left recorded would be
    // skipped as made by the next lay, its functions never built.
    let before = stash.len();
    let out = build_laid(program, consts, langs, bodies, ground, stash, &all, &laid_all, &owner_of, &unchecked, alloc);
    if !matches!(out, Ok(Lay::Done)) {
        forget_laid(bodies, &all[stash.len() - before..], ground.view());
    }
    out
}

/// What a verification's lay did ([`relay`]).
pub(super) enum Relaid {
    /// `recorded`: the states this lay recorded, none built, which the caller forgets once it
    /// has asked ([`forget_laid`]: a later make lays and builds them); `laid`: each root's full
    /// lay, digested as the build digested it (code_reads.rs `laid_answer`).
    Done { recorded: AVec<'static, CoroutineOf>, laid: AVec<'static, (CoroutineOf, ContentId)> },
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
pub(super) fn relay<P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    asking: usize,
    roots: &[CoroutineOf],
    alloc: Alloc<'static>,
) -> Result<Relaid, RunError> {
    let recorded = match order_and_lay(program, &[], langs, bodies, ground, layouts, asking, roots, alloc)? {
        Ordered::Laid { all, .. } => all,
        Ordered::Unchecked(owner) => return Ok(Relaid::Unchecked(owner)),
    };
    let mut laid: AVec<'static, (CoroutineOf, ContentId)> = AVec::with_capacity_in(roots.len(), alloc);
    for &c in roots {
        let digest = match program.fn_index(c.owner) {
            Some(ci) if bodies.checked.get(ci).and_then(Option::as_ref).is_some() => {
                let adts = FrontAdts(program, ci, &[], langs, bodies);
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

/// Forgets the states of `laid` (`States::forget`): recorded by a lay that built none of them.
pub(super) fn forget_laid(bodies: &Bodies<'_>, laid: &[CoroutineOf], ground: PTys<'_>) {
    for &c in laid {
        if let Ok(args) = ground.list(c.owner_args) {
            bodies.coroutines.states.forget(c.owner, c.ordinal, args);
        }
    }
}

/// Phases 1 and 2 of [`lay_and_build`]'s answer.
enum Ordered {
    /// Every state laid, in order, each recorded.
    Laid { all: AVec<'static, CoroutineOf>, laid: AVec<'static, (LaidState, AVec<'static, StateName>)> },
    Unchecked(ItemKey),
}

/// Phases 1 (ORDER) and 2 (LAY) of [`lay_and_build`]. A lay that stops forgets the states it
/// recorded: one left recorded would be taken as made by the next lay, never built.
#[expect(clippy::too_many_arguments, reason = "lay_and_build's tables, borrowed apart")]
fn order_and_lay<P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    consts: &[(ItemKey, DataRecord<Global>)],
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    asking: usize,
    roots: &[CoroutineOf],
    alloc: Alloc<'static>,
) -> Result<Ordered, RunError> {
    use crate::front2::coroutine_needs::MOST_HELD;
    let states = &bodies.coroutines.states;
    let unchecked: Cell<Option<ItemKey>> = Cell::new(None);
    // The owner's index and checked body of coroutine `c`, or `None` (and `unchecked` set).
    let owner_of = |c: CoroutineOf| -> Option<usize> {
        let ci = program.fn_index(c.owner)?;
        if bodies.checked.get(ci).and_then(Option::as_ref).is_none() {
            unchecked.set(Some(c.owner));
            return None;
        }
        Some(ci)
    };
    let asking_name = || program.function(asking).named();
    // 1. ORDER.
    let mut all: AVec<'static, CoroutineOf> = AVec::new_in(alloc);
    let mut pending: AVec<'static, CoroutineOf> = AVec::with_capacity_in(roots.len(), alloc);
    pending.extend_from_slice(roots);
    let mut rounds = 0usize;
    while !pending.is_empty() {
        rounds += 1;
        if rounds > MOST_HELD {
            return Err(refuse(asking_name(), "more than 4096 rounds of coroutine states used by one another's bodies"));
        }
        let order = {
            let all = &all;
            let mut held_by = |c: CoroutineOf| -> Result<Option<AVec<'static, CoroutineOf>>, BuildError> {
                if all.contains(&c) || ground.view().list(c.owner_args).is_ok_and(|a| states.made(c.owner, c.ordinal, a)) {
                    return Ok(None);
                }
                let Some(ci) = owner_of(c) else { return Err(not_answered()) };
                let adts = FrontAdts(program, ci, consts, langs, bodies);
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
        for &c in order.iter() {
            let Some(ci) = owner_of(c) else {
                return Ok(Ordered::Unchecked(unchecked.get().unwrap_or(c.owner)));
            };
            let adts = FrontAdts(program, ci, consts, langs, bodies);
            let args = args_of(ground, c, alloc).map_err(|error| build_error(program, program.function(ci).named(), error, None))?;
            let uses = roots_of_instance(program, bodies, &adts, ci, &args, &mut *ground, alloc)?;
            for &u in uses.iter() {
                if !all.contains(&u) && !order.contains(&u) && !pending.contains(&u) {
                    pending.push(u);
                }
            }
        }
        all.extend_from_slice(&order);
        if all.len() > MOST_HELD {
            return Err(refuse(asking_name(), "more than 4096 coroutine states laid for one instance"));
        }
    }
    // 2. LAY.
    let mut laid_all: AVec<'static, (LaidState, AVec<'static, StateName>)> = AVec::with_capacity_in(all.len(), alloc);
    for (n, &c) in all.iter().enumerate() {
        let Some(ci) = owner_of(c) else {
            forget_laid(bodies, &all[..n], ground.view());
            return Ok(Ordered::Unchecked(unchecked.get().unwrap_or(c.owner)));
        };
        let function = program.function(ci);
        let at_owner = |error| build_error(program, function.named(), error, None);
        let adts = FrontAdts(program, ci, consts, langs, bodies);
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
fn build_laid<P: FrontEnd>(
    program: &P,
    consts: &[(ItemKey, DataRecord<Global>)],
    langs: &BackLangs,
    bodies: &Bodies<'_>,
    ground: &mut PortableTys<'_>,
    stash: &mut Vec<BuiltParts, Global>,
    all: &[CoroutineOf],
    laid_all: &[(LaidState, AVec<'static, StateName>)],
    owner_of: &dyn Fn(CoroutineOf) -> Option<usize>,
    unchecked: &Cell<Option<ItemKey>>,
    alloc: Alloc<'static>,
) -> Result<Lay, RunError> {
    for (&c, (laid, rows)) in all.iter().zip(laid_all.iter()) {
        let Some(ci) = owner_of(c) else {
            return Ok(Lay::Unchecked(unchecked.get().unwrap_or(c.owner)));
        };
        let function = program.function(ci);
        let at_owner = |error| build_error(program, function.named(), error, None);
        let adts = FrontAdts(program, ci, consts, langs, bodies);
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
fn held_types<P: FrontEnd>(
    program: &P,
    bodies: &Bodies<'_>,
    adts: &FrontAdts<'_, P>,
    ci: usize,
    c: CoroutineOf,
    ground: &mut PortableTys<'_>,
    alloc: Alloc<'static>,
) -> Result<AVec<'static, PTy>, BuildError> {
    let function = program.function(ci);
    let (Some((body, _)), Some(done)) = (function.body, bodies.checked.get(ci).and_then(Option::as_ref)) else {
        return Err(not_answered());
    };
    let view = body.view(program.body_types(ci));
    let facts = bodies.coroutines.table.get(c.owner, c.ordinal).ok_or_else(not_answered)?;
    let recipe = match program.coroutine_recipe(c.owner, c.ordinal) {
        Some(Ok(r)) => r,
        Some(Err(_)) => return Err(BuildError::UnsupportedSig { what: "an async block whose flow was refused: its state recipe was not made" }),
        None => return Err(not_answered()),
    };
    let mut args: AVec<'_, PTy> = AVec::new_in(alloc);
    args.extend_from_slice(ground.view().list(c.owner_args).map_err(|_| BuildError::Malformed { what: "a coroutine's arguments past the ground table" })?);
    let mut out: AVec<'static, PTy> = AVec::with_capacity_in(recipe.saved.len() + 1, alloc);
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
fn lay_one<P: FrontEnd, L: core::alloc::Allocator + Clone>(
    program: &P,
    bodies: &Bodies<'_>,
    adts: &FrontAdts<'_, P>,
    ci: usize,
    c: CoroutineOf,
    ground: &mut PortableTys<'_>,
    layouts: &mut Layouts<L>,
    alloc: Alloc<'static>,
) -> Result<(LaidState, AVec<'static, StateName>), BuildError> {
    let types = held_types(program, bodies, adts, ci, c, &mut *ground, alloc)?;
    let recipe = match program.coroutine_recipe(c.owner, c.ordinal) {
        Some(Ok(r)) => r,
        _ => return Err(BuildError::UnsupportedSig { what: "an async block whose flow was refused: its state recipe was not made" }),
    };
    let points = recipe.points;
    // An ADT whose row is not made stays `NoAdt` (the need the lazy program fills, then the
    // instance is built again); any other layout refusal is the state's.
    let refused = |e: blaze_build::layout::LayoutError| match e.why {
        blaze_build::layout::Refusal::NoAdt(adt) => BuildError::NoAdt { adt },
        _ => BuildError::UnsupportedSig { what: "the layout of an upvar or saved local of a coroutine state was refused" },
    };
    let mut sizes: AVec<'_, (u64, u8)> = AVec::with_capacity_in(types.len(), alloc);
    for &t in types.iter() {
        let mut cx = LayoutCx { ground: &mut *ground, facts: adts.facts(), scratch: alloc };
        let id = layouts.of(t, &mut cx).map_err(refused)?;
        let row = layouts.get(id);
        sizes.push((row.size, row.align));
    }
    // One drop flag per saved local whose ground type needs drop (main's ruling (ii)); the
    // upvars (types[0]) are dropped by the unresumed path, not flagged.
    let manually_drop = program.lang_named("manually_drop");
    let mut flags: AVec<'_, bool> = AVec::with_capacity_in(types.len(), alloc);
    for &t in types.iter().skip(1) {
        let mut drop_impl = |ty: PTy, g: &mut PortableTys<'_>| adts.drop_impl(ty, g);
        let needs = blaze_build::drop_facts::needs_drop(t, &mut *ground, adts.facts(), manually_drop, &mut drop_impl, alloc)
            .map_err(|e| match e {
                blaze_build::drop_facts::NeedsDropError::NoAdt { adt } => BuildError::NoAdt { adt },
                _ => BuildError::UnsupportedSig { what: "whether a saved local of a coroutine state needs drop was not answered" },
            })?;
        flags.push(needs);
    }
    let (env, saved) = match sizes.split_first() {
        Some((env, saved)) => (*env, saved),
        None => return Err(BuildError::Malformed { what: "a coroutine state with no upvars' row" }),
    };
    if saved.len() != recipe.saved.len() {
        return Err(BuildError::Malformed { what: "a coroutine state's held types not one per saved local of its recipe" });
    }
    let laid = lay_state(env, saved, &flags, points, alloc, alloc)?;
    // Every size fits a u32: the state does (`lay_state` refused past 4 GiB).
    let mut rows: AVec<'static, StateName> = AVec::with_capacity_in(saved.len(), alloc);
    for (i, s) in recipe.saved.iter().enumerate() {
        rows.push(StateName { identity: s.identity, offset: laid.field[i], size: saved[i].0 as u32, flag: laid.flag[i] });
    }
    // Unique keys (state_recipe.rs refused a saved identity twice): an unstable sort is
    // deterministic.
    rows.sort_unstable_by_key(|r| r.identity);
    Ok((laid.state, rows))
}

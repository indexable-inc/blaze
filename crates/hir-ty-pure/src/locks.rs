//! The locks of an instance, a part of its summary ([`crate::walk::Summary::locks`],
//! docs/v2/plan/design/effect-summary.md): the lock classes it takes, the edges "holds A while
//! taking B" of its body and its callees, and the guards its body holds across a call that
//! blocks the thread.
//!
//! A lock's name is a run-time string (`Lock::new("x", ..)`), so the class of an acquire is
//! read from its receiver, as lockdep classes a lock by where it is declared:
//!
//! - `Type.field` where the receiver is a borrow of a field of an ADT (`&self.state`);
//! - the static's path where it is a static (`&STATE`);
//! - else the lock's own type, shown (`std::sync::Mutex<u32>`), which is coarser: two locks of
//!   one type share it.
//!
//! An acquire is a call of a row that takes a lock ([`Rule::Lock`]: std's, `lock_api`'s,
//! `blaze_core::lock`'s). Its destination holds the class, and the class follows a guard through
//! plain moves and the toolchain's calls that hand on what they are given (`Option::unwrap`,
//! `Result::expect`). A function whose return slot holds a classed guard says so
//! ([`Locks::returns`]), so an accessor `fn state(&self) -> Guard` gives its class to its caller.
//!
//! What is live across a call is the MIR's ([`MirBody::held_across_calls_in`]): a guard dropped
//! or moved into the call is not held. A guard lent to the call (`signal.wait(&mut guard)`) is
//! not held across it for [`Locks::across`]: the call releases it while it waits.
//!
//! Every set is sorted and holds at most [`MOST`] entries; past that the summary is
//! [`Locks::wide`], which a lint refuses as unknown (no-fallback), never takes as clean.

use std::sync::Arc;

use base_db::{CrateOrigin, Db};
use hir_def::{AdtId, FunctionId, HasModule as _, ModuleDefId, VariantId};
use hir_ty::{next_solver::TyKind, traits::StoredParamEnvAndCrate};
use hir_ty_mir::mir::{
    BasicBlockId, LocalId, MirBody, MirSpan, Operand, OperandKind, PlaceTy, ProjectionElem,
    Rvalue, StatementKind, StoredPlace, TerminatorKind, dispatch::Body, return_slot,
};
use rustc_type_ir::inherent::IntoKind as _;

use crate::{
    effect_attrs::{self, NONBLOCKING},
    effects::{self, EffectSet, Rule, effect_index},
    function::{Place, Step, generic, instance_path, place, refusal},
    models::effect_row,
    walk::{Cx, Def, Instance, Summary, item_path, summary},
};

/// The most classes, edges or holds one summary keeps.
pub(crate) const MOST: usize = 32;

/// The most calls a chain of a lock finding follows.
const CHAIN: usize = 32;

/// What an instance does with locks, its callees' included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Locks {
    /// The classes it or a callee takes, sorted by class, each once with the site of the
    /// fewest calls to its acquire.
    pub(crate) taken: Box<[Taken]>,
    /// The edges "holds `held` while taking `taken`", sorted, each pair once: its body's own
    /// before a callee's.
    pub(crate) order: Box<[Edge]>,
    /// The guards its own body holds across a call that blocks the thread.
    pub(crate) across: Box<[Across]>,
    /// The class of the guard it returns, if it returns one.
    pub(crate) returns: Option<Box<str>>,
    /// More than [`MOST`] of something: what is missing is unknown.
    pub(crate) wide: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Taken {
    pub(crate) class: Box<str>,
    /// Calls to the acquire: 1 for an acquire of the body's own.
    pub(crate) depth: u16,
    /// The summary's site it is taken through.
    pub(crate) site: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Edge {
    pub(crate) held: Box<str>,
    pub(crate) taken: Box<str>,
    pub(crate) how: EdgeHow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EdgeHow {
    /// The body holds `held`, taken at site `held_at`, across site `taken_at`, which takes
    /// `taken`.
    Own { held_at: u32, taken_at: u32 },
    /// The callee of this site has the edge.
    Site(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Across {
    pub(crate) class: Box<str>,
    /// The site of the guard's acquire.
    pub(crate) held_at: u32,
    /// The site of the call that blocks.
    pub(crate) call: u32,
}

/// A site of the body as this pass reads it: what it reaches, the block whose call it is, and
/// whether it is a closure given to a trusted function (walked for the effects alone).
pub(crate) struct LockSite<'a> {
    pub(crate) instance: &'a Instance,
    pub(crate) call: Option<BasicBlockId>,
    pub(crate) given: bool,
}

/// The function an instance is of, when it is one.
fn func_of(instance: &Instance) -> Option<FunctionId> {
    match instance.def {
        Def::Body(Body::Fn(func)) | Def::Leaf(func, _) => Some(func),
        Def::Body(Body::Closure(_) | Body::Coroutine(_) | Body::CoroutineByMove(_)) => None,
    }
}

/// What a call of `func` counts as by its row, if it has one ([`effects::MODELS`],
/// [`effect_attrs::ROWS`]).
fn row_kinds(db: &Db, func: FunctionId) -> EffectSet {
    if let Some(modeled) = effect_row(db, func) {
        return modeled.kinds;
    }
    effect_attrs::row_of(db, func).map_or(EffectSet::EMPTY, |row| row.kinds())
}

/// Whether a call of `instance` takes a lock.
pub(crate) fn acquires(db: &Db, instance: &Instance) -> bool {
    func_of(instance).is_some_and(|func| row_kinds(db, func).has(Rule::Lock))
}

/// Whether a call of `instance`, whose summary is `callee` (a body's), may block the thread:
/// its row, its leaf, or its summary through no function trusted `nonblocking`.
fn blocks_thread(db: &Db, instance: &Instance, callee: Option<&Summary>) -> bool {
    if func_of(instance).is_some_and(|func| row_kinds(db, func).has(Rule::Block)) {
        return true;
    }
    if let Def::Leaf(_, leaf) = instance.def {
        return effects::leaf_kinds(leaf).0.has(Rule::Block);
    }
    let Some(index) = effect_index(&NONBLOCKING) else { return false };
    callee.is_some_and(|it| it.set(index).has(Rule::Block))
}

fn slot(local: LocalId) -> usize {
    u32::from(local.into_raw()) as usize
}

/// The local an operand reads whole, if it reads one.
fn whole_local(operand: &Operand) -> Option<LocalId> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place)
            if place.as_ref().projection.as_slice().is_empty() =>
        {
            Some(place.local)
        }
        _ => None,
    }
}

/// What `local` is assigned whole: in `block` first (an argument's temporary is made just
/// before its call), else anywhere in the body; the last assignment of the block, the first of
/// the body.
fn defined<'b>(body: &'b MirBody<'_>, block: BasicBlockId, local: LocalId) -> Option<&'b Rvalue> {
    let assigns = |statements: &'b [hir_ty_mir::mir::Statement]| {
        statements.iter().filter_map(move |statement| match &statement.kind {
            StatementKind::Assign(to, rvalue)
                if to.local == local && to.as_ref().projection.as_slice().is_empty() =>
            {
                Some(rvalue)
            }
            _ => None,
        })
    };
    if let Some(it) = assigns(&body.basic_blocks[block].statements).last() {
        return Some(it);
    }
    body.basic_blocks.iter().find_map(|(_, data)| assigns(&data.statements).next())
}

/// The class of a lock borrowed at `place`: `Type.field` for a field of an ADT, the static's
/// path for a static.
fn place_class(cx: &Cx<'_>, body: &MirBody<'_>, block: BasicBlockId, place: &StoredPlace) -> Option<Box<str>> {
    let db = cx.db;
    let place = place.as_ref();
    let elems = place.projection.as_slice();
    if let [prefix @ .., ProjectionElem::Field(field)] = elems {
        let parent = PlaceTy::from_ty(body.locals[place.local].ty.as_ref()).multi_projection_ty(
            &cx.infcx,
            cx.env.param_env,
            prefix,
        );
        let TyKind::Adt(adt, _) = parent.ty.kind() else { return None };
        let adt = adt.def_id();
        let variant = match (parent.variant_id, adt) {
            (Some(it), _) => it,
            (None, AdtId::StructId(it)) => VariantId::StructId(it),
            (None, AdtId::UnionId(it)) => VariantId::UnionId(it),
            (None, AdtId::EnumId(_)) => return None,
        };
        let fields = variant.fields(db).fields();
        let (_, data) = fields.iter().nth(field.0 as usize)?;
        // alloc: a class, once per acquire of a summary
        return Some(
            format!("{}.{}", item_path(db, ModuleDefId::AdtId(adt)), data.name.as_str()).into(),
        );
    }
    if matches!(elems, [] | [ProjectionElem::Deref])
        && let Some(Rvalue::Use(Operand { kind: OperandKind::Static(id), .. })) =
            defined(body, block, place.local)
    {
        return Some(item_path(db, ModuleDefId::StaticId(*id)).into()); // alloc: a class
    }
    None
}

/// The class of the lock the call ending `block` takes, from its receiver `arg`: its place's,
/// else its type, shown.
fn receiver_class<'db>(
    cx: &Cx<'db>,
    body: &MirBody<'db>,
    block: BasicBlockId,
    arg: &Operand,
) -> Box<str> {
    let mut local = whole_local(arg);
    for _ in 0..4 {
        let Some(at) = local else { break };
        match defined(body, block, at) {
            Some(Rvalue::Ref(_, place)) => match place_class(cx, body, block, place) {
                Some(class) => return class,
                None => break,
            },
            Some(Rvalue::Use(Operand { kind: OperandKind::Static(id), .. })) => {
                return item_path(cx.db, ModuleDefId::StaticId(*id)).into(); // alloc: a class
            }
            Some(Rvalue::Use(operand)) => local = whole_local(operand),
            _ => break,
        }
    }
    let mut ty = match &arg.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => {
            place.as_ref().ty(body, &cx.infcx, cx.env.param_env).ty
        }
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { .. } | OperandKind::Static(_) => {
            return "a lock of a constant".into();
        }
    };
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        ty = inner;
    }
    format!("{}", cx.shown(ty)).into() // alloc: a class, once per acquire of a summary
}

/// Adds `class` taken through `site` at `depth`, keeping the fewest calls; false past [`MOST`].
fn add_taken(taken: &mut Vec<Taken>, class: &str, depth: u16, site: u32) -> bool {
    match taken.iter().position(|it| &*it.class == class) {
        Some(at) => {
            let it = &mut taken[at];
            if depth < it.depth {
                it.depth = depth;
                it.site = site;
            }
            true
        }
        None if taken.len() < MOST => {
            taken.push(Taken { class: class.into(), depth, site });
            true
        }
        None => false,
    }
}

/// Adds an edge, one per pair, the body's own before a callee's; false past [`MOST`].
fn add_edge(order: &mut Vec<Edge>, held: &str, taken: &str, how: EdgeHow) -> bool {
    match order.iter().position(|it| &*it.held == held && &*it.taken == taken) {
        Some(at) => {
            let it = &mut order[at];
            if matches!((it.how, how), (EdgeHow::Site(_), EdgeHow::Own { .. })) {
                it.how = how;
            }
            true
        }
        None if order.len() < MOST => {
            order.push(Edge { held: held.into(), taken: taken.into(), how });
            true
        }
        None => false,
    }
}

/// The locks of `body`, whose sites are `sites` and whose body callees' summaries `callee`
/// gives (`None` for a leaf).
pub(crate) fn of<'db>(
    cx: &Cx<'db>,
    body: &MirBody<'db>,
    sites: &[LockSite<'_>],
    callee: &dyn Fn(&Instance) -> Option<Arc<Summary>>,
) -> Locks {
    let db = cx.db;
    // alloc: once per instance, mostly empty
    let mut taken: Vec<Taken> = Vec::new();
    // alloc: as above
    let mut order: Vec<Edge> = Vec::new();
    let mut fits = true;
    // alloc: one summary handle per site, once per instance
    let summaries: Vec<Option<Arc<Summary>>> =
        sites.iter().map(|site| if site.given { None } else { callee(site.instance) }).collect();
    // The class of each local that holds a guard, with the site of its acquire.
    // alloc: one slot per local, once per instance that takes or is handed a guard
    let mut class: Vec<Option<(Box<str>, u32)>> = Vec::new();
    for (at, site) in sites.iter().enumerate() {
        if site.given {
            continue;
        }
        let summary = summaries[at].as_deref();
        let at = at as u32;
        if let Some(summary) = summary {
            fits &= !summary.locks.wide;
            for it in summary.locks.taken.iter() {
                fits &= add_taken(&mut taken, &it.class, it.depth.saturating_add(1), at);
            }
            for it in summary.locks.order.iter() {
                fits &= add_edge(&mut order, &it.held, &it.taken, EdgeHow::Site(at));
            }
        }
        let Some(block) = site.call else { continue };
        let Some(TerminatorKind::Call { args, destination, .. }) =
            body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind)
        else {
            continue;
        };
        let found = if acquires(db, site.instance) {
            let found = match args.first() {
                Some(arg) => receiver_class(cx, body, block, arg),
                None => "a lock taken with no receiver".into(),
            };
            fits &= add_taken(&mut taken, &found, 1, at);
            Some(found)
        } else {
            summary.and_then(|it| it.locks.returns.clone()) // clone: a class, few
        };
        if let Some(found) = found
            && destination.as_ref().projection.as_slice().is_empty()
        {
            if class.is_empty() {
                class.resize(body.locals.len(), None);
            }
            class[slot(destination.local)] = Some((found, at));
        }
    }
    // alloc: once per instance that holds a guard, mostly empty
    let mut across: Vec<Across> = Vec::new();
    if !class.is_empty() {
        follow(db, body, sites, &mut class);
        // alloc: the calls of one body, once per instance that holds a guard
        let points: Vec<(u32, BasicBlockId)> = sites
            .iter()
            .enumerate()
            .filter(|(_, site)| !site.given)
            .filter_map(|(at, site)| Some((at as u32, site.call?)))
            .collect();
        let blocks: Vec<BasicBlockId> = points.iter().map(|it| it.1).collect(); // alloc: as above
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let held = body.held_across_calls_in(frame, &blocks);
            for (&(at, block), locals) in points.iter().zip(held.iter()) {
                let site = &sites[at as usize];
                let summary = summaries[at as usize].as_deref();
                let acquire = acquires(db, site.instance).then(|| {
                    let Some(TerminatorKind::Call { destination, .. }) =
                        body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind)
                    else {
                        return None;
                    };
                    class[slot(destination.local)].as_ref().map(|(it, _)| it.clone()) // clone: a class
                });
                for &local in locals.iter() {
                    let Some((held_class, held_at)) = &class[slot(local)] else { continue };
                    let own = |taken_at| EdgeHow::Own { held_at: *held_at, taken_at };
                    match &acquire {
                        Some(Some(taken_class)) => {
                            if taken_class != held_class {
                                fits &= add_edge(&mut order, held_class, taken_class, own(at));
                            }
                            continue;
                        }
                        Some(None) => continue,
                        None => {}
                    }
                    if let Some(summary) = summary {
                        for it in summary.locks.taken.iter() {
                            if it.class != *held_class {
                                fits &= add_edge(&mut order, held_class, &it.class, own(at));
                            }
                        }
                    }
                    if !lent(body, block, local) && blocks_thread(db, site.instance, summary) {
                        if across.len() < MOST {
                            across.push(Across { class: held_class.clone(), held_at: *held_at, call: at }); // clone: a class
                        } else {
                            fits = false;
                        }
                    }
                }
            }
        });
    }
    let returns = match class.get(slot(return_slot())) {
        Some(Some((it, _))) => Some(it.clone()), // clone: a class
        _ => None,
    };
    taken.sort_by(|a, b| a.class.cmp(&b.class));
    order.sort_by(|a, b| (&a.held, &a.taken).cmp(&(&b.held, &b.taken)));
    across.dedup();
    Locks {
        taken: taken.into_boxed_slice(),
        order: order.into_boxed_slice(),
        across: across.into_boxed_slice(),
        returns,
        wide: !fits,
    }
}

/// Carries each guard's class to the locals it is moved into whole, and through the
/// toolchain's calls handed it by move (`Option::unwrap`): a few rounds, as a guard passes
/// through few.
fn follow(
    db: &Db,
    body: &MirBody<'_>,
    sites: &[LockSite<'_>],
    class: &mut [Option<(Box<str>, u32)>],
) {
    for _ in 0..4 {
        let mut changed = false;
        for (_, data) in body.basic_blocks.iter() {
            for statement in &data.statements {
                let StatementKind::Assign(to, Rvalue::Use(operand)) = &statement.kind else {
                    continue;
                };
                if !to.as_ref().projection.as_slice().is_empty() || class[slot(to.local)].is_some()
                {
                    continue;
                }
                if let Some(from) = whole_local(operand)
                    && let Some(it) = class[slot(from)].clone() // clone: a class
                {
                    class[slot(to.local)] = Some(it);
                    changed = true;
                }
            }
        }
        for site in sites {
            let (Some(block), Some(func)) = (site.call, func_of(site.instance)) else { continue };
            if site.given
                || acquires(db, site.instance)
                || !matches!(func.krate(db).data(db).origin, CrateOrigin::Lang(_))
            {
                continue;
            }
            let Some(TerminatorKind::Call { args, destination, .. }) =
                body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind)
            else {
                continue;
            };
            if !destination.as_ref().projection.as_slice().is_empty()
                || class[slot(destination.local)].is_some()
            {
                continue;
            }
            let handed = args.iter().find_map(|arg| match &arg.kind {
                OperandKind::Move(_) => class[slot(whole_local(arg)?)].clone(), // clone: a class
                _ => None,
            });
            if let Some(it) = handed {
                class[slot(destination.local)] = Some(it);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Whether the call ending `block` is handed a borrow of `guard` (`signal.wait(&mut guard)`),
/// which releases the lock while the call waits.
fn lent(body: &MirBody<'_>, block: BasicBlockId, guard: LocalId) -> bool {
    let Some(TerminatorKind::Call { args, .. }) =
        body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind)
    else {
        return false;
    };
    args.iter().any(|arg| {
        whole_local(arg).is_some_and(|local| {
            matches!(defined(body, block, local), Some(Rvalue::Ref(_, place)) if place.local == guard)
        })
    })
}

// What a lint reads.

/// An edge of the lock order as the crate's graph takes it: the classes, and whether the
/// function's own body adds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderEdge {
    pub held: Box<str>,
    pub taken: Box<str>,
    pub own: bool,
}

/// An edge of `function`'s own body, with where the held lock was taken and the calls down to
/// where the other is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldWhileTaking {
    pub held: Box<str>,
    pub taken: Box<str>,
    /// Where `held` is taken in the function.
    pub held_at: Option<Place>,
    /// From the function to the acquire of `taken`, as a refusal's chain.
    pub chain: Box<[Step]>,
}

/// A guard of `function`'s own body held across a call that blocks the thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldAcross {
    pub class: Box<str>,
    pub held_at: Option<Place>,
    /// From the function to what blocks.
    pub chain: Box<[Step]>,
    /// What the last step does: "calls `std::sync::Condvar::wait`, which waits on a condition
    /// variable".
    pub what: Box<str>,
}

/// The lock facts of `func`, checked as defined (the instance [`crate::check_generic`] checks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockFacts {
    pub function: Box<str>,
    /// Every edge it has, sorted: its own and its callees'.
    pub order: Box<[OrderEdge]>,
    /// The classes it takes, sorted.
    pub taken: Box<[Box<str>]>,
    /// More than the summary keeps: what is missing is unknown.
    pub wide: bool,
    /// Wide here, and through no callee that is wide: where a finding says so once.
    pub wide_here: bool,
}

/// The root instance of `func` as defined, with its environment.
fn root_of(db: &Db, func: FunctionId) -> (StoredParamEnvAndCrate, Instance) {
    let (env, args) = generic(db, func);
    (env, Instance { def: Def::Body(Body::Fn(func)), args })
}

/// What `func`'s summary says of locks, without places: for a crate's graph.
pub fn lock_facts(db: &Db, func: FunctionId) -> LockFacts {
    let (env, root) = root_of(db, func);
    let summary = summary(db, env.clone(), root.clone()); // clone: the query's key, reference counts
    let locks = &summary.locks;
    let wide_here = locks.wide
        && !summary.sites.iter().any(|site| {
            !site.given
                && matches!(site.instance.def, Def::Body(_))
                && summary_of(db, &env, &site.instance).locks.wide
        });
    LockFacts {
        wide_here,
        function: instance_path(db, &root),
        order: locks
            .order
            .iter()
            .map(|it| OrderEdge {
                held: it.held.clone(),   // clone: a class
                taken: it.taken.clone(), // clone: a class
                own: matches!(it.how, EdgeHow::Own { .. }),
            })
            .collect(), // alloc: a lint's answer, once per function
        taken: locks.taken.iter().map(|it| it.class.clone()).collect(), // alloc: as above
        wide: locks.wide,
    }
}

/// The edges of `func`'s own body with their places and chains.
pub fn held_while_taking(db: &Db, func: FunctionId) -> Box<[HeldWhileTaking]> {
    let (env, root) = root_of(db, func);
    let cx = Cx::new(db, env.clone()); // clone: a handle, reference counts
    let summary = summary(db, env, root.clone()); // clone: the query's key, reference counts
    summary
        .locks
        .order
        .iter()
        .filter_map(|edge| {
            let EdgeHow::Own { held_at, taken_at } = edge.how else { return None };
            Some(HeldWhileTaking {
                held: edge.held.clone(),   // clone: a class
                taken: edge.taken.clone(), // clone: a class
                held_at: site_place(&cx, &root, &summary, held_at),
                chain: taken_chain(&cx, &root, &summary, taken_at, &edge.taken),
            })
        })
        .collect() // alloc: a lint's answer, once per function
}

/// The guards `func`'s own body holds across a call that blocks, with their chains.
#[expect(
    blaze::reads_positions,
    reason = "a lock finding names the file and line of each step; read only where one is shown"
)]
pub fn held_across(db: &Db, func: FunctionId) -> Box<[HeldAcross]> {
    let (env, root) = root_of(db, func);
    let cx = Cx::new(db, env.clone()); // clone: a handle, reference counts
    let summary = summary(db, env.clone(), root.clone()); // clone: the query's key
    let Some(index) = effect_index(&NONBLOCKING) else { return Box::default() };
    summary
        .locks
        .across
        .iter()
        .filter_map(|it| {
            let site = summary.sites.get(it.call as usize)?;
            let first = Step {
                name: instance_path(db, &root),
                at: site.span.and_then(|span| place(&cx, &root, span)),
            };
            let row = func_of(&site.instance).and_then(|func| {
                effect_row(db, func)
                    .map(|it| it.does)
                    .or_else(|| effect_attrs::row_of(db, func).map(|it| it.says))
            });
            let (chain, what): (Vec<Step>, Box<str>) = match (row, &site.instance.def) {
                (Some(does), _) => (
                    vec![first], // alloc: a lint's answer
                    format!("calls `{}`, which {does}", instance_path(db, &site.instance)).into(), // alloc: as above
                ),
                (None, Def::Body(_)) => {
                    let callee = summary_of(db, &env, &site.instance);
                    let below =
                        refusal(&cx, &NONBLOCKING, index, Rule::Block, &site.instance, &callee);
                    let mut chain = vec![first]; // alloc: a lint's answer
                    chain.extend(below.chain.iter().cloned()); // clone: the steps, once
                    (chain, below.what)
                }
                (None, Def::Leaf(..)) => (
                    vec![first], // alloc: a lint's answer
                    format!("calls `{}`, which may block", instance_path(db, &site.instance)).into(), // alloc: as above
                ),
            };
            Some(HeldAcross {
                class: it.class.clone(), // clone: a class
                held_at: site_place(&cx, &root, &summary, it.held_at),
                chain: chain.into_boxed_slice(),
                what,
            })
        })
        .collect() // alloc: a lint's answer, once per function
}

fn summary_of(db: &Db, env: &StoredParamEnvAndCrate, instance: &Instance) -> Arc<Summary> {
    summary(db, env.clone(), instance.clone()) // clone: the query's key, reference counts
}

/// Where site `at` of `instance`'s body is written.
#[expect(blaze::reads_positions, reason = "a lock finding's place, read only where it is shown")]
fn site_place(cx: &Cx<'_>, instance: &Instance, summary: &Summary, at: u32) -> Option<Place> {
    let span: MirSpan = summary.sites.get(at as usize)?.span?;
    place(cx, instance, span)
}

/// The calls from `root` through site `at` down to the acquire of `class`: one step per
/// instance, each at the call it makes, following each callee's [`Taken`] witness.
#[expect(blaze::reads_positions, reason = "a lock finding's chain, read only where it is shown")]
fn taken_chain(
    cx: &Cx<'_>,
    root: &Instance,
    start: &Arc<Summary>,
    at: u32,
    class: &str,
) -> Box<[Step]> {
    let db = cx.db;
    let mut chain: Vec<Step> = Vec::new(); // alloc: a lint's answer
    let mut instance = root.clone(); // clone: reference counts
    let mut current = Arc::clone(start);
    let mut site_at = Some(at);
    for _ in 0..CHAIN {
        let Some(site) = site_at.and_then(|it| current.sites.get(it as usize)) else { break };
        chain.push(Step {
            name: instance_path(db, &instance),
            at: site.span.and_then(|span| place(cx, &instance, span)),
        });
        let next = site.instance.clone(); // clone: reference counts
        if acquires(db, &next) {
            chain.push(Step { name: instance_path(db, &next), at: None });
            break;
        }
        let Def::Body(_) = next.def else { break };
        let below = summary(db, cx.stored.clone(), next.clone()); // clone: the query's key
        site_at = below.locks.taken.iter().find(|it| &*it.class == class).map(|it| it.site);
        instance = next;
        current = below;
    }
    chain.into_boxed_slice()
}

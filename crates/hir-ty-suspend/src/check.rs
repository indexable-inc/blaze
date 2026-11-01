//! The check: at each call that may park the fiber, every value held across it must be able to
//! cross threads; every vtable and function pointer made keeps the promise of the unmarked
//! trait method or pointer it is called through.
//!
//! A crate is walked from each of its functions with no type or const parameter, through what
//! each instance calls, puts in a vtable or makes a pointer of; each instance reached that is
//! the crate's own, or a generic one instantiated here, is checked. The fiber driver's own
//! bodies (the module of a function that calls a root itself) hold the fiber's handle across
//! its switch by design and are not checked.

use base_db::{Crate, Db};
use blaze_hash::{HashMap, HashSet};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, HasModule as _, ModuleId,
    attrs::AttrFlags,
    hir::BindingId,
    nameres::demand::{crate_body_owners, walk_bodies},
    signatures::FunctionSignature,
};
use hir_ty::{
    ParamEnvAndCrate,
    next_solver::{DbInterner, GenericArgKind, ParamEnv, StoredTy, Ty, TyKind},
    traits::{StoredParamEnvAndCrate, implements_trait_unique},
};
use hir_ty_mir::mir::{BasicBlockId, LocalId, MirBody, MirSpan, StatementKind, TerminatorKind};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use smallvec::SmallVec;

use crate::{
    dispatch::{Cx, Def, Instance, Target, Unseen, body_of, concrete_args, sites},
    root::{roots, send_trait},
    summary::{owner_of, path, summary, suspends},
};

/// A place in a body: the item whose body it is written in, and where in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Site {
    pub owner: DefWithBodyId,
    pub span: MirSpan,
}

/// Why a value can't be held across a suspend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Why {
    /// Its type is not `Send`.
    NotSend,
    /// Its type is, or holds through a `Box`, tuple, array or reference, this ADT marked
    /// `#[must_not_suspend]`.
    MustNotSuspend(AdtId),
}

/// A value held across a call that may park the fiber, which must not cross threads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Held {
    /// The call.
    pub call: Site,
    /// The binding that holds it, when one does; else it is a temporary.
    pub binding: Option<BindingId>,
    /// Where it got its value: its binding, or the statement or call that made it.
    pub made: Option<Site>,
    pub ty: StoredTy,
    pub why: Why,
    /// The calls from the call to the root, each named.
    pub path: Box<[String]>,
}

/// What an unmarked `dyn` or pointer call promises (never to suspend), broken where the code
/// that breaks it is made, or a call the walk can't see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Promise {
    /// A vtable of `concrete` whose slot for `method` (`None`: its drop) may suspend, though the
    /// trait method is not `#[blaze::may_suspend]`.
    Dyn { method: Option<FunctionId>, concrete: StoredTy },
    /// A function pointer of a function that may suspend, put somewhere other than a field
    /// marked `#[blaze::may_suspend]`.
    Pointer,
    /// What the instance runs can't be seen.
    Unseen(Unseen),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Broken {
    pub at: Site,
    pub promise: Promise,
    /// The calls from what was made to the root, each named; empty for [`Promise::Unseen`].
    pub path: Box<[String]>,
}

/// What the check found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub held: Box<[Held]>,
    pub broken: Box<[Broken]>,
    /// The crate graph parks fibers but has no `core::marker::Send`: nothing was checked, an
    /// error of its own.
    pub no_send: bool,
}

impl Held {
    /// The path as an error says it: `` `variance` → `fetch` → `get_or_run` → `suspend` ``.
    pub fn path_said(&self) -> String {
        self.path.join(" → ")
    }
}

impl Broken {
    pub fn path_said(&self) -> String {
        self.path.join(" → ")
    }
}

/// A call that may park the fiber, and every value held across it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Point {
    pub call: Site,
    pub held: Box<[HeldLocal]>,
    target: Target,
    env: StoredParamEnvAndCrate,
}

/// A value held across a [`Point`]'s call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HeldLocal {
    /// The binding that holds it, when one does; else it is a temporary.
    pub binding: Option<BindingId>,
    /// Where it got its value: its binding, or the statement or call that made it.
    pub made: Option<Site>,
    pub ty: StoredTy,
    /// Why it can't be held there, an error [`suspend_check`] reports; `None`: it may be.
    pub why: Option<Why>,
}

impl Point {
    /// The calls from the call to the root, each named.
    pub fn path(&self, db: &Db) -> Box<[String]> {
        path(db, &self.env, &self.target)
    }

    pub fn path_said(&self, db: &Db) -> String {
        self.path(db).join(" → ")
    }
}

/// The crate's walk: every instance reached from its functions with no type or const
/// parameter, and which of them are checked here. Empty when the graph has no root or no `Send`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Walk {
    order: Box<[Instance]>,
    checked: Box<[Instance]>,
    /// The fiber driver's modules: those of a function that calls a root itself.
    drivers: Box<[ModuleId]>,
}

fn env_of(krate: Crate) -> StoredParamEnvAndCrate {
    ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store()
}

#[blaze_query::tracked(returns(ref))]
fn walk(db: &Db, krate: Crate) -> Walk {
    if roots(db, krate).is_empty() || send_trait(db, krate).is_none() {
        return Walk::default();
    }
    let env = env_of(krate);
    let interner = DbInterner::new_with(db, krate);
    let mut starts = Vec::new(); // alloc: the walk's starts, once per crate
    walk_bodies(db, crate_body_owners(db, krate), |_| true, |owner, _| {
        if let DefWithBodyId::FunctionId(func) = owner
            && FunctionSignature::of(db, func).has_body()
            && let Some(args) = concrete_args(interner, func)
        {
            starts.push(Instance { def: Def::Fn(func), args: args.store() });
        }
    });
    let order = reached(db, &env, &starts);
    let mut drivers: Vec<ModuleId> = Vec::new(); // alloc: a few modules, once per crate
    for instance in &order {
        if summary(db, env, *instance).driver
            && let Some(module) = module_of(db, instance.def)
            && !drivers.contains(&module)
        {
            drivers.push(module);
        }
    }
    let checked = order
        .iter()
        .filter(|it| checked_here(db, krate, it) && module_of(db, it.def).is_none_or(|it| !drivers.contains(&it)))
        .copied()
        .collect();
    Walk { order: order.into_boxed_slice(), checked, drivers: drivers.into_boxed_slice() }
}

/// What the check finds in `krate`: nothing, and nothing walked, when its crate graph has no
/// root.
#[blaze_query::tracked(returns(ref))]
pub fn suspend_check(db: &Db, krate: Crate) -> Found {
    if roots(db, krate).is_empty() {
        return Found::default();
    }
    if send_trait(db, krate).is_none() {
        return Found { no_send: true, ..Found::default() };
    }
    let env = env_of(krate);
    let mut held: Vec<Held> = Vec::new();
    let mut broken: Vec<Broken> = Vec::new();
    let mut seen_held: HashSet<(Site, Option<Site>, Why)> = HashSet::default();
    let mut seen_broken: HashSet<(Site, Promise)> = HashSet::default();
    for instance in walk(db, krate).checked.iter() {
        let checked = instance_check(db, env, *instance);
        // The same error, found again in another instance of one generic body, is said once.
        for point in checked.points.iter() {
            for local in point.held.iter() {
                let Some(why) = local.why else { continue };
                if seen_held.insert((point.call, local.made, why)) {
                    held.push(Held {
                        call: point.call,
                        binding: local.binding,
                        made: local.made,
                        ty: local.ty,
                        why,
                        path: point.path(db),
                    });
                }
            }
        }
        for it in checked.broken.iter() {
            if seen_broken.insert((it.at, it.promise)) {
                broken.push(it.clone()); // clone: an error, once
            }
        }
    }
    Found { held: held.into_boxed_slice(), broken: broken.into_boxed_slice(), no_send: false }
}

/// Every call in `owner`'s body (its closures' and `async` blocks' included) that may park the
/// fiber, with what is held across it, as [`suspend_check`] finds them in `krate`: once per
/// instance of a generic body that differs. Empty for a body the check does not check (the fiber
/// driver's, one no function of `krate` reaches).
#[blaze_query::tracked(returns(ref))]
pub fn suspend_points(db: &Db, krate: Crate, owner: DefWithBodyId) -> Box<[Point]> {
    let env = env_of(krate);
    let mut points: Vec<Point> = Vec::new(); // alloc: the body's points, none when it has none
    for instance in walk(db, krate).checked.iter() {
        if owner_of(db, instance.def) != Some(owner) {
            continue;
        }
        for point in instance_check(db, env, *instance).points.iter() {
            if !points.contains(point) {
                points.push(point.clone()); // clone: a body's points, once
            }
        }
    }
    points.into_boxed_slice()
}

/// The path of the call at `site` to the root, as an error says it, when the call may park the
/// fiber; `None` when it may not, or `krate`'s check does not check its body.
pub fn may_suspend_at(db: &Db, krate: Crate, site: Site) -> Option<String> {
    suspend_points(db, krate, site.owner).iter().find(|it| it.call == site).map(|it| it.path_said(db))
}

/// The functions of `krate`'s walk that run on a fiber or a pool thread: those the fiber
/// driver's code reaches, and those that may suspend (which only run on a fiber). A function
/// reached only through a `dyn` call or a function pointer the driver makes elsewhere is not
/// seen from the driver; it is here when it may suspend.
#[blaze_query::tracked(returns(ref))]
pub fn on_fiber(db: &Db, krate: Crate) -> Box<[FunctionId]> {
    let env = env_of(krate);
    let walk = walk(db, krate);
    let mut seen: HashSet<Instance> = HashSet::default();
    let mut stack: Vec<Instance> = walk
        .order
        .iter()
        .filter(|it| module_of(db, it.def).is_some_and(|it| walk.drivers.contains(&it)))
        .copied()
        .collect();
    stack.extend(walk.order.iter().filter(|it| summary(db, env, **it).suspends.is_some()));
    let mut out: Vec<FunctionId> = Vec::new(); // alloc: the functions, once per crate
    while let Some(instance) = stack.pop() {
        if !seen.insert(instance) {
            continue;
        }
        if let Def::Fn(func) = instance.def
            && !out.contains(&func)
        {
            out.push(func);
        }
        stack.extend(summary(db, env, instance).callees.iter().filter(|it| !seen.contains(it)));
    }
    out.into_boxed_slice()
}

/// Every instance `starts` reach, in walk order (depth first, each where first reached).
fn reached(db: &Db, env: &StoredParamEnvAndCrate, starts: &[Instance]) -> Vec<Instance> {
    let mut order: Vec<Instance> = Vec::new(); // alloc: the instances, once per crate
    let mut index: HashMap<Instance, u32> = HashMap::default();
    let mut stack: Vec<(usize, usize)> = Vec::new(); // alloc: the walk's path, once per crate
    for start in starts {
        if index.contains_key(start) {
            continue;
        }
        index.insert(*start, order.len() as u32);
        order.push(*start);
        stack.push((order.len() - 1, 0));
        while let Some(top) = stack.last_mut() {
            let (at, next) = *top;
            let callees = &summary(db, *env, order[at]).callees;
            let Some(callee) = callees.get(next) else {
                stack.pop();
                continue;
            };
            top.1 += 1;
            if !index.contains_key(callee) {
                index.insert(*callee, order.len() as u32);
                order.push(*callee);
                stack.push((order.len() - 1, 0));
            }
        }
    }
    order
}

/// Whether `instance` is checked in `krate`: its body is the crate's own, or it is a generic
/// body instantiated here (with a type or const argument).
fn checked_here(db: &Db, krate: Crate, instance: &Instance) -> bool {
    let own = owner_of(db, instance.def).is_some_and(|it| it.krate(db) == krate);
    own || instance.args.as_ref().iter().any(|it| !matches!(it.kind(), GenericArgKind::Lifetime(_)))
}

fn module_of(db: &Db, def: Def) -> Option<ModuleId> {
    owner_of(db, def).map(|it| it.module(db))
}

/// What the check finds in one instance: its points, and the promises it breaks.
#[derive(Debug, Default, PartialEq, Eq)]
struct Checked {
    points: Box<[Point]>,
    broken: Box<[Broken]>,
}

#[blaze_query::tracked(returns(ref))]
fn instance_check(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Checked {
    let cx = Cx::new(db, env);
    let Some(owner) = owner_of(db, instance.def) else { return Checked::default() };
    let site = |span| Site { owner, span };
    let mut broken = Vec::new(); // alloc: the instance's errors, none when it has none
    match summary(db, cx.stored, instance).unseen {
        // The code generator reports a body that does not lower, as the build's error.
        None | Some(Unseen::NoMir) => {}
        Some(unseen) => broken.push(Broken {
            at: site(MirSpan::Unknown),
            promise: Promise::Unseen(unseen),
            path: Box::default(),
        }),
    }
    let Ok(body) = body_of(db, &cx.stored, &instance) else {
        return Checked { broken: broken.into_boxed_slice(), ..Checked::default() };
    };
    let sites = sites(&cx, body);

    let mut blocks: SmallVec<[BasicBlockId; 8]> = SmallVec::new();
    let mut firsts: SmallVec<[&Target; 8]> = SmallVec::new();
    for (block, targets) in &sites.calls {
        if let Some(target) = targets.iter().find(|it| suspends(db, &cx.stored, it)) {
            blocks.push(*block);
            firsts.push(target);
        }
    }
    let mut points = Vec::new(); // alloc: the instance's points, none when it has none
    // `walk` checks no instance of a graph without `Send`.
    if !blocks.is_empty()
        && let Some(send) = send_trait(db, cx.env.krate)
    {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let across = body.held_across_calls_in(frame, &blocks);
            for ((&block, &&target), locals) in blocks.iter().zip(&firsts).zip(across) {
                let Some(terminator) = &body.basic_blocks[block].terminator else { continue };
                let held = locals
                    .iter()
                    .map(|&local| {
                        let ty = cx.normalize(body.locals[local].ty.as_ref());
                        let why = match must_not_suspend(&cx, ty) {
                            Some(adt) => Some(Why::MustNotSuspend(adt)),
                            None if !implements_trait_unique(ty, db, cx.env, *send) => Some(Why::NotSend),
                            None => None,
                        };
                        let binding = binding_of(body, local);
                        HeldLocal { binding, made: made_at(body, local, binding).map(site), ty: ty.store(), why }
                    })
                    .collect();
                points.push(Point { call: site(terminator.span), held, target, env });
            }
        });
    }

    for slot in &sites.slots {
        let marked = slot.method.is_some_and(|it| AttrFlags::query(db, it.into()).contains(AttrFlags::MAY_SUSPEND));
        if marked {
            continue;
        }
        if let Some(target) = slot.targets.iter().find(|it| suspends(db, &cx.stored, it)) {
            broken.push(Broken {
                at: site(slot.span),
                promise: Promise::Dyn { method: slot.method, concrete: slot.concrete.store() },
                path: path(db, &cx.stored, target),
            });
        }
    }
    for reified in &sites.reified {
        if reified.into_marked {
            continue;
        }
        if let Some(target) = reified.targets.iter().find(|it| suspends(db, &cx.stored, it)) {
            broken.push(Broken { at: site(reified.span), promise: Promise::Pointer, path: path(db, &cx.stored, target) });
        }
    }
    Checked { points: points.into_boxed_slice(), broken: broken.into_boxed_slice() }
}

/// The ADT marked `#[must_not_suspend]` that `ty` is or holds, as rustc's
/// `check_must_not_suspend_ty` looks: through `Box`, tuples, arrays, slices and references, not
/// into an ADT's fields.
fn must_not_suspend<'db>(cx: &Cx<'db>, ty: Ty<'db>) -> Option<AdtId> {
    match ty.kind() {
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if AttrFlags::query(cx.db, adt.into()).contains(AttrFlags::MUST_NOT_SUSPEND) {
                return Some(adt);
            }
            match adt {
                AdtId::StructId(it) if cx.lang.OwnedBox == Some(it) => must_not_suspend(cx, args.type_at(0)),
                _ => None,
            }
        }
        TyKind::Tuple(fields) => fields.iter().find_map(|it| must_not_suspend(cx, it)),
        TyKind::Array(inner, _) | TyKind::Slice(inner) | TyKind::Ref(_, inner, _) => must_not_suspend(cx, inner),
        _ => None,
    }
}

fn binding_of(body: &MirBody<'_>, local: LocalId) -> Option<BindingId> {
    body.binding_locals.iter().find_map(|(binding, &it)| (it == local).then_some(binding))
}

/// Where `local` got its value: its binding, else the first statement or call that writes it
/// as a whole.
fn made_at(body: &MirBody<'_>, local: LocalId, binding: Option<BindingId>) -> Option<MirSpan> {
    if let Some(binding) = binding {
        return Some(MirSpan::BindingId(binding));
    }
    let whole = |place: &hir_ty_mir::mir::StoredPlace| place.local == local && place.projection.is_empty();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, _) = &statement.kind
                && whole(place)
            {
                return Some(statement.span);
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
            && whole(destination)
        {
            return Some(terminator.span);
        }
    }
    None
}

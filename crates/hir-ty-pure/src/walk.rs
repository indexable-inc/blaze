//! The walk of a build script from its `main` (design sections 2 and 3): every instance it can
//! run, each summarized once by [`summary`], a query, so a callee shared by many scripts
//! or reached many times is walked once.
//!
//! The walk is closed-world: it follows each call to what the code generator would call
//! (`hir_ty_mir::mir::dispatch`: drop glue, `Fn*` of a closure or function, `Future::poll`
//! of a coroutine, the impl's method of a trait's), each function pointer made (reified or put
//! in a constant), and each
//! vtable made (every method of the trait and its supertraits, and drop, for the concrete
//! type). A call through a function pointer or `dyn` then adds nothing: what it can reach
//! was walked where the pointer or vtable was made. The models (`crate::models`) stop it at
//! the toolchain's leaves and give the values of its transfers.
//!
//! What it cannot follow makes the script impure, said with the calls that lead to it:
//! foreign functions, inline assembly, intrinsics not known to be pure, state that changes
//! (`static mut`, thread-locals, foreign statics, statics with interior mutability), bodies
//! that do not lower, code pointers made of bytes, and instances past [`LONGEST`].

use std::cmp::Ordering;

use base_db::{Crate, Db};
use blaze_hash::{HashMap, HashSet};
use std::alloc::Global;
use blaze_query::db::{AsId, FromId};
use hir_def::{
    CallableDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, ModuleDefId, StaticId,
    attrs::AttrFlags,
    lang_item::{LangItems, lang_items},
    nameres::crate_def_map,
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};
use hir_expand::name::Name;
use hir_ty::{
    InferBodyId, MemoryMap, ParamEnvAndCrate, PointerCast,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    drop::is_freeze,
    intrinsic::{Intrinsic, SimdOp},
    next_solver::{
        AllocationData, ConstKind, DbInterner, GenericArg, GenericArgs, ParamEnv,
        StoredGenericArgs, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    AssertKind, BasicBlockId, BinOp, CastKind, MirBody, MirLowerError, MirSpan, Operand, OperandKind, Rvalue,
    StatementKind, StoredPlace, TerminatorKind,
    dispatch::{
        self, Body, Call, Callee, DropStep, LONGEST, UnsizeStep, clone_parts, variants,
        vtable_methods, within_longest,
    },
    normalize_or_keep, return_slot,
};
use intern::{Symbol, sym};
use rustc_type_ir::{
    TypeVisitableExt as _,
    inherent::{GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;

use crate::{
    Abs, Effects, Impure, Proof, Purity, allowed_env, effect_attrs,
    effects::{self, EFFECTS, EffectDef, EffectSet, Rule},
    identity,
    memory::{self, CallKind, MemoryEffects, Origin, SiteFacts},
    models::{Applied, Leaf, Model, Transfer, apply_leaf, effect_row, model_of, tag},
    values::{Flow, Spread, Values, evaluate},
};

/// What an instance runs: a body the walk goes into, or a leaf of the models it stops at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Def {
    Body(Body),
    Leaf(FunctionId, Leaf),
}

/// An instance: what it runs and its generic arguments (a closure's or coroutine's are its
/// parent's; a leaf's are none).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Instance {
    pub(crate) def: Def,
    pub(crate) args: StoredGenericArgs,
}

/// A leaf whose arguments depend on the instance's parameters: each caller applies it again
/// with its own values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deferred {
    pub(crate) leaf: Leaf,
    pub(crate) func: FunctionId,
    pub(crate) args: [Abs; 2],
}

/// What one instance does, its callees' included.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) effects: Effects,
    /// The variables it reads, each allowed, sorted, each once.
    pub(crate) env: SmallVec<[Symbol; 2]>,
    pub(crate) deferred: SmallVec<[Deferred; 2]>,
    /// What it returns, in terms of its parameters.
    pub(crate) ret: Abs,
    /// Every instance it calls, makes a pointer of or puts in a vtable, in the order of its
    /// sites.
    pub(crate) callees: Box<[Instance]>,
    /// The first thing it does that is not pure, the one with the fewest calls to it.
    pub(crate) impure: Option<Box<Impure>>,
    /// Every kind of effect it or a callee may have, no trust applied (crate::effects).
    pub(crate) kinds: EffectSet,
    /// Per effect of [`EFFECTS`], by index: the kinds the effect refuses that it may have through
    /// no function trusted for the effect. Missing past the end: none.
    pub(crate) sets: SmallVec<[EffectSet; 4]>,
    /// How it has each kind of `sets`, by effect and then kind, each once.
    pub(crate) witnesses: Box<[Witness]>,
    /// The sites the effects go through, in order: its callees, and the closures and functions
    /// it gives a trusted function (which are not callees of a build script's graph).
    pub(crate) sites: Box<[EffectSite]>,
    /// What memory it touches (crate::memory).
    pub(crate) memory: MemoryEffects,
    /// The functions trusted for some effect it calls with no site of their own, sorted, each
    /// once: foreign functions, and methods of a trusted trait called through `dyn`.
    pub(crate) trusted_calls: Box<[FunctionId]>,
}

impl Summary {
    /// The kinds effect `index` refuses that the instance has.
    pub(crate) fn set(&self, index: usize) -> EffectSet {
        self.sets.get(index).copied().unwrap_or(EffectSet::EMPTY)
    }

    /// How it has `rule` for effect `index`.
    pub(crate) fn witness(&self, index: usize, rule: Rule) -> Option<&Witness> {
        self.witnesses.iter().find(|it| usize::from(it.effect) == index && it.rule == rule)
    }
}

impl Default for Summary {
    /// The empty summary a cycle of instances starts from: no effect, and the memory of a call
    /// that may not return (recursion is not known to end).
    fn default() -> Summary {
        Summary {
            effects: Effects::NONE,
            env: SmallVec::new(),
            deferred: SmallVec::new(),
            ret: Abs::Unset,
            callees: Box::default(),
            impure: None,
            kinds: EffectSet::EMPTY,
            sets: SmallVec::new(),
            witnesses: Box::default(),
            sites: Box::default(),
            memory: MemoryEffects::RECURSIVE,
            trusted_calls: Box::default(),
        }
    }
}

/// How an instance has a kind for an effect: it does it itself, or a site's callee does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Witness {
    pub(crate) effect: u8,
    pub(crate) rule: Rule,
    /// Calls to where it is done: 0 for the instance's own.
    pub(crate) depth: u16,
    pub(crate) how: How,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum How {
    /// Its body does it, at `span`: what it does ("reads `COUNT`, a `static mut`").
    Own { what: Box<str>, span: Option<MirSpan> },
    /// The callee of effect site `site` does: as `said` says ("calls `std::env::var`, which
    /// reads the environment"), or, without it, as the callee's own witness says.
    Site { site: u32, said: Option<Box<str>> },
}

/// A site the effects go through: what it reaches and where in the body it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectSite {
    pub(crate) instance: Instance,
    pub(crate) span: Option<MirSpan>,
    /// A closure or function given to a trusted function: walked for the effects alone.
    pub(crate) given: bool,
}

/// The `main` of the crate `krate`: the function its root module names `main` in the value
/// namespace, free and with a body.
pub fn main_of(db: &Db, krate: Crate) -> Option<FunctionId> {
    let root = crate_def_map(db, krate).root_module_id();
    let data = root.to_db(db).def_map(db).module(db, root);
    let Some(ModuleDefId::FunctionId(main)) =
        data.name_in_scope(db, &Name::new_symbol_root(sym::main)).take_values()
    else {
        return None;
    };
    let free = matches!(main.loc(db).container, ItemContainerId::ModuleId(_));
    (free && FunctionSignature::of(db, main).has_body()).then_some(main)
}

/// Whether the build script whose `main` is `main` is pure, and its proof if it is.
#[blaze_query::tracked(returns(ref))]
pub fn script_purity(db: &Db, main: FunctionId) -> Purity {
    let stored = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main.krate(db) }.store();
    let cx = Cx::new(db, stored);
    let mut roots: SmallVec<[(Instance, String); 2]> = SmallVec::new();
    let empty = GenericArgs::new_from_slice(&[]).store();
    roots.push((Instance { def: Def::Body(Body::Fn(main)), args: empty }, "`main`".to_owned())); // alloc: once per script
    let output = db.callable_item_signature(main.into()).skip_binder().skip_binder().output();
    if !output.is_unit() {
        // `lang_start` reports what `main` returns through `Termination::report`.
        let mut walk = Walk::new(&cx, None);
        match cx.lang.Termination.and_then(|it| {
            it.trait_items(db).method_by_name(&Name::new_symbol_root(Symbol::intern("report")))
        }) {
            Some(report) => {
                let args = GenericArgs::new_from_slice(&[GenericArg::from(output)]);
                walk.call(report, args, Spread::Opaque, None);
            }
            None => walk.refuse(Rule::Unchecked, "returns a value, but the toolchain has no `Termination::report`"),
        }
        if let Some(what) = walk.own.take() {
            return Purity::Impure(Impure { what, through: vec!["`main`".to_owned()] }); // alloc: an impure script's reason, once
        }
        for site in walk.sites {
            let (Reached::Walk(instance, _) | Reached::Leaf(instance, _)) = site.reached;
            let name = format!("`main` → {}", name_of(db, &instance)); // alloc: once per script root
            roots.push((instance, name));
        }
    }
    let mut effects = Effects::NONE;
    let mut env: Vec<Symbol> = Vec::new(); // alloc: the proof's variables, once per script
    for (root, name) in &roots {
        let Def::Leaf(func, leaf) = root.def else {
            let summary = summary(db, cx.stored.clone(), root.clone()); // clone: the query's key, reference counts
            if let Some(impure) = &summary.impure {
                let mut through = vec![name.clone()]; // clone: an impure script's reason, once
                through.extend(impure.through.iter().cloned()); // clone: an impure script's reason, once
                return Purity::Impure(Impure { what: impure.what.clone(), through }); // clone: as above
            }
            effects = effects.union(summary.effects);
            env.extend(summary.env.iter().copied());
            for deferred in &summary.deferred {
                match apply_leaf(deferred.leaf, &[Abs::Unknown, Abs::Unknown]) {
                    Applied::Done { effects: more, env: read, .. } => {
                        effects = effects.union(more);
                        env.extend(read);
                    }
                    Applied::Deferred | Applied::Refused(_) => {
                        let what = format!("{}: its argument is not known", leaf_said(db, deferred.func)); // alloc: an impure script's reason, once
                        return Purity::Impure(Impure { what, through: vec![name.clone()] }); // clone: as above
                    }
                }
            }
            continue;
        };
        match apply_leaf(leaf, &[Abs::Unknown, Abs::Unknown]) {
            Applied::Done { effects: more, env: read, .. } => {
                effects = effects.union(more);
                env.extend(read);
            }
            Applied::Deferred | Applied::Refused(_) => {
                let what = format!("{}: its argument is not known", leaf_said(db, func)); // alloc: an impure script's reason, once
                return Purity::Impure(Impure { what, through: vec!["`main`".to_owned()] }); // alloc: as above
            }
        }
    }
    let (order, index) = reached(db, &cx.stored, roots.iter().map(|(it, _)| it));
    let callees: Vec<&[Instance]> = order.iter().map(|it| callees_of(db, &cx.stored, it)).collect(); // alloc: one slice per instance, once per script
    let identity = match identity::of(db, &cx.stored, &order, &callees, &index) {
        Ok(it) => it,
        Err(what) => return Purity::Impure(Impure { what, through: vec!["`main`".to_owned()] }), // alloc: an impure script's reason, once
    };
    env.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    env.dedup();
    debug_assert!(env.iter().all(|it| allowed_env(it.as_str())));
    Purity::Pure(Proof {
        identity,
        env: env.into_boxed_slice(),
        effects,
        instances: order.len() as u32,
    })
}

/// Each instance the walk from `main` reaches, in walk order, by name with its part of the
/// identity ([`identity::content`]): what a test names when two scripts' identities differ.
#[cfg(test)]
pub(crate) fn identity_parts(
    db: &Db,
    main: FunctionId,
) -> Vec<(String, Result<blaze_ids::ContentId, String>)> {
    let stored = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main.krate(db) }.store();
    let root =
        Instance { def: Def::Body(Body::Fn(main)), args: GenericArgs::new_from_slice(&[]).store() };
    let (order, _) = reached(db, &stored, std::iter::once(&root));
    order
        .iter()
        // clone: a test's names and parts; the query's key, reference counts
        .map(|it| (name_of(db, it), identity::content(db, stored.clone(), it.clone()).clone()))
        // alloc: a test's list of parts
        .collect()
}

/// Every instance `roots` reach, in walk order (depth first, each callee in the order of its
/// sites, each instance where it is first reached), and each one's place in it.
fn reached<'a>(
    db: &Db,
    stored: &StoredParamEnvAndCrate,
    roots: impl Iterator<Item = &'a Instance>,
) -> (Vec<Instance>, HashMap<Instance, u32, Global>) {
    let mut order: Vec<Instance> = Vec::new(); // alloc: the instances, once per script
    let mut index: HashMap<Instance, u32, Global> = HashMap::new_in(Global);
    let mut stack: Vec<(usize, usize)> = Vec::new(); // alloc: the walk's path, once per script
    for root in roots {
        if index.contains_key(root) {
            continue;
        }
        index.insert(root.clone(), order.len() as u32); // clone: reference counts
        order.push(root.clone()); // clone: reference counts
        stack.push((order.len() - 1, 0));
        while let Some(top) = stack.last_mut() {
            let (at, next) = *top;
            let callees = callees_of(db, stored, &order[at]);
            let Some(callee) = callees.get(next) else {
                stack.pop();
                continue;
            };
            top.1 += 1;
            if !index.contains_key(callee) {
                index.insert(callee.clone(), order.len() as u32); // clone: reference counts
                order.push(callee.clone()); // clone: reference counts
                stack.push((order.len() - 1, 0));
            }
        }
    }
    (order, index)
}

fn callees_of<'db>(
    db: &'db Db,
    stored: &StoredParamEnvAndCrate,
    instance: &Instance,
) -> &'db [Instance] {
    match instance.def {
        Def::Leaf(..) => &[],
        _ => &summary(db, stored.clone(), instance.clone()).callees, // clone: the query's key, reference counts
    }
}

/// What the instance does, asked from `env`: worked out in its home (`crate::home`), so every
/// build script that reaches it shares one summary.
pub(crate) fn summary<'db>(db: &'db Db, env: StoredParamEnvAndCrate, instance: Instance) -> &'db Summary {
    summary_in(db, crate::home::env_of(db, &env, &instance), instance)
}

/// What the instance does, in `env`. A cycle of instances starts from the empty summary and
/// iterates to its fixpoint: each local's value only rises, in a lattice three high.
#[blaze_query::tracked(returns(ref), cycle_initial = |_, _, _, _| Summary::default())]
pub(crate) fn summary_in(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Summary {
    let Some(body) = body_of(db, &env, &instance) else { return Summary::default() };
    match body {
        Ok(body) => {
            let cx = Cx::new(db, env);
            summarize(&cx, body)
        }
        Err(error) => {
            let what = format!("has a body that does not lower to MIR: {error:?}"); // alloc: an impure script's reason, once
            let mut own = Own::default();
            own.add(EffectSet::one(Rule::Unchecked), &what, None, 0);
            let (sets, witnesses) = own.into_sets();
            Summary {
                impure: Some(Box::new(Impure { what, through: Vec::new() })), // alloc: an impure summary's reason, kept out of line
                kinds: EffectSet::one(Rule::Unchecked),
                sets,
                witnesses,
                memory: MemoryEffects::UNKNOWN,
                ..Summary::default()
            }
        }
    }
}

/// The kinds an effect keeps in a summary's set: those it refuses, and [`Rule::Open`], which a
/// generic check reports as left to each instance.
pub(crate) fn kept(effect: &EffectDef) -> EffectSet {
    effect.refuses.union(EffectSet::one(Rule::Open))
}

/// Bit `i` set for each effect `EFFECTS[i]` that `func` is trusted for.
pub(crate) fn trust_mask(db: &Db, func: FunctionId) -> u32 {
    EFFECTS
        .iter()
        .enumerate()
        .filter(|(_, effect)| effects::trusted(db, func, effect).is_some())
        .fold(0, |mask, (index, _)| mask | 1 << index)
}

/// The kinds a body has itself, each with the first place it has it: per kind, the first place
/// counted for every effect, and before it any counted for fewer (a call of a function trusted
/// for some effects: `except` has a bit per effect of [`EFFECTS`] it does not count for).
#[derive(Default)]
struct Own {
    /// Every kind, trust not applied.
    kinds: EffectSet,
    /// The kinds with a place counted for every effect.
    full: EffectSet,
    // alloc: the body's own kinds, few, once per instance
    first: Vec<(Rule, Box<str>, Option<MirSpan>, u32)>,
}

impl Own {
    fn add(&mut self, kinds: EffectSet, what: &str, span: Option<MirSpan>, except: u32) {
        for rule in kinds.without(self.full).iter() {
            // A place counted for at least the effects this one is already stands for it.
            let known = self.first.iter().any(|it| it.0 == rule && it.3 & !except == 0);
            if !known {
                self.first.push((rule, what.into(), span, except));
            }
            if except == 0 {
                self.full = self.full.union(EffectSet::one(rule));
            }
        }
        self.kinds = self.kinds.union(kinds);
    }

    /// The sets and witnesses of these kinds alone.
    fn into_sets(self) -> (SmallVec<[EffectSet; 4]>, Box<[Witness]>) {
        let mut sets = SmallVec::new();
        let mut witnesses = Vec::new(); // alloc: an instance's witnesses, once per instance
        for (index, effect) in EFFECTS.iter().enumerate() {
            let mut set = EffectSet::EMPTY;
            for (rule, what, span, except) in &self.first {
                if kept(effect).has(*rule) && except & 1 << index == 0 && !set.has(*rule) {
                    set = set.union(EffectSet::one(*rule));
                    let how = How::Own { what: what.clone(), span: *span }; // clone: one per effect that keeps it
                    witnesses.push(Witness { effect: index as u8, rule: *rule, depth: 0, how });
                }
            }
            sets.push(set);
        }
        witnesses.sort_by_key(|it| (it.effect, it.rule));
        (sets, witnesses.into_boxed_slice())
    }
}

/// The monomorphized body of `instance` in `env`; none for a leaf, which has no body walked.
pub(crate) fn body_of<'db>(
    db: &'db Db,
    env: &StoredParamEnvAndCrate,
    instance: &Instance,
) -> Option<Result<&'db MirBody<'db>, MirLowerError<'db>>> {
    match instance.def {
        // clone: handles of interned values, for the body query's key
        Def::Body(body) => Some(dispatch::body_of(db, env.clone(), body, instance.args.clone())),
        Def::Leaf(..) => None,
    }
}

/// What the queries of one environment share.
pub(crate) struct Cx<'db> {
    pub(crate) db: &'db Db,
    pub(crate) interner: DbInterner<'db>,
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnvAndCrate<'db>,
    pub(crate) stored: StoredParamEnvAndCrate,
    lang: &'db LangItems,
}

impl<'db> Cx<'db> {
    pub(crate) fn new(db: &'db Db, stored: StoredParamEnvAndCrate) -> Cx<'db> {
        let env = stored.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        Cx { db, interner, infcx, env, stored, lang: lang_items(db, env.krate) }
    }

    pub(crate) fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        normalize_or_keep(&self.infcx, self.env.param_env, ty)
    }
}

/// What a site reaches, and how its parameters take the site's operands.
#[derive(Clone)]
enum Reached {
    Walk(Instance, Spread),
    Leaf(Instance, Spread),
}

struct Site {
    reached: Reached,
    /// The block whose call's operands the parameters are taken from; none for a site that
    /// is not a call.
    call: Option<BasicBlockId>,
    /// Where in the body: the statement or terminator it is of.
    span: Option<MirSpan>,
    /// What the memory pass applies its callee's facts to.
    origin: Origin,
    /// A closure or function given to a trusted function: for the effects alone, not a callee
    /// of a build script's graph.
    given: bool,
}

/// One body's walk: its sites in order, the flow of each block's call and the first thing it
/// does itself that is not pure.
struct Walk<'a, 'db> {
    cx: &'a Cx<'db>,
    body: Option<&'a MirBody<'db>>,
    sites: Vec<Site>,
    flows: Vec<Flow>,
    own: Option<String>,
    /// The kinds the body has itself.
    kinds: Own,
    /// Where the statement or terminator being gathered is, and what its sites come from.
    span: Option<MirSpan>,
    origin: Origin,
    /// Sites pushed now are given to a trusted function.
    giving: bool,
    /// Per block, what its call is past its sites.
    calls: Vec<CallKind>,
    /// Whether the body is a workspace crate's (effects::workspace).
    workspace: bool,
    /// Functions trusted for some effect called with no site (Summary::trusted_calls).
    trusted_calls: SmallVec<[FunctionId; 2]>,
    dropped: HashSet<Ty<'db>, Global>,
    statics: HashSet<StaticId, Global>,
    code: HashMap<(Ty<'db>, bool), bool, Global>,
}

fn summarize<'db>(cx: &Cx<'db>, body: &MirBody<'db>) -> Summary {
    let db = cx.db;
    let mut walk = Walk::new(cx, Some(body));
    walk.gather(body);
    let mut values = Values::solve(db, cx.env, &cx.infcx, body, &walk.flows);
    let mut out = Out::default();
    if let Some(what) = walk.own.take() {
        out.impure(Impure { what, through: Vec::new() }); // alloc: empty, allocates nothing
    }
    // Memory the walk cannot follow, or state no argument reaches, is any memory.
    let refused = walk.kinds.kinds.intersect(EffectSet::of(&[Rule::Unchecked, Rule::Open, Rule::MutableState]));
    let mut fold = Fold::new(std::mem::take(&mut walk.kinds));
    let mut callees = Vec::with_capacity(walk.sites.len()); // alloc: the summary's callees, once per instance
    let mut sites = Vec::with_capacity(walk.sites.len()); // alloc: the summary's effect sites, once per instance
    let mut facts = Vec::with_capacity(walk.sites.len()); // alloc: one site's memory facts each, once per instance
    for (at, site) in walk.sites.iter().enumerate() {
        let (instance, spread) = match &site.reached {
            Reached::Walk(instance, spread) | Reached::Leaf(instance, spread) => (instance, *spread),
        };
        sites.push(EffectSite { instance: instance.clone(), span: site.span, given: site.given }); // clone: reference counts
        let at = at as u32;
        let memory = match instance.def {
            Def::Leaf(func, leaf) => {
                let (kinds, does) = effects::leaf_kinds(leaf);
                fold.row(db, func, kinds, EffectSet::EMPTY, at, || format!("calls {}, which {does}", fn_name(db, func))); // alloc: a witness's text
                memory::leaf_memory(leaf)
            }
            Def::Body(def) => {
                let callee = summary(db, cx.stored.clone(), instance.clone()); // clone: the query's key, reference counts
                fold.callee(db, def, instance.args.as_ref(), callee, walk.workspace, at);
                callee.memory
            }
        };
        if !site.given {
            facts.push(SiteFacts { origin: site.origin, facts: memory });
        }
        // A closure or function given to a trusted function is walked for the effects alone.
        if site.given {
            continue;
        }
        let operands: SmallVec<[Abs; 8]> = match site.call.map(|it| &body.basic_blocks[it]) {
            Some(block) => match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Call { args, .. } | TerminatorKind::TailCall { args, .. }) => {
                    args.iter().map(|arg| values.operand(arg)).collect() // alloc: inline up to eight operands
                }
                _ => SmallVec::new(),
            },
            None => SmallVec::new(),
        };
        let params = match site.call {
            Some(_) => spread.params(&operands),
            None => Spread::Opaque.params(&operands),
        };
        match instance.def {
            Def::Leaf(func, leaf) => out.leaf(db, func, leaf, &params, None),
            _ => {
                let callee = summary(db, cx.stored.clone(), instance.clone()); // clone: the query's key, reference counts
                out.callee(db, instance, callee, &params);
            }
        }
        callees.push(instance.clone()); // clone: reference counts
    }
    let ret = values.local(return_slot());
    let pointerless: SmallVec<[bool; 16]> =
        body.locals.iter().map(|(_, local)| walk.pointerless(local.ty.as_ref())).collect(); // alloc: inline up to sixteen locals
    let memory = memory::infer(body, &pointerless, &walk.calls, &facts, !refused.is_empty());
    let (kinds, sets, witnesses) = fold.finish();
    walk.trusted_calls.sort();
    Summary {
        kinds,
        sets,
        witnesses,
        sites: sites.into_boxed_slice(),
        memory,
        trusted_calls: walk.trusted_calls.into_iter().collect(), // alloc: once per instance, mostly empty
        ..out.finish(ret, callees.into_boxed_slice())
    }
}

/// The effects of a body as its sites are taken in: per effect, each kind once with its
/// witness of the fewest calls (the first site of those).
struct Fold {
    kinds: EffectSet,
    sets: SmallVec<[EffectSet; 4]>,
    // alloc: the body's witnesses, few, once per instance
    witnesses: Vec<Witness>,
}

impl Fold {
    fn new(own: Own) -> Fold {
        let kinds = own.kinds;
        let (sets, witnesses) = own.into_sets();
        Fold { kinds, sets, witnesses: witnesses.into_vec() }
    }

    fn add(&mut self, effect: usize, rule: Rule, depth: u16, how: impl FnOnce() -> How) {
        if let Some(known) =
            self.witnesses.iter_mut().find(|it| usize::from(it.effect) == effect && it.rule == rule)
        {
            if depth < known.depth {
                (known.depth, known.how) = (depth, how());
            }
            return;
        }
        self.sets[effect] = self.sets[effect].union(EffectSet::one(rule));
        self.witnesses.push(Witness { effect: effect as u8, rule, depth, how: how() });
    }

    /// Whether the call of `func` counts for `effect`: not when `func` is trusted for it, nor
    /// under a function that never returns for an effect that holds of what returns.
    fn counts(db: &Db, func: Option<FunctionId>, effect: &EffectDef) -> bool {
        let Some(func) = func else { return true };
        if effects::trusted(db, func, effect).is_some() {
            return false;
        }
        effect.through_never || !never_returns(db, func)
    }

    /// A call of `func` that has `kinds` itself (a leaf of the models, a row, an allocator's
    /// method), at site `at`, standing for the kinds `covers` of what is under it.
    fn row(
        &mut self,
        db: &Db,
        func: FunctionId,
        kinds: EffectSet,
        _covers: EffectSet,
        at: u32,
        said: impl Fn() -> String,
    ) {
        self.kinds = self.kinds.union(kinds);
        for (index, effect) in EFFECTS.iter().enumerate() {
            let counted = kinds.intersect(kept(effect));
            if counted.is_empty() || !Fold::counts(db, Some(func), effect) {
                continue;
            }
            for rule in counted.iter() {
                self.add(index, rule, 1, || How::Site { site: at, said: Some(said().into()) });
            }
        }
    }

    /// A call of the body `def` with `args`, whose summary is `callee`, at site `at`, from a
    /// body of a workspace crate when `workspace`.
    fn callee(&mut self, db: &Db, def: Body, args: GenericArgs<'_>, callee: &Summary, workspace: bool, at: u32) {
        let func = match def {
            Body::Fn(func) => Some(func),
            Body::Closure(_) | Body::Coroutine(_) | Body::CoroutineByMove(_) => None,
        };
        if let Some(func) = func
            && let Some(rule) = effect_attrs::allocator_rule(db, func, args)
        {
            // The allocator's own body is not walked: its kind is the call's.
            return self.row(db, func, EffectSet::one(rule), EffectSet::EMPTY, at, || {
                format!("calls {}, a method of an allocator", fn_name(db, func)) // alloc: a witness's text
            });
        }
        let (row, covers, said): (EffectSet, EffectSet, Option<&'static str>) =
            match func.and_then(|it| effect_row(db, it)) {
                Some(modeled) if !modeled.from_workspace || workspace => {
                    (modeled.kinds, modeled.covers, Some(modeled.does))
                }
                _ => match func.and_then(|it| effect_attrs::row_of(db, it)) {
                    Some(row) => (row.kinds(), row.kinds(), Some(row.says)),
                    None => (EffectSet::EMPTY, EffectSet::EMPTY, None),
                },
            };
        self.kinds = self.kinds.union(row).union(callee.kinds);
        for (index, effect) in EFFECTS.iter().enumerate() {
            if !Fold::counts(db, func, effect) {
                continue;
            }
            if let (Some(func), Some(does)) = (func, said) {
                for rule in row.intersect(kept(effect)).iter() {
                    self.add(index, rule, 1, || How::Site {
                        site: at,
                        said: Some(format!("calls {}, which {does}", fn_name(db, func)).into()), // alloc: a witness's text
                    });
                }
            }
            for rule in callee.set(index).without(covers).iter() {
                let depth = callee.witness(index, rule).map_or(0, |it| it.depth).saturating_add(1);
                self.add(index, rule, depth, || How::Site { site: at, said: None });
            }
        }
    }

    fn finish(mut self) -> (EffectSet, SmallVec<[EffectSet; 4]>, Box<[Witness]>) {
        self.witnesses.sort_by_key(|it| (it.effect, it.rule));
        (self.kinds, self.sets, self.witnesses.into_boxed_slice())
    }
}

/// Whether `func` returns `!`.
pub(crate) fn never_returns(db: &Db, func: FunctionId) -> bool {
    db.callable_item_signature(func.into()).skip_binder().skip_binder().output().is_never()
}

/// What an `Assert` checks, as a witness says it.
fn assertion(msg: &AssertKind) -> &'static str {
    match msg {
        AssertKind::BoundsCheck { .. } => "checks an index against a length, which panics past it",
        AssertKind::Overflow(..) | AssertKind::OverflowNeg(_) => "checks arithmetic for overflow, which panics on it",
        AssertKind::DivisionByZero(_) => "checks a divisor, which panics on zero",
        AssertKind::RemainderByZero(_) => "checks the divisor of a remainder, which panics on zero",
        AssertKind::MisalignedPointerDereference { .. } => "checks a pointer's alignment, which panics when it is not",
        AssertKind::NullPointerDereference | AssertKind::NullReferenceConstructed => {
            "checks a pointer for null, which panics on it"
        }
        AssertKind::InvalidEnumConstruction(_) => "checks an enum's value, which panics when it is invalid",
    }
}

/// A summary as it is built.
#[derive(Default)]
struct Out {
    effects: Effects,
    env: SmallVec<[Symbol; 2]>,
    deferred: SmallVec<[Deferred; 2]>,
    impure: Option<Impure>,
}

impl Out {
    /// Keeps the impurity with the fewest calls to it, the first of those.
    fn impure(&mut self, impure: Impure) {
        if self.impure.as_ref().is_none_or(|it| impure.through.len() < it.through.len()) {
            self.impure = Some(impure);
        }
    }

    fn read(&mut self, name: Symbol) {
        if !self.env.contains(&name) {
            self.env.push(name);
        }
    }

    /// Applies `leaf`, the function `func`, to `args`; `through` names the callee it was
    /// deferred from.
    fn leaf(&mut self, db: &Db, func: FunctionId, leaf: Leaf, args: &[Abs], through: Option<&Instance>) {
        let at = |i: usize| args.get(i).copied().unwrap_or(Abs::Unknown);
        match apply_leaf(leaf, args) {
            Applied::Done { effects, env, .. } => {
                self.effects = self.effects.union(effects);
                if let Some(name) = env {
                    self.read(name);
                }
            }
            Applied::Deferred => {
                let deferred = Deferred { leaf, func, args: [at(0), at(1)] };
                if !self.deferred.contains(&deferred) {
                    self.deferred.push(deferred);
                }
            }
            Applied::Refused(what) => {
                let through = through.map(|it| vec![name_of(db, it)]).unwrap_or_default(); // alloc: an impure script's reason
                self.impure(Impure { what, through });
            }
        }
    }

    /// Takes in what the callee `instance` does, its parameters `params` given.
    fn callee(&mut self, db: &Db, instance: &Instance, callee: &Summary, params: &[Abs]) {
        self.effects = self.effects.union(callee.effects);
        for &name in &callee.env {
            self.read(name);
        }
        for deferred in &callee.deferred {
            let args = deferred.args.map(|it| it.substitute(params));
            self.leaf(db, deferred.func, deferred.leaf, &args, Some(instance));
        }
        if let Some(impure) = &callee.impure
            && self.impure.as_ref().is_none_or(|it| impure.through.len() + 1 < it.through.len())
        {
            let mut through = Vec::with_capacity(impure.through.len() + 1); // alloc: an impure script's reason
            through.push(name_of(db, instance));
            through.extend(impure.through.iter().cloned()); // clone: an impure script's reason
            self.impure(Impure { what: impure.what.clone(), through }); // clone: as above
        }
    }

    fn finish(mut self, ret: Abs, callees: Box<[Instance]>) -> Summary {
        self.env.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        self.deferred.sort_by(|a, b| {
            tag(a.leaf).cmp(&tag(b.leaf)).then_with(|| {
                a.args.iter().zip(&b.args).fold(Ordering::Equal, |order, (a, b)| {
                    order.then_with(|| abs_order(*a, *b))
                })
            })
        });
        Summary {
            effects: self.effects,
            env: self.env,
            deferred: self.deferred,
            ret,
            callees,
            impure: self.impure.map(Box::new),
            ..Summary::default()
        }
    }
}

fn abs_order(a: Abs, b: Abs) -> Ordering {
    fn key(it: Abs) -> u16 {
        match it {
            Abs::Unset => 0,
            Abs::Const(_) => 1,
            Abs::Env(_) => 2,
            Abs::Root(root) => 3 + root as u16,
            Abs::Param(i) => 16 + u16::from(i),
            Abs::Under(i) => 272 + u16::from(i),
            Abs::Unknown => 528,
        }
    }
    let text = |it: Abs| match it {
        Abs::Const(name) | Abs::Env(name) => Some(name),
        _ => None,
    };
    key(a).cmp(&key(b)).then_with(|| match (text(a), text(b)) {
        (Some(a), Some(b)) => a.as_str().cmp(b.as_str()),
        _ => Ordering::Equal,
    })
}

/// An instance as an impure script's reason names it.
pub(crate) fn name_of(db: &Db, instance: &Instance) -> String {
    let parent = |owner: InferBodyId<'_>| match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => fn_name(db, func),
        InferBodyId::DefWithBodyId(_) | InferBodyId::AnonConstId(_) => "a constant".to_owned(), // alloc: an impure script's reason
    };
    match instance.def {
        Def::Body(Body::Fn(func)) | Def::Leaf(func, _) => fn_name(db, func),
        Def::Body(Body::Closure(id)) => {
            format!("a closure in {}", parent(InternedClosureId::from_id(id).loc(db).owner)) // alloc: an impure script's reason
        }
        Def::Body(Body::Coroutine(id) | Body::CoroutineByMove(id)) => {
            format!("an `async` block in {}", parent(InternedCoroutineId::from_id(id).loc(db).owner)) // alloc: an impure script's reason
        }
    }
}

pub(crate) fn fn_name(db: &Db, func: FunctionId) -> String {
    format!("`{}`", FunctionSignature::of(db, func).name.as_str()) // alloc: an impure script's reason
}

/// What a leaf whose argument is not known is said to do.
fn leaf_said(db: &Db, func: FunctionId) -> String {
    format!("calls {}", fn_name(db, func)) // alloc: an impure script's reason
}

/// How a call's own spread and the one its callee takes the arguments in make one.
/// The kind of a call or drop the walk cannot resolve for `args`: [`Rule::Open`] when they
/// hold a type parameter (a generic check, decided per instance), else [`Rule::Unchecked`].
fn open_or_unchecked(args: GenericArgs<'_>) -> Rule {
    if args.has_param() { Rule::Open } else { Rule::Unchecked }
}

fn compose(outer: Spread, inner: Spread) -> Spread {
    match (outer, inner) {
        (Spread::Direct, it) | (it, Spread::Direct) => it,
        _ => Spread::Opaque,
    }
}

impl<'a, 'db> Walk<'a, 'db> {
    fn new(cx: &'a Cx<'db>, body: Option<&'a MirBody<'db>>) -> Walk<'a, 'db> {
        Walk {
            cx,
            body,
            sites: Vec::new(), // alloc: the body's sites, once per body
            // alloc: one flow per block, once per body
            flows: vec![Flow::Unknown; body.map_or(0, |it| it.basic_blocks.len())],
            own: None,
            kinds: Own::default(),
            span: None,
            origin: Origin::Made,
            giving: false,
            // alloc: one call kind per block, once per body
            calls: vec![CallKind::Sites; body.map_or(0, |it| it.basic_blocks.len())],
            workspace: body.is_some_and(|it| effects::workspace(cx.db, it.owner.krate(cx.db))),
            trusted_calls: SmallVec::new(),
            dropped: HashSet::new_in(Global),
            statics: HashSet::new_in(Global),
            code: HashMap::new_in(Global),
        }
    }

    /// Something a build script may not do, and a kind of effect: the script's reason and the
    /// kind's witness.
    fn refuse(&mut self, rule: Rule, what: &str) {
        self.observe(EffectSet::one(rule), what);
        if self.own.is_none() && !self.giving {
            self.own = Some(what.to_owned()); // alloc: an impure script's reason, once
        }
    }

    /// A reason names types, whose `Debug` reads the attached database.
    fn refuse_with(&mut self, rule: Rule, what: impl FnOnce() -> String) {
        let script = self.own.is_none() && !self.giving;
        if !script && self.kinds.full.has(rule) {
            return;
        }
        let what = hir_ty::attach_db(self.cx.db, what);
        self.observe(EffectSet::one(rule), &what);
        if script {
            self.own = Some(what);
        }
    }

    /// A kind of effect the body has itself, which a build script may have.
    fn observe(&mut self, kinds: EffectSet, what: &str) {
        self.observe_except(kinds, what, 0);
    }

    /// The same of a call of a function trusted for the effects of `except` (trust_mask).
    fn observe_except(&mut self, kinds: EffectSet, what: &str, except: u32) {
        if !kinds.without(self.kinds.full).is_empty() {
            self.kinds.add(kinds, what, self.span, except);
        }
    }

    /// A call with no site of `func`, which may be trusted for some effects: their mask.
    fn trusted_call(&mut self, func: FunctionId) -> u32 {
        let mask = trust_mask(self.cx.db, func);
        if mask != 0 && !self.trusted_calls.contains(&func) {
            self.trusted_calls.push(func);
        }
        mask
    }

    /// Of a call at `at`, what it is past its sites.
    fn call_kind(&mut self, at: Option<BasicBlockId>, kind: CallKind) {
        if let Some(at) = at {
            self.calls[at.into_raw().into_u32() as usize] = kind;
        }
    }

    /// The sites of every block, cleanup blocks too, in block order.
    fn gather(&mut self, body: &'a MirBody<'db>) {
        for (id, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                (self.span, self.origin) = (Some(statement.span), Origin::Made);
                statement.kind.for_each_operand(&mut |operand| self.operand(operand));
                match &statement.kind {
                    StatementKind::Assign(_, rvalue) => self.rvalue(rvalue),
                    StatementKind::InlineAsm(asm) => {
                        self.observe(effect_attrs::asm_kinds(asm), "runs inline assembly");
                        self.refuse(Rule::Foreign, "runs inline assembly");
                    }
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            (self.span, self.origin) = (Some(terminator.span), Origin::Made);
            terminator.kind.for_each_operand(&mut |operand| self.operand(operand));
            match &terminator.kind {
                TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. } => {
                    self.origin = Origin::Call(id);
                    let flow = self.call_site(func, id);
                    self.flows[id.into_raw().into_u32() as usize] = flow;
                }
                TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                    self.origin = Origin::Drop(id);
                    let ty = self.place_ty(place);
                    self.drop_glue(ty);
                }
                TerminatorKind::Assert { msg, .. } => {
                    if let Some(rule) = effect_attrs::assert_rule(body, id) {
                        self.observe(EffectSet::one(rule), assertion(msg));
                    }
                }
                _ => {}
            }
        }
        (self.span, self.origin) = (None, Origin::Made);
    }

    fn place_ty(&self, place: &StoredPlace) -> Ty<'db> {
        let body = self.body.expect("a place is of a body");
        let ty = place.as_ref().ty(body, &self.cx.infcx, self.cx.env.param_env).ty;
        self.cx.normalize(ty)
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(self.place_ty(place)),
            OperandKind::Constant { ty, .. } => Some(ty.as_ref()),
            OperandKind::Allocation { allocation } => Some(allocation.as_ref().ty),
            OperandKind::Static(_) => None,
        }
    }

    /// A call terminator's callee: a function, a constructor (whose value is its operands)
    /// or a pointer (closed).
    fn call_site(&mut self, func: &Operand, at: BasicBlockId) -> Flow {
        let callee = func.fn_def().or_else(|| match self.operand_ty(func)?.kind() {
            TyKind::FnDef(def, args) => Some((def.0, args.skip_binder())),
            _ => None,
        });
        match callee {
            Some((CallableDefId::FunctionId(f), args)) => self.call(f, args, Spread::Direct, Some(at)),
            Some(_) => {
                self.call_kind(Some(at), CallKind::Ctor);
                Flow::Join
            }
            None => {
                self.call_kind(Some(at), CallKind::Indirect);
                Flow::Unknown
            }
        }
    }

    fn push(&mut self, reached: Reached, call: Option<BasicBlockId>) {
        let (span, origin, given) = (self.span, self.origin, self.giving);
        self.sites.push(Site { reached, call, span, origin, given });
    }

    /// A walked instance at a site; its returned value flows as `spread` gives.
    fn walk(&mut self, def: Def, args: GenericArgs<'db>, spread: Spread, call: Option<BasicBlockId>) -> Flow {
        if !within_longest(args) {
            self.refuse_with(Rule::Unchecked, || format!("reaches an instance whose type arguments are past {LONGEST} types long")); // alloc: an impure script's reason
            return Flow::Unknown;
        }
        let instance = Instance { def, args: args.store() };
        let ret = summary(self.cx.db, self.cx.stored.clone(), instance.clone()).ret; // clone: the query's key, reference counts
        self.push(Reached::Walk(instance, spread), call);
        Flow::Ret { ret, spread }
    }

    /// What calling `f` with `args` runs, as the code generator dispatches it.
    fn call(&mut self, f: FunctionId, args: GenericArgs<'db>, spread: Spread, at: Option<BasicBlockId>) -> Flow {
        let db = self.cx.db;
        if !within_longest(args) {
            self.refuse_with(Rule::Unchecked, || format!("calls {} with type arguments past {LONGEST} types long", fn_name(db, f))); // alloc: an impure script's reason
            return Flow::Unknown;
        }
        match dispatch::call(&self.cx.infcx, self.cx.env, f, args) {
            Call::DropGlue(ty) => {
                let before = self.sites.len();
                self.drop_glue(ty);
                if self.sites.len() == before {
                    self.call_kind(at, CallKind::Nothing);
                }
                Flow::Unknown
            }
            Call::UnknownIntrinsic(error) => {
                self.refuse_with(Rule::Unchecked, || format!("calls an intrinsic the walk does not know: {error}")); // alloc: an impure script's reason
                Flow::Unknown
            }
            // `const_eval_select(args, const_fn, runtime_fn)` calls `runtime_fn` with the fields
            // of the tuple `args`.
            Call::ConstEvalSelect(Some((CallableDefId::FunctionId(g), g_args))) => {
                self.call(g, g_args, compose(spread, Spread::Tuple { from: 0 }), at)
            }
            Call::ConstEvalSelect(Some(_)) => {
                self.call_kind(at, CallKind::Indirect);
                Flow::Unknown
            }
            Call::ConstEvalSelect(None) => {
                self.refuse(Rule::Unchecked, "calls `const_eval_select` with a run-time function not known");
                Flow::Unknown
            }
            Call::Intrinsic(intrinsic) => {
                self.call_kind(at, CallKind::Intrinsic(intrinsic));
                let kinds = effect_attrs::intrinsic_kinds(intrinsic);
                if !kinds.is_empty() {
                    self.observe(kinds, &format!("calls the intrinsic {}", fn_name(db, f))); // alloc: a witness's text, once per kind
                }
                self.intrinsic(f, args, intrinsic, spread)
            }
            Call::Closure(body, parent) => self.walk(Def::Body(body), parent, compose(spread, Spread::Closure), at),
            Call::FnItem(CallableDefId::FunctionId(g), g_args) => {
                self.call(g, g_args, compose(spread, Spread::Tuple { from: 1 }), at)
            }
            Call::FnItem(..) | Call::Indirect => {
                self.call_kind(at, CallKind::Indirect);
                Flow::Unknown
            }
            Call::AsyncClosure => {
                self.refuse(Rule::Unchecked, "calls an `async` closure, which the walk does not follow");
                Flow::Unknown
            }
            Call::Method(Err(why)) => {
                self.refuse_with(open_or_unchecked(args), || {
                    format!("calls {} for `{:?}`, which no impl's method is found for: {}", fn_name(db, f), args.type_at(0), why.describe()) // alloc: an impure script's reason
                });
                Flow::Unknown
            }
            Call::Method(Ok(callee)) => self.method(f, args, callee, spread, at),
        }
    }

    /// A call of `f` past the dispatch on intrinsics, drop glue and `Fn*`.
    fn method(
        &mut self,
        f: FunctionId,
        args: GenericArgs<'db>,
        callee: Callee<'db>,
        spread: Spread,
        at: Option<BasicBlockId>,
    ) -> Flow {
        let db = self.cx.db;
        match callee {
            Callee::Poll(body, parent) => {
                self.walk(Def::Body(body), parent, Spread::Opaque, at);
                Flow::Unknown
            }
            Callee::Virtual | Callee::FnPtr(_) => {
                // What a `dyn` call reaches was walked where its vtable was made; a method of a
                // trait trusted for an effect (a door) is trusted wherever the vtable is from.
                if matches!(callee, Callee::Virtual) {
                    self.trusted_call(f);
                }
                self.call_kind(at, CallKind::Indirect);
                Flow::Unknown
            }
            Callee::Derived(..) => {
                self.refuse_with(Rule::Unchecked, || format!("calls a derived {}, which blaze does not build", fn_name(db, f))); // alloc: an impure script's reason
                Flow::Unknown
            }
            Callee::CloneBuiltin(_) => {
                self.clone_builtin(f, args.type_at(0));
                Flow::Transfer { transfer: Transfer::Same { arg: 0 }, spread }
            }
            Callee::Fn(g, _)
                if g == f
                    && matches!(f.loc(db).container, ItemContainerId::TraitId(_))
                    && !FunctionSignature::of(db, f).has_body() =>
            {
                self.refuse_with(open_or_unchecked(args), || {
                    format!("calls {} for `{:?}`, which no impl gives a body", fn_name(db, f), args.type_at(0)) // alloc: an impure script's reason
                });
                Flow::Unknown
            }
            Callee::Fn(g, g_args) => self.function(g, g_args, spread, at),
        }
    }

    /// A call of the function `g` itself: a leaf or transfer of the models, or its body.
    fn function(&mut self, g: FunctionId, args: GenericArgs<'db>, spread: Spread, at: Option<BasicBlockId>) -> Flow {
        let db = self.cx.db;
        match model_of(db, g) {
            Some(Model::Leaf(leaf)) => {
                let instance = Instance {
                    def: Def::Leaf(g, leaf),
                    args: GenericArgs::new_from_slice(&[]).store(),
                };
                self.push(Reached::Leaf(instance, spread), at);
                return Flow::Leaf { leaf, spread };
            }
            Some(Model::Transfer(transfer)) => {
                self.walk(Def::Body(Body::Fn(g)), args, spread, at);
                return Flow::Transfer { transfer, spread };
            }
            None => {}
        }
        if let ItemContainerId::ExternBlockId(_) = g.loc(db).container {
            let link = AttrFlags::link_name(db, g.into())
                .unwrap_or_else(|| FunctionSignature::of(db, g).name.symbol().clone()); // clone: a handle of an interned name
            let what = format!("calls the foreign function `{}`", link.as_str()); // alloc: an impure script's reason
            // fncache's allow-list: the allocator's shims, `memcpy` and its kin, libm, LLVM's
            // intrinsics but the machine's state are deterministic, no `Foreign` to purity.
            let mut kinds = effect_attrs::foreign_kinds(db, g, link.as_str());
            if effects::deterministic_foreign(link.as_str()) {
                kinds = kinds.without(EffectSet::one(Rule::Foreign));
            }
            let except = self.trusted_call(g);
            self.observe_except(kinds, &what, except);
            // A build script may call none: its sandbox is the proof.
            if self.own.is_none() && !self.giving {
                self.own = Some(what);
            }
            return Flow::Unknown;
        }
        if !FunctionSignature::of(db, g).has_body() {
            self.refuse_with(Rule::Unchecked, || format!("calls {}, which has no body", fn_name(db, g))); // alloc: an impure script's reason
            return Flow::Unknown;
        }
        if EFFECTS.iter().any(|effect| effects::trusted(db, g, effect).is_some()) {
            self.give(g, args);
        }
        self.walk(Def::Body(Body::Fn(g)), args, spread, at)
    }

    /// The closures and functions among `args`, the type arguments of a call of the trusted
    /// function `g`, as sites of their own: what they do is the caller's, whatever `g` is trusted
    /// for. One given to a function trusted to be pure may not capture state with interior
    /// mutability, as its runs may be parallel.
    fn give(&mut self, g: FunctionId, args: GenericArgs<'db>) {
        let db = self.cx.db;
        let was = std::mem::replace(&mut self.giving, true);
        let pure = effects::trusted(db, g, &effects::PURE).is_some();
        for arg in args.iter() {
            let Some(ty) = arg.ty() else { continue };
            match ty.kind() {
                TyKind::Closure(id, closure_args) => {
                    let closure = closure_args.as_closure();
                    if pure && self.shares_mutable(closure.tupled_upvars_ty()) {
                        let what = format!("gives {} a closure that captures state with interior mutability", fn_name(db, g)); // alloc: a witness's text, once
                        self.observe(EffectSet::one(Rule::MutableState), &what);
                    }
                    let parent = GenericArgs::new_from_slice(closure.parent_args());
                    self.walk(Def::Body(Body::Closure(id.0.as_id())), parent, Spread::Opaque, None);
                    self.drop_glue(ty);
                }
                TyKind::FnDef(def, fn_args) => {
                    if let CallableDefId::FunctionId(f) = def.0 {
                        self.call(f, fn_args.skip_binder(), Spread::Opaque, None);
                    }
                }
                _ => {}
            }
        }
        self.giving = was;
    }

    /// Whether a closure's captures, `upvars`, hold state with interior mutability by value or
    /// behind a shared reference.
    fn shares_mutable(&self, upvars: Ty<'db>) -> bool {
        let cx = self.cx;
        let TyKind::Tuple(fields) = cx.normalize(upvars).kind() else { return true };
        fields.iter().any(|field| {
            let field = cx.normalize(field);
            let shared = match field.kind() {
                TyKind::Ref(_, pointee, rustc_type_ir::Mutability::Not) => !is_freeze(&cx.infcx, pointee, cx.env.param_env),
                _ => false,
            };
            shared || !is_freeze(&cx.infcx, field, cx.env.param_env)
        })
    }

    /// An intrinsic but `const_eval_select`, which [`dispatch::call`] follows.
    fn intrinsic(&mut self, f: FunctionId, args: GenericArgs<'db>, intrinsic: Intrinsic, spread: Spread) -> Flow {
        use Intrinsic as I;
        let db = self.cx.db;
        match intrinsic {
            I::Transmute | I::TransmuteUnchecked => {
                self.transmute(args.type_at(0), args.type_at(1));
                Flow::Unknown
            }
            I::ReadViaCopy => Flow::Transfer { transfer: Transfer::Same { arg: 0 }, spread },
            I::Simd(SimdOp::WithExposedProvenance) => {
                self.refuse(Rule::Address, "makes a pointer of a number (`simd_with_exposed_provenance`), an address made from a number");
                Flow::Unknown
            }
            _ if pure_intrinsic(intrinsic) => Flow::Unknown,
            _ => {
                self.refuse_with(Rule::Foreign, || format!("calls the intrinsic {}, which is not pure", fn_name(db, f))); // alloc: an impure script's reason
                Flow::Unknown
            }
        }
    }

    /// `Clone::clone` of a type whose impl the compiler supplies: tuples, closures and arrays
    /// clone their parts (and drop those cloned when one panics); the rest is `Copy`.
    fn clone_builtin(&mut self, clone: FunctionId, ty: Ty<'db>) {
        let Some(parts) = clone_parts(&self.cx.infcx, self.cx.env, ty) else {
            return self.refuse(Rule::Unchecked, "clones a closure whose captures are not known");
        };
        for part in parts {
            self.call(clone, GenericArgs::new_from_slice(&[GenericArg::from(part)]), Spread::Opaque, None);
            self.drop_glue(part);
        }
    }

    /// Drop glue of `ty`: its `Drop` impl's `drop`, then its fields'.
    fn drop_glue(&mut self, ty: Ty<'db>) {
        let cx = self.cx;
        let mut steps = SmallVec::new();
        dispatch::drop_glue(&cx.infcx, cx.env, ty, &mut self.dropped, &mut steps);
        for step in steps {
            match step {
                DropStep::Drop(drop, args) => {
                    self.call(drop, args, Spread::Opaque, None);
                }
                DropStep::NoDropTrait => self.refuse(Rule::Unchecked, "drops a value, but the toolchain has no `Drop::drop`"),
                DropStep::Unknown(ty) => {
                    let rule = if ty.has_param() { Rule::Open } else { Rule::Unchecked };
                    self.refuse_with(rule, || format!("drops `{ty:?}`, whose drop glue the walk does not know")) // alloc: an impure script's reason
                }
            }
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) {
        if let Rvalue::BinaryOp(op, left, _) | Rvalue::CheckedBinaryOp(op, left, _) = rvalue {
            return self.compare(op, left);
        }
        let Rvalue::Cast(kind, operand, to) = rvalue else { return };
        match kind {
            CastKind::PointerCoercion(PointerCast::ReifyFnPointer) => {
                if let Some(TyKind::FnDef(def, args)) = self.operand_ty(operand).map(|it| it.kind())
                    && let CallableDefId::FunctionId(f) = def.0
                {
                    self.call(f, args.skip_binder(), Spread::Opaque, None);
                }
            }
            CastKind::PointerCoercion(PointerCast::ClosureFnPointer(_)) => {
                match self.operand_ty(operand).map(|it| it.kind()) {
                    Some(TyKind::Closure(id, args)) => {
                        let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                        self.walk(Def::Body(Body::Closure(id.0.as_id())), parent, Spread::Opaque, None);
                    }
                    _ => self.refuse(Rule::Unchecked, "makes a function pointer of a closure not known"),
                }
            }
            CastKind::PointerCoercion(PointerCast::Unsize) => match self.operand_ty(operand) {
                Some(from) => self.unsize(from, to.as_ref()),
                None => self.refuse(Rule::Unchecked, "unsizes the address of a static"),
            },
            CastKind::DynStar => self.refuse(Rule::Unchecked, "makes a `dyn*`, which the walk does not follow"),
            CastKind::PointerFromExposedAddress => {
                self.refuse(Rule::Address, "casts a number to a pointer, an address made from a number")
            }
            // Where the layout is the toolchain's own (a hash of a pointer for a map's bucket, a
            // `Debug` of one), the rows say which uses observe it; in a workspace crate any
            // cast of a pointer to a number does.
            CastKind::PointerExposeAddress if self.workspace => {
                self.observe(EffectSet::one(Rule::Address), "casts a pointer to a number, which observes its address")
            }
            CastKind::Transmute => match self.operand_ty(operand) {
                Some(from) => self.transmute(from, to.as_ref()),
                None => self.refuse(Rule::Unchecked, "transmutes the address of a static"),
            },
            CastKind::PointerExposeAddress
            | CastKind::PtrToPtr
            | CastKind::PointerCoercion(
                PointerCast::UnsafeFnPointer
                | PointerCast::MutToConstPointer
                | PointerCast::ArrayToPointer,
            )
            | CastKind::IntToInt
            | CastKind::FloatToInt
            | CastKind::FloatToFloat
            | CastKind::IntToFloat
            | CastKind::FnPtrToPtr => {}
        }
    }

    /// A built-in comparison that observes an address, in a workspace crate's body (as a cast of
    /// a pointer to a number is): ordering raw pointers, since where two allocations sit is the
    /// allocator's choice, and comparing function pointers at all, since whether two are equal
    /// is the code generator's. Equality of raw pointers is not counted: two live allocations
    /// never overlap, so it says only whether they are one (a dangling pointer compared with a
    /// fresh allocation is the gap, docs/v2/plan/lanes/pure-fns.md). The toolchain's own
    /// orderings stay within one allocation (a slice iterator's end).
    fn compare(&mut self, op: &BinOp, left: &Operand) {
        if !self.workspace {
            return;
        }
        let Some(ty) = self.operand_ty(left) else { return };
        let ordering = matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge);
        match self.cx.normalize(ty).kind() {
            TyKind::RawPtr(..) if ordering => {
                self.observe(EffectSet::one(Rule::Address), "orders pointers by their addresses")
            }
            TyKind::FnPtr(..) if ordering || matches!(op, BinOp::Eq | BinOp::Ne) => self.observe(
                EffectSet::one(Rule::Address),
                "compares function pointers, whose addresses the code generator picks",
            ),
            _ => {}
        }
    }

    /// A transmute that makes a function pointer or vtable of what is not one.
    fn transmute(&mut self, from: Ty<'db>, to: Ty<'db>) {
        let pointer = |ty: Ty<'db>| matches!(ty.kind(), TyKind::RawPtr(..) | TyKind::Ref(..) | TyKind::FnPtr(..));
        let number = |ty: Ty<'db>| matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_));
        if self.workspace && pointer(self.cx.normalize(from)) && number(self.cx.normalize(to)) {
            self.observe(EffectSet::one(Rule::Address), "transmutes a pointer to a number, which observes its address");
        }
        if self.code(to, false) && !self.code(from, false) {
            self.refuse_with(Rule::Unchecked, || {
                format!("transmutes `{from:?}` to `{to:?}`, a function pointer or vtable made of bytes") // alloc: an impure script's reason
            });
        }
    }

    /// The vtables an unsizing coercion of `from` to `to` makes.
    fn unsize(&mut self, from: Ty<'db>, to: Ty<'db>) {
        let mut steps = SmallVec::new();
        dispatch::unsized_vtables(&self.cx.infcx, self.cx.env, from, to, &mut steps);
        for step in steps {
            match step {
                UnsizeStep::Vtable(concrete, dyn_ty) => self.vtable(concrete, dyn_ty),
                UnsizeStep::NotStruct(from) => {
                    self.refuse_with(Rule::Unchecked, || format!("unsizes `{from:?}`, which is not a struct")) // alloc: an impure script's reason
                }
                UnsizeStep::NoFields(from) => {
                    self.refuse_with(Rule::Unchecked, || format!("unsizes `{from:?}`, a struct without fields")) // alloc: an impure script's reason
                }
                UnsizeStep::Unfollowed(from, to) => {
                    self.refuse_with(Rule::Unchecked, || format!("unsizes `{from:?}` to `{to:?}`, which the walk does not follow")) // alloc: an impure script's reason
                }
            }
        }
    }

    /// The vtable of `concrete` for `dyn_ty`: its drop glue, and every method of the
    /// principal trait and its supertraits that a trait object can call.
    fn vtable(&mut self, concrete: Ty<'db>, dyn_ty: Ty<'db>) {
        self.drop_glue(concrete);
        for (func, args) in vtable_methods(self.cx.interner, concrete, dyn_ty) {
            self.call(func, args, Spread::Opaque, None);
        }
    }

    /// The statics and constants an operand reads: whether they change, and the function
    /// pointers and vtables a constant's value holds.
    fn operand(&mut self, operand: &Operand) {
        let cx = self.cx;
        match &operand.kind {
            OperandKind::Static(id) => self.static_use(*id),
            OperandKind::Constant { konst, ty } => {
                let (konst, ty) = (konst.as_ref(), ty.as_ref());
                if matches!(konst.kind(), ConstKind::Alias(..)) && self.code(ty, true) {
                    match evaluate(cx.db, &cx.infcx, cx.env, konst) {
                        Ok(value) => self.allocation(&value, ty),
                        Err(error) => self.refuse_with(Rule::Unchecked, || format!("uses a constant that holds pointers: {error}")), // alloc: an impure script's reason
                    }
                }
            }
            OperandKind::Allocation { allocation } => {
                let value = allocation.as_ref();
                if self.code(value.ty, true) {
                    self.allocation(&value, value.ty);
                }
            }
            OperandKind::Copy(_) | OperandKind::Move(_) => {}
        }
    }

    /// Reading the static `id`: whether it changes, and what code its value points to.
    fn static_use(&mut self, id: StaticId) {
        if !self.statics.insert(id) {
            return;
        }
        let cx = self.cx;
        let db = cx.db;
        let signature = StaticSignature::of(db, id);
        let name = || signature.name.as_str().to_owned(); // alloc: an impure script's reason
        if signature.flags.contains(StaticFlags::EXTERN) {
            let link = AttrFlags::link_name(db, id.into())
                .unwrap_or_else(|| signature.name.symbol().clone()); // clone: a handle of an interned name
            // The allocator's shim marker, which `__rust_alloc` and the rest read.
            if !link.as_str().starts_with("__rust_") {
                self.refuse_with(Rule::Foreign, || format!("reads `{}`, a foreign static", name())); // alloc: an impure script's reason
            }
            return;
        }
        if signature.flags.contains(StaticFlags::MUTABLE) {
            return self.refuse_with(Rule::MutableState, || format!("reads or writes `{}`, a `static mut`", name())); // alloc: an impure script's reason
        }
        if AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL) {
            return self.refuse_with(Rule::MutableState, || format!("reads `{}`, a thread-local", name())); // alloc: an impure script's reason
        }
        let ty = db.type_for_static(id).skip_binder();
        if !is_freeze(&cx.infcx, ty, cx.env.param_env) {
            return self.refuse_with(Rule::MutableState, || format!("reads `{}`, a static with interior mutability", name())); // alloc: an impure script's reason
        }
        if self.code(ty, true) {
            match db.const_eval_static(id) {
                Ok(value) => self.allocation(&value, ty),
                Err(error) => self.refuse_with(Rule::Unchecked, || format!("reads `{}`, whose value does not evaluate: {error:?}", name())), // alloc: an impure script's reason
            }
        }
    }

    /// The function pointers and vtables an evaluated value of `ty` holds, and the statics it
    /// points into.
    fn allocation(&mut self, value: &AllocationData<'db>, ty: Ty<'db>) {
        let MemoryMap::Complex(map) = &value.memory_map else { return };
        for memory in &map.statics {
            self.static_use(memory.id);
        }
        if map.vtable.is_empty() {
            return;
        }
        let mut dyns: SmallVec<[Ty<'db>; 2]> = SmallVec::new();
        self.scan(ty, true, &mut HashSet::new_in(Global), &mut dyns);
        for pointee in map.vtable.tys() {
            match pointee.kind() {
                TyKind::FnDef(def, args) => {
                    if let CallableDefId::FunctionId(f) = def.0 {
                        self.call(f, args.skip_binder(), Spread::Opaque, None);
                    }
                }
                TyKind::Closure(id, args) => {
                    let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                    self.walk(Def::Body(Body::Closure(id.0.as_id())), parent, Spread::Opaque, None);
                }
                _ => match dyns.as_slice() {
                    &[dyn_ty] => self.vtable(pointee, dyn_ty),
                    _ => self.refuse_with(Rule::Unchecked, || {
                        format!("uses a value of `{ty:?}` holding a vtable of `{pointee:?}` for a trait the walk cannot name") // alloc: an impure script's reason
                    }),
                },
            }
        }
    }

    /// Whether a value of `ty` holds no pointer, so no memory is reached through it.
    fn pointerless(&self, ty: Ty<'db>) -> bool {
        self.no_pointer(ty, &mut HashSet::new_in(Global))
    }

    fn no_pointer(&self, ty: Ty<'db>, seen: &mut HashSet<Ty<'db>, Global>) -> bool {
        let cx = self.cx;
        let ty = cx.normalize(ty);
        if !seen.insert(ty) {
            return true;
        }
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            | TyKind::FnDef(..) => true,
            TyKind::Array(element, _) | TyKind::Slice(element) | TyKind::Pat(element, _) => {
                self.no_pointer(element, seen)
            }
            TyKind::Tuple(fields) => fields.iter().all(|it| self.no_pointer(it, seen)),
            TyKind::Closure(_, args) => self.no_pointer(args.as_closure().tupled_upvars_ty(), seen),
            TyKind::Adt(def, args) => variants(cx.db, def.def_id()).into_iter().all(|variant| {
                cx.db.field_types(variant).iter().all(|(_, field)| {
                    self.no_pointer(field.ty().instantiate(cx.interner, args).skip_norm_wip(), seen)
                })
            }),
            _ => false,
        }
    }

    /// Whether a value of `ty` may hold a function pointer or vtable; `raw`: or a raw pointer,
    /// which may be one cast.
    fn code(&mut self, ty: Ty<'db>, raw: bool) -> bool {
        if let Some(&known) = self.code.get(&(ty, raw)) {
            return known;
        }
        let holds = self.scan(ty, raw, &mut HashSet::new_in(Global), &mut SmallVec::new());
        self.code.insert((ty, raw), holds);
        holds
    }

    fn scan(
        &self,
        ty: Ty<'db>,
        raw: bool,
        seen: &mut HashSet<Ty<'db>, Global>,
        dyns: &mut SmallVec<[Ty<'db>; 2]>,
    ) -> bool {
        let cx = self.cx;
        let ty = cx.normalize(ty);
        if !seen.insert(ty) {
            return false;
        }
        match ty.kind() {
            TyKind::FnPtr(..) => true,
            TyKind::Dynamic(..) => {
                dyns.push(ty);
                true
            }
            TyKind::RawPtr(pointee, _) => self.scan(pointee, raw, seen, dyns) | raw,
            TyKind::Ref(_, pointee, _) | TyKind::Pat(pointee, _) => self.scan(pointee, raw, seen, dyns),
            TyKind::Array(element, _) | TyKind::Slice(element) => self.scan(element, raw, seen, dyns),
            TyKind::Tuple(fields) => {
                fields.iter().fold(false, |any, it| self.scan(it, raw, seen, dyns) | any)
            }
            TyKind::Closure(_, args) => {
                self.scan(args.as_closure().tupled_upvars_ty(), raw, seen, dyns)
            }
            TyKind::Adt(def, args) => {
                let mut any = false;
                for variant in variants(cx.db, def.def_id()) {
                    for (_, field) in cx.db.field_types(variant).iter() {
                        let field = field.ty().instantiate(cx.interner, args).skip_norm_wip();
                        any |= self.scan(field, raw, seen, dyns);
                    }
                }
                any
            }
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Never
            | TyKind::FnDef(..)
            | TyKind::Foreign(_) => false,
            _ => true,
        }
    }
}

/// Whether an intrinsic other than `const_eval_select`, a transmute, `read_via_copy` and
/// `simd_with_exposed_provenance` is pure: arithmetic, memory of the caller's own, type
/// facts. Volatile memory is a device's, and the rest reach outside the program.
fn pure_intrinsic(intrinsic: Intrinsic) -> bool {
    use Intrinsic as I;
    match intrinsic {
        I::VolatileCopyMemory
        | I::VolatileCopyNonoverlappingMemory
        | I::VolatileLoad
        | I::VolatileSetMemory
        | I::VolatileStore
        | I::UnalignedVolatileLoad
        | I::UnalignedVolatileStore
        | I::Breakpoint
        | I::ReturnAddress
        | I::VaArg
        | I::VaCopy
        | I::VaEnd
        | I::ConstAllocate
        | I::ConstDeallocate
        | I::ConstMakeGlobal
        | I::ContractCheckEnsures
        | I::ContractCheckRequires
        | I::Autodiff
        | I::Offload
        | I::OffloadGetNumDevices
        | I::GpuLaunchSizedWorkgroupMem
        | I::AmdgpuDispatchPtr
        | I::RustcPeek => false,
        I::Simd(op) => !matches!(op, SimdOp::WithExposedProvenance),
        I::Atomic(..)
        | I::CopySign(_)
        | I::Maximum(_)
        | I::Minimum(_)
        | I::MaximumNumberNsz(_)
        | I::MinimumNumberNsz(_)
        | I::AbortImmediate
        | I::AddWithOverflow
        | I::AggregateRawPtr
        | I::AlignOf
        | I::AlignOfVal
        | I::ArithOffset
        | I::AssertInhabited
        | I::AssertMemUninitializedValid
        | I::AssertZeroValid
        | I::Assume
        | I::Bitreverse
        | I::BlackBox
        | I::BoxNew
        | I::Bswap
        | I::CallerLocation
        | I::CarryingMulAdd
        | I::CarrylessMul
        | I::CatchUnwind
        | I::Ceil
        | I::CodeviewAnnotation
        | I::ColdPath
        | I::CompareBytes
        | I::ConstEvalSelect
        | I::Copy
        | I::CopyNonoverlapping
        | I::Cos
        | I::Ctlz
        | I::CtlzNonzero
        | I::Ctpop
        | I::Cttz
        | I::CttzNonzero
        | I::DiscriminantValue
        | I::DisjointBitor
        | I::ExactDiv
        | I::Exp
        | I::Exp2
        | I::Fabs
        | I::FaddAlgebraic
        | I::FaddFast
        | I::FdivAlgebraic
        | I::FdivFast
        | I::FieldOffset
        | I::FieldRepresentingTypeActualTypeId
        | I::FieldRepresentingTypeName
        | I::FieldRepresentingTypeOffset
        | I::FloatToIntUnchecked
        | I::Floor
        | I::Fma
        | I::FmulAlgebraic
        | I::FmulFast
        | I::Fmuladd
        | I::Forget
        | I::FremAlgebraic
        | I::FremFast
        | I::FsubAlgebraic
        | I::FsubFast
        | I::BlazeIntCast
        | I::IntegerMax
        | I::IntegerMin
        | I::IsValStaticallyKnown
        | I::Log
        | I::Log10
        | I::Log2
        | I::MulWithOverflow
        | I::NeedsDrop
        | I::NontemporalStore
        | I::Offset
        | I::OffsetOf
        | I::OverflowChecks
        | I::Powf
        | I::Powi
        | I::PrefetchReadData
        | I::PrefetchReadInstruction
        | I::PrefetchWriteData
        | I::PrefetchWriteInstruction
        | I::PtrGuaranteedCmp
        | I::PtrMask
        | I::PtrMetadata
        | I::PtrOffsetFrom
        | I::PtrOffsetFromUnsigned
        | I::RawEq
        | I::ReadViaCopy
        | I::RotateLeft
        | I::RotateRight
        | I::Round
        | I::RoundTiesEven
        | I::SaturatingAdd
        | I::SaturatingSub
        | I::SelectUnpredictable
        | I::Sin
        | I::SizeOf
        | I::SizeOfVal
        | I::SliceGetUnchecked
        | I::Sqrt
        | I::SubWithOverflow
        | I::SveTupleCreate2
        | I::SveTupleCreate3
        | I::SveTupleCreate4
        | I::SveTupleGet
        | I::SveTupleSet
        | I::ThreeWayCompare
        | I::Transmute
        | I::TransmuteUnchecked
        | I::Trunc
        | I::TypeId
        | I::TypeIdArrayLen
        | I::TypeIdElementTy
        | I::TypeIdEq
        | I::TypeIdFieldRepresentingType
        | I::TypeIdFields
        | I::TypeIdFunctionPtr
        | I::TypeIdGenerics
        | I::TypeIdIsSigned
        | I::TypeIdNonExhaustive
        | I::TypeIdPointsMutably
        | I::TypeIdPointsTo
        | I::TypeIdSizeOf
        | I::TypeIdTypeOf
        | I::TypeIdVariantName
        | I::TypeIdVariantNonExhaustive
        | I::TypeIdVariants
        | I::TypeIdVtable
        | I::TypeName
        | I::TypedSwapNonoverlapping
        | I::UbChecks
        | I::UncheckedAdd
        | I::UncheckedDiv
        | I::UncheckedFunnelShl
        | I::UncheckedFunnelShr
        | I::UncheckedMul
        | I::UncheckedRem
        | I::UncheckedShl
        | I::UncheckedShr
        | I::UncheckedSub
        | I::Unreachable
        | I::VariantCount
        | I::VtableAlign
        | I::VtableSize
        | I::WrappingAdd
        | I::WrappingMul
        | I::WrappingSub
        | I::WriteBoxViaMove
        | I::WriteBytes
        | I::WriteViaMove
        // `core::reflect`'s: facts of a type argument, answered by const eval.
        | I::BlazeAdtName
        | I::BlazeFieldAttrs
        | I::BlazeFieldIndex
        | I::BlazeVariantDiscriminant
        | I::BlazeVariantIndex => true,
    }
}

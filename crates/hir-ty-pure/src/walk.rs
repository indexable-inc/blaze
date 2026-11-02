//! The walk of a build script from its `main` (design sections 2 and 3): every instance it can
//! run, each summarized once by [`summary`], a query, so a callee shared by many scripts
//! or reached many times is walked once.
//!
//! The walk is closed-world: it follows each call to what the code generator would call
//! (drop glue, `Fn*` of a closure or function, `Future::poll` of a coroutine, the impl's
//! method of a trait's), each function pointer made (reified or put in a constant), and each
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
use blaze_query::db::{AsId, FromId};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, CallableDefId, DefWithBodyId, FunctionId, GenericParamId, HasModule,
    ItemContainerId, ModuleDefId, StaticId, VariantId,
    attrs::AttrFlags,
    lang_item::{LangItems, lang_items},
    nameres::crate_def_map,
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};
use hir_expand::name::Name;
use hir_ty::{
    InferBodyId, MemoryMap, ParamEnvAndCrate, PointerCast,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    drop::{DropGlue, destructor, has_drop_glue, is_freeze},
    dyn_compatibility::generics_require_sized_self,
    intrinsic::{Intrinsic, SimdOp, intrinsic_of},
    method_resolution::is_dyn_method,
    next_solver::{
        AllocationData, ConstKind, DbInterner, GenericArg, GenericArgs, ParamEnv, Region,
        RegionExt as _, StoredGenericArgs, TraitRef, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    BasicBlockId, CastKind, MirBody, MirLowerError, Operand, OperandKind, Rvalue, StatementKind,
    StoredPlace, TerminatorKind, is_by_move_coroutine, normalize_or_keep, return_slot,
};
use intern::{Symbol, sym};
use rustc_type_ir::{
    Interner as _,
    inherent::{BoundExistentialPredicates as _, Clause as _, GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;

use crate::{
    Abs, Effects, Impure, Proof, Purity, allowed_env, identity,
    models::{Applied, Leaf, Model, Transfer, apply_leaf, model_of, tag},
    values::{Flow, Spread, Values, evaluate},
};

/// The most type nodes an instance's generic arguments may have: one past it is a type that
/// grows without end (polymorphic recursion), which the walk does not follow.
pub(crate) const LONGEST: usize = 256;

/// What an instance runs: the code generator's `InstanceDef` for those the walk goes into, and
/// a leaf of the models for those it stops at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Def {
    Fn(FunctionId),
    /// An `InternedClosureId`, without its database lifetime.
    Closure(blaze_query::db::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block.
    Coroutine(blaze_query::db::Id),
    /// The same of the future an `async` closure gives when called by value.
    CoroutineByMove(blaze_query::db::Id),
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
}

impl Default for Summary {
    fn default() -> Summary {
        Summary {
            effects: Effects::NONE,
            env: SmallVec::new(),
            deferred: SmallVec::new(),
            ret: Abs::Unset,
            callees: Box::default(),
            impure: None,
        }
    }
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
    roots.push((Instance { def: Def::Fn(main), args: empty }, "`main`".to_owned())); // alloc: once per script
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
            None => walk.refuse("returns a value, but the toolchain has no `Termination::report`"),
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
    let root = Instance { def: Def::Fn(main), args: GenericArgs::new_from_slice(&[]).store() };
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
) -> (Vec<Instance>, HashMap<Instance, u32>) {
    let mut order: Vec<Instance> = Vec::new(); // alloc: the instances, once per script
    let mut index: HashMap<Instance, u32> = HashMap::default();
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

/// What the instance does, in `env`. A cycle of instances starts from the empty summary and
/// iterates to its fixpoint: each local's value only rises, in a lattice three high.
#[blaze_query::tracked(returns(ref), cycle_initial = |_, _, _, _| Summary::default())]
pub(crate) fn summary(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Summary {
    let Some(body) = body_of(db, &env, &instance) else { return Summary::default() };
    match body {
        Ok(body) => {
            let cx = Cx::new(db, env);
            summarize(&cx, body)
        }
        Err(error) => Summary {
            impure: Some(Box::new(Impure { // alloc: an impure summary's reason, kept out of line
                what: format!("has a body that does not lower to MIR: {error:?}"), // alloc: an impure script's reason, once
                through: Vec::new(), // alloc: empty, allocates nothing
            })),
            ..Summary::default()
        },
    }
}

/// The monomorphized body of `instance` in `env`; none for a leaf, which has no body walked.
pub(crate) fn body_of<'db>(
    db: &'db Db,
    env: &StoredParamEnvAndCrate,
    instance: &Instance,
) -> Option<Result<&'db MirBody<'db>, MirLowerError<'db>>> {
    // clone: handles of interned values, for the body query's key
    let (args, stored) = (instance.args.clone(), env.clone());
    Some(match instance.def {
        Def::Fn(func) => db.monomorphized_runtime_mir_body(InferBodyId::from(func), args, stored),
        Def::Closure(id) => db.monomorphized_runtime_mir_body_for_closure(
            InternedClosureId::from_id(id),
            args,
            stored,
        ),
        Def::Coroutine(id) => db.monomorphized_runtime_mir_body_for_coroutine(
            InternedCoroutineId::from_id(id),
            args,
            stored,
        ),
        Def::CoroutineByMove(id) => db.monomorphized_runtime_mir_body_for_coroutine_by_move(
            InternedCoroutineId::from_id(id),
            args,
            stored,
        ),
        Def::Leaf(..) => return None,
    })
}

/// What the queries of one environment share.
struct Cx<'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    stored: StoredParamEnvAndCrate,
    lang: &'db LangItems,
}

impl<'db> Cx<'db> {
    fn new(db: &'db Db, stored: StoredParamEnvAndCrate) -> Cx<'db> {
        let env = stored.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        Cx { db, interner, infcx, env, stored, lang: lang_items(db, env.krate) }
    }

    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
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
}

/// One body's walk: its sites in order, the flow of each block's call and the first thing it
/// does itself that is not pure.
struct Walk<'a, 'db> {
    cx: &'a Cx<'db>,
    body: Option<&'a MirBody<'db>>,
    sites: Vec<Site>,
    flows: Vec<Flow>,
    own: Option<String>,
    dropped: HashSet<Ty<'db>>,
    statics: HashSet<StaticId>,
    code: HashMap<(Ty<'db>, bool), bool>,
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
    let mut callees = Vec::with_capacity(walk.sites.len()); // alloc: the summary's callees, once per instance
    for site in &walk.sites {
        let (instance, spread) = match &site.reached {
            Reached::Walk(instance, spread) | Reached::Leaf(instance, spread) => (instance, *spread),
        };
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
    out.finish(ret, callees.into_boxed_slice())
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
fn name_of(db: &Db, instance: &Instance) -> String {
    let parent = |owner: InferBodyId<'_>| match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => fn_name(db, func),
        InferBodyId::DefWithBodyId(_) | InferBodyId::AnonConstId(_) => "a constant".to_owned(), // alloc: an impure script's reason
    };
    match instance.def {
        Def::Fn(func) | Def::Leaf(func, _) => fn_name(db, func),
        Def::Closure(id) => {
            format!("a closure in {}", parent(InternedClosureId::from_id(id).loc(db).owner)) // alloc: an impure script's reason
        }
        Def::Coroutine(id) | Def::CoroutineByMove(id) => {
            format!("an `async` block in {}", parent(InternedCoroutineId::from_id(id).loc(db).owner)) // alloc: an impure script's reason
        }
    }
}

fn fn_name(db: &Db, func: FunctionId) -> String {
    format!("`{}`", FunctionSignature::of(db, func).name.as_str()) // alloc: an impure script's reason
}

/// What a leaf whose argument is not known is said to do.
fn leaf_said(db: &Db, func: FunctionId) -> String {
    format!("calls {}", fn_name(db, func)) // alloc: an impure script's reason
}

fn short(args: GenericArgs<'_>) -> bool {
    args.types().map(|it| it.walk().count()).sum::<usize>() <= LONGEST
}

/// How a call's own spread and the one its callee takes the arguments in make one.
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
            dropped: HashSet::default(),
            statics: HashSet::default(),
            code: HashMap::default(),
        }
    }

    fn refuse(&mut self, what: &str) {
        if self.own.is_none() {
            self.own = Some(what.to_owned()); // alloc: an impure script's reason, once
        }
    }

    /// A reason names types, whose `Debug` reads the attached database.
    fn refuse_with(&mut self, what: impl FnOnce() -> String) {
        if self.own.is_none() {
            self.own = Some(hir_ty::attach_db(self.cx.db, what));
        }
    }

    /// The sites of every block, cleanup blocks too, in block order.
    fn gather(&mut self, body: &'a MirBody<'db>) {
        for (id, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                statement.kind.for_each_operand(&mut |operand| self.operand(operand));
                match &statement.kind {
                    StatementKind::Assign(_, rvalue) => self.rvalue(rvalue),
                    StatementKind::InlineAsm(_) => self.refuse("runs inline assembly"),
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            terminator.kind.for_each_operand(&mut |operand| self.operand(operand));
            match &terminator.kind {
                TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. } => {
                    let flow = self.call_site(func, id);
                    self.flows[id.into_raw().into_u32() as usize] = flow;
                }
                TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                    let ty = self.place_ty(place);
                    self.drop_glue(ty);
                }
                _ => {}
            }
        }
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
            Some(_) => Flow::Join,
            None => Flow::Unknown,
        }
    }

    fn push(&mut self, reached: Reached, call: Option<BasicBlockId>) {
        self.sites.push(Site { reached, call });
    }

    /// A walked instance at a site; its returned value flows as `spread` gives.
    fn walk(&mut self, def: Def, args: GenericArgs<'db>, spread: Spread, call: Option<BasicBlockId>) -> Flow {
        if !short(args) {
            self.refuse_with(|| format!("reaches an instance whose type arguments are past {LONGEST} types long")); // alloc: an impure script's reason
            return Flow::Unknown;
        }
        let instance = Instance { def, args: args.store() };
        let ret = summary(self.cx.db, self.cx.stored.clone(), instance.clone()).ret; // clone: the query's key, reference counts
        self.push(Reached::Walk(instance, spread), call);
        Flow::Ret { ret, spread }
    }

    /// What calling `f` with `args` runs, as the code generator dispatches it.
    fn call(&mut self, f: FunctionId, args: GenericArgs<'db>, spread: Spread, at: Option<BasicBlockId>) -> Flow {
        let (db, lang) = (self.cx.db, self.cx.lang);
        if !short(args) {
            self.refuse_with(|| format!("calls {} with type arguments past {LONGEST} types long", fn_name(db, f))); // alloc: an impure script's reason
            return Flow::Unknown;
        }
        if Some(f) == lang.DropInPlace || Some(f) == lang.DropGlue {
            self.drop_glue(args.type_at(0));
            return Flow::Unknown;
        }
        match intrinsic_of(db, f) {
            Err(error) => {
                self.refuse_with(|| format!("calls an intrinsic the walk does not know: {error}")); // alloc: an impure script's reason
                return Flow::Unknown;
            }
            Ok(Some(intrinsic)) => return self.intrinsic(f, args, intrinsic, spread, at),
            Ok(None) => {}
        }
        if let ItemContainerId::TraitId(trait_) = f.loc(db).container {
            if [lang.Fn, lang.FnMut, lang.FnOnce].contains(&Some(trait_)) {
                return self.fn_trait(f, args, args.type_at(0), spread, at);
            }
            if [lang.AsyncFn, lang.AsyncFnMut, lang.AsyncFnOnce].contains(&Some(trait_)) {
                self.refuse("calls an `async` closure, which the walk does not follow");
                return Flow::Unknown;
            }
        }
        self.resolved(f, args, spread, at)
    }

    /// `Fn*::call*` for `Self` = `self_ty`.
    fn fn_trait(
        &mut self,
        f: FunctionId,
        args: GenericArgs<'db>,
        self_ty: Ty<'db>,
        spread: Spread,
        at: Option<BasicBlockId>,
    ) -> Flow {
        match self.cx.normalize(self_ty).kind() {
            TyKind::Ref(_, inner, _) => self.fn_trait(f, args, inner, spread, at),
            TyKind::Closure(id, closure_args) => {
                let parent = GenericArgs::new_from_slice(closure_args.as_closure().parent_args());
                self.walk(Def::Closure(id.0.as_id()), parent, compose(spread, Spread::Closure), at)
            }
            TyKind::FnDef(def, fn_args) => match def.0 {
                CallableDefId::FunctionId(g) => {
                    self.call(g, fn_args.skip_binder(), compose(spread, Spread::Tuple { from: 1 }), at)
                }
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => Flow::Unknown,
            },
            TyKind::FnPtr(..) | TyKind::Dynamic(..) => Flow::Unknown,
            TyKind::CoroutineClosure(..) => {
                self.refuse("calls an `async` closure, which the walk does not follow");
                Flow::Unknown
            }
            _ => self.resolved(f, args, spread, at),
        }
    }

    /// A call of `f` past the dispatch on intrinsics, drop glue and `Fn*`.
    fn resolved(&mut self, f: FunctionId, args: GenericArgs<'db>, spread: Spread, at: Option<BasicBlockId>) -> Flow {
        let cx = self.cx;
        let db = cx.db;
        if Some(f) == cx.lang.FuturePoll
            && let TyKind::Coroutine(id, coroutine_args) = cx.normalize(args.type_at(0)).kind()
        {
            let coroutine = coroutine_args.as_coroutine();
            let def = match is_by_move_coroutine(db, id.0, coroutine) {
                true => Def::CoroutineByMove(id.0.as_id()),
                false => Def::Coroutine(id.0.as_id()),
            };
            let parent = GenericArgs::new_from_slice(coroutine.parent_args());
            self.walk(def, parent, Spread::Opaque, at);
            return Flow::Unknown;
        }
        if is_dyn_method(cx.interner, cx.env.param_env, f, args).is_some() {
            return Flow::Unknown;
        }
        let (callee, callee_args) = db.lookup_impl_method(cx.env, f, args);
        let Either::Left(g) = callee else {
            self.refuse_with(|| format!("calls a derived {}, which blaze does not build", fn_name(db, f))); // alloc: an impure script's reason
            return Flow::Unknown;
        };
        if g == f && let ItemContainerId::TraitId(trait_) = f.loc(db).container {
            if Some(trait_) == cx.lang.Clone && FunctionSignature::of(db, f).name.as_str() == "clone" {
                self.clone_builtin(f, args.type_at(0));
                return Flow::Transfer { transfer: Transfer::Same { arg: 0 }, spread };
            }
            if Some(trait_) == cx.lang.FnPtrTrait {
                return Flow::Unknown;
            }
            if !FunctionSignature::of(db, f).has_body() {
                self.refuse_with(|| {
                    format!("calls {} for `{:?}`, which no impl gives a body", fn_name(db, f), args.type_at(0)) // alloc: an impure script's reason
                });
                return Flow::Unknown;
            }
        }
        self.function(g, callee_args, spread, at)
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
                self.walk(Def::Fn(g), args, spread, at);
                return Flow::Transfer { transfer, spread };
            }
            None => {}
        }
        if let ItemContainerId::ExternBlockId(_) = g.loc(db).container {
            self.refuse_with(|| {
                let name = AttrFlags::link_name(db, g.into())
                    .unwrap_or_else(|| FunctionSignature::of(db, g).name.symbol().clone()); // clone: an impure script's reason
                format!("calls the foreign function `{}`", name.as_str()) // alloc: an impure script's reason
            });
            return Flow::Unknown;
        }
        if !FunctionSignature::of(db, g).has_body() {
            self.refuse_with(|| format!("calls {}, which has no body", fn_name(db, g))); // alloc: an impure script's reason
            return Flow::Unknown;
        }
        self.walk(Def::Fn(g), args, spread, at)
    }

    fn intrinsic(
        &mut self,
        f: FunctionId,
        args: GenericArgs<'db>,
        intrinsic: Intrinsic,
        spread: Spread,
        at: Option<BasicBlockId>,
    ) -> Flow {
        use Intrinsic as I;
        let db = self.cx.db;
        match intrinsic {
            I::ConstEvalSelect => {
                // `const_eval_select(args, const_fn, runtime_fn)` calls `runtime_fn` with the
                // fields of the tuple `args`.
                match args.types().nth(2).map(|it| it.kind()) {
                    Some(TyKind::FnDef(def, fn_args)) => match def.0 {
                        CallableDefId::FunctionId(g) => {
                            self.call(g, fn_args.skip_binder(), compose(spread, Spread::Tuple { from: 0 }), at)
                        }
                        CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => Flow::Unknown,
                    },
                    _ => {
                        self.refuse("calls `const_eval_select` with a run-time function not known");
                        Flow::Unknown
                    }
                }
            }
            I::Transmute | I::TransmuteUnchecked => {
                self.transmute(args.type_at(0), args.type_at(1));
                Flow::Unknown
            }
            I::ReadViaCopy => Flow::Transfer { transfer: Transfer::Same { arg: 0 }, spread },
            I::Simd(SimdOp::WithExposedProvenance) => {
                self.refuse("makes a pointer of a number (`simd_with_exposed_provenance`), an address made from a number");
                Flow::Unknown
            }
            _ if pure_intrinsic(intrinsic) => Flow::Unknown,
            _ => {
                self.refuse_with(|| format!("calls the intrinsic {}, which is not pure", fn_name(db, f))); // alloc: an impure script's reason
                Flow::Unknown
            }
        }
    }

    /// `Clone::clone` of a type whose impl the compiler supplies: tuples, closures and arrays
    /// clone their parts (and drop those cloned when one panics); the rest is `Copy`.
    fn clone_builtin(&mut self, clone: FunctionId, ty: Ty<'db>) {
        let ty = self.cx.normalize(ty);
        let part = |this: &mut Self, part: Ty<'db>| {
            this.call(clone, GenericArgs::new_from_slice(&[GenericArg::from(part)]), Spread::Opaque, None);
            this.drop_glue(part);
        };
        match ty.kind() {
            TyKind::Tuple(fields) => fields.iter().for_each(|it| part(self, it)),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => match ty.closure_upvars_ty().map(|it| it.kind()) {
                Some(TyKind::Tuple(fields)) => fields.iter().for_each(|it| part(self, it)),
                _ => self.refuse("clones a closure whose captures are not known"),
            },
            TyKind::Array(element, _) => part(self, element),
            _ => {}
        }
    }

    /// Drop glue of `ty`: its `Drop` impl's `drop`, then its fields'.
    fn drop_glue(&mut self, ty: Ty<'db>) {
        let cx = self.cx;
        let db = cx.db;
        let ty = cx.normalize(ty);
        if !self.dropped.insert(ty) || has_drop_glue(&cx.infcx, ty, cx.env.param_env) == DropGlue::None {
            return;
        }
        match ty.kind() {
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if destructor(db, adt).is_some() {
                    let drop = cx.lang.Drop.and_then(|it| {
                        it.trait_items(db).method_by_name(&Name::new_symbol_root(sym::drop))
                    });
                    match drop {
                        Some(drop) => {
                            self.call(drop, GenericArgs::new_from_slice(&[GenericArg::from(ty)]), Spread::Opaque, None);
                        }
                        None => self.refuse("drops a value, but the toolchain has no `Drop::drop`"),
                    }
                }
                // A box's pointee is dropped by its glue, not through its fields' (a pointer).
                if let AdtId::StructId(it) = adt
                    && cx.lang.OwnedBox == Some(it)
                {
                    self.drop_glue(args.type_at(0));
                }
                for variant in variants(db, adt) {
                    for (_, field) in db.field_types(variant).iter() {
                        self.drop_glue(field.ty().instantiate(cx.interner, args).skip_norm_wip());
                    }
                }
            }
            TyKind::Tuple(fields) => fields.iter().for_each(|it| self.drop_glue(it)),
            TyKind::Array(element, _) | TyKind::Slice(element) => self.drop_glue(element),
            TyKind::Closure(_, args) => self.drop_glue(args.as_closure().tupled_upvars_ty()),
            TyKind::Dynamic(..) => {}
            _ => self.refuse_with(|| format!("drops `{ty:?}`, whose drop glue the walk does not know")), // alloc: an impure script's reason
        }
    }

    fn rvalue(&mut self, rvalue: &Rvalue) {
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
                        self.walk(Def::Closure(id.0.as_id()), parent, Spread::Opaque, None);
                    }
                    _ => self.refuse("makes a function pointer of a closure not known"),
                }
            }
            CastKind::PointerCoercion(PointerCast::Unsize) => match self.operand_ty(operand) {
                Some(from) => self.unsize(from, to.as_ref()),
                None => self.refuse("unsizes the address of a static"),
            },
            CastKind::DynStar => self.refuse("makes a `dyn*`, which the walk does not follow"),
            CastKind::PointerFromExposedAddress => {
                self.refuse("casts a number to a pointer, an address made from a number")
            }
            CastKind::Transmute => match self.operand_ty(operand) {
                Some(from) => self.transmute(from, to.as_ref()),
                None => self.refuse("transmutes the address of a static"),
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

    /// A transmute that makes a function pointer or vtable of what is not one.
    fn transmute(&mut self, from: Ty<'db>, to: Ty<'db>) {
        if self.code(to, false) && !self.code(from, false) {
            self.refuse_with(|| {
                format!("transmutes `{from:?}` to `{to:?}`, a function pointer or vtable made of bytes") // alloc: an impure script's reason
            });
        }
    }

    /// The vtables an unsizing coercion of `from` to `to` makes.
    fn unsize(&mut self, from: Ty<'db>, to: Ty<'db>) {
        let cx = self.cx;
        let (from, to) = (cx.normalize(from), cx.normalize(to));
        if from == to {
            return;
        }
        match (from.kind(), to.kind()) {
            (
                TyKind::RawPtr(from, _) | TyKind::Ref(_, from, _),
                TyKind::RawPtr(to, _) | TyKind::Ref(_, to, _),
            ) => self.unsize_pointee(from, to),
            (TyKind::Pat(from, _), TyKind::Pat(to, _)) => self.unsize(from, to),
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args)) if def.def_id() == to_def.def_id() => {
                let AdtId::StructId(id) = def.def_id() else {
                    return self.refuse_with(|| format!("unsizes `{from:?}`, which is not a struct")); // alloc: an impure script's reason
                };
                for (_, field) in cx.db.field_types(id.into()).iter() {
                    let from = field.ty().instantiate(cx.interner, args).skip_norm_wip();
                    let to = field.ty().instantiate(cx.interner, to_args).skip_norm_wip();
                    self.unsize(from, to);
                }
            }
            _ => self.refuse_with(|| format!("unsizes `{from:?}` to `{to:?}`, which the walk does not follow")), // alloc: an impure script's reason
        }
    }

    fn unsize_pointee(&mut self, from: Ty<'db>, to: Ty<'db>) {
        let cx = self.cx;
        let (from, to) = (cx.normalize(from), cx.normalize(to));
        if from == to {
            return;
        }
        match (from.kind(), to.kind()) {
            (TyKind::Array(..), TyKind::Slice(_)) => {}
            // An upcast: the vtable it starts from was walked where it was made.
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) => {}
            (_, TyKind::Dynamic(..)) => self.vtable(from, to),
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args)) if def.def_id() == to_def.def_id() => {
                let AdtId::StructId(id) = def.def_id() else {
                    return self.refuse_with(|| format!("unsizes `{from:?}`, which is not a struct")); // alloc: an impure script's reason
                };
                let fields = cx.db.field_types(id.into());
                let Some((_, last)) = fields.iter().last() else {
                    return self.refuse_with(|| format!("unsizes `{from:?}`, a struct without fields")); // alloc: an impure script's reason
                };
                let from = last.ty().instantiate(cx.interner, args).skip_norm_wip();
                let to = last.ty().instantiate(cx.interner, to_args).skip_norm_wip();
                self.unsize_pointee(from, to);
            }
            _ => self.refuse_with(|| format!("unsizes `{from:?}` to `{to:?}`, which the walk does not follow")), // alloc: an impure script's reason
        }
    }

    /// The vtable of `concrete` for `dyn_ty`: its drop glue, and every method of the
    /// principal trait and its supertraits that a trait object can call.
    fn vtable(&mut self, concrete: Ty<'db>, dyn_ty: Ty<'db>) {
        let cx = self.cx;
        self.drop_glue(concrete);
        let TyKind::Dynamic(bounds, _) = dyn_ty.kind() else { return };
        let Some(principal) = bounds.principal() else { return };
        let trait_ref = cx
            .interner
            .instantiate_bound_regions_with_erased(principal.with_self_ty(cx.interner, concrete));
        let mut seen = HashSet::default();
        self.trait_methods(trait_ref, &mut seen);
    }

    fn trait_methods(&mut self, trait_ref: TraitRef<'db>, seen: &mut HashSet<TraitRef<'db>>) {
        if !seen.insert(trait_ref) {
            return;
        }
        let cx = self.cx;
        let (db, interner) = (cx.db, cx.interner);
        let supertraits: SmallVec<[TraitRef<'db>; 4]> = interner
            .explicit_super_clauses_of(trait_ref.def_id)
            .iter_instantiated(interner, trait_ref.args.as_slice())
            .filter_map(|clause| clause.skip_norm_wip().0.as_trait_clause())
            .map(|pred| interner.instantiate_bound_regions_with_erased(pred).trait_ref)
            .collect(); // alloc: inline up to four supertraits, all read before any is walked
        for supertrait in supertraits {
            self.trait_methods(supertrait, seen);
        }
        let trait_items = trait_ref.def_id.0.trait_items(db);
        for &(_, item) in trait_items.items.iter() {
            let AssocItemId::FunctionId(func) = item else { continue };
            if generics_require_sized_self(db, func.into()) {
                continue;
            }
            let mut generic = false;
            let args = GenericArgs::fill_rest(interner, func.into(), trait_ref.args.iter(), |_, id, _| {
                generic |= !matches!(id, GenericParamId::LifetimeParamId(_));
                Region::new_erased(interner).into()
            });
            // A method with type or const parameters of its own has no slot.
            if !generic {
                self.call(func, args, Spread::Opaque, None);
            }
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
                        Err(error) => self.refuse_with(|| format!("uses a constant that holds pointers: {error}")), // alloc: an impure script's reason
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
                self.refuse_with(|| format!("reads `{}`, a foreign static", name())); // alloc: an impure script's reason
            }
            return;
        }
        if signature.flags.contains(StaticFlags::MUTABLE) {
            return self.refuse_with(|| format!("reads or writes `{}`, a `static mut`", name())); // alloc: an impure script's reason
        }
        if AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL) {
            return self.refuse_with(|| format!("reads `{}`, a thread-local", name())); // alloc: an impure script's reason
        }
        let ty = db.type_for_static(id).skip_binder();
        if !is_freeze(&cx.infcx, ty, cx.env.param_env) {
            return self.refuse_with(|| format!("reads `{}`, a static with interior mutability", name())); // alloc: an impure script's reason
        }
        if self.code(ty, true) {
            match db.const_eval_static(id) {
                Ok(value) => self.allocation(&value, ty),
                Err(error) => self.refuse_with(|| format!("reads `{}`, whose value does not evaluate: {error:?}", name())), // alloc: an impure script's reason
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
        self.scan(ty, true, &mut HashSet::default(), &mut dyns);
        for pointee in map.vtable.tys() {
            match pointee.kind() {
                TyKind::FnDef(def, args) => {
                    if let CallableDefId::FunctionId(f) = def.0 {
                        self.call(f, args.skip_binder(), Spread::Opaque, None);
                    }
                }
                TyKind::Closure(id, args) => {
                    let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                    self.walk(Def::Closure(id.0.as_id()), parent, Spread::Opaque, None);
                }
                _ => match dyns.as_slice() {
                    &[dyn_ty] => self.vtable(pointee, dyn_ty),
                    _ => self.refuse_with(|| {
                        format!("uses a value of `{ty:?}` holding a vtable of `{pointee:?}` for a trait the walk cannot name") // alloc: an impure script's reason
                    }),
                },
            }
        }
    }

    /// Whether a value of `ty` may hold a function pointer or vtable; `raw`: or a raw pointer,
    /// which may be one cast.
    fn code(&mut self, ty: Ty<'db>, raw: bool) -> bool {
        if let Some(&known) = self.code.get(&(ty, raw)) {
            return known;
        }
        let holds = self.scan(ty, raw, &mut HashSet::default(), &mut SmallVec::new());
        self.code.insert((ty, raw), holds);
        holds
    }

    fn scan(
        &self,
        ty: Ty<'db>,
        raw: bool,
        seen: &mut HashSet<Ty<'db>>,
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

/// The variants of `adt`: its one for a struct or union.
fn variants(db: &Db, adt: AdtId) -> SmallVec<[VariantId; 4]> {
    match adt {
        AdtId::StructId(it) => SmallVec::from_slice(&[VariantId::StructId(it)]),
        AdtId::UnionId(it) => SmallVec::from_slice(&[VariantId::UnionId(it)]),
        AdtId::EnumId(it) => it
            .enum_variants(db)
            .variants
            .values()
            .map(|&(id, _)| VariantId::EnumVariantId(id))
            .collect(), // alloc: inline up to four variants
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
        | I::WriteViaMove => true,
    }
}

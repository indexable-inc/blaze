//! The checks of the attribute effects over the one walk (docs/v2/plan/design/pure-fns.md):
//! `#[blaze::pure]` ([`fn_purity`], [`fn_purity_generic`]) and any other [`EffectDef`]
//! ([`check`], [`check_generic`]). A check reads the root's [`walk::summary`], never walks again:
//! the kinds the effect refuses that the root has through no function trusted for it, and for
//! the first of those, the witnesses from the root down to where it is done, each with its place.
//! The root's parameters are checked here, as the walk sees only what the root's body does with
//! them: state with interior mutability behind a shared reference, and code or memory reached
//! through a `dyn`, a function pointer or a raw pointer, which no walk of the root can see.
//!
//! The inferred facts ([`instance_effects`], [`fn_memory`]) are the same summary's, for any
//! instance, with or without an attribute.

use std::alloc::Global;

use base_db::{Crate, CrateOrigin, Db, FileId};
use blaze_hash::HashSet;
use blaze_query::db::FromId;
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, TraitId,
    attrs::AttrFlags,
    signatures::{FunctionSignature, StructSignature},
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    drop::is_freeze,
    next_solver::{GenericArgs, StoredGenericArgs, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    MirSpan,
    checks::source_span,
    dispatch::{Body, variants},
    location::{source_file_range, source_location_in_file, source_path},
};
use intern::Symbol;
use rustc_type_ir::{
    TypeVisitableExt as _,
    inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind as _},
};

use crate::{
    effects::{self, EFFECTS, EffectDef, EffectSet, PURE, Rule, effect_index, trusted_trait},
    memory::MemoryEffects,
    walk::{Cx, Def, How, Instance, Summary, body_of, never_returns, summary},
};

/// What a check found of an instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FnPurity {
    Pure(EffectProof),
    Impure(Refusal),
}

impl FnPurity {
    pub fn as_result(&self) -> Result<&EffectProof, &Refusal> {
        match self {
            FnPurity::Pure(proof) => Ok(proof),
            FnPurity::Impure(refusal) => Err(refusal),
        }
    }
}

/// An instance has an effect: what it rests on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectProof {
    /// Every function trusted for the effect the instance reaches, sorted by `path`, each once:
    /// the kernel adds the content hash of each one's library (its crate's code; for `link:
    /// Some`, the native library its extern block links) to the entry's identity.
    pub trusted: Box<[Trusted]>,
    /// Some call is left to each instance (only from a generic check).
    pub open: bool,
    /// How many instances the check reached.
    pub instances: u32,
}

/// `#[blaze::pure]`'s proof.
pub type PureProof = EffectProof;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trusted {
    /// The function marked trusted, or the method of a trusted trait (a `ctx` door) reached.
    pub func: FunctionId,
    /// Its path as an item, crate first, no instance's arguments: `zstd_sys::ZSTD_compress`,
    /// `<KernelReads as Reads>::layout`, `par::map` for every `par::map::<F>` reached.
    pub path: Box<str>,
    /// The `trusted = ".."` text, as written.
    pub reason: Symbol,
    /// The crate that defines it.
    pub krate: Crate,
    /// The symbol it links to when it is a foreign function; none for a Rust function.
    pub link: Option<Symbol>,
    /// Trusted through this trait's attribute, not its own.
    pub door: Option<TraitId>,
}

/// Why an instance does not have an effect: the shortest chain of calls to the first thing it
/// does that the effect refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The effect's name: "pure".
    pub effect: &'static str,
    /// The checked function, by path.
    pub function: Box<str>,
    pub rule: Rule,
    /// What the rule is said as for the effect: "reads the environment".
    pub says: &'static str,
    /// What the last step does: "calls `var`, which reads the environment".
    pub what: Box<str>,
    /// From the checked function to the one that does it, each step with where in its body it
    /// makes the next call (the last step's `at` is where it does `what`).
    pub chain: Box<[Step]>,
    /// The effect's advice, the message's last clause.
    pub fix: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: Box<str>,
    pub at: Option<Place>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub file: FileId,
    /// The file's path as the build names it; empty when it has none.
    pub path: Box<str>,
    pub line: u32,
    pub column: u32,
    /// Where in `file` it is written, as rustc's span of it (`line` and `column` are its start).
    pub range: span::TextRange,
}

impl Refusal {
    /// "`kernel::compute` is `#[blaze::pure]`, but reads the environment: `kernel::compute`
    /// (src/lib.rs:12) → `helper` (src/lib.rs:20), which calls `var`, which reads the
    /// environment; a pure function reads the outside world only through its arguments".
    pub fn said(&self) -> String {
        format!(
            "`{}` is `#[blaze::{}]`, but {}: {}; {}",
            self.function,
            self.effect,
            self.says,
            self.witness(),
            self.fix
        ) // alloc: said once, when a lint reports it
    }

    /// The chain and what its last step does: "`kernel::compute` (src/lib.rs:12) → `helper`
    /// (src/lib.rs:20), which calls `var`, which reads the environment".
    pub fn witness(&self) -> String {
        let steps: Vec<String> = self // alloc: said once, when a lint reports it
            .chain
            .iter()
            .map(|step| match &step.at {
                Some(at) if !at.path.is_empty() => {
                    format!("`{}` ({}:{})", step.name, at.path, at.line)
                } // alloc: as above
                _ => format!("`{}`", step.name), // alloc: as above
            })
            .collect();
        format!("{}, which {}", steps.join(" → "), self.what) // alloc: as above
    }
}

/// Whether the monomorphized instance `func<args>` is pure, with `krate`'s impls in scope (the
/// crate whose code makes the instance; a non-generic function's own crate). `args` has no
/// type parameter left.
pub fn fn_purity(db: &Db, krate: Crate, func: FunctionId, args: StoredGenericArgs) -> &FnPurity {
    let env = ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }.store();
    checked(db, env, func, args, 0, PURE.refuses.0, false)
}

/// The same of the definition, in its own where-clauses: a call only an instance decides (a
/// method of a type parameter, its drop) is [`EffectProof::open`], checked at each instance.
pub fn fn_purity_generic(db: &Db, func: FunctionId) -> &FnPurity {
    let (env, args) = generic(db, func);
    checked(db, env, func, args, 0, PURE.refuses.0, true)
}

/// `effect` of the instance `func<args>`, `krate`'s impls in scope.
pub fn check(
    db: &Db,
    krate: Crate,
    func: FunctionId,
    args: StoredGenericArgs,
    effect: &EffectDef,
) -> Result<EffectProof, Refusal> {
    let Some(index) = effect_index(effect) else { return Err(unregistered(db, func, effect)) };
    let env = ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }.store();
    // clone: a lint's answer, once per function
    checked(db, env, func, args, index as u8, effect.refuses.0, false)
        .as_result()
        .map(Clone::clone)
        .map_err(Clone::clone)
}

/// `effect` of the definition `func`, in its own where-clauses.
pub fn check_generic(
    db: &Db,
    func: FunctionId,
    effect: &EffectDef,
) -> Result<EffectProof, Refusal> {
    let Some(index) = effect_index(effect) else { return Err(unregistered(db, func, effect)) };
    let (env, args) = generic(db, func);
    // clone: a lint's answer, once per function
    checked(db, env, func, args, index as u8, effect.refuses.0, true)
        .as_result()
        .map(Clone::clone)
        .map_err(Clone::clone)
}

/// Whether `func` is pure with no `#[blaze::pure]` on it: its definition passes the check the
/// attribute asks for (the same walk and the same scan of its parameters), with no call left
/// to an instance. What keys on the attribute (a cache of its results) may key on this
/// instead where it holds: the proof is the same, only nobody wrote the word.
pub fn derived_pure(db: &Db, func: FunctionId) -> bool {
    matches!(fn_purity_generic(db, func), FnPurity::Pure(proof) if !proof.open)
}

/// Every kind of effect the instance `body<args>` may have, `krate`'s impls in scope, no trust
/// applied.
pub fn instance_effects(db: &Db, krate: Crate, body: Body, args: StoredGenericArgs) -> EffectSet {
    let env = ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }.store();
    summary(db, env, Instance { def: Def::Body(body), args }).kinds
}

/// What the instance `body<args>` touches (level 1 of the inferred facts), `krate`'s impls in
/// scope. Trust is not believed: a trusted function's body is walked.
pub fn fn_memory(db: &Db, krate: Crate, body: Body, args: StoredGenericArgs) -> MemoryEffects {
    let env = ParamEnvAndCrate { param_env: hir_ty::next_solver::ParamEnv::empty(), krate }.store();
    summary(db, env, Instance { def: Def::Body(body), args }).memory
}

/// The environment and arguments of `func` as defined: its where-clauses, its own parameters.
pub(crate) fn generic(db: &Db, func: FunctionId) -> (StoredParamEnvAndCrate, StoredGenericArgs) {
    let krate = func.krate(db);
    let param_env = db.trait_environment(func.into());
    let interner = hir_ty::next_solver::DbInterner::new_with(db, krate);
    let args = GenericArgs::identity_for_item(interner, func.into()).store();
    (ParamEnvAndCrate { param_env, krate }.store(), args)
}

/// An effect not in [`EFFECTS`]: a summary keeps no set of it.
fn unregistered(db: &Db, func: FunctionId, effect: &EffectDef) -> Refusal {
    Refusal {
        effect: effect.name,
        function: fn_path(db, func, &[]),
        rule: Rule::Unchecked,
        says: "is checked for an effect `hir_ty_pure::effects::EFFECTS` does not list",
        what: format!("asks for `{}`, which no summary records", effect.name).into(), // alloc: a refusal, once
        chain: Box::default(),
        fix: "add the effect to `EFFECTS`",
    }
}

/// The check of `EFFECTS[effect]`, refusing the kinds `refuses`, of `func<args>` in `env`;
/// `generic`: the definition's check, where [`Rule::Open`] is left to each instance.
#[blaze_query::tracked(returns(ref))]
fn checked(
    db: &Db,
    env: StoredParamEnvAndCrate,
    func: FunctionId,
    args: StoredGenericArgs,
    effect: u8,
    refuses: u32,
    generic: bool,
) -> FnPurity {
    let index = usize::from(effect);
    let def = EFFECTS[index];
    let refuses = EffectSet(refuses);
    let cx = Cx::new(db, env.clone()); // clone: a handle, reference counts
    let root = Instance { def: Def::Body(Body::Fn(func)), args: args.clone() }; // clone: reference counts
    let mut found = Found::default();
    if let Some((reason, door)) = effects::trusted(db, func, def) {
        found.trust(db, func, reason, door);
        return FnPurity::Pure(found.proof(1));
    }
    if let Some(refusal) = params(&cx, func, args.as_ref(), def, refuses, generic, &mut found) {
        return FnPurity::Impure(refusal);
    }
    let summary = summary(db, env.clone(), root.clone()); // clone: the query's key, reference counts
    let mut refused = summary.set(index).intersect(refuses);
    if generic {
        found.open |= summary.set(index).has(Rule::Open);
    } else if summary.set(index).has(Rule::Open) {
        refused = refused.union(EffectSet::one(Rule::Open));
    }
    // The kind with the fewest calls to it, the first in `Rule` order of those.
    let shortest = refused
        .iter()
        .min_by_key(|rule| summary.witness(index, *rule).map_or(u16::MAX, |it| it.depth));
    if let Some(rule) = shortest {
        return FnPurity::Impure(refusal(&cx, def, index, rule, &root, summary));
    }
    let instances = reach(db, &env, &root, def, &mut found);
    FnPurity::Pure(found.proof(instances))
}

/// What a check collects on the way: the trusted functions, and whether a call is open.
#[derive(Default)]
struct Found {
    // alloc: the proof's trusted functions, few, once per check
    trusted: Vec<Trusted>,
    open: bool,
}

impl Found {
    /// Keep `func` as trusted, once, by its item path: trust is the item's attribute (or its
    /// trait's), never an instance's, so the entry names the item whichever instance met it first.
    fn trust(&mut self, db: &Db, func: FunctionId, reason: Symbol, door: Option<TraitId>) {
        if self.trusted.iter().any(|it| it.func == func) {
            return;
        }
        let link = match func.loc(db).container {
            ItemContainerId::ExternBlockId(_) => Some(
                AttrFlags::link_name(db, func.into())
                    .unwrap_or_else(|| FunctionSignature::of(db, func).name.symbol().clone()), // clone: a handle of an interned name
            ),
            _ => None,
        };
        let path = crate::walk::fn_item_path(db, func).into_boxed_str();
        self.trusted.push(Trusted { func, path, reason, krate: func.krate(db), link, door });
    }

    fn proof(mut self, instances: u32) -> EffectProof {
        self.trusted.sort_by(|a, b| a.path.cmp(&b.path));
        EffectProof { trusted: self.trusted.into_boxed_slice(), open: self.open, instances }
    }
}

/// Every instance the root reaches for `def`, past none trusted for it (nor, for an effect that
/// holds of what returns, through one that never does), each trusted one met kept in `found`:
/// how many instances it reached.
fn reach(
    db: &Db,
    env: &StoredParamEnvAndCrate,
    root: &Instance,
    def: &EffectDef,
    found: &mut Found,
) -> u32 {
    let mut seen: HashSet<Instance, Global> = HashSet::new_in(Global);
    let mut stack: Vec<Instance> = Vec::new(); // alloc: the walk's frontier, once per check
    seen.insert(root.clone()); // clone: reference counts
    stack.push(root.clone()); // clone: reference counts
    while let Some(instance) = stack.pop() {
        let Def::Body(_) = instance.def else { continue };
        let summary = summary(db, env.clone(), instance); // clone: the query's key, reference counts
        for &func in &summary.trusted_calls {
            if let Some((reason, door)) = effects::trusted(db, func, def) {
                found.trust(db, func, reason, door);
            }
        }
        for site in &summary.sites {
            let callee = &site.instance;
            if let Def::Body(Body::Fn(func)) | Def::Leaf(func, _) = callee.def {
                if let Some((reason, door)) = effects::trusted(db, func, def) {
                    found.trust(db, func, reason, door);
                    continue;
                }
                if !def.through_never && never_returns(db, func) {
                    continue;
                }
            }
            if seen.insert(callee.clone()) {
                stack.push(callee.clone()); // clone: reference counts
            }
        }
    }
    seen.len() as u32
}

/// The refusal of `rule` for `def` (at `index` of [`EFFECTS`]) of `root`, whose summary is
/// `summary`: its witnesses followed down to where it is done.
fn refusal(
    cx: &Cx<'_>,
    def: &EffectDef,
    index: usize,
    rule: Rule,
    root: &Instance,
    summary: &Summary,
) -> Refusal {
    let db = cx.db;
    let mut chain: Vec<Step> = Vec::new(); // alloc: the refusal's chain, once
    let mut at = (root.clone(), summary); // clone: reference counts
    let mut what: Box<str> = rule.said().into();
    // A witness's depth falls by one each call: the bound is the root's. A recursion's does
    // not (`walk::RECURSION_DEPTH`): its chain ends where the next instance is one already in
    // it, so it is as long as the instances it names, and `CHAIN` at most.
    let bound = match rule {
        Rule::Recursion => CHAIN,
        _ => summary.witness(index, rule).map_or(0, |it| usize::from(it.depth)) + 1,
    };
    let mut named: Vec<Instance> = Vec::new(); // alloc: the instances of a recursion's chain
    for _ in 0..=bound {
        let (instance, summary) = (&at.0, at.1);
        if rule == Rule::Recursion {
            named.push(instance.clone()); // clone: reference counts
        }
        let name = instance_path(db, instance);
        let Some(witness) = summary.witness(index, rule) else {
            chain.push(Step { name, at: None });
            break;
        };
        match &witness.how {
            How::Own { what: did, span } => {
                chain.push(Step { name, at: span.and_then(|it| place(cx, instance, it)) });
                what = did.clone(); // clone: the refusal's text, once
                break;
            }
            How::Site { site, said } => {
                let Some(site) = summary.sites.get(*site as usize) else {
                    chain.push(Step { name, at: None });
                    break;
                };
                chain.push(Step { name, at: site.span.and_then(|it| place(cx, instance, it)) });
                if let Some(said) = said {
                    what = said.clone(); // clone: the refusal's text, once
                    break;
                }
                let next = site.instance.clone(); // clone: reference counts
                if rule == Rule::Recursion && named.contains(&next) {
                    // alloc: the refusal's text, once
                    what = format!(
                        "calls `{}`, which is called again from here: the calls go round",
                        instance_path(db, &next)
                    )
                    .into();
                    break;
                }
                let Def::Body(_) = next.def else {
                    what = format!("calls {}", instance_path(db, &next)).into(); // alloc: the refusal's text, once
                    break;
                };
                let callee = crate::walk::summary(db, cx.stored.clone(), next.clone()); // clone: the query's key
                at = (next, callee);
            }
        }
    }
    Refusal {
        effect: def.name,
        function: instance_path(db, root),
        rule,
        says: (def.says)(rule),
        what,
        chain: chain.into_boxed_slice(),
        fix: def.fix,
    }
}

/// The most steps a recursion's chain names before it stops short.
const CHAIN: usize = 64;

/// Where `span` of `instance`'s body is written: the body's own source when the span is of a
/// body inlined into it.
fn place(cx: &Cx<'_>, instance: &Instance, span: MirSpan) -> Option<Place> {
    let db = cx.db;
    let body = body_of(db, &cx.stored, instance)?.ok()?;
    let (span, _) = source_span(body, span);
    let owner = match body.owner {
        InferBodyId::DefWithBodyId(owner) => owner,
        InferBodyId::AnonConstId(_) => return None,
    };
    let (file, line, column) = source_location_in_file(db, owner, span)?;
    let (_, range) = source_file_range(db, owner, span)?;
    let path =
        source_path(db, hir_def::HasModule::krate(&owner, db), file).unwrap_or_default().into();
    Some(Place { file, path, line, column, range })
}

/// An instance by path, as a refusal names it: `kernel::compute`, `kernel::compute::{closure}`.
fn instance_path(db: &Db, instance: &Instance) -> Box<str> {
    let owner = |owner: InferBodyId<'_>| match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => fn_path(db, func, &[]),
        InferBodyId::DefWithBodyId(_) | InferBodyId::AnonConstId(_) => "a constant".into(),
    };
    match instance.def {
        Def::Body(Body::Fn(func)) | Def::Leaf(func, _) => {
            fn_path(db, func, instance.args.as_ref().as_slice())
        }
        Def::Body(Body::Closure(id)) => {
            format!("{}::{{closure}}", owner(InternedClosureId::from_id(id).loc(db).owner)).into() // alloc: a refusal's name
        }
        Def::Body(Body::Coroutine(id) | Body::CoroutineByMove(id)) => {
            format!("{}::{{async block}}", owner(InternedCoroutineId::from_id(id).loc(db).owner))
                .into() // alloc: a refusal's name
        }
    }
}

/// `func` by path, crate first, with `args` where its symbol has them.
fn fn_path<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[hir_ty::next_solver::GenericArg<'db>],
) -> Box<str> {
    let spelled = blaze_query::db::attach(db, || hir_ty_print::rustc_function_name(db, func, args));
    match spelled {
        Some(path) => path.into(),
        None => FunctionSignature::of(db, func).name.as_str().into(),
    }
}

/// The root's parameters, of `func<args>`: the first kind of trouble `refuses` has, as a
/// refusal; a `dyn` of a trait trusted for `def` and type parameters (a generic check) noted in
/// `found`.
fn params<'db>(
    cx: &Cx<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
    def: &EffectDef,
    refuses: EffectSet,
    generic: bool,
    found: &mut Found,
) -> Option<Refusal> {
    let db = cx.db;
    let sig =
        db.callable_item_signature(func.into()).instantiate(cx.interner, args).skip_norm_wip();
    let mut scan =
        Params { cx, def, generic, seen: HashSet::new_in(Global), trouble: None, refuses, found };
    for input in sig.skip_binder().inputs().iter().copied() {
        scan.ty(input, false, false);
        if scan.trouble.is_some() {
            break;
        }
    }
    let (rule, what) = scan.trouble?;
    let name = fn_path(db, func, args.as_slice());
    Some(Refusal {
        effect: def.name,
        function: name.clone(), // clone: a refusal's name, once
        rule,
        says: (def.says)(rule),
        what: what.into(),
        chain: Box::new([Step { name, at: None }]),
        fix: def.fix,
    })
}

/// The scan of the root's parameter types.
struct Params<'a, 'db> {
    cx: &'a Cx<'db>,
    def: &'a EffectDef,
    generic: bool,
    seen: HashSet<(Ty<'db>, bool, bool), Global>,
    trouble: Option<(Rule, String)>,
    refuses: EffectSet,
    found: &'a mut Found,
}

impl<'db> Params<'_, 'db> {
    fn refuse(&mut self, rule: Rule, what: impl FnOnce() -> String) {
        if self.trouble.is_none() && self.refuses.has(rule) {
            self.trouble = Some((rule, blaze_query::db::attach(self.cx.db, what)));
        }
    }

    /// Whether the effect refuses reading state that changes (`pure`), so memory the check
    /// can't see (behind a raw pointer, a union's live field) is refused for it.
    fn memory_matters(&self) -> bool {
        self.refuses.has(Rule::MutableState)
    }

    /// `ty`, reached behind a shared pointer when `shared`, directly in a toolchain type's
    /// field when `toolchain`.
    fn ty(&mut self, ty: Ty<'db>, shared: bool, toolchain: bool) {
        let cx = self.cx;
        let ty = cx.normalize(ty);
        if self.trouble.is_some() || !self.seen.insert((ty, shared, toolchain)) {
            return;
        }
        if ty.has_param() && matches!(ty.kind(), TyKind::Param(_) | TyKind::Alias(..)) {
            // Each instance is checked with its own.
            if self.generic {
                self.found.open = true;
            } else {
                self.refuse(Rule::Unchecked, || {
                    format!("takes `{}`, a type the check can't see into", cx.shown(ty))
                });
            }
            return;
        }
        // Interior mutability is state another thread changes, which only an effect that reads
        // state refuses; for the others the type is still scanned for code it hides.
        if shared && self.memory_matters() && !is_freeze(&cx.infcx, ty, cx.env.param_env) {
            return self.refuse(Rule::MutableState, || {
                format!("takes `{}` behind a shared reference, state with interior mutability that another thread can change during the call", cx.shown(ty))
            });
        }
        match ty.kind() {
            TyKind::Ref(_, pointee, rustc_type_ir::Mutability::Not) => self.ty(pointee, true, false),
            TyKind::Ref(_, pointee, rustc_type_ir::Mutability::Mut) => self.ty(pointee, shared, false),
            // What memory holds hides no code: an effect that doesn't read state (`noalloc`,
            // `nopanic`, `nonblocking`) looks through a raw pointer as through a reference, for
            // a `dyn` or a function pointer behind it.
            TyKind::RawPtr(pointee, _) if toolchain || !self.memory_matters() => self.ty(pointee, true, false),
            TyKind::RawPtr(..) => self.refuse(Rule::Unchecked, || {
                format!("takes the raw pointer `{}`, whose memory the check can't see: take a reference", cx.shown(ty))
            }),
            TyKind::FnPtr(..) => self.refuse(Rule::Unchecked, || {
                format!("takes the function pointer `{}`, whose calls the check can't see: take a type parameter `F: Fn(..)`", cx.shown(ty))
            }),
            TyKind::Dynamic(bounds, _) => {
                let door = bounds.principal().map(|it| it.skip_binder().def_id.0);
                // Every method of the trait is held, in each impl, within a `#[blaze::may(..)]`
                // that leaves this effect's word out: a call through the `dyn` cannot have it.
                if door.is_some_and(|it| bounded_trait(cx.db, it, self.def)) {
                    return;
                }
                match door.and_then(|it| trusted_trait(cx.db, it, self.def).map(|why| (it, why))) {
                    Some((trait_, why)) => {
                        for (_, item) in trait_.trait_items(cx.db).items.iter() {
                            if let hir_def::AssocItemId::FunctionId(method) = *item {
                                self.found.trust(cx.db, method, why.clone(), Some(trait_)); // clone: a handle of an interned text
                            }
                        }
                    }
                    None => self.refuse(Rule::Unchecked, || {
                        format!("takes `{}`, whose calls the check can't see: take a type parameter, or mark the trait trusted", cx.shown(ty))
                    }),
                }
            }
            TyKind::Adt(adt, adt_args) => {
                let id = adt.def_id();
                let home = toolchain_crate(cx.db, HasModule::krate(&id, cx.db));
                if let AdtId::UnionId(_) = id
                    && !home
                    && self.memory_matters()
                {
                    return self.refuse(Rule::Unchecked, || {
                        format!("takes the union `{}`, whose live field the check can't know", cx.shown(ty))
                    });
                }
                // `Unique<T>` and `PhantomData<T>` own a `T` (a `Box`'s, a `Vec`'s elements).
                if home && let AdtId::StructId(it) = id {
                    let name = StructSignature::of(cx.db, it).name.symbol().clone(); // clone: a handle of an interned name
                    if matches!(name.as_str(), "Unique" | "PhantomData") {
                        if let Some(owned) = adt_args.types().next() {
                            self.ty(owned, shared, false);
                        }
                        return;
                    }
                }
                for variant in variants(cx.db, id) {
                    for (_, field) in cx.db.field_types(variant).iter() {
                        let field = field.ty().instantiate(cx.interner, adt_args).skip_norm_wip();
                        self.ty(field, shared, home);
                    }
                }
            }
            TyKind::Array(element, _) | TyKind::Slice(element) => self.ty(element, shared, false),
            // A pattern type is its base type narrowed, in the same place: core's `NonNull<T>`
            // holds `pattern_type!(*const T is !null)`, a raw pointer in a toolchain type's field
            // (a `Vec`'s buffer), not one the function takes of its own.
            TyKind::Pat(base, _) => self.ty(base, shared, toolchain),
            TyKind::Tuple(fields) => {
                for field in fields.iter() {
                    self.ty(field, shared, false);
                }
            }
            TyKind::Closure(_, closure_args) => self.ty(closure_args.as_closure().tupled_upvars_ty(), shared, false),
            _ => {}
        }
    }
}

/// Whether every method of `trait_` (it has one) is bounded for `def` by a `#[blaze::may(..)]`
/// on it or on the trait ([`effects::bound_mask`]).
fn bounded_trait(db: &Db, trait_: TraitId, def: &EffectDef) -> bool {
    let Some(index) = effect_index(def) else { return false };
    let mut methods = trait_.trait_items(db).items.iter().filter_map(|(_, item)| match *item {
        hir_def::AssocItemId::FunctionId(method) => Some(method),
        _ => None,
    });
    let mut any = false;
    let all = methods.all(|method| {
        any = true;
        effects::bound_mask(db, method) & 1 << index != 0
    });
    any && all
}

/// Whether `krate` is the toolchain's (core, alloc, std and their kin).
fn toolchain_crate(db: &Db, krate: Crate) -> bool {
    matches!(krate.data(db).origin, CrateOrigin::Lang(_))
}

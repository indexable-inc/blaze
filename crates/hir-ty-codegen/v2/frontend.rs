//! The front end of a v2 build over the database: what each instance and static is by
//! content, whose code it is, and what it refers to.

use std::sync::atomic::{AtomicBool, Ordering};

use base_db::{Crate, CrateOrigin, Db};
use blaze_codegen::{
    CodegenError, Owner,
    backend::{Address, Described, DescribedStatic, FrontRef, Frontend},
    plan::{Mode, Origin, Package, PlanError, Policy},
};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemId, ItemKey};
use blaze_query::db::FromId;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, GenericDefId,
    GenericParamId, HasModule, Lookup, attrs::AttrFlags, expr_store::ExpressionStore,
    signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::{HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{DbInterner, GenericArgs, ParamEnv, Region, RegionExt, Ty, TyKind},
};
use hir_ty_mir::mir::{MirOptions, overflow::crate_checks_overflow};
use hir_ty_share::{BodyOptions, OptionsSet, ShareDef, ShareInstance, Unshared};
use rustc_type_ir::inherent::{
    BoundExistentialPredicates as _, GenericArgs as _, IntoKind, SliceLike as _,
};
use smallvec::{SmallVec, smallvec};

use super::{
    Body, BuildTime, HashSource, Hashes, HirInstance, HirStatic, Kind, build::Build, refs,
    undescribed,
};
use crate::{
    Instance, InstanceDef, exported_static, exported_symbol, fncache, home::Homes, linked_symbol,
};

/// The build's database, `Sync` and shared by reference by every task. Nothing here locks:
/// each call has its own [`Homes`], which lives as long as the task's work on one instance.
pub struct Databases<'db> {
    db: &'db Db,
}

impl<'db> Databases<'db> {
    pub fn new(db: &'db Db) -> Self {
        Databases { db }
    }

    /// Runs `work` with the database attached to its thread, which any formatted type or
    /// constant (an error's message, a panic's) needs, on whichever worker the task is.
    pub(crate) fn with<T>(&self, work: impl FnOnce(&Db, &mut Homes) -> T) -> T {
        hir_ty::attach_db(self.db, || work(self.db, &mut Homes::default()))
    }
}

/// The check of the bodies a build compiles, run beside it: told what the walk reaches, and
/// whether the build's outcome is already its failure.
pub trait BodyChecks: Sync {
    /// Check these first: bodies the build reached. Any the check does not hold is passed over.
    fn reached(&self, bodies: &[DefWithBodyId]);

    /// Whether a check failed: what the build makes is then not used.
    fn stopped(&self) -> bool;
}

/// The front end of a v2 build.
pub struct HirFrontend<'db> {
    pub(crate) databases: Databases<'db>,
    pub(crate) hashes: HashSource,
    pub(crate) build_time: std::sync::Arc<BuildTime>,
    /// How each origin's units build bodies: what a function's references are walked for and
    /// what a generic is shared under.
    units: Option<Units>,
    // dyn: `reached` once a level of the walk, `stopped` once a function compiled (2 ns
    // against its 50 to 200 µs), never per instruction
    checks: Option<&'db dyn BodyChecks>, // dyn: never per instruction, as above
    /// Set when the build's job is cancelled or gives way to another task.
    stop: Option<&'db AtomicBool>,
}

/// One value for each origin of the build's [`Policy`], told apart as the plan tells them.
struct ByOrigin<T> {
    /// A member's, a dependency's, build-time code's, of a package the profile names no
    /// override for.
    member: T,
    dependency: T,
    build_time: T,
    /// Of a package the profile names (`[profile.*.package.<name>]`), the same three.
    packages: HashMap<Package, [T; 3]>,
    std: T,
}

impl<T> ByOrigin<T> {
    /// Made once a build.
    fn of(policy: &Policy, mut each: impl FnMut(&Origin) -> T) -> ByOrigin<T> {
        let mut kinds = |name: Package| {
            [each(&Origin::Member(name)), each(&Origin::Dependency(name)), each(&Origin::BuildTime(name))]
        };
        // No package is named the empty name: these are of one the profile has nothing for.
        let [member, dependency, build_time] = kinds(Package::new(""));
        let packages: HashMap<_, _> = (policy.profile.packages.keys())
            .filter(|it| *it != "*")
            .map(|name| {
                let name = Package::new(name);
                (name, kinds(name))
            })
            .collect(); // alloc: once a build, sized by the packages the profile names
        let std = each(&Origin::Std);
        ByOrigin { member, dependency, build_time, packages, std }
    }

    fn map<U>(&self, mut f: impl FnMut(&T) -> U) -> ByOrigin<U> {
        let (member, dependency, build_time) =
            (f(&self.member), f(&self.dependency), f(&self.build_time));
        let packages = (self.packages.iter())
            .map(|(name, kinds)| (*name, kinds.each_ref().map(&mut f)))
            .collect(); // alloc: once a build, sized by the packages the profile names
        ByOrigin { member, dependency, build_time, packages, std: f(&self.std) }
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        [&self.member, &self.dependency, &self.build_time, &self.std]
            .into_iter()
            .chain(self.packages.values().flatten())
    }

    fn get(&self, origin: &Origin) -> &T {
        let (name, kind) = match origin {
            Origin::Std => return &self.std,
            Origin::Member(name) => (name, 0),
            Origin::Dependency(name) => (name, 1),
            Origin::BuildTime(name) => (name, 2),
        };
        match self.packages.get(name) {
            Some(kinds) => &kinds[kind],
            None => [&self.member, &self.dependency, &self.build_time][kind],
        }
    }
}

/// How the units of each origin of the build's [`Policy`] build bodies. Made once a build,
/// choosing for each origin as the plan does.
struct Units {
    /// Whether some unit of the origin MIR-inlines (`Spec::mir_inline`), at the first spec or
    /// one a build tiers it up to.
    inlines: ByOrigin<Result<bool, PlanError>>,
    /// Some origin's units inline: what a generic, which every origin may ask for, is walked
    /// with.
    any: bool,
    /// Every options a unit of the origin may build a body with ([`Build::options`]), by
    /// whether the body is of the library's crates: what a generic it asks for is shared
    /// under ([`hir_ty_share::share_key`]).
    options: ByOrigin<Result<BodyOptions, String>>,
}

impl Units {
    fn of(db: &Db, policy: &Policy) -> Units {
        let builds = ByOrigin::of(policy, |origin| builds_of(policy, origin));
        let inlines = builds.map(|it| match it {
            Ok(builds) => Ok(builds.iter().any(|it| it.inlines)),
            Err(e) => Err(e.clone()), // clone: once a build, an error
        });
        let any = inlines.values().any(|it| it == &Ok(true));
        let crates = base_db::all_crates(db);
        let mut by_build: HashMap<Build, Asked> = HashMap::default();
        let options = builds.map(|it| -> Result<BodyOptions, String> {
            let builds = it.as_ref().map_err(ToString::to_string)?;
            let mut asked = Asked::default();
            for &build in builds {
                let of = by_build.entry(build).or_insert_with(|| asked_of(db, &crates, build));
                asked.add(of);
            }
            Ok(BodyOptions { lang: options_set(&asked.lang)?, other: options_set(&asked.other)? })
        });
        Units { inlines, any, options }
    }
}

/// How the units of `origin` build bodies: at its first spec, at the spec a live build tiers
/// it up to, and in a tiered run at LLVM's, which optimizes (`blaze_stencil::tier::Tier::Llvm`).
fn builds_of(policy: &Policy, origin: &Origin) -> Result<SmallVec<[Build; 3]>, PlanError> {
    let choice = policy.choose(origin, None)?;
    let first = Build::of(&choice.first);
    let mut builds: SmallVec<[Build; 3]> = smallvec![first]; // alloc: none, inline
    builds.extend(choice.then.as_ref().map(Build::of));
    if policy.mode == Mode::Tiered && policy.toolchain.llvm.is_some() {
        builds.push(Build { optimized: true, ..first });
    }
    Ok(builds)
}

/// The options bodies are built with, each once: of the library's crates and of the rest.
#[derive(Default)]
struct Asked {
    lang: SmallVec<[MirOptions; 2]>,
    other: SmallVec<[MirOptions; 2]>,
}

impl Asked {
    fn push(&mut self, lang: bool, options: MirOptions) {
        let class = if lang { &mut self.lang } else { &mut self.other };
        if !class.contains(&options) {
            class.push(options);
        }
    }

    fn add(&mut self, other: &Asked) {
        other.lang.iter().for_each(|&it| self.push(true, it));
        other.other.iter().for_each(|&it| self.push(false, it));
    }
}

/// Every [`Build::options`] `build` gives a body of each crate of the graph, compiled in its
/// own crate or in another, as an instance is in the crate [`Homes`] picks for it.
fn asked_of(db: &Db, crates: &[Crate], build: Build) -> Asked {
    let mut asked = Asked::default();
    for (at, &written) in crates.iter().enumerate() {
        let lang = written.data(db).origin.is_lang();
        let elsewhere = crates.get(usize::from(at == 0)).copied().filter(|it| *it != written);
        for compiling in [Some(written), elsewhere].into_iter().flatten() {
            asked.push(lang, build.options(db, written, compiling));
        }
    }
    asked
}

/// `options` as the share key takes them. The set must hold each whole: options it can't tell
/// apart would key two bodies as one.
fn options_set(options: &[MirOptions]) -> Result<OptionsSet, String> {
    let set = OptionsSet::of(options);
    match options.iter().find(|it| !set.iter().any(|kept| kept == **it)) {
        None => Ok(set),
        // alloc: once a build, an error
        Some(lost) => Err(format!(
            "`hir_ty_share::OptionsSet` can't hold the MIR options {lost:?} a unit builds bodies \
             with, so no generic can be shared by what its code reads"
        )),
    }
}

const NO_POLICY: &str = "the front end was made with no policy (`HirFrontend::with_policy`), so \
                         it can't tell how the instance's units build its MIR";

impl<'db> HirFrontend<'db> {
    /// `build_time` is shared: a daemon makes one a job and hands it to each unit's front end.
    pub fn new(databases: Databases<'db>, hashes: HashSource, build_time: std::sync::Arc<BuildTime>) -> Self {
        HirFrontend { databases, hashes, build_time, units: None, checks: None, stop: None }
    }

    /// The front end of a build that stops before its next function once `stop` is set: its
    /// job was cancelled, or the plane asked its task to give way. Checked or not.
    pub fn with_stop(mut self, stop: &'db AtomicBool) -> Self {
        self.stop = Some(stop);
        self
    }

    /// The front end of a build whose unit's bodies `checks` checks beside it.
    // dyn: called once a level of the walk, never per instance
    pub fn with_checks(mut self, checks: &'db dyn BodyChecks) -> Self {
        self.checks = Some(checks);
        self
    }

    /// The front end of a build compiled as `policy` says: what describing and preparing a
    /// function need, which list what its code calls as the units of its origin inline it.
    pub fn with_policy(mut self, policy: &Policy) -> Self {
        self.units = Some(Units::of(self.databases.db, policy));
        self
    }

    /// Whether a unit of `instance` MIR-inlines bodies of crates other than the library's,
    /// whose code then calls what they call: one of its crate's origin when it is not
    /// generic, else of any origin, as any may ask for it. What its references are walked
    /// for ([`refs::walk`]).
    pub(crate) fn inlines(
        &self,
        db: &Db,
        instance: &HirInstance,
    ) -> Result<bool, String> {
        self.inlines_as(db, instance, None)
    }

    /// [`Self::inlines`], with the origin of the instance's crate when the caller has it.
    fn inlines_as(
        &self,
        db: &Db,
        instance: &HirInstance,
        origin: Option<&Origin>,
    ) -> Result<bool, String> {
        let Some(units) = &self.units else { return Err(NO_POLICY.to_owned()) }; // alloc: an error
        let of_origin = match crate_of(db, &instance.0) {
            Some(krate) if !generic(&instance.0) => match origin {
                Some(origin) => units.inlines.get(origin),
                None => units.inlines.get(&self.origin(db, krate)),
            },
            _ => return Ok(units.any),
        };
        of_origin.as_ref().copied().map_err(|it| it.to_string()) // alloc: the error's message
    }

    /// A function that is not generic, as a root of the build: `main`, a test, an exported
    /// function.
    pub fn root(&self, func: FunctionId) -> HirInstance {
        self.databases.with(|db, homes| root_in(db, homes, func))
    }

    /// A root of `func` whose type parameters are, in order, the function items `items` (each
    /// with no generic arguments of its own), every region erased: a proc macro crate's
    /// `Client::expand1::<F>` or `expand2::<F>` for its `#[proc_macro*]` function `F`. An error
    /// names `func` when it has a const parameter or a type parameter count other than
    /// `items.len()`.
    pub fn root_with_fn_items(&self, func: FunctionId, items: &[FunctionId]) -> Result<HirInstance, String> {
        self.databases.with(|db, homes| root_with_fn_items_in(db, homes, func, items))
    }

    /// The C `main` of a program whose `fn main` is `main`.
    pub fn start(&self, main: FunctionId) -> HirInstance {
        HirInstance(Kind::Start(main))
    }

    /// The allocator shim of the program of `krate`, with the `#[global_allocator]`'s
    /// functions it calls: roots of a program with a `main`, and of a `cdylib`.
    pub fn allocator_shim(&self, krate: Crate) -> Result<Vec<HirInstance>, CodegenError> {
        self.with(|db, _| {
            super::allocator::shim(db, krate, |func| self.root(func))
                // alloc: the error's message
                .map_err(|what| undescribed(db, &krate, None, format!("its allocator shim: {what}")))
        })
    }

    pub(crate) fn with<T>(&self, work: impl FnOnce(&Db, &mut Homes) -> T) -> T {
        self.databases.with(work)
    }

    fn described(
        &self,
        db: &Db,
        homes: &mut Homes,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Result<Described<HirInstance, HirStatic>, CodegenError> {
        let hashes = &self.hashes;
        let item = match &instance.0 {
            Kind::Asm(text) => Ok(ItemKey(ContentId::of(ASM_ITEM_DOMAIN, text.as_bytes()))),
            Kind::Allocator(forward) => Ok(super::allocator::item(forward)),
            kind => item_of(db, kind).and_then(|item| hashes.item(db, item)),
        }
        .map_err(|what| undescribed(db, instance, None, what))?;
        let fail = |what: String| undescribed(db, instance, Some(item), what);
        let code = code_of(db, hashes, &instance.0).map_err(fail)?;
        let generic = generic(&instance.0);
        let origin = match crate_of(db, &instance.0) {
            Some(krate) if !generic => self.origin(db, krate),
            _ => *requester,
        };
        let args = match generic {
            true => self.shared_args(db, &instance.0, &origin),
            false => args_of(db, hashes, &instance.0),
        }
        .map_err(fail)?;
        let owner = self.owner(db, &instance.0, requester);
        let plain = if generic { args_of(db, hashes, &instance.0).map_err(fail)? } else { args };
        let identity = identity(db, &instance.0, item, plain);
        let bound = bound(db, &instance.0, code, plain);
        let overflow_checks = if generic { written_checks(db, &instance.0) } else { None };
        let symbol = symbol_of(db, &instance.0);
        let inlined = self.inlines_as(db, instance, Some(&origin)).map_err(fail)?;
        let refs = refs::walk(db, instance, inlined, homes).map_err(fail)?;
        // alloc: `Described::built_in` owns its list, which the memo's is borrowed from
        let built_in = refs::built_in(db, instance, inlined).map_err(fail)?.to_vec();
        Ok(Described {
            item,
            code,
            args,
            origin,
            overflow_checks,
            address: if symbol.is_some() { Address::Significant } else { Address::Free },
            symbol,
            name: super::linked::debug_name(db, &instance.0),
            refs,
            built_in,
            owner,
            identity,
            bound,
        })
    }

    /// A generic's arguments as its code reads them, compiled as `origin` asks: its share key
    /// under every options a unit of `origin` builds a body with, or, for an instance that is
    /// per instance by what it is ([`per_instance`]), its arguments' content ID
    /// ([`args_of`]). Both give exact code. A key that can't be made for any other reason is
    /// an error: the instance is never compiled per instance in its place. A generic callee is
    /// of its caller's origin, so its key here is the one its caller's key holds.
    fn shared_args(&self, db: &Db, kind: &Kind, origin: &Origin) -> Result<ContentId, String> {
        let Kind::Code(instance, env) = kind else { return args_of(db, &self.hashes, kind) };
        let options = self.body_options(origin)?;
        // clone: the query's key, a reference count
        let shared = ShareInstance { def: share_def(instance.def), args: instance.args.clone() };
        match hir_ty_share::share_key(db, &shared, env, options) {
            Ok(key) => Ok(Hasher::new(SHARED_ARGS_DOMAIN).id(&key.0).finish()),
            Err(why) if per_instance(why) => args_of(db, &self.hashes, kind),
            // alloc: the error's message
            Err(why) => Err(hir_ty::attach_db(db, || format!("no share key for {instance:?}: {why}"))),
        }
    }

    /// Every options a unit of `origin` builds a body with: what a generic it asks for is
    /// shared under.
    pub(super) fn body_options(&self, origin: &Origin) -> Result<BodyOptions, String> {
        match &self.units {
            Some(units) => units.options.get(origin).clone(), // clone: a copy, or an error
            None => Err(NO_POLICY.to_owned()), // alloc: an error
        }
    }

    /// Whose code a crate's is, for its settings.
    pub(crate) fn origin(&self, db: &Db, krate: Crate) -> Origin {
        let data = krate.data(db);
        let name = match &data.origin {
            CrateOrigin::Lang(_) => return Origin::Std,
            CrateOrigin::Local { name: Some(name), .. } | CrateOrigin::Library { name, .. } => {
                Package::new(name.as_str())
            }
            CrateOrigin::Local { name: None, .. } | CrateOrigin::Rustc { .. } => {
                Package::new(&fncache::crate_name(db, krate))
            }
        };
        if self.build_time.0.contains(&krate) {
            return Origin::BuildTime(name);
        }
        match data.origin {
            CrateOrigin::Local { .. } => Origin::Member(name),
            _ => Origin::Dependency(name),
        }
    }

    /// Whether a crate's code is the workspace's for a dependency image: a member's or a
    /// build-time crate's, never the standard library's.
    fn workspace(&self, db: &Db, krate: Crate) -> bool {
        match krate.data(db).origin {
            CrateOrigin::Lang(_) => false,
            CrateOrigin::Local { .. } => true,
            _ => self.build_time.0.contains(&krate),
        }
    }

    /// Whose an instance is for a dependency image ([`Owner`]): the workspace's when its item
    /// is or any of its type arguments names a workspace crate's type, closure or item. A
    /// shim's item is the standard library's (`drop_in_place`, `FnOnce::call_once`), so its
    /// type arguments decide; an `asm!` function is its asker's, the allocator shim the
    /// program's.
    fn owner(&self, db: &Db, kind: &Kind, requester: &Origin) -> Owner {
        let ours = |krate: Crate| self.workspace(db, krate);
        let instance = match kind {
            Kind::Code(instance, _) => instance,
            Kind::Start(main) if ours(main.krate(db)) => return Owner::Workspace,
            Kind::Start(_) => return Owner::Dependency,
            Kind::Asm(_) => {
                return match requester {
                    Origin::Member(_) | Origin::BuildTime(_) => Owner::Workspace,
                    Origin::Dependency(_) | Origin::Std => Owner::Dependency,
                };
            }
            Kind::Allocator(_) => return Owner::Workspace,
        };
        let item = match instance.def {
            InstanceDef::VtableShim(func) => Some(func.krate(db)),
            _ => crate_of(db, kind),
        };
        let named = || {
            (instance.args.as_ref().iter())
                .flat_map(|arg| arg.walk())
                .filter_map(|arg| arg.ty())
                .any(|ty| names_workspace(db, ty, &ours))
        };
        if item.is_some_and(&ours) || named() { Owner::Workspace } else { Owner::Dependency }
    }

    fn described_static(
        &self,
        db: &Db,
        homes: &mut Homes,
        value: &HirStatic,
    ) -> Result<DescribedStatic<HirInstance, HirStatic>, CodegenError> {
        let hashes = &self.hashes;
        let item = hashes
            .item(db, ItemId::Static(value.0))
            .map_err(|what| undescribed(db, value, None, what))?;
        let held = super::statics::value(db, homes, hashes, value.0)
            .map_err(|what| undescribed(db, value, Some(item), what))?;
        Ok(DescribedStatic {
            item,
            instance: ContentId::default(),
            value: held.value,
            mutable: held.mutable,
            thread_local: held.thread_local,
            section: held.section,
            origin: self.origin(db, value.0.krate(db)),
            // alloc: only an exported static, whose `DescribedStatic::symbol` owns its name
            symbol: exported_static(db, value.0).map(|it| it.as_str().to_owned()),
            refs: held.refs,
        })
    }
}

impl Frontend for HirFrontend<'_> {
    type Instance = HirInstance;
    type Static = HirStatic;

    fn describe(
        &self,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Result<Described<HirInstance, HirStatic>, CodegenError> {
        self.with(|db, homes| self.described(db, homes, instance, requester))
    }

    fn describe_static(
        &self,
        value: &HirStatic,
    ) -> Result<DescribedStatic<HirInstance, HirStatic>, CodegenError> {
        self.with(|db, homes| self.described_static(db, homes, value))
    }

    fn reached(&self, functions: &[(HirInstance, Origin)], statics: &[HirStatic]) {
        let Some(checks) = self.checks else { return };
        let bodies: Vec<DefWithBodyId> = self.with(|db, _| {
            (functions.iter().filter_map(|(instance, _)| body_of(db, instance)))
                .chain(statics.iter().map(|it| DefWithBodyId::StaticId(it.0)))
                // alloc: once a level of the walk, sized by it
                .collect()
        });
        checks.reached(&bodies);
    }

    fn stopped(&self) -> bool {
        self.stop.is_some_and(|it| it.load(Ordering::Relaxed)) || self.checks.is_some_and(|it| it.stopped())
    }
}

/// The body whose check an instance's code waits on: a function's, or that of the item a
/// closure or coroutine is written in. `None` for a shim, `asm!`, the allocator's and a closure
/// in an anonymous constant, which their module's item checks cover.
fn body_of(db: &Db, instance: &HirInstance) -> Option<DefWithBodyId> {
    match &instance.0 {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => Some(func.into()),
            def @ (InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_)) => match closure_loc(db, def)?.owner {
                InferBodyId::DefWithBodyId(owner) => Some(owner),
                InferBodyId::AnonConstId(_) => None,
            },
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => None,
        },
        Kind::Start(main) => Some((*main).into()),
        Kind::Asm(_) | Kind::Allocator(_) => None,
    }
}

impl HirInstance {
    /// Whether an edit of `body` alone may change this instance's key: its code reads
    /// `body`'s ([`code_of`]: `body` itself, a closure or coroutine written in it, the start
    /// of a `main` that is it, a shim adapting it), or its type arguments name a closure or
    /// coroutine written in it (`block_on::<its future>`, its drop glue). No other instance's
    /// key reads a body (docs/v2/plan/lanes/firewall/notes.md).
    pub fn of_body(&self, db: &Db, body: DefWithBodyId) -> bool {
        let instance = match &self.0 {
            Kind::Code(instance, _) => instance,
            Kind::Start(main) => return DefWithBodyId::from(*main) == body,
            Kind::Asm(_) | Kind::Allocator(_) => return false,
        };
        let is_body = |func: FunctionId| DefWithBodyId::from(func) == body;
        // A closure or coroutine whose body is `owner` is written in `body`, or in an
        // anonymous constant of its expressions.
        let owned_by = |owner: InferBodyId<'_>| {
            owner.expression_store_owner(db) == ExpressionStoreOwnerId::Body(body)
        };
        let own = match instance.def {
            InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => is_body(func),
            def @ (InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_)) => {
                closure_loc(db, def).is_some_and(|loc| owned_by(loc.owner))
            }
            InstanceDef::ReifyShim { .. } => matches!(
                instance.args.as_ref().type_at(0).kind(),
                TyKind::FnDef(def, _) if matches!(def.0, CallableDefId::FunctionId(func) if is_body(func))
            ),
            InstanceDef::DropGlue | InstanceDef::CallOnceShim | InstanceDef::FnPointerShim => false,
        };
        own || (instance.args.as_ref().iter())
            .flat_map(|arg| arg.walk())
            .filter_map(|arg| arg.ty())
            .any(|ty| match ty.kind() {
                TyKind::Closure(id, _) => owned_by(id.0.loc(db).owner),
                TyKind::CoroutineClosure(id, _) => owned_by(id.0.loc(db).owner),
                TyKind::Coroutine(id, _) | TyKind::CoroutineWitness(id, _) => {
                    owned_by(id.0.loc(db).owner)
                }
                _ => false,
            })
    }
}

/// [`HirFrontend::root`] in a task's `homes`. Each lifetime parameter is erased, as in the
/// instance a call names, so the root and the call are one instance.
pub(crate) fn root_in(db: &Db, homes: &mut Homes, func: FunctionId) -> HirInstance {
    let interner = DbInterner::new_no_crate(db);
    let erased = Region::new_erased(interner);
    rooted(db, homes, func, GenericArgs::for_item(interner, func.into(), |_, _, _, _| erased.into()))
}

/// [`HirFrontend::root_with_fn_items`].
fn root_with_fn_items_in(
    db: &Db,
    homes: &mut Homes,
    func: FunctionId,
    items: &[FunctionId],
) -> Result<HirInstance, String> {
    let interner = DbInterner::new_no_crate(db);
    let erased = Region::new_erased(interner);
    let mut given = items.iter();
    let (mut types, mut consts) = (0, 0);
    let args = GenericArgs::for_item(interner, func.into(), |_, id, _, _| match id {
        GenericParamId::LifetimeParamId(_) => erased.into(),
        GenericParamId::TypeParamId(_) => {
            types += 1;
            match given.next() {
                Some(&item) => {
                    let item = CallableDefId::FunctionId(item).into();
                    Ty::new_fn_def(interner, item, GenericArgs::default()).into()
                }
                None => erased.into(),
            }
        }
        GenericParamId::ConstParamId(_) => {
            consts += 1;
            erased.into()
        }
    });
    if types != items.len() || consts > 0 {
        let name = FunctionSignature::of(db, func).name.as_str();
        // alloc: the error's message
        return Err(format!(
            "`{name}` has {types} type and {consts} const parameters; it is rooted with {} \
             function items for its type parameters and nothing else",
            items.len()
        ));
    }
    Ok(rooted(db, homes, func, args))
}

/// The root of `func` instantiated with `args`, in the empty environment of its crate.
fn rooted<'db>(db: &'db Db, homes: &mut Homes, func: FunctionId, args: GenericArgs<'db>) -> HirInstance {
    let krate = func.krate(db);
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    let instance = Instance { def: InstanceDef::Fn(func), args: args.store(), checked: false };
    let env = homes.home(db, &instance, &env);
    HirInstance(Kind::Code(instance, env))
}

/// The code hashes of the instances whose bodies the IR of `instance` builds in, at any depth,
/// sorted, each once: its [`blaze_codegen::CompiledFunction::inlined`]. Nothing is allocated
/// for one that builds nothing in, as nearly every one. `inlined` is whether its references
/// were walked for a unit that MIR-inlines ([`HirFrontend::inlines`]), and so the callees'
/// are: what was inlined into a callee was inlined with it.
pub(crate) fn inlined<'a>(
    db: &'a Db,
    hashes: &HashSource,
    (instance, inlined): (&'a HirInstance, bool),
) -> Result<Vec<CodeHash>, String> {
    let mut found = Vec::new(); // alloc: the list returned, empty for nearly every instance
    // alloc: inline, eight on the stack
    let mut queue: SmallVec<[&HirInstance; 8]> = smallvec![instance];
    let mut seen = HashSet::default();
    while let Some(at) = queue.pop() {
        let built_in = refs::built_in(db, at, inlined)?;
        if built_in.is_empty() {
            continue;
        }
        let refs = refs::walked(db, at, inlined)?;
        for &place in built_in {
            let Some(FrontRef::Function(callee)) = refs.get(place as usize) else {
                // alloc: the error's message
                return Err(format!("{at:?} builds in place {place}, which is no function"));
            };
            if seen.insert(callee) {
                found.push(code_of(db, hashes, &callee.0)?);
                queue.push(callee);
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// The body of a closure or coroutine, by its interned id.
fn closure_loc<'db>(db: &'db Db, def: InstanceDef) -> Option<InternedClosure<'db>> {
    match def {
        InstanceDef::Closure(id) => Some(InternedClosureId::from_id(id).loc(db)),
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            Some(InternedCoroutineId::from_id(id).loc(db))
        }
        _ => None,
    }
}

fn owner_item(db: &Db, owner: InferBodyId<'_>) -> Result<ItemId, String> {
    match owner {
        InferBodyId::DefWithBodyId(owner) => Ok(match owner {
            DefWithBodyId::FunctionId(it) => ItemId::Function(it),
            DefWithBodyId::StaticId(it) => ItemId::Static(it),
            DefWithBodyId::ConstId(it) => ItemId::Const(it),
            DefWithBodyId::VariantId(it) => ItemId::Adt(AdtId::EnumId(it.lookup(db).parent)),
        }),
        InferBodyId::AnonConstId(_) => {
            // alloc: the error's message
            Err("a closure in an anonymous constant, which is no item".to_owned())
        }
    }
}

/// Which item an instance is of. A shim is of the trait method or lang item rustc names it
/// by: drop glue is `drop_in_place`, a vtable's `call_once` is `FnOnce::call_once`.
pub(super) fn item_of(db: &Db, kind: &Kind) -> Result<ItemId, String> {
    let (instance, env) = match kind {
        Kind::Code(instance, env) => (instance, env),
        Kind::Start(main) => return Ok(ItemId::Function(*main)),
        // alloc: the error's message
        Kind::Asm(_) => return Err("an `asm!` function, which is no item".to_owned()),
        Kind::Allocator(_) => {
            // alloc: the error's message
            return Err("a function of the allocator shim, which is no item".to_owned());
        }
    };
    let lang_items = || DbInterner::new_with(db, env.krate).lang_items();
    let lang = |item: Option<FunctionId>, name: &str| {
        // alloc: the error's message
        item.map(ItemId::Function).ok_or_else(|| format!("no `{name}` lang item"))
    };
    match instance.def {
        InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => Ok(ItemId::Function(func)),
        InstanceDef::Closure(_) | InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => {
            let loc = closure_loc(db, instance.def).expect("a closure or coroutine");
            owner_item(db, loc.owner)
        }
        InstanceDef::DropGlue => lang(lang_items().DropGlue, "drop_glue"),
        InstanceDef::CallOnceShim | InstanceDef::FnPointerShim => {
            lang(lang_items().FnOnce_call_once, "FnOnce::call_once")
        }
        InstanceDef::ReifyShim { .. } => {
            match instance.args.as_ref().type_at(0).kind() {
                TyKind::FnDef(def, _) => Ok(match def.0 {
                    CallableDefId::FunctionId(it) => ItemId::Function(it),
                    CallableDefId::StructId(it) => ItemId::Adt(AdtId::StructId(it)),
                    CallableDefId::EnumVariantId(it) => {
                        ItemId::Adt(AdtId::EnumId(it.lookup(db).parent))
                    }
                }),
                _ => lang(lang_items().FnOnce_call_once, "FnOnce::call_once"),
            }
        }
    }
}

const SHIM_DOMAIN: &str = "blaze v2 shim 1";
/// An `asm!` function is no item: it is named by its text.
const ASM_ITEM_DOMAIN: &str = "blaze v2 asm item 1";
const ASM_DOMAIN: &str = "blaze v2 asm 1";
const START_DOMAIN: &str = "blaze v2 start 1";
/// A cached function's code with the code of its body it writes ([`fncache::written_code`]).
const CACHED_DOMAIN: &str = "blaze v2 cached function 1";
const ARGS_DOMAIN: &str = "blaze v2 args 1";
/// A share key in place of the arguments: never equal to an [`ARGS_DOMAIN`] hash.
const SHARED_ARGS_DOMAIN: &str = "blaze v2 shared args 1";

/// Whether an instance with no share key for `why` is compiled for its arguments alone by
/// what it is: one of a cycle (the query engine's answer for all of it), a shim built without MIR, or a
/// call of an intrinsic whose code is the instance's. Every other reason is something the key
/// could not read (a layout, a constant, an intrinsic missing from the table), and an error.
fn per_instance(why: &Unshared) -> bool {
    match why {
        Unshared::Recursive | Unshared::Shim(_) | Unshared::PerInstance(_) => true,
        Unshared::UnknownIntrinsic(_)
        | Unshared::NoOptions
        | Unshared::NoMir(_)
        | Unshared::NoLayout(_)
        | Unshared::NoContent(_)
        | Unshared::ConstEval(_)
        | Unshared::Unhashable(_)
        | Unshared::Shape(_)
        | Unshared::RecursionLimit { .. } => false,
    }
}

/// What an instance runs, as the share key names it.
fn share_def(def: InstanceDef) -> ShareDef {
    match def {
        InstanceDef::Fn(func) => ShareDef::Fn(func),
        InstanceDef::Closure(id) => ShareDef::Closure(id),
        InstanceDef::Coroutine(id) => ShareDef::Coroutine(id),
        InstanceDef::CoroutineByMove(id) => ShareDef::CoroutineByMove(id),
        InstanceDef::DropGlue => ShareDef::DropGlue,
        InstanceDef::CallOnceShim => ShareDef::CallOnceShim,
        InstanceDef::VtableShim(func) => ShareDef::VtableShim(func),
        InstanceDef::FnPointerShim => ShareDef::FnPointerShim,
        InstanceDef::ReifyShim { receiver } => ShareDef::ReifyShim { receiver },
    }
}

/// What an instance's code is. A shim's is what kind of shim it is, with the code of the
/// function whose signature it adapts: what else it reads is its type argument's.
fn code_of(db: &Db, hashes: &HashSource, kind: &Kind) -> Result<CodeHash, String> {
    let (instance, env) = match kind {
        Kind::Code(instance, env) => (instance, env),
        Kind::Asm(text) => return Ok(CodeHash(ContentId::of(ASM_DOMAIN, text.as_bytes()))),
        Kind::Allocator(forward) => return Ok(super::allocator::code(forward)),
        Kind::Start(main) => {
            let mut hasher = Hasher::new(START_DOMAIN);
            hasher.id(&hashes.code(db, Body::Item((*main).into()))?.0);
            return Ok(CodeHash(hasher.finish()));
        }
    };
    let shim = |tag: u8, adapted: Option<FunctionId>| -> Result<CodeHash, String> {
        let mut hasher = Hasher::new(SHIM_DOMAIN);
        hasher.update(&[tag]);
        if let Some(func) = adapted {
            hasher.id(&hashes.code(db, Body::Item(func.into()))?.0);
        }
        Ok(CodeHash(hasher.finish()))
    };
    match instance.def {
        InstanceDef::Fn(func) => {
            let code = hashes.code(db, Body::Item(func.into()))?;
            let written = hir_ty::attach_db(db, || fncache::written_code(db, func, instance, env));
            let Some(written) = written else {
                return Ok(code);
            };
            let mut hasher = Hasher::new(CACHED_DOMAIN);
            hasher.id(&code.0);
            match written {
                Ok(hash) => hasher.update(&[0]).update(&hash.to_le_bytes()),
                Err(why) => hasher.update(&[1]).update(why.as_bytes()),
            };
            Ok(CodeHash(hasher.finish()))
        }
        InstanceDef::Closure(id) => hashes.code(db, Body::Closure(id)),
        InstanceDef::Coroutine(id) => hashes.code(db, Body::Coroutine(id)),
        InstanceDef::CoroutineByMove(id) => hashes.code(db, Body::CoroutineByMove(id)),
        InstanceDef::DropGlue => shim(0, None),
        InstanceDef::CallOnceShim => shim(1, None),
        InstanceDef::VtableShim(func) => shim(2, Some(func)),
        InstanceDef::FnPointerShim => shim(3, None),
        InstanceDef::ReifyShim { receiver } => {
            let adapted = match instance.args.as_ref().type_at(0).kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    CallableDefId::FunctionId(func) => Some(func),
                    CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => None,
                },
                _ => None,
            };
            shim(if receiver { 5 } else { 4 }, adapted)
        }
    }
}

/// The content ID of an instance's generic arguments, regions left out.
fn args_of(db: &Db, hashes: &HashSource, kind: &Kind) -> Result<ContentId, String> {
    let Kind::Code(instance, _) = kind else { return Ok(ContentId::default()) };
    let mut hasher = Hasher::new(ARGS_DOMAIN);
    for arg in instance.args.as_ref().iter() {
        if let Some(ty) = arg.ty() {
            hasher.update(&[0]).id(&hashes.ty(db, ty)?);
        } else if let Some(konst) = arg.konst() {
            hasher.update(&[1]).id(&hashes.konst(db, konst)?);
        }
    }
    Ok(hasher.finish())
}

/// Whether the instance is compiled as the crate asking for it says: it has generic
/// arguments other than regions, as every shim does.
fn generic(kind: &Kind) -> bool {
    let Kind::Code(instance, _) = kind else { return false };
    match instance.def {
        InstanceDef::Fn(_)
        | InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_) => {
            instance.args.as_ref().iter().any(|arg| arg.region().is_none())
        }
        InstanceDef::DropGlue
        | InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => true,
    }
}

/// The crate an instance's code is written in, for one that is not generic.
pub(super) fn crate_of(db: &Db, kind: &Kind) -> Option<Crate> {
    match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) => Some(func.krate(db)),
            def => closure_loc(db, def).map(|loc| loc.owner.module(db).krate(db)),
        },
        Kind::Start(main) => Some(main.krate(db)),
        Kind::Asm(_) | Kind::Allocator(_) => None,
    }
}

/// Whether a type, not looking into its arguments (the walk does), is a workspace crate's.
/// An alias is taken as one: a monomorphic instance's arguments have none left.
fn names_workspace(db: &Db, ty: Ty<'_>, ours: &impl Fn(Crate) -> bool) -> bool {
    let closure = |owner: InferBodyId<'_>| ours(owner.module(db).krate(db));
    match ty.kind() {
        TyKind::Adt(def, _) => ours(def.def_id().krate(db)),
        TyKind::Foreign(alias) => ours(alias.0.krate(db)),
        TyKind::FnDef(def, _) => ours(def.0.krate(db)),
        TyKind::Closure(id, _) => closure(id.0.loc(db).owner),
        TyKind::CoroutineClosure(id, _) => closure(id.0.loc(db).owner),
        TyKind::Coroutine(id, _) | TyKind::CoroutineWitness(id, _) => {
            closure(id.0.loc(db).owner)
        }
        TyKind::Dynamic(predicates, _) => (predicates.principal_def_id().into_iter())
            .chain(predicates.auto_traits())
            .any(|it| ours(GenericDefId::TraitId(it.0).krate(db))),
        TyKind::Alias(..) => true,
        _ => false,
    }
}

const IDENTITY_DOMAIN: &str = "blaze v2 instance identity 1";
const INSTANCE_BOUND_DOMAIN: &str = "blaze v2 instance bound 1";

/// What an instance is across versions ([`Described::identity`]): a closure or coroutine by
/// its own key (its place among its store's closures, which an edit before it keeps), a
/// shim by its kind, with its item and plain arguments.
fn identity(db: &Db, kind: &Kind, item: ItemKey, args: ContentId) -> ContentId {
    let mut hasher = Hasher::new(IDENTITY_DOMAIN);
    match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(_) => hasher.update(&[0]).id(&item.0),
            InstanceDef::Closure(id) => hasher
                .update(&[1])
                .id(&blaze_items::key::closure_key(db, InternedClosureId::from_id(id)).0),
            InstanceDef::Coroutine(id) => hasher
                .update(&[2])
                .id(&blaze_items::key::coroutine_key(db, InternedCoroutineId::from_id(id)).0),
            InstanceDef::CoroutineByMove(id) => hasher
                .update(&[3])
                .id(&blaze_items::key::coroutine_key(db, InternedCoroutineId::from_id(id)).0),
            InstanceDef::DropGlue => hasher.update(&[4]).id(&item.0),
            InstanceDef::CallOnceShim => hasher.update(&[5]).id(&item.0),
            InstanceDef::VtableShim(_) => hasher.update(&[6]).id(&item.0),
            InstanceDef::FnPointerShim => hasher.update(&[7]).id(&item.0),
            InstanceDef::ReifyShim { receiver } => {
                hasher.update(&[8, u8::from(receiver)]).id(&item.0)
            }
        },
        Kind::Start(_) => hasher.update(&[10]).id(&item.0),
        Kind::Asm(_) => hasher.update(&[11]).id(&item.0),
        Kind::Allocator(forward) => hasher.update(&[12]).part(forward.symbol.as_bytes()),
    };
    hasher.id(&args);
    hasher.finish()
}

/// An instance's code as it runs, before what its references bind to (codegen adds those,
/// `blaze_codegen::exec::bound`): its code hash, its plain arguments and its item's
/// `blaze_items::bound_hash`, `Err` naming why it could not be hashed. `asm!` and the
/// allocator shim are no item: their code is all there is.
fn bound(db: &Db, kind: &Kind, code: CodeHash, args: ContentId) -> Result<ContentId, String> {
    let mut hasher = Hasher::new(INSTANCE_BOUND_DOMAIN);
    hasher.id(&code.0).id(&args);
    if !matches!(kind, Kind::Asm(_) | Kind::Allocator(_)) {
        let item = item_of(db, kind)?;
        match blaze_items::bound_hash(db, item) {
            Ok(Some(it)) => hasher.update(&[1]).id(&it),
            // A shim's `FnOnce::call_once`: no body, so nothing it binds but its arguments.
            Ok(None) => hasher.update(&[0]),
            Err(why) => return Err(why.describe(db)),
        };
    }
    Ok(hasher.finish())
}

/// The overflow checks the MIR of a generic says, as rustc builds a crate's MIR with its own:
/// none in the standard library's, and the instantiating crate's where the body inherits them.
fn written_checks(db: &Db, kind: &Kind) -> Option<bool> {
    let Kind::Code(instance, _) = kind else { return None };
    let (krate, inherits) = match instance.def {
        InstanceDef::Fn(func) => (
            func.krate(db),
            AttrFlags::query(db, func.into()).contains(AttrFlags::RUSTC_INHERIT_OVERFLOW_CHECKS),
        ),
        def => {
            let loc = closure_loc(db, def)?;
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            (loc.owner.module(db).krate(db), store.inherits_overflow_checks(loc.expr))
        }
    };
    if inherits {
        return None;
    }
    Some(crate_checks_overflow(db, krate).unwrap_or(false))
}

/// The symbol outside code names an instance by: `#[no_mangle]`, `#[export_name]`, the
/// standard library's functions linked by name, the C `main`.
pub(super) fn symbol_of(db: &Db, kind: &Kind) -> Option<String> {
    match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) if instance.args.as_ref().iter().all(|it| it.region().is_some()) => {
                exported_symbol(db, func).or_else(|| linked_symbol(db, func))
            }
            _ => None,
        },
        // alloc: once per program, the C `main`; `Described::symbol` owns its name
        Kind::Start(_) => Some(crate::Target::current().entry().0.to_owned()),
        // alloc: one of the shim's few entries, once per program
        Kind::Allocator(forward) => Some(forward.symbol.to_owned()),
        Kind::Asm(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use hir_ty_share::Unshared;

    use super::per_instance;

    /// An instance is compiled for its arguments alone only where it is per instance by what
    /// it is; a key the walk could not make is an error, never a build per instance.
    #[test]
    fn only_what_is_per_instance_is_built_per_instance() {
        assert!(per_instance(&Unshared::Recursive));
        assert!(per_instance(&Unshared::Shim("drop glue")));
        assert!(per_instance(&Unshared::PerInstance("size_of".to_owned())));
        for refused in [
            Unshared::UnknownIntrinsic("new_intrinsic".to_owned()),
            Unshared::NoOptions,
            Unshared::NoLayout("T".to_owned()),
            Unshared::ConstEval("N".to_owned()),
            Unshared::Shape("a union"),
        ] {
            assert!(!per_instance(&refused), "{refused}");
        }
    }
}

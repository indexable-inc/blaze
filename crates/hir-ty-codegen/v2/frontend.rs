//! The front end of a v2 build over the database: what each instance and static is by
//! content, whose code it is, and what it refers to.

use std::sync::atomic::{AtomicBool, Ordering};

use base_db::{Crate, CrateOrigin, Db};
use blaze_codegen::{
    CodegenError, InstanceKey, Owner,
    backend::{
        Address, Counted, Described, DescribedStatic, FrontRef, Frontend, Instantiates, Limit,
    },
    plan::{Mode, Origin, Package, PlanError, Policy},
};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemId, ItemKey};
use blaze_query::db::FromId;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, GenericDefId,
    GenericParamId, HasModule, Lookup, signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::{InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{DbInterner, GenericArgs, ParamEnv, Region, RegionExt, Ty, TyKind},
};
use hir_ty_identity::{Compiled, OptionsSet, Shim, Subject, instance_identity};
use hir_ty_mir::mir::MirOptions;
use hir_ty_mir::mir::dispatch;
use hir_ty_share::Unshared;
use rustc_type_ir::inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind};
use smallvec::{SmallVec, smallvec};
use std::alloc::Global;

use super::{
    Body, BuildTime, HashSource, Hashes, HirInstance, HirStatic, Kind, build::Build, refs,
    undescribed,
};
use crate::{Instance, InstanceDef, exported_static, exported_symbol, fncache, linked_symbol};

/// The build's database, `Sync` and shared by reference by every task. Nothing here locks.
pub struct Databases<'db> {
    db: &'db Db,
}

impl<'db> Databases<'db> {
    pub fn new(db: &'db Db) -> Self {
        Databases { db }
    }

    /// Runs `work` with the database attached to its thread, which any formatted type or
    /// constant (an error's message, a panic's) needs, on whichever worker the task is.
    pub(crate) fn with<T>(&self, work: impl FnOnce(&Db) -> T) -> T {
        blaze_query::db::attach(self.db, || work(self.db))
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
    pub(crate) build_time: BuildTime,
    /// Whether the build links a dependency image (`premap`): a dev executable's, which takes
    /// edits on the daemon's code memory. Without one, no code is the image's: every crate but
    /// the standard library's is the workspace's ([`Self::workspace`]), so every body may take
    /// the program's world and none is split off. Set only by [`Self::with_image`], from the
    /// one place a build decides it (blaze-cargo's `links_image`); `false` until then.
    image: bool,
    /// How each origin's units build bodies: what a function's references are walked for and
    /// what a generic is shared under.
    units: Option<Units>,
    // dyn: `reached` once a level of the walk, `stopped` once a function compiled (2 ns
    // against its 50 to 200 µs), never per instruction
    checks: Option<&'db dyn BodyChecks>, // dyn: never per instruction, as above
    /// Set when the build's job is cancelled or gives way to another task.
    stop: Option<&'db AtomicBool>,
    /// What the walk of a final binary found of its trait objects ([`super::devirt`]).
    pub(crate) devirt: super::devirt::State,
}

/// One value for each origin of the build's [`Policy`], told apart as the plan tells them.
struct ByOrigin<T> {
    /// A member's, a dependency's, build-time code's, of a package the profile names no
    /// override for.
    member: T,
    dependency: T,
    build_time: T,
    /// Of a package the profile names (`[profile.*.package.<name>]`), the same three.
    packages: HashMap<Package, [T; 3], Global>,
    std: T,
}

impl<T> ByOrigin<T> {
    /// Made once a build.
    fn of(policy: &Policy, mut each: impl FnMut(&Origin) -> T) -> ByOrigin<T> {
        let mut kinds = |name: Package| {
            [
                each(&Origin::Member(name)),
                each(&Origin::Dependency(name)),
                each(&Origin::BuildTime(name)),
            ]
        };
        // No package is named the empty name: these are of one the profile has nothing for.
        let [member, dependency, build_time] = kinds(Package::new(""));
        let packages: HashMap<_, _, Global> = (policy.profile.packages.keys())
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
    /// Some origin's units inline: what an instance every origin may ask for and compile (a
    /// generic, an `#[inline]` function, a closure: [`user_compiled`]) is walked with.
    any: bool,
    /// Whether some unit of the origin tells a debugger of its named locals
    /// ([`DebugInfo::tells_locals`](blaze_codegen::settings::DebugInfo::tells_locals), at the
    /// first spec or the one a build tiers it up to):
    /// a generic it asks for is then keyed by its arguments as well
    /// ([`HirFrontend::generic_args`]).
    tells: ByOrigin<Result<bool, PlanError>>,
    /// Some origin's units tell a debugger of their named locals: what an instance every
    /// origin may compile ([`user_compiled`]) is keyed as.
    tells_any: bool,
    /// Whether some unit of the origin puts names or places of its own in its compiled code
    /// beside its positions ([`names_of`]): its template is then kept under its code, never its
    /// template code ([`Hashes::template_code`]).
    names: ByOrigin<Result<bool, PlanError>>,
    /// Some origin's units do, as [`Self::any`] for inlining.
    names_any: bool,
    /// Every options a unit of the origin may build a body with ([`Build::options`]), by
    /// whether the body is of the library's crates: what a generic it asks for has its compile
    /// identity under ([`hir_ty_identity::instance_identity`]).
    options: ByOrigin<Result<Compiled, String>>,
}

impl Units {
    fn of(db: &Db, policy: &Policy) -> Units {
        let builds = ByOrigin::of(policy, |origin| builds_of(policy, origin));
        let inlines = builds.map(|it| match it {
            Ok(builds) => Ok(builds.iter().any(|it| it.inlines)),
            Err(e) => Err(e.clone()), // clone: once a build, an error
        });
        let any = inlines.values().any(|it| it == &Ok(true));
        let tells = ByOrigin::of(policy, |origin| tells_of(policy, origin));
        let tells_any = tells.values().any(|it| it == &Ok(true));
        let names = ByOrigin::of(policy, |origin| names_of(policy, origin));
        let names_any = names.values().any(|it| it == &Ok(true));
        let crates = base_db::all_crates(db);
        let mut by_build: HashMap<Build, Asked, Global> = HashMap::new_in(Global);
        let options = builds.map(|it| -> Result<Compiled, String> {
            let builds = it.as_ref().map_err(ToString::to_string)?;
            let mut asked = Asked::default();
            for &build in builds {
                let of = by_build.entry(build).or_insert_with(|| asked_of(db, &crates, build));
                asked.add(of);
            }
            Ok(Compiled { lang: options_set(&asked.lang)?, other: options_set(&asked.other)? })
        });
        Units { inlines, any, tells, tells_any, names, names_any, options }
    }
}

/// Whether a unit of `origin` tells a debugger of its named locals, at its first spec or the
/// spec a live build tiers it up to: what `hir-ty-codegen`'s `built_function` builds
/// `Function::variables` for.
fn tells_of(policy: &Policy, origin: &Origin) -> Result<bool, PlanError> {
    let choice = policy.choose(origin, None)?;
    let told = |spec: &blaze_codegen::settings::Spec| spec.debuginfo.tells_locals();
    Ok(told(&choice.first) || choice.then.as_ref().is_some_and(told))
}

/// Whether a unit of `origin` puts in its compiled code names or places a template keyed by
/// its template code ([`Hashes::template_code`]) would leave out, at its first spec or the spec
/// a live build tiers it up to: a PGO build's profile record names the function by its path
/// (`pgo::name`), and a report's rows (`Spec::report`) are of absolute positions in its side
/// table.
fn names_of(policy: &Policy, origin: &Origin) -> Result<bool, PlanError> {
    let choice = policy.choose(origin, None)?;
    // A sanitizing unit reads names too: SanitizerCoverage is off for a `__sanitizer_*`
    // symbol (`backend::unsanitized`), and LLVM's sanitizers write descriptors that name the
    // globals and frames they guard.
    let named = |spec: &blaze_codegen::settings::Spec| {
        spec.report
            || !matches!(spec.pgo, blaze_codegen::settings::Pgo::None)
            || !spec.sanitize.is_empty()
    };
    Ok(named(&choice.first) || choice.then.as_ref().is_some_and(named))
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

/// Every [`Build::options`] `build` gives a body of each crate of the graph, of an instance
/// compiled once or per instance. Where the instance is worked out (its home,
/// `hir_ty_mir::mir::home`) changes none of them.
fn asked_of(db: &Db, crates: &[Crate], build: Build) -> Asked {
    let mut asked = Asked::default();
    for &written in crates {
        let lang = written.data(db).origin.is_lang();
        for generic in [false, true] {
            asked.push(lang, build.options(db, written, generic));
        }
    }
    asked
}

/// `options` as compile identity takes them. The set must hold each whole: options it can't tell
/// apart would key two bodies as one.
fn options_set(options: &[MirOptions]) -> Result<OptionsSet, String> {
    let set = OptionsSet::of(options);
    match options.iter().find(|it| !set.iter().any(|kept| kept == **it)) {
        None => Ok(set),
        // alloc: once a build, an error
        Some(lost) => Err(format!(
            "`hir_ty_identity::OptionsSet` can't hold the MIR options {lost:?} a unit builds bodies \
             with, so no generic has a compile identity"
        )),
    }
}

const NO_POLICY: &str = "the front end was made with no policy (`HirFrontend::with_policy`), so \
                         it can't tell how the instance's units build its MIR";

impl<'db> HirFrontend<'db> {
    /// `build_time`: whether the build is of code that runs at build time ([`BuildTime`]).
    pub fn new(databases: Databases<'db>, hashes: HashSource, build_time: BuildTime) -> Self {
        HirFrontend {
            databases,
            hashes,
            build_time,
            image: false,
            units: None,
            checks: None,
            stop: None,
            devirt: Default::default(),
        }
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
    /// It leaves whether the build links a dependency image to [`Self::with_image`].
    pub fn with_policy(mut self, policy: &Policy) -> Self {
        self.units = Some(Units::of(self.databases.db, policy));
        self
    }

    /// The front end of a build that links a dependency image (`image`), whose code is split
    /// between the image and the workspace ([`Self::image`]): what blaze-cargo's `links_image`
    /// says of the unit, or a harness that asks what an edit of a crate reaches
    /// (`hir-firewall`), which is what the split follows. In any order with
    /// [`Self::with_policy`], which leaves it alone.
    pub fn with_image(mut self, image: bool) -> Self {
        self.image = image;
        self
    }

    /// Whether a unit of `instance` MIR-inlines bodies of crates other than the library's,
    /// whose code then calls what they call: one of its crate's origin when it is compiled
    /// there only, else ([`user_compiled`]: a generic, an `#[inline]` function, a closure) of
    /// any origin, as any may ask for it. What its references are walked
    /// for ([`refs::walk`]).
    pub(crate) fn inlines(&self, db: &Db, instance: &HirInstance) -> Result<bool, String> {
        self.inlines_as(db, instance, None)
    }

    /// [`Self::inlines`] and the program's world, as describing `instance` walked it
    /// ([`super::devirt::walking`]): what a unit builds its body with.
    pub(crate) fn walking(&self, db: &Db, instance: &HirInstance) -> Result<refs::Walking, String> {
        let inlined = self.inlines(db, instance)?;
        super::devirt::walking(db, self.devirt.world(), instance, inlined, || {
            self.owner(db, &instance.0)
        })
    }

    /// [`Self::inlines`] as describing `instance` for `requester` decides it: of the origin
    /// [`Self::compiled_for`] picks.
    pub(crate) fn inlines_for(
        &self,
        db: &Db,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Result<bool, String> {
        let origin = self.compiled_for(db, instance, requester);
        self.inlines_as(db, instance, Some(&origin))
    }

    /// The origin `instance` is compiled for when `requester` asks for it: the requester's
    /// where rustc compiles it into each crate that uses it ([`user_compiled`]), else its own
    /// crate's, where rustc links every user to one copy. It follows that rule alone, never
    /// the instance's overflow checks, so turning them on or off moves no code between origins.
    /// Its callees are asked for by this origin (the driver's walk), so a body that takes its
    /// caller's overflow checks (core's `#[inline] #[rustc_inherit_overflow_checks]` operator
    /// impls) reached through other bodies their user compiles (`impl Add<&u8> for u8`,
    /// `impl Sum<&u8> for u8`) takes the user's.
    fn compiled_for(&self, db: &Db, instance: &HirInstance, requester: &Origin) -> Origin {
        match crate_of(db, &instance.0) {
            Some(krate) if !user_compiled(db, &instance.0) => self.origin(db, krate),
            _ => *requester,
        }
    }

    /// [`Self::inlines`], with the origin of the instance's crate when the caller has it.
    fn inlines_as(
        &self,
        db: &Db,
        instance: &HirInstance,
        origin: Option<&Origin>,
    ) -> Result<bool, String> {
        let Some(units) = &self.units else { return Err(NO_POLICY.to_owned()) }; // alloc: an error
        // One answer for every requester of an instance its users compile, as describing it for
        // one and preparing it (`walking`, which names no requester) must list one walk.
        let of_origin = match crate_of(db, &instance.0) {
            Some(krate) if !user_compiled(db, &instance.0) => match origin {
                Some(origin) => units.inlines.get(origin),
                None => units.inlines.get(&self.origin(db, krate)),
            },
            _ => return Ok(units.any),
        };
        of_origin.as_ref().copied().map_err(|it| it.to_string()) // alloc: the error's message
    }

    /// What `of` says of the units of `origin` (by origin, and whether any origin's does) for
    /// `instance`: as [`Self::inlines_as`], one answer for every requester of an instance its
    /// users compile.
    fn units_as(
        &self,
        db: &Db,
        instance: &HirInstance,
        origin: &Origin,
        of: fn(&Units) -> (&ByOrigin<Result<bool, PlanError>>, bool),
    ) -> Result<bool, String> {
        let Some(units) = &self.units else { return Err(NO_POLICY.to_owned()) }; // alloc: an error
        let (by_origin, any) = of(units);
        match crate_of(db, &instance.0) {
            Some(_) if !user_compiled(db, &instance.0) => {
                by_origin.get(origin).as_ref().copied().map_err(|it| it.to_string()) // alloc: the error's message
            }
            _ => Ok(any),
        }
    }

    /// A function that is not generic, as a root of the build: `main`, a test, an exported
    /// function.
    pub fn root(&self, func: FunctionId) -> HirInstance {
        self.databases.with(|db| root_in(db, func))
    }

    /// A root of `func` whose type parameters are, in order, the function items `items` (each
    /// with no generic arguments of its own), every region erased: a proc macro crate's
    /// `Client::expand1::<F>` or `expand2::<F>` for its `#[proc_macro*]` function `F`. An error
    /// names `func` when it has a const parameter or a type parameter count other than
    /// `items.len()`.
    pub fn root_with_fn_items(
        &self,
        func: FunctionId,
        items: &[FunctionId],
    ) -> Result<HirInstance, String> {
        self.databases.with(|db| root_with_fn_items_in(db, func, items))
    }

    /// The C `main` of a program whose `fn main` is `main`.
    pub fn start(&self, main: FunctionId) -> HirInstance {
        HirInstance(Kind::Start(main))
    }

    /// The allocator shim of the program of `krate`, with the `#[global_allocator]`'s
    /// functions it calls: roots of a program with a `main`, and of a `cdylib`.
    pub fn allocator_shim(&self, krate: Crate) -> Result<Vec<HirInstance>, CodegenError> {
        self.with(|db| {
            super::allocator::shim(db, krate, |func| self.root(func))
                // alloc: the error's message
                .map_err(|what| {
                    undescribed(db, &krate, None, format!("its allocator shim: {what}"))
                })
        })
    }

    pub(crate) fn with<T>(&self, work: impl FnOnce(&Db) -> T) -> T {
        self.databases.with(work)
    }

    fn described(
        &self,
        db: &Db,
        instance: &HirInstance,
        requester: &Origin,
    ) -> Result<Described<HirInstance, HirStatic>, CodegenError> {
        let hashes = &self.hashes;
        let item = match &instance.0 {
            Kind::Asm(text, _) => Ok(ItemKey(ContentId::of(ASM_ITEM_DOMAIN, text.as_bytes()))),
            Kind::Allocator(forward) => Ok(super::allocator::item(forward)),
            kind => item_of(db, kind).and_then(|item| hashes.item(db, item)),
        }
        .map_err(|what| undescribed(db, &super::Named(db, instance), None, what))?;
        // Each failure past here is the front end's of this instance: the error carries its item.
        let fail = |what: String| super::failed_in_front(db, None, Some(item), instance, what);
        let generic = generic(&instance.0);
        let origin = self.compiled_for(db, instance, requester);
        let inlined = self.inlines_as(db, instance, Some(&origin)).map_err(fail)?;
        let owner = self.owner(db, &instance.0);
        let walking = super::devirt::walking(db, self.devirt.world(), instance, inlined, || owner)
            .map_err(fail)?;
        let built_in_code = built_in_code(db, hashes, &instance.0).map_err(fail)?;
        // A body built against the program's dyn facts is keyed by those it read (rule 21). A
        // copy of it built into a caller is built against none: `built_in_code`.
        let read = refs::read(db, instance, walking).map_err(fail)?;
        let told =
            self.units_as(db, instance, &origin, |it| (&it.tells, it.tells_any)).map_err(fail)?;
        // What its compiled function reads of names beside its code (the files its `file!()`
        // names, its told locals' types) is in every key of it (rule 21), its link code's as
        // its template code's: read once, folded into both ([`with_names`]).
        let body = kind_body(&instance.0);
        let names = body.map(|body| hashes.names_read(db, body, told)).transpose().map_err(fail)?;
        let code = with_names(super::devirt::code(built_in_code, read), names.map(|it| it.0));
        // Its template is kept under its template code, which holds no name and no place, when
        // all its positions are its own tables' and its code is its body's alone: none inlined
        // from another body (`inlined`, a unit's inliner; `lang`, the library's bodies, which
        // `Build::options` always inlines as shipped), no report rows or PGO record naming it
        // (`Units::names`), and a body of its own (no shim's declaration, no `main`'s start).
        // The template then holds its positions by table and the instance fills its own
        // anchors (docs/v2/plan/design/name-free-identity.md).
        let lang = crate_of(db, &instance.0).is_some_and(|it| it.data(db).origin.is_lang());
        let named =
            self.units_as(db, instance, &origin, |it| (&it.names, it.names_any)).map_err(fail)?;
        let held = match (inlined || lang || named, names) {
            (false, Some((names, true))) => {
                template_code_of(db, hashes, &instance.0, names).map_err(fail)?
            }
            // A told local's type holding an alias, or no body of its own: kept under its code.
            _ => None,
        };
        let (template_code, anchors) = match held {
            Some((own, owner)) => {
                (super::devirt::code(own, read), Some(hashes.anchors(db, owner).map_err(fail)?))
            }
            None => (code, None),
        };
        let plain = args_of(db, hashes, &instance.0).map_err(fail)?;
        let args = match generic {
            true => {
                let placement = refs::placement(db, instance, walking).map_err(fail)?;
                self.generic_args(db, &instance.0, &origin, plain, placement).map_err(fail)?
            }
            false => plain,
        };
        let identity = identity(db, &instance.0, item, plain);
        let bound = bound(db, &instance.0, code, plain);
        let cold = cold_of(db, &instance.0);
        // Always the origin's profile, which keys it (rule 21). Pinning the setting of one
        // whose MIR keeps no check would let requesters that differ only in overflow checks
        // share its key while its callees that take their caller's setting differ between them
        // (`<u8 as Add>::add` under `impl Add<&u8> for u8`); for one compiled in its own crate
        // only, the origin's profile is already that crate's setting.
        let overflow_checks = None;
        let symbol = symbol_of(db, &instance.0).map_err(fail)?;
        let mut refs = refs::walk(db, instance, walking).map_err(fail)?;
        asked_by(owner, &mut refs);
        // alloc: `Described::built_in` owns its list, which the memo's is borrowed from
        let built_in = refs::built_in(db, instance, walking).map_err(fail)?.to_vec();
        // Written in a workspace crate: the complement of what a caller may build in
        // (`always_inlinable_callee`), so nothing patchable is ever built in.
        let editable =
            crate_of(db, &instance.0).is_some_and(|krate| krate.data(db).origin.is_local());
        Ok(Described {
            item,
            code,
            template_code,
            anchors,
            built_in_code,
            args,
            origin,
            editable,
            overflow_checks,
            address: if symbol.is_some() { Address::Significant } else { Address::Free },
            symbol,
            name: super::linked::debug_name(db, &instance.0),
            refs,
            built_in,
            owner,
            identity,
            bound,
            cold,
        })
    }

    /// A generic's arguments in its key, compiled as `origin` asks: what its code reads of them
    /// ([`Self::shared_args`]) with `placement`, which place of its references each request of
    /// its code is given ([`refs::placement`]), and, where a unit of `origin` tells a debugger
    /// of its named locals ([`Units::tells`]), `plain`, its arguments' content ID
    /// ([`args_of`]). A told local's type is in the IR's shape and the side table
    /// (`side::Variable::ty`), and it is the instance's own (`&UnsafeCell<usize>` or
    /// `&UnsafeCell<Align8<*mut ()>>` for one body of one compile identity), so two instances
    /// sharing one key would give the store two shapes under it, or one instance the other's
    /// debug info (rule 21). Without told locals the instances stay one.
    ///
    /// A compile identity names each callee by its own identity, never by which instance it
    /// is, so callees of equal identities that are one instance in one instance of the generic
    /// and two in another (`size_of::<S>` and `size_of::<D>` in `needs_realloc::<S, D>` for
    /// `S == D` and for two types of one layout) leave it equal, while the reference list,
    /// which holds each instance once, and so the code, which names a reference by its place,
    /// differ: `placement` is that one difference (rule 21).
    fn generic_args(
        &self,
        db: &Db,
        kind: &Kind,
        origin: &Origin,
        plain: ContentId,
        placement: ContentId,
    ) -> Result<ContentId, String> {
        let shared = self.shared_args(db, kind, origin)?;
        // Already the instance's own arguments ([`args_of`]), which its walk, and so its
        // placement, is made from: mixing either in would only change the key, not what it
        // covers.
        if shared == plain {
            return Ok(shared);
        }
        let shared = Hasher::new(PLACED_ARGS_DOMAIN).id(&shared).id(&placement).finish();
        match self.tells(origin)? {
            true => Ok(Hasher::new(TOLD_ARGS_DOMAIN).id(&shared).id(&plain).finish()),
            false => Ok(shared),
        }
    }

    /// Whether some unit of `origin` tells a debugger of its named locals ([`Units::tells`]).
    fn tells(&self, origin: &Origin) -> Result<bool, String> {
        let Some(units) = &self.units else { return Err(NO_POLICY.to_owned()) }; // alloc: an error
        units.tells.get(origin).as_ref().copied().map_err(|it| it.to_string()) // alloc: the error's message
    }

    /// A generic's arguments as its code reads them, compiled as `origin` asks: its compile
    /// identity under every options a unit of `origin` builds a body with, or, for an instance that is
    /// per instance by what it is ([`per_instance`]), its arguments' content ID
    /// ([`args_of`]). Both give exact code. A key that can't be made for any other reason is
    /// an error: the instance is never compiled per instance in its place. A generic callee is
    /// of its caller's origin, so its key here is the one its caller's key holds.
    pub(super) fn shared_args(
        &self,
        db: &Db,
        kind: &Kind,
        origin: &Origin,
    ) -> Result<ContentId, String> {
        let Kind::Code(instance, env) = kind else { return args_of(db, &self.hashes, kind) };
        let compiled = self.body_options(origin)?;
        // clone: the query's key, a reference count
        let of = hir_ty_identity::Instance {
            subject: subject_of(instance.def),
            args: instance.args.clone(),
        };
        match instance_identity(db, &of, env, compiled) {
            Ok(identity) => Ok(Hasher::new(SHARED_ARGS_DOMAIN).id(&identity.0).finish()),
            Err(why) if per_instance(why) => args_of(db, &self.hashes, kind),
            // alloc: the error's message
            Err(why) => Err(blaze_query::db::attach(db, || {
                format!("no compile identity for {instance:?}: {why}")
            })),
        }
    }

    /// Every options a unit of `origin` builds a body with: what a generic it asks for has its
    /// compile identity under.
    pub(super) fn body_options(&self, origin: &Origin) -> Result<Compiled, String> {
        match &self.units {
            Some(units) => units.options.get(origin).clone(), // clone: a copy, or an error
            None => Err(NO_POLICY.to_owned()),                // alloc: an error
        }
    }

    /// Whose code a crate's is, for its settings: build-time code's in a build-time build
    /// ([`BuildTime`]), never the standard library.
    pub(crate) fn origin(&self, db: &Db, krate: Crate) -> Origin {
        let data = krate.data(db);
        let name = match &data.origin {
            CrateOrigin::Lang(_) => return Origin::Std,
            CrateOrigin::Local { name: Some(name), .. } | CrateOrigin::Library { name, .. } => {
                Package::new(name.as_str())
            }
            CrateOrigin::Local { name: None, .. } | CrateOrigin::Rustc { .. } => {
                Package::new(&krate.named(db).to_string())
            }
        };
        if self.build_time.0 {
            return Origin::BuildTime(name);
        }
        match data.origin {
            CrateOrigin::Local { .. } => Origin::Member(name),
            _ => Origin::Dependency(name),
        }
    }

    /// Whether a crate's code is the workspace's for a dependency image, never the standard
    /// library's: in a build that links no image ([`Self::image`]) or a build-time build
    /// ([`BuildTime`]) every other crate's, as no image holds it; in a live program's, a crate
    /// an edit reaches ([`reaches_local`]). The one rule of ownership: every function's and
    /// static's owner ([`Self::owner`], [`Self::static_owner`]) is made from it.
    fn workspace(&self, db: &Db, krate: Crate) -> bool {
        match krate.data(db).origin {
            CrateOrigin::Lang(_) => false,
            _ if !self.image => true,
            _ if self.build_time.0 => true,
            _ => reaches_local(db, krate),
        }
    }

    /// Whose an instance is for a dependency image ([`Owner`]): the workspace's when its item
    /// is or any of its type arguments names a workspace crate's type, closure or item
    /// ([`Self::workspace`]). A shim's item is the standard library's (`drop_in_place`,
    /// `FnOnce::call_once`), so its type arguments decide. An `asm!` function is its asker's:
    /// the walk names it with `false` and the front end sets `true` on the references of code
    /// the image holds ([`asked_by`]), so the flag is its asker's owner. The allocator shim
    /// is the program's. No requester changes it.
    pub(crate) fn owner(&self, db: &Db, kind: &Kind) -> Owner {
        let ours = |krate: Crate| self.workspace(db, krate);
        let instance = match kind {
            Kind::Code(instance, _) => instance,
            Kind::Start(main) if ours(main.krate(db)) => return Owner::Workspace,
            Kind::Start(_) => return Owner::Dependency,
            Kind::Asm(_, true) => return Owner::Dependency,
            Kind::Asm(_, false) => return Owner::Workspace,
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

    /// Whose a static is for a dependency image: its crate's, by [`Self::workspace`], as a
    /// function of that crate is.
    fn static_owner(&self, db: &Db, value: &HirStatic) -> Owner {
        if self.workspace(db, value.0.krate(db)) { Owner::Workspace } else { Owner::Dependency }
    }

    fn described_static(
        &self,
        db: &Db,
        value: &HirStatic,
    ) -> Result<DescribedStatic<HirInstance, HirStatic>, CodegenError> {
        let hashes = &self.hashes;
        let item = hashes
            .item(db, ItemId::Static(value.0))
            .map_err(|what| undescribed(db, &super::StaticNamed(db, value.0), None, what))?;
        let mut held = super::statics::value(db, hashes, value.0)
            .map_err(|what| undescribed(db, &super::StaticNamed(db, value.0), Some(item), what))?;
        let owner = self.static_owner(db, value);
        asked_by(owner, &mut held.refs);
        Ok(DescribedStatic {
            item,
            instance: ContentId::default(),
            value: held.value,
            mutable: held.mutable,
            thread_local: held.thread_local,
            section: held.section,
            origin: self.origin(db, value.0.krate(db)),
            owner,
            // alloc: only an exported static, whose `DescribedStatic::symbol` owns its name
            symbol: exported_static(db, value.0).map(|it| it.as_str().to_owned()),
            // alloc: per static described, its path, which errors name it by
            name: crate::fncache::static_name(db, value.0),
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
        self.with(|db| self.described(db, instance, requester))
    }

    fn describe_static(
        &self,
        value: &HirStatic,
    ) -> Result<DescribedStatic<HirInstance, HirStatic>, CodegenError> {
        self.with(|db| self.described_static(db, value))
    }

    fn reached(&self, functions: &[(HirInstance, Origin)], statics: &[HirStatic]) {
        self.devirt.reached(functions, statics);
        let Some(checks) = self.checks else { return };
        let bodies: Vec<DefWithBodyId> = self.with(|db| {
            (functions.iter().filter_map(|(instance, _)| body_of(db, instance)))
                .chain(statics.iter().map(|it| DefWithBodyId::StaticId(it.0)))
                // alloc: once a level of the walk, sized by it
                .collect()
        });
        checks.reached(&bodies);
    }

    fn stopped(&self) -> bool {
        self.stop.is_some_and(|it| it.load(Ordering::Relaxed))
            || self.checks.is_some_and(|it| it.stopped())
    }

    fn counted(&self, functions: &[(HirInstance, Origin)]) -> Vec<Counted> {
        // alloc: once a level of the walk, sized by it
        self.with(|db| functions.iter().map(|(instance, _)| counted(db, &instance.0)).collect())
    }

    fn spell(&self, instance: &HirInstance) -> String {
        self.with(|db| spelled(db, &instance.0))
    }

    fn past_recursion_limit(
        &self,
        instance: &HirInstance,
        depth: u32,
        limit: u32,
        head: &HirInstance,
        limit_of: &HirInstance,
    ) -> CodegenError {
        self.with(|db| past_recursion_limit(db, instance, depth, limit, head, limit_of))
    }
    fn closed(&self) -> Result<bool, CodegenError> {
        self.devirt.closed(self)
    }
}

/// The domain of the body a shim is an instance of, for the walk's recursion limit.
const SHIM_BODY_DOMAIN: &str = "blaze v2 recursion shim body 2";

/// What the build's walk reads of `kind` for rustc's recursion limit ([`Counted`]).
fn counted(db: &Db, kind: &Kind) -> Counted {
    let limit = limit_crate(db, kind).map(|krate| Limit {
        krate: blaze_items::key::crate_key(db, krate),
        value: hir_def::nameres::crate_def_map(db, krate).recursion_limit(),
    });
    Counted { body: instantiates(db, kind), limit, heads_own_path: heads_own_path(db, kind) }
}

/// Whether `kind` is collected in its own crate's build only, as rustc's
/// `should_codegen_locally` is false for it in a crate that uses it: a function its users do
/// not compile ([`user_compiled`], the one rule of it). A shim, a closure and a coroutine are
/// built where they are used, as rustc builds them.
fn heads_own_path(db: &Db, kind: &Kind) -> bool {
    let Kind::Code(instance, _) = kind else { return false };
    matches!(instance.def, InstanceDef::Fn(_)) && !user_compiled(db, kind)
}

/// What `kind` is an instance of, as rustc's `check_recursion_limit` counts its `DefId`
/// (`InstanceKind::def_id`): a function, its vtable shim and its reify shim are the function's;
/// each closure and coroutine is its own; all drop glue is one (counted at a quarter); the
/// `FnOnce::call_once` of a closure, by value in a vtable (`CallOnceShim`) or behind a fn
/// pointer (`FnPointerShim`), is `FnOnce::call_once`'s, as rustc's `ClosureOnceShim` is; a
/// constructor's reify shim is the constructor's. The `Fn`-trait call of a fn item or pointer
/// in a vtable (rustc's `FnPtrShim`, one per trait method) is one body here, as blaze makes one
/// shim for the three methods. The C `main`, an `asm!` function and the allocator shim's are
/// none: rustc's collector counts no such item.
fn instantiates(db: &Db, kind: &Kind) -> Option<Instantiates> {
    let Kind::Code(instance, _) = kind else { return None };
    let shim = |what: &str| ContentId::of(SHIM_BODY_DOMAIN, what.as_bytes());
    let function = |func| blaze_items::item_key(db, ItemId::Function(func)).0;
    let body = match instance.def {
        InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => function(func),
        InstanceDef::Closure(id) => {
            blaze_items::key::closure_key(db, InternedClosureId::from_id(id)).0
        }
        InstanceDef::Coroutine(id) => {
            blaze_items::key::coroutine_key(db, InternedCoroutineId::from_id(id)).0
        }
        // rustc's by-move body of a coroutine is a `DefId` of its own.
        InstanceDef::CoroutineByMove(id) => {
            let coroutine = blaze_items::key::coroutine_key(db, InternedCoroutineId::from_id(id)).0;
            Hasher::new(SHIM_BODY_DOMAIN).part(b"by move").id(&coroutine).finish()
        }
        InstanceDef::DropGlue => shim("drop glue"),
        InstanceDef::CallOnceShim | InstanceDef::FnPointerShim => shim("FnOnce::call_once"),
        InstanceDef::ReifyShim { receiver: true } => {
            shim("the Fn-trait call of a fn item or pointer")
        }
        InstanceDef::ReifyShim { receiver: false } => {
            match instance.args.as_ref().type_at(0).kind() {
                TyKind::FnDef(def, _) => match def.0 {
                    CallableDefId::FunctionId(func) => function(func),
                    CallableDefId::StructId(it) => {
                        blaze_items::item_key(db, ItemId::Adt(AdtId::StructId(it))).0
                    }
                    CallableDefId::EnumVariantId(it) => {
                        let variant = it.lookup(db);
                        let parent =
                            blaze_items::item_key(db, ItemId::Adt(AdtId::EnumId(variant.parent))).0;
                        Hasher::new(SHIM_BODY_DOMAIN)
                            .part(b"variant")
                            .id(&parent)
                            .part(variant.name.as_str().as_bytes())
                            .finish()
                    }
                },
                _ => shim("the Fn-trait call of a fn item or pointer"),
            }
        }
    };
    Some(Instantiates { body, drop_glue: matches!(instance.def, InstanceDef::DropGlue) })
}

/// The crate whose `#![recursion_limit]` holds what the walk reaches from `kind`: the crate it
/// is built for (a function's, a closure's owner's, a shim's environment's). `None` for an
/// `asm!` function and the allocator shim's, which are of no crate.
fn limit_crate(db: &Db, kind: &Kind) -> Option<Crate> {
    match kind {
        Kind::Code(instance, env) => match instance.def {
            InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => Some(func.krate(db)),
            def @ (InstanceDef::Closure(_)
            | InstanceDef::Coroutine(_)
            | InstanceDef::CoroutineByMove(_)) => {
                closure_loc(db, def).map(|loc| loc.owner.module(db).krate(db))
            }
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => Some(env.krate),
        },
        Kind::Start(main) => Some(main.krate(db)),
        Kind::Asm(..) | Kind::Allocator(_) => None,
    }
}

/// How many characters of an argument an error spells before `…` (`display_truncated`'s
/// `max_size`): a type of rust#37311's chain has 2^k parts as a tree, which only a bounded
/// spelling can print.
const SPELLED_CHARS: usize = 40;

/// An instance as an error spells it: its body and its arguments, each cut short.
fn spelled(db: &Db, kind: &Kind) -> String {
    use hir_ty::display::HirDisplay as _;
    let Kind::Code(instance, env) = kind else { return super::linked::debug_name(db, kind) };
    let target = hir_ty::display::DisplayTarget::from_crate(db, env.krate);
    let args: Vec<String> = (instance.args.as_ref().iter())
        .filter(|arg| arg.region().is_none())
        .map(|arg| arg.display_truncated(db, Some(SPELLED_CHARS), target).to_string())
        .collect(); // alloc: an error's text
    match args.is_empty() {
        true => body_named(db, instance.def),
        false => format!("{}::<{}>", body_named(db, instance.def), args.join(", ")), // alloc: an error's text
    }
}

/// The body an instance is of, by its path.
fn body_named(db: &Db, def: InstanceDef) -> String {
    let path = |func| blaze_items::key::path(db, ItemId::Function(func)).to_string(); // alloc: an error's text
    match def {
        InstanceDef::Fn(func) => path(func),
        InstanceDef::VtableShim(func) => format!("the vtable shim of {}", path(func)), // alloc: an error's text
        def @ (InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_)) => {
            match closure_loc(db, def).map(|loc| loc.owner) {
                Some(InferBodyId::DefWithBodyId(owner)) => {
                    format!(
                        "a closure in {}",
                        blaze_items::key::path(db, ItemId::of_body(db, owner))
                    ) // alloc: an error's text
                }
                _ => "a closure in an anonymous constant".to_owned(), // alloc: an error's text
            }
        }
        InstanceDef::DropGlue => "core::ptr::drop_in_place".to_owned(), // alloc: an error's text
        InstanceDef::CallOnceShim => "the `FnOnce::call_once` shim".to_owned(), // alloc: an error's text
        InstanceDef::FnPointerShim => "the fn pointer shim".to_owned(), // alloc: an error's text
        InstanceDef::ReifyShim { .. } => "the reify shim".to_owned(),   // alloc: an error's text
    }
}

/// [`Frontend::past_recursion_limit`]: rustc's error, with the function, the instance, how often
/// the function repeats above it on its path, and the crate whose limit it is.
fn past_recursion_limit(
    db: &Db,
    instance: &HirInstance,
    depth: u32,
    limit: u32,
    head: &HirInstance,
    limit_of: &HirInstance,
) -> CodegenError {
    let krate = match limit_crate(db, &limit_of.0) {
        Some(krate) => krate.named(db).to_string(), // alloc: an error's text
        None => "<none>".to_owned(), // alloc: an error's text; a head of no crate has no limit
    };
    let (body, counted) = match &instance.0 {
        Kind::Code(it, _) => {
            let quarter = if matches!(it.def, InstanceDef::DropGlue) {
                ", drop glue counting a quarter"
            } else {
                ""
            };
            (body_named(db, it.def), quarter)
        }
        kind => (super::linked::debug_name(db, kind), ""),
    };
    // The item it is an instance of. It has no instance key: the walk refused it before it
    // described it, and its key is its description's. An `asm!` function and the allocator
    // shim's are never counted, so never here.
    let item = item_of(db, &instance.0)
        .map_or(ItemKey(ContentId::default()), |it| blaze_items::item_key(db, it));
    CodegenError {
        unit: InstanceKey(ContentId::default()),
        item,
        // alloc: the error's message
        what: format!(
            "reached the recursion limit while instantiating `{}`: `{body}` is instantiated {depth} \
             times above it on the path the build reached it by from `{}`{counted}, past crate \
             `{krate}`'s recursion_limit of {limit} (`#![recursion_limit = \"N\"]` in `{krate}` raises it)",
            spelled(db, &instance.0),
            spelled(db, &head.0),
        ),
        front: None,
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
        Kind::Asm(..) | Kind::Allocator(_) => None,
    }
}

impl HirInstance {
    /// Whether an edit of `body` alone may change this instance's key: its code reads
    /// `body`'s ([`code_of`]: `body` itself, a closure or coroutine written in it, the start
    /// of a `main` that is it, a shim adapting it), or its type arguments name a closure or
    /// coroutine written in it (`block_on::<its future>`, its drop glue). No other instance's
    /// key reads a body (docs/v2/plan/design/item-hash.md).
    pub fn of_body(&self, db: &Db, body: DefWithBodyId) -> bool {
        let instance = match &self.0 {
            Kind::Code(instance, _) => instance,
            Kind::Start(main) => return DefWithBodyId::from(*main) == body,
            Kind::Asm(..) | Kind::Allocator(_) => return false,
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

/// [`HirFrontend::root`], given the database. Each lifetime parameter is erased, as in the
/// instance a call names, so the root and the call are one instance.
pub(crate) fn root_in(db: &Db, func: FunctionId) -> HirInstance {
    let interner = DbInterner::new_no_crate(db);
    let erased = Region::new_erased(interner);
    rooted(db, func, GenericArgs::for_item(interner, func.into(), |_, _, _, _| erased.into()))
}

/// [`HirFrontend::root_with_fn_items`].
fn root_with_fn_items_in(
    db: &Db,
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
    Ok(rooted(db, func, args))
}

/// The root of `func` instantiated with `args`, in the empty environment of its crate.
fn rooted<'db>(db: &'db Db, func: FunctionId, args: GenericArgs<'db>) -> HirInstance {
    let krate = func.krate(db);
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate }.store();
    let instance = Instance { def: InstanceDef::Fn(func), args: args.store(), checked: false };
    let env = instance.home_env(db, &env);
    HirInstance(Kind::Code(instance, env))
}

/// The code of the body of `kind` as a caller builds it in
/// ([`blaze_codegen::backend::Described::built_in_code`]): its [`code_of`]. A copy built in is
/// built under the copy's options (`MirOptions::built_in`, which `hir-ty-mir`'s inliner and
/// `Build::built_in` both follow), which read no dyn facts: the copy's own part of the code is
/// its plain body. The caller's `Devirtualize` may still make a `dyn` call in the copy direct
/// after `Inline`; the facts it read are the caller's (`Inlined::devirtualized`), keying the
/// caller's code. The callee's node holds this code and [`inlined`] names it by it, so the two
/// compare (`blaze-codegen` `driver`'s `unkept_inline`); its own unit's code may hash the dyn
/// facts it read beside it ([`super::devirt::code`]).
///
/// A copy bakes in the names of the files its `file!()` reads as its own unit's does, so they
/// are folded in ([`with_names`], untold: a copy's locals are its caller's to tell).
pub(crate) fn built_in_code(db: &Db, hashes: &HashSource, kind: &Kind) -> Result<CodeHash, String> {
    let names = kind_body(kind).map(|body| hashes.names_read(db, body, false)).transpose()?;
    Ok(with_names(code_of(db, hashes, kind)?, names.map(|it| it.0)))
}

/// The code hashes of the bodies the compile of `instance` built in, sorted, each once, each by
/// its [`built_in_code`]: its [`blaze_codegen::CompiledFunction::inlined`]. `built_in` is what
/// the build recorded it put there under its unit's settings ([`crate::Pending::built_in`]),
/// never a walk of it again: the driver checks it against what the walk ahead of the build
/// listed, which its key holds (`unkept_inline`), so a walk that misses what a build does is
/// refused, not keyed. A callee is named as the walk names it ([`super::places::call_ref`] in the
/// environment the walk lists its calls in). Nothing is allocated for one that builds nothing
/// in, as nearly every one.
pub(crate) fn inlined(
    db: &Db,
    hashes: &HashSource,
    instance: &HirInstance,
    built_in: &[Instance],
) -> Result<Vec<CodeHash>, String> {
    if built_in.is_empty() {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    }
    let env = match &instance.0 {
        Kind::Code(_, env) => env.clone(), // clone: two `Copy` ids in a `Clone`-only type
        Kind::Start(main) => {
            ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main.krate(db) }.store()
        }
        Kind::Allocator(_) | Kind::Asm(..) => {
            // alloc: the error's message
            return Err(format!(
                "{instance:?} built in {built_in:?}, though nothing of its kind builds a body in"
            ));
        }
    };
    let mut found = Vec::with_capacity(built_in.len()); // alloc: the list returned
    for it in built_in {
        let FrontRef::Function(callee) =
            super::places::call_ref(db, it.def, it.args.as_ref(), &env)?
        else {
            // alloc: the error's message
            return Err(format!(
                "{instance:?} built in {it:?}, which its callers name as no function"
            ));
        };
        found.push(built_in_code(db, hashes, &callee.0)?);
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

/// Whether `kind` runs only when something has gone wrong ([`Described::cold`]): a
/// [`crate::cold_fn`], or a closure or coroutine of one. A shim is not.
fn cold_of(db: &Db, kind: &Kind) -> bool {
    let Kind::Code(instance, env) = kind else { return false };
    let func = match instance.def {
        InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => func,
        InstanceDef::Closure(_) | InstanceDef::Coroutine(_) | InstanceDef::CoroutineByMove(_) => {
            match closure_loc(db, instance.def).map(|it| it.owner) {
                Some(InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func))) => func,
                _ => return false,
            }
        }
        _ => return false,
    };
    crate::cold_fn(db, DbInterner::new_with(db, env.krate).lang_items(), func)
}

/// Which item an instance is of. A shim is of the trait method or lang item rustc names it
/// by: drop glue is `drop_in_place`, a vtable's `call_once` is `FnOnce::call_once`.
pub(super) fn item_of(db: &Db, kind: &Kind) -> Result<ItemId, String> {
    let (instance, env) = match kind {
        Kind::Code(instance, env) => (instance, env),
        Kind::Start(main) => return Ok(ItemId::Function(*main)),
        // alloc: the error's message
        Kind::Asm(..) => return Err("an `asm!` function, which is no item".to_owned()),
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
        InstanceDef::ReifyShim { .. } => match instance.args.as_ref().type_at(0).kind() {
            TyKind::FnDef(def, _) => Ok(match def.0 {
                CallableDefId::FunctionId(it) => ItemId::Function(it),
                CallableDefId::StructId(it) => ItemId::Adt(AdtId::StructId(it)),
                CallableDefId::EnumVariantId(it) => {
                    ItemId::Adt(AdtId::EnumId(it.lookup(db).parent))
                }
            }),
            _ => lang(lang_items().FnOnce_call_once, "FnOnce::call_once"),
        },
    }
}

const SHIM_DOMAIN: &str = "blaze v2 shim 1";
/// An `asm!` function is no item: it is named by its text.
const ASM_ITEM_DOMAIN: &str = "blaze v2 asm item 1";
const ASM_DOMAIN: &str = "blaze v2 asm 1";
const START_DOMAIN: &str = "blaze v2 start 1";
/// A cached function's code with the code of its body it writes ([`fncache::written_code`]).
const CACHED_DOMAIN: &str = "blaze v2 cached function 1";
/// A code with what its body reads of names beside it ([`with_names`]).
const WITH_NAMES_DOMAIN: &str = "blaze v2 code with names read 1";
/// The PGO name of an instance hashes this ID ([`blaze_items::pgo`]), so the profile lints
/// compute a plain function's name with the same domain.
use blaze_items::pgo::ARGS_DOMAIN;
/// A compile identity in place of the arguments: never equal to an [`ARGS_DOMAIN`] hash.
const SHARED_ARGS_DOMAIN: &str = "blaze v2 shared args 2";
/// A compile identity and the arguments' content ID, for a generic whose told locals' types
/// are in its shape ([`HirFrontend::generic_args`]).
const TOLD_ARGS_DOMAIN: &str = "blaze v2 told args 1";
/// A compile identity with which place of its references each request of the code is given
/// ([`refs::placement`]), for a generic shared under it ([`HirFrontend::generic_args`]).
const PLACED_ARGS_DOMAIN: &str = "blaze v2 placed args 1";

/// Whether an instance with no compile identity for `why` is compiled for its arguments alone:
/// [`Unshared::is_per_instance`], which compile identity reads for a caller too.
fn per_instance(why: &Unshared) -> bool {
    why.is_per_instance()
}

/// What an instance runs, as compile identity names it.
fn subject_of(def: InstanceDef) -> Subject {
    match def {
        InstanceDef::Fn(func) => Subject::Body(dispatch::Body::Fn(func)),
        InstanceDef::Closure(id) => Subject::Body(dispatch::Body::Closure(id)),
        InstanceDef::Coroutine(id) => Subject::Body(dispatch::Body::Coroutine(id)),
        InstanceDef::CoroutineByMove(id) => Subject::Body(dispatch::Body::CoroutineByMove(id)),
        InstanceDef::DropGlue => Subject::Shim(Shim::DropGlue),
        InstanceDef::CallOnceShim => Subject::Shim(Shim::CallOnce),
        InstanceDef::VtableShim(func) => Subject::Shim(Shim::Vtable(func)),
        InstanceDef::FnPointerShim => Subject::Shim(Shim::FnPointer),
        InstanceDef::ReifyShim { receiver } => Subject::Shim(Shim::Reify { receiver }),
    }
}

/// What an instance's code is. A shim's is what kind of shim it is, with the code of the
/// function whose signature it adapts: what else it reads is its type argument's.
fn code_of(db: &Db, hashes: &HashSource, kind: &Kind) -> Result<CodeHash, String> {
    let (instance, env) = match kind {
        Kind::Code(instance, env) => (instance, env),
        Kind::Asm(text, _) => return Ok(CodeHash(ContentId::of(ASM_DOMAIN, text.as_bytes()))),
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
            cached(db, hashes.code(db, Body::Item(func.into()))?, func, instance, env)
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

/// `code`, of the body of `func` as `instance` runs it in `env`, with what the function cache
/// keys of it ([`fncache::written_code`]): [`code_of`]'s and [`template_code_of`]'s one rule.
fn cached(
    db: &Db,
    code: CodeHash,
    func: FunctionId,
    instance: &Instance,
    env: &hir_ty::traits::StoredParamEnvAndCrate,
) -> Result<CodeHash, String> {
    let written = blaze_query::db::attach(db, || fncache::written_code(db, func, instance, env));
    let Some(written) = written else {
        return Ok(code);
    };
    let mut hasher = Hasher::new(CACHED_DOMAIN);
    hasher.id(&code.0);
    match written {
        Ok(hash) => hasher.update(&[0]).update(&hash.to_le_bytes()),
        Err(fncache::Uncached::Impure(why)) => hasher.update(&[1]).update(why.as_bytes()),
        // The body has no code identity: the function has no code either (rule 3).
        // clone: the error owns its text; the query keeps its own
        Err(fncache::Uncached::Refused(why)) => return Err(why.clone()),
        // The body reaches what blaze can't compile: the function has no code either.
        // alloc: the error's message
        Err(fncache::Uncached::Uncompiled(it)) => return Err(it.to_string()),
    };
    Ok(CodeHash(hasher.finish()))
}

/// What the template of `kind` is kept under when its positions are its own body's, as
/// [`code_of`] makes its code: its body's [`Hashes::template_code`] with `names`, what it reads
/// of names ([`Hashes::names_read`], folded into its code as well), and
/// the body whose span tables its positions are in. `None` for an instance whose template may
/// hold what is not its own (a shim's declaration of the function it adapts, `main`'s start,
/// the allocator's, `asm!`), or whose body is written in no body of its own: kept under its
/// code.
fn template_code_of(
    db: &Db,
    hashes: &HashSource,
    kind: &Kind,
    names: ContentId,
) -> Result<Option<(CodeHash, DefWithBodyId)>, String> {
    let Kind::Code(instance, env) = kind else { return Ok(None) };
    let Some(body) = kind_body(kind) else { return Ok(None) };
    let Some(owner) = blaze_query::db::attach(db, || super::hashes::span_owner(db, body)) else {
        return Ok(None);
    };
    let code = with_names(hashes.template_code(db, body)?, Some(names));
    match instance.def {
        InstanceDef::Fn(func) => Ok(Some((cached(db, code, func, instance, env)?, owner))),
        _ => Ok(Some((code, owner))),
    }
}

/// The body `kind` is compiled from, `None` for a shim, `main`'s start, the allocator's or
/// `asm!`, which are written in no body of their own.
fn kind_body(kind: &Kind) -> Option<Body> {
    let Kind::Code(instance, _) = kind else { return None };
    match instance.def {
        InstanceDef::Fn(func) => Some(Body::Item(func.into())),
        InstanceDef::Closure(id) => Some(Body::Closure(id)),
        InstanceDef::Coroutine(id) => Some(Body::Coroutine(id)),
        InstanceDef::CoroutineByMove(id) => Some(Body::CoroutineByMove(id)),
        InstanceDef::DropGlue
        | InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => None,
    }
}

/// `code` with what its compiled body reads of names beside it ([`Hashes::names_read`]):
/// the files its `file!()` names and its told locals' types. A body with none of its own (a
/// shim, `main`'s start, the allocator's, `asm!`) keeps `code`.
fn with_names(code: CodeHash, names: Option<ContentId>) -> CodeHash {
    let Some(names) = names else { return code };
    let mut hasher = Hasher::new(WITH_NAMES_DOMAIN);
    hasher.id(&code.0).id(&names);
    CodeHash(hasher.finish())
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
    instance.is_generic()
}

/// Marks the `asm!` functions among the references of code of `owner` as the image's when
/// the image holds that code ([`Kind::Asm`]'s flag), so an `asm!` function is its asker's.
fn asked_by(owner: Owner, refs: &mut [FrontRef<HirInstance, HirStatic>]) {
    if owner == Owner::Dependency {
        for it in refs {
            if let FrontRef::Function(HirInstance(Kind::Asm(_, image))) = it {
                *image = true;
            }
        }
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
        Kind::Asm(..) | Kind::Allocator(_) => None,
    }
}

/// Whether `krate`'s code refers to code an edit can change: it is local (a member, or a path
/// package such as one `[patch]`ed in), or it depends on such a crate, directly or not. A
/// registry crate over a local one (`zstd-safe` over a vendored `zstd-sys`) names the local
/// crate's types and calls its functions, so its code is the workspace's: in the dependency
/// image it would refer to code an edit changes. A proc-macro dependency is not followed: its
/// code never runs in the program, so nothing of the image refers to it, and an edit of a
/// local proc macro changes the code hashes of what it expands into, which rekeys the image.
#[blaze_query::tracked(returns(copy))]
fn reaches_local(db: &Db, krate: Crate) -> bool {
    let data = krate.data(db);
    match data.origin {
        CrateOrigin::Local { .. } => true,
        CrateOrigin::Lang(_) => false,
        _ => (data.dependencies.iter())
            .filter(|dep| !dep.crate_id.data(db).target_kind.is_proc_macro())
            .any(|dep| reaches_local(db, dep.crate_id)),
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
        TyKind::Coroutine(id, _) | TyKind::CoroutineWitness(id, _) => closure(id.0.loc(db).owner),
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
        Kind::Code(instance, _) => {
            match instance.def {
                InstanceDef::Fn(_) => hasher.update(&[0]).id(&item.0),
                InstanceDef::Closure(id) => hasher.update(&[1]).id(&blaze_items::key::closure_key(
                    db,
                    InternedClosureId::from_id(id),
                )
                .0),
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
            }
        }
        Kind::Start(_) => hasher.update(&[10]).id(&item.0),
        Kind::Asm(..) => hasher.update(&[11]).id(&item.0),
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
    if !matches!(kind, Kind::Asm(..) | Kind::Allocator(_)) {
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

/// [`crate::Instance::compiled_by_user`] of `kind`'s instance: the one rule of which origin an
/// instance is compiled for ([`HirFrontend::compiled_for`]) and which units' walk its
/// references are listed for ([`HirFrontend::inlines`]), so the two never disagree.
fn user_compiled(db: &Db, kind: &Kind) -> bool {
    let Kind::Code(instance, _) = kind else { return false };
    instance.compiled_by_user(db)
}

/// The symbol outside code names an instance by: `#[no_mangle]`, `#[export_name]`, the
/// standard library's functions linked by name, the C `main`. The C `main`'s name is its
/// crate's target's, so a target that does not load is an error.
pub(super) fn symbol_of(db: &Db, kind: &Kind) -> Result<Option<String>, String> {
    Ok(match kind {
        Kind::Code(instance, _) => match instance.def {
            InstanceDef::Fn(func) if !instance.is_generic() => {
                exported_symbol(db, func).or_else(|| linked_symbol(db, func))
            }
            _ => None,
        },
        Kind::Start(main) => {
            // alloc: the error's message
            let target = crate::target_of_crate(db, main.krate(db)).map_err(|e| e.to_string())?;
            // alloc: once per program, the C `main`; `Described::symbol` owns its name
            Some(target.entry().0.to_owned())
        }
        // alloc: one of the shim's few entries, once per program
        Kind::Allocator(forward) => Some(forward.symbol.to_owned()),
        Kind::Asm(..) => None,
    })
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
        assert!(per_instance(&Unshared::TooDeep { depth: 129, limit: 128 }));
        for refused in [
            Unshared::UnknownIntrinsic {
                name: "new_intrinsic".to_owned(),
                body: "`f`".to_owned(),
                through: hir_ty_share::Through::Own,
            },
            Unshared::NoOptions,
            Unshared::NoLayout("T".to_owned()),
            Unshared::ConstEval("N".to_owned()),
            Unshared::Shape("a union"),
        ] {
            assert!(!per_instance(&refused), "{refused}");
        }
    }
}

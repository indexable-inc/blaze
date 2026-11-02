//! A unit compiled by blaze's code generator and linked by blaze's linker: its policy (the
//! unit's profile, and each package's where cargo's profile gives it another), its roots (a
//! program's `main`, a test harness's, a library's functions), the build through
//! `blaze_codegen::driver::Build`, and the link into an image kept between jobs, so the next
//! link of the same file relinks only what changed.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use blaze_codegen::{
    Backend, InstanceKey, NativeLib, NativeLibKind, Target,
    driver::{Build, Built},
    plan::{Mode as BuildMode, OptLevel, Origin, Override, Package, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Sanitize, Sanitizers, Toolchain},
};
use blaze_build_script::{
    apply::{Compile, Linked as LinkedTo},
    unit as bs,
};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_link::{Change, Image, Native, Output};
use blaze_units::schema::{self, Mode, Unit};
use hir_ty_codegen::v2::{
    Backends, BodyChecks, BuildTime, Databases, HashSource, HirFrontend, HirInstance, ItemHashes, LinkInputs,
    RootKind, Roots, Runs,
};

use super::{Job, Real, UnitReader, files::Mounted, graph::flag, script};
use crate::serve::work::{Full, Linking, OutputFields};

type KeptImage = crate::serve::work::Image<Real>;

#[cfg(test)]
mod tests;

/// What a unit makes, as the linker sees it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Makes {
    /// An `rlib` (or a check's `rmeta`): nothing linked; the functions it compiles are in the
    /// store for the programs that link it.
    Library,
    /// A binary, an example, a test, a build script.
    Executable,
    /// A `cdylib` or a proc macro, under its file's name.
    Shared { proc_macro: bool },
}

/// What `unit` makes; an error for what blaze does not make.
pub(crate) fn makes(unit: &Unit) -> Result<Makes, String> {
    let target = &unit.target;
    let has = |kind: &str| target.crate_types.iter().any(|it| it == kind) || target.kind.iter().any(|it| it == kind);
    if matches!(unit.mode, Mode::Test) || target.is_custom_build() || has("bin") {
        return Ok(Makes::Executable);
    }
    if target.is_proc_macro() {
        return Ok(Makes::Shared { proc_macro: true });
    }
    if has("cdylib") {
        return Ok(Makes::Shared { proc_macro: false });
    }
    if has("staticlib") || has("dylib") {
        let kind = if has("staticlib") { "staticlib" } else { "dylib" };
        return Err(format!(
            "`{}` asks for a `{kind}`, which blaze's linker does not write: crate types `lib`, \
             `rlib`, `cdylib`, `proc-macro` and `bin` are what it builds",
            target.name
        ));
    }
    Ok(Makes::Library)
}

fn root_kind(unit: &Unit, makes: &Makes) -> RootKind {
    match makes {
        Makes::Executable if matches!(unit.mode, Mode::Test) => RootKind::Test,
        Makes::Executable => RootKind::Executable,
        Makes::Shared { proc_macro: true } => RootKind::ProcMacro,
        Makes::Shared { proc_macro: false } => RootKind::Shared,
        Makes::Library => RootKind::Library,
    }
}

/// The triple `unit` is built for.
pub(crate) fn triple<'a>(job: &'a Job, unit: &'a Unit) -> &'a str {
    unit.platform.as_deref().unwrap_or(job.input.config.host.as_str())
}

/// The toolchain `unit` is compiled with: its target, the `-C target-cpu`,
/// `-C target-feature` of its rustflags, where it runs, the backends of this blaze.
pub(crate) fn toolchain(job: &Job, unit: usize) -> Result<Toolchain, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let plan = job.plan(unit).map_err(|it| it.what)?;
    hir_ty_codegen::v2::toolchain(triple(job, u), &plan.rustflags, runs(u))
        // alloc: an error
        .map_err(|it| format!("unit {unit} (`{}` of {}): {it}", u.target.name, u.pkg_id))
}

/// Where `unit`'s code runs. A dev build of the host's target (no `--target`) runs where it
/// is built: `dev` and `test` are cargo's profiles of `build`, `run`, `test` and their
/// build scripts and proc macros. A `--target` unit, a `release` or `bench` build, or one of a
/// custom profile, which may be shipped, runs elsewhere: the target's baseline, as rustc's code.
fn runs(unit: &Unit) -> Runs {
    let dev = matches!(unit.profile.name.as_str(), "dev" | "test");
    if unit.platform.is_none() && dev { Runs::Here } else { Runs::Elsewhere }
}

fn opt_level(level: &str) -> Result<OptLevel, String> {
    Ok(match level {
        "0" => OptLevel::O0,
        "1" => OptLevel::O1,
        "2" => OptLevel::O2,
        "3" => OptLevel::O3,
        "s" => OptLevel::S,
        "z" => OptLevel::Z,
        other => return Err(format!("opt-level `{other}` is none rustc takes")),
    })
}

pub(crate) fn debuginfo(debuginfo: &schema::DebugInfo) -> Result<DebugInfo, String> {
    Ok(match debuginfo {
        schema::DebugInfo::Level(0) => DebugInfo::None,
        schema::DebugInfo::Level(1) => DebugInfo::Limited,
        schema::DebugInfo::Level(2) => DebugInfo::Full,
        schema::DebugInfo::Named(name) => match name.as_str() {
            "none" | "false" => DebugInfo::None,
            "line-tables-only" | "line-directives-only" => DebugInfo::LineTablesOnly,
            "limited" => DebugInfo::Limited,
            "full" | "true" => DebugInfo::Full,
            other => return Err(format!("debuginfo `{other}` is none rustc takes")),
        },
        schema::DebugInfo::Level(other) => return Err(format!("debuginfo `{other}` is none rustc takes")),
    })
}

fn panic(panic: &str) -> Result<Panic, String> {
    match panic {
        "unwind" => Ok(Panic::Unwind),
        "abort" => Ok(Panic::Abort),
        other => Err(format!("panic strategy `{other}` is none rustc takes")),
    }
}

/// The crates compiled for build scripts and proc macros, not for the program, as the code
/// generator takes them. Worked out once per job from [`Graph`](super::graph::Graph)'s bitset.
fn build_time(job: &Job) -> std::sync::Arc<BuildTime> {
    let crates = job.build_time.get_or_init(|| {
        let mut crates = blaze_hash::HashSet::default();
        for at in 0..job.graph.len() as u32 {
            if job.graph.build_time(at)
                && let Some(krate) = job.crates.krate(at as usize)
            {
                crates.insert(krate);
            }
        }
        std::sync::Arc::new(BuildTime(crates))
    });
    std::sync::Arc::clone(crates)
}

fn is_build_time(unit: &Unit) -> bool {
    unit.target.is_custom_build() || unit.target.is_proc_macro()
}

/// Whose code `unit`'s roots are, for their settings.
pub(crate) fn origin(job: &Job, unit: &Unit) -> Result<Origin, String> {
    let package = job.package(unit).map_err(|it| it.what)?;
    Ok(match () {
        _ if unit.is_std => Origin::Std,
        _ if is_build_time(unit) => Origin::BuildTime(Package::new(&package.name)),
        _ if package.is_member => Origin::Member(Package::new(&package.name)),
        _ => Origin::Dependency(Package::new(&package.name)),
    })
}

/// What blaze makes of cargo's `codegen-backend` for `package`'s code at `level`: `llvm`,
/// rustc's default, leaves the machine code blaze's own choice, and so does `cranelift` where
/// the level compiles with it already. Any other is an error naming it, never a key ignored:
/// blaze's code generator has no setting that puts a backend over the level's.
pub(crate) fn codegen_backend(profile: &schema::Profile, package: &str, level: OptLevel) -> Result<(), String> {
    let asked = match profile.codegen_backend.as_deref() {
        None | Some("llvm") => return Ok(()),
        Some(asked) => asked,
    };
    let name = &profile.name;
    match asked {
        "cranelift" if level.backend(false).is_some_and(Backend::is_cranelift) => Ok(()),
        "cranelift" | "stencil" => Err(format!(
            "the profile `{name}` of `{package}` asks for codegen-backend `{asked}`, and blaze's code \
             generator takes no backend over the one opt-level {} compiles with",
            profile.opt_level
        )),
        other => Err(format!(
            "the profile `{name}` of `{package}` asks for codegen-backend `{other}`, which blaze has \
             none of (`cranelift`, `stencil`, `llvm`)"
        )),
    }
}

/// The sanitizers of a build for `target` with `rustflags`, after refusing the `-Z` flags blaze
/// does not do ([`crate::unstable`]) and checking that blaze's toolchain has their runtimes
/// ([`crate::unstable::runtimes`]). A sanitized build stops until blaze's linker links one.
pub(crate) fn sanitize(rustflags: &[String], target: &str) -> Result<Sanitize, String> {
    if let Some(why) = crate::unstable::refusal(rustflags) {
        return Err(why);
    }
    let sanitize = crate::unstable::sanitize(rustflags)?;
    if sanitize.sanitizers != Sanitizers::NONE {
        let sysroot = crate::toolchain::sysroot().ok_or("blaze's toolchain is not installed")?;
        crate::unstable::runtimes(sanitize.sanitizers, target, &sysroot)?;
    }
    match sanitize.is_empty() {
        true => Ok(sanitize),
        false => Err(crate::unstable::UNLINKED.to_owned()),
    }
}

/// What a build's rustflags ask of profile-guided optimization: nothing, or to build with the
/// profile at a path as the flags give it (`-C profile-use=PATH`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PgoFlag<'a> {
    None,
    Use(&'a str),
}

/// The profile-guided optimization of a build with `rustflags`. The last `-C profile-use` is
/// the one, as rustc takes a codegen option given twice; one naming no file is an error. A
/// `-C profile-generate` is an error naming it: blaze's linker writes no profile sections yet,
/// so an instrumented binary would count into nothing and write no profile.
pub(crate) fn pgo(rustflags: &[String]) -> Result<PgoFlag<'_>, String> {
    let mut asked = PgoFlag::None;
    let mut flags = rustflags.iter().map(String::as_str);
    while let Some(flag) = flags.next() {
        let option = match flag {
            "-C" | "--codegen" => flags.next().unwrap_or(""),
            _ => match flag.strip_prefix("--codegen=").or_else(|| flag.strip_prefix("-C")) {
                Some(option) => option,
                None => continue,
            },
        };
        let (name, value) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (option, None),
        };
        match name {
            "profile-generate" | "profile_generate" => {
                // alloc: an error
                return Err(format!(
                    "the flags for rustc have `-C {option}`; blaze's linker writes no profile sections yet \
                     (`__llvm_prf_data`, `__llvm_prf_cnts`, `__llvm_prf_names` and the profile runtime), so an \
                     instrumented binary would write no profile, and blaze does not build with it"
                ));
            }
            "profile-use" | "profile_use" => match value {
                Some(path) if !path.is_empty() => asked = PgoFlag::Use(path),
                _ => {
                    // alloc: an error
                    return Err(format!(
                        "the flags for rustc have `-C {option}`, which names no profile: it takes \
                         `-C profile-use=PATH`, PATH a `.profdata`"
                    ));
                }
            },
            _ => {}
        }
    }
    Ok(asked)
}

/// The profiles a job's `-C profile-use`s name, each read once for the job, by its path as the
/// flags give it: a job has one or two (the target's `RUSTFLAGS` and the host's).
pub(crate) struct Profiles(Box<[(String, Result<Profiled, String>)]>);

/// A profile read for a job: its content ID, its bytes given to the code generator while the
/// job lives, and those bytes again until a build puts them in the store.
#[cfg_attr(not(feature = "llvm"), allow(dead_code))]
pub(crate) struct Profiled {
    id: ContentId,
    unstored: parking_lot::Mutex<Option<Vec<u8>>>,
    #[cfg(feature = "llvm")]
    _given: hir_ty_codegen::v2::Profile,
}

/// The job's profiles, read the first time a unit asks: every unit's flags, read once.
fn profiles(job: &Job) -> &Profiles {
    job.profiles.get_or_init(|| {
        // alloc: once per job, one entry per profile its units name
        let mut read: Vec<(String, Result<Profiled, String>)> = Vec::new();
        for plan in &job.units().plans {
            if let Ok(PgoFlag::Use(path)) = pgo(&plan.rustflags)
                && !read.iter().any(|it| it.0 == path)
            {
                // alloc: once per job and profile
                read.push((path.to_owned(), read_profile(&job.input.config.invocation.root, path)));
            }
        }
        Profiles(read.into_boxed_slice())
    })
}

/// The profile at `path`, relative to the workspace's root (where cargo runs rustc for its
/// members), read off the machine and given to the code generator; an error naming the path
/// when it can't be read or given.
pub(crate) fn read_profile(root: &str, path: &str) -> Result<Profiled, String> {
    let at = std::path::Path::new(root).join(path);
    let bytes = std::fs::read(&at).map_err(|error| {
        // alloc: an error
        format!("the flags for rustc have `-C profile-use={path}`, and `{}` can't be read: {error}", at.display())
    })?;
    profiled(path, bytes)
}

#[cfg(feature = "llvm")]
fn profiled(path: &str, bytes: Vec<u8>) -> Result<Profiled, String> {
    let id = hir_ty_codegen::v2::profile_id(&bytes);
    let given = hir_ty_codegen::v2::give_profile(id, &bytes)
        // alloc: an error
        .map_err(|what| format!("the profile of `-C profile-use={path}`: {what}"))?;
    Ok(Profiled { id, unstored: parking_lot::Mutex::new(Some(bytes)), _given: given })
}

#[cfg(not(feature = "llvm"))]
fn profiled(path: &str, _bytes: Vec<u8>) -> Result<Profiled, String> {
    // alloc: an error
    Err(format!(
        "the flags for rustc have `-C profile-use={path}`, and this blaze is built without LLVM \
         (blaze-cargo's feature `llvm`), the one back end that builds with a profile"
    ))
}

/// The [`Pgo`] of a unit whose flags ask `asked`: the content ID of the job's read of the
/// profile, or the error that read stopped with.
fn pgo_of(job: &Job, asked: PgoFlag<'_>) -> Result<Pgo, String> {
    let PgoFlag::Use(path) = asked else { return Ok(Pgo::None) };
    match profiles(job).0.iter().find(|it| it.0 == path) {
        Some((_, Ok(read))) => Ok(Pgo::Use(read.id)),
        // alloc: an error
        Some((_, Err(what))) => Err(what.clone()),
        // alloc: an error
        None => Err(format!("`-C profile-use={path}` is in no unit's flags the job read its profiles from")),
    }
}

/// The [`Pgo`] `unit` is built with, as its [`policy`] has it: what its record key holds of a
/// profile to use, whose path in its flags says nothing of the bytes there.
pub(crate) fn unit_pgo(job: &Job, unit: usize) -> Result<Pgo, String> {
    pgo_of(job, pgo(&job.plan(unit).map_err(|it| it.what)?.rustflags)?)
}

/// What a build with `pgo` puts in the store beside its code: the profile's bytes under its
/// content ID, so a build reads them by `Key(id)` without the path, put by the first build of
/// the job that finds them missing. A profile the job did not give is an error naming
/// `give_profile`.
fn profile_put(job: &Job, pgo: Pgo) -> Result<Option<blaze_codegen::driver::Put>, String> {
    let Pgo::Use(id) = pgo else { return Ok(None) };
    let read = profiles(job).0.iter().find_map(|it| it.1.as_ref().ok().filter(|read| read.id == id)).ok_or_else(|| {
        // alloc: an error
        format!(
            "the profile {id:x} the build uses was not given to the code generator \
             (`hir_ty_codegen::v2::give_profile`)"
        )
    })?;
    let key = blaze_cas::Key(id);
    if job.input.store.has(&blaze_cas::enter(), &key) {
        return Ok(None);
    }
    let bytes = read.unstored.lock().take();
    Ok(bytes.map(|bytes| blaze_codegen::driver::Put { key, kind: blaze_cas::Kind::Output, bytes }))
}

/// Gives back the bytes of a profile put that a failed build will not hand to the store.
fn unput_profile(job: &Job, put: blaze_codegen::driver::Put) {
    let read = profiles(job).0.iter().find_map(|it| it.1.as_ref().ok().filter(|read| blaze_cas::Key(read.id) == put.key));
    if let Some(read) = read {
        *read.unstored.lock() = Some(put.bytes);
    }
}

/// How `unit`'s build compiles every function: its own profile, and for each package of its
/// closure whose unit cargo gives another profile (`[profile.*.package.*]`), that one; the
/// sanitizers of its rustflags, the same for every function, std's too, as cargo passes
/// `RUSTFLAGS` to every crate.
pub(crate) fn policy(job: &Job, unit: usize, toolchain: Toolchain) -> Result<Policy, String> {
    let root = job.unit(unit).map_err(|it| it.what)?;
    let rustflags = &job.plan(unit).map_err(|it| it.what)?.rustflags;
    let sanitize = sanitize(rustflags, triple(job, root))?;
    let pgo = pgo_of(job, pgo(rustflags)?)?;
    let base = &root.profile;
    let opt = opt_level(&base.opt_level)?;
    codegen_backend(base, &job.package(root).map_err(|it| it.what)?.name, opt)?;
    let info = debuginfo(&base.debuginfo)?;
    // BTreeMap: it is `Profile::packages`, which is serialized and hashed.
    let mut packages = BTreeMap::new();
    for dep in job.graph.closure(unit as u32).into_iter().skip(1) {
        if job.graph.has(dep, flag::STD) {
            continue;
        }
        let dep = job.unit(dep as usize).map_err(|it| it.what)?;
        let name = job.package(dep).map_err(|it| it.what)?.name.clone();
        let profile = &dep.profile;
        let level = opt_level(&profile.opt_level)?;
        codegen_backend(profile, &name, level)?;
        let dep_info = debuginfo(&profile.debuginfo)?;
        let differs = Override {
            opt_level: (level != opt).then_some(level),
            overflow_checks: (profile.overflow_checks != base.overflow_checks).then_some(profile.overflow_checks),
            debug_assertions: (profile.debug_assertions != base.debug_assertions)
                .then_some(profile.debug_assertions),
            debuginfo: (dep_info != info).then_some(dep_info),
        };
        if differs != Override::default() {
            packages.insert(name, differs);
        }
    }
    Ok(Policy {
        toolchain,
        profile: Profile {
            opt_level: opt,
            overflow_checks: base.overflow_checks,
            debug_assertions: base.debug_assertions,
            debuginfo: info,
            panic: panic(&base.panic)?,
            packages,
            build_override: Override::default(),
        },
        mode: BuildMode::Output,
        fold: true,
        sanitize,
        pgo,
    })
}

/// What a unit's codegen made ([`generate`]), kept for its link: nothing of the front end
/// that made it, which the link reads through its own task's ([`Parts::over`]).
pub(crate) struct Parts {
    pub(crate) built: Built,
    pub(crate) roots: Roots,
    pub(crate) policy: Policy,
}

impl Parts {
    /// What codegen made, read through `reader`'s front end. The unit's crate is the same in
    /// it as in the one codegen read: every input of the crate is of a unit it depends on,
    /// done before its codegen began, so a write applied since changed none of it.
    pub(crate) fn over<'a>(&'a self, job: &Job, reader: &'a UnitReader, mounted: &Arc<Mounted>) -> Generated<'a> {
        let frontend = frontend(job, reader, mounted).with_policy(&self.policy);
        Generated { built: &self.built, roots: &self.roots, frontend, policy: &self.policy, side: std::cell::OnceCell::new() }
    }
}

/// What a unit's codegen made, reading the task's front end.
pub(crate) struct Generated<'a> {
    pub(crate) built: &'a Built,
    pub(crate) roots: &'a Roots,
    pub(crate) frontend: HirFrontend<'a>,
    pub(crate) policy: &'a Policy,
    /// The span tables and debug names the build's code names: read once, by the first
    /// link or test selection that needs them, and none for a library.
    side: std::cell::OnceCell<LinkInputs>,
}

impl Generated<'_> {
    /// What the link takes beside the code ([`HirFrontend::link_inputs`]).
    pub(crate) fn side(&self) -> Result<&LinkInputs, String> {
        if let Some(side) = self.side.get() {
            return Ok(side);
        }
        let side = self.frontend.link_inputs(self.built).map_err(|it| it.what)?;
        Ok(self.side.get_or_init(|| side))
    }
}

/// The code generator's view of the job's front end, its files named as rustc names them. Every
/// thread of the build reads the one database of the task's reader by reference: the query
/// engine's reads take `&self`, so none waits on another and none is cloned.
fn frontend<'db>(job: &Job, reader: &'db UnitReader, mounted: &Arc<Mounted>) -> HirFrontend<'db> {
    frontend_over(job, reader.db(), reader.paths(), mounted)
}

/// [`frontend`] over the database `db`, whose files are `paths`: what the loop's writer reads
/// before it lends the front end to a task ([`super::harness::write`]).
pub(crate) fn frontend_over<'db>(
    job: &Job,
    db: &'db base_db::Db,
    paths: &Arc<std::collections::HashMap<base_db::FileId, String>>,
    mounted: &Arc<Mounted>,
) -> HirFrontend<'db> {
    let names = {
        let (mounted, paths) = (Arc::clone(mounted), Arc::clone(paths));
        move |file: base_db::FileId| -> Option<String> { paths.get(&file).map(|path| mounted.name(path)) }
    };
    HirFrontend::new(Databases::new(db), HashSource::Items(ItemHashes::new(Box::new(names))), build_time(job))
}

/// The tests of test unit `unit` as its harness lists them, compiling nothing: the same
/// listing, names and attributes as the test binary's ([`generate`]).
pub(crate) fn test_roots(job: &Job, reader: &UnitReader, unit: usize) -> Result<Roots, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let krate = job
        .crates
        .krate(unit)
        .ok_or_else(|| format!("unit {unit} (`{}`) has no crate in the front end", u.target.name))?;
    frontend(job, reader, &job.mounted).roots(krate, RootKind::Test).map_err(|it| it.what)
}

/// The store as a build reads it: with no lock, under the build's guard.
struct Reading<'a> {
    store: &'a blaze_cas::Reader,
    guard: &'a blaze_cas::Guard,
}

impl blaze_codegen::driver::Lookup for Reading<'_> {
    fn read(&self, key: &blaze_cas::Key) -> Result<Option<&[u8]>, String> {
        match self.store.get(self.guard, key) {
            Ok(entry) => Ok(entry.map(|it| it.bytes)),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }
}

/// `unit`'s functions compiled from its roots: `Err` with the code generator's errors, each
/// naming the function it could not compile. `checks` is the unit's check running beside it:
/// told the bodies the build reaches, it stops the build once it failed. `None` for a crate
/// whose check outcome is already known.
pub(crate) fn generate<'db>(
    job: &'db Job,
    reader: &'db UnitReader,
    unit: usize,
    makes: &Makes,
    mounted: &Arc<Mounted>,
    // dyn: `HirFrontend::with_checks` takes it; called once a level of the walk
    checks: Option<&'db dyn BodyChecks>,
) -> Result<Result<Parts, Vec<String>>, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let krate = job
        .crates
        .krate(unit)
        .ok_or_else(|| format!("unit {unit} (`{}`) has no crate in the front end", u.target.name))?;
    let toolchain = toolchain(job, unit)?;
    // The process's code generator compiles for one toolchain: the first unit sets it up, and
    // a unit of another is refused here, naming both.
    hir_ty_codegen::v2::select(&toolchain)?;
    let policy = policy(job, unit, toolchain)?;
    // A cancelled job's build stops within a function: its caller posts it again.
    let frontend = frontend(job, reader, mounted).with_policy(&policy).with_stop(&job.input.cancelled);
    let frontend = match checks {
        Some(checks) => frontend.with_checks(checks),
        None => frontend,
    };
    let roots = match frontend.roots(krate, root_kind(u, makes)) {
        Ok(it) => it,
        Err(error) => return Ok(Err(vec![error.what])),
    };
    let origin = origin(job, u)?;
    let asked: Vec<(HirInstance, Origin)> = roots.roots.iter().map(|it| (it.clone(), origin)).collect();
    // The job holds the profile given for as long as it lives, so for the whole build.
    let profile = profile_put(job, policy.pgo)?;
    let backends = Backends::default();
    let built = {
        let guard = blaze_cas::enter();
        let store = Reading { store: &job.input.store, guard: &guard };
        Build::new(&backends, &frontend, &policy, &job.memo)
            .owned_by(&u.target.name)
            .run_with_statics(&asked, &roots.statics, &store)
    };
    match built {
        Ok(mut built) => {
            built.puts.extend(profile);
            Ok(Ok(Parts { built, roots, policy }))
        }
        Err(errors) => {
            if let Some(put) = profile {
                unput_profile(job, put);
            }
            Ok(Err(errors.into_iter().map(|it| it.what).collect()))
        }
    }
}

/// An image kept linked at one path, and what is in it, so the next link of it is a change.
/// Each column is sorted, so a link's diff against it is one merge pass: `O(n)` with no hashing
/// and no allocation but the diff itself (keys are content hashes, 32 bytes each).
pub struct Linked {
    image: Image,
    output: Output,
    functions: Vec<InstanceKey>,
    statics: Vec<StaticRow>,
    natives: Vec<ContentId>,
    names: Vec<String>,
}

impl Linked {
    /// The image's tables as the linker counts them, each with its elements and bytes
    /// ([`blaze_api::Response::Tables`]): `relocation` and the rest.
    pub(crate) fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        self.image.tables()
    }
}

/// What of sorted `old` is not in sorted `new`, and what of `new` is not in `old`, in one pass.
pub(crate) fn diff<T: Ord + Copy>(old: &[T], new: &[T], removed: &mut Vec<T>, added: &mut Vec<T>) {
    let (mut a, mut b) = (0, 0);
    while a < old.len() && b < new.len() {
        match old[a].cmp(&new[b]) {
            std::cmp::Ordering::Less => {
                removed.push(old[a]);
                a += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(new[b]);
                b += 1;
            }
            std::cmp::Ordering::Equal => {
                a += 1;
                b += 1;
            }
        }
    }
    removed.extend_from_slice(&old[a..]);
    added.extend_from_slice(&new[b..]);
}

/// The symbol names of a link, sorted, each kept `String` moved from the last link's rather
/// than copied: only a name new to the image is allocated, and each name gone is moved out.
pub(crate) fn renamed(old: Vec<String>, new: &mut Vec<&str>) -> (Vec<String>, Vec<String>) {
    new.sort_unstable();
    new.dedup();
    let mut kept = Vec::with_capacity(new.len());
    let mut gone = Vec::new();
    let mut old = old.into_iter().peekable();
    for name in new.iter() {
        while old.peek().is_some_and(|it| it.as_str() < *name) {
            gone.extend(old.next());
        }
        match old.peek().is_some_and(|it| it.as_str() == *name) {
            true => kept.extend(old.next()),
            false => kept.push((*name).to_owned()),
        }
    }
    gone.extend(old);
    (kept, gone)
}

/// What rustc is given for `unit` from the build scripts it is linked with
/// ([`blaze_build_script::apply::Compile`]), in cargo's order ([`Graph::links`]).
///
/// [`Graph::links`]: super::graph::Graph::links
fn given(job: &Job, unit: u32) -> Result<Compile, String> {
    let units = job.units();
    let u = job.unit(unit as usize).map_err(|it| it.what)?;
    let runs = job.graph.links(unit);
    let mut linked = Vec::with_capacity(runs.len());
    let mut own = Vec::new();
    for &run in runs {
        let applied = job.scripts[run as usize].get().ok_or_else(|| {
            format!(
                "the build script run of `{}` that `{}` is linked with has not run",
                units.graph.units[run as usize].pkg_id, u.target.name
            )
        })?;
        let mine = job.graph.package(run) == job.graph.package(unit);
        linked.push(LinkedTo { own: mine, output: &applied.output });
        if mine {
            own.push(&applied.output);
        }
    }
    // `Compile::of` asks of the package only whether it has a library.
    let lib = bs::Target { name: String::new(), kind: vec!["lib".to_owned()], crate_types: Vec::new() };
    let targets = match job.graph.package_has_lib(unit) {
        true => vec![lib],
        false => Vec::new(),
    };
    let package = bs::Package { targets, ..bs::Package::default() };
    Ok(Compile::of(&package, &script::target_of(u), job.graph.has(unit, flag::TEST), &linked, &own))
}

/// The native libraries `unit` links, in rustc's order: each crate linked in, what depends
/// first ([`Graph::linked_in`]), the `-l`s its rustc is given (its build script's
/// `rustc-link-lib`s) then its `#[link]`s, then the unit's own `rustc-link-arg`s; and where to
/// look for them, the unit's `-L`s.
///
/// [`Graph::linked_in`]: super::graph::Graph::linked_in
fn natives(job: &Job, unit: usize, frontend: &HirFrontend<'_>) -> Result<(Vec<NativeLib>, Vec<PathBuf>), String> {
    let unit = unit as u32;
    let mine = given(job, unit)?;
    let mut libs = Vec::new();
    for at in job.graph.linked_in(unit) {
        let theirs;
        let compile = match at == unit {
            true => &mine,
            false => {
                theirs = given(job, at)?;
                &theirs
            }
        };
        libs.reserve(compile.libs.len());
        for lib in &compile.libs {
            libs.push(link_lib(lib)?);
        }
        if let Some(krate) = job.crates.krate(at as usize) {
            libs.extend(frontend.linked(krate).map_err(|it| it.what)?);
        }
    }
    for arg in &mine.link_args {
        libs.push(link_arg(arg)?);
    }
    Ok((libs, mine.search))
}

/// A native input a link read off the machine: its path, and the BLAKE3 hash of its contents.
pub(crate) type Read = (String, ContentId);

/// What [`link`] made: the file's bytes, every native input the link read off the machine, a
/// test binary's execution table, and how it reached the image.
pub(crate) struct LinkMade {
    pub(crate) bytes: Vec<u8>,
    pub(crate) read: Vec<Read>,
    pub(crate) table: Option<blaze_codegen::ExecTable>,
    pub(crate) how: Linking,
}

/// Why [`link`] made no image at `image`.
#[derive(Debug)]
pub(crate) struct LinkFailed {
    pub(crate) image: String,
    pub(crate) cause: LinkCause,
}

impl std::fmt::Display for LinkFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "linking `{}`: {}", self.image, self.cause)
    }
}

/// What stopped a link.
#[derive(Debug)]
pub(crate) enum LinkCause {
    /// blaze's linker refused, at `step`. The image kept is as it was before the link.
    Linker { step: LinkStep, error: blaze_link::LinkError },
    /// A native input on the link line could not be read.
    Native(blaze_usercode::inputs::Unreadable),
    /// A native input on the link line is gone from the machine.
    NativeGone(PathBuf),
    /// blaze's toolchain, whose C runtime and sanitizer runtimes the link takes, is not installed.
    NoToolchain,
    /// The image's path is no file name the linker can write.
    Unnamed,
    /// What the link is given (the unit, its build scripts' `-l`s and `-L`s, the code's side
    /// tables, a test binary's table) was refused before or after the linker ran: that step's
    /// own error.
    Given(String),
}

impl std::fmt::Display for LinkCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkCause::Linker { step, error } => write!(f, "{}: {error}", step.name()),
            LinkCause::Native(error) => write!(f, "{}: {}", error.path.display(), error.error),
            LinkCause::NativeGone(path) => write!(f, "{}: the native library is gone", path.display()),
            LinkCause::NoToolchain => f.write_str("blaze's toolchain is not installed"),
            LinkCause::Unnamed => f.write_str("the path is no file the linker can name"),
            LinkCause::Given(what) => f.write_str(what),
        }
    }
}

impl From<String> for LinkCause {
    fn from(what: String) -> LinkCause {
        LinkCause::Given(what)
    }
}

/// Where in a link blaze's linker refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LinkStep {
    /// Working out the [`Output`] for the unit's target.
    Output,
    /// Finding the link line's native inputs.
    Locate,
    /// Making a new image.
    Image,
    /// Staging the change into the image.
    Stage,
    /// Taking the staged image's bytes.
    Bytes,
}

impl LinkStep {
    pub(crate) fn name(self) -> &'static str {
        match self {
            LinkStep::Output => "the output",
            LinkStep::Locate => "finding the native inputs",
            LinkStep::Image => "making the image",
            LinkStep::Stage => "staging the change",
            LinkStep::Bytes => "writing the image",
        }
    }
}

/// `generated` linked into the image at `path` as `makes` says, and how. The image kept at
/// `path` is taken out of `images`, relinked and put back; a link that fails after taking it
/// puts it back as it was, so the next link still patches it. A kept image of another
/// [`Output`] is replaced, and the link says which fields changed ([`Full::Output`]).
pub(crate) fn link(
    job: &Job,
    unit: usize,
    path: &str,
    makes: &Makes,
    generated: &Generated<'_>,
    images: &mut Vec<KeptImage>,
) -> Result<LinkMade, LinkFailed> {
    // alloc: error path
    link_at(job, unit, path, makes, generated, images).map_err(|cause| LinkFailed { image: path.to_owned(), cause })
}

fn link_at(
    job: &Job,
    unit: usize,
    path: &str,
    makes: &Makes,
    generated: &Generated<'_>,
    images: &mut Vec<KeptImage>,
) -> Result<LinkMade, LinkCause> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let built = generated.built;
    let debug = match generated.policy.profile.debuginfo {
        DebugInfo::None => blaze_link::Debug::None,
        _ => blaze_link::Debug::Lines,
    };
    let shared = match makes {
        Makes::Shared { .. } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_owned();
            let exports = built.symbols.iter().map(|it| it.1.clone()).collect();
            Some((name, exports))
        }
        _ => None,
    };
    let test = job.graph.has(unit as u32, flag::TEST);
    let linker = |step: LinkStep| move |error: blaze_link::LinkError| LinkCause::Linker { step, error };
    let mut output =
        blaze_link::target::output(triple(job, u), shared, debug, true).map_err(linker(LinkStep::Output))?;
    output.exec_log = test && blaze_codegen::exec::records(generated.policy.mode);
    output.sanitize = generated.policy.sanitize;
    let (libs, search) = natives(job, unit, &generated.frontend)?;
    let sysroot = crate::toolchain::sysroot().ok_or(LinkCause::NoToolchain)?;
    let located =
        blaze_link::target::locate(&mut output, &libs, &search, &sysroot).map_err(linker(LinkStep::Locate))?;
    let side = generated.side()?;

    let (mut linked, changed) = kept_image(images, path, &output)?;
    // The image goes back whether the link is made or not: one that fails leaves it as it
    // was, and the next link is a change of it.
    let made = relink(path, generated, test, &located, side, &mut linked);
    images.push(KeptImage { path: path.to_owned(), kept: linked });
    let (bytes, read, table, full) = made?;
    Ok(LinkMade { bytes, read, table, how: linking(changed, full, output.incremental) })
}

/// The image to link `output` into at `path`, taken out of `images`, and the fields of
/// `output` that differ from the kept image's (`None` when none was kept there). A kept image
/// of another output is let go only once its replacement is made, so an error leaves
/// `images` as it was.
pub(crate) fn kept_image(
    images: &mut Vec<KeptImage>,
    path: &str,
    output: &Output,
) -> Result<(Linked, Option<OutputFields>), LinkCause> {
    let at = images.iter().position(|it| it.path == path);
    let changed = at.map(|at| OutputFields::between(&images[at].kept.output, output));
    if let (Some(at), Some(changed)) = (at, changed)
        && changed.is_empty()
    {
        return Ok((images.swap_remove(at).kept, Some(changed)));
    }
    let to = blaze_core::FileName::new(path.trim_start_matches('/')).ok_or(LinkCause::Unnamed)?;
    let fresh = Linked {
        image: Image::new(to, output.clone())
            .map_err(|error| LinkCause::Linker { step: LinkStep::Image, error })?,
        output: output.clone(),
        functions: Vec::new(),
        statics: Vec::new(),
        natives: Vec::new(),
        names: Vec::new(),
    };
    if let Some(at) = at {
        images.swap_remove(at);
    }
    Ok((fresh, changed))
}

/// How a link went: `changed` is the fields of the output that differ from the kept image's
/// (`None` when no image was kept), `full` whether blaze-link laid the image out whole
/// ([`blaze_link::Staged::full`]). A kept image of the same output laid out whole is one whose
/// first link failed (nothing was laid out to patch) or one that keeps no room.
pub(crate) fn linking(changed: Option<OutputFields>, full: bool, incremental: bool) -> Linking {
    match (changed, full) {
        (Some(changed), _) if !changed.is_empty() => Linking::Full(Full::Output(changed)),
        (_, false) => Linking::Spliced,
        (_, true) if !incremental => match changed {
            Some(_) => Linking::Full(Full::NotIncremental),
            None => Linking::First,
        },
        (_, true) => Linking::First,
    }
}

/// A static by its item, its instance and the hash of its whole definition.
pub(crate) type StaticRow = (ItemKey, ContentId, ContentId);

const STATIC_DOMAIN: &str = "blaze v2 link static 1";

/// Each of `statics` as a row, sorted, and what of `old`'s rows is gone and what is new: a
/// static put again unchanged is in neither, so the link neither moves it nor rewrites what
/// refers to it (blaze-link's [`Change`] is a delta). One whose definition changed is in both.
pub(crate) fn static_rows(
    old: &[StaticRow],
    statics: &[blaze_codegen::Static],
) -> Result<(Vec<StaticRow>, Vec<StaticRow>, Vec<StaticRow>), String> {
    let mut scratch = Vec::new();
    let mut rows = Vec::with_capacity(statics.len());
    for it in statics {
        scratch.clear();
        blaze_protocol::cbor::append(&mut scratch, it).map_err(|error| format!("the static {:?}: {error}", it.item))?;
        rows.push((it.item, it.instance, ContentId::of(STATIC_DOMAIN, &scratch)));
    }
    rows.sort_unstable();
    let (mut gone, mut new) = (Vec::new(), Vec::new());
    diff(old, &rows, &mut gone, &mut new);
    Ok((rows, gone, new))
}

/// [`link`]'s change of `linked` to what `generated` built, and whether blaze-link laid the
/// image out whole ([`blaze_link::Staged::full`]). Its columns are written only once the image
/// has taken the change, so after an error they still say what the image holds.
fn relink(
    path: &str,
    generated: &Generated<'_>,
    test: bool,
    located: &[blaze_link::target::Located],
    side: &LinkInputs,
    linked: &mut Linked,
) -> Result<(Vec<u8>, Vec<Read>, Option<blaze_codegen::ExecTable>, bool), LinkCause> {
    let built = generated.built;
    // Each native input by the hash of its contents, through the process's table of files by
    // their `stat`: one `stat` for a library the image already has, read only when it is new.
    let mut files = blaze_usercode::inputs::Disk::new(Vec::new());
    let mut read = Vec::with_capacity(located.len());
    let mut native_ids = Vec::with_capacity(located.len());
    for it in located {
        let hash = blaze_usercode::trace::Inputs::file(&mut files, &it.path)
            .map_err(LinkCause::Native)?
            // alloc: error path
            .ok_or_else(|| LinkCause::NativeGone(it.path.clone()))?;
        native_ids.push(ContentId(hash));
        read.push((it.path.to_string_lossy().into_owned(), ContentId(hash)));
    }
    let mut new_natives: Vec<ContentId> = native_ids.clone();
    new_natives.sort_unstable();
    new_natives.dedup();
    let mut removed_natives = Vec::new();
    let mut added_natives = Vec::new();
    diff(&linked.natives, &new_natives, &mut removed_natives, &mut added_natives);
    let mut bytes_of = Vec::with_capacity(added_natives.len());
    for (order, it) in located.iter().enumerate() {
        if added_natives.binary_search(&native_ids[order]).is_ok() {
            let bytes = std::fs::read(&it.path).map_err(|error| {
                // alloc: error path
                LinkCause::Native(blaze_usercode::inputs::Unreadable { path: it.path.clone(), error: error.to_string() })
            })?;
            // alloc: once per native input new to the image, which keeps it shared from here.
            bytes_of.push((order, Arc::<[u8]>::from(bytes)));
        }
    }

    let mut functions: Vec<InstanceKey> = built.functions.iter().map(|it| it.key).collect();
    functions.sort_unstable();
    let (statics, gone_statics, new_statics) = static_rows(&linked.statics, &built.statics)?;
    let mut removed = Vec::new();
    let mut added_keys = Vec::new();
    diff(&linked.functions, &functions, &mut removed, &mut added_keys);
    let mut patchable: Vec<InstanceKey> = built.patchable.iter().copied().collect();
    patchable.sort_unstable();
    let mut names: Vec<(String, Target)> = Vec::with_capacity(built.symbols.len() + built.static_symbols.len());
    for (key, name) in &built.symbols {
        let target = match patchable.binary_search(key).is_ok() {
            true => Target::Function(*key),
            false => Target::Direct(*key),
        };
        names.push((name.clone(), target));
    }
    for (item, instance, name) in &built.static_symbols {
        names.push((name.clone(), Target::Static { item: *item, instance: *instance }));
    }
    let mut now_named: Vec<&str> = names.iter().map(|it| it.0.as_str()).collect();
    let (kept_names, unnamed) = renamed(std::mem::take(&mut linked.names), &mut now_named);
    // clone: the names gone, a few a link, which an image that refuses the change still has
    let gone_names = unnamed.clone();
    let change = Change {
        added: built.functions.iter().filter(|it| added_keys.binary_search(&it.key).is_ok()).collect(),
        removed,
        // A whole build tiers nothing up, so no slot runs another function than its own.
        slots: Vec::new(),
        statics: built
            .statics
            .iter()
            .filter(|it| new_statics.binary_search_by(|row| (row.0, row.1).cmp(&(it.item, it.instance))).is_ok())
            .collect(),
        removed_statics: gone_statics.iter().map(|row| (row.0, row.1)).collect(),
        names,
        unnamed,
        // clone: once per image linked, a unit links one or two
        debug_names: side.debug_names.clone(),
        natives: bytes_of
            .iter()
            .map(|(order, bytes)| Native {
                id: native_ids[*order],
                name: &located[*order].name,
                bytes,
                kind: located[*order].kind,
                order: *order as u32,
            })
            .collect(),
        removed_natives,
        spans: side.spans.iter().map(|(anchor, bytes)| (*anchor, bytes.as_slice())).collect(),
    };
    let made = linked.image.stage(change).map_err(|error| (LinkStep::Stage, error)).and_then(|staged| {
        let full = staged.full();
        linked.image.bytes(staged).map(|bytes| (bytes, full)).map_err(|error| (LinkStep::Bytes, error))
    });
    let (bytes, full) = match made {
        Ok(made) => made,
        Err((step, error)) => {
            // Every name the image may still hold: one it never took is unnamed harmlessly.
            linked.names = kept_names;
            linked.names.extend(gone_names);
            linked.names.sort_unstable();
            return Err(LinkCause::Linker { step, error });
        }
    };
    linked.functions = functions;
    linked.statics = statics;
    linked.natives = new_natives;
    linked.names = kept_names;
    // A test binary's tests are selected by its table, which only its link numbers.
    let table = match test {
        true => Some(super::test::image::table(
            path,
            built,
            &side.spans,
            linked.image.slots(),
            |key| linked.image.slot(key).map(|it| it.0),
            ContentId::default(),
            &generated.roots.tests,
        )?),
        false => None,
    };
    Ok((bytes, read, table, full))
}

/// A `rustc-link-lib` as rustc takes `-l`: `[KIND[:MODIFIERS]=]NAME[:RENAME]`.
pub(crate) fn link_lib(value: &str) -> Result<NativeLib, String> {
    let (kind, name) = match value.split_once('=') {
        Some((kind, name)) => (Some(kind), name),
        None => (None, value),
    };
    let (kind, modifiers) = match kind.map(|it| it.split_once(':').unwrap_or((it, ""))) {
        Some((kind, modifiers)) => (kind, modifiers),
        None => ("dylib", ""),
    };
    let mut whole = false;
    let mut bundle = true;
    let mut verbatim = false;
    for modifier in modifiers.split(',').filter(|it| !it.is_empty()) {
        let (on, name) = match modifier.split_at(1) {
            ("+", name) => (true, name),
            ("-", name) => (false, name),
            _ => return Err(format!("`-l {value}`: a link modifier starts with `+` or `-`")),
        };
        match name {
            "whole-archive" => whole = on,
            "bundle" => bundle = on,
            "verbatim" => verbatim = on,
            "as-needed" => {}
            other => return Err(format!("`-l {value}`: unknown link modifier `{other}`")),
        }
    }
    let kind = match kind {
        "dylib" => NativeLibKind::Dylib,
        "static" => NativeLibKind::Static { whole, bundle },
        "framework" => NativeLibKind::Framework,
        "link-arg" => return Err(format!("`-l {value}`: blaze's linker takes no raw link argument")),
        other => return Err(format!("`-l {value}`: unknown library kind `{other}`")),
    };
    // `NAME:RENAME` links `RENAME` where the crate's `#[link]` says `NAME`.
    let name = name.split_once(':').map_or(name, |it| it.1);
    if name.is_empty() {
        return Err(format!("`-l {value}` names no library"));
    }
    Ok(NativeLib { name: name.to_owned(), kind, verbatim })
}

/// What blaze's linker does with a `rustc-link-arg`: `-lNAME` and `-pthread` (gcc's
/// `-lpthread`) are libraries; any other is an error naming it.
pub(crate) fn link_arg(arg: &str) -> Result<NativeLib, String> {
    if arg == "-pthread" {
        return Ok(NativeLib { name: "pthread".to_owned(), kind: NativeLibKind::Dylib, verbatim: false });
    }
    match arg.strip_prefix("-l") {
        Some(name) if !name.is_empty() => {
            Ok(NativeLib { name: name.to_owned(), kind: NativeLibKind::Dylib, verbatim: false })
        }
        _ => Err(format!("blaze's linker takes no link argument `{arg}`")),
    }
}

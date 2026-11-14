//! A unit compiled by blaze's code generator and linked by blaze's linker: its policy (the
//! unit's profile, and each package's where cargo's profile gives it another), its roots (a
//! program's `main`, a test harness's, a library's functions), the build through
//! `blaze_codegen::driver::Build`, and the link into an image kept between jobs, so the next
//! link of the same file relinks only what changed.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use blaze_api::{ApiError, Goal, Tiers};
use blaze_build_script::{
    apply::{Compile, Linked as LinkedTo},
    unit as bs,
};
use blaze_codegen::{
    Arch, Backend, InstanceKey, NativeLib, NativeLibKind, Target,
    driver::{Build, Built, ProfileCounts},
    plan::{Mode as BuildMode, OptLevel, Origin, Override, Package, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Sanitize, Sanitizers, Toolchain},
};
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_link::{Change, Image, Native, NativeKind, Output, target::CrateLibs};
use blaze_units::schema::{self, Mode, Unit};
use hir_ty_codegen::v2::{
    Assumed, Backends, BodyChecks, BuildTime, Databases, GlobalAsmObject, HashSource, HirFrontend,
    HirInstance, ItemHashes, LinkInputs, RootKind, Roots, Runs,
};

use super::{Job, Real, UnitReader, compile::Written, files::Mounted, graph::flag, script};
use crate::serve::work::{Full, Linking, Made, OutputFields};

type KeptImage = crate::serve::work::Image<Real>;

mod kept;
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

/// Whether the build is asked for `unit` itself (a root of its unit graph: a plain `build` or
/// `--lib` of its package), not only built for what depends on it.
pub(crate) fn asked(job: &Job, unit: usize) -> bool {
    job.units().graph.roots.contains(&unit)
}

/// What `unit` makes, `asked` saying whether the build is asked for it ([`asked`]); an error
/// for what blaze does not make.
pub(crate) fn makes(unit: &Unit, asked: bool) -> Result<Makes, String> {
    let target = &unit.target;
    let has = |kind: &str| {
        target.crate_types.iter().any(|it| it == kind) || target.kind.iter().any(|it| it == kind)
    };
    // A library's doctests make programs: each example's and the runner's.
    if matches!(unit.mode, Mode::Test | Mode::Doctest) || target.is_custom_build() || has("bin") {
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
        // cargo writes every crate type a library lists, a dependency's too, but a dependency's
        // `staticlib` or `dylib` is a file no step of the build reads: what depends on the
        // library links its `rlib`, and an archive or dylib in `deps/` is no output a program
        // links (only an artifact dependency would name one). So a library a
        // build only depends on, which is also an `rlib` (`lib`), is built as that rlib and
        // nothing else; the programs that link it behave as under cargo (tree-sitter-highlight's
        // `crate-type = ["lib", "staticlib"]`). Where the build is asked for the library itself,
        // or it has no rlib to link, the file is what is asked for, and this linker does not
        // write it: an error naming it, never a build that quietly leaves it out.
        let rlib = has("lib") || has("rlib");
        if asked || !rlib {
            let why = match asked {
                true => "the build is asked for this library",
                false => "it has no `lib` or `rlib` its dependents could link instead",
            };
            return Err(format!(
                "`{}` asks for a `{kind}`, which blaze's linker does not write ({why}): crate \
                 types `lib`, `rlib`, `cdylib`, `proc-macro` and `bin` are what it builds",
                target.name
            ));
        }
    }
    Ok(Makes::Library)
}

/// Whether `unit` is a proc macro built for wasm32-wasip1 to run as a wasm module
/// ([`blaze_lower::wasm_macro`]): it builds code, linked as a WASI reactor whose exports are the
/// bridge's glue ([`super::harness`]).
pub(crate) fn wasm_macro(job: &Job, unit: usize) -> bool {
    match (job.unit(unit), job.plan(unit)) {
        (Ok(u), Ok(plan)) => blaze_lower::wasm_macro(u, plan),
        _ => false,
    }
}

/// The roots codegen compiles for `unit`. A proc macro built for wasm (`wasm`) is a program
/// whose roots are its glue's exports, kept by name as a `cdylib`'s are; a proc macro built for
/// the host is no build's (every build plans wasm macros, and [`super::compile`]'s `macros`
/// refuses one), so asking for its roots is an error naming it.
fn root_kind(unit: &Unit, makes: &Makes, wasm: bool) -> Result<RootKind, String> {
    Ok(match makes {
        // A doctest's programs as tests: a `test_harness` example's crate has `cfg(test)` and
        // its harness, as rustdoc's `--test` builds it; any other program's roots are its
        // `main`, as a test crate's without `cfg(test)` are.
        Makes::Executable if matches!(unit.mode, Mode::Test | Mode::Doctest) => RootKind::Test,
        Makes::Executable => RootKind::Executable,
        Makes::Shared { proc_macro: true } if wasm => RootKind::Shared,
        Makes::Shared { proc_macro: true } => {
            return Err(format!(
                "proc macro `{}` is built for the host, not as a wasm module, yet its codegen was asked for: \
                 a build runs proc macros only as wasm32-wasip1 modules",
                unit.target.name
            ));
        }
        Makes::Shared { proc_macro: false } => RootKind::Shared,
        Makes::Library => RootKind::Library,
    })
}

/// The triple `unit` is built for.
pub(crate) fn triple<'a>(job: &'a Job, unit: &'a Unit) -> &'a str {
    unit.platform.as_deref().unwrap_or(job.input.config.host.as_str())
}

/// The toolchain `unit` is compiled with: its target and its target data, the `-C target-cpu`,
/// `-C target-feature` of its rustflags, where it runs, the backends of this blaze.
pub(crate) fn toolchain(job: &Job, unit: usize) -> Result<Toolchain, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let plan = job.plan(unit).map_err(|it| it.what)?;
    let triple = triple(job, u);
    // The data its crates are laid out by, which the toolchain's id is of.
    blaze_lower::target_data(triple)
        .map_err(|it| it.to_string())
        .and_then(|target_data| {
            hir_ty_codegen::v2::toolchain(triple, &target_data, &plan.rustflags, runs(u))
        })
        // alloc: an error
        .map_err(|it| format!("unit {unit} (`{}` of {}): {it}", u.target.name, u.pkg_id))
}

/// A per-crate layout override must not leave the toolchain identity keyed by the target's
/// default layout. Refuse it before selecting a backend or reading stored code (#1621 M2).
fn verify_target_data(target: &str, workspace: &base_db::CrateWorkspaceData) -> Result<(), String> {
    let actual = workspace.target.as_ref().map_err(|error| error.to_string())?;
    let keyed = blaze_lower::target_data(target).map_err(|error| error.to_string())?;
    if actual != &keyed {
        return Err(format!(
            "target `{target}` has front-end data {actual:?}, but its codegen toolchain is keyed \
             by {keyed:?}: the toolchain key must cover the crate's target data"
        )); // alloc: an error naming both layouts
    }
    Ok(())
}

/// Where `unit`'s code runs: [`blaze_units::schema::runs_here`], the one rule, which the
/// standard library's key reads too, so a std compiled for this machine's CPU and one for the
/// target's baseline are two stds. Elsewhere is the target's baseline, as rustc's code.
fn runs(unit: &Unit) -> Runs {
    if unit.runs_here() { Runs::Here } else { Runs::Elsewhere }
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
        schema::DebugInfo::Level(other) => {
            return Err(format!("debuginfo `{other}` is none rustc takes"));
        }
    })
}

fn panic(panic: &str) -> Result<Panic, String> {
    match panic {
        "unwind" => Ok(Panic::Unwind),
        "abort" => Ok(Panic::Abort),
        other => Err(format!("panic strategy `{other}` is none rustc takes")),
    }
}

/// Whether unit `unit` compiles code that runs at build time ([`BuildTime`]): a build script, a
/// proc macro or a unit one of them depends on ([`Graph`](super::graph::Graph)'s bitset). Of the
/// unit, never of its crate: blaze-lower makes one crate of a host unit and a program unit that
/// lower alike, and the program's build of it is the program's.
pub(crate) fn build_time(job: &Job, unit: usize) -> BuildTime {
    BuildTime(job.graph.build_time(unit as u32))
}

/// Whose code unit `at`'s roots are, for their settings.
pub(crate) fn origin(job: &Job, at: usize, unit: &Unit) -> Result<Origin, String> {
    let package = job.package(unit).map_err(|it| it.what)?;
    Ok(match () {
        _ if unit.is_std => Origin::Std,
        _ if build_time(job, at).0 => Origin::BuildTime(Package::new(&package.name)),
        _ if package.is_member => Origin::Member(Package::new(&package.name)),
        _ => Origin::Dependency(Package::new(&package.name)),
    })
}

/// The build goal unit `unit`'s code is compiled for (docs/v2/plan/design/tiers.md): the
/// request's `--goal` when it gave one, else its profile's own
/// ([`Goal::of_profile`]: `dev` edits, `release` is `max`). Build-time code (a build script, a
/// proc macro, what they depend on) runs once during the build and is no part of the program
/// the request waits for: its goal is its own profile's whatever the request says.
///
/// `--goal max` of a profile that asks for `codegen-backend = "stencil"` is refused, naming
/// both: that profile's every function is tier 0 code, and `max` is never answered with it.
pub(crate) fn goal(job: &Job, unit: usize) -> Result<Goal, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let profile = &u.profile;
    let backend = profile.codegen_backend.as_deref();
    let asked = match build_time(job, unit).0 {
        true => None,
        false => job.input.goal,
    };
    match asked {
        Some(Goal::Max) if backend == Some("stencil") => Err(format!(
            "`--goal max` waits for the optimized code of `{}`, and its profile `{}` asks for \
             codegen-backend `stencil`, which compiles every function as tier 0 code: `max` is \
             never answered with tier 0 code (docs/v2/plan/design/tiers.md); drop `--goal max`, \
             or the profile's `codegen-backend`",
            u.target.name, profile.name
        )),
        Some(asked) => Ok(asked),
        None => Ok(Goal::of_profile(&profile.name, &profile.opt_level, backend)),
    }
}

/// What the image of unit `unit`, built as `built` for `arch`, says of its goal: the goal and
/// how many of its functions are the optimized tier's (LLVM at `-O2` or `-O3`). None for
/// build-time code and for wasm, whose one producer no goal picks.
fn tiers(job: &Job, unit: usize, built: &Built, arch: Arch) -> Result<Option<Tiers>, String> {
    if build_time(job, unit).0 || arch.is_wasm() {
        return Ok(None);
    }
    let optimized =
        built.made.iter().filter(|it| matches!(it.1.backend, Backend::Llvm { opt: 2.. })).count();
    Ok(Some(Tiers::of(goal(job, unit)?).with(optimized as u64, built.made.len() as u64)))
}

/// What blaze makes of cargo's `codegen-backend` for `package`'s code at `level`, `true` when
/// it asks for stencils at every level. `llvm`, rustc's default, leaves the machine code
/// blaze's own choice, and so does `cranelift` (rustc's fast unoptimized back end) where the
/// level compiles with stencils. `stencil`, blaze's own fast native code, makes every
/// function of the build copy-and-patch stencils whatever its level, std's too
/// ([`Profile::stencils`]): what the seed builds with. Any other is an error naming it, never
/// a key ignored.
pub(crate) fn codegen_backend(
    profile: &schema::Profile,
    package: &str,
    level: OptLevel,
) -> Result<bool, String> {
    let asked = match profile.codegen_backend.as_deref() {
        None | Some("llvm") => return Ok(false),
        Some(asked) => asked,
    };
    let name = &profile.name;
    match asked {
        "stencil" => Ok(true),
        "cranelift" if level.backend(false) == Some(Backend::Stencil) => Ok(false),
        "cranelift" => Err(format!(
            "the profile `{name}` of `{package}` asks for codegen-backend `cranelift` at opt-level \
             {}, which blaze compiles with LLVM: native code is never Cranelift's (AGENTS.md rule \
             8); `stencil` compiles every level as copy-and-patch stencils",
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
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) struct Profiled {
    id: ContentId,
    unstored: blaze_core::lock::Lock<Option<Vec<u8>>>,
    /// How its records are named, sorted once as it is read ([`blaze_lints::profile::names`]),
    /// or why they don't read: what tells a profile blaze made, which applying to nothing
    /// refuses, from one rustc or clang made, which only warns ([`judge`]).
    names: Result<blaze_lints::profile::Names, String>,
    #[cfg(feature = "native")]
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
    let span = tracing::info_span!(target: "blaze", "profile names", path, records = tracing::field::Empty).entered();
    let at = std::path::Path::new(root).join(path);
    let bytes = std::fs::read(&at).map_err(|error| {
        // alloc: an error
        format!(
            "the flags for rustc have `-C profile-use={path}`, and `{}` can't be read: {error}",
            at.display()
        )
    })?;
    let names = blaze_lints::profile::names(&bytes);
    if let Ok(names) = &names {
        span.record("records", names.records);
    }
    profiled(path, bytes, names)
}

#[cfg(feature = "native")]
fn profiled(
    path: &str,
    bytes: Vec<u8>,
    names: Result<blaze_lints::profile::Names, String>,
) -> Result<Profiled, String> {
    let id = hir_ty_codegen::v2::profile_id(&bytes);
    let given = hir_ty_codegen::v2::give_profile(id, &bytes)
        // alloc: an error
        .map_err(|what| format!("the profile of `-C profile-use={path}`: {what}"))?;
    let unstored = blaze_core::lock::Lock::new("serve.codegen.profile.unstored", Some(bytes));
    Ok(Profiled { id, unstored, names, _given: given })
}

#[cfg(not(feature = "native"))]
fn profiled(
    path: &str,
    _bytes: Vec<u8>,
    _names: Result<blaze_lints::profile::Names, String>,
) -> Result<Profiled, String> {
    // alloc: an error
    Err(format!(
        "the flags for rustc have `-C profile-use={path}`, and this blaze is built without LLVM \
         (blaze-cargo's feature `native`), the one back end that builds with a profile"
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
        None => Err(format!(
            "`-C profile-use={path}` is in no unit's flags the job read its profiles from"
        )),
    }
}

/// The [`Pgo`] `unit` is built with, as its [`policy`] has it: what its record key holds of a
/// profile to use, whose path in its flags says nothing of the bytes there.
pub(crate) fn unit_pgo(job: &Job, unit: usize) -> Result<Pgo, String> {
    pgo_of(job, pgo(&job.plan(unit).map_err(|it| it.what)?.rustflags)?)
}

/// What `unit`'s codegen did with each profile it used, sorted by profile ID, as this job's
/// build of it counted ([`Job::counted`]): none when it built no code this job.
pub(crate) fn counted(job: &Job, unit: usize) -> &[(ContentId, ProfileCounts)] {
    job.counted.get(unit).and_then(|it| it.get()).map_or(&[], |it| &it[..])
}

/// The check that each profile the job's units name applied to something, summed over the
/// whole job ([`judge`]), once its build ended: `ok` whether it succeeded, `built` the units
/// it built (a build ahead builds some). Its warnings, one a profile, for the job to say.
pub(crate) fn unapplied(job: &Job, ok: bool, built: &[u32]) -> Result<Vec<String>, ApiError> {
    let _span = tracing::info_span!(target: "blaze", "profile applied check").entered();
    if !ok {
        return Ok(Vec::new());
    }
    let units = job.units();
    // alloc: once per job, a flag a unit
    let mut was_built = vec![false; units.plans.len()];
    for at in built {
        if let Some(it) = was_built.get_mut(*at as usize) {
            *it = true;
        }
    }
    // alloc: once per job, a unit that names a profile
    let accounted: Vec<Accounted<'_>> = units
        .plans
        .iter()
        .enumerate()
        .filter(|(unit, plan)| {
            was_built[*unit]
                && !job.graph.has(*unit as u32, flag::RUN)
                && matches!(pgo(&plan.rustflags), Ok(PgoFlag::Use(_)))
        })
        .map(|(unit, _)| Accounted {
            unit,
            counted: job.counted.get(unit).and_then(|it| it.get()).map(|it| &it[..]),
        })
        .collect();
    if accounted.is_empty() {
        return Ok(Vec::new());
    }
    // alloc: once per job, a profile read
    let named: Vec<Named<'_>> = profiles(job)
        .0
        .iter()
        .filter_map(|(path, read)| {
            let read = read.as_ref().ok()?;
            Some(Named {
                id: read.id,
                path,
                names: read.names.as_ref().copied().map_err(String::as_str),
            })
        })
        .collect();
    judge(ok, accounted, |unit| super::compile::recorded_profiles(job, unit), &named)
}

/// A unit of a job that names a profile, as [`judge`] is given it.
pub(crate) struct Accounted<'a> {
    pub(crate) unit: usize,
    /// What its codegen did with each profile, as this job counted it ([`Job::counted`]): set
    /// when it built, or when its record was replayed; none when neither was in this job.
    pub(crate) counted: Option<&'a [(ContentId, ProfileCounts)]>,
}

/// A unit's record, read when nothing counted it this job.
pub(crate) enum Recorded {
    Read(Vec<(ContentId, ProfileCounts)>),
    /// There is none: the unit and its record key named, for the error.
    Missing(String),
}

/// A profile the job read, as [`judge`] names it.
pub(crate) struct Named<'a> {
    pub(crate) id: ContentId,
    /// As the flags have it, `-C profile-use=<path>`.
    pub(crate) path: &'a str,
    /// How its records are named ([`blaze_lints::profile::names`]), or why they don't read.
    pub(crate) names: Result<blaze_lints::profile::Names, &'a str>,
}

/// Whether each profile the units of a job that ended (`ok` whether it succeeded) used applied
/// to something, summed over every unit: each unit's counts are this job's ([`Accounted`]),
/// else its record's (`recorded`), else, as nothing can say what its profile did, an error
/// naming it. Summed, not per unit, as a target the profiled program never ran (another
/// binary, an example, a test) applies none of a profile's records and builds under rustc.
///
/// A profile used that applied to nothing refuses the job only when blaze made it (blaze named
/// every record, [`blaze_lints::profile::Names::blaze_made`]) and no function found its record
/// edited: blaze's own names, made under other item keys, are blaze's failure (rule 3). Any
/// other (rustc or clang made it, it was edited, it mixes names, its names don't read) is a
/// warning naming its path, its counts and the cause, and the build goes on as under rustc
/// (rule 4). A job that failed is not judged: a failed unit's counts say nothing.
pub(crate) fn judge<'a>(
    ok: bool,
    units: impl IntoIterator<Item = Accounted<'a>>,
    mut recorded: impl FnMut(usize) -> Result<Recorded, ApiError>,
    profiles: &[Named<'_>],
) -> Result<Vec<String>, ApiError> {
    if !ok {
        return Ok(Vec::new());
    }
    // alloc: none when no unit used a profile; one entry per profile used
    let mut totals: BTreeMap<ContentId, ProfileCounts> = BTreeMap::new();
    for unit in units {
        let read;
        let counts = match unit.counted {
            Some(counts) => counts,
            None => match recorded(unit.unit)? {
                Recorded::Read(it) => {
                    read = it;
                    &read[..]
                }
                Recorded::Missing(what) => {
                    // alloc: an error
                    let what = format!(
                        "{what}: the job built it with a profile and succeeded, and neither its build this job nor \
                         a record says what the profile did to it, which the check that a profile applied to \
                         something reads"
                    );
                    return Err(ApiError::new(what, None));
                }
            },
        };
        for (id, counts) in counts {
            let at = totals.entry(*id).or_default();
            *at = at.plus(*counts);
        }
    }
    // alloc: none when every profile applied somewhere
    let (mut refused, mut warnings) = (Vec::new(), Vec::new());
    for (id, counts) in totals {
        if counts.used == 0 || counts.applied > 0 {
            continue;
        }
        let Some(named) = profiles.iter().find(|it| it.id == id) else {
            // alloc: an error
            let what = format!(
                "the job's units used the profile of content {id:x}, which is none the job read"
            );
            return Err(ApiError::new(what, None));
        };
        match applied_nothing(named, counts) {
            Err(line) => refused.push(line),
            Ok(line) => warnings.push(line),
        }
    }
    if !refused.is_empty() {
        return Err(ApiError::new(refused.join("\n"), None));
    }
    Ok(warnings)
}

/// What is said of profile `named`, used by `counts.used` functions and applied to none: the
/// refusal (`Err`) when blaze made it under other keys, else a warning (`Ok`).
fn applied_nothing(named: &Named<'_>, counts: ProfileCounts) -> Result<String, String> {
    let Named { id, path, names } = named;
    let ProfileCounts { used, mismatched, .. } = counts;
    // alloc: once per profile that applied to nothing
    let head = format!(
        "the profile `-C profile-use={path}` (content {id:x}) gave counts to none of the {used} functions the build \
         compiled with it, summed over every target ({mismatched} found a record of their name with another hash)"
    );
    let names = match names {
        Ok(names) => *names,
        // alloc: as above
        Err(why) => {
            return Ok(format!(
                "{head}. Its records' names don't read ({why}), so who made it is not known; the build goes on without its counts"
            ));
        }
    };
    let blaze_lints::profile::Names { records, blaze, rustc, foreign } = names;
    if mismatched > 0 {
        // alloc: as above
        return Ok(format!(
            "{head}: a record under a name this build has, with another hash, means the program was edited since it \
             was profiled. Profile it again with blaze's `-Cprofile-generate`; the build goes on without its counts"
        ));
    }
    if names.blaze_made() {
        // alloc: an error, once per profile refused
        return Err(format!(
            "{head}: blaze named each of its {records} records (`blaze.pgo.<hash>`), so blaze made it, and none names a \
             function of this build: it was made by a blaze whose crate or item key version differs, or built for \
             another target, target features or cfgs than this build, so its names are of other keys (or of a \
             program that shares no function with this one). Make the profile again with this blaze's \
             `-Cprofile-generate` and this build's flags, or build without it"
        ));
    }
    if records == 0 {
        // alloc: as above
        return Ok(format!(
            "{head}: it has no records, so the profiled run wrote nothing (it ran no instrumented code, or the \
             profile was merged from no raw profiles). The build goes on without its counts"
        ));
    }
    if blaze == 0 {
        // alloc: as above
        return Ok(format!(
            "{head}: none of its {records} records is named by blaze ({rustc} are symbols rustc mangled, {foreign} \
             other names), so rustc or clang made it: blaze can't apply profiles rustc made; make one with blaze's \
             -Cprofile-generate. The build goes on without its counts"
        ));
    }
    // alloc: as above
    Ok(format!(
        "{head}: of its {records} records blaze named {blaze}, rustc {rustc} and neither {foreign}, so it was merged \
         with another tool's profile or edited. The build goes on without its counts"
    ))
}

/// What a build with `pgo` puts in the store beside its code: the profile's bytes under its
/// content ID, so a build reads them by `Key(id)` without the path, put by the first build of
/// the job that finds them missing. A profile the job did not give is an error naming
/// `give_profile`.
fn profile_put(job: &Job, pgo: Pgo) -> Result<Option<blaze_codegen::driver::Put>, String> {
    let Pgo::Use(id) = pgo else { return Ok(None) };
    let read = profiles(job)
        .0
        .iter()
        .find_map(|it| it.1.as_ref().ok().filter(|read| read.id == id))
        .ok_or_else(|| {
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
    let read = profiles(job)
        .0
        .iter()
        .find_map(|it| it.1.as_ref().ok().filter(|read| blaze_cas::Key(read.id) == put.key));
    if let Some(read) = read {
        *read.unstored.lock() = Some(put.bytes);
    }
}

/// Whether `rustflags` turn lint `blaze::vecreport` on (`-W`, `-D`, `-F`, `--force-warn`,
/// itself or its group `blaze::pedantic_perf`; the last flag naming it wins, `-A` turns it
/// off): its release build is then built for the report, which [`compile`'s link
/// prints](super::compile). A `#![warn(blaze::vecreport)]` in the source is not read here:
/// the report changes the build's keys, which are known before its source is read.
pub(crate) fn vecreport(rustflags: &[String]) -> bool {
    const NAMES: [&str; 2] = ["blaze::vecreport", "blaze::pedantic_perf"];
    let mut on = false;
    let mut flags = rustflags.iter().map(String::as_str);
    while let Some(flag) = flags.next() {
        let (level, joined) = match flag {
            "-W" | "--warn" | "-D" | "--deny" | "-F" | "--forbid" | "--force-warn" => (true, None),
            "-A" | "--allow" => (false, None),
            _ => {
                let joined = [
                    ("-W", true),
                    ("--warn=", true),
                    ("-D", true),
                    ("--deny=", true),
                    ("-F", true),
                ]
                .into_iter()
                .chain([
                    ("--forbid=", true),
                    ("--force-warn=", true),
                    ("-A", false),
                    ("--allow=", false),
                ])
                .find_map(|(prefix, level)| flag.strip_prefix(prefix).map(|name| (level, name)));
                match joined {
                    Some((level, name)) => (level, Some(name)),
                    None => continue,
                }
            }
        };
        let Some(name) = joined.or_else(|| flags.next()) else { break };
        if NAMES.contains(&name) {
            on = level;
        }
    }
    on
}

/// How `unit`'s build compiles every function: its own profile, and for each package linked
/// into it ([`Graph::linked_in`](super::graph::Graph::linked_in)) whose unit cargo gives
/// another profile (`[profile.*.package.*]`), that one; the sanitizers of its rustflags, the
/// same for every function, std's too, as cargo passes `RUSTFLAGS` to every crate. Only what
/// is linked in: a package that is also a build script's or proc macro's dependency (a
/// release build's host package under `build-override`) shares its crate with the linked unit
/// (`blaze_lower`'s run classes) and is compiled under that unit's own policy, so its other
/// profile must not name this unit's functions of the package.
pub(crate) fn policy(job: &Job, unit: usize, toolchain: Toolchain) -> Result<Policy, String> {
    let root = job.unit(unit).map_err(|it| it.what)?;
    let plan = job.plan(unit).map_err(|it| it.what)?;
    let rustflags = &plan.rustflags;
    let sanitize = sanitize(rustflags, triple(job, root))?;
    let pgo = pgo_of(job, pgo(rustflags)?)?;
    let base = &root.profile;
    let opt = opt_level(&base.opt_level)?;
    // The goal picks the tier of the profile's own code (docs/v2/plan/design/tiers.md): tier 0
    // under `edit`, the optimized tier under `max`. A profile already at its goal's tier (dev
    // under `edit`, release under `max`) is compiled at its own level, so its policy, and
    // every key and byte made from it, is what it was before the goal. A package cargo gives
    // another level (`[profile.*.package.*]`, below) keeps that level under both goals.
    let goal = goal(job, unit)?;
    let at = match goal {
        Goal::Edit => opt.tier_zero(),
        Goal::Max => opt.optimized(),
    };
    let stencils = codegen_backend(base, &job.package(root).map_err(|it| it.what)?.name, at)
        .map_err(|why| match at == opt {
            true => why,
            false => {
                format!("{why} (the goal `{goal}` compiles the profile's own code at {at:?})")
            }
        })?;
    let info = debuginfo(&base.debuginfo)?;
    // BTreeMap: it is `Profile::packages`, which is serialized and hashed.
    let mut packages = BTreeMap::new();
    for dep in job.graph.linked_in(unit as u32).into_iter().skip(1) {
        if job.graph.has(dep, flag::STD) {
            continue;
        }
        let dep = job.unit(dep as usize).map_err(|it| it.what)?;
        let name = job.package(dep).map_err(|it| it.what)?.name.clone();
        let profile = &dep.profile;
        let level = opt_level(&profile.opt_level)?;
        // `stencil` is the whole build's, from its profile: a package that leaves it, or asks
        // for it where the rest is at LLVM's levels, would be a key ignored.
        if codegen_backend(profile, &name, level)? != stencils
            && (stencils || level.backend(false) != Some(Backend::Stencil))
        {
            return Err(format!(
                "the profile `{}` of `{name}` asks for codegen-backend `{}`, and the build's for \
                 `{}`: blaze takes `stencil` for a whole build, from the profile, never per package",
                profile.name,
                profile.codegen_backend.as_deref().unwrap_or("llvm"),
                base.codegen_backend.as_deref().unwrap_or("llvm"),
            ));
        }
        let dep_info = debuginfo(&profile.debuginfo)?;
        let differs = Override {
            opt_level: (level != opt).then_some(level),
            overflow_checks: (profile.overflow_checks != base.overflow_checks)
                .then_some(profile.overflow_checks),
            debug_assertions: (profile.debug_assertions != base.debug_assertions)
                .then_some(profile.debug_assertions),
            debuginfo: (dep_info != info).then_some(dep_info),
        };
        if differs != Override::default() {
            packages.insert(name, differs);
        }
    }
    let mut policy = Policy {
        toolchain,
        profile: Profile {
            opt_level: at,
            overflow_checks: base.overflow_checks,
            debug_assertions: base.debug_assertions,
            debuginfo: info,
            // A wasm macro's module aborts on a panic, after its glue's hook gave the host the
            // message (`guest/entry.rs`), as its std was planned (`blaze_units`' `std_key`):
            // wasm32 code unwinds nowhere, and the host fails that expansion alone.
            panic: if plan.macro_wasm { Panic::Abort } else { panic(&base.panic)? },
            packages,
            build_override: Override::default(),
            stencils,
        },
        mode: BuildMode::Output,
        fold: true,
        sanitize,
        pgo,
        // A wasm macro's std reaches the host for what WASI would give (`RandomState`'s keys).
        proc_macro_bridge: plan.macro_wasm,
        report: vecreport(rustflags),
        fuel: false,
        // What the program's allocator allows is read from its sources, which [`generate`]
        // has: there it is set for the unit's program.
        heap_to_stack: false,
    };
    // A dev executable runs on the daemon's code memory and takes edits while it runs: its
    // members' functions are reached through slots ([`super::run::inproc::runs_in_process`]).
    // A launcher's program is not linked at all: every function is reached through its
    // slot word, in the launcher's process ([`super::run::launch::launched`]).
    if super::run::launch::launched(job, unit, &policy)? {
        policy.mode = BuildMode::Launched;
    } else if super::run::inproc::runs_in_process(job, unit, &policy)? {
        policy.mode = BuildMode::Live;
    }
    Ok(policy)
}

/// What a unit's codegen made ([`generate`]), kept for its link: nothing of the front end
/// that made it, which the link reads through its own task's ([`Parts::over`]).
pub(crate) struct Parts {
    pub(crate) built: Built,
    pub(crate) roots: Roots,
    pub(crate) policy: Policy,
    /// Whether its link maps the dependency image ([`links_image`]): what both front ends
    /// are made with and what the link makes a dev executable by.
    pub(crate) image: bool,
    /// Whether its unit's code runs at build time: what the link's front end is made with.
    pub(crate) build_time: BuildTime,
    /// What the walks over the front end keep, which made `built`: where `built` goes back
    /// once the link is done with it ([`Drop`]).
    kept: Arc<blaze_codegen::driver::Kept<HirInstance, hir_ty_codegen::v2::HirStatic>>,
    /// A launched build's start, made for its link ([`super::run::launch::prepare`]): taken
    /// by the link, once. `None` for every other build, whose code is in `built`.
    launch: Option<blaze_core::lock::Lock<Option<super::run::launch::Prepared>>>,
}

/// The `Built` goes back to the driver that made it, whatever became of its link
/// (`blaze_codegen::driver::Kept::give_back`): the next build of the program writes the
/// rows an edit changed into its lists, where making them again is a copy a row of the
/// program (16.7 ms of a one-function edit of hello world), and freeing them here was
/// 10 ms more of the link's task. Nothing is copied to give it back.
impl Drop for Parts {
    fn drop(&mut self) {
        self.kept.give_back(std::mem::take(&mut self.built));
    }
}

impl Parts {
    /// What codegen made, read through `reader`'s front end. The unit's crate is the same in
    /// it as in the one codegen read: every input of the crate is of a unit it depends on,
    /// done before its codegen began, so a write applied since changed none of it.
    pub(crate) fn over<'a>(
        &'a self,
        reader: &'a UnitReader,
        mounted: &Arc<Mounted>,
    ) -> Generated<'a> {
        let frontend = frontend(reader, mounted, self.build_time)
            .with_policy(&self.policy)
            .with_image(self.image);
        Generated {
            built: &self.built,
            roots: &self.roots,
            frontend,
            policy: &self.policy,
            image: self.image,
            launch: self.launch.as_ref(),
            side: Default::default(),
            spans: Default::default(),
        }
    }
}

/// What a unit's codegen made, reading the task's front end.
pub(crate) struct Generated<'a> {
    pub(crate) built: &'a Built,
    pub(crate) roots: &'a Roots,
    pub(crate) frontend: HirFrontend<'a>,
    pub(crate) policy: &'a Policy,
    /// Whether the link maps the dependency image ([`Parts::image`]).
    pub(crate) image: bool,
    /// A launched build's start, for its link to take ([`Parts::launch`]).
    pub(crate) launch: Option<&'a blaze_core::lock::Lock<Option<super::run::launch::Prepared>>>,
    /// The span tables the build's code names, as a change of those its last link was given
    /// ([`HirFrontend::link_inputs`]): read once, by the first link that needs them, and none
    /// for a library.
    #[expect(
        clippy::disallowed_types,
        reason = "read once, by the first link that needs it: per unit's codegen, not global"
    )]
    side: std::cell::OnceCell<LinkInputs>,
    /// Every span table the build's code names, made whole ([`HirFrontend::span_tables`]):
    /// read once, by a test's selection, a test binary's table, or a link that holds no
    /// earlier tables to change.
    #[expect(
        clippy::disallowed_types,
        reason = "read once, by the first test selection or link that needs it: per unit's codegen, not global"
    )]
    spans: std::cell::OnceCell<Vec<(ContentId, Vec<u8>)>>,
}

impl Generated<'_> {
    /// What lint `blaze::vecreport` says of the workspace's code the build compiled (each
    /// function whose body is written in a workspace crate, [`Built::editable`]), read from
    /// each function's side table: this build's from its puts, the rest from `store`. Empty
    /// for a build not made for it ([`Policy::report`]). A side table the store lacks, or one
    /// that does not read, is an error naming the function.
    pub(crate) fn report(
        &self,
        store: &crate::serve::direct::Stores,
    ) -> Result<Vec<hir_ty_codegen::v2::report::Reported>, String> {
        if !self.policy.report {
            return Ok(Vec::new());
        }
        // Each side table the store holds is one direct read into the task's frame, read for
        // its rows in the scope.
        blaze_mem::frame::current(|frame| self.report_in(store, frame))
    }

    fn report_in(
        &self,
        store: &crate::serve::direct::Stores,
        frame: &blaze_mem::frame::Frame,
    ) -> Result<Vec<hir_ty_codegen::v2::report::Reported>, String> {
        use crate::serve::direct::{self, Which};
        let workspace = &self.built.editable;
        // alloc: once per unit reported, a function each
        let mut reports = Vec::new();
        for function in
            self.built.functions.iter().filter(|it| workspace.binary_search(&it.key).is_ok())
        {
            let Some(id) = function.side else { continue };
            let key = blaze_codegen::query::keys::side(&id);
            let named = || format!("{:?}", function.key); // alloc: an error's name
            let bytes = match self.built.puts.iter().find(|it| it.key == key) {
                Some(put) => &put.bytes[..],
                None => match direct::read(store, Which::Home, &key, frame) {
                    Ok(Some(bytes)) => bytes,
                    // alloc: an error
                    Ok(None) => {
                        return Err(format!("the side table of {} is not in the store", named()));
                    }
                    // alloc: an error
                    Err(corrupt) => {
                        return Err(format!("the side table of {}: {corrupt}", named()));
                    }
                },
            };
            // alloc: an error
            let rows = hir_ty_codegen::v2::report::of_side(bytes)
                .map_err(|why| format!("the report of {}: {why}", named()))?;
            if !rows.is_empty() {
                reports.push((function.key, rows));
            }
        }
        self.frontend.vecreport(self.built, &reports).map_err(|it| it.what)
    }

    /// What the link takes beside the code ([`HirFrontend::link_inputs`]): the span tables
    /// an edit changed, for a link that holds the rest.
    pub(crate) fn side(&self) -> Result<&LinkInputs, String> {
        if let Some(side) = self.side.get() {
            return Ok(side);
        }
        let side = self.frontend.link_inputs(self.built).map_err(|it| it.what)?;
        Ok(self.side.get_or_init(|| side))
    }

    /// Every span table the build's code names, by anchor, sorted
    /// ([`HirFrontend::span_tables`]): each made again, whatever a link before was given.
    pub(crate) fn spans(&self) -> Result<&[(ContentId, Vec<u8>)], String> {
        if let Some(spans) = self.spans.get() {
            return Ok(spans);
        }
        let _span = tracing::info_span!(target: "blaze", "link span tables whole").entered();
        let spans = self.frontend.span_tables(self.built).map_err(|it| it.what)?;
        Ok(self.spans.get_or_init(|| spans))
    }
}

/// The code generator's view of the job's front end, its files named as rustc names them. Every
/// thread of the build reads the one database of the task's reader by reference: the query
/// engine's reads take `&self`, so none waits on another and none is cloned. What its walks
/// keep for the next build of the database is the reader's ([`UnitReader::walks`]), held by
/// the kept front end and let go with it.
fn frontend<'db>(
    reader: &'db UnitReader,
    mounted: &Arc<Mounted>,
    build_time: BuildTime,
) -> HirFrontend<'db> {
    frontend_over(reader.db(), reader.paths(), reader.spellings(), mounted, build_time)
        .with_walks(reader.walks())
}

/// [`frontend`] over the database `db`, whose files are `paths` and whose crates' roots cargo
/// passes as `spellings` says: what the loop's writer reads before it lends the front end to a
/// task ([`super::harness::write`]). `build_time` is its unit's ([`build_time`]).
pub(crate) fn frontend_over<'db>(
    db: &'db base_db::Db,
    paths: &Arc<std::collections::HashMap<base_db::FileId, String>>,
    spellings: &Arc<blaze_lower::Spellings>,
    mounted: &Arc<Mounted>,
    build_time: BuildTime,
) -> HirFrontend<'db> {
    HirFrontend::new(Databases::new(db), hash_source(db, paths, spellings, mounted), build_time)
}

/// How [`frontend_over`] names the files of `db` in what it hashes: what a launched run's
/// span tables are read with too ([`super::run::launch`]).
pub(crate) fn hash_source(
    db: &base_db::Db,
    paths: &Arc<std::collections::HashMap<base_db::FileId, String>>,
    spellings: &Arc<blaze_lower::Spellings>,
    mounted: &Arc<Mounted>,
) -> HashSource {
    // A file of the view by the job's paths, as rustc names it: from the directory of the crate
    // root it is under as cargo passed the root, `..`s kept (`Spellings::name`); one with two
    // such names has none, and the code generator refuses what names it. One `include!` parses
    // from outside every source root by its path outside, which a body lowered during the build
    // may read first.
    let names = {
        let (mounted, paths, spellings, files) =
            (Arc::clone(mounted), Arc::clone(paths), Arc::clone(spellings), db.shared_files());
        move |file: base_db::FileId| -> Option<String> {
            match paths.get(&file) {
                Some(path) => match spellings.name(path) {
                    Ok(Some(spelled)) => Some(mounted.name(&spelled)),
                    Ok(None) => Some(mounted.name(path)),
                    Err(_) => None,
                },
                None => files.included_source_path(file).map(|path| mounted.name(path.as_str())),
            }
        }
    };
    // alloc: once a front end, its namer
    HashSource::Items(ItemHashes::new(Box::new(names)))
}

/// The tests of test unit `unit` as its harness lists them, compiling nothing: the same
/// listing, names and attributes as the test binary's ([`generate`]).
pub(crate) fn test_roots(job: &Job, reader: &UnitReader, unit: usize) -> Result<Roots, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let krate = job.crates.krate(unit).ok_or_else(|| {
        format!("unit {unit} (`{}`) has no crate in the front end", u.target.name)
    })?;
    frontend(reader, &job.mounted, build_time(job, unit))
        .roots(krate, RootKind::Test)
        .map_err(|it| it.what)
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

/// Whether a unit's link maps the dependency image: a dev executable, which runs on the
/// daemon's code memory and takes edits. [`policy`] makes a unit live only when it
/// [`runs_in_process`] (a binary's build or a test binary for the host that links the dev
/// runtime: no wasm macro or doctest's program, each of which runs as a process of its own), so
/// the mode and what the unit makes say it. The one place a build decides it: codegen's front end splits ownership by it
/// ([`HirFrontend::with_image`]) and the link makes the dev run by it ([`Parts::image`]).
///
/// [`runs_in_process`]: super::run::inproc::runs_in_process
fn links_image(makes: &Makes, policy: &Policy) -> bool {
    policy.mode == BuildMode::Live && *makes == Makes::Executable
}

/// `krate`'s functions, built as `unit`, compiled from its roots (the unit's own crate, or one
/// of the crates a `Doctest` unit expands into): `Err` with the code generator's errors, each
/// naming the function it could not compile. `checks` is the unit's check running beside it:
/// told the bodies the build reaches, it stops the build once it failed. `None` for a crate
/// whose check outcome is already known.
pub(crate) fn generate<'db>(
    job: &'db Job,
    reader: &'db UnitReader,
    unit: usize,
    krate: base_db::Crate,
    makes: &Makes,
    mounted: &Arc<Mounted>,
    // dyn: `HirFrontend::with_checks` takes it; called once a level of the walk
    checks: Option<&'db dyn BodyChecks>,
) -> Result<Result<Parts, Refused>, String> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let toolchain = toolchain(job, unit)?;
    verify_target_data(triple(job, u), krate.workspace_data(reader.db())).map_err(|error| {
        format!("unit {unit} (`{}` of {}), crate `{}`: {error}", u.target.name, u.pkg_id, krate.named(reader.db()))
    })?; // alloc: an error's place
    // The process's code generator compiles for one toolchain: the first unit sets it up, and
    // a unit of another is refused here, naming both.
    hir_ty_codegen::v2::select(&toolchain)?;
    // An allocation may move to the stack only where no allocation of the program is observed
    // (`MirOptions::heap_to_stack`): known of a unit that links a program, never of a library
    // whose program is linked later, which may bring its own `#[global_allocator]`.
    let heap_to_stack = match makes {
        Makes::Executable | Makes::Shared { .. } => {
            hir_ty_codegen::v2::allocations_unobserved(reader.db(), krate)
                // alloc: an error
                .map_err(|it| format!("unit {unit} (`{}` of {}): {it}", u.target.name, u.pkg_id))?
        }
        Makes::Library => false,
    };
    let policy = Policy { heap_to_stack, ..policy(job, unit, toolchain)? };
    // A cancelled job's build stops within a function: its caller posts it again.
    let build_time = build_time(job, unit);
    let image = links_image(makes, &policy);
    let frontend = frontend(reader, mounted, build_time)
        .with_policy(&policy)
        .with_image(image)
        .with_stop(&job.input.cancelled);
    let frontend = match checks {
        Some(checks) => frontend.with_checks(checks),
        None => frontend,
    };
    let roots = match frontend.roots(krate, root_kind(u, makes, wasm_macro(job, unit))?) {
        Ok(it) => it,
        Err(error) => return Ok(Err(refused(reader, mounted.root(), vec![error]))),
    };
    let origin = origin(job, unit, u)?;
    let asked: Vec<(HirInstance, Origin)> =
        roots.roots.iter().map(|it| (it.clone(), origin)).collect();
    // The job holds the profile given for as long as it lives, so for the whole build.
    let profile = profile_put(job, policy.pgo)?;
    // `BLAZE_VERIFY_STORE` in the request's environment: the store hits it samples are
    // compiled again and checked ([`Build::verify_store`]); a value that is none is the error.
    let verify = blaze_store_check::Setting::from_env(&job.input.config.invocation.env)
        .map_err(|it| it.to_string())?;
    // A blaze that keys as another toolchain (`BLAZE_DEV_TOOLCHAIN_AS`) may be served what
    // other code made: the check is on whatever the request asked, at the least rate.
    let verify = match crate::serve::dev_toolchain::dev() {
        Some(_) => verify.at_least(crate::serve::dev_toolchain::VERIFY_RATE),
        None => verify,
    };
    let backends = Backends::default();
    // The describes the walks before this one kept for this database and this front end's
    // configuration (docs/v2/plan/design/incremental-walk.md): the walk takes each one the
    // database's memo still holds and describes only what an edit changed or newly reaches.
    let kept = frontend.kept();
    // A launched run (`blaze run` of a host binary): only its start is made here, and each
    // later first call is answered from its bind record or made by the front end
    // (docs/v2/plan/design/launcher.md). Nothing is walked whole.
    if policy.mode == BuildMode::Launched {
        // The view the start reads: what the next build asks what changed since.
        let view = blaze_codegen::backend::Frontend::view(&frontend);
        let guard = blaze_cas::enter();
        let store = Reading { store: &job.input.store, guard: &guard };
        let build = Build::new(&backends, &frontend, &policy, &job.memo)
            .owned_by(&u.target.name)
            .keeping(&kept)
            .recording(&job.input.store);
        let start = match build.start(&asked, &roots.statics, "main", &store) {
            Ok(start) => start,
            Err(errors) => {
                if let Some(put) = profile {
                    unput_profile(job, put);
                }
                return Ok(Err(refused(reader, mounted.root(), errors)));
            }
        };
        let (prepared, puts) = super::run::launch::prepare(
            job,
            unit,
            reader,
            mounted,
            build_time,
            (&frontend, &build, &policy),
            view,
            start,
            &store,
        )?;
        // The start's records join the unit's puts as a walked build's do (`compile` moves
        // `built.puts` into them).
        let mut built = Built::default();
        built.puts = puts;
        built.puts.extend(profile);
        let launch = Some(blaze_core::lock::Lock::new("serve.unit.codegen.launch", Some(prepared)));
        return Ok(Ok(Parts { built, roots, policy, image, build_time, kept, launch }));
    }
    let built = {
        let guard = blaze_cas::enter();
        let store = Reading { store: &job.input.store, guard: &guard };
        Build::new(&backends, &frontend, &policy, &job.memo)
            .owned_by(&u.target.name)
            .verify_store(verify)
            .explain_misses(true)
            .keeping(&kept)
            // Describes as store records (docs/v2/plan/design/store.md, "Describe records"):
            // a function no kept walk holds is read from its record where it has one, and
            // each describe made is among the build's puts.
            .recording(&job.input.store)
            .run_with_statics(&asked, &roots.statics, &roots.check_roots(origin), &store)
    };
    match built {
        Ok(mut built) => {
            built.puts.extend(profile);
            // What the walk and the passes after it did, kept with the build (`blaze explain
            // edit_work`): after an edit each follows the edit, and `whole` says a walk that
            // ran over the program.
            let (walk, work) = (&built.walk, &built.work);
            tracing::info!(
                target: "blaze",
                stage = blaze_query::db::edit_work::CODEGEN_WALK,
                asked = work.asked as u64,
                named = work.named as u64,
                held = work.held as u64,
                each_build = work.each_build as u64,
                described = walk.described as u64,
                verified = walk.verified as u64,
                found = walk.found as u64,
                reached = walk.reached as u64,
                visited = walk.visited as u64,
                replaced = work.replaced as u64,
                copied = work.copied as u64,
                whole = u64::from(walk.whole.is_some()),
                "edit work"
            );
            if let Some(slot) = job.counted.get(unit) {
                // alloc: once per unit built, one entry per profile it used (none or one)
                let _ = slot
                    .set(built.stats.profiles.iter().map(|(id, counts)| (*id, *counts)).collect());
            }
            Ok(Ok(Parts { built, roots, policy, image, build_time, kept, launch: None }))
        }
        Err(errors) => {
            if let Some(put) = profile {
                unput_profile(job, put);
            }
            Ok(Err(refused(reader, mounted.root(), errors)))
        }
    }
}

/// Why a unit's code was not built ([`generate`]): each function blaze could not compile, as
/// the code generator names it (`name: why`), in its order; and, of those the front end failed
/// on in a crate of the toolchain (std's, which no unit checks), the report the job says once
/// however many units reach the function, which each unit's own error points to.
pub(crate) struct Refused {
    pub(crate) errors: Vec<String>,
    pub(crate) toolchain: Vec<ToolchainFailed>,
}

/// An item of a toolchain crate the front end failed on, with one reason: the job's report of
/// it, made of the item alone so that every unit that reaches it makes the same one, and which
/// of the unit's [`Refused::errors`] are its instances.
pub(crate) struct ToolchainFailed {
    /// Its index in [`Refused::errors`] and the instance as the error names it, each one.
    instances: Vec<(usize, String)>,
    /// The item by its path, and its crate by name.
    path: String,
    krate: String,
    /// What failed, as the code generator says it.
    why: String,
    /// The front end's own errors of the item's body
    /// ([`blaze_diagnostics::check::check_one_body`]), and the text of each file they point
    /// into, as the database has it.
    diags: Vec<blaze_diagnostics::Diag>,
    sources: Vec<(String, String)>,
}

impl Refused {
    /// Each function's line in the unit's error, `name: why`: a toolchain function's names it
    /// and points at its report, said once a job ([`ToolchainFailed::report`]).
    pub(crate) fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.errors.iter().enumerate().map(|(at, error)| {
            let of = self
                .toolchain
                .iter()
                .find_map(|it| Some((it, it.instances.iter().find(|(i, _)| *i == at)?)));
            match of {
                Some((failed, (_, named))) => failed.line(named),
                None => error.clone(), // clone: the error path, a line of a unit's error
            }
        })
    }
}

impl ToolchainFailed {
    /// The unit's line of its instance `named`.
    fn line(&self, named: &str) -> String {
        // alloc: the error path, a line of a unit's error
        format!(
            "{named}: the front end fails on `{}` in the toolchain's `{}`; the error above, said once a build, says why",
            self.path, self.krate,
        )
    }

    /// The job's report of the item, as a unit's record keeps it ([`drawn::Item::Toolchain`]):
    /// an error of no unit, its text the item, its crate and why, then each of the front end's
    /// errors of its body, each its child once drawn; with the text of each file they point
    /// into as the database has it, which the record keeps too. The same for every unit that
    /// reaches the item, so the job, which says a diagnostic of no unit once, says it once.
    pub(crate) fn report(&self) -> (super::drawn::Item, &[(String, String)]) {
        let after = match self.diags.len() {
            0 => "the front end reports no error of its body, so the fault is blaze's",
            1 => "the front end's error of its body follows",
            _ => "the front end's errors of its body follow",
        };
        // alloc: the error path, once per unit that reaches the item
        let message = format!(
            "the front end fails on `{}` in the toolchain's `{}`, so blaze can't compile it and no \
             unit that calls it is built (said once a build): {}\n  {after}",
            self.path, self.krate, self.why,
        );
        // clone: the error path, the front end's errors of one body
        (super::drawn::Item::Toolchain { message, diags: self.diags.clone() }, &self.sources)
    }
}

/// The code generator's `errors` as a unit's [`Refused`]: each item of a toolchain crate the
/// front end failed on has its body checked for the report of it (names relative to `root`).
fn refused(reader: &UnitReader, root: &str, errors: Vec<blaze_codegen::CodegenError>) -> Refused {
    let db = reader.db();
    let files = blaze_diagnostics::Files::new(reader.paths(), root);
    let mut toolchain: Vec<ToolchainFailed> = Vec::new(); // alloc: none until a toolchain item failed
    for (at, error) in errors.iter().enumerate() {
        let Some(front) = &error.front else { continue };
        let krate = front.id.krate(db);
        if !krate.data(db).origin.is_lang() {
            continue;
        }
        // clone: the error path, the instance's name in its unit's line
        let named = front.named.clone();
        let path = blaze_query::db::attach(db, || blaze_items::key::path(db, front.id));
        if let Some(failed) = toolchain.iter_mut().find(|it| it.path == path && it.why == front.why)
        {
            failed.instances.push((at, named));
            continue;
        }
        let body = match front.id {
            blaze_items::ItemId::Function(it) => Some(hir_def::DefWithBodyId::FunctionId(it)),
            blaze_items::ItemId::Const(it) => Some(hir_def::DefWithBodyId::ConstId(it)),
            blaze_items::ItemId::Static(it) => Some(hir_def::DefWithBodyId::StaticId(it)),
            _ => None,
        };
        let diags = body
            .map_or_else(Vec::new, |def| blaze_diagnostics::check::check_one_body(db, def, files));
        let sources = texts(reader, root, &diags);
        let krate = match &krate.extra_data(db).display_name {
            Some(name) => name.canonical_name().as_str().to_owned(), // alloc: the error path
            None => format!("{krate:?}"), // alloc: the error path, a crate with no name
        };
        // clone: the error path, the reason the report says
        let why = front.why.clone();
        // alloc: the error path, the item's instances in the unit, most often one
        let instances = vec![(at, named)];
        toolchain.push(ToolchainFailed { instances, path, krate, why, diags, sources });
    }
    Refused { errors: errors.into_iter().map(|it| it.what).collect(), toolchain }
}

/// The text of each file `diags` point into, by its name in them (relative to `root`), as the
/// database has it ([`UnitReader::files_named`]): the text their spans were made against. A name
/// the database has no file of is left out, for the mounts to read ([`Mounted::add_sources`]).
fn texts(
    reader: &UnitReader,
    root: &str,
    diags: &[blaze_diagnostics::Diag],
) -> Vec<(String, String)> {
    if diags.is_empty() {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let db = reader.db();
    let names = blaze_diagnostics::emit::Sources::default().unread(diags);
    (reader.files_named(root, &names).into_iter())
        // alloc: the error path, a file's text, once per file of a failed body's errors
        .map(|(name, file)| (name, db.file_text(file).text(db).to_string()))
        .collect()
}

/// An image kept linked at one path, and what is in it, so the next link of it is a change.
/// Functions, statics and span tables are rows of their key and the hash of each as built
/// ([`kept::Rows`]), which a link brings forward by its build's delta: only what the build
/// changed is hashed. Natives are their contents' hashes, sorted.
pub struct Linked {
    image: Image,
    output: Output,
    rows: kept::Rows,
    natives: Vec<ContentId>,
    names: Vec<String>,
    /// The profile its first link laid it out by ([`super::layout::Weights::id`]): a kept
    /// image keeps that order, so another profile makes it again.
    layout: Option<ContentId>,
    /// A dev executable's program in the daemon's code memory, which its runs map
    /// ([`Output::dev_run`]): each link is committed into it.
    loaded: Option<Arc<super::run::inproc::Loaded>>,
    /// A live image's slots by what their function is across edits ([`edited_slots`]).
    slotted: Vec<(ContentId, InstanceKey)>,
    /// What a dev executable's running code assumes of its workspace crates' items, as of its
    /// last link ([`HirFrontend::live_assumed`]).
    assumed: Vec<(ItemKey, Assumed)>,
    /// What its last link was given, as one id ([`kept::Rows::given`]); `None` before one. A
    /// link that is given the same changes nothing, so it is not staged.
    given: Option<ContentId>,
}

impl Linked {
    /// The image's tables as the linker counts them, each with its elements and bytes
    /// ([`blaze_api::Response::Tables`]): `relocation` and the rest.
    pub(crate) fn tables(&self) -> Vec<(&'static str, u64, u64)> {
        self.image.tables()
    }

    /// The bytes it holds kept: the image's ([`Image::held`]) and its rows' by capacity. A dev
    /// executable's code memory is its program's ([`super::run::inproc::DevRuns`]), which
    /// outlives the image, and is not counted.
    pub(crate) fn held(&self) -> u64 {
        fn rows<T>(column: &Vec<T>) -> u64 {
            (column.capacity() * size_of::<T>()) as u64
        }
        let names = self.names.iter().map(|it| it.capacity() as u64).sum::<u64>();
        self.image.held()
            + self.rows.held()
            + rows(&self.natives)
            + rows(&self.names)
            + names
            + rows(&self.slotted)
            + rows(&self.assumed)
    }

    /// Whether a run maps it now: a dev executable one of whose runs or test processes is
    /// still attached to its code memory.
    pub(crate) fn live(&self) -> bool {
        self.loaded.as_ref().is_some_and(|it| it.running())
    }

    /// Kept idle until its next link: the image's stage scratch given back ([`Image::rest`]).
    pub(crate) fn rest(&mut self) {
        self.image.rest();
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
pub(crate) fn given(job: &Job, unit: u32) -> Result<Compile, String> {
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
    let lib =
        bs::Target { name: String::new(), kind: vec!["lib".to_owned()], crate_types: Vec::new() };
    let targets = match job.graph.package_has_lib(unit) {
        true => vec![lib],
        false => Vec::new(),
    };
    let package = bs::Package { targets, ..bs::Package::default() };
    Ok(Compile::of(&package, &script::target_of(u), job.graph.has(unit, flag::TEST), &linked, &own))
}

/// What native inputs `unit` links: the native libraries, in rustc's order: each crate linked
/// in, what depends first ([`Graph::linked_in`]), the `-l`s its rustc is given (those of its
/// rustflags, then its build script's `rustc-link-lib`s) then its `#[link]`s, with the `-L`s
/// its rustc is given (its rustflags', then its build scripts'), where they are looked for
/// first, as rustc finds a crate's `-l` when it builds that crate; where to look for them
/// after, the unit's `-L`s (its rustflags', then its build scripts'); the object each
/// crate's `global_asm!`s assemble to for `triple`, in the same order, which go on the link
/// line before every library, as rustc puts a crate's code before the libraries; and the
/// unit's link arguments and its rustflags' link options (`-C strip`, ...), which
/// [`blaze_link::args::apply`] reads into the output and the libraries and directories after
/// these.
///
/// [`Graph::linked_in`]: super::graph::Graph::linked_in
fn natives(
    job: &Job,
    unit: usize,
    krate: base_db::Crate,
    frontend: &HirFrontend<'_>,
    triple: &str,
) -> Result<Natives, String> {
    let unit = unit as u32;
    let mine = given(job, unit)?;
    let own = link_rustflags(job, unit)?;
    // alloc: once per link, each crate's libraries and the directories its compile searches
    let mut crates = Vec::new();
    let mut assembled = Vec::new();
    for at in job.graph.linked_in(unit) {
        let theirs;
        let theirs_flags;
        let (compile, flags) = match at == unit {
            true => (&mine, &own),
            false => {
                theirs = given(job, at)?;
                theirs_flags = link_rustflags(job, at)?;
                (&theirs, &theirs_flags)
            }
        };
        // alloc: once per crate linked, its libraries
        let mut libs = Vec::with_capacity(flags.libs.len() + compile.libs.len());
        for lib in &flags.libs {
            libs.push(link_lib(lib).map_err(|why| {
                // alloc: error path
                format!("{why}, in the rustflags of `{}`", job.units().graph.units[at as usize].pkg_id)
            })?);
        }
        for lib in &compile.libs {
            libs.push(link_lib(lib)?);
        }
        // The unit's own crate is the one linked, `krate` (a doctest's example's, not its
        // library's), the rest each unit's.
        let crate_of = match at == unit {
            true => Some(krate),
            false => job.crates.krate(at as usize),
        };
        if let Some(krate) = crate_of {
            libs.extend(frontend.linked(krate).map_err(|it| it.what)?);
            assembled.extend(frontend.global_asm(krate, triple).map_err(|it| it.what)?);
        }
        if !libs.is_empty() {
            let mut search = search_dirs(&flags.search);
            search.extend(search_dirs(&compile.search));
            crates.push(CrateNatives { libs, search });
        }
    }
    let mut search = search_dirs(&own.search);
    search.extend(search_dirs(&mine.search));
    let mut args = own.args;
    args.extend(script_args(job, unit, &mine)?);
    Ok(Natives { crates, search, assembled, args, options: own.options })
}

/// What `unit`'s rustflags ask of a link ([`blaze_link::args::rustflags::Rustflags`]): its
/// `-l`s, `-L`s, `-C link-arg`s and link options. None for a proc macro built for wasm: its
/// rustflags are the host's, for the native shared library rustc would make of it, and blaze
/// links it as a WASI reactor that is no output of the build, which nothing of them can apply
/// to.
pub(crate) fn link_rustflags(
    job: &Job,
    unit: u32,
) -> Result<blaze_link::args::rustflags::Rustflags, String> {
    if wasm_macro(job, unit as usize) {
        return Ok(blaze_link::args::rustflags::Rustflags::default());
    }
    let plan = job.plan(unit as usize).map_err(|it| it.what)?;
    blaze_link::args::rustflags::Rustflags::read(&plan.rustflags).map_err(|why| {
        // alloc: error path
        format!("{why} (of `{}`)", job.units().graph.units[unit as usize].pkg_id)
    })
}

/// The `rustc-link-arg*`s of the build scripts `unit` is linked with (`mine`, its [`given`]),
/// in the order cargo gives rustc them, each naming its script's package. They follow the
/// `-C link-arg`s of its rustflags ([`link_rustflags`]) on the link line.
pub(crate) fn script_args(
    job: &Job,
    unit: u32,
    mine: &Compile,
) -> Result<Vec<blaze_link::args::GivenArg>, String> {
    let units = job.units();
    let runs = job.graph.links(unit);
    // alloc: once per link, its build scripts' arguments
    let mut args = Vec::with_capacity(mine.link_args.len());
    for blaze_build_script::apply::LinkArg { from, arg } in &mine.link_args {
        let run = runs.get(*from).ok_or_else(|| {
            // alloc: error path
            format!(
                "the link argument `{arg}` of unit `{}` names build script run {from} of the {} it \
                 is linked with",
                units.graph.units[unit as usize].target.name,
                runs.len()
            )
        })?;
        args.push(blaze_link::args::GivenArg {
            // clone: the link owns its arguments, once per argument per link
            arg: arg.clone(),
            from: blaze_link::args::ArgFrom::BuildScript {
                // alloc: as above
                package: units.graph.units[*run as usize].pkg_id.to_string(),
            },
        });
    }
    Ok(args)
}

/// The `-l`s and `-L`s of the rustflags of the crates linked into `unit` other than its own
/// (each `-L` as `-L DIR`), each with its crate's package, in link order: its link reads them,
/// a crate's `-L`s being where its bundled static libraries are found first ([`natives`]), and
/// its plan holds only its own rustflags. Empty for a unit that links nothing.
pub(crate) fn linked_rustflag_libs(job: &Job, unit: u32) -> Result<Vec<(String, String)>, String> {
    // alloc: none unless a crate linked in has a `-l` in its rustflags
    let mut out = Vec::new();
    if wasm_macro(job, unit as usize) {
        return Ok(out);
    }
    for at in job.graph.linked_in(unit) {
        if at == unit {
            continue;
        }
        let flags = link_rustflags(job, at)?;
        let package = &job.units().graph.units[at as usize].pkg_id;
        // alloc: once per `-l` per unit's key
        out.extend(flags.libs.into_iter().map(|lib| (package.to_string(), lib)));
        // alloc: once per `-L` per unit's key
        out.extend(
            flags.search.iter().map(|dir| (package.to_string(), format!("-L {}", dir.display()))),
        );
    }
    Ok(out)
}

/// The directories a link searches for native libraries, from the unit's `-L`s: each one's
/// directory with its kind taken off as rustc takes it (`native=/out` is `/out`), and none of
/// `crate=` or `dependency=` ([`blaze_build_script::output::native_search_dir`]).
pub(crate) fn search_dirs(search: &[PathBuf]) -> Vec<PathBuf> {
    search
        .iter()
        .filter_map(|it| blaze_build_script::output::native_search_dir(it))
        // alloc: once per link, the directories it searches
        .map(std::path::Path::to_path_buf)
        .collect()
}

/// [`natives`]: each crate's libraries with where its compile looks for them, where to look for
/// them after, the crates' `global_asm!` objects, the link arguments, and the rustflags' link
/// options.
struct Natives {
    crates: Vec<CrateNatives>,
    search: Vec<PathBuf>,
    assembled: Vec<GlobalAsmObject>,
    args: Vec<blaze_link::args::GivenArg>,
    options: Vec<blaze_link::args::rustflags::GivenOption>,
}

/// The place on the link line of the first library: the `global_asm!` objects take the places
/// before it, so a crate that gains or loses one moves no library the image already has (an
/// image keeps the place a native input was added at).
const FIRST_LIBRARY: u32 = 1 << 20;

/// One crate's native libraries and the `-L`s its rustc is given ([`natives`]), owned for
/// [`blaze_link::target::CrateLibs`].
struct CrateNatives {
    libs: Vec<NativeLib>,
    search: Vec<PathBuf>,
}

/// A native input a link read off the machine: its path, and the BLAKE3 hash of its contents.
pub(crate) type Read = (String, ContentId);

/// What [`link`] made: what the store is given of the file, every native input the link read
/// off the machine, a test binary's execution table, how it reached the image, for a dev
/// executable whose runs could not take the link as a patch, why they were told to start
/// again, and, for an image linked from scratch because the kept one refused the change as a
/// splice ([`Full::NoThunk`]), what the build says of it.
pub(crate) struct LinkMade {
    /// The file, keyed by the root of the linker's digest of it ([`Image::digest`],
    /// `compile::linked_key`), which the blocks each link wrote keep: the runs of it that
    /// hold bytes, or, where the image took the link as a patch of a file the store can
    /// derive from, the patch ([`Filed::written`]).
    pub(crate) written: Written,
    /// The file itself, for the loop to place it from ([`Made`]): what the link wrote of the
    /// file before it where the image took the link as a patch, the runs of the whole file
    /// that hold bytes otherwise. The build made it, so its place reads, clones and waits
    /// for no file of the store. None for a wasm module, which is not placed to run.
    pub(crate) made: Option<Made>,
    /// The image's debug side file and what goes with it, each by the path it goes at
    /// ([`debug_side`], [`SideFile`]): for the loop to place from these bytes, after the
    /// build's answer. Empty for an image with none, and for a link that changed nothing.
    pub(crate) side: Vec<SideFile>,
    pub(crate) read: Vec<Read>,
    pub(crate) table: Option<blaze_codegen::ExecTable>,
    pub(crate) how: Linking,
    /// The image's goal and how many of its functions are optimized ([`tiers`]).
    pub(crate) tiers: Option<Tiers>,
    pub(crate) restart: Option<String>,
    pub(crate) whole: Option<String>,
}

/// Where an image's debug info goes, given its level, its format and the profile's
/// `split-debuginfo`: beside the image unless the profile says `off`, which puts it inside as
/// before. `packed` and `unpacked` are both the one side file blaze writes (it has no
/// per-crate objects to leave debug info in). An image with no debug info has none either
/// way, and a format blaze writes no side file for (PE, wasm) keeps what it had.
pub(crate) fn debug_place(
    debug: blaze_link::Debug,
    format: blaze_link::Format,
    split: Option<&str>,
) -> Result<blaze_link::Debug, String> {
    use blaze_link::{Debug, Format};
    match (debug, split) {
        (Debug::None, _) => Ok(Debug::None),
        (_, Some("off")) => Ok(Debug::Lines),
        (_, None | Some("packed" | "unpacked")) => Ok(match format {
            Format::Elf | Format::MachO => Debug::Beside,
            Format::Pe | Format::Wasm => debug,
        }),
        // alloc: error path
        (_, Some(other)) => Err(format!(
            "split-debuginfo `{other}` is none rustc takes: `off`, `packed` or `unpacked`"
        )),
    }
}

/// Where the side file named `name` ([`blaze_link::DebugWrite::name`], from the image's own
/// file name) goes for the executable at `exe`. ELF: beside it under the name its
/// `.gnu_debuglink` holds, whatever the copy is called. Mach-O: the bundle a debugger looks
/// for beside the executable, `<its file name>.dSYM`.
fn side_path(format: blaze_link::Format, exe: &str, name: &str) -> std::path::PathBuf {
    let exe = std::path::Path::new(exe);
    let dir = exe.parent().unwrap_or(std::path::Path::new(""));
    match (format, exe.file_name().and_then(|it| it.to_str())) {
        (blaze_link::Format::MachO, Some(own)) => {
            // alloc: once per side file written, its path
            dir.join(format!("{own}.dSYM")).join("Contents/Resources/DWARF").join(own)
        }
        _ => dir.join(name),
    }
}

/// A file of an image's debug info for the loop to place ([`debug_side`]): the side file at
/// one of its paths, or a `.dSYM` bundle's `Info.plist`.
#[derive(Clone, Debug)] // clone: a path, a key and a refcount, once per path it is placed at
pub(crate) struct SideFile {
    /// Where it goes, absolute.
    pub(crate) path: String,
    /// What names its bytes for the loop's record of the path: the side file's by the build
    /// id of the executable it describes, the plist's by its bytes.
    pub(crate) key: blaze_cas::Key,
    /// The file, shared by the paths it goes at: the whole of it as the runs that hold
    /// bytes, or what the link's update wrote of the one before it ([`Made`]).
    pub(crate) file: Arc<Made>,
}

/// The size of the blocks [`sparse_of`] looks at: one that is all zeros is left a hole.
const SIDE_BLOCK: usize = 4096;

/// `bytes` as the runs of it that hold anything, by [`SIDE_BLOCK`]: a side file's sections
/// have room after them (`blaze_link`'s `beside`), which is zeros and stays holes.
fn sparse_of(bytes: &[u8]) -> blaze_cas::Sparse {
    // alloc: once per side file made, its runs, which the paths' writes own
    let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
    for (index, block) in bytes.chunks(SIDE_BLOCK).enumerate() {
        if block.iter().all(|it| *it == 0) {
            continue;
        }
        let at = (index * SIDE_BLOCK) as u64;
        match runs.last_mut() {
            Some((start, run)) if *start + run.len() as u64 == at => run.extend_from_slice(block),
            _ => runs.push((at, block.to_vec())), // alloc: a run's bytes, as above
        }
    }
    blaze_cas::Sparse { len: bytes.len() as u64, runs }
}

/// The debug side file of the image `linked` was just committed as, for the loop to place
/// beside `path` and beside the copy cargo makes of it; nothing for an image with none, and
/// for a link that changed nothing of it. Derived data: it is in no record of the store,
/// and a build that places the executable from the store without linking it places none
/// (the executable then backtraces by its symbol table, without lines, until its next link).
///
/// Off the answer's path, and no copy of a file (docs/v2/plan/design/debug-side-file.md,
/// "The write after the answer"). Until 2026-11-11 this ran in the link and wrote the files
/// itself: 72 ms of a 171 ms `unit link` on ripgrep, almost all of it two copies of the
/// whole file. Then the daemon held the file's bytes beside the image and each path was
/// written new from them: still the file's length an edit (27 ms of hello world's link: the
/// whole file summed again for its CRC, copied once in memory and scanned for its runs, and
/// 4.4 MB written at each path). Now what the link hands over is what its update wrote
/// ([`Made::Patch`]: the changed functions' records, the header and the seal, a few
/// kilobytes), which the loop writes into a clone of the file at each path
/// (`blaze_cas::Store::place_patched`); the file's whole bytes are the linker's alone
/// (`blaze_link::Image::debug_file_view`, shared, read only where a path does not hold the
/// file the patch is over), and its CRC is kept by block (`blaze_link`'s `beside::crc`).
///
/// Who reads the side file, and when: std's backtrace in the built program (as it panics),
/// gdb and lldb (as someone debugs it), `blaze perf` (as it names a run's lines). No
/// artifact names it, so the build's answer does not wait for it (`serve::job`'s
/// `Job::names`): it is placed after the answer, a run of the daemon's own (`blaze run`,
/// `blaze test`) starts only once it is, and a reader outside the daemon finds the last
/// side file whole until the rename (on ELF std reads it with the edited function's lines
/// stale and gdb refuses it by CRC; on macOS both refuse it by UUID and use symbols).
///
/// What the link still pays is this span: the changed functions' rows encoded
/// (`Image::debug_file`) and the blocks its writes touch summed (`summed`).
fn debug_side(
    job: &Job,
    unit: usize,
    path: &str,
    linked: &mut Linked,
) -> Result<Vec<SideFile>, LinkCause> {
    if linked.output.debug != blaze_link::Debug::Beside {
        return Ok(Vec::new()); // alloc: none, empty
    }
    let format = linked.output.format;
    let span = tracing::info_span!(
        target: "blaze",
        "link debug file",
        unit = %path,
        bytes = tracing::field::Empty,
        written = tracing::field::Empty,
        ranges = tracing::field::Empty,
        records = tracing::field::Empty,
        encoded = tracing::field::Empty,
        summed = tracing::field::Empty,
        all = tracing::field::Empty,
        whole = tracing::field::Empty,
        placed_bytes = tracing::field::Empty,
    )
    .entered();
    let linker = |error: blaze_link::LinkError| LinkCause::Linker { step: LinkStep::Debug, error };
    let Some(write) = linked.image.debug_file().map_err(linker)? else {
        return Ok(Vec::new()); // alloc: none, empty
    };
    // A link that changed nothing of it leaves the file at each path as its last link
    // placed it: nothing is written.
    if write.base.is_some() && write.writes.is_empty() {
        span.record("bytes", write.len);
        span.record("written", 0u64);
        return Ok(Vec::new()); // alloc: none, empty
    }
    span.record("bytes", write.len);
    span.record("written", write.written());
    span.record("ranges", write.writes.len());
    span.record("records", write.records);
    // How many functions' rows were encoded, and why every one was when the update did not
    // follow the link's change; and the bytes of the file read to seal it.
    span.record("encoded", write.encoded);
    span.record("summed", write.summed);
    if let Some(why) = write.all {
        span.record("all", why);
    }
    if let Some(why) = write.whole {
        span.record("whole", why);
    }
    let side_key = |id: &[u8; 16]| blaze_cas::Key(ContentId::of("blaze v2 serve debug side file", id));
    let blaze_link::DebugWrite { name, id, base, len, writes, plist, .. } = write;
    let file = match base {
        // What the update wrote of the side file before it: the loop writes those ranges
        // into a clone of the file at each path. The whole file is the linker's, shared, and
        // read only where a path does not hold the file of `base`.
        Some(base) => {
            let whole = linked.image.debug_file_view();
            span.record("placed_bytes", writes.iter().map(|it| it.1.len() as u64).sum::<u64>());
            Made::Patch {
                base: side_key(&base),
                file: blaze_cas::Sparse { len, runs: writes },
                // alloc: once per side file patched, the closure's reference count
                whole: Arc::new(move || sparse_of(&whole)),
            }
        }
        // The whole file, its one write: the runs of it that hold bytes.
        None => {
            let _span =
                tracing::info_span!(target: "blaze", "link debug file: its runs", unit = %path)
                    .entered();
            let file = match &writes[..] {
                [(0, bytes)] if bytes.len() as u64 == len => sparse_of(bytes),
                _ => blaze_cas::Sparse { len, runs: writes },
            };
            span.record("placed_bytes", file.bytes());
            Made::Whole(file)
        }
    };
    let file = Arc::new(file); // alloc: once per side file made, shared by its paths
    let key = side_key(&id);
    let plist = plist.as_ref().map(|bytes| {
        let key = blaze_cas::Key(ContentId::of("blaze v2 serve debug side plist", bytes));
        // alloc: once per side file made, a bundle's plist of a few hundred bytes
        let file = blaze_cas::Sparse { len: bytes.len() as u64, runs: vec![(0, bytes.clone())] };
        (key, Arc::new(Made::Whole(file))) // alloc: as above, shared by its paths
    });
    let plan = job.plan(unit).map_err(|it| LinkCause::Given(it.what))?;
    let exes = std::iter::once(path).chain(super::artifact::uplifted(path, plan));
    let mut files = Vec::new(); // alloc: once per side file made, a path or two
    for exe in exes {
        let side = side_path(format, exe, &name);
        // A `.dSYM` bundle's `Info.plist` (`NAME.dSYM/Contents/Resources/DWARF/NAME`); none
        // for a side file that is no bundle.
        if let (Some((key, file)), Some(contents)) = (&plist, side.ancestors().nth(3)) {
            files.push(SideFile {
                path: named(contents.join("Info.plist"))?,
                key: *key,
                file: Arc::clone(file), // clone: a refcount
            });
        }
        // clone: a refcount
        files.push(SideFile { path: named(side)?, key, file: Arc::clone(&file) });
    }
    Ok(files)
}

/// `path` as the text the loop names an output's path by.
fn named(path: std::path::PathBuf) -> Result<String, LinkCause> {
    path.into_os_string().into_string().map_err(|it| {
        // alloc: error path
        LinkCause::Given(format!(
            "{}: a debug side file's path is not UTF-8, and the loop names an output by text",
            std::path::Path::new(&it).display()
        ))
    })
}

/// What the build says when a splice was refused for a thunk it could not place and the
/// image is linked from scratch in its place: `branch` is the linker's words for the branch
/// ([`blaze_link::LinkError::no_thunk`]).
pub(crate) fn full_link_note(branch: &str) -> String {
    // alloc: once per link from scratch
    format!("full link: the splice could not place a thunk for {branch}")
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
            // A kept image with no room for a change is linked from scratch, said on the link
            // line ([`Full::NoRoom`]); what reaches here is a refusal that link could not take
            // either.
            LinkCause::Linker { step, error } if error.no_room().is_some() => write!(
                f,
                "{}: {error}. A link of the image from scratch has no room for it either: the limit is the \
                 output's, not the kept image's",
                step.name()
            ),
            LinkCause::Linker { step, error } => write!(f, "{}: {error}", step.name()),
            LinkCause::Native(error) => write!(f, "{}: {}", error.path.display(), error.error),
            LinkCause::NativeGone(path) => {
                write!(f, "{}: the native library is gone", path.display())
            }
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
    /// Making the image's debug side file.
    Debug,
}

impl LinkStep {
    pub(crate) fn name(self) -> &'static str {
        match self {
            LinkStep::Output => "the output",
            LinkStep::Locate => "finding the native inputs",
            LinkStep::Image => "making the image",
            LinkStep::Stage => "staging the change",
            LinkStep::Bytes => "writing the image",
            LinkStep::Debug => "making the debug side file",
        }
    }
}

/// `generated`, built from `krate` as `unit`, linked into the image at `path` as `makes` says,
/// and how. The image kept at
/// `path` is taken out of `images`, relinked and put back; a link that fails after taking it
/// puts it back as it was, so the next link still patches it. A kept image of another
/// [`Output`] is replaced, and the link says which fields changed ([`Full::Output`]).
pub(crate) fn link(
    job: &Job,
    unit: usize,
    krate: base_db::Crate,
    path: &str,
    makes: &Makes,
    generated: &Generated<'_>,
    images: &mut Vec<KeptImage>,
) -> Result<LinkMade, LinkFailed> {
    // In a task of the link's tag, so the image and the staging a link keeps are named on the
    // daemon's memory line.
    blaze_mem::frame::task(blaze_mem::Tag::Link, |_| {
        link_at(job, unit, krate, path, makes, generated, images)
    })
    // alloc: error path
    .map_err(|cause| LinkFailed { image: path.to_owned(), cause })
}

fn link_at(
    job: &Job,
    unit: usize,
    krate: base_db::Crate,
    path: &str,
    makes: &Makes,
    generated: &Generated<'_>,
    images: &mut Vec<KeptImage>,
) -> Result<LinkMade, LinkCause> {
    let u = job.unit(unit).map_err(|it| it.what)?;
    let built = generated.built;
    if generated.policy.mode == BuildMode::Launched {
        return launched(job, unit, path, generated).map_err(LinkCause::Given);
    }
    let debug = match generated.policy.profile.debuginfo {
        DebugInfo::None => blaze_link::Debug::None,
        _ => blaze_link::Debug::Lines,
    };
    let wasm = wasm_macro(job, unit);
    let shared = match makes {
        // A proc macro built for wasm is a WASI reactor ([`blaze_link::target::reactor`]).
        Makes::Shared { proc_macro: true } if wasm => None,
        Makes::Shared { .. } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_owned();
            let exports = built.symbols.iter().map(|it| it.1.clone()).collect();
            Some((name, exports))
        }
        _ => None,
    };
    let test = job.graph.has(unit as u32, flag::TEST);
    let linker =
        |step: LinkStep| move |error: blaze_link::LinkError| LinkCause::Linker { step, error };
    let output = match wasm {
        // A wasm module is linked whole and never slid or patched in place, and its debug info
        // is its name section: each expansion instantiates the module afresh.
        true => blaze_link::target::reactor(blaze_link::Debug::None, false),
        false => {
            // A unit with no platform of its own is built for this machine, to run here; one
            // built for `--target` runs wherever that is ([`blaze_link::target::RunsOn`]).
            let runs = match u.platform {
                None => blaze_link::target::RunsOn::Here,
                Some(_) => blaze_link::target::RunsOn::Elsewhere,
            };
            blaze_link::target::output(triple(job, u), shared, debug, true, runs)
        }
    };
    let mut output = output.map_err(linker(LinkStep::Output))?;
    // Debug info goes beside the image unless the profile says `split-debuginfo = "off"`
    // ([`debug_place`]): the image an edit writes, hashes and places carries none.
    output.debug = debug_place(output.debug, output.format, u.profile.split_debuginfo.as_deref())
        .map_err(LinkCause::Given)?;
    // Code built to take edits ([`policy`]) is linked as a dev executable: the live descriptor
    // and the start hook's reservations, committed into the daemon's code memory ([`relink`]).
    // A doctest's programs run as processes of their own under its runner, never mapped.
    if generated.image {
        output.live = Some(blaze_link::Live);
        output.dev_run = Some(super::run::inproc::dev_run(None));
    }
    output.exec_log = test && blaze_codegen::exec::records(generated.policy.mode);
    output.sanitize = generated.policy.sanitize;
    let (assembled, located) = {
        let _span = tracing::info_span!(target: "blaze", "link natives", unit = %path).entered();
        // Each crate's libraries and assembly, asked of the front end; then where each
        // library is on the machine. Two spans, so a link that is slow here says which.
        let Natives { crates, mut search, assembled, args, options } = {
            let _span =
                tracing::info_span!(target: "blaze", "link natives: the crates' libraries")
                    .entered();
            natives(job, unit, krate, &generated.frontend, triple(job, u))?
        };
        // What the rustflags' link options and the link arguments ask: set on the output, and
        // the arguments' libraries and directories after the rest, as rustc appends them to
        // the link line. One it does not implement stops the link, named.
        let applied = blaze_link::args::apply(&mut output, &options, &args)
            .map_err(linker(LinkStep::Output))?;
        search.extend(applied.search);
        // alloc: once per link, each crate's libraries as the linker takes them
        let mut libs: Vec<CrateLibs<'_>> =
            crates.iter().map(|it| CrateLibs { libs: &it.libs, search: &it.search }).collect();
        libs.push(CrateLibs { libs: &applied.libs, search: &[] });
        let sysroot = crate::toolchain::sysroot().ok_or(LinkCause::NoToolchain)?;
        let located = {
            let _span = tracing::info_span!(target: "blaze", "link natives: locate").entered();
            blaze_link::target::locate(&mut output, &libs, &search, &sysroot)
                .map_err(linker(LinkStep::Locate))?
        };
        (assembled, located)
    };
    // The span tables the code names, as a change of those the last link was given: what
    // an edit moved is made again, the rest is neither read nor encoded (`items` resolved,
    // `read` tables kept untouched, `made` tables encoded; `whole` says why nothing kept
    // was used).
    let side = {
        let span = tracing::info_span!(
            target: "blaze",
            "link inputs",
            unit = %path,
            items = tracing::field::Empty,
            functions = tracing::field::Empty,
            verified = tracing::field::Empty,
            read = tracing::field::Empty,
            made = tracing::field::Empty,
            bytes = tracing::field::Empty,
            whole = tracing::field::Empty,
        )
        .entered();
        let side = generated.side()?;
        span.record("items", side.work.items);
        span.record("functions", side.work.functions);
        span.record("verified", side.work.verified);
        span.record("read", side.work.kept);
        span.record("made", side.work.made);
        span.record("bytes", side.work.bytes);
        if let Some(why) = side.work.whole {
            span.record("whole", why);
        }
        side
    };
    // The profiles the target directory keeps for layout, what they say of this image, and
    // that as this build's instances.
    let profiles = super::layout::profiles(job)?;
    let weights = profiles.and_then(|it| it.weights(path));
    let heat = weights.map(|it| it.heat(&built.identities));

    let (mut linked, changed, relaid) =
        kept_image(images, path, &output, weights.map(super::layout::Weights::id))?;
    // The image goes back whether the link is made or not: one that fails leaves it as it
    // was, and the next link is a change of it.
    let mut made = relink(
        job,
        unit,
        path,
        generated,
        test,
        &assembled,
        &located,
        side,
        heat.as_ref(),
        &mut linked,
    );
    // A splice refused for a thunk it could not place ([`blaze_link::Refusal::NoThunk`]: a
    // branch in code the kept image holds in place, out of reach of the patch region) is a
    // change a link from scratch takes, its islands cut through the code. That link is a
    // named step, never a silent one: the link line says it ([`Full::NoThunk`]) and the
    // build says which branch ([`LinkMade::whole`]). The kept image is let go first, so the
    // two are never held at once; a link from scratch that fails too is the build's error,
    // and the next link starts from an image never laid out.
    // A splice refused for want of room ([`blaze_link::LinkError::no_room`]) is taken the same
    // way, and said the same way ([`Full::NoRoom`]): only where an image was kept, since one
    // just made that has no room is refused for good.
    let mut whole = None;
    let patched = changed.is_some_and(OutputFields::is_empty) && !relaid;
    let scratch = match &made {
        Err(LinkCause::Linker { step: LinkStep::Stage, error }) => match error.no_thunk() {
            Some(branch) => Some((Full::NoThunk, full_link_note(branch))),
            None if patched && error.no_room().is_some() => Some((
                Full::NoRoom,
                // alloc: once per link from scratch
                format!("full link: the kept image has no room for the change: {error}"),
            )),
            None => None,
        },
        _ => None,
    };
    if let Some((reason, why)) = scratch {
        tracing::info!(target: "blaze", unit = %path, why = %why, "link from scratch");
        match fresh_image(path, &output, weights.map(super::layout::Weights::id)) {
            Ok(fresh) => {
                linked = fresh;
                made = relink(
                    job,
                    unit,
                    path,
                    generated,
                    test,
                    &assembled,
                    &located,
                    side,
                    heat.as_ref(),
                    &mut linked,
                );
                whole = Some((reason, why));
            }
            Err(cause) => {
                images.push(KeptImage { path: path.to_owned(), kept: linked });
                return Err(cause);
            }
        }
    }
    // What address holds which function, for a profile of this build to be read through.
    let tables = tracing::info_span!(target: "blaze", "link tables", unit = %path).entered();
    let mapped = match (&made, profiles) {
        (Ok(_), Some(profiles)) => profiles.write_map(
            path,
            linked.image.build_id(),
            &linked.image.functions(),
            &built.identities,
        ),
        _ => Ok(()),
    };
    // What each address is by code identity and what it refers to, for `blaze perf` to name a
    // run's functions across builds and say why each changed.
    let perf_mapped = match &made {
        Ok(_) => {
            let _span =
                tracing::info_span!(target: "blaze", "link perf map", unit = %path).entered();
            super::perf::write_map(job, path, &linked.image, built)
        }
        Err(_) => Ok(()),
    };
    drop(tables);
    // The image's debug info, beside it: derived from the image as committed.
    let debug = match &made {
        Ok(_) => debug_side(job, unit, path, &mut linked),
        Err(_) => Ok(Vec::new()), // alloc: none, empty
    };
    // The file's identity as the last commit left it: the blocks this link wrote, hashed.
    let root = match &made {
        Ok(_) => linked
            .image
            .digest()
            .map_err(|error| LinkCause::Linker { step: LinkStep::Bytes, error }),
        Err(_) => Ok([0; 32]),
    };
    // What the store is given of the file, settled while the image is at hand: the patch the
    // image took the link as where the store can derive the file from the one before it, else
    // the runs of the file that hold bytes ([`Filed::written`]). A wasm module is read back
    // by its bytes ([`super::compile`]'s `Module`), so it goes whole.
    let made = made.and_then(|(filed, read, table, laid, told)| {
        // What the link wrote of the file before it, where the image took it as a patch:
        // what its place writes ([`Made::Patch`]).
        let patched = match &filed {
            Filed::Patch(patch) if !patch.full && !wasm => patch.base.map(|base| {
                // clone: the patch's ranges, which the store's record and the path's place
                // each own: the edit's bytes, never the file's
                (base, blaze_cas::Sparse { len: patch.len, runs: patch.writes.clone() })
            }),
            _ => None,
        };
        let written = filed
            .written(&job.input.store, &linked.image, root?, path, wasm)
            .map_err(|error| LinkCause::Linker { step: LinkStep::Bytes, error })?;
        // The file for its place. A patch goes as the ranges it wrote, with the image's
        // state shared for whoever finds the path without the file before it: nothing of the
        // file is read, copied or allocated here (until 2026-11-11 every link read the whole
        // file's runs, 21 MB for hello world, which each path's place then wrote). A file
        // written whole is read from the image while it is at hand: the runs that hold
        // bytes, none of the patch regions' room (`link runs` says how many bytes).
        let runs = match (wasm, patched) {
            (true, _) => None,
            (false, Some((base, file))) => {
                let view = linked
                    .image
                    .file_view()
                    .map_err(|error| LinkCause::Linker { step: LinkStep::Bytes, error })?;
                Some(Made::Patch {
                    base: super::compile::linked_key(&base),
                    file,
                    // alloc: once per link patched, the closure's reference count
                    whole: Arc::new(move || {
                        let (len, runs) = view.runs();
                        blaze_cas::Sparse { len, runs }
                    }),
                })
            }
            (false, None) => {
                let span = tracing::info_span!(
                    target: "blaze",
                    "link runs",
                    unit = %path,
                    bytes = tracing::field::Empty,
                )
                .entered();
                let (len, runs) = linked
                    .image
                    .file_runs()
                    .map_err(|error| LinkCause::Linker { step: LinkStep::Bytes, error })?;
                let file = blaze_cas::Sparse { len, runs };
                span.record("bytes", file.bytes());
                Some(Made::Whole(file))
            }
        };
        Ok((written, runs, read, table, laid, told))
    });
    images.push(KeptImage { path: path.to_owned(), kept: linked });
    let (written, runs, read, table, laid, told) = made?;
    mapped?;
    perf_mapped?;
    let side = debug?;
    let how = match (&whole, relaid && changed.is_some_and(OutputFields::is_empty)) {
        (Some((reason, _)), _) => Linking::Full(*reason),
        (None, true) => Linking::Full(Full::Layout),
        (None, false) => linking(changed, laid, output.incremental),
    };
    let restart = match told {
        super::run::inproc::Told::Restart { why } => Some(why),
        super::run::inproc::Told::Nothing | super::run::inproc::Told::Patched { .. } => None,
    };
    let tiers = tiers(job, unit, generated.built, generated.policy.toolchain.arch)
        .map_err(LinkCause::Given)?;
    Ok(LinkMade {
        written,
        made: runs,
        side,
        read,
        table,
        how,
        tiers,
        restart,
        whole: whole.map(|it| it.1),
    })
}

/// A launcher program's link ([`super::run::launch::link`]): the file at `path` is blaze's
/// launcher, a sidecar beside it (and beside the copy a run starts it from) names the program,
/// and the launchers running the program were told of this build. Nothing is laid out, so
/// there is no image to keep, no patch room and no debug side file.
fn launched(
    job: &Job,
    unit: usize,
    path: &str,
    generated: &Generated<'_>,
) -> Result<LinkMade, String> {
    use blaze_launch_host::Told;
    let linked = super::run::launch::link(job, unit, path, generated)?;
    let whole = |bytes: &[u8]| blaze_cas::Sparse {
        len: bytes.len() as u64,
        runs: vec![(0, bytes.to_vec())], // alloc: the file's one run, once a link
    };
    let plan = job.plan(unit).map_err(|it| it.what)?;
    let sidecar = Arc::new(Made::Whole(whole(&linked.sidecar)));
    let key = blaze_cas::Key(ContentId::of("blaze v2 serve launch sidecar", &linked.sidecar));
    // The file and its sidecar are the same bytes at every build of the program: placed by
    // its first, and left as they are by every later one, so a running launcher's file is
    // never replaced under it and a build that changed a body writes no file at all.
    // alloc: the sidecar's paths, once a program
    let side = match linked.first {
        true => std::iter::once(path)
            .chain(super::artifact::uplifted(path, plan))
            .map(|it| SideFile {
                path: super::run::launch::sidecar_of(it),
                key,
                file: Arc::clone(&sidecar),
            })
            .collect(),
        false => Vec::new(), // alloc: none, empty
    };
    let restart = linked.told.iter().find_map(|it| match it {
        Told::Restart { why } => Some(why.clone()), // clone: the reason, into the build's answer
        Told::Nothing | Told::Edit { .. } => None,
    });
    let edited = linked.told.iter().any(|it| matches!(it, Told::Edit { .. }));
    let how = match (linked.first, edited || restart.is_some()) {
        (true, _) => Linking::First,
        (false, true) => Linking::Spliced,
        (false, false) => Linking::Unchanged,
    };
    let tiers = tiers(job, unit, generated.built, generated.policy.toolchain.arch)?;
    Ok(LinkMade {
        made: linked.first.then(|| Made::Whole(whole(&linked.file))),
        // The store is given the launcher's bytes by a program's first build; every later
        // one names what it keeps, and hashes and writes nothing.
        written: match linked.first {
            true => Written::Bytes(linked.file.to_vec()), // alloc: the file, once a program
            false => Written::Kept(linked.key),
        },
        side,
        read: Vec::new(), // alloc: none, empty
        table: None,
        how,
        tiers,
        restart,
        whole: None,
    })
}

/// The image to link `output` laid out by profile `layout` into at `path`, taken out of
/// `images`; the fields of `output` that differ from the kept image's (`None` when none was
/// kept there); and whether the kept image was laid out by another profile. A kept image of
/// another output or profile is let go only once its replacement is made, so an error leaves
/// `images` as it was.
pub(crate) fn kept_image(
    images: &mut Vec<KeptImage>,
    path: &str,
    output: &Output,
    layout: Option<ContentId>,
) -> Result<(Linked, Option<OutputFields>, bool), LinkCause> {
    let at = images.iter().position(|it| it.path == path);
    let changed = at.map(|at| OutputFields::between(&images[at].kept.output, output));
    let relaid = at.is_some_and(|at| images[at].kept.layout != layout);
    if let (Some(at), Some(changed)) = (at, changed)
        && changed.is_empty()
        && !relaid
    {
        return Ok((images.swap_remove(at).kept, Some(changed), false));
    }
    let fresh = fresh_image(path, output, layout)?;
    if let Some(at) = at {
        images.swap_remove(at);
    }
    Ok((fresh, changed, relaid))
}

/// An image of `output` at `path`, laid out by profile `layout`, with nothing linked into it:
/// what a first link, or a link from scratch, stages against.
pub(crate) fn fresh_image(
    path: &str,
    output: &Output,
    layout: Option<ContentId>,
) -> Result<Linked, LinkCause> {
    let to = blaze_core::FileName::new(path.trim_start_matches('/')).ok_or(LinkCause::Unnamed)?;
    Ok(Linked {
        image: Image::new(to, output.clone())
            .map_err(|error| LinkCause::Linker { step: LinkStep::Image, error })?,
        output: output.clone(),
        rows: kept::Rows::default(),
        natives: Vec::new(),
        names: Vec::new(),
        layout,
        loaded: None,
        slotted: Vec::new(),
        assumed: Vec::new(),
        given: None,
    })
}

/// What [`relink`] did to an image: staged nothing (it was given what its last link was, so
/// its file is the one it has), patched it, or laid it out whole
/// ([`blaze_link::Staged::full`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Laid {
    Nothing,
    Patched,
    Whole,
}

/// How a link went: `changed` is the fields of the output that differ from the kept image's
/// (`None` when no image was kept), `laid` what blaze-link did to it. A kept image of the same
/// output laid out whole is one whose first link failed (nothing was laid out to patch) or one
/// that keeps no room.
pub(crate) fn linking(changed: Option<OutputFields>, laid: Laid, incremental: bool) -> Linking {
    match (changed, laid) {
        (Some(changed), _) if !changed.is_empty() => Linking::Full(Full::Output(changed)),
        (_, Laid::Nothing) => Linking::Unchanged,
        (_, Laid::Patched) => Linking::Spliced,
        (_, Laid::Whole) if !incremental => match changed {
            Some(_) => Linking::Full(Full::NotIncremental),
            None => Linking::First,
        },
        (_, Laid::Whole) => Linking::First,
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
    let mut rows = Vec::with_capacity(statics.len());
    for it in statics {
        rows.push(static_row(it)?);
    }
    rows.sort_unstable();
    let (mut gone, mut new) = (Vec::new(), Vec::new());
    diff(old, &rows, &mut gone, &mut new);
    Ok((rows, gone, new))
}

/// The row of one static: its item, its instance and the hash of its whole definition.
pub(crate) fn static_row(it: &blaze_codegen::Static) -> Result<StaticRow, String> {
    // alloc: per static hashed, its encoding, freed with the row made
    let mut scratch = Vec::new();
    blaze_protocol::cbor::append(&mut scratch, it)
        .map_err(|error| format!("the static {:?}: {error}", it.item))?;
    Ok((it.item, it.instance, ContentId::of(STATIC_DOMAIN, &scratch)))
}

/// A function by its instance key and the hash of all of it as this build filled it
/// ([`filled_id`]).
pub(crate) type FunctionRow = (InstanceKey, ContentId);

const FUNCTION_DOMAIN: &str = "blaze v2 link function 1";

/// The hash of all of `function` as this build filled it: its key, what it does
/// ([`blaze_codegen::content::code_id`]: its architecture, alignment, code, relocations as
/// filled, its data by content ID, which covers each datum's bytes and relocations as filled,
/// unwind information, lines, coverage and profile), what it inlined and its side table.
/// Destructured, so a field added to [`blaze_codegen::CompiledFunction`] is a compile error
/// here until it is hashed (or `code_id` is checked to cover it).
fn filled_id(function: &blaze_codegen::CompiledFunction) -> ContentId {
    let blaze_codegen::CompiledFunction {
        key,
        // In `code_id`.
        arch: _,
        align: _,
        code: _,
        relocs: _,
        data: _,
        unwind: _,
        lines: _,
        coverage: _,
        profile: _,
        // Not in `code_id`, which is what the code does.
        inlined,
        side,
    } = function;
    let mut hasher = blaze_ids::Hasher::new(FUNCTION_DOMAIN);
    hasher.id(&key.0).id(&blaze_codegen::content::code_id(function));
    hasher.update(&(inlined.len() as u64).to_le_bytes());
    for it in inlined {
        hasher.id(&it.0);
    }
    match side {
        Some(side) => hasher.update(&[1]).id(side),
        None => hasher.update(&[0]),
    };
    hasher.finish()
}

/// Each of `functions` as a row, sorted, and the keys of what of `old`'s rows is gone and what
/// is new. A function's key is its code and settings, not which callee is at each of its
/// places (`blaze_codegen::driver::placed`): an edit to a callee gives the callee a new key
/// and fills its callers' relocations with it under the callers' old keys. So the row covers
/// the whole function as filled, and a caller whose fill changed is in both lists: blaze-link
/// removes its old piece, which still refers to the callee the change removes, and adds the
/// new one (blaze-link's `Stage::apply` removes first). A function put again as it was is in
/// neither. Two fills of one key in one build are refused (rule 21), naming the function by
/// its entry in `debug_names` ([`Built::debug_names`]).
pub(crate) fn function_rows(
    old: &[FunctionRow],
    functions: &[Arc<blaze_codegen::CompiledFunction>],
    debug_names: &[(InstanceKey, String)],
) -> Result<(Vec<FunctionRow>, Vec<InstanceKey>, Vec<InstanceKey>), String> {
    let span = tracing::info_span!(
        target: "blaze",
        "link function rows",
        functions = functions.len(),
        gone = tracing::field::Empty,
        new = tracing::field::Empty,
    )
    .entered();
    // alloc: the image's column, kept in `Linked` until its next link
    let mut rows = blaze_graph::par::map(functions, |it| (it.key, filled_id(it)));
    rows.sort_unstable();
    if let Some(pair) = rows.windows(2).find(|it| it[0].0 == it[1].0 && it[0].1 != it[1].1) {
        let key = pair[0].0;
        let name =
            debug_names.iter().find(|it| it.0 == key).map_or("(no debug name)", |it| it.1.as_str());
        // alloc: error path
        return Err(format!(
            "the function `{name}` ({key:?}) is filled two ways in one build ({:?} and {:?}): one key is one function",
            pair[0].1, pair[1].1
        ));
    }
    // alloc: the change's lists, once per link: `removed` goes into blaze-link's `Change`
    let (mut gone, mut new) = (Vec::new(), Vec::new());
    diff(old, &rows, &mut gone, &mut new);
    span.record("gone", gone.len());
    span.record("new", new.len());
    let keys = |rows: Vec<FunctionRow>| {
        // alloc: the change's lists, once per link, each key once
        let mut keys: Vec<InstanceKey> = rows.into_iter().map(|it| it.0).collect();
        keys.dedup();
        keys
    };
    Ok((rows, keys(gone), keys(new)))
}

/// The domain an input blaze ships ([`blaze_link::target::Located::bytes`]) is named in.
const SHIPPED_NATIVE: &str = "blaze v2 link shipped native input 1";

/// [`link`]'s change of `linked` to what `generated` built, with the crates' `global_asm!`
/// objects `assembled` and the native inputs `located`, laid out by `heat` where the image is
/// laid out whole, and what was done to the image ([`Laid`]).
/// Its columns are written only once the image has taken the change, so after an error they
/// still say what the image holds.
#[expect(clippy::too_many_arguments, reason = "the link's inputs, each its own")]
fn relink(
    job: &Job,
    unit: usize,
    path: &str,
    generated: &Generated<'_>,
    test: bool,
    assembled: &[GlobalAsmObject],
    located: &[blaze_link::target::Located],
    side: &LinkInputs,
    heat: Option<&blaze_link::Heat>,
    linked: &mut Linked,
) -> Result<
    (Filed, Vec<Read>, Option<blaze_codegen::ExecTable>, Laid, super::run::inproc::Told),
    LinkCause,
> {
    let built = generated.built;
    // The copy a run starts a dev executable from when it is not `path` (cargo's `executable`,
    // [`super::artifact::executable`]): its program is held under it too. Worked out before
    // anything of `linked` is taken, so its error leaves the kept image whole.
    let runs_from = match linked.output.dev_run {
        Some(_) => {
            let plan = job.plan(unit).map_err(|it| LinkCause::Given(it.what))?;
            super::artifact::uplifted(path, plan)
        }
        None => None,
    };
    // What of the image this build changes: its natives and names against the columns the
    // image keeps, and its function rows, static rows and span tables by the build's delta
    // (`rows` hashed, `hashed` bytes of code and digest; `whole` says why every row was made
    // again, when the image's last link was not of the build's base).
    let delta = tracing::info_span!(
        target: "blaze",
        "link delta",
        unit = %path,
        gone = tracing::field::Empty,
        new = tracing::field::Empty,
        rows = tracing::field::Empty,
        hashed = tracing::field::Empty,
        whole = tracing::field::Empty,
    )
    .entered();
    // Each native input by the hash of its contents, through the process's table of files by
    // their `stat`: one `stat` for a library the image already has, read only when it is new.
    // A look at one is the build's own ([`crate::serve::work::JobInput::looks`]).
    let mut files = blaze_usercode::inputs::Disk::new(Vec::new(), &job.input.looks);
    let mut read = Vec::with_capacity(located.len());
    // The `global_asm!` objects first, which are not read off the machine, then `located`.
    let mut native_ids = Vec::with_capacity(assembled.len() + located.len());
    native_ids.extend(assembled.iter().map(|it| it.id));
    if assembled.len() >= FIRST_LIBRARY as usize {
        // alloc: error path
        return Err(LinkCause::Given(format!(
            "{} crates with `global_asm!` linked into one image, more than the {FIRST_LIBRARY} places \
             before the first library",
            assembled.len()
        )));
    }
    for it in located {
        // An input blaze ships is its bytes, the same on every machine: hashed here, and no
        // file the build read.
        if let Some(bytes) = &it.bytes {
            native_ids.push(ContentId::of(SHIPPED_NATIVE, bytes));
            continue;
        }
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
    for (at, it) in assembled.iter().enumerate() {
        if added_natives.binary_search(&it.id).is_ok() {
            // clone: an `Arc`, the assembler's object shared with the image
            bytes_of.push((at, it.bytes.clone()));
        }
    }
    for (order, it) in located.iter().enumerate() {
        let order = assembled.len() + order;
        if added_natives.binary_search(&native_ids[order]).is_ok() {
            if let Some(bytes) = &it.bytes {
                // clone: an `Arc`, the shipped input shared with the image
                bytes_of.push((order, Arc::clone(bytes)));
                continue;
            }
            let bytes = std::fs::read(&it.path).map_err(|error| {
                // alloc: error path
                LinkCause::Native(blaze_usercode::inputs::Unreadable {
                    path: it.path.clone(),
                    error: error.to_string(),
                })
            })?;
            // alloc: once per native input new to the image, which keeps it shared from here.
            bytes_of.push((order, Arc::<[u8]>::from(bytes)));
        }
    }

    // alloc: per link, the functions reached through slots: a workspace's, by key
    let mut patchable: Vec<InstanceKey> = built.patchable.iter().copied().collect();
    patchable.sort_unstable();
    let mut names: Vec<(String, Target)> =
        Vec::with_capacity(built.symbols.len() + built.static_symbols.len());
    // The names Rust defines weakly (`#[linkage = "weak"]`, as compiler-builtins' are): a
    // strong definition beside them wins the link, as it would under ld64 or lld.
    let mut weak_names: Vec<String> = Vec::new(); // alloc: per link, the few weak names
    for (key, name, linkage) in &built.symbols {
        let target = match patchable.binary_search(key).is_ok() {
            true => Target::Function(*key),
            false => Target::Direct(*key),
        };
        if *linkage == blaze_codegen::Linkage::Weak {
            weak_names.push(name.clone()); // clone: the few weak names, once per link
        }
        names.push((name.clone(), target));
    }
    for (item, instance, name, linkage) in &built.static_symbols {
        if *linkage == blaze_codegen::Linkage::Weak {
            weak_names.push(name.clone()); // clone: the few weak names, once per link
        }
        names.push((name.clone(), Target::Static { item: *item, instance: *instance }));
    }
    weak_names.sort_unstable();
    let (slots, slotted) = match linked.output.live {
        Some(_) => edited_slots(&linked.slotted, &patchable, &built.identities, &built.debug_names)
            // alloc: error path
            .map_err(|what| LinkCause::Given(format!("{path}: {what}")))?,
        None => (Vec::new(), Vec::new()),
    };
    // The rows: brought to this build by its delta where the image's last link was of the
    // delta's base, so only what the build changed is hashed; else made again from the
    // build's whole lists, and the span says why. Nothing of `linked` but its rows is written
    // from here to the stage, and a stage the image refuses takes the rows back (`undo`).
    let spans = match linked.rows.spans_of(side) {
        Some(spans) => spans,
        // The image holds neither the tables the front end kept nor their base (its first
        // link beside another image's, or one after a link that failed): it takes them all.
        None => kept::Spans::All {
            serial: side.serial,
            // alloc: per link given every table, a row a table
            tables: generated.spans()?.iter().map(|(anchor, it)| (*anchor, None, &it[..])).collect(),
        },
    };
    // alloc: per link, the span tables the linker is given: those that changed
    let given_spans: Vec<(ContentId, &[u8])> = match &spans {
        kept::Spans::Same => Vec::new(),
        kept::Spans::Changed(inputs) => {
            // The front end's tables outlive `spans`, which the rows take.
            let inputs: &LinkInputs = *inputs;
            inputs.tables.iter().map(|it| (it.anchor, &it.bytes[..])).collect()
        }
        kept::Spans::All { tables, .. } => tables.iter().map(|it| (it.0, it.2)).collect(),
    };
    let takes = linked.rows.takes(built);
    let mut work = kept::Work::default();
    let kept::Applied { removed, added: added_keys, gone_statics, new_statics, spans: _, undo } =
        linked.rows.apply(built, spans, &mut work)?;
    delta.record("gone", removed.len() + gone_statics.len());
    delta.record("new", added_keys.len() + new_statics.len());
    delta.record("rows", work.rows);
    delta.record("hashed", work.row_bytes + work.digest_bytes);
    if let Some(why) = work.whole {
        delta.record("whole", why);
    }
    // What the link touched, kept with the build (`blaze explain edit_work`). `rows` and
    // `tables` follow the edit. `symbols` and `patchable` are every name and every function
    // reached through a slot of the program: this link goes over each of them once (the
    // names are copied out of the `Built` and compared with the last link's), which is the
    // part of a link that still grows with the program and not with the edit.
    tracing::info!(
        target: "blaze",
        stage = blaze_query::db::edit_work::LINK,
        rows = work.rows as u64,
        tables = work.tables as u64,
        symbols = names.len() as u64,
        patchable = patchable.len() as u64,
        whole = u64::from(work.whole.is_some()),
        "edit work"
    );
    let mut now_named: Vec<&str> = names.iter().map(|it| it.0.as_str()).collect();
    let (kept_names, unnamed) = renamed(std::mem::take(&mut linked.names), &mut now_named);
    let given = linked.rows.given(built.id, &native_ids);
    // Nothing to link: every row kept, the same names, natives and span tables, and a file
    // patched in place (no code memory, no slots). Staging it would make a new generation
    // whose file differs only in its build id, so an image whose inputs did not change (a
    // doctest example beside an edited one) keeps the file it has, byte for byte.
    let unchanged = linked.given == Some(given)
        && linked.output.incremental
        && linked.output.dev_run.is_none()
        && linked.output.live.is_none()
        && removed.is_empty()
        && added_keys.is_empty()
        && gone_statics.is_empty()
        && new_statics.is_empty()
        && removed_natives.is_empty()
        && added_natives.is_empty()
        && slots.is_empty()
        && unnamed.is_empty();
    if unchanged {
        let _span = tracing::info_span!(target: "blaze", "link unchanged", unit = %path).entered();
        linked.names = kept_names;
        let table = tests_table(test, path, generated, linked)?;
        let told = super::run::inproc::Told::Nothing;
        return Ok((Filed::Unchanged, read, table, Laid::Nothing, told));
    }
    // clone: the names gone, a few a link, which an image that refuses the change still has
    let gone_names = unnamed.clone();
    // What the change gives the linker of the build's lists, each searched by key with its
    // order checked: a search that proves nothing is the link's error, and the image, which
    // was not asked to take the change, is as it was.
    let whole = takes.is_err() || !linked.output.incremental;
    let (debug_names, cold, added, statics) =
        match lists(built, takes, whole, &added_keys, &new_statics) {
            Ok(lists) => lists,
            Err(what) => {
                untaken(linked, kept_names, gone_names, undo);
                // alloc: error path
                return Err(LinkCause::Given(format!("{path}: {what}")));
            }
        };
    let change = Change {
        added,
        removed,
        // A whole build tiers nothing up: the slots that run another function than their own
        // are an edited function's earlier keys', in a live image.
        slots,
        statics,
        removed_statics: gone_statics,
        names,
        weak_names,
        unnamed,
        debug_names,
        // What the image holds with this change: in its build id.
        given: Some(given),
        natives: bytes_of
            .iter()
            .map(|(at, bytes)| match assembled.get(*at) {
                Some(object) => Native {
                    id: object.id,
                    name: &object.name,
                    bytes,
                    kind: NativeKind::Object,
                    order: *at as u32,
                },
                None => {
                    let index = *at - assembled.len();
                    let it = &located[index];
                    Native {
                        id: native_ids[*at],
                        name: &it.name,
                        bytes,
                        kind: it.kind,
                        order: FIRST_LIBRARY + index as u32,
                    }
                }
            })
            .collect(),
        removed_natives,
        spans: given_spans,
        heat,
        cold,
        // How each function was made, sorted by key: what the image's code is summed by.
        made: &built.made,
    };
    // What a dev executable's running code assumes, and what of it this link changes: a
    // program running the earlier link is told to start again, naming each change.
    let (assumed, refused) = match linked.output.dev_run {
        Some(_) => {
            let assumed = live_assumed(job, unit, &generated.frontend);
            let refused = changed_assumptions(&linked.assumed, &assumed, &generated.frontend);
            (assumed, refused)
        }
        None => (Vec::new(), None),
    };
    drop(delta);
    // The stage and its commit: what the change places and writes. A link placed into the
    // room the image kept is taken as a patch ([`Image::patch`]), so the span says what a
    // writer of the file in place would write (`written` bytes in `ranges`), and the file's
    // identity is kept by the blocks written (docs/v2/plan/design/patch-in-place.md).
    let patching = tracing::info_span!(
        target: "blaze",
        "link patch",
        unit = %path,
        full = tracing::field::Empty,
        written = tracing::field::Empty,
        ranges = tracing::field::Empty,
    )
    .entered();
    let made = linked
        .image
        .stage(change)
        .map_err(|error| LinkCause::Linker { step: LinkStep::Stage, error })
        .and_then(|staged| {
            let laid = match staged.full() {
                true => Laid::Whole,
                false => Laid::Patched,
            };
            match linked.output.dev_run {
                // A dev executable's file is its code memory's image part, which its runs map.
                Some(dev_run) => super::run::inproc::commit(
                    &job.shared.dev_runs,
                    path,
                    runs_from,
                    &mut linked.image,
                    &mut linked.loaded,
                    staged,
                    dev_run.cells,
                    refused.as_deref(),
                )
                // What the commit wrote of the file, as a link with no memory gives it: the
                // store keeps the file as the one before it with those ranges written, and
                // its bytes are read (from the image) only where the store cannot
                // ([`Filed::written`]).
                .map(|(patch, told)| {
                    patching.record("full", patch.full);
                    patching.record("written", patch.written());
                    patching.record("ranges", patch.writes.len());
                    (Filed::Patch(patch), laid, told)
                })
                // alloc: error path
                .map_err(|what| LinkCause::Given(format!("{path}: {what}"))),
                None => linked
                    .image
                    .patch(staged)
                    .map(|patch| {
                        patching.record("full", patch.full);
                        patching.record("written", patch.written());
                        patching.record("ranges", patch.writes.len());
                        // The patch goes on as it is: the whole file is made only where the
                        // store is given it whole ([`Filed::written`]).
                        (Filed::Patch(patch), laid, super::run::inproc::Told::Nothing)
                    })
                    .map_err(|error| LinkCause::Linker { step: LinkStep::Bytes, error }),
            }
        });
    drop(patching);
    let (filed, laid, told) = match made {
        Ok(made) => made,
        Err(cause) => {
            untaken(linked, kept_names, gone_names, undo);
            return Err(cause);
        }
    };
    linked.natives = new_natives;
    linked.names = kept_names;
    linked.slotted = slotted;
    linked.assumed = assumed;
    linked.given = Some(given);
    let table = {
        let _span = tracing::info_span!(target: "blaze", "link tables", unit = %path).entered();
        tests_table(test, path, generated, linked)?
    };
    Ok((filed, read, table, laid, told))
}

/// The file a link left, as [`relink`] has it.
enum Filed {
    /// The file the image has: the link changed nothing of it.
    Unchanged,
    /// What the commit changed of the file before it ([`Image::patch`]): the file itself is
    /// never made.
    Patch(blaze_link::Patch),
}

impl Filed {
    /// What the store is given of the file of digest `root` that `image` was just committed
    /// to at `path`. A patch of the file before goes as the patch where the store can derive
    /// from that file ([`Written::linked`]). Everything else goes as the runs of the file
    /// that hold bytes ([`Written::Runs`]): a file written whole is its patch's writes, moved,
    /// and a file the store cannot derive (or one the link left as it was) is read from the
    /// image's state run by run ([`Image::file_runs`]), which the `link copy` span says. No
    /// path makes the file at its length: an image's patch room (64 MiB each of code and
    /// data, at least) is in no run, so it is neither allocated, copied, hashed nor put.
    /// Only a file the daemon reads back by its bytes (`read_back`: a proc macro's wasm
    /// module, which keeps no room) is given whole.
    fn written(
        self,
        store: &blaze_cas::Reader,
        image: &Image,
        root: [u8; 32],
        path: &str,
        read_back: bool,
    ) -> Result<Written, blaze_link::LinkError> {
        if read_back {
            let span = tracing::info_span!(
                target: "blaze",
                "link copy",
                unit = %path,
                bytes = tracing::field::Empty,
            )
            .entered();
            let bytes = image.file()?;
            span.record("bytes", bytes.len());
            return Ok(Written::Linked(bytes, root));
        }
        let runs = || -> Result<Written, blaze_link::LinkError> {
            let span = tracing::info_span!(
                target: "blaze",
                "link copy",
                unit = %path,
                bytes = tracing::field::Empty,
            )
            .entered();
            let (len, runs) = image.file_runs()?;
            span.record("bytes", runs.iter().map(|it| it.1.len() as u64).sum::<u64>());
            Ok(Written::Runs { root, len, runs })
        };
        match self {
            Filed::Unchanged => runs(),
            Filed::Patch(patch) if patch.full => {
                Ok(Written::Runs { root, len: patch.len, runs: patch.writes })
            }
            Filed::Patch(patch) => Written::linked(store, root, Some(patch), runs),
        }
    }
}

/// Puts `linked` back as it was before a change its image did not take: every name the image
/// may still hold (one it never took is unnamed harmlessly), and its rows, which the next
/// link's delta is against.
fn untaken(
    linked: &mut Linked,
    kept_names: Vec<String>,
    gone_names: Vec<String>,
    undo: kept::Undo,
) {
    linked.names = kept_names;
    linked.names.extend(gone_names);
    linked.names.sort_unstable();
    linked.rows.undo(undo);
}

/// What a change gives the linker of the build's lists: the debug names of the functions the
/// build's delta lists, the cold functions a layout reads, the functions and the statics the
/// change adds.
type Lists<'a> = (
    Vec<(InstanceKey, String)>,
    Vec<InstanceKey>,
    Vec<&'a blaze_codegen::CompiledFunction>,
    Vec<&'a blaze_codegen::Static>,
);

/// [`Lists`] of `built` for a change that adds the functions `added` and the statics
/// `new_statics`, each found by a search of its list whose key order is checked
/// ([`kept::Listed`]): a list out of order is an error naming it and the key, and so is a
/// function or a static the rows hold that the build's list lacks. `takes` is the rows'
/// ([`kept::Rows::takes`]); `whole` says the image is laid out whole.
fn lists<'a>(
    built: &'a Built,
    takes: Result<bool, &'static str>,
    whole: bool,
    added: &[InstanceKey],
    new_statics: &[blaze_codegen::StaticId],
) -> Result<Lists<'a>, String> {
    // The names for debuggers of what the delta lists: the image holds the rest.
    let debug_names: Vec<(InstanceKey, String)> = match takes {
        // alloc: none, empty: the image was given this build's names
        Ok(true) => Vec::new(),
        Ok(false) => {
            let names = kept::Listed::new(&built.debug_names, "debug_names", |it| it.0);
            let functions = &built.delta.functions;
            // alloc: per link, the names of the functions the build changed
            let mut out = Vec::with_capacity(functions.added.len() + functions.changed.len());
            for key in functions.added.iter().chain(&functions.changed) {
                if let Some(named) = names.find(key)? {
                    // clone: per link, the names of the functions the build changed
                    out.push(named.clone());
                }
            }
            out
        }
        // clone: once per image linked from the build's whole lists
        Err(_) => built.debug_names.clone(),
    };
    // What a layout reads of the cold functions: all of them where the image is laid out
    // whole (its first link, or an output that keeps no room), else those of the functions
    // this link adds, which are all it places.
    let cold: Vec<InstanceKey> = match whole {
        // clone: once per image laid out whole, the functions codegen inferred cold
        true => built.cold.clone(),
        false => {
            let colds = kept::Listed::new(&built.cold, "cold", |it| *it);
            let mut out = Vec::new(); // alloc: per link, the few cold functions it adds
            for key in added {
                if colds.find(key)?.is_some() {
                    out.push(*key);
                }
            }
            out
        }
    };
    let functions = kept::functions_of(built);
    let statics = kept::statics_of(built);
    Ok((
        debug_names,
        cold,
        added.iter().map(|key| functions.need(key).map(|it| &**it)).collect::<Result<_, _>>()?,
        new_statics.iter().map(|id| statics.need(id)).collect::<Result<_, _>>()?,
    ))
}

/// A test binary's table, by which its tests are selected and which only its link numbers;
/// none for another image.
fn tests_table(
    test: bool,
    path: &str,
    generated: &Generated<'_>,
    linked: &Linked,
) -> Result<Option<blaze_codegen::ExecTable>, String> {
    if !test {
        return Ok(None);
    }
    let image = &linked.image;
    // The jump table as the binary's runtime will name it (its descriptor's lineage and
    // generation, its header's slot count: the capacity), and the slots given out of it.
    let jump = blaze_codegen::JumpTable {
        lineage: image.lineage(),
        generation: image.generation(),
        capacity: image.capacity(),
        given: image.slots(),
    };
    let table = super::test::image::table(
        path,
        generated.built,
        // Every span table, made whole: a test binary's table reads them all.
        generated.spans()?,
        jump,
        |key| image.slot(key).map(|it| it.0),
        ContentId::default(),
        &generated.roots.tests,
    )?;
    Ok(Some(table))
}

/// What the code `unit` links of its workspace crates assumes of their items, by key, sorted
/// ([`HirFrontend::live_assumed`]).
fn live_assumed(job: &Job, unit: usize, frontend: &HirFrontend<'_>) -> Vec<(ItemKey, Assumed)> {
    // alloc: per live link, kept by the image
    let mut rows = Vec::new();
    for at in job.graph.linked_in(unit as u32) {
        if let Some(krate) = job.crates.krate(at as usize) {
            rows.extend(frontend.live_assumed(krate));
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// Why a program running a dev executable's earlier link can't take this one as a patch: the
/// functions whose signature and the types whose layout changed, named (the first few, and
/// how many more); `None` when nothing it assumed did. Code built before the edit (a frame
/// still running, a value it made) would meet code built after it. An item added or removed
/// changes nothing the running code assumed: its old code never named a new one, and a
/// removed function's slot keeps its code until another fills it.
fn changed_assumptions(
    old: &[(ItemKey, Assumed)],
    new: &[(ItemKey, Assumed)],
    frontend: &HirFrontend<'_>,
) -> Option<String> {
    use std::fmt::Write as _;
    /// How many changes the reason names.
    const NAMED: usize = 3;
    // alloc: per live link with a change, the changed items
    let mut changed: Vec<(ItemKey, Assumed)> = Vec::new();
    let (mut a, mut b) = (0, 0);
    while a < old.len() && b < new.len() {
        match old[a].0.cmp(&new[b].0) {
            std::cmp::Ordering::Less => a += 1,
            std::cmp::Ordering::Greater => b += 1,
            std::cmp::Ordering::Equal => {
                if old[a].1 != new[b].1 {
                    changed.push(new[b]);
                }
                a += 1;
                b += 1;
            }
        }
    }
    if changed.is_empty() {
        return None;
    }
    // alloc: a restart's reason, once per refused link
    let mut why = String::new();
    for (at, (key, now)) in changed.iter().take(NAMED).enumerate() {
        if at > 0 {
            why.push_str("; ");
        }
        let path = frontend.item_path(*key);
        let _ = match now {
            Assumed::Signature(_) => {
                write!(
                    why,
                    "the signature of `{path}` changed: code built before the edit calls it as it was"
                )
            }
            Assumed::Layout(_) => {
                write!(
                    why,
                    "the layout of `{path}` changed: values made before the edit are laid out as it was"
                )
            }
            Assumed::Unhashable(_) => {
                write!(why, "`{path}` changed, and blaze can't hash what code assumes of it")
            }
        };
    }
    if changed.len() > NAMED {
        let _ = write!(why, "; and {} more", changed.len() - NAMED);
    }
    Some(why)
}

/// The slots of a live image that run an edited function's new code, as `(slot, function)`,
/// and the image's slots by identity after this link. An edit of a function's body gives it a
/// new key, and its new callers call that key's slot; code built before the edit (a frame
/// still running, a caller whose code did not change) calls the slot of a key it had before,
/// which has to run the new code too, or the edit never reaches it. `slotted` holds, for each
/// identity (what a function is across edits of its body, told apart from instances side
/// by side, `Built::identities`, `blaze_codegen::driver::subject`),
/// every key the image made a slot for; `patchable` (sorted) is this build's functions
/// reached through slots. A function gone from the build leaves its keys out: its slots are
/// emptied with its code, as the linker does for any function removed.
pub(crate) fn edited_slots(
    slotted: &[(ContentId, InstanceKey)],
    patchable: &[InstanceKey],
    identities: &[(InstanceKey, ContentId)],
    debug_names: &[(InstanceKey, String)],
) -> Result<(Vec<(InstanceKey, InstanceKey)>, Vec<(ContentId, InstanceKey)>), String> {
    // alloc: once per live link, a pair per patchable function, kept by the image
    let mut now: Vec<(ContentId, InstanceKey)> = Vec::with_capacity(patchable.len());
    for key in patchable {
        // The walk's instances are first, in key order (`driver.rs`), then each folded
        // group's body: a patchable function is found by search among the first, and looked
        // for through the list only if the search misses it.
        let found = match identities.binary_search_by(|it| it.0.cmp(key)) {
            Ok(at) => Some(identities[at].1),
            Err(_) => identities.iter().find(|it| it.0 == *key).map(|it| it.1),
        };
        if let Some(id) = found {
            now.push((id, *key));
        }
    }
    now.sort_unstable();
    now.dedup();
    // Two functions one across edits: which of them an earlier key's slot runs is not
    // decidable (rule 21), never a guess.
    if let Some(pair) = now.windows(2).find(|it| it[0].0 == it[1].0) {
        let name = |key: InstanceKey| {
            debug_names.iter().find(|it| it.0 == key).map_or("(no debug name)", |it| it.1.as_str())
        };
        // alloc: error path
        return Err(format!(
            "`{}` ({:?}) and `{}` ({:?}) are one function across edits (identity {:?}): the live image can't \
             say which of them the slots of its earlier keys run",
            name(pair[0].1),
            pair[0].1,
            name(pair[1].1),
            pair[1].1,
            pair[0].0
        ));
    }
    // alloc: once per live link, a pair per slot an edited function's earlier key has
    let mut slots = Vec::new();
    // alloc: once per live link, kept by the image
    let mut kept = Vec::with_capacity(now.len() + slotted.len());
    for &(id, key) in &now {
        let from = slotted.partition_point(|it| it.0 < id);
        for &(_, old) in slotted[from..].iter().take_while(|it| it.0 == id) {
            if old != key {
                slots.push((old, key));
                kept.push((id, old));
            }
        }
        kept.push((id, key));
    }
    kept.sort_unstable();
    kept.dedup();
    slots.sort_unstable();
    Ok((slots, kept))
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
        "link-arg" => {
            return Err(format!("`-l {value}`: blaze's linker takes no raw link argument"));
        }
        other => return Err(format!("`-l {value}`: unknown library kind `{other}`")),
    };
    // `NAME:RENAME` links `RENAME` where the crate's `#[link]` says `NAME`.
    let name = name.split_once(':').map_or(name, |it| it.1);
    if name.is_empty() {
        return Err(format!("`-l {value}` names no library"));
    }
    Ok(NativeLib { name: name.to_owned(), kind, verbatim })
}


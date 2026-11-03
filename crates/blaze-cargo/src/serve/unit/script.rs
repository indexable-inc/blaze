//! A build script's run ([`Front::script`](crate::serve::work::Front::script)), as cargo runs
//! it and reads what it printed (`cargo/src/cargo/core/compiler/custom_build.rs`), in the
//! sandbox: what it is given (the job's whole environment, cargo's variables over it, the
//! `DEP_<links>_<KEY>` of the runs it depends on), its result kept in the store by what it read
//! ([`blaze_usercode::run`]), a script that provably prints only constants not run at all
//! ([`blaze_usercode::script::inert`]), and what it printed applied as cargo applies it: its
//! cfgs and env to its package's units through the job's writer ([`Writer::ran`]), its link
//! libraries and search paths to what links them, its metadata to its dependents' scripts.
//!
//! A run outside the sandbox is one `BLAZE_SANDBOX=off` or the workspace's
//! `unsandboxed-build-scripts` (a member's `build-script = false`) takes out: it runs every
//! time, and nothing of it is kept.
//!
//! cargo's files of a run (`output`, `stderr`, `root-output`) are written through the loop's
//! effects, never on its thread, and `output` is dated when the run began, as cargo dates it.
//! `BLAZE_ELIDE=verify` compiles and runs an inert script as well, and fails the build where
//! the run differs from what elision said.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use blaze_api::{ApiError, BuildScriptRan, Diagnostic, Level, Response, UnitOf};
use blaze_build_script::{
    output::{Context, Output, Severity},
    unit as bs,
};
use blaze_cas::Store;
use blaze_core::{Command, Completion, Done, Effect, FileName, Names, Sandbox, Stamp, Tag};
use blaze_units::{
    query::{UnitPlan, Units},
    schema::{DebugInfo, Mode, Unit},
    stdlib,
};
use blaze_usercode::{
    roots::{Role, RootMap, Rooted},
    run::{Begun, OutFile, Run, Value},
    script::{self, Place, Prepared},
};

use super::{
    Job, Real, UnitWriter,
    error::{UnitError, manifest_dir},
    package_of, say,
};
use crate::serve::work::{RunPuts, Runs, Writer};

/// The job's variable that asks an inert script compiled and run as well, and checked against
/// what elision said of it: `verify`, or unset.
pub(crate) const ELIDE: &str = "BLAZE_ELIDE";

/// Whether the job asks `BLAZE_ELIDE=verify`; any other value of it is an error naming it.
pub(crate) fn verifies(job: &Job) -> Result<bool, ApiError> {
    verifies_in(&job.input.config.invocation.env)
}

fn verifies_in(env: &[(String, String)]) -> Result<bool, ApiError> {
    match env.iter().find(|(name, _)| name == ELIDE) {
        None => Ok(false),
        Some((_, value)) if value == "verify" => Ok(true),
        Some((_, value)) => Err(ApiError::new(
            format!("{ELIDE}={value} is not a value blaze takes: `verify` runs an inert build script as well, unset runs none"),
            None,
        )),
    }
}

/// The job variable that says how a build script's kept run is used: `check` runs it again
/// and compares ([`blaze_usercode::run::Mode::Verify`]), or unset.
pub(crate) const SCRIPT_CACHE: &str = "BLAZE_SCRIPT_CACHE";

/// Whether the job asks `BLAZE_SCRIPT_CACHE=check`; any other value of it is an error naming
/// it.
pub(crate) fn rechecks(job: &Job) -> Result<bool, ApiError> {
    rechecks_in(&job.input.config.invocation.env)
}

fn rechecks_in(env: &[(String, String)]) -> Result<bool, ApiError> {
    match env.iter().find(|(name, _)| name == SCRIPT_CACHE) {
        None => Ok(false),
        Some((_, value)) if value == "check" => Ok(true),
        Some((_, value)) => Err(ApiError::new(
            format!(
                "{SCRIPT_CACHE}={value} is not a value blaze takes: `check` runs a build script the store kept \
                 again and compares, unset uses what was kept"
            ),
            None,
        )),
    }
}

/// What a build script's run told cargo, for what depends on it.
#[derive(Debug)]
pub(crate) struct Applied {
    /// The run's package.
    pub(crate) package_id: String,
    pub(crate) links: Option<String>,
    pub(crate) output: Output,
    pub(crate) out_dir: String,
}

/// The unit that compiles run `run`'s script: its dependency that builds the custom-build
/// target.
pub(crate) fn compile_of(units: &Units, run: usize) -> Option<usize> {
    let unit = units.graph.units.get(run)?;
    unit.dependencies.iter().map(|it| it.index).find(|at| {
        units.graph.units.get(*at).is_some_and(|it| it.target.is_custom_build() && !matches!(it.mode, Mode::RunCustomBuild))
    })
}

/// What a script that provably prints only constants prints, read from its source through the
/// VFS, or what blaze's port of one of std's own scripts says it prints ([`std_printed`]): it
/// is then neither compiled nor run. `unit` is the script's compile or its run. Worked out once
/// per job and script: its compile and its run both ask.
pub(crate) fn inert(job: &Job, unit: usize) -> Result<Option<&Value>, ApiError> {
    let units = job.units();
    let Some(u) = units.graph.units.get(unit) else { return Ok(None) };
    let compile = match u.mode {
        Mode::RunCustomBuild => match compile_of(units, unit) {
            Some(it) => it,
            None => return Ok(None),
        },
        _ => unit,
    };
    let Some(slot) = job.inert.get(compile) else { return Ok(None) };
    match slot.get_or_init(|| work_out_inert(job, compile)) {
        Ok(value) => Ok(value.as_ref()),
        // clone: the error the build stops with, once per unit that asks
        Err(what) => Err(ApiError::new(what.clone(), None)),
    }
}

/// Whether `unit` is the compile or the run of one of std's own build scripts: never compiled,
/// so neither `BLAZE_ELIDE=verify` nor `BLAZE_PURE=verify` can run it to check its port.
pub(crate) fn ported(job: &Job, unit: usize) -> bool {
    job.units().graph.units.get(unit).is_some_and(|it| it.is_std && it.target.is_custom_build())
}

fn work_out_inert(job: &Job, compile: usize) -> Result<Option<Value>, String> {
    if ported(job, compile) {
        return std_printed(job, compile).map(Some);
    }
    let units = job.units();
    let Some(u) = units.graph.units.get(compile) else { return Ok(None) };
    let root = u.target.src_path.to_string_lossy().into_owned();
    // A root file whose bytes fail their check stopped the job as it was lowered.
    let Ok(Some((_, _, bytes))) = job.mounted.read(&root) else { return Ok(None) };
    let Some(plan) = units.plans.get(compile) else { return Ok(None) };
    let files: BTreeMap<PathBuf, Vec<u8>> = [(PathBuf::from(&root), bytes.to_vec())].into();
    Ok(script::inert(Path::new(&root), &files, &plan.rustflags))
}

/// What std's own build script compiled by unit `compile` prints, from blaze's port of it
/// ([`blaze_units::stdlib::scripts`]), checked to be the script the port is of. A std script
/// can't be compiled (no host std is there to build it against before these scripts ran), so
/// its run is this. None of them reads a `DEP_*` variable, so this is known before the runs
/// it depends on end.
fn std_printed(job: &Job, compile: usize) -> Result<Value, String> {
    let units = job.units();
    let u = units.graph.units.get(compile).ok_or_else(|| format!("the build has no unit {compile}"))?;
    let package = package_of(units, u).map_err(|it| it.what)?;
    let failed = |what: String| format!("std's `{}` build script: {what}", package.name);
    let root = u.target.src_path.to_string_lossy();
    let read = |path: &str| match job.mounted.read(path) {
        Ok(Some((_, _, bytes))) => Ok(bytes),
        Ok(None) => Err(format!("{path} is not among the build's files")),
        Err(error) => Err(format!("{path}: {error}")),
    };
    let which = stdlib::scripts::identify(&package.name, &root, read).map_err(failed)?;
    let mut runs = (0..units.graph.units.len())
        .filter(|at| units.graph.units[*at].mode == Mode::RunCustomBuild && compile_of(units, *at) == Some(compile));
    let (Some(run), None) = (runs.next(), runs.next()) else {
        return Err(failed(format!("unit {compile} compiles it, and the build does not run it exactly once")));
    };
    let (unit, vars) = script_env(job, run).map_err(failed)?;
    let printed = stdlib::scripts::printed(which, &vars, &unit.package.manifest_dir).map_err(failed)?;
    Ok(script::elided(printed))
}

/// Run unit `run` as `blaze_build_script` describes it, its program named and not built, and
/// the whole environment it is started in, sorted by name.
fn script_env(job: &Job, run: usize) -> Result<(bs::Unit, Vec<(OsString, OsString)>), String> {
    let units = job.units();
    let compile = compile_of(units, run).ok_or_else(|| format!("the run of unit {run} has no build script compiled"))?;
    let of = Of {
        units,
        host: &job.input.config.host,
        tools: &job.shared.tools,
        launcher: &job.shared.launcher,
        program: named_program(units, compile).map_err(|it| it.what)?,
        scripts: &job.scripts,
    };
    let unit = script_unit(&of, run)?;
    let env = inherited(&job.input.config.invocation.env, job.plan(run).map_err(|it| it.what)?);
    let vars = blaze_build_script::env(&unit, &env).map_err(|error| error.0)?.vars;
    Ok((unit, vars))
}

/// What a run is made from, beside the job: the build, and what the run's dependencies said.
pub(crate) struct Of<'a> {
    pub(crate) units: &'a Units,
    pub(crate) host: &'a str,
    /// Where blaze's stand-ins for cargo's tools are: `rustc`, `rustdoc`.
    pub(crate) tools: &'a Path,
    pub(crate) launcher: &'a Path,
    /// The compiled script.
    pub(crate) program: PathBuf,
    /// What each run told cargo, by its unit; a slot not set is a run not ended.
    #[expect(clippy::disallowed_types, reason = "the job's slots, one per unit, each set once by its run: per job, not global")]
    pub(crate) scripts: &'a [std::sync::OnceLock<Applied>],
}

fn var<'a>(plan: &'a UnitPlan, name: &str) -> Option<&'a str> {
    plan.env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str())
}

/// The run of build script unit `run`, as `blaze_build_script` describes it.
pub(crate) fn script_unit(of: &Of<'_>, run: usize) -> Result<bs::Unit, String> {
    let units = of.units;
    let unit = units.graph.units.get(run).ok_or_else(|| format!("the build has no unit {run}"))?;
    let plan = units.plans.get(run).ok_or_else(|| format!("the build has no plan of unit {run}"))?;
    let package = package_of(units, unit).map_err(|it| it.what)?;
    let out_dir = plan
        .out_dir
        .clone()
        .ok_or_else(|| format!("the run of `{}`'s build script has no OUT_DIR", package.name))?;
    let manifest_path = PathBuf::from(&package.manifest_path);
    let manifest_dir = manifest_dir(&package.name, &package.manifest_path).map_err(|it| it.to_string())?;
    let targets = targets(units, &unit.pkg_id);
    let text = |name: &str| var(plan, name).filter(|it| !it.is_empty()).map(str::to_owned);
    let descr = match &package.source {
        Some(source) if super::compile::local(Some(source)) => {
            format!("{} v{} {source}", package.name, package.version)
        }
        _ => format!("{} v{}", package.name, package.version),
    };
    let target = triple_of(unit, of.host).to_owned();
    let cfg_args = blaze_units::stdlib::target::CfgArgs::from_flags(&plan.rustflags)?;
    let target_cfgs = blaze_units::stdlib::target::target_cfg(&target, &cfg_args)?;
    let profile = &unit.profile;
    let release = var(plan, "PROFILE")
        .ok_or_else(|| UnitError::NoVariable { package: package.name.clone(), name: "PROFILE" }.to_string())?
        == "release";
    let debuginfo = !matches!(&profile.debuginfo, DebugInfo::Level(0))
        && !matches!(&profile.debuginfo, DebugInfo::Named(name) if name == "none" || name == "false");
    let dependencies = unit
        .dependencies
        .iter()
        .filter_map(|dep| of.scripts.get(dep.index).and_then(|it| it.get()).map(|applied| (dep, applied)))
        .map(|(dep, applied)| {
            let count = units.graph.units.len();
            let it = units
                .graph
                .units
                .get(dep.index)
                .ok_or_else(|| UnitError::Dependency { unit: run, dep: dep.index, count }.to_string())?;
            let package = package_of(units, it).map_err(|it| it.what)?;
            Ok(bs::Dependency::of(&package.name, applied.links.as_deref(), &applied.output))
        })
        .collect::<Result<_, String>>()?;
    // `<profile>/build/<package>-<hash>/out`, or `<profile>/build/<package>/<hash>/out`.
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .find(|it| it.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| UnitError::ProfileDir { package: package.name.clone(), out_dir: out_dir.clone() }.to_string())?;
    let jobs = std::thread::available_parallelism()
        .map_err(|error| UnitError::Parallelism { package: package.name.clone(), error: error.to_string() }.to_string())?
        .get() as u32;
    let exe = |name: &str| of.tools.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    Ok(bs::Unit {
        package: bs::Package {
            name: package.name.clone(),
            version: package.version.clone(),
            descr: Some(descr),
            manifest_dir,
            manifest_path,
            authors: var(plan, "CARGO_PKG_AUTHORS")
                .map(|it| it.split(':').filter(|it| !it.is_empty()).map(str::to_owned).collect())
                .unwrap_or_default(),
            description: text("CARGO_PKG_DESCRIPTION"),
            homepage: text("CARGO_PKG_HOMEPAGE"),
            repository: text("CARGO_PKG_REPOSITORY"),
            license: text("CARGO_PKG_LICENSE"),
            license_file: text("CARGO_PKG_LICENSE_FILE"),
            readme: text("CARGO_PKG_README"),
            rust_version: text("CARGO_PKG_RUST_VERSION"),
            links: plan.links.clone(),
            targets,
        },
        features: unit.features.clone(),
        profile: bs::Profile {
            name: profile.name.clone(),
            release,
            opt_level: profile.opt_level.clone(),
            debuginfo,
            debug_assertions: profile.debug_assertions,
        },
        target,
        host: of.host.to_owned(),
        target_cfgs,
        rustflags: plan.rustflags.clone(),
        jobs,
        script: of.program.clone(),
        out_dir: PathBuf::from(&out_dir),
        tools: bs::Tools {
            cargo: of.launcher.to_path_buf(),
            rustc: exe("rustc"),
            rustdoc: exe("rustdoc"),
            rustc_wrapper: None,
            rustc_workspace_wrapper: None,
            linker: None,
        },
        workspace_member: package.is_member,
        dependencies,
        config_env: Vec::new(),
        dylib_search: bs::DylibSearch {
            host_deps: vec![profile_dir.join("deps")],
            host_root: Some(profile_dir),
            sysroot_libdir: PathBuf::new(),
            plugin_search: Vec::new(),
        },
        jobserver: None,
    })
}

/// The target of `unit`, as `blaze_build_script` names it.
pub(crate) fn target_of(unit: &Unit) -> bs::Target {
    bs::Target {
        name: unit.target.name.clone(),
        kind: unit.target.kind.clone(),
        crate_types: unit.target.crate_types.clone(),
    }
}

/// The targets of package `pkg_id` the build has a unit of, each once, in the graph's order.
pub(crate) fn targets(units: &Units, pkg_id: &str) -> Vec<bs::Target> {
    let mut all = Vec::new();
    for it in units.graph.units.iter().filter(|it| it.pkg_id == pkg_id).map(target_of) {
        if !all.contains(&it) {
            all.push(it);
        }
    }
    all
}

fn triple_of<'a>(unit: &'a Unit, host: &'a str) -> &'a str {
    unit.platform.as_deref().unwrap_or(host)
}

/// The job's variables that say how blaze checks what it kept, never what a run does
/// ([`ELIDE`], [`SCRIPT_CACHE`], [`pure::PURE`], `BLAZE_PROC_MACRO_CACHE`): of the job alone,
/// as [`blaze_usercode::script::RUN_ONLY`] is of the run, so no script sees them. A script
/// that reads its whole environment (`env::vars()`) would otherwise be traced reading
/// `BLAZE_SCRIPT_CACHE=check`, and its check would miss what an unchecked build kept and
/// compare with nothing.
pub(crate) const JOB_ONLY: &[&str] = &[ELIDE, SCRIPT_CACHE, pure::PURE, proc_macro_api::kept::CACHE];

/// The environment a script is started in: the job's whole environment but [`JOB_ONLY`], what
/// cargo sets for the unit over it.
pub(crate) fn inherited(job: &[(String, String)], plan: &UnitPlan) -> Vec<(OsString, OsString)> {
    // cargo's first, then a stable sort by name and the first of each name kept: cargo's over
    // the job's, in one sort with no map.
    let mut env: Vec<&(String, String)> = Vec::with_capacity(plan.env.len() + job.len());
    env.extend(plan.env.iter().chain(job.iter().filter(|it| !JOB_ONLY.contains(&it.0.as_str()))));
    env.sort_by(|a, b| a.0.cmp(&b.0));
    env.dedup_by(|later, first| later.0 == first.0);
    env.into_iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()
}

/// How a run ended, judged as cargo judges it.
#[derive(Debug)]
pub(crate) struct Judged {
    pub(crate) output: Option<Output>,
    /// cargo's lines of it: its warnings, and why it failed.
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// A process's end as an `ExitStatus`, for cargo's text of it.
fn status(code: Option<i32>, signal: Option<i32>) -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        match (code, signal) {
            (_, Some(signal)) => std::process::ExitStatus::from_raw(signal & 0x7f),
            (Some(code), None) => std::process::ExitStatus::from_raw((code & 0xff) << 8),
            (None, None) => std::process::ExitStatus::from_raw(0x7f),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code.unwrap_or(1) as u32)
    }
}

/// What a sandboxed script may print, checked before cargo reads it
/// ([`crate::directives`]); `None` when it may print it all.
pub(crate) fn refused(unit: &bs::Unit, stdout: &[u8], link_dirs: &[PathBuf]) -> Option<String> {
    let ran = crate::directives::Ran {
        package_dir: &unit.package.manifest_dir,
        out_dir: &unit.out_dir,
        link_dirs,
    };
    let refused = crate::directives::refused(stdout, &ran);
    (!refused.is_empty()).then(|| crate::directives::refused_error(&unit.package.name, &refused))
}

/// `value`, a run of `unit` started in `env`, as cargo makes of it: what it printed, once it
/// succeeded, with its warnings; or why the build fails, with the output cargo shows. Read
/// from the bytes alone: cargo's files of the run are the caller's to write ([`ScriptRun`]).
pub(crate) fn judge(
    unit: &bs::Unit,
    env: &[(OsString, OsString)],
    value: &Value,
    checked: Option<&[PathBuf]>,
    of: Option<UnitOf>,
    color: bool,
) -> Judged {
    let cx = Context { nightly_features_allowed: true, ..Context::of(unit, env) };
    let line = |severity: &Severity, message: &str| {
        let level = match severity {
            Severity::Error => Level::Error,
            Severity::Warning => Level::Warning,
        };
        say::cargo(level, format!("{}@{}: {message}", unit.package.name, unit.package.version), of.clone(), color)
    };
    let failure = |error: String| {
        let what = match error.starts_with("failed to run custom build command") {
            true => error,
            false => format!(
                "failed to run custom build command for `{}`\n\nCaused by:\n  {}",
                unit.package.descr(),
                error.lines().collect::<Vec<_>>().join("\n  ")
            ),
        };
        say::cargo(Level::Error, what, of.clone(), color)
    };
    let status = status(value.code, value.signal);
    match blaze_build_script::run::judge(unit, &cx, status, &value.stdout, &value.stderr) {
        Ok(output) => {
            let mut diagnostics: Vec<Diagnostic> =
                output.log_messages.iter().map(|(severity, message)| line(severity, message)).collect();
            if let Some(link_dirs) = checked
                && let Some(why) = refused(unit, &value.stdout, link_dirs)
            {
                diagnostics.push(say::refused(why, of.clone(), color));
                return Judged { output: None, diagnostics };
            }
            Judged { output: Some(output), diagnostics }
        }
        Err(failed) => {
            let mut diagnostics: Vec<Diagnostic> =
                failed.log_messages.iter().map(|(severity, message)| line(severity, message)).collect();
            diagnostics.push(failure(failed.error.0));
            Judged { output: None, diagnostics }
        }
    }
}

/// What cargo's `build-script-executed` says of a run.
pub(crate) fn executed(package_id: &str, output: &Output, out_dir: &str) -> BuildScriptRan {
    BuildScriptRan {
        package_id: package_id.to_owned(),
        linked_libs: output.library_links.clone(),
        linked_paths: output.library_paths.iter().map(|it| it.path().to_string_lossy().into_owned()).collect(),
        cfgs: output.cfgs.clone(),
        env: output.env.clone(),
        out_dir: out_dir.to_owned(),
    }
}

/// The files a run left in its `OUT_DIR`, by `/`-separated path under it, moved out of its
/// value. A directory (no bytes) is no file: it is there on disk, which the run restored.
fn out_files(files: Vec<OutFile>) -> Vec<(String, Vec<u8>)> {
    files.into_iter().filter_map(|file| Some((file.path, file.bytes?))).collect()
}

/// Every file under `dir`, by `/`-separated path: what a run outside the sandbox left.
fn walk(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(rel) = pending.pop() {
        let at = dir.join(&rel);
        let entries = match std::fs::read_dir(&at) {
            Ok(it) => it,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && rel.as_os_str().is_empty() => return Ok(out),
            Err(error) => return Err(format!("{}: {error}", at.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", at.display()))?;
            let rel = rel.join(entry.file_name());
            let kind = entry.file_type().map_err(|error| format!("{}: {error}", entry.path().display()))?;
            if kind.is_dir() {
                pending.push(rel);
            } else if kind.is_file() {
                let bytes = std::fs::read(entry.path()).map_err(|error| format!("{}: {error}", entry.path().display()))?;
                out.push((rel.to_string_lossy().replace('\\', "/"), bytes));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Whether the store has run `unit`'s result for its inputs as they are now, or the script is
/// inert; false when that is not known. The run it prepares is kept for [`start`], which
/// takes it rather than prepare it again.
pub(crate) fn fresh(job: &Job, unit: usize) -> bool {
    let Some(slot) = job.starts.get(unit) else { return false };
    let mut slot = slot.lock();
    if slot.is_none() {
        let Ok(Some(start)) = prepare(job, unit) else { return false };
        *slot = Some(Box::new(start));
    }
    let Some(start) = slot.as_mut() else { return false };
    let start = &mut **start;
    match &start.how {
        How::Inert => true,
        How::Outside(_) => false,
        // Checked: it runs again whatever the store has.
        How::Sandboxed(prepared) if prepared.spec.mode == blaze_usercode::run::Mode::Verify => false,
        How::Sandboxed(prepared) => {
            let guard = blaze_cas::enter();
            let mut inputs = Rooted::new(prepared.spec.roots.view(), &mut start.disk);
            matches!(
                blaze_usercode::index::find(&job.input.store, &guard, &prepared.spec.key, &mut inputs),
                Ok(blaze_usercode::index::Found::Hit { .. })
            )
        }
    }
}

/// A run ready to begin.
pub(crate) struct Start {
    unit: bs::Unit,
    /// The environment it is started in, which its output is read with.
    env: Vec<(OsString, OsString)>,
    /// The machine's files and the run's environment, as what it read is checked against:
    /// made once per run, so no check sorts the environment again or takes a shared lock. Its
    /// environment is the run's own key's check ([`Prepared::env`]: the given names left out),
    /// never a pure key's, which reads the script's whole environment
    /// ([`Prepared::script_env`]).
    disk: blaze_usercode::inputs::Disk,
    how: How,
    /// What elision says the script prints, when it is inert: its result when it is not run
    /// ([`How::Inert`]), or what its run is checked against under `BLAZE_ELIDE=verify`.
    elided: Option<Value>,
}

enum How {
    /// It prints only what elision says ([`Start::elided`]): not run.
    Inert,
    /// In the sandbox, its result kept by what it read.
    Sandboxed(Prepared),
    /// Outside the sandbox (`BLAZE_SANDBOX=off`, `unsandboxed-build-scripts`): every time,
    /// nothing kept.
    Outside(Command),
}

/// Whether run `run` is outside the sandbox: `BLAZE_SANDBOX=off`, or the workspace took its
/// package out by name.
fn outside(job: &Job, run: usize) -> bool {
    job.sandbox_off() || job.plan(run).is_ok_and(|it| it.unsandboxed)
}

/// The path build script compile `compile` writes its program at: its first output, named
/// before it is built.
pub(crate) fn named_program(units: &Units, compile: usize) -> Result<PathBuf, ApiError> {
    let plan = units.plans.get(compile).ok_or_else(|| ApiError::new(format!("the build has no plan of unit {compile}"), None))?;
    plan.outputs.first().map(PathBuf::from).ok_or_else(|| UnitError::ScriptProgram { unit: compile }.into())
}

/// The roots every build script run of `job` has besides its own package and `OUT_DIR`
/// ([`blaze_usercode::roots`]): the target directory, the workspace, blaze's tools and its
/// launcher. What its key and what it read are written against, so the same script in another
/// checkout, or another workspace on this machine, is found once a second checkout verified it
/// ([`blaze_usercode::roots::verify`]). The cargo home's registry and git checkouts, where every
/// registry and git package is, are machine-wide: a run that read there outside its own package
/// can still be shared with another checkout.
pub(crate) fn build_roots(job: &Job) -> RootMap {
    let env = &job.input.config.invocation.env;
    RootMap::default()
        .with(Role::Target, Path::new(&job.units().target_dir))
        .with(Role::Workspace, Path::new(&job.input.config.invocation.root))
        .with(Role::Tools, &job.shared.tools)
        .with(Role::Launcher, &job.shared.launcher)
        .with_cargo_home(|name| env.iter().find(|it| it.0 == name).map(|it| it.1.as_str()))
}

/// [`build_roots`] with `unit`'s own: what [`script::prepare`] gives its run.
pub(crate) fn run_roots(job: &Job, unit: &bs::Unit) -> RootMap {
    build_roots(job).with(Role::Package, &unit.package.manifest_dir).with(Role::OutDir, &unit.out_dir)
}

/// Run `run` ready to begin; none while its script is not built.
fn prepare(job: &Job, run: usize) -> Result<Option<Start>, ApiError> {
    let units = job.units();
    let Some(compile) = compile_of(units, run) else {
        return Err(ApiError::new(format!("the run of unit {run} has no build script compiled"), None));
    };
    // `BLAZE_SCRIPT_CACHE=check` reruns a pure key's hit too, compared as an elided one is.
    let verify = !ported(job, run) && (verifies(job)? || pure::verifies(job)? || (rechecks(job)? && pure::hit(job, run).is_some()));
    // clone: the run's own, kept with it until it ends; once per run of an inert script.
    let elided = inert(job, run)?.or_else(|| pure::hit(job, run)).cloned();
    let program = match (&elided, verify) {
        // Never compiled: its program is named, not there.
        (Some(_), false) => None,
        _ => match job.made[compile].get().and_then(|it| it.main.clone()) {
            Some(it) => Some(it),
            None => return Ok(None),
        },
    };
    let script_path = match &program {
        Some((path, _)) => PathBuf::from(path),
        None => named_program(units, compile)?,
    };
    let of = Of {
        units,
        host: &job.input.config.host,
        tools: &job.shared.tools,
        launcher: &job.shared.launcher,
        program: script_path,
        scripts: &job.scripts,
    };
    let unit = script_unit(&of, run).map_err(|what| ApiError::new(what, Some(format!("the build script of unit {run}"))))?;
    let env = inherited(&job.input.config.invocation.env, job.plan(run)?);
    let Some((_, code)) = program else {
        let disk = blaze_usercode::inputs::Disk::new(Vec::new());
        return Ok(Some(Start { unit, env, disk, how: How::Inert, elided }));
    };
    let mut read = vec![
        unit.package.manifest_dir.clone(),
        PathBuf::from(&job.input.config.invocation.root),
        PathBuf::from(&units.target_dir),
        job.shared.launcher.clone(),
        job.shared.tools.clone(),
    ];
    read.dedup();
    let place = Place {
        code,
        read,
        network: false,
        memory: None,
        mode: match rechecks(job)? {
            true => blaze_usercode::run::Mode::Verify,
            false => blaze_usercode::run::Mode::Normal,
        },
        roots: build_roots(job),
    };
    let prepared = script::prepare(&unit, &env, &place)
        .map_err(|error| ApiError::new(error.0, Some(unit.package.descr())))?;
    let disk = blaze_usercode::inputs::Disk::new(prepared.env.clone());
    let how = match outside(job, run) {
        true => How::Outside(Command { sandbox: Sandbox::Off, ..prepared.spec.command }),
        false => How::Sandboxed(prepared),
    };
    Ok(Some(Start { unit, env, disk, how, elided }))
}

/// A build script's run, on the loop.
pub(crate) struct ScriptRun {
    job: Arc<Job>,
    unit: usize,
    tag: Tag,
    state: State,
    /// The ticket of a record its run put that a value depends on where its roots are
    /// ([`blaze_usercode::roots::verify::retraction`]): durable before its job is done.
    puts: RunPuts,
}

enum State {
    Ready(Box<Start>),
    /// `quiet` is a sandboxed run's bound; `stuck` says why it was killed past it.
    Running { start: Box<Start>, run: Option<Run>, began: SystemTime, cancelled: bool, quiet: Option<Quiet>, stuck: Option<String> },
    Writing(Box<Writing>),
    Done,
}

/// cargo's files of a run that passed, in [`blaze_build_script::run::files`]' order (`output`
/// or `stdout`, `stderr`, `root-output`), written as cargo writes them: each cut to nothing,
/// then given its bytes, then the first dated when the run began. None is synced: cargo syncs
/// none of them. The run ends ok once every effect of them completed.
struct Writing {
    files: [FileName; 3],
    /// Each file's bytes until it is cut and they are written; none are for an empty file.
    bytes: [Vec<u8>; 3],
    /// When the run began, as `output`'s time.
    modified_ns: u64,
    /// The effects asked that have not completed.
    left: u8,
}

/// The tags of [`Writing`]'s effects, after the run's own: a file's cut, its bytes (each
/// `+ file`), and the first file's time.
const CUT: u64 = 1;
const BYTES: u64 = 4;
const TIMED: u64 = 7;

/// The tags of [`Quiet`]'s effects, after [`Writing`]'s: the timer before a look, and the look.
const TICK: u64 = 8;
const LOOK: u64 = 9;

/// How often a sandboxed run's live record is looked at, on the driver's clock.
const LOOK_EVERY: Duration = Duration::from_secs(15);
/// How long a sandboxed run's live record may stay as it is before the run is one that makes
/// no progress: past any one compile a script waits on, which reads as it starts.
const QUIET_MOST: Duration = Duration::from_secs(600);

/// The live record of a sandboxed run, beside its `OUT_DIR`, where cargo's files of it are.
const LIVE_FILE: &str = "blaze-live";

/// A sandboxed run's bound. Its sandbox appends to its live record for each file, listing and
/// variable that it or any program it started reads, so a record that stays as it is for
/// [`QUIET_MOST`] is a run making no progress (cc's executor sleeping for a job token that
/// never comes): it is killed, and fails saying so. Time is the driver's: each look is a timer
/// of [`LOOK_EVERY`], then a stamp of the record.
struct Quiet {
    live: Arc<Names>,
    last: Stamp,
    /// How long the record has stayed as `last`, and how long the run has run, in looks.
    quiet: Duration,
    ran: Duration,
}

/// What [`Quiet::step`] found: the record has not moved for `quiet`, of a run of `ran`.
#[derive(PartialEq, Eq, Debug)]
struct Stuck {
    quiet: Duration,
    ran: Duration,
}

impl Quiet {
    /// The bound of the run `tag` whose live record is `live`: its first timer asked.
    fn begin(live: &FileName, tag: Tag, effects: &mut Vec<Effect>) -> Quiet {
        // alloc: once per sandboxed run, the one name its looks stamp
        let mut names = Names::with_capacity(1, live.as_str().len() + 1);
        names.push(live);
        effects.push(Effect::Timer { after: LOOK_EVERY, tag: Tag(tag.0 + TICK) });
        // alloc: as above.
        Quiet { live: Arc::new(names), last: Stamp::GONE, quiet: Duration::ZERO, ran: Duration::ZERO }
    }

    /// Whether `completion` of the run `tag` is a look's.
    fn owns(tag: Tag, completion: Tag) -> bool {
        matches!(completion.0.wrapping_sub(tag.0), TICK | LOOK)
    }

    /// A look's `completion`: the next asked while the record moves, [`Stuck`] once it has not
    /// for [`QUIET_MOST`]. A look that fails is an error: the bound can't be kept.
    fn step(&mut self, tag: Tag, completion: Completion, effects: &mut Vec<Effect>) -> Result<Option<Stuck>, String> {
        match (completion.tag.0.wrapping_sub(tag.0), completion.done) {
            (TICK, Done::Fired) => {
                self.ran += LOOK_EVERY;
                effects.push(Effect::Stamp { files: Arc::clone(&self.live), tag: Tag(tag.0 + LOOK) });
                Ok(None)
            }
            (LOOK, Done::Stamped(stamps)) => {
                let Some(&now) = stamps.first() else {
                    return Err("a look at its live record was told of no file".to_owned()); // alloc: error path
                };
                match now == self.last {
                    true => self.quiet += LOOK_EVERY,
                    false => (self.last, self.quiet) = (now, Duration::ZERO),
                }
                if self.quiet >= QUIET_MOST {
                    return Ok(Some(Stuck { quiet: self.quiet, ran: self.ran }));
                }
                effects.push(Effect::Timer { after: LOOK_EVERY, tag: Tag(tag.0 + TICK) });
                Ok(None)
            }
            (_, Done::Failed(error)) => Err(format!("a look at its live record failed: {error}")), // alloc: error path
            (own, other) => Err(format!("a look at its live record (tag +{own}) ended as {other:?}")), // alloc: error path
        }
    }
}

/// Why run unit `unit` was not reused, for `blaze why`, under its unit's subject
/// ([`super::compile::unit_subject`]): `missed` of its subject, or why it has none.
fn run_miss(job: &Job, unit: usize, missed: impl FnOnce(blaze_ids::ContentId, String) -> blaze_kernel::Miss) -> blaze_kernel::Miss {
    let (item, subject) = match job.unit(unit) {
        Ok(u) => (super::compile::span_unit(u), super::compile::unit_subject(job, u)),
        Err(error) => (format!("unit {unit}"), Err(error.what)), // alloc: once a miss
    };
    match subject {
        Ok(subject) => missed(subject, item),
        Err(why) => blaze_kernel::Miss {
            step: RUN,
            subject: blaze_ids::ContentId::default(),
            item,
            missed: blaze_kernel::Missed::Failed(why),
            key: blaze_ids::ContentId::default(),
            kept: None,
            // alloc: none
            parts: Vec::new(),
            beyond: None,
        },
    }
}

/// The step `blaze why` names a build script's run by.
const RUN: &str = "build script run";

/// Why the sandboxed run of unit `unit`'s build script, keyed as `prepared` says, was not
/// reused, for `blaze why`: its key's parts by name (its code, each variable it is given), and
/// past them, what it read when it last ran. Made on a miss only.
fn run_missed(job: &Job, unit: usize, prepared: &Prepared) -> blaze_kernel::Miss {
    use blaze_kernel::Part;
    run_miss(job, unit, |subject, item| {
        let key = prepared.spec.key;
        let mut hasher = blaze_ids::Hasher::new("blaze v2 why run key 1");
        hasher.id(&key.code).id(&key.given);
        let parts = std::iter::once(Part::new("the build script's code", Some(key.code)))
            .chain(prepared.made.parts().into_iter().map(|(name, hash)| Part::new(name, Some(hash))))
            // alloc: once a miss
            .collect();
        let mut miss = blaze_kernel::Miss::of_parts(RUN, subject, item, hasher.finish(), parts);
        miss.beyond = Some("what it read when it last ran (the files and variables it traced)");
        miss
    })
}

/// `spec`, of a sandboxed run of `unit`, with its live record named ([`blaze_usercode::confine::LIVE`])
/// at [`LIVE_FILE`] beside its `OUT_DIR`, so its bound can look at it ([`Quiet`]); an earlier
/// run's record there removed. With the record's name.
fn live_record(unit: &bs::Unit, spec: &blaze_usercode::run::Spec) -> Result<(blaze_usercode::run::Spec, FileName), String> {
    let Some(dir) = unit.out_dir.parent() else {
        return Err(format!("{}: its OUT_DIR is in no directory", unit.package.descr())); // alloc: error path
    };
    let path = dir.join(LIVE_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: an earlier run's live record: {error}", path.display())), // alloc: error path
    }
    let name = file_name(&path)?;
    // clone: once per sandboxed run, the spec the run owns
    let mut spec = spec.clone();
    spec.command.env.push((OsString::from(blaze_usercode::confine::LIVE), path.into_os_string()));
    Ok((spec, name))
}

/// `path` as the driver names it, under its root `/`.
fn file_name(path: &Path) -> Result<FileName, String> {
    path.to_str()
        .and_then(|it| FileName::new(it.trim_start_matches('/')))
        .ok_or_else(|| format!("{} is not a path blaze can write cargo's files of a run at", path.display()))
}

pub(crate) fn start(job: &Arc<Job>, unit: usize, tags: u64) -> Result<ScriptRun, ApiError> {
    let u = job.unit(unit)?;
    if !matches!(u.mode, Mode::RunCustomBuild) {
        return Err(ApiError::new(format!("unit {unit} (`{}`) is no build script's run", u.target.name), None));
    }
    let kept = job.starts.get(unit).and_then(|it| it.lock().take());
    let start = match kept {
        Some(it) => *it,
        None => match prepare(job, unit)? {
            Some(it) => it,
            None => {
                return Err(ApiError::new(
                    format!("the build script of `{}` was not built before its run", u.target.name),
                    None,
                ));
            }
        },
    };
    Ok(ScriptRun { job: job.clone(), unit, tag: Tag(tags), state: State::Ready(Box::new(start)), puts: RunPuts::default() })
}

impl ScriptRun {
    fn of(&self) -> Option<UnitOf> {
        self.job.unit_of(self.unit).ok()
    }

    /// A look of the running run's bound ([`Quiet`]): past it, or when it can't be kept, the
    /// run is killed and ends through its spawn's completion with the error. One that comes
    /// after the run ended, or once it is being killed, is dropped.
    fn looked(&mut self, completion: Completion, effects: &mut Vec<Effect>) {
        let State::Running { start, quiet: Some(quiet), cancelled: false, stuck: stuck @ None, .. } = &mut self.state else {
            return;
        };
        let what = match quiet.step(self.tag, completion, effects) {
            Ok(None) => return,
            // alloc: error path
            Ok(Some(Stuck { quiet, ran })) => format!(
                "the build script of `{}` ({}) made no progress: its sandbox's record of what it and \
                 the programs it started read stayed as it was for {} s, of a run of {} s, so it \
                 was killed (blaze bounds a run that waits on nothing at {} s of no reads)",
                start.unit.package.descr(),
                start.unit.script.display(),
                quiet.as_secs(),
                ran.as_secs(),
                QUIET_MOST.as_secs()
            ),
            // alloc: error path
            Err(why) => format!("the build script of `{}`: its bound can't be kept: {why}", start.unit.package.descr()),
        };
        *stuck = Some(what);
        effects.push(Effect::Kill { tag: self.tag });
    }

    fn error(&mut self, what: String, out: &mut Vec<Response>) -> Option<bool> {
        out.push(Response::Diagnostic(say::refused(what, self.of(), self.job.input.color)));
        self.state = State::Done;
        Some(false)
    }

    /// The run's end: checked against what elision said of it under `BLAZE_ELIDE=verify`,
    /// judged, its warnings said, what it printed applied through `writer`, and cargo's files
    /// of it written ([`Writing`]). It ends once they are.
    fn finish(
        &mut self,
        start: Box<Start>,
        value: Value,
        began: SystemTime,
        writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let job = self.job.clone();
        if let Some(elided) = &start.elided
            && let Err(error) = script::verify_elided(elided, &value)
        {
            return self.error(format!("the build script of `{}`: {}", start.unit.package.descr(), error.0), out);
        }
        // What a run outside the sandbox prints is not checked: it could do anything anyway.
        let link_dirs: Vec<PathBuf> = match start.how {
            How::Outside(_) => Vec::new(),
            _ => match crate::directives::link_dirs(&job.units().link_dirs, &start.env) {
                Ok(it) => it,
                Err(why) => {
                    return self.error(format!("the build script of `{}`: {why}", start.unit.package.descr()), out);
                }
            },
        };
        let checked = match start.how {
            How::Outside(_) => None,
            _ => Some(link_dirs.as_slice()),
        };
        let judged = judge(&start.unit, &start.env, &value, checked, self.of(), job.input.color);
        out.extend(judged.diagnostics.into_iter().map(Response::Diagnostic));
        let Some(output) = judged.output else {
            self.state = State::Done;
            return Some(false);
        };
        let Value { stdout, stderr, files, .. } = value;
        let out_dir = start.unit.out_dir.to_string_lossy().into_owned();
        let files = match start.how {
            How::Inert => out_files(files),
            How::Sandboxed(_) => out_files(files),
            How::Outside(_) => match walk(&start.unit.out_dir) {
                Ok(it) => it,
                Err(what) => return self.error(what, out),
            },
        };
        let ran = blaze_lower::Ran { cfgs: output.cfgs.clone(), env: output.env.clone(), out_dir: out_dir.clone(), files };
        let Some(writer) = writer else {
            let what = format!("the build script run of unit {} was given no writer to apply it with", self.unit);
            return self.error(what, out);
        };
        if let Err(error) = writer.ran(self.unit, ran) {
            return self.error(error.what, out);
        }
        let package_id = match job.unit(self.unit) {
            Ok(it) => it.pkg_id.clone(),
            Err(error) => return self.error(error.what, out),
        };
        out.push(Response::BuildScript(executed(&package_id, &output, &out_dir)));
        let links = start.unit.package.links.clone();
        let applied = Applied { package_id, links, output, out_dir };
        if job.scripts[self.unit].set(applied).is_err() {
            return self.error(format!("the build script run of unit {} ended twice in one job", self.unit), out);
        }
        match Writing::begin(&start.unit, stdout, stderr, began, self.tag, effects) {
            Ok(writing) => {
                self.state = State::Writing(Box::new(writing)); // alloc: once per script run
                None
            }
            Err(what) => self.error(what, out),
        }
    }

    /// A completion of one of [`Writing`]'s effects: the run ends ok once none is left.
    fn written(&mut self, mut writing: Box<Writing>, completion: Completion, effects: &mut Vec<Effect>, out: &mut Vec<Response>) -> Option<bool> {
        match writing.step(self.tag, completion, effects) {
            Ok(true) => {
                self.state = State::Done;
                Some(true)
            }
            Ok(false) => {
                self.state = State::Writing(writing);
                None
            }
            Err(what) => self.error(what, out),
        }
    }
}

impl Writing {
    /// cargo's files of a run of `unit` that printed `stdout` and `stderr` and began at `began`,
    /// each asked cut first, with tags after the run's `tag`.
    fn begin(
        unit: &bs::Unit,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        began: SystemTime,
        tag: Tag,
        effects: &mut Vec<Effect>,
    ) -> Result<Writing, String> {
        let at = blaze_build_script::run::files(unit, &[], &[])
            .map_err(|error| format!("cargo's files of the build script run of `{}`: {error}", unit.package.descr()))?;
        let files = [file_name(&at.printed.0)?, file_name(&at.stderr.0)?, file_name(&at.root_output.0)?];
        // alloc: the effect owns what it writes; `OUT_DIR`'s path, once per run.
        let root_output = at.root_output.1.to_vec();
        let since = began.duration_since(SystemTime::UNIX_EPOCH).ok();
        let modified_ns = since
            .and_then(|it| u64::try_from(it.as_nanos()).ok())
            .ok_or_else(|| format!("{}: the run began at {began:?}, which is no time a file can be given", files[0]))?;
        for (n, file) in (0u64..).zip(&files) {
            // clone: a file's name for each effect of it, three files once per run.
            effects.push(Effect::Truncate { file: file.clone(), len: 0, tag: Tag(tag.0 + CUT + n) });
        }
        Ok(Writing { files, bytes: [stdout, stderr, root_output], modified_ns, left: 3 })
    }

    /// `completion` of one of its effects, the run's tag being `tag`: the next effect of its
    /// file asked, and true once none is left.
    fn step(&mut self, tag: Tag, completion: Completion, effects: &mut Vec<Effect>) -> Result<bool, String> {
        let own = completion.tag.0.wrapping_sub(tag.0);
        if !(CUT..=TIMED).contains(&own) {
            return Err(format!("cargo's files of a build script run were told of tag {} they never asked for", completion.tag.0));
        }
        let file = ((own - CUT) % 3) as usize;
        match completion.done {
            Done::Truncated if !self.bytes[file].is_empty() => {
                let bytes = std::mem::take(&mut self.bytes[file]);
                // clone: as for its cut.
                let name = self.files[file].clone();
                effects.push(Effect::WriteAt { file: name, at: 0, bytes, tag: Tag(tag.0 + BYTES + file as u64) });
                self.left += 1;
            }
            // `output` has all its bytes: it is dated now, so no write of it comes after.
            Done::Truncated | Done::Written if file == 0 => {
                // clone: as for its cut.
                let name = self.files[0].clone();
                effects.push(Effect::Times { file: name, modified_ns: self.modified_ns, tag: Tag(tag.0 + TIMED) });
                self.left += 1;
            }
            Done::Truncated | Done::Written | Done::Timed => {}
            Done::Failed(error) => return Err(format!("writing {}: {error}", self.files[file])),
            other => return Err(format!("writing {} ended as {other:?}", self.files[file])),
        }
        self.left -= 1;
        Ok(self.left == 0)
    }
}

impl Runs<Real> for ScriptRun {
    fn start(
        &mut self,
        store: &Store,
        writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        let State::Ready(mut start) = std::mem::replace(&mut self.state, State::Done) else { return None };
        // cargo's start of a run: `OUT_DIR` made, `invoked.timestamp` written, and the time it
        // began, which its `output` is given.
        let began = match blaze_build_script::run::begin(&start.unit) {
            Ok(it) => it,
            Err(error) => return self.error(error.0, out),
        };
        match &start.how {
            How::Inert => {
                let Some(value) = start.elided.take() else {
                    let what = format!("the build script of `{}` is inert, but elision said nothing of it", start.unit.package.descr());
                    return self.error(what, out);
                };
                if let Err(what) = blaze_usercode::run::restore(&start.unit.out_dir, &value.files) {
                    let what = format!("the build script of `{}`: its OUT_DIR: {what}", start.unit.package.descr());
                    return self.error(what, out);
                }
                self.finish(start, value, began, writer, effects, out)
            }
            How::Outside(command) => {
                let miss = run_miss(&self.job, self.unit, |subject, item| blaze_kernel::Miss {
                    step: RUN,
                    subject,
                    item,
                    missed: blaze_kernel::Missed::Always(
                        // alloc: once a run outside the sandbox
                        "it runs outside the sandbox (`BLAZE_SANDBOX=off` or `unsandboxed-build-scripts`), so nothing of it is kept"
                            .to_owned(),
                    ),
                    key: blaze_ids::ContentId::default(),
                    kept: None,
                    // alloc: none
                    parts: Vec::new(),
                    beyond: None,
                });
                self.puts.misses.push(miss);
                effects.push(Effect::Spawn { command: Box::new(command.clone()), tag: self.tag });
                self.state = State::Running { start, run: None, began, cancelled: false, quiet: None, stuck: None };
                None
            }
            How::Sandboxed(prepared) => {
                let (spec, live) = match live_record(&start.unit, &prepared.spec) {
                    Ok(it) => it,
                    Err(what) => return self.error(what, out),
                };
                let begun = Run::begin(spec, store, &mut start.disk, self.tag, effects);
                match begun {
                    Ok(Begun::Reused(value)) => self.finish(start, value, began, writer, effects, out),
                    Ok(Begun::Running(run)) => {
                        let miss = run_missed(&self.job, self.unit, prepared);
                        self.puts.misses.push(miss);
                        let quiet = Some(Quiet::begin(&live, self.tag, effects));
                        self.state = State::Running { start, run: Some(run), began, cancelled: false, quiet, stuck: None };
                        None
                    }
                    Err(error) => {
                        let what = format!("the build script of `{}`: {error}", start.unit.package.descr());
                        self.error(what, out)
                    }
                }
            }
        }
    }

    fn take_puts(&mut self) -> RunPuts {
        std::mem::take(&mut self.puts)
    }

    fn owns(&self, tag: Tag) -> bool {
        tag.0 >= self.tag.0 && tag.0 - self.tag.0 < 1 << 32
    }

    fn complete(
        &mut self,
        completion: Completion,
        store: &mut Store,
        writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        if Quiet::owns(self.tag, completion.tag) {
            self.looked(completion, effects);
            return None;
        }
        let (mut start, run, began, cancelled, stuck) = match std::mem::replace(&mut self.state, State::Done) {
            State::Running { start, run, began, cancelled, stuck, .. } => (start, run, began, cancelled, stuck),
            State::Writing(writing) => return self.written(writing, completion, effects, out),
            other => {
                self.state = other;
                return None;
            }
        };
        if cancelled {
            // A run killed is no result: nothing of it is kept or applied.
            return Some(false);
        }
        if let Some(what) = stuck {
            return self.error(what, out);
        }
        let (job, run_unit) = (self.job.clone(), self.unit);
        let Start { unit, env, disk, how, .. } = &mut *start;
        let value = match (run, &*how) {
            (Some(run), How::Sandboxed(prepared)) => {
                let mut checked = None;
                let completed = run.complete(
                    completion,
                    store,
                    disk,
                    |exit| script::cargo_rule(unit, env, exit),
                    |traced| {
                        checked = pure::check(&job, run_unit, unit, traced);
                        script::keyed(prepared, traced)
                    },
                );
                match completed {
                    Ok(completed) => {
                        // What the run kept under its sandboxed key, and a retraction it put.
                        self.puts.kept(completed.tickets.iter().copied());
                        self.puts.retraction(completed.retracted);
                        let value = completed.value;
                        // A pure key's hit verified across checkouts, run again and otherwise:
                        // retracted before the run is kept under the pure key, which then
                        // keeps this checkout's own here alone.
                        let differed = match pure::retract(&job, run_unit, unit, store, &prepared.spec.roots, &value) {
                            Ok(None) => None,
                            Ok(Some((what, puts))) => {
                                self.puts.extend(puts);
                                Some(what)
                            }
                            Err((what, puts)) => {
                                self.puts.extend(puts);
                                return self.error(what, out);
                            }
                        };
                        match pure::keep(&job, run_unit, unit, checked, store, prepared.script_env(), &prepared.spec.roots, &value) {
                            Ok(puts) => self.puts.extend(puts),
                            Err((what, puts)) => {
                                self.puts.extend(puts);
                                return self.error(what, out);
                            }
                        }
                        if let Some(what) = differed {
                            let what = format!(
                                "the build script of `{}`: run again, it gave other than the result \
                                 its pure key found, verified across checkouts for the same inputs ({what}): it \
                                 depends on where its roots are, so it is recorded as doing so and no checkout is \
                                 given that result again",
                                unit.package.descr()
                            );
                            return self.error(what, out);
                        }
                        value
                    }
                    Err(error) => {
                        // alloc: error path
                        let mut what = format!("the build script of `{}`: {error}", unit.package.descr());
                        // What it put before it failed is in the store: waited for as any put, and
                        // a retraction among it flushed at once, whichever error carried it.
                        self.puts.kept(error.landed().iter().copied());
                        self.puts.retraction(error.retracted());
                        if let blaze_usercode::run::RunError::Differs { ran, .. } = &error {
                            // Its pure key may have found a value verified with the same
                            // checkouts as its sandboxed key (both are kept by one run): the
                            // rerun that differs from one is checked against the other too, and
                            // retracts it, before the run is kept under the pure key, which then
                            // keeps this checkout's own here alone. A failure of either is said
                            // with the difference, which the build fails with all the same.
                            match pure::retract(&job, run_unit, unit, store, &prepared.spec.roots, ran) {
                                Ok(None) => {}
                                Ok(Some((differs, puts))) => {
                                    self.puts.extend(puts);
                                    // alloc: error path
                                    what = format!(
                                        "{what}; and its pure key's result, verified across checkouts for the same \
                                         inputs, differs too ({differs}), so it is recorded as depending on where its \
                                         roots are and no checkout is given it again"
                                    );
                                }
                                Err((more, puts)) => {
                                    self.puts.extend(puts);
                                    what = format!("{what}; and {more}"); // alloc: error path
                                }
                            }
                            match pure::keep(&job, run_unit, unit, checked, store, prepared.script_env(), &prepared.spec.roots, ran) {
                                Ok(puts) => self.puts.extend(puts),
                                Err((more, puts)) => {
                                    self.puts.extend(puts);
                                    what = format!("{what}; and {more}"); // alloc: error path
                                }
                            }
                        }
                        return self.error(what, out);
                    }
                }
            }
            _ => match completion.done {
                Done::Exited(exit) => {
                    Value { code: exit.code, signal: exit.signal, stdout: exit.stdout, stderr: exit.stderr, files: Vec::new() }
                }
                Done::Failed(error) => {
                    let what = format!(
                        "failed to run custom build command for `{}`\n\nCaused by:\n  could not execute process `{}` \
                         (never executed)\n\nCaused by:\n  {error}",
                        unit.package.descr(),
                        unit.script.display()
                    );
                    return self.error(what, out);
                }
                other => return self.error(format!("the build script's spawn ended as {other:?}"), out),
            },
        };
        self.finish(start, value, began, writer, effects, out)
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match &mut self.state {
            State::Running { cancelled, .. } => {
                *cancelled = true;
                effects.push(Effect::Kill { tag: self.tag });
                None
            }
            _ => {
                self.state = State::Done;
                Some(false)
            }
        }
    }
}

pub(crate) mod pure;

#[cfg(test)]
mod tests;

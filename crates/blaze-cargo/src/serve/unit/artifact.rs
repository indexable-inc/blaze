//! cargo's `compiler-artifact` of a unit (`cargo/src/cargo/core/compiler/mod.rs`, the
//! `machine_message::Artifact` a finished unit sends): its files as cargo lists them, each
//! output's uplifted copy where cargo makes one (`target/debug/foo` for `deps/foo-0123`), the
//! executable, the profile it was built with.

#![forbid(unsafe_code)]

use blaze_api::{ApiError, Artifact, ArtifactKind, UnitProfile};
use blaze_ids::ContentId;
use blaze_units::{
    query::{PackagePlan, UnitPlan},
    schema::{DebugInfo, Mode, Unit},
};

use super::unit_target;

/// How the program of an artifact that runs is run, as cargo runs it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Runs {
    pub(crate) runner: Vec<String>,
    pub(crate) env: Vec<(String, String)>,
}

/// The artifact of `unit`, its main file `path` of content `content`.
pub(crate) fn artifact(
    unit: &Unit,
    plan: &UnitPlan,
    package: &PackagePlan,
    path: &str,
    content: ContentId,
    fresh: bool,
    runs: Runs,
) -> Result<Artifact, ApiError> {
    let executable = executable(unit, plan);
    let runs = match executable {
        Some(_) => runs,
        None => Runs::default(),
    };
    Ok(Artifact {
        package: unit.pkg_id.clone(),
        kind: kind(unit)?,
        path: path.to_owned(),
        content,
        manifest_path: package.manifest_path.clone(),
        target: unit_target(unit)?,
        profile: profile(unit),
        features: unit.features.clone(),
        filenames: filenames(unit, plan),
        executable,
        fresh,
        runner: runs.runner,
        env: runs.env,
        harness: plan.harness,
    })
}

/// The variable of the dynamic library search path on `os` (cargo's `dylib_path_envvar`).
pub(crate) fn dylib_var(os: &str) -> &'static str {
    match os {
        "windows" => "PATH",
        "macos" | "ios" | "tvos" | "watchos" | "visionos" => "DYLD_FALLBACK_LIBRARY_PATH",
        "aix" => "LIBPATH",
        _ => "LD_LIBRARY_PATH",
    }
}

/// What cargo sets for running a unit's program (`Compilation::fill_env` for a target
/// process): `CARGO`, the package's `CARGO_MANIFEST_*` and `CARGO_PKG_*`, `OUT_DIR` of a package
/// with a build script, what its script set with `rustc-env`, and the dynamic library search
/// path of the build's native directories under the profile's (`native`), then `deps` and the
/// profile's own. `plan` is the unit's, `os` the host's.
pub(crate) fn run_env(
    plan: &UnitPlan,
    cargo: &str,
    script: &[(String, String)],
    native: &[String],
    os: &str,
) -> Vec<(String, String)> {
    let mut env = vec![("CARGO".to_owned(), cargo.to_owned())];
    for (name, value) in &plan.env {
        let kept = name == "CARGO_MANIFEST_DIR"
            || name == "CARGO_MANIFEST_PATH"
            || name == "OUT_DIR"
            || name.starts_with("CARGO_PKG_");
        if kept {
            env.push((name.clone(), value.clone()));
        }
    }
    for (name, value) in script {
        env.retain(|(it, _)| it != name);
        env.push((name.clone(), value.clone()));
    }
    let deps = made(plan).into_iter().map(dir_of).find(|it| it.ends_with("/deps"));
    if let Some(deps) = deps {
        let root = dir_of(deps);
        let mut search: Vec<&str> = native.iter().map(String::as_str).filter(|it| super::files::under(it, root).is_some()).collect();
        search.push(deps);
        search.push(root);
        let separator = match os {
            "windows" => ";",
            _ => ":",
        };
        env.push((dylib_var(os).to_owned(), search.join(separator)));
    }
    env.sort();
    env
}

/// What the artifact is, as the agent API names it; a target kind blaze doesn't know is an
/// error naming it.
pub(crate) fn kind(unit: &Unit) -> Result<ArtifactKind, ApiError> {
    let kinds = &unit.target.kind;
    if matches!(unit.mode, Mode::Test) {
        return Ok(ArtifactKind::Test);
    }
    if kinds.iter().any(|it| it == "proc-macro") {
        return Ok(ArtifactKind::ProcMacro);
    }
    if kinds.iter().any(|it| matches!(it.as_str(), "lib" | "rlib" | "dylib" | "cdylib" | "staticlib")) {
        return Ok(ArtifactKind::Lib);
    }
    let first = kinds.first().map_or("", String::as_str);
    ArtifactKind::named(first).ok_or_else(|| {
        // alloc: error path
        ApiError::new(format!("target `{}` is of the kind `{first}`, which blaze doesn't know", unit.target.name), None)
    })
}

/// cargo's `ArtifactProfile`.
pub(crate) fn profile(unit: &Unit) -> UnitProfile {
    let profile = &unit.profile;
    UnitProfile {
        opt_level: profile.opt_level.clone(),
        debuginfo: debuginfo(&profile.debuginfo),
        debug_assertions: profile.debug_assertions,
        overflow_checks: profile.overflow_checks,
        test: matches!(unit.mode, Mode::Test),
    }
}

/// The debuginfo level as a number: `line-tables-only` and `line-directives-only` are
/// between none and limited, so 1 as the API counts them.
fn debuginfo(debuginfo: &DebugInfo) -> u32 {
    match debuginfo {
        DebugInfo::Level(level) => u32::from(*level),
        DebugInfo::Named(name) => match name.as_str() {
            "none" | "false" => 0,
            "full" | "true" => 2,
            _ => 1,
        },
    }
}

/// Whether `path` is a file cargo's `filenames` leaves out: dep-info, and the debug info
/// beside an image (`FileFlavor::DebugInfo`: `.dSYM`, `.pdb`, `.dwp`).
pub(crate) fn listed(path: &str) -> bool {
    ![".d", ".dSYM", ".pdb", ".dwp"].iter().any(|it| path.ends_with(it))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |it| it.0)
}

/// Whether `path` is a file rustc writes, named with the unit's `extra_filename`
/// (`deps/foo-0123`, `build/foo-0123/build_script_build-0123`), not a copy cargo makes.
fn rustc_writes(path: &str, plan: &UnitPlan) -> bool {
    plan.extra_filename.is_empty() || file_name(path).contains(plan.extra_filename.as_str())
}

/// The copy cargo makes of `path`, if it makes one: the output of the same name but the
/// `extra_filename` (`target/debug/foo` of `deps/foo-0123`), or a build script's
/// `build-script-build` beside it.
pub(crate) fn uplifted<'a>(path: &str, plan: &'a UnitPlan) -> Option<&'a str> {
    if plan.extra_filename.is_empty() {
        return None;
    }
    let name = file_name(path).replacen(&plan.extra_filename, "", 1);
    let script = name.starts_with("build_script_");
    plan.outputs
        .iter()
        .filter(|it| it.as_str() != path && listed(it) && !rustc_writes(it, plan))
        .find(|it| {
            file_name(it) == name
                || (script && dir_of(it) == dir_of(path) && file_name(it) == name.replace('_', "-"))
        })
        .map(String::as_str)
}

/// The files the unit itself makes, by the path rustc writes them at (the copies cargo makes
/// are the same bytes), dep-info and debug info left out.
pub(crate) fn made(plan: &UnitPlan) -> Vec<&str> {
    plan.outputs.iter().map(String::as_str).filter(|it| listed(it) && rustc_writes(it, plan)).collect()
}

/// cargo's `filenames`: each output's uplifted copy where there is one, else its own path
/// (`OutputFile::bin_dst`).
pub(crate) fn filenames(_unit: &Unit, plan: &UnitPlan) -> Vec<String> {
    made(plan).into_iter().map(|it| uplifted(it, plan).unwrap_or(it).to_owned()).collect()
}

/// cargo's `executable`: the program a unit that makes one makes (a binary, an example, a test
/// of any target), at its uplifted path when cargo uplifts it; none for a build script.
pub(crate) fn executable(unit: &Unit, plan: &UnitPlan) -> Option<String> {
    let runs = match unit.mode {
        Mode::Test => true,
        Mode::Build => {
            let kinds = &unit.target.kind;
            kinds.iter().any(|it| it == "bin")
                || (kinds.iter().any(|it| it == "example") && unit.target.crate_types.iter().any(|it| it == "bin"))
        }
        _ => false,
    };
    match runs {
        true => filenames(unit, plan).into_iter().next(),
        false => None,
    }
}

/// The unit's main file: the executable, or the library's first file.
pub(crate) fn main(unit: &Unit, plan: &UnitPlan) -> Option<String> {
    executable(unit, plan).or_else(|| filenames(unit, plan).into_iter().next())
}

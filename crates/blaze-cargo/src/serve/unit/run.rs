//! The program of a build run ([`Front::run`](crate::serve::work::Front::run)): the binary
//! `cargo run` picks (`cargo/src/cargo/ops/cargo_run.rs`: `--bin`, then the package's
//! `default-run`, then the only one), run with its arguments in the sandbox tests run in, its
//! end reported as [`Response::Ran`].

#![forbid(unsafe_code)]

use std::{ffi::OsString, path::PathBuf, sync::Arc};

use blaze_api::{ApiError, Artifact, ArtifactKind, Response, TargetKind};
use blaze_cas::Store;
use blaze_codegen::plan::{Mode as BuildMode, Policy};
use blaze_core::{Command, Completion, Done, Effect, Sandbox, Tag};
use blaze_hash::HashMap;
use blaze_lazy::{Purpose, Refused};
use blaze_units::schema::Mode;

use super::{
    Job, Real, UnitWriter, codegen,
    error::{UnitError, manifest_dir},
    package_of,
};
use crate::serve::work::Runs;

pub(crate) mod inproc;
#[cfg(test)]
mod tests;

/// How a process the daemon runs for a unit starts: a `run`'s program or a test binary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Launch {
    /// From its executable as linked: a profile compiled whole (a release one, as
    /// `blaze_lazy::allow` decides), or a run under `target.<triple>.runner`, which takes a file.
    File,
}

/// What a dev profile's process needs of blaze that it does not have yet.
pub(crate) const NOT_IN_PROCESS: &str = "blaze runs a dev profile's process on its code memory, \
    compiled lazily and tiered (docs/v2/plan/lanes/fastcompile/inproc.md, lazy.md), and this \
    blaze can't yet: its code generator has no lazy build (`Build::run` with `later`, \
    `Build::compile_one`) or tiered mode (`Mode::Tiered`, `Backend::Stencil`), and its linker has \
    no lazy placement";

/// How the process of unit `unit` starts; `runner` says it runs under a
/// `target.<triple>.runner`. What a `run` and a test binary both start through.
pub(crate) fn launch(job: &Job, unit: usize, runner: bool) -> Result<Launch, String> {
    if runner {
        return Ok(Launch::File);
    }
    let u = job.unit(unit).map_err(|it| it.what)?;
    let policy = codegen::policy(job, unit, codegen::toolchain(job, unit)?)?;
    launch_of(&u.profile.name, &u.target.name, &policy)
}

/// How a process of `target` built as `policy` under profile `profile` starts, with no runner.
/// A profile that may compile lazily (a process the daemon runs, writing no output) runs in
/// process, and while blaze can't, that is an error naming what it lacks: never a quiet start
/// from a file.
pub(crate) fn launch_of(profile: &str, target: &str, policy: &Policy) -> Result<Launch, String> {
    // clone: once per process started, of a policy a few hundred bytes long.
    let live = Policy { mode: BuildMode::Live, ..policy.clone() };
    match blaze_lazy::allow(&Purpose { profile, policy: &live, output: None }) {
        Ok(()) => Err(format!("`{target}` runs under profile `{profile}`: {NOT_IN_PROCESS}")),
        Err(Refused::Release { .. }) => Ok(Launch::File),
        Err(refused) => Err(refused.to_string()),
    }
}

/// The environment `artifact`'s program runs in: the job's, cargo's variables of it over it,
/// and the job's dynamic library search path after cargo's directories, as
/// `Compilation::fill_env` joins them (on macOS, with none set, `$HOME/lib`, `/usr/local/lib`
/// and `/usr/lib` after).
pub(crate) fn process_env(job: &[(String, String)], artifact: &Artifact, os: &str) -> Vec<(OsString, OsString)> {
    let var = super::artifact::dylib_var(os);
    let separator = match os {
        "windows" => ";",
        _ => ":",
    };
    let mut env: HashMap<String, String> = job.iter().cloned().collect();
    for (name, value) in &artifact.env {
        let joined = match env.get(name).filter(|_| name == var) {
            Some(theirs) if theirs.starts_with(value.as_str()) => theirs.clone(),
            Some(theirs) if !theirs.is_empty() => format!("{value}{separator}{theirs}"),
            _ if name == var && os == "macos" => {
                let mut dirs = vec![value.clone()];
                if let Some(home) = env.get("HOME") {
                    dirs.push(format!("{home}/lib"));
                }
                dirs.extend(["/usr/local/lib".to_owned(), "/usr/lib".to_owned()]);
                dirs.join(separator)
            }
            _ => value.clone(),
        };
        env.insert(name.clone(), joined);
    }
    let mut env: Vec<(String, String)> = env.into_iter().collect();
    env.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    env.into_iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()
}

/// Whether `artifact` is a program `cargo run` may run: a binary or an example built as one.
fn runnable(artifact: &Artifact) -> bool {
    !artifact.profile.test
        && artifact.executable.is_some()
        && (artifact.kind == ArtifactKind::Bin || artifact.target.kind == TargetKind::Example)
}

/// The program cargo runs of `built`: `bin` when given, else the `default-run` of the package
/// they are of, else the only one; cargo's error otherwise.
pub(crate) fn choose<'a>(
    built: &'a [Artifact],
    bin: Option<&str>,
    default_run: Option<&str>,
) -> Result<&'a Artifact, String> {
    let runnable: Vec<&Artifact> = built.iter().filter(|it| runnable(it)).collect();
    let names = || runnable.iter().map(|it| it.target.name.clone()).collect::<Vec<_>>();
    if let Some(bin) = bin {
        return runnable.iter().copied().find(|it| it.target.name == bin).ok_or_else(|| {
            let mut what = format!("no bin target named `{bin}`");
            if !runnable.is_empty() {
                what += "\n\nhelp: available bin targets:";
                for name in names() {
                    what += &format!("\n    {name}");
                }
            }
            what
        });
    }
    if let Some(default) = default_run
        && let Some(it) = runnable.iter().copied().find(|it| it.target.name == default)
    {
        return Ok(it);
    }
    match runnable.as_slice() {
        [] => Err("a bin target must be available for `cargo run`".to_owned()),
        [only] => Ok(only),
        _ => Err(format!(
            "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a \
             binary, or the `default-run` manifest key.\navailable binaries: {}",
            names().join(", ")
        )),
    }
}

/// The program `start` runs, on the loop.
pub(crate) struct Program {
    command: Option<Command>,
    tag: Tag,
    running: bool,
}

pub(crate) fn start(
    job: &Arc<Job>,
    built: &[Artifact],
    bin: Option<&str>,
    args: &[String],
    tags: u64,
) -> Result<Program, ApiError> {
    let units = job.units();
    let default_run = {
        let mut packages: Vec<&str> =
            built.iter().filter(|it| runnable(it)).map(|it| it.package.as_str()).collect();
        packages.sort_unstable();
        packages.dedup();
        match packages.as_slice() {
            [one] => {
                let at = units
                    .packages
                    .binary_search_by(|it| it.id.as_str().cmp(one))
                    .map_err(|_| ApiError::new(format!("the build has no package `{one}`"), None))?;
                units.packages[at].default_run.as_deref()
            }
            _ => None,
        }
    };
    let chosen = choose(built, bin, default_run).map_err(|what| ApiError::new(what, None))?;
    let program =
        chosen.executable.clone().ok_or_else(|| UnitError::NoExecutable { target: chosen.target.name.clone() })?;
    let target = &chosen.target;
    let kinds: Vec<&str> = target.kinds().collect(); // alloc: once per run, a word or two
    let at = job
        .graph
        .find(&units.graph.units, &chosen.package, &target.name, &kinds, Mode::Build)
        .ok_or_else(|| ApiError::new(format!("no unit of `{}` is in the build", target.name), None))? as usize;
    let unit = &units.graph.units[at];
    let package = package_of(units, unit)?;
    match launch(job, at, !chosen.runner.is_empty()).map_err(|what| ApiError::new(what, None))? {
        Launch::File => {}
    }
    let config = &job.input.config;
    let env = process_env(&config.invocation.env, chosen, std::env::consts::OS);
    // cargo's `target.<triple>.runner`: the runner, its arguments, then the program's.
    let (program, args): (String, Vec<String>) = match chosen.runner.split_first() {
        Some((runner, before)) => {
            (runner.clone(), before.iter().cloned().chain([program]).chain(args.iter().cloned()).collect())
        }
        None => (program, args.to_vec()),
    };
    let manifest_dir = manifest_dir(&package.name, &package.manifest_path)?;
    let sandbox = match job.sandbox_off() {
        true => Sandbox::Off,
        false => Sandbox::On {
            read: vec![PathBuf::from(&config.invocation.root), PathBuf::from(&units.target_dir), manifest_dir],
            write: Vec::new(),
            network: false,
            trace: false,
            services: Vec::new(),
        },
    };
    let command = Command {
        program: PathBuf::from(program),
        args: args.into_iter().map(OsString::from).collect(),
        env,
        dir: PathBuf::from(&config.invocation.cwd),
        stdin: Vec::new(),
        sandbox,
        memory: None,
    };
    Ok(Program { command: Some(command), tag: Tag(tags), running: false })
}

impl Runs<Real> for Program {
    fn start(
        &mut self,
        _store: &Store,
        _writer: Option<&mut UnitWriter>,
        effects: &mut Vec<Effect>,
        _out: &mut Vec<Response>,
    ) -> Option<bool> {
        let command = self.command.take()?;
        effects.push(Effect::Spawn { command: Box::new(command), tag: self.tag });
        self.running = true;
        None
    }

    fn owns(&self, tag: Tag) -> bool {
        tag == self.tag
    }

    fn complete(
        &mut self,
        completion: Completion,
        _store: &mut Store,
        _writer: Option<&mut UnitWriter>,
        _effects: &mut Vec<Effect>,
        out: &mut Vec<Response>,
    ) -> Option<bool> {
        self.running = false;
        match completion.done {
            Done::Exited(exit) => {
                let ok = exit.code == Some(0);
                out.push(Response::Ran { code: exit.code, stdout: exit.stdout, stderr: exit.stderr });
                Some(ok)
            }
            Done::Failed(error) => {
                out.push(Response::Ran { code: None, stdout: Vec::new(), stderr: error.0.into_bytes() });
                Some(false)
            }
            other => {
                let what = UnitError::RunEnded { how: format!("{other:?}") }.to_string();
                out.push(Response::Ran { code: None, stdout: Vec::new(), stderr: what.into_bytes() });
                Some(false)
            }
        }
    }

    fn cancel(&mut self, effects: &mut Vec<Effect>) -> Option<bool> {
        match self.running {
            true => {
                effects.push(Effect::Kill { tag: self.tag });
                None
            }
            false => {
                self.command = None;
                Some(false)
            }
        }
    }
}

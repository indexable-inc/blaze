//! `run`'s program, as cargo's `ops::run` (`ops/cargo_run.rs`) picks, shows and runs it: in
//! the command's directory, with its arguments, on the terminal, the command replaced by it
//! where the platform can.

use std::{
    ffi::OsString,
    fmt::Write as _,
    io::Write,
    path::{Path, PathBuf},
};

use blaze_api::{Artifact, TargetKind};
use blaze_core::exit::Exit;

use super::{args::Flags, say};

/// The one executable `run` runs of what the daemon built for it, with cargo's errors. The
/// daemon applies `default-run` and cargo's checks before it builds (`commands/run.rs`); this
/// holds the build to them.
pub fn pick<'a>(artifacts: &'a [Artifact], flags: &Flags) -> Result<&'a Artifact, String> {
    let specific = !flags.bins.is_empty() || !flags.examples.is_empty();
    let runnable = |it: &&Artifact| {
        !it.profile.test && matches!(it.target.kind, TargetKind::Bin | TargetKind::Example)
    };
    let named = |it: &&Artifact| {
        let kind = it.target.kind;
        let among =
            |names: &[Option<String>]| names.iter().flatten().any(|name| *name == it.target.name);
        // `--bin` without a name is cargo's to answer with the list; any bin is it here.
        let any = |names: &[Option<String>]| !names.is_empty() && names.iter().all(Option::is_none);
        let bins = any(&flags.bins) || among(&flags.bins);
        let examples = any(&flags.examples) || among(&flags.examples);
        !specific || (kind == TargetKind::Bin && bins) || (kind == TargetKind::Example && examples)
    };
    let bins: Vec<&Artifact> = artifacts.iter().filter(runnable).filter(named).collect();
    match bins.as_slice() {
        [] if !specific => Err("a bin target must be available for `cargo run`".to_owned()),
        [] => {
            let names: Vec<String> = flags
                .bins
                .iter()
                .chain(&flags.examples)
                .flatten()
                .map(|it| format!("`{it}`"))
                .collect();
            Err(format!("the build has no program {} to run", names.join(" or ")))
        }
        [one] if one.executable.is_none() => {
            Err(format!("example target `{}` is a library and cannot be executed", one.target.name))
        }
        [one] => Ok(one),
        _ if !specific => {
            let mut names: Vec<&str> = bins.iter().map(|it| it.target.name.as_str()).collect();
            names.sort_unstable();
            Err(format!(
                "`cargo run` could not determine which binary to run. Use the `--bin` option to \
                 specify a binary, or the `default-run` manifest key.\navailable binaries: {}",
                names.join(", ")
            ))
        }
        several => {
            let mut said = "`cargo run` can run at most one executable, but multiple were specified\nhelp: available targets:".to_owned();
            for it in several {
                let kind = it.target.kind.name();
                let package = say::package_name(&it.package);
                let _ = write!(said, "\n    {kind} `{}` in package `{package}`", it.target.name);
            }
            Err(said)
        }
    }
}

/// The executable as cargo runs it from `cwd`: below it, relative, and `./foo` for one in it.
pub fn shown_exe(path: &Path, cwd: &Path) -> PathBuf {
    match path.strip_prefix(cwd) {
        Ok(relative) if relative.file_name() == Some(relative.as_os_str()) => {
            Path::new(".").join(relative)
        }
        Ok(relative) => relative.to_owned(),
        Err(_) => path.to_owned(),
    }
}

/// A program cargo runs: `Compilation::target_process`, its runner first.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Exec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// What cargo sets, over the command's own environment.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl Exec {
    /// `executable` run by `artifact`'s runner when it has one.
    pub fn of(
        artifact: &Artifact,
        executable: PathBuf,
        args: impl IntoIterator<Item = OsString>,
        cwd: PathBuf,
    ) -> Exec {
        let (program, mut before) = match artifact.runner.split_first() {
            Some((runner, rest)) => {
                (PathBuf::from(runner), rest.iter().map(OsString::from).collect::<Vec<_>>())
            }
            None => (executable.clone(), Vec::new()),
        };
        if !artifact.runner.is_empty() {
            before.push(executable.into_os_string());
        }
        before.extend(args);
        Exec { program, args: before, env: artifact.env.clone(), cwd }
    }

    /// As cargo-util's `ProcessBuilder` shows it, without the variables.
    pub fn shown(&self) -> String {
        self.shown_at(0)
    }

    /// As cargo shows it under `verbose` `-v`s: with the variables it sets from `-vv`
    /// (`display_env_vars`, which `run` and `test` turn on under `extra_verbose`).
    pub fn shown_at(&self, verbose: u32) -> String {
        let args: Vec<String> =
            self.args.iter().map(|it| it.to_string_lossy().into_owned()).collect();
        let env: &[(String, String)] = if verbose >= 2 { &self.env } else { &[] };
        say::command_line_with(env, &self.program.to_string_lossy(), &args)
    }

    pub fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command
            .args(&self.args)
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .current_dir(&self.cwd);
        command
    }
}

/// What `run` runs: the program, from the command's directory, with the arguments after it.
pub fn planned(artifact: &Artifact, program_args: &[OsString], cwd: &Path) -> Result<Exec, String> {
    let Some(executable) = artifact.executable.as_deref() else {
        return Err(format!(
            "example target `{}` is a library and cannot be executed",
            artifact.target.name
        ));
    };
    let shown = shown_exe(&super::place::host_path(executable), cwd);
    Ok(Exec::of(artifact, shown, program_args.iter().cloned(), cwd.to_owned()))
}

/// `exec` made to start `artifact`'s program on the daemon's code memory, where the daemon
/// holds it as a dev executable (docs/v2/plan/design/compile-time.md): the daemon writes
/// the executable from the memory when it is not that file, and gives the start hook's
/// variable, which waits for this process, the one that becomes the program ([`exec`]). Any
/// other program runs its file, as does one under a runner, which takes the file.
pub fn launch(artifact: &Artifact, exec: &mut Exec) -> Result<(), String> {
    let Some(executable) = artifact.executable.as_deref() else { return Ok(()) };
    if !artifact.runner.is_empty() {
        return Ok(());
    }
    match launched(executable)? {
        Some(env) if cfg!(unix) => exec.env.extend(env),
        Some(_) => {
            return Err(format!(
                "{executable} runs on the daemon's code memory, and on this platform `blaze run` starts it as a child \
                 whose process ID the daemon's launch can't know beforehand"
            ));
        }
        None => {}
    }
    Ok(())
}

/// The daemon's answer to starting `executable` as this process (`Request::Launch`): the start
/// hook's variables when it runs on the daemon's code memory, `None` when it runs its file.
fn launched(executable: &str) -> Result<Option<Vec<(String, String)>>, String> {
    let request =
        blaze_api::Request::Launch { program: executable.to_owned(), pid: std::process::id() };
    let mut launched = None;
    super::observe::ask(request, super::daemon::Start::Never, |response| {
        if let blaze_api::ResponseRef::Launched { live, env } = response {
            launched = Some((live, env));
        }
        Ok(())
    })
    .map_err(|why| format!("starting {executable} on the daemon's code memory: {why}"))?;
    match launched {
        Some((true, env)) => Ok(Some(env)),
        Some((false, _)) => Ok(None),
        None => Err(format!("the daemon answered the launch of {executable} with no `Launched`")),
    }
}

/// The hidden command a test executable starts through (`blaze __launch PROGRAM ARGS...`), on
/// Unix: `test` runs each of its programs as a child, whose process ID it can't know before it
/// spawns it, and the daemon's launch waits for the one process that becomes the program. So
/// the child is this command, which launches the program as itself ([`launch`]) and becomes it
/// ([`exec`]): a dev test binary runs on the daemon's code memory, never from its file
/// (docs/v2/plan.md), as `blaze run`'s program does.
pub const TRAMPOLINE: &str = "__launch";

/// `exec` made to start through [`TRAMPOLINE`]: this blaze, which asks the daemon to launch
/// `artifact`'s program as the process it then becomes. One under a runner, which takes the
/// file, or with no executable, stays as it is. Unix's: elsewhere the trampoline could not
/// become the program, only wait for it.
pub fn through_trampoline(artifact: &Artifact, exec: Exec) -> Result<Exec, String> {
    let Some(executable) = artifact.executable.as_deref() else { return Ok(exec) };
    if !artifact.runner.is_empty() {
        return Ok(exec);
    }
    let blaze = std::env::current_exe()
        .map_err(|why| format!("starting {executable}: this blaze's own path: {why}"))?;
    // alloc: once per test executable run, its command line
    let mut args = Vec::with_capacity(exec.args.len() + 2);
    args.push(OsString::from(TRAMPOLINE));
    args.push(OsString::from(executable));
    args.extend(exec.args);
    Ok(Exec { program: blaze, args, env: exec.env, cwd: exec.cwd })
}

/// [`TRAMPOLINE`]'s command, `words` what follows it: the program's path, then its arguments.
/// Its variables and directory are this process's, which `test` set. Ends only when the program
/// could not start: the daemon refused it, or it could not be executed.
pub fn trampoline(words: impl Iterator<Item = OsString>, color: bool) -> i32 {
    let mut words = words;
    let Some(program) = words.next() else {
        let _ = std::io::stderr().write_all(
            say::error(&format!("blaze {TRAMPOLINE}: no program to start"), color).as_bytes(),
        );
        return Exit::Usage.code();
    };
    let Some(executable) = program.to_str() else {
        let what = format!(
            "blaze {TRAMPOLINE}: the program's path {} is not UTF-8",
            program.to_string_lossy()
        );
        let _ = std::io::stderr().write_all(say::error(&what, color).as_bytes());
        return Exit::Failed.code();
    };
    let env = match launched(executable) {
        Ok(env) => env.unwrap_or_default(),
        Err(what) => {
            let _ = std::io::stderr().write_all(say::error(&what, color).as_bytes());
            return Exit::Failed.code();
        }
    };
    // alloc: once, the program's command
    let program = Exec {
        program: PathBuf::from(&program),
        args: words.collect(),
        env,
        cwd: PathBuf::from("."),
    };
    exec(&program, &mut std::io::stderr(), Told { color, quiet: false, verbose: 0 })
}

/// How `run` speaks of its program: colored, `-q`, how many `-v`s.
#[derive(Clone, Copy, Debug, Default)]
pub struct Told {
    pub color: bool,
    pub quiet: bool,
    pub verbose: u32,
}

/// cargo's `exec_replace`: on Unix this process becomes the program, which ends the command as
/// it ends (with its code, or by its signal); elsewhere it waits for it and exits with its code,
/// saying nothing of it under `-q` (`to_run_error`).
// dyn: the CLI's output writer, written once per program run.
pub fn exec(exec: &Exec, err: &mut dyn Write, told: Told) -> i32 {
    let mut command = exec.command();
    #[cfg(unix)]
    {
        let error = std::os::unix::process::CommandExt::exec(&mut command);
        never_executed(exec, &error, err, told)
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Err(error) => never_executed(exec, &error, err, told),
            Ok(status) => match say::Ended::of(status) {
                say::Ended::Code(0) => 0,
                say::Ended::Code(code) if told.quiet => code,
                ended => {
                    let what = format!(
                        "process didn't exit successfully: {} ({})",
                        exec.shown_at(told.verbose),
                        say::ended(ended)
                    );
                    let _ = err.write_all(say::error(&what, told.color).as_bytes());
                    match ended {
                        say::Ended::Code(code) => code,
                        say::Ended::Signal(_) => Exit::Failed.code(),
                    }
                }
            },
        }
    }
}

/// cargo's `ProcessError` for a program that could not be started, with its cause; said under
/// `-q` too.
// dyn: the CLI's output writer, written once per program that could not start.
fn never_executed(exec: &Exec, error: &std::io::Error, err: &mut dyn Write, told: Told) -> i32 {
    let what =
        format!("could not execute process {} (never executed)", exec.shown_at(told.verbose));
    let _ = err.write_all(caused(&what, &[error.to_string()], told.color).as_bytes());
    Exit::Failed.code()
}

/// cargo's `display_error` of an error with causes: `Caused by:` and each, indented.
pub fn caused(what: &str, causes: &[String], color: bool) -> String {
    let mut said = say::error(what, color);
    for cause in causes {
        said.push_str("\nCaused by:\n");
        // cargo's `indented_lines`: an empty line stays empty, without the indent.
        for line in cause.lines() {
            if !line.is_empty() {
                said.push_str("  ");
                said.push_str(line);
            }
            said.push('\n');
        }
    }
    said
}

#[cfg(test)]
pub(crate) mod tests;

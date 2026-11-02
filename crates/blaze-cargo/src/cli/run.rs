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

use super::{args::Flags, say};

/// The one executable `run` runs of what the daemon built for it, with cargo's errors. The
/// daemon applies `default-run` and cargo's checks before it builds (`commands/run.rs`); this
/// holds the build to them.
pub fn pick<'a>(artifacts: &'a [Artifact], flags: &Flags) -> Result<&'a Artifact, String> {
    let specific = !flags.bins.is_empty() || !flags.examples.is_empty();
    let runnable =
        |it: &&Artifact| !it.profile.test && matches!(it.target.kind, TargetKind::Bin | TargetKind::Example);
    let named = |it: &&Artifact| {
        let kind = it.target.kind;
        let among = |names: &[Option<String>]| names.iter().flatten().any(|name| *name == it.target.name);
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
            let names: Vec<String> = flags.bins.iter().chain(&flags.examples).flatten().map(|it| format!("`{it}`")).collect();
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
        Ok(relative) if relative.file_name() == Some(relative.as_os_str()) => Path::new(".").join(relative),
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
    pub fn of(artifact: &Artifact, executable: PathBuf, args: impl IntoIterator<Item = OsString>, cwd: PathBuf) -> Exec {
        let (program, mut before) = match artifact.runner.split_first() {
            Some((runner, rest)) => (PathBuf::from(runner), rest.iter().map(OsString::from).collect::<Vec<_>>()),
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
        let args: Vec<String> = self.args.iter().map(|it| it.to_string_lossy().into_owned()).collect();
        let env: &[(String, String)] = if verbose >= 2 { &self.env } else { &[] };
        say::command_line_with(env, &self.program.to_string_lossy(), &args)
    }

    pub fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.args).envs(self.env.iter().map(|(name, value)| (name, value))).current_dir(&self.cwd);
        command
    }
}

/// What `run` runs: the program, from the command's directory, with the arguments after it.
pub fn planned(artifact: &Artifact, program_args: &[OsString], cwd: &Path) -> Result<Exec, String> {
    let Some(executable) = artifact.executable.as_deref() else {
        return Err(format!("example target `{}` is a library and cannot be executed", artifact.target.name));
    };
    let shown = shown_exe(Path::new(executable), cwd);
    Ok(Exec::of(artifact, shown, program_args.iter().cloned(), cwd.to_owned()))
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
                    let what = format!("process didn't exit successfully: {} ({})", exec.shown_at(told.verbose), say::ended(ended));
                    let _ = err.write_all(say::error(&what, told.color).as_bytes());
                    match ended {
                        say::Ended::Code(code) => code,
                        say::Ended::Signal(_) => super::FAILED,
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
    let what = format!("could not execute process {} (never executed)", exec.shown_at(told.verbose));
    let _ = err.write_all(caused(&what, &[error.to_string()], told.color).as_bytes());
    super::FAILED
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

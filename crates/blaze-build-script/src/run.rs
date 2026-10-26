//! A run of a build script as cargo 1.99 runs it: the dirty work of `build_work` in
//! `src/compiler/custom_build.rs` of rust-lang/cargo at 5f94df478. The script runs in its
//! package's directory with no input and both outputs kept, after `OUT_DIR` and the directory
//! around it are made; cargo's files of the run ([`run_dir`]) are written once it succeeds,
//! which is where cargo and blaze read a run back from.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::SystemTime,
};

use crate::{
    Error,
    env::Env,
    output::{self, Context, Output, Severity},
    unit::Unit,
};

/// What runs: the script, where, and with what environment. A sandbox runs the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub program: PathBuf,
    pub cwd: PathBuf,
    pub env: Env,
}

impl Invocation {
    pub fn of(unit: &Unit, env: Env) -> Invocation {
        Invocation { program: unit.script.clone(), cwd: unit.package.manifest_dir.clone(), env }
    }

    /// The command that runs it directly, with nothing of the caller's environment. A jobserver
    /// the run shares is given to it after (`jobserver::Client::configure`).
    #[expect(clippy::disallowed_methods, reason = "the directory is the package's, set here")]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.current_dir(&self.cwd).stdin(Stdio::null());
        self.env.apply(&mut command);
        command
    }
}

/// A finished run, before cargo has judged it.
#[derive(Clone, Debug)]
pub struct Ran {
    /// When it started, as the file system tells time: the time `output` is given, so that a
    /// file changed while the script ran makes it run again.
    pub began: SystemTime,
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Why the build fails, with what the script asked to be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    pub error: Error,
    pub log_messages: Vec<(Severity, String)>,
}

/// The directory cargo keeps a run's files in: the one around `OUT_DIR` in cargo's first
/// layout of its build directory (`build/<package>-<hash>/out`), and `run` beside `OUT_DIR`
/// in its second (`build/<package>/<hash>/out`, nightly cargo's default since 2026:
/// `BuildDirLayout::build_script_execution` of rust-lang/cargo at cad07cf8e).
pub fn run_dir(unit: &Unit) -> PathBuf {
    let around = unit.out_dir.parent().unwrap_or(&unit.out_dir);
    match is_second_layout(&unit.out_dir) {
        true => around.join("run"),
        false => around.to_owned(),
    }
}

/// The file of a run's directory that holds what the script printed: `output` in cargo's
/// first layout, `stdout` in its second.
pub fn printed(unit: &Unit) -> PathBuf {
    let name = if is_second_layout(&unit.out_dir) { "stdout" } else { "output" };
    run_dir(unit).join(name)
}

/// Whether `out_dir` is in a unit's directory of cargo's second layout,
/// `build/<package>/<hash>/out`, where the hash is sixteen hexadecimal digits.
pub fn is_second_layout(out_dir: &Path) -> bool {
    let unit = out_dir.parent();
    let hash = unit.and_then(Path::file_name).and_then(|it| it.to_str());
    let build = unit.and_then(Path::parent).and_then(Path::parent).and_then(Path::file_name);
    out_dir.file_name().is_some_and(|it| it == "out")
        && hash.is_some_and(|it| it.len() == 16 && it.bytes().all(|it| it.is_ascii_hexdigit()))
        && build.is_some_and(|it| it == "build")
}

/// Runs `command`, made from `unit`'s [`Invocation`] (directly or in a sandbox).
pub fn run(unit: &Unit, mut command: Command) -> Result<Ran, Error> {
    fs::create_dir_all(&unit.out_dir).map_err(|err| {
        Error(format!("failed to create script output directory for build command: {err}"))
    })?;
    let failed = |err: std::io::Error| Error(format!("{}: {err}", run_dir(unit).display()));
    let began = invocation_time(&run_dir(unit)).map_err(failed)?;
    command.stdin(Stdio::null());
    // A script this process just wrote stays busy while a child another thread forked holds
    // it open, until that child execs.
    let mut output = command.output();
    for wait in 1..10 {
        match &output {
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10 * wait));
                output = command.output();
            }
            _ => break,
        }
    }
    let output = output.map_err(|err| {
        Error(format!(
            "failed to run custom build command for `{}`\n\nCaused by:\n  could not execute \
             process `{}` (never executed)\n\nCaused by:\n  {err}",
            unit.package.descr(),
            unit.script.display()
        ))
    })?;
    Ok(Ran { began, status: output.status, stdout: output.stdout, stderr: output.stderr })
}

/// `paths::set_invocation_time`: the time a file written now is given.
fn invocation_time(run_dir: &Path) -> std::io::Result<SystemTime> {
    fs::create_dir_all(run_dir)?;
    let timestamp = run_dir.join("invoked.timestamp");
    fs::write(&timestamp, "This file has an mtime of when this was started.")?;
    fs::metadata(&timestamp)?.modified()
}

impl Ran {
    /// What cargo shows of the run when it fails: each line of its output that starts with
    /// `cargo::error=`, `cargo:warning=` or `cargo::warning=`, as cargo watches it stream by.
    pub fn logged(&self) -> Vec<(Severity, String)> {
        String::from_utf8_lossy(&self.stdout)
            .lines()
            .filter_map(|line| {
                if let Some(error) = line.strip_prefix("cargo::error=") {
                    return Some((Severity::Error, error.to_owned()));
                }
                let warning = line
                    .strip_prefix("cargo:warning=")
                    .or_else(|| line.strip_prefix("cargo::warning="))?;
                Some((Severity::Warning, warning.to_owned()))
            })
            .collect()
    }

    /// What cargo makes of the run: what the script printed, once it succeeded and logged no
    /// error, with cargo's files written in [`run_dir`] (`output` or `stdout`, `stderr`,
    /// `root-output`); or why the build fails.
    pub fn finish(&self, unit: &Unit, cx: &Context<'_>) -> Result<Output, Failed> {
        let logged = self.logged();
        let failed = |error: String, log_messages| Failed { error: Error(error), log_messages };
        if !self.status.success() {
            let mut cause = format!(
                "process didn't exit successfully: `{}` ({})",
                unit.script.display(),
                status_to_string(self.status)
            );
            for (name, out) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
                if let Ok(out) = std::str::from_utf8(out)
                    && !out.trim().is_empty()
                {
                    cause.push_str(&format!("\n--- {name}\n{out}"));
                }
            }
            let cause = cause.lines().map(|it| format!("  {it}")).collect::<Vec<_>>().join("\n");
            let error = format!(
                "failed to run custom build command for `{}`\n\nCaused by:\n{cause}",
                unit.package.descr()
            );
            return Err(failed(error, logged));
        }
        if logged.iter().any(|(severity, _)| *severity == Severity::Error) {
            return Err(failed("build script logged errors".to_owned(), logged));
        }
        self.write(unit).map_err(|err| failed(err.to_string(), Vec::new()))?;
        output::parse(&self.stdout, cx).map_err(|error| Failed { error, log_messages: Vec::new() })
    }

    /// cargo's files of a run that succeeded, which a later build reads the run back from.
    fn write(&self, unit: &Unit) -> std::io::Result<()> {
        let dir = run_dir(unit);
        let output = printed(unit);
        fs::write(&output, &self.stdout)?;
        let times = fs::FileTimes::new().set_accessed(self.began).set_modified(self.began);
        fs::File::options().write(true).open(&output)?.set_times(times)?;
        fs::write(dir.join("stderr"), &self.stderr)?;
        let mut root_output = fs::File::create(dir.join("root-output"))?;
        root_output.write_all(path_bytes(&unit.out_dir)?)
    }
}

/// `paths::path2bytes`.
fn path_bytes(path: &Path) -> std::io::Result<&[u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        path.to_str().map(str::as_bytes).ok_or_else(|| {
            std::io::Error::other(format!("invalid non-unicode path: {}", path.display()))
        })
    }
}

/// `exit_status_to_string` of cargo-util.
fn status_to_string(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let name = match signal {
                libc::SIGABRT => ", SIGABRT: process abort signal",
                libc::SIGALRM => ", SIGALRM: alarm clock",
                libc::SIGFPE => ", SIGFPE: erroneous arithmetic operation",
                libc::SIGHUP => ", SIGHUP: hangup",
                libc::SIGILL => ", SIGILL: illegal instruction",
                libc::SIGINT => ", SIGINT: terminal interrupt signal",
                libc::SIGKILL => ", SIGKILL: kill",
                libc::SIGPIPE => ", SIGPIPE: write on a pipe with no one to read",
                libc::SIGQUIT => ", SIGQUIT: terminal quit signal",
                libc::SIGSEGV => ", SIGSEGV: invalid memory reference",
                libc::SIGTERM => ", SIGTERM: termination signal",
                libc::SIGBUS => ", SIGBUS: access to undefined memory",
                libc::SIGSYS => ", SIGSYS: bad system call",
                libc::SIGTRAP => ", SIGTRAP: trace/breakpoint trap",
                _ => "",
            };
            return format!("signal: {signal}{name}");
        }
    }
    status.to_string()
}

#[cfg(test)]
mod tests;

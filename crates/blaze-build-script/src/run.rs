//! A run of a build script as cargo 1.99 runs it: the dirty work of `build_work` in
//! `src/compiler/custom_build.rs` of rust-lang/cargo at 5f94df478. The script runs in its
//! package's directory with no input and both outputs kept, after `OUT_DIR` and the directory
//! around it are made; cargo's files of the run ([`run_dir`]) are written once it succeeds,
//! which is where cargo and blaze read a run back from.
//!
//! The script itself runs in blaze's sandbox, never here. [`begin`] touches the file system
//! and waits on it, so it is for a thread that may block. [`judge`] reads only the bytes it
//! is given; the caller makes cargo's files of a run itself from [`invoked`] and [`files`].

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::ExitStatus,
    time::SystemTime,
};

use crate::{
    Error,
    output::{self, Context, Output, Severity},
    unit::Unit,
};

/// Why the build fails, with what the script asked to be shown.
#[derive(Debug, PartialEq, Eq)]
pub struct Failed {
    pub error: Error,
    pub log_messages: Vec<(Severity, String)>,
}

/// The directory cargo keeps a run's files in: the one around `OUT_DIR` in cargo's first
/// layout of its build directory (`build/<package>-<hash>/out`), and `run` beside `OUT_DIR`
/// in its second (`build/<package>/<hash>/out`, nightly cargo's default since 2026:
/// `BuildDirLayout::build_script_execution` of rust-lang/cargo at cad07cf8e).
pub fn run_dir(unit: &Unit) -> PathBuf {
    layout(unit).0
}

/// The file of a run's directory that holds what the script printed: `output` in cargo's
/// first layout, `stdout` in its second.
pub fn printed(unit: &Unit) -> PathBuf {
    let (mut dir, name) = layout(unit);
    dir.push(name);
    dir
}

/// [`run_dir`], and the name of [`printed`] in it, from one look at the layout.
fn layout(unit: &Unit) -> (PathBuf, &'static str) {
    let around = unit.out_dir.parent().unwrap_or(&unit.out_dir);
    match is_second_layout(&unit.out_dir) {
        true => (around.join("run"), "stdout"),
        // alloc: setup, once per run: the run's directory.
        false => (around.to_owned(), "output"),
    }
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

/// What cargo does before it runs a script: `OUT_DIR` made, and the time the run begins, as
/// the modification time of [`invoked`] written in a [`run_dir`] made.
pub fn begin(unit: &Unit) -> Result<SystemTime, Error> {
    fs::create_dir_all(&unit.out_dir).map_err(|err| {
        // alloc: error path.
        Error(format!("failed to create script output directory for build command: {err}"))
    })?;
    let (run_dir, _) = layout(unit);
    // In cargo's first layout the run's directory holds `OUT_DIR`, so it was just made.
    let made = unit.out_dir.parent() == Some(run_dir.as_path());
    // alloc: error path.
    invocation_time(&run_dir, made).map_err(|err| Error(format!("{}: {err}", run_dir.display())))
}

/// `paths::set_invocation_time`: the file in `unit`'s [`run_dir`] whose modification time,
/// once these bytes are written to it, is the time the run began.
pub fn invoked(unit: &Unit) -> (PathBuf, &'static [u8]) {
    let mut path = run_dir(unit);
    path.push(INVOKED);
    (path, INVOKED_TEXT)
}

const INVOKED: &str = "invoked.timestamp";
const INVOKED_TEXT: &[u8] = b"This file has an mtime of when this was started.";

/// `paths::set_invocation_time`: the time a file written now is given, read from the file
/// written rather than looked up again by path. `made` when `run_dir` is known to exist.
fn invocation_time(run_dir: &Path, made: bool) -> std::io::Result<SystemTime> {
    if !made {
        fs::create_dir_all(run_dir)?;
    }
    let mut timestamp = fs::File::create(run_dir.join(INVOKED))?;
    timestamp.write_all(INVOKED_TEXT)?;
    timestamp.metadata()?.modified()
}

/// What cargo shows of a run when it fails, from what it printed: each line that starts with
/// `cargo::error=`, `cargo:warning=` or `cargo::warning=`, as cargo watches it stream by.
///
/// The lines are those of `str::lines` over the output read lossily, found by memchr's
/// search in the bytes; only a message is read as text, alone, which is the same, since
/// a replaced sequence never spans a line break.
pub fn logged(stdout: &[u8]) -> Vec<(Severity, String)> {
    log_lines(stdout)
        // The check for an error reads `log_lines` alone; this is for when messages are shown.
        // alloc: the messages shown, made only when they are.
        .map(|(severity, message)| (severity, String::from_utf8_lossy(message).into_owned()))
        // alloc: as above.
        .collect()
}

/// The lines [`logged`] keeps, borrowed: only the lines that start with `cargo:` are looked
/// at, found by one search for a line break before it.
fn log_lines(stdout: &[u8]) -> impl Iterator<Item = (Severity, &[u8])> {
    let first = stdout.starts_with(b"cargo:").then_some(0);
    let rest = memchr::memmem::find_iter(stdout, b"\ncargo:").map(|at| at + 1);
    first.into_iter().chain(rest).filter_map(move |start| {
        let line = &stdout[start + b"cargo:".len()..];
        let line = match memchr::memchr(b'\n', line) {
            Some(end) => line[..end].strip_suffix(b"\r").unwrap_or(&line[..end]),
            None => line,
        };
        match line {
            [b':', rest @ ..] => match rest {
                [b'e', b'r', b'r', b'o', b'r', b'=', message @ ..] => {
                    Some((Severity::Error, message))
                }
                [b'w', b'a', b'r', b'n', b'i', b'n', b'g', b'=', message @ ..] => {
                    Some((Severity::Warning, message))
                }
                _ => None,
            },
            [b'w', b'a', b'r', b'n', b'i', b'n', b'g', b'=', message @ ..] => {
                Some((Severity::Warning, message))
            }
            _ => None,
        }
    })
}

/// Why cargo fails a run that printed `stdout` and `stderr` and ended with `status`, before
/// it reads what was printed: the script failed, or logged an error. What is shown of the
/// run is read only then.
fn refused(unit: &Unit, status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> Result<(), Failed> {
    let failed = |error: String| Failed { error: Error(error), log_messages: logged(stdout) };
    if !status.success() {
        // alloc: error path.
        let mut cause = format!(
            "process didn't exit successfully: `{}` ({})",
            unit.script.display(),
            status_to_string(status)
        );
        for (name, out) in [("stdout", stdout), ("stderr", stderr)] {
            if let Ok(out) = std::str::from_utf8(out)
                && !out.trim().is_empty()
            {
                // alloc: error path.
                cause.push_str(&format!("\n--- {name}\n{out}"));
            }
        }
        // alloc: error path.
        let cause = cause.lines().map(|it| format!("  {it}")).collect::<Vec<_>>().join("\n");
        // alloc: error path.
        let error = format!(
            "failed to run custom build command for `{}`\n\nCaused by:\n{cause}",
            unit.package.descr()
        );
        return Err(failed(error));
    }
    if log_lines(stdout).any(|(severity, _)| severity == Severity::Error) {
        // alloc: error path.
        return Err(failed("build script logged errors".to_owned()));
    }
    Ok(())
}

/// What cargo makes of a run of `unit` that ended with `status` having printed `stdout` and
/// `stderr`: what the script printed, once it succeeded and logged no error; or why the build
/// fails, read from the bytes alone.
pub fn judge(
    unit: &Unit,
    cx: &Context<'_>,
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Output, Failed> {
    refused(unit, status, stdout, stderr)?;
    // alloc: error path.
    output::parse(stdout, cx).map_err(|error| Failed { error, log_messages: Vec::new() })
}

/// cargo's files of a run that succeeded, each a path and its bytes, which a later build
/// reads the run back from. The file of what was printed is to have the time the run began
/// as its modification time.
#[derive(Debug, PartialEq, Eq)]
pub struct Files<'a> {
    /// `output` or `stdout` ([`printed`]).
    pub printed: (PathBuf, &'a [u8]),
    pub stderr: (PathBuf, &'a [u8]),
    /// `root-output`: `OUT_DIR`, as cargo's `paths::path2bytes` has it.
    pub root_output: (PathBuf, &'a [u8]),
}

/// The [`Files`] of a run of `unit` that printed `stdout` and `stderr`: an error for an
/// `OUT_DIR` that is not Unicode where paths are not bytes, as cargo's.
pub fn files<'a>(unit: &'a Unit, stdout: &'a [u8], stderr: &'a [u8]) -> std::io::Result<Files<'a>> {
    let (dir, name) = layout(unit);
    Ok(Files {
        printed: (dir.join(name), stdout),
        stderr: (dir.join("stderr"), stderr),
        root_output: (dir.join("root-output"), path_bytes(&unit.out_dir)?),
    })
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
            // alloc: error path.
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
            // alloc: error path.
            return format!("signal: {signal}{}", blaze_core::signal::described(signal));
        }
    }
    // alloc: error path.
    status.to_string()
}

#[cfg(test)]
mod tests;

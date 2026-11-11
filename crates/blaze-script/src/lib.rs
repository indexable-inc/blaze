//! What a repository script needs, and nothing of blaze's (docs/v2/plan/design/scripts.md,
//! "Tier 0"): the pinned released blaze builds this crate and the scripts of `scripts/`, so it
//! depends on no crate of `crates/` and uses no feature that release lacks.
//!
//! - [`input`]: a script's one typed input (AGENTS.md rule scripts-take-typed-input), with the
//!   refusals `tools/typed-input` gave, word for word.
//! - [`pyjson`]: JSON written as Python's `json.dumps` writes it, so a generated file comes out
//!   byte for byte as the Python script wrote it.
//! - [`run`], [`git`], [`run_shown`], [`run_within`]: a program run, its output captured or
//!   shown, or captured with a deadline.
//! - [`pyfmt`]: `{x:g}`, `{x!r}` and `{x}` of a value, as an f-string writes them.
//! - [`pystr`]: Python's `splitlines` and whitespace, where Rust's `lines` and `trim` differ.
//! - [`pypath`]: `os.path`'s `normpath`, `relpath` and the rest, `glob.glob` and `fnmatch`.
//! - [`TempDir`]: `tempfile.TemporaryDirectory`, for a `selftest` that writes files.
//! - [`fail`], [`expect`]: how a script ends in an error, and how its `selftest` checks.
//!
//! When a release carries bstd and reflection, a script's `Input::parse` is derived
//! (`bstd::data`) and these become bstd's (`blaze::std_use` moves the callers).

pub mod input;
pub mod pyfmt;
pub mod pyjson;
pub mod pypath;
pub mod pystr;

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

pub use regex;
pub use serde_json::{self as json, Map, Value};
pub use toml;

/// Ends the script as Python's `sys.exit(f"{script}: {message}")` does: the line on standard
/// error, exit status 1.
pub fn fail(script: &str, message: &str) -> ! {
    eprintln!("{script}: {message}");
    std::process::exit(1)
}

/// A `selftest`'s one check: when `holds` is false the script ends with
/// `<script> selftest: <what>`, exit status 1.
pub fn expect(script: &str, holds: bool, what: &str) {
    if !holds {
        fail(&format!("{script} selftest"), what);
    }
}

/// What a program that ran left.
#[derive(Debug, Clone)]
pub struct Ran {
    /// The exit status; -1 when a signal ended it.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Ran {
    /// Standard output as text, bytes that are not UTF-8 replaced.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Standard error as text, bytes that are not UTF-8 replaced.
    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Runs `argv` in `cwd` with no standard input and both outputs captured. `Err` is the program
/// that could not be started, with why.
pub fn run(argv: &[&str], cwd: &Path) -> Result<Ran, String> {
    let (program, args) = argv.split_first().ok_or_else(|| "no program to run".to_owned())?;
    // The workspace's `toolchain::command` is a crate of blaze's; this crate may depend on none.
    #[allow(clippy::disallowed_methods, reason = "the one place a script starts a program; `cwd` is always given")]
    let mut command = Command::new(program);
    let done = command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program} did not start in {}: {e}", cwd.display()))?;
    Ok(Ran { status: done.status.code().unwrap_or(-1), stdout: done.stdout, stderr: done.stderr })
}

/// Runs `argv` in `cwd` with this process's standard input, output and error, as Python's
/// `subprocess.run(argv, cwd=cwd)`: its exit status, -1 when a signal ended it.
pub fn run_shown(argv: &[&str], cwd: &Path) -> Result<i32, String> {
    let (program, args) = argv.split_first().ok_or_else(|| "no program to run".to_owned())?;
    #[allow(clippy::disallowed_methods, reason = "as `run`: a script starts a program here; `cwd` is always given")]
    let mut command = Command::new(program);
    let status = command
        .args(args)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("{program} did not start in {}: {e}", cwd.display()))?;
    Ok(status.code().unwrap_or(-1))
}

/// Runs `argv` in `cwd` as [`run`] does, killing it when it has not ended within `limit`, as
/// `subprocess.run(..., timeout=...)` and `asyncio.timeout` do: `Ok(None)` when it was killed.
pub fn run_within(argv: &[&str], cwd: &Path, limit: Duration) -> Result<Option<Ran>, String> {
    let (program, args) = argv.split_first().ok_or_else(|| "no program to run".to_owned())?;
    #[allow(clippy::disallowed_methods, reason = "as `run`: a script starts a program here; `cwd` is always given")]
    let mut command = Command::new(program);
    let mut child = command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program} did not start in {}: {e}", cwd.display()))?;
    // Both pipes are drained while it runs, so a program that writes much never blocks on one.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|it| Box::new(it) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|it| Box::new(it) as Box<dyn Read + Send>));
    let deadline = Instant::now() + limit;
    // Bounded: it ends at the deadline at the latest, polling every 20 ms.
    let status = loop {
        match child.try_wait().map_err(|e| format!("waiting for {program}: {e}"))? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(Some(Ran { status: status.code().unwrap_or(-1), stdout, stderr }))
}

/// `git <args>` in `root`: its standard output, or `Err` with what it said when it failed.
pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut argv = vec!["git"];
    argv.extend_from_slice(args);
    let ran = run(&argv, root)?;
    match ran.status {
        0 => Ok(ran.out()),
        status => Err(format!("git {} exited {status}: {}", args.join(" "), ran.err().trim())),
    }
}

/// An error opening `path` as Python's `OSError` prints: `[Errno 2] No such file or directory: 'x'`.
pub fn os_error(e: &std::io::Error, path: &str) -> String {
    let said = e.to_string();
    let said = said.split(" (os error").next().unwrap_or(&said);
    match e.raw_os_error() {
        Some(code) => format!("[Errno {code}] {said}: '{path}'"),
        None => format!("{said}: '{path}'"),
    }
}

/// A file's text as Python's `open(path, encoding="utf-8").read()` gives it (`\r\n` and `\r`
/// become `\n`), or `None` when there is no such file, it is a directory or it is not UTF-8.
pub fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(universal_newlines(String::from_utf8(bytes).ok()?))
}

/// Text with `\r\n` and `\r` made `\n`, as Python's text mode reads a file or a program's
/// output (`open(...)`, `subprocess.run(..., text=True)`).
pub fn universal_newlines(text: String) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text }
}

/// A new empty directory under the system's temporary directory, removed with all it holds
/// when this is dropped: Python's `tempfile.TemporaryDirectory(prefix=...)`.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A directory named `<prefix><pid>-<n>`; `Err` names the directory that could not be made.
    pub fn new(prefix: &str) -> Result<TempDir, String> {
        static MADE: AtomicU32 = AtomicU32::new(0);
        let n = MADE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}{}-{n}", std::process::id()));
        // A directory left by a process of the same id that was killed is not this one's.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).map_err(|e| format!("{} could not be made: {e}", path.display()))?;
        Ok(TempDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_past_its_deadline_is_killed() {
        let here = Path::new(".");
        let done = run_within(&["sh", "-c", "echo out; echo err >&2; exit 3"], here, Duration::from_secs(10)).unwrap();
        let done = done.unwrap();
        assert_eq!((done.status, done.out(), done.err()), (3, "out\n".to_owned(), "err\n".to_owned()));
        let started = Instant::now();
        assert!(run_within(&["sleep", "5"], here, Duration::from_millis(200)).unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_temp_dir_is_new_and_goes_when_dropped() {
        let (a, b) = (TempDir::new("blaze-script-t-").unwrap(), TempDir::new("blaze-script-t-").unwrap());
        assert_ne!(a.path(), b.path());
        std::fs::write(a.path().join("x"), "1").unwrap();
        let held = a.path().to_owned();
        drop(a);
        assert!(!held.exists() && b.path().is_dir());
    }
}

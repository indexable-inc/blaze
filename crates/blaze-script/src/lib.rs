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
//! - [`child`]: a program run with its whole environment given and its outputs to files, under
//!   a deadline that kills its process group, the kernel's peak memory and CPU time with its
//!   status (`in_process` times with it, `smoke` checks with it).
//! - [`pyfmt`]: `{x:g}`, `{x!r}` and `{x}` of a value, as an f-string writes them.
//! - [`pystr`]: Python's `splitlines` and whitespace, where Rust's `lines` and `trim` differ.
//! - [`pypath`]: `os.path`'s `normpath`, `relpath` and the rest, `glob.glob` and `fnmatch`.
//! - [`TempDir`]: `tempfile.TemporaryDirectory`, for a `selftest` that writes files.
//! - [`fail`], [`expect`], [`exit`]: how a script ends, in an error or with a status, and how
//!   its `selftest` checks.
//! - [`fan_out`]: N at once with the results in the input's order, for `ThreadPoolExecutor` and
//!   asyncio's tasks under a semaphore.
//! - [`engine`], [`report`]: harness/v2's `engine.py` and `report.py`, the blaze under test and
//!   a run's result.
//!
//! When a release carries bstd and reflection, a script's `Input::parse` is derived
//! (`bstd::data`) and these become bstd's (`blaze::std_use` moves the callers).

// `#[expect(blaze::..)]` names a blaze lint; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

pub mod child;
pub mod engine;
pub mod input;
pub mod pyfmt;
pub mod pyjson;
pub mod pypath;
pub mod pystr;
pub mod report;

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
    exit(1)
}

/// Directories to remove at exit when they are empty (`atexit.register(os.rmdir...)`).
#[allow(clippy::disallowed_types, reason = "blaze-script depends on no crate of crates/ (blaze_core::lock is blaze's)")]
#[expect(
    blaze::process_static,
    reason = "a script process's directories to remove at exit: a handful per run, and the process is one script's run"
)]
static AT_EXIT: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// `path` removed by [`exit`] if it is empty then, as storelib's `work_dir` registers it.
pub fn remove_at_exit_if_empty(path: &Path) {
    AT_EXIT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(path.to_owned());
}

/// Ends the script with `status`, after what `atexit` would run: each directory
/// [`remove_at_exit_if_empty`] named is removed if it is empty.
pub fn exit(status: i32) -> ! {
    let held = std::mem::take(&mut *AT_EXIT.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    for path in held.iter().rev() {
        let _ = std::fs::remove_dir(path);
    }
    #[allow(clippy::disallowed_methods, reason = "a script's status is its interface, as Python's sys.exit")]
    std::process::exit(status)
}

/// `f` of each item, `jobs` at a time on named threads, each taking the next item; `emit` gets
/// the results in the items' order, each as soon as it and all before it are done (as a loop over
/// `ThreadPoolExecutor` futures in submission order).
pub fn fan_out<T: Sync, R: Send>(jobs: usize, items: &[T], f: impl Fn(&T) -> R + Sync, mut emit: impl FnMut(R)) {
    let next = AtomicU32::new(0);
    let f = &f;
    let next = &next;
    std::thread::scope(|scope| {
        #[allow(clippy::disallowed_methods, clippy::disallowed_types, reason = "blaze-script depends on no crate of crates/ (crossbeam is blaze's)")]
        let (tx, rx) = std::sync::mpsc::channel::<(usize, R)>();
        for n in 0..jobs.clamp(1, items.len().max(1)) {
            let tx = tx.clone();
            let worker = move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                    let Some(item) = items.get(i) else { break };
                    if tx.send((i, f(item))).is_err() {
                        break;
                    }
                }
            };
            std::thread::Builder::new()
                .name(format!("fan-out-{n}"))
                .spawn_scoped(scope, worker)
                .unwrap_or_else(|e| fail("fan_out", &format!("a thread could not start: {e}")));
        }
        drop(tx);
        let mut held: Vec<Option<R>> = (0..items.len()).map(|_| None).collect();
        let mut at = 0;
        // Ends when every worker has ended and dropped its sender.
        for (i, result) in rx {
            held[i] = Some(result);
            while held.get(at).is_some_and(Option::is_some) {
                if let Some(result) = held[at].take() {
                    emit(result);
                }
                at += 1;
            }
        }
    });
}

/// `round(x, digits)` of a float, as Python rounds: to the nearest of the decimal places, the
/// exact binary value deciding.
pub fn py_round(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}

/// `time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())`: now, in UTC, to the second.
pub fn utc_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |it| it.as_secs());
    utc_of(secs)
}

/// The UTC time `secs` after the epoch as `%Y-%m-%dT%H:%M:%SZ`.
pub fn utc_of(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rest = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
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

/// A file's text as Python's `Path.read_text(errors="replace")` gives it: bytes that are not
/// UTF-8 each read as U+FFFD, and `\r\n` and `\r` made `\n`; the error is the open's or the read's.
pub fn read_text_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(universal_newlines(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Text with `\r\n` and `\r` made `\n`, as Python's text mode reads a file or a program's
/// output (`open(...)`, `subprocess.run(..., text=True)`).
pub fn universal_newlines(text: String) -> String {
    if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text }
}

/// Where `shutil.which(name)` finds the program `name`: `name` itself when it has a `/`, else
/// the first `<dir>/<name>` along `PATH` (the default path when it is unset) that is a file
/// this process may execute.
pub fn which(name: &str) -> Option<PathBuf> {
    #[allow(clippy::disallowed_methods, reason = "shutil.which reads the harness's own PATH")]
    let path = std::env::var_os("PATH");
    let runnable = |at: &Path| {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(at.as_os_str().as_bytes()) else { return false };
        // SAFETY: `c` is a NUL-terminated path that lives across the call.
        at.is_file() && unsafe { access(c.as_ptr(), X_OK) } == 0
    };
    if name.contains('/') {
        return runnable(Path::new(name)).then(|| PathBuf::from(name));
    }
    let dirs = path.unwrap_or_else(|| "/bin:/usr/bin".into());
    std::env::split_paths(&dirs).map(|dir| dir.join(name)).find(|at| runnable(at))
}

const X_OK: i32 = 1;

unsafe extern "C" {
    fn access(path: *const std::ffi::c_char, mode: i32) -> i32;
}

/// A new empty directory under the system's temporary directory, removed with all it holds
/// when this is dropped: Python's `tempfile.TemporaryDirectory(prefix=...)`. blaze's one
/// (`blaze_core::temp`, rule `blaze::tmp_path`); `new`'s `Err` names the directory.
pub use blaze_core::temp::TempDir;

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
    fn a_lossy_read_is_pythons_errors_replace() {
        let dir = TempDir::new("blaze-script-lossy-").unwrap();
        let path = dir.path().join("t");
        std::fs::write(&path, b"a\xffb\r\nc\rd").unwrap();
        assert_eq!(read_text_lossy(&path).unwrap(), "a\u{FFFD}b\nc\nd");
        assert!(read_text_lossy(&dir.path().join("none")).is_err());
    }

    #[test]
    fn which_finds_a_program_on_path() {
        let sh = which("sh").expect("sh is on PATH");
        assert!(sh.is_absolute() && sh.ends_with("sh"), "{}", sh.display());
        assert_eq!(which("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert_eq!(which("no-such-program-blaze-script"), None);
        assert_eq!(which("/no/such/program"), None);
    }

    #[test]
    fn fanned_out_results_come_in_order() {
        let items: Vec<u64> = (0..20).collect();
        let mut got = Vec::new();
        fan_out(
            4,
            &items,
            |&n| {
                std::thread::sleep(Duration::from_millis((20 - n) % 7));
                n * 2
            },
            |r| got.push(r),
        );
        assert_eq!(got, items.iter().map(|n| n * 2).collect::<Vec<_>>());
        let mut none = Vec::new();
        fan_out(3, &[] as &[u64], |&n| n, |r| none.push(r));
        assert!(none.is_empty());
    }

    #[test]
    fn rounding_and_time_are_pythons() {
        assert_eq!(py_round(1.23456, 2), 1.23);
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(0.5, 0), 0.0);
        assert_eq!(utc_of(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_of(1_762_819_200 + 3_661), "2025-11-11T01:01:01Z");
        assert_eq!(utc_of(951_782_400), "2000-02-29T00:00:00Z");
    }
}

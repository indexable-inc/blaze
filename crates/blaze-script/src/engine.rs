//! The blaze a harness measures, behind one interface: `harness/v2/engine.py` ported whole
//! (docs/v2/plan/design/scripts.md, "The engine and the report"), with what it takes from
//! `harness/storelib.py` (`Layout`, `work_dir`, `blaze status --json`).
//!
//! Every harness takes, in its typed input, an `engine` ([`EngineSpec`]):
//!
//! ```text
//! {"kind": "v2", "blaze": "target/release/blaze"}
//! ```
//!
//! and does everything through [`Engine`]. Builds, runs and tests go through the `blaze`
//! command line. What only an engine's insides can say is a method here. Python's two exceptions
//! are [`Fault`]: `Unmeasurable` (the engine has no answer, a failure of the run) and `Failed` (a
//! step the harness could not do, an error of the run, never a finding about the compiler).
//! Every message is the Python's, word for word.
//!
//! Not here yet: `digest`, `digests` and `outputs`, which hash with BLAKE3; they come with the
//! first harness that uses them (`proof.py`).

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use crate::{
    input::{Fields, Input, InputError},
    pyfmt::{repr, repr_str},
    pyjson::dumps,
    pystr,
};

/// The environment of a command: every variable, by name.
pub type Env = BTreeMap<String, String>;

/// `.exe` on Windows, else nothing: a program's file name.
pub const EXE: &str = if cfg!(windows) { ".exe" } else { "" };

/// Read from the caller's environment by cargo, rustc or blaze, and set by the harness alone.
pub const SCRUBBED: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "BLAZE_HOME",
    "BLAZE_BIN",
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_EMIT",
    // A memory limit nothing reads any more (the daemon's is BLAZE_DAEMON_MEMORY_GB, which a
    // caller keeps): dropped so a stale export says nothing.
    "BLAZE_MEMORY_BUDGET",
    // Where the socket goes without BLAZE_HOME (blaze_api::client::home); every command here has one.
    "XDG_RUNTIME_DIR",
];

/// The commands whose request ends in `Response::Work` (crates/blaze-api/src/build.rs), which
/// the command line prints under `--message-format=json` as a `blaze-work` line.
pub const WORKING: &[&str] = &["build", "test", "run"];
pub const WORK_COUNTS: &[&str] = &["reran", "green", "compiled", "found", "critical_path_us"];

pub const DEV_BACKENDS: &[&str] = &["default", "stencil", "cranelift"];

/// What the v2 daemon lacks for a forced dev backend; [`Engine::of`] refuses one until it has it.
pub const DEV_BACKEND_MISSING: &str = concat!(
    "the v2 daemon has no setting for the dev tier's backend yet: cargo's `codegen-backend` profile key ",
    "(`-Zcodegen-backend --config 'profile.dev.codegen-backend=\"stencil\"'`) is parsed by blaze-manifest and ",
    "not read into `blaze_codegen::plan::Policy`, which compiles opt-level 0 and 1 with Cranelift whatever it says; ",
    "requested in fed1eb428d:docs/v2/requests/requests-backend-axis.md (codegen: `Profile::codegen_backend` in plan.rs; cargo: ",
    "`serve/unit/codegen.rs::policy` reading it; coordinator: blaze-manifest's `-Zcodegen-backend` gate)"
);

/// The commands the v2 scripts run, each as `blaze --help` lists it (crates/blaze-cargo/src/cli.rs,
/// `HELP`): the preflight refuses a binary that lists one of them otherwise.
pub const HELP_LISTS: &[&str] = &[
    "build",
    "check",
    "run",
    "test",
    "stop",
    "status",
    "toolchain",
    "daemon",
    "codegen-inputs --format=json",
    "trace --out PATH",
    "tables --format=json",
    "debug sql",
];

/// The checkout: `harness/v2/..` as the Python's `ROOT` was, this crate's `../..`.
pub fn root() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::canonicalize(&here).unwrap_or(here)
}

/// The pinned nightly (`harness/toolchain`), as the Python read it at import.
pub fn toolchain() -> String {
    let path = root().join("harness/toolchain");
    match std::fs::read_to_string(&path) {
        Ok(text) => pystr::strip(&text).to_owned(),
        Err(e) => crate::fail("engine.py", &crate::os_error(&e, &path.display().to_string())),
    }
}

/// What went wrong, as the Python raised it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// What was asked has no answer in this engine.
    Unmeasurable(String),
    /// A step the harness could not do, said whole: the command, its status, what it said.
    Failed(String),
    /// An `OSError` the Python let through, as `f"{type(e).__name__}: {e}"` says it.
    Os(String),
}

impl Fault {
    /// The message, as `str(e)` gives it (an `Os` with its type's name in front).
    pub fn said(&self) -> &str {
        match self {
            Fault::Unmeasurable(it) | Fault::Failed(it) | Fault::Os(it) => it,
        }
    }

    /// An I/O error as `f"{type(e).__name__}: {e}"` prints Python's, `path` its file name.
    pub fn os(e: &std::io::Error, path: &Path) -> Fault {
        Fault::Os(format!("{}: {}", os_error_name(e), crate::os_error(e, &path.display().to_string())))
    }
}

/// [`Fault::os`] of an error about `path`, for `map_err`.
pub fn os_at(path: &Path) -> impl FnOnce(std::io::Error) -> Fault + '_ {
    move |e| Fault::os(&e, path)
}

/// The name of the `OSError` subclass Python raises for `e`.
pub fn os_error_name(e: &std::io::Error) -> &'static str {
    match e.raw_os_error() {
        Some(2) => "FileNotFoundError",
        Some(17) => "FileExistsError",
        Some(1 | 13) => "PermissionError",
        Some(20) => "NotADirectoryError",
        Some(21) => "IsADirectoryError",
        _ => "OSError",
    }
}

fn failed<T>(message: String) -> Result<T, Fault> {
    Err(Fault::Failed(message))
}

/// The `engine` field of every script's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSpec {
    pub kind: String,
    /// The `blaze` command line client.
    pub blaze: String,
    pub backend: String,
    /// The backend of every function the dev tier compiles: "default" leaves it to blaze, the
    /// others force it.
    pub dev_backend: String,
}

impl Input for EngineSpec {
    const FIELDS: &'static [&'static str] = &["kind", "blaze", "backend", "dev_backend"];

    fn parse(f: &Fields<'_>) -> Result<EngineSpec, InputError> {
        Ok(EngineSpec {
            kind: f.one_of("kind", &["v2"], None)?,
            blaze: f.str("blaze", None)?,
            backend: f.one_of("backend", &["cranelift", "llvm"], Some("cranelift"))?,
            dev_backend: f.one_of("dev_backend", DEV_BACKENDS, Some("default"))?,
        })
    }
}

impl EngineSpec {
    /// `dataclasses.asdict(spec)`.
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert("kind".to_owned(), Value::String(self.kind.clone()));
        out.insert("blaze".to_owned(), Value::String(self.blaze.clone()));
        out.insert("backend".to_owned(), Value::String(self.backend.clone()));
        out.insert("dev_backend".to_owned(), Value::String(self.dev_backend.clone()));
        Value::Object(out)
    }
}

/// A command that ran to its end.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub argv: Vec<String>,
    pub seconds: f64,
    /// The exit status; `-N` when signal N ended it, as Python's `returncode`.
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// The last `tail` characters of what it printed, stdout then stderr.
    pub fn said(&self, tail: usize) -> String {
        last_chars(&format!("{}{}", self.stdout, self.stderr), tail).to_owned()
    }

    pub fn checked(self, step: &str) -> Result<Ran, Fault> {
        if self.returncode != 0 {
            return failed(format!(
                "{step}: `{}` exited {}:\n{}",
                self.argv.join(" "),
                self.returncode,
                self.said(3000)
            ));
        }
        Ok(self)
    }
}

/// `text[-n:]` of a Python string: the last `n` characters.
pub fn last_chars(text: &str, n: usize) -> &str {
    let count = text.chars().count();
    if count <= n {
        return text;
    }
    let skip = count - n;
    let at = text.char_indices().nth(skip).map_or(text.len(), |(i, _)| i);
    &text[at..]
}

/// `text[:n]` of a Python string: the first `n` characters.
pub fn first_chars(text: &str, n: usize) -> &str {
    let at = text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    &text[..at]
}

/// What a step that printed nothing fit to read says, `{x!r:.n}`: the repr cut to `n` characters.
pub fn repr_cut(value: &Value, n: usize) -> String {
    first_chars(&repr(value), n).to_owned()
}

/// What `subprocess.run(text=True, encoding="utf-8", errors="replace")` makes of a pipe's bytes.
pub fn text(data: &[u8]) -> String {
    crate::universal_newlines(String::from_utf8_lossy(data).into_owned())
}

/// How a command ended: run out, or killed at its deadline with what it had printed so far.
enum Ended {
    Exited { status: i32, stdout: Vec<u8>, stderr: Vec<u8>, seconds: f64 },
    TimedOut { stdout: Vec<u8> },
}

#[cfg(unix)]
fn status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|n| -n)).unwrap_or(-1)
}

#[cfg(not(unix))]
fn status_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Runs `argv` in `cwd` under exactly `env`, no standard input, both outputs read to their end
/// (as `communicate` does) within `timeout` seconds of the spawn; past it the program is killed.
fn run_to_end(argv: &[String], cwd: &Path, env: &Env, timeout: f64) -> std::io::Result<Ended> {
    let Some((program, args)) = argv.split_first() else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "no program to run"));
    };
    let began = Instant::now();
    #[allow(clippy::disallowed_methods, reason = "as `crate::run`: a script starts a program here; `cwd` is always given")]
    let mut command = Command::new(program);
    let mut child = command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Each pipe is read on a thread of its own and handed over in chunks, so the deadline is
    // kept while a program (or a child it left holding the pipe) writes or is silent.
    #[allow(clippy::disallowed_methods, clippy::disallowed_types, reason = "blaze-script depends on no crate of crates/ (crossbeam is blaze's)")]
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Option<Vec<u8>>)>();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|it| Box::new(it) as Box<dyn Read + Send>),
        child.stderr.take().map(|it| Box::new(it) as Box<dyn Read + Send>),
    ];
    for (k, pipe) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        let started = std::thread::Builder::new().name(format!("engine-pipe-{k}")).spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut buffer = vec![0u8; 64 << 10];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if tx.send((k, Some(buffer[..n].to_vec()))).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            let _ = tx.send((k, None));
        });
        if let Err(e) = started {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    drop(tx);
    let deadline = began + Duration::from_secs_f64(timeout.max(0.0));
    let mut out: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    let mut open = [true, true];
    let mut status = None;
    // Bounded: it ends at the deadline at the latest.
    loop {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if let Some(done) = status
            && !open[0]
            && !open[1]
        {
            let seconds = began.elapsed().as_secs_f64();
            let [stdout, stderr] = out;
            return Ok(Ended::Exited { status: status_code(done), stdout, stderr, seconds });
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            while let Ok((k, Some(bytes))) = rx.try_recv() {
                out[k].extend_from_slice(&bytes);
            }
            let [stdout, _] = out;
            return Ok(Ended::TimedOut { stdout });
        }
        let wait = (deadline - now).min(Duration::from_millis(5));
        match rx.recv_timeout(wait) {
            Ok((k, Some(bytes))) => out[k].extend_from_slice(&bytes),
            Ok((k, None)) => open[k] = false,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                open = [false, false];
                if status.is_none() {
                    std::thread::sleep(wait);
                }
            }
        }
    }
}

/// Runs `argv` to its end. A command that outlives `timeout`, or with `check` exits other than
/// 0, is a `Failed` naming the step. `Ran::seconds` is the spawn to the reaped exit.
pub fn run(step: &str, argv: &[String], cwd: &Path, env: &Env, timeout: f64, check: bool) -> Result<Ran, Fault> {
    let shown = argv.join(" ");
    let ended = match run_to_end(argv, cwd, env, timeout) {
        Ok(it) => it,
        Err(e) => {
            let program = argv.first().map_or("", String::as_str);
            return failed(format!("{step}: `{shown}` in {} did not start: {}", cwd.display(), crate::os_error(&e, program)));
        }
    };
    match ended {
        Ended::TimedOut { stdout } => {
            let out = String::from_utf8_lossy(&stdout).into_owned();
            failed(format!("{step}: `{shown}` in {} took more than {timeout:.0} s:\n{}", cwd.display(), last_chars(&out, 3000)))
        }
        Ended::Exited { status, stdout, stderr, seconds } => {
            let ran = Ran { argv: argv.to_vec(), seconds, returncode: status, stdout: text(&stdout), stderr: text(&stderr) };
            if check && ran.returncode != 0 {
                return failed(format!("{step}: `{shown}` in {} exited {}:\n{}", cwd.display(), ran.returncode, ran.said(3000)));
            }
            Ok(ran)
        }
    }
}

/// `argv` of strings from anything that shows as one.
pub fn argv<S: AsRef<str>>(words: &[S]) -> Vec<String> {
    words.iter().map(|it| it.as_ref().to_owned()).collect()
}

fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Where the daemon of one home keeps everything, by the daemon's own rule (storelib.Layout,
/// which states where each is decided).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub home: PathBuf,
    pub socket: PathBuf,
    pub state: PathBuf,
    pub store: PathBuf,
    pub lock: PathBuf,
    pub log: PathBuf,
    pub profile: PathBuf,
    pub builds: PathBuf,
    pub sources: PathBuf,
}

/// The one place a script lays out a daemon's home.
pub fn layout(home: &Path) -> Layout {
    let state = home.join("v2");
    Layout {
        home: home.to_owned(),
        socket: home.join("v2.sock"),
        store: state.join("store"),
        lock: state.join("serve.lock"),
        log: state.join("serve.log"),
        profile: state.join("profile"),
        builds: home.join("build"),
        sources: home.join("sources"),
        state,
    }
}

/// Where the daemon of `home` keeps its store, and reads no other.
pub fn store_of(home: &Path) -> PathBuf {
    layout(home).store
}

/// `shutil.copytree(source, target, symlinks=True)`: a link copied as a link.
fn copy_tree(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    let mut entries: Vec<_> = std::fs::read_dir(source)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry.file_type()?;
        let to = target.join(entry.file_name());
        if kind.is_symlink() {
            let link = std::fs::read_link(entry.path())?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &to)?;
            #[cfg(not(unix))]
            return Err(std::io::Error::other(format!("no symbolic link {} here", link.display())));
        } else if kind.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// The store of `home` made a copy of the store directory `source`, or with None emptied, before
/// the daemon of `home` starts. A `source` that is no directory is a `Failed` naming it.
pub fn seed_store(source: Option<&Path>, home: &Path) -> Result<(), Fault> {
    let store = store_of(home);
    let _ = std::fs::remove_dir_all(&store);
    let Some(source) = source else { return Ok(()) };
    if !source.is_dir() {
        return failed(format!("seeding the store of the home {}: no store directory {}", shown(home), shown(source)));
    }
    if let Some(parent) = store.parent() {
        make_dirs(parent, true).map_err(|e| Fault::os(&e, parent))?;
    }
    copy_tree(source, &store).map_err(|e| Fault::os(&e, source))
}

/// `Path.mkdir(parents=True, exist_ok=exist_ok, mode=0o700)`: the last directory with mode 0700.
fn make_dirs(path: &Path, exist_ok: bool) -> std::io::Result<()> {
    if path.is_dir() {
        return if exist_ok { Ok(()) } else { Err(std::io::Error::from_raw_os_error(17)) };
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Err(_) if exist_ok && path.is_dir() => Ok(()),
        other => other,
    }
}

/// What blaze needs of the file system a daemon's home and build directories are on.
pub const CLONES: &str =
    "blaze places a build's outputs by cloning the store's files (FICLONE, clonefile), which this file system refuses";

static MADE: AtomicU32 = AtomicU32::new(0);

/// A name no other process or call here has: `<prefix><pid>-<n>-<nanos>`.
fn unique(prefix: &str) -> String {
    let n = MADE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.subsec_nanos());
    format!("{prefix}{}-{n}-{nanos:x}", std::process::id())
}

/// `os.strerror(code)`: the system's words for an error number.
fn strerror(code: i32) -> String {
    let said = std::io::Error::from_raw_os_error(code).to_string();
    said.split(" (os error").next().unwrap_or(&said).to_owned()
}

#[cfg(target_os = "macos")]
fn clone_file(source: &Path, target: &Path) -> Result<(), String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        // libSystem's clonefile(2): a copy that shares the source's blocks (APFS).
        fn clonefile(src: *const std::ffi::c_char, dst: *const std::ffi::c_char, flags: u32) -> std::ffi::c_int;
    }
    let (Ok(src), Ok(dst)) = (CString::new(source.as_os_str().as_bytes()), CString::new(target.as_os_str().as_bytes()))
    else {
        return Err("clonefile: a path holds a NUL".to_owned());
    };
    // SAFETY: both are NUL-terminated paths that live across the call; flags 0.
    if unsafe { clonefile(src.as_ptr(), dst.as_ptr(), 0) } != 0 {
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(format!("clonefile: {}", strerror(code)));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn clone_file(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    const FICLONE: std::ffi::c_ulong = 0x4004_9409;
    unsafe extern "C" {
        fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int;
    }
    let (src, dst) = match (std::fs::File::open(source), std::fs::File::create(target)) {
        (Ok(src), Ok(dst)) => (src, dst),
        (Err(e), _) | (_, Err(e)) => return Err(format!("FICLONE: {}", strerror(e.raw_os_error().unwrap_or(0)))),
    };
    // SAFETY: both descriptors are open for the call; FICLONE takes the source's descriptor.
    if unsafe { ioctl(dst.as_raw_fd(), FICLONE, src.as_raw_fd()) } != 0 {
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(format!("FICLONE: {}", strerror(code)));
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_file(_: &Path, _: &Path) -> Result<(), String> {
    Err(format!("no probe of cloning on {}", std::env::consts::OS))
}

/// None when a file in `directory` (which is there) can be cloned beside itself, as blaze
/// clones a store's file into a build directory; else why not, as the system says it.
pub fn cannot_clone(directory: &Path) -> Result<Option<String>, std::io::Error> {
    let source = directory.join(unique(".clone-probe-"));
    let target = PathBuf::from(format!("{}.clone", source.display()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&source)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &vec![0u8; 64 << 10]));
    let said = written.map(|()| clone_file(&source, &target).err());
    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&target);
    said
}

/// Where a work directory nobody named may go, best first: `/tmp`, then `blaze-harness` in the
/// user's cache directory (`XDG_CACHE_HOME`, else `.cache` in the user's home,
/// [`blaze_core::home::user`]). A user with neither has `/tmp` alone: no directory is guessed
/// (it was a literal `~` under the current directory).
pub fn work_bases() -> Vec<PathBuf> {
    #[allow(clippy::disallowed_methods, reason = "storelib's rule, read where it is used as the Python read it")]
    let caches = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|it| !it.is_empty())
        .map(PathBuf::from)
        .or_else(|| blaze_core::home::user().ok().map(|home| home.join(".cache")));
    let mut bases = vec![PathBuf::from("/tmp")];
    bases.extend(caches.map(|caches| caches.join("blaze-harness")));
    bases
}

/// The directory a script keeps its checkouts and daemon homes in, its real path: `given`,
/// made if it is not there, or with none given a new `<prefix>...` in the first of
/// [`work_bases`] whose file system can do what blaze needs ([`CLONES`]). A directory chosen
/// here is removed at exit when the script left it empty.
pub fn work_dir(given: &str, prefix: &str) -> Result<PathBuf, Fault> {
    if !given.is_empty() {
        std::fs::create_dir_all(given).map_err(|e| Fault::os(&e, Path::new(given)))?;
        let path = std::fs::canonicalize(given).map_err(|e| Fault::os(&e, Path::new(given)))?;
        return match cannot_clone(&path) {
            Err(e) => Err(Fault::os(&e, &path)),
            Ok(None) => Ok(path),
            Ok(Some(why)) => failed(format!(
                "work: {} is on a file system blaze cannot build on ({why}): {CLONES}; name a directory on xfs or btrfs with reflink, or on APFS",
                shown(&path)
            )),
        };
    }
    let mut refused: Vec<String> = Vec::new();
    for base in work_bases() {
        let why = match std::fs::create_dir_all(&base).and_then(|()| cannot_clone(&base)) {
            Ok(why) => why,
            Err(e) => Some(format!("{}: {}", os_error_name(&e), crate::os_error(&e, &shown(&base)))),
        };
        match why {
            None => {
                let made = base.join(unique(prefix));
                std::fs::create_dir(&made).map_err(|e| Fault::os(&e, &made))?;
                let path = std::fs::canonicalize(&made).unwrap_or(made);
                crate::remove_at_exit_if_empty(&path);
                return Ok(path);
            }
            Some(why) => refused.push(format!("{} ({why})", shown(&base))),
        }
    }
    failed(format!(
        "no work directory: {CLONES} in each of {}; give `work`, a directory on xfs or btrfs with reflink, or on APFS",
        refused.join(", ")
    ))
}

/// Everything the daemon of `home` kept removed, and with `keep_store` all but its store. The
/// daemon is stopped first by the caller.
pub fn empty_home(home: &Path, keep_store: bool) -> Result<(), Fault> {
    let store = store_of(home);
    if !keep_store || !store.is_dir() {
        let _ = std::fs::remove_dir_all(home);
        return Ok(());
    }
    let name = home.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    let aside = home.with_file_name(format!("{name}.store-kept"));
    let _ = std::fs::remove_dir_all(&aside);
    std::fs::rename(&store, &aside).map_err(|e| Fault::os(&e, &store))?;
    let _ = std::fs::remove_dir_all(home);
    if let Some(parent) = store.parent() {
        make_dirs(parent, false).map_err(|e| Fault::os(&e, parent))?;
    }
    std::fs::rename(&aside, &store).map_err(|e| Fault::os(&e, &aside))
}

/// The front of the name of the default build directory of the workspace at `ws`, before its
/// id: the last part of its path, each character but ASCII letters, digits, `-`, `_` and `.` as
/// `_`, then `-` (blaze_manifest::home::build_dir); empty for a path with no last part.
pub fn build_dir_name(ws: &Path) -> String {
    let last = ws.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    let name: String = last
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    if name.is_empty() { name } else { format!("{name}-") }
}

fn is_build_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The build directory the daemon of `home` gave the workspace at `ws` when no target directory
/// was named: found by its name's front; none or more than one is a `Failed` naming them.
pub fn default_build_dir(home: &Path, ws: &Path) -> Result<PathBuf, Fault> {
    let builds = layout(home).builds;
    let front = build_dir_name(ws);
    let mut found: Vec<PathBuf> = match std::fs::read_dir(&builds) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|it| it.path())
            .filter(|p| {
                let name = p.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
                p.is_dir() && name.starts_with(&front) && is_build_id(&name[front.len()..])
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Fault::os(&e, &builds)),
    };
    found.sort();
    if found.len() != 1 {
        let names: Vec<String> =
            found.iter().map(|p| p.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default()).collect();
        let listed = if names.is_empty() { "none".to_owned() } else { names.join(", ") };
        return failed(format!(
            "the build directory of {}: {} directories named `{front}<32 hex digits>` in {} ({listed}), not one; a build with no `CARGO_TARGET_DIR` makes one there (crates/blaze-manifest/src/home.rs, `build_dir`)",
            shown(ws),
            found.len(),
            shown(&builds)
        ));
    }
    Ok(found.remove(0))
}

/// This process's environment without [`SCRUBBED`], with the pinned toolchain named and colour,
/// backtraces and cargo's new build-directory layout off.
pub fn scrubbed() -> Env {
    let mut env: Env = std::env::vars_os()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .filter(|(k, _)| !SCRUBBED.contains(&k.as_str()))
        .collect();
    env.insert("RUSTUP_TOOLCHAIN".to_owned(), toolchain());
    env.insert("RUST_BACKTRACE".to_owned(), "0".to_owned());
    env.insert("CARGO_TERM_COLOR".to_owned(), "never".to_owned());
    env.insert("CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT".to_owned(), "false".to_owned());
    env
}

/// Plain cargo with the pinned nightly into `target`, to fetch and read sources with.
pub fn reference_env(target: &Path, extra: &[(&str, &str)]) -> Env {
    let mut env = scrubbed();
    env.insert("CARGO_TARGET_DIR".to_owned(), shown(target));
    env.insert("CARGO_INCREMENTAL".to_owned(), "0".to_owned());
    for (k, v) in extra {
        env.insert((*k).to_owned(), (*v).to_owned());
    }
    env
}

pub fn cargo(step: &str, args: &[String], ws: &Path, env: &Env, timeout: f64) -> Result<Ran, Fault> {
    let mut words = vec!["cargo".to_owned()];
    words.extend_from_slice(args);
    run(step, &words, ws, env, timeout, false)
}

/// Where two files first differ: the byte offset and the two bytes, or which is longer, or
/// "the same". A megabyte of each at a time.
pub fn first_difference(a: &Path, b: &Path) -> Result<String, Fault> {
    fn fill(f: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut got = 0;
        while got < buffer.len() {
            let n = f.read(&mut buffer[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Ok(got)
    }
    let size = 1 << 20;
    let (mut x, mut y) = (vec![0u8; size], vec![0u8; size]);
    let mut fa = std::fs::File::open(a).map_err(|e| Fault::os(&e, a))?;
    let mut fb = std::fs::File::open(b).map_err(|e| Fault::os(&e, b))?;
    let mut at = 0usize;
    loop {
        let nx = fill(&mut fa, &mut x).map_err(|e| Fault::os(&e, a))?;
        let ny = fill(&mut fb, &mut y).map_err(|e| Fault::os(&e, b))?;
        let n = nx.min(ny);
        if let Some(i) = (0..n).find(|&i| x[i] != y[i]) {
            return Ok(format!("byte {:#x}: {:#04x} against {:#04x}", at + i, x[i], y[i]));
        }
        if nx != ny {
            return Ok(format!("one ends at byte {:#x}", at + n));
        }
        if nx == 0 {
            return Ok("the same".to_owned());
        }
        at += nx;
    }
}

/// The `blaze-v2-proof` binary built beside the blaze at `blaze`.
pub fn proof_beside(blaze: &str) -> PathBuf {
    Path::new(blaze).parent().unwrap_or(Path::new("")).join(format!("blaze-v2-proof{EXE}"))
}

/// The BLAKE3 hash of a source tree, from `blaze-v2-proof` at `proof`
/// (`blaze_v2_proof::tree`); the directories named in `skip` at the top are left out.
pub fn tree_digest(root: &Path, skip: &[String], proof: &Path) -> Result<String, Fault> {
    if !proof.is_file() {
        return failed(format!("tree digest of {}: no `blaze-v2-proof` at {}", shown(root), shown(proof)));
    }
    let mut given = Map::new();
    given.insert("tree".to_owned(), Value::String(shown(root)));
    given.insert("skip".to_owned(), Value::Array(skip.iter().cloned().map(Value::String).collect()));
    let step = format!("tree digest of {}", shown(root));
    let ran = run(&step, &[shown(proof), dumps(&Value::Object(given))], root, &scrubbed(), 600.0, false)?;
    if ran.returncode != 0 {
        return failed(format!("{step}: `blaze-v2-proof` exited {}:\n{}", ran.returncode, ran.said(3000)));
    }
    match serde_json::from_str::<Value>(&ran.stdout).ok().and_then(|it| it.get("digest").cloned()) {
        Some(Value::String(digest)) => Ok(digest),
        _ => failed(format!("{step}: `blaze-v2-proof` printed no digest:\n{}", ran.said(3000))),
    }
}

/// [`first_difference`]'s answer, worded as it words it, found by `blaze-v2-proof` at `proof`
/// (`blaze_v2_proof::bytes`), then the two bytes at that offset read.
pub fn first_difference_by(proof: &Path, a: &Path, b: &Path) -> Result<String, Fault> {
    let step = format!("where {} and {} first differ", shown(a), shown(b));
    if !proof.is_file() {
        return failed(format!("{step}: no `blaze-v2-proof` at {}", shown(proof)));
    }
    let mut given = Map::new();
    given.insert("first".to_owned(), Value::String(shown(a)));
    given.insert("second".to_owned(), Value::String(shown(b)));
    let cwd = a.parent().unwrap_or(Path::new("."));
    let ran = run(&step, &[shown(proof), dumps(&Value::Object(given))], cwd, &scrubbed(), 600.0, false)?;
    if ran.returncode != 0 && ran.returncode != 1 {
        return failed(format!("{step}: `blaze-v2-proof` exited {}:\n{}", ran.returncode, ran.said(3000)));
    }
    let at = match serde_json::from_str::<Value>(&ran.stdout).ok().and_then(|it| it.get("differs_at").cloned()) {
        Some(Value::Null) => return Ok("the same".to_owned()),
        Some(Value::Number(n)) if n.is_u64() => n.as_u64().unwrap_or(0),
        _ => return failed(format!("{step}: `blaze-v2-proof` printed no `differs_at`:\n{}", ran.said(3000))),
    };
    let byte_at = |path: &Path| -> Result<Option<u8>, Fault> {
        use std::io::{Seek, SeekFrom};
        let mut f = std::fs::File::open(path).map_err(|e| Fault::os(&e, path))?;
        f.seek(SeekFrom::Start(at)).map_err(|e| Fault::os(&e, path))?;
        let mut one = [0u8; 1];
        let n = f.read(&mut one).map_err(|e| Fault::os(&e, path))?;
        Ok((n == 1).then_some(one[0]))
    };
    match (byte_at(a)?, byte_at(b)?) {
        (Some(x), Some(y)) => Ok(format!("byte {at:#x}: {x:#04x} against {y:#04x}")),
        _ => Ok(format!("one ends at byte {at:#x}")),
    }
}

/// What blaze says its toolchain is (`blaze toolchain --json`), for a result to name.
pub fn toolchain_identity(blaze: &Path, env: &Env) -> Result<String, Fault> {
    let step = format!("`{} toolchain --json`", shown(blaze));
    let words = vec![shown(blaze), "toolchain".to_owned(), "--json".to_owned()];
    Ok(pystr::strip(&run(&step, &words, &root(), env, 900.0, true)?.stdout).to_owned())
}

/// What one or more builds did, as the engine says it. None where the engine does not say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Work {
    pub compiled: Option<u64>,
    pub reran: Option<u64>,
    pub found: Option<u64>,
    pub link: Option<String>,
    pub said: String,
    pub green: Option<u64>,
    /// The reports of steps recomputed (`blaze-report` lines, one a root cause).
    pub reports: Option<u64>,
    pub causes: Vec<String>,
}

/// One of the daemon's long-lived tables: how many elements and the bytes of all its columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Table {
    pub elements: u64,
    pub bytes: u64,
}

/// Where an engine's logs were at a moment: [`Engine::work_since`] reads what came after it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mark {
    pub sizes: BTreeMap<String, usize>,
}

/// The daemon's half of `blaze status --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub pid: i64,
    pub build: String,
    pub exe: String,
    pub uptime_ms: i64,
    pub memory_bytes: i64,
    pub budget_bytes: i64,
    pub bugs: i64,
    pub requests: Vec<i64>,
    pub views: i64,
    pub history: Option<String>,
    pub settings: Vec<(String, Option<String>)>,
}

/// `blaze status --json`: this blaze, where it keeps its state, and its daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlazeStatus {
    pub version: String,
    pub api_schema: i64,
    pub home: String,
    pub state: String,
    pub socket: String,
    pub daemon: Option<DaemonStatus>,
}

pub const STATUS_FORMAT: i64 = 1;

fn is_int(value: &Value) -> bool {
    matches!(value, Value::Number(n) if n.is_i64() || n.is_u64())
}

/// storelib's `field_of(message, field, int|str, where)`.
fn field_of(message: &Map<String, Value>, field: &str, kind: &str, where_: &str) -> Result<Value, Fault> {
    let value = message.get(field).cloned().unwrap_or(Value::Null);
    let fits = match kind {
        "int" => is_int(&value),
        _ => value.is_string(),
    };
    if !fits {
        return failed(format!(
            "{where_}'s `{field}` is {}, not {kind}: {}",
            repr(&value),
            repr(&Value::Object(message.clone()))
        ));
    }
    Ok(value)
}

fn int_of(message: &Map<String, Value>, field: &str, where_: &str) -> Result<i64, Fault> {
    Ok(field_of(message, field, "int", where_)?.as_i64().unwrap_or(i64::MAX))
}

fn str_of(message: &Map<String, Value>, field: &str, where_: &str) -> Result<String, Fault> {
    Ok(field_of(message, field, "str", where_)?.as_str().unwrap_or("").to_owned())
}

fn only(where_: &str, value: &Map<String, Value>, names: &[&str]) -> Result<(), Fault> {
    let mut unknown: Vec<&str> = value.keys().map(String::as_str).filter(|k| !names.contains(k)).collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        return failed(format!("{where_} has fields this harness does not know: {}", unknown.join(", ")));
    }
    Ok(())
}

/// What `blaze status --json` printed, read (storelib's `blaze_status_of`).
pub fn blaze_status_of(text: &str) -> Result<BlazeStatus, Fault> {
    let where_ = "`blaze status --json`";
    let parsed: Value = match serde_json::from_str(text) {
        Ok(it) => it,
        Err(e) => {
            return failed(format!("{where_} is not JSON ({e}): {}", first_chars(&repr_str(text), 300)));
        }
    };
    let Value::Object(value) = &parsed else {
        return failed(format!("{where_} is not a JSON object: {}", repr_cut(&parsed, 200)));
    };
    only(where_, value, &["format", "version", "api_schema", "home", "state", "socket", "daemon"])?;
    let form = int_of(value, "format", where_)?;
    if form != STATUS_FORMAT {
        return failed(format!("{where_} is of format {form}, this harness reads {STATUS_FORMAT}"));
    }
    let mut daemon = None;
    if let Some(raw) = value.get("daemon").filter(|it| !it.is_null()) {
        let inner = format!("{where_}'s `daemon`");
        let Value::Object(it) = raw else {
            return failed(format!("{inner} is not a JSON object: {}", repr_cut(raw, 200)));
        };
        only(
            &inner,
            it,
            &["pid", "build", "exe", "uptime_ms", "memory_bytes", "budget_bytes", "bugs", "requests", "views", "history", "settings"],
        )?;
        let (Some(Value::Array(requests_raw)), Some(Value::Object(settings_raw))) = (it.get("requests"), it.get("settings"))
        else {
            return failed(format!("{inner}'s `requests` is no list or its `settings` no object: {}", repr_cut(raw, 300)));
        };
        let requests: Vec<i64> = requests_raw.iter().filter(|r| is_int(r)).map(|r| r.as_i64().unwrap_or(i64::MAX)).collect();
        let mut settings: Vec<(String, Option<String>)> = settings_raw
            .iter()
            .filter_map(|(k, v)| match v {
                Value::Null => Some((k.clone(), None)),
                Value::String(s) => Some((k.clone(), Some(s.clone()))),
                _ => None,
            })
            .collect();
        settings.sort();
        if requests.len() != requests_raw.len() || settings.len() != settings_raw.len() {
            return failed(format!(
                "{inner} has a request that is no number or a setting that is no string: {}",
                repr_cut(raw, 300)
            ));
        }
        let history = match it.get("history") {
            None | Some(Value::Null) => None,
            Some(_) => Some(str_of(it, "history", &inner)?),
        };
        daemon = Some(DaemonStatus {
            pid: int_of(it, "pid", &inner)?,
            build: str_of(it, "build", &inner)?,
            exe: str_of(it, "exe", &inner)?,
            uptime_ms: int_of(it, "uptime_ms", &inner)?,
            memory_bytes: int_of(it, "memory_bytes", &inner)?,
            budget_bytes: int_of(it, "budget_bytes", &inner)?,
            bugs: int_of(it, "bugs", &inner)?,
            requests,
            views: int_of(it, "views", &inner)?,
            history,
            settings,
        });
    }
    Ok(BlazeStatus {
        version: str_of(value, "version", where_)?,
        api_schema: int_of(value, "api_schema", where_)?,
        home: str_of(value, "home", where_)?,
        state: str_of(value, "state", where_)?,
        socket: str_of(value, "socket", where_)?,
        daemon,
    })
}

/// `blaze status --json` asked of the daemon `env`'s home names; it starts none. A failed answer
/// is `Failed` naming its exit code and stderr; the `Err` says what Python raised, as
/// `(type name, message)`.
pub fn blaze_status(blaze: &str, env: &Env, cwd: &Path, timeout: f64) -> Result<BlazeStatus, (&'static str, String)> {
    let words = vec![blaze.to_owned(), "status".to_owned(), "--json".to_owned()];
    match run_to_end(&words, cwd, env, timeout) {
        Err(e) => Err((os_error_name(&e), crate::os_error(&e, blaze))),
        Ok(Ended::TimedOut { .. }) => {
            let listed: Vec<String> = words.iter().map(|it| repr_str(it)).collect();
            // The timeout as the caller wrote it: every caller gives a whole number of seconds.
            let secs = if timeout.fract() == 0.0 { format!("{timeout:.0}") } else { crate::pyfmt::float_repr(timeout) };
            Err(("TimeoutExpired", format!("Command '[{}]' timed out after {secs} seconds", listed.join(", "))))
        }
        Ok(Ended::Exited { status, stdout, stderr, .. }) => {
            if status != 0 {
                return Err(("Failed", format!("`{blaze} status --json` exited {status}: {}", last_chars(&text(&stderr), 1500))));
            }
            blaze_status_of(&text(&stdout)).map_err(|f| ("Failed", f.said().to_owned()))
        }
    }
}

/// Whether `blaze --help`'s text `listed` has a line for the command `name`: the line starts
/// with it, whole (`build, b`, `debug sql QUERY`; `tables` is not `tables-of`).
pub fn help_lists(listed: &str, name: &str) -> bool {
    pystr::splitlines(listed).into_iter().any(|line| {
        let text = pystr::strip(line);
        text.starts_with(name) && (text.len() == name.len() || matches!(text.as_bytes()[name.len()], b' ' | b','))
    })
}

/// The cargo profile `blaze COMMAND ARGS` builds with, as cargo picks it: `--profile`,
/// `--release`, else `test` for `test` and `bench` for `bench`, else `dev`.
pub fn profile_of(command: &str, args: &[String]) -> String {
    let words = match args.iter().position(|it| it == "--") {
        Some(at) => &args[..at],
        None => args,
    };
    let mut named: Option<String> = None;
    for (at, word) in words.iter().enumerate() {
        if word == "--release" || word == "-r" {
            named = named.or_else(|| Some("release".to_owned()));
        } else if word == "--profile" && at + 1 < words.len() {
            named = Some(words[at + 1].clone());
        } else if let Some(rest) = word.strip_prefix("--profile=") {
            named = Some(rest.to_owned());
        }
    }
    named.unwrap_or_else(|| match command {
        "test" => "test".to_owned(),
        "bench" => "bench".to_owned(),
        _ => "dev".to_owned(),
    })
}

/// `args` of `blaze COMMAND`, with the dev tier forced to `dev_backend` through cargo's own
/// setting, before any `--`; "default" and a command that compiles nothing leave them as they are.
pub fn dev_backend_args(dev_backend: &str, command: &str, args: &[String]) -> Vec<String> {
    if dev_backend == "default" || !WORKING.contains(&command) {
        return args.to_vec();
    }
    let profile = profile_of(command, args);
    let forced =
        ["-Zcodegen-backend".to_owned(), "--config".to_owned(), format!("profile.{profile}.codegen-backend=\"{dev_backend}\"")];
    let at = args.iter().position(|it| it == "--").unwrap_or(args.len());
    let mut out = args[..at].to_vec();
    out.extend(forced);
    out.extend_from_slice(&args[at..]);
    out
}

/// The JSON a command printed, after checking it exited 0.
pub fn json_answer(step: &str, ran: Ran) -> Result<Value, Fault> {
    let ran = ran.checked(step)?;
    serde_json::from_str(&ran.stdout)
        .or_else(|e| failed(format!("{step}: `{}` printed no JSON ({e}):\n{}", ran.argv.join(" "), ran.said(3000))))
}

/// The `blaze-work` lines of a command's `--message-format=json` output.
pub fn worked_lines(stdout: &str) -> Vec<Map<String, Value>> {
    pystr::splitlines(stdout)
        .into_iter()
        .filter(|text| text.starts_with('{') && text.contains("\"blaze-work\""))
        .filter_map(|text| match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(line)) if line.get("reason").and_then(Value::as_str) == Some("blaze-work") => Some(line),
            _ => None,
        })
        .collect()
}

/// The `blaze-report` lines of a command's output that say why its build recomputed steps (a
/// report whose rule is one of `why:...`), each as printed.
pub fn report_lines(stdout: &str) -> Vec<String> {
    pystr::splitlines(stdout)
        .into_iter()
        .filter(|text| text.starts_with('{') && text.contains("\"blaze-report\"") && text.contains("\"why:"))
        .filter(|text| {
            matches!(serde_json::from_str::<Value>(text),
                Ok(Value::Object(line)) if line.get("reason").and_then(Value::as_str) == Some("blaze-report"))
        })
        .map(str::to_owned)
        .collect()
}

/// `line`, checked whole; a request without one is a `Failed` naming it.
pub fn required<'a>(label: &str, line: Option<&'a Map<String, Value>>) -> Result<&'a Map<String, Value>, Fault> {
    let Some(line) = line else {
        return failed(format!(
            "{label}: no `blaze-work` line, so what it did is not known (it is printed only under `--message-format=json`, and every build, test and run that runs reports it)"
        ));
    };
    for field in WORK_COUNTS {
        work_count(label, line, field)?;
    }
    let link = line.get("link");
    if !matches!(link, Some(Value::String(it)) if it == "patched" || it == "placed") {
        return failed(format!(
            "{label}: `blaze-work` has link {}, not \"patched\" or \"placed\": {}",
            repr(link.unwrap_or(&Value::Null)),
            repr(&Value::Object(line.clone()))
        ));
    }
    Ok(line)
}

/// The count `field` of a `blaze-work` line: an int, not a bool, not below 0.
pub fn work_count(label: &str, line: &Map<String, Value>, field: &str) -> Result<u64, Fault> {
    match line.get(field) {
        Some(Value::Number(n)) if n.is_u64() => Ok(n.as_u64().unwrap_or(0)),
        value => failed(format!(
            "{label}: `blaze-work` has {field} {}, not a count: {}",
            repr(value.unwrap_or(&Value::Null)),
            repr(&Value::Object(line.clone()))
        )),
    }
}

/// `json.dumps(value, sort_keys=True)`.
pub fn dumps_sorted(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(fields) => {
                let mut keys: Vec<&String> = fields.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), sorted(&fields[k]))).collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    dumps(&sorted(value))
}

pub fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        other => other,
    }
}

/// What each build, test and run through [`Engine::build`] did, by its daemon's home.
#[derive(Debug, Default)]
struct Records {
    /// Its label and its `blaze-work` line, or None where it printed none.
    worked: BTreeMap<String, Vec<(String, Option<Map<String, Value>>)>>,
    /// Beside each of those, the `blaze-report` lines the command printed.
    reported: BTreeMap<String, Vec<Vec<String>>>,
    /// Every command through `build`, in order: its step and its seconds.
    took: Vec<(String, f64)>,
}

/// The blaze under test: blaze v2, one daemon per machine whose only interface is the agent API;
/// the command line is its client. Every build is its own, so a build that exits 0 was blaze's.
/// Shared by threads as the Python's is under the GIL: its records sit behind a lock.
#[derive(Debug)]
pub struct Engine {
    pub spec: EngineSpec,
    /// The client, its path resolved.
    pub blaze: PathBuf,
    /// `blaze --version`'s line, once the preflight has asked.
    pub version: String,
    #[allow(clippy::disallowed_types, reason = "blaze-script depends on no crate of crates/ (blaze_core::lock is blaze's)")]
    records: Mutex<Records>,
}

/// `Path(p).resolve()`: the real path where there is one, else the absolute path made normal.
fn resolve(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(crate::pypath::abspath(path)))
}

impl Engine {
    /// The engine of `spec`, its assumptions about the binary checked (the preflight).
    pub fn of(spec: &EngineSpec) -> Result<Engine, Fault> {
        let blaze = resolve(&spec.blaze);
        if !blaze.is_file() {
            return failed(format!("engine.blaze: no file {}", shown(&blaze)));
        }
        if spec.backend != "cranelift" {
            return failed(format!(
                "engine.backend {}: v2 picks each unit's backend from its profile (opt-level), and BLAZE_BACKEND, BLAZE_OPT and BLAZE_EMIT are gone: the code generator refuses every unit while one is set (CHANGES_CODE in crates/hir-ty-codegen/v2/backend.rs); build with `--release` for LLVM",
                repr_str(&spec.backend)
            ));
        }
        if spec.dev_backend != "default" {
            return failed(format!("engine.dev_backend {}: {DEV_BACKEND_MISSING}", repr_str(&spec.dev_backend)));
        }
        let mut eng = Engine { spec: spec.clone(), blaze, version: String::new(), records: Mutex::default() };
        eng.preflight()?;
        Ok(eng)
    }

    pub fn kind(&self) -> &str {
        &self.spec.kind
    }

    fn records(&self) -> std::sync::MutexGuard<'_, Records> {
        // A thread that panicked holding it left whole records: each push is one statement.
        self.records.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The environment of a `blaze` command whose daemon lives in `home`; the build directory is
    /// the daemon's default under the home unless `target` is given (`CARGO_TARGET_DIR`).
    pub fn env(&self, home: &Path, target: Option<&Path>, extra: &[(&str, &str)]) -> Result<Env, Fault> {
        make_dirs(home, true).map_err(|e| Fault::os(&e, home))?;
        let mut env = scrubbed();
        env.insert("BLAZE_HOME".to_owned(), shown(home));
        env.insert("BLAZE_STATS".to_owned(), "1".to_owned());
        let path = env.get("PATH").cloned().unwrap_or_default();
        let dir = self.blaze.parent().map(shown).unwrap_or_default();
        env.insert("PATH".to_owned(), format!("{dir}:{path}"));
        if let Some(target) = target {
            env.insert("CARGO_TARGET_DIR".to_owned(), shown(target));
        }
        for (k, v) in extra {
            env.insert((*k).to_owned(), (*v).to_owned());
        }
        Ok(env)
    }

    /// `blaze COMMAND ARGS` in `ws`.
    pub fn command(&self, step: &str, command: &str, args: &[String], ws: &Path, env: &Env, timeout: f64) -> Result<Ran, Fault> {
        let mut words = vec![shown(&self.blaze), command.to_owned()];
        words.extend_from_slice(args);
        run(step, &words, ws, env, timeout, false)
    }

    pub fn stop(&self, ws: &Path, env: &Env) -> Result<(), Fault> {
        self.command("stopping the daemon", "stop", &[], ws, env, 300.0).map(|_| ())
    }

    /// Where the builds of `ws` under `env` put their outputs: the `CARGO_TARGET_DIR` `env`
    /// names, else the daemon's default for `ws` under its home.
    pub fn build_dir(&self, ws: &Path, env: &Env) -> Result<PathBuf, Fault> {
        if let Some(named) = env.get("CARGO_TARGET_DIR").filter(|it| !it.is_empty()) {
            return Ok(PathBuf::from(named));
        }
        default_build_dir(Path::new(env.get("BLAZE_HOME").map_or("", String::as_str)), ws)
    }

    /// What this harness assumes of the binary, held to the binary before any build; each miss
    /// is a `Failed` naming the assumption and what the binary said instead.
    fn preflight(&mut self) -> Result<(), Fault> {
        let base = if Path::new("/tmp").is_dir() { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let tmp = base.join(unique("bz-pre-"));
        std::fs::create_dir(&tmp).map_err(|e| Fault::os(&e, &tmp))?;
        let checked = self.preflight_in(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        checked
    }

    fn preflight_in(&mut self, tmp: &Path) -> Result<(), Fault> {
        let home = std::fs::canonicalize(tmp).unwrap_or_else(|_| tmp.to_owned()).join("h");
        let env = self.env(&home, None, &[])?;
        let blaze = shown(&self.blaze);
        let what = format!("preflight of {blaze}");
        let said = run(&format!("{what}: `blaze --version`"), &[blaze.clone(), "--version".to_owned()], &home, &env, 60.0, true)?.stdout;
        let words = pystr::split_whitespace(&said);
        if words.len() < 2 || words[0] != "blaze" {
            return failed(format!("{what}: `blaze --version` said {}, not `blaze <version> ...`", repr_str(pystr::strip(&said))));
        }
        self.version = pystr::strip(&said).to_owned();
        let version = self.version.clone();
        let listed = run(&format!("{what}: `blaze --help`"), &[blaze.clone(), "--help".to_owned()], &home, &env, 60.0, true)?.stdout;
        let missing: Vec<String> =
            HELP_LISTS.iter().filter(|name| !help_lists(&listed, name)).map(|name| format!("`{name}`")).collect();
        if !missing.is_empty() {
            return failed(format!(
                "{what} ({version}): `blaze --help` lists no {}, which the harness runs (engine.py and the scripts over it); the harness is written against another blaze's commands",
                missing.join(", ")
            ));
        }
        let status = blaze_status(&blaze, &env, &home, 60.0).map_err(|(_, e)| {
            Fault::Failed(format!("{what} ({version}): `blaze status --json` with BLAZE_HOME={}: {e}", shown(&home)))
        })?;
        if let Some(daemon) = &status.daemon {
            return failed(format!("{what}: a daemon (pid {}) answers in the new home {}", daemon.pid, shown(&home)));
        }
        let want = layout(&home);
        for (name, got, wanted) in
            [("home", &status.home, &want.home), ("state", &status.state, &want.state), ("socket", &status.socket, &want.socket)]
        {
            if real_path(Path::new(got)) != real_path(wanted) {
                return failed(format!(
                    "{what} ({version}): with BLAZE_HOME={}, `blaze status --json` says its {name} is {got}, and the harness lays out {} (storelib.layout): the daemon's rule for where it keeps things moved, and the layout has to follow it",
                    shown(&home),
                    shown(wanted)
                ));
            }
        }
        Ok(())
    }

    /// None when blaze built what `ran` asked for; otherwise who did, and why. v2 has no path to
    /// cargo, so always None.
    pub fn not_blazes(&self, _ran: &Ran) -> Option<String> {
        None
    }

    /// `blaze COMMAND ARGS` (build, run, test, check), with what it did recorded by its home. A
    /// command that fails is returned as it is; what it said is the caller's to compare.
    pub fn build(&self, step: &str, ws: &Path, env: &Env, args: &[String], command: &str, timeout: f64) -> Result<Ran, Fault> {
        let args = dev_backend_args(&self.spec.dev_backend, command, args);
        let ran = self.command(step, command, &args, ws, env, timeout)?;
        let home = env.get("BLAZE_HOME").cloned().unwrap_or_default();
        let mut records = self.records();
        records.took.push((step.to_owned(), ran.seconds));
        if WORKING.contains(&command) {
            let mut words = vec!["blaze".to_owned(), command.to_owned()];
            words.extend(args.iter().cloned());
            let label = format!("{step} (`{}`)", words.join(" "));
            let mut lines = if args.iter().any(|it| it == "--message-format=json") { worked_lines(&ran.stdout) } else { Vec::new() };
            if lines.len() > 1 {
                return failed(format!("{label}: {} `blaze-work` lines; a request reports what it did once", lines.len()));
            }
            let line = lines.pop();
            records.worked.entry(home.clone()).or_default().push((label, line));
            records.reported.entry(home).or_default().push(report_lines(&ran.stdout));
        }
        drop(records);
        Ok(ran)
    }

    /// `build` with no arguments, the command `build` and the default timeout.
    pub fn build_plain(&self, step: &str, ws: &Path, env: &Env) -> Result<Ran, Fault> {
        self.build(step, ws, env, &[], "build", 7200.0)
    }

    /// Every command through `build`, in order: its step and its seconds.
    pub fn took(&self) -> Vec<(String, f64)> {
        self.records().took.clone()
    }

    /// Seconds until the daemon has `ws` loaded and can serve it: `blaze status --json`'s
    /// `daemon` asked until it is there.
    pub fn wait_ready(&self, ws: &Path, env: &Env, timeout: f64) -> Result<f64, Fault> {
        let began = Instant::now();
        let mut said = "it was not asked".to_owned();
        let blaze = shown(&self.blaze);
        // Bounded: it ends at `timeout`.
        while began.elapsed().as_secs_f64() < timeout {
            match blaze_status(&blaze, env, ws, 300.0) {
                Ok(status) if status.daemon.is_some() => return Ok(began.elapsed().as_secs_f64()),
                Ok(_) => said = "`blaze status --json` says no daemon is running".to_owned(),
                Err((name, e)) => said = format!("{name}: {e}"),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        failed(format!("the daemon did not answer `blaze status --json` for {} in {timeout:.0} s: {said}", shown(ws)))
    }

    /// Where the record of what the daemon of `home` did is now.
    pub fn mark(&self, home: &Path) -> Mark {
        let n = self.records().worked.get(&shown(home)).map_or(0, Vec::len);
        Mark { sizes: BTreeMap::from([("requests".to_owned(), n)]) }
    }

    /// What the builds since `mark` did.
    pub fn work_since(&self, home: &Path, mark: &Mark) -> Result<Work, Fault> {
        let key = shown(home);
        let from = mark.sizes.get("requests").copied().unwrap_or(0);
        let records = self.records();
        let since: Vec<(String, Option<Map<String, Value>>)> =
            records.worked.get(&key).map(|all| all.iter().skip(from).cloned().collect()).unwrap_or_default();
        let reports: Vec<String> =
            records.reported.get(&key).map(|all| all.iter().skip(from).flatten().cloned().collect()).unwrap_or_default();
        drop(records);
        if since.is_empty() {
            return failed(format!("what the daemon of {key} did: no build, test or run since the mark"));
        }
        let mut lines: Vec<(&str, &Map<String, Value>)> = Vec::new();
        for (label, line) in &since {
            lines.push((label, required(label, line.as_ref())?));
        }
        let total = |field: &str| -> Result<u64, Fault> {
            let mut sum = 0;
            for (label, line) in &lines {
                sum += work_count(label, line, field)?;
            }
            Ok(sum)
        };
        if lines.iter().any(|(_, line)| !line.contains_key("links")) {
            return failed(format!("what the daemon of {key} did: a `blaze-work` line without `links`"));
        }
        let mut links: Vec<(&str, &Map<String, Value>)> = Vec::new();
        for (label, line) in &lines {
            for it in line["links"].as_array().into_iter().flatten() {
                if let Value::Object(it) = it {
                    links.push((label, it));
                }
            }
        }
        let how = |it: &Map<String, Value>| it.get("how").and_then(Value::as_str).map(str::to_owned);
        let path = |it: &Map<String, Value>| crate::pyfmt::py_str(it.get("path").unwrap_or(&Value::Null));
        let link = if let Some((label, it)) = links.iter().find(|(_, it)| how(it).as_deref() == Some("full")) {
            let why: Vec<String> =
                it.get("why").and_then(Value::as_array).into_iter().flatten().map(crate::pyfmt::py_str).collect();
            format!("placed whole: {label}: {} ({})", path(it), why.join(", "))
        } else if let Some((label, it)) = links.iter().find(|(_, it)| how(it).as_deref() == Some("first")) {
            format!("first link: {label}: {}", path(it))
        } else {
            "patched in place".to_owned()
        };
        let mut said: Vec<String> = lines.iter().map(|(_, line)| dumps_sorted(&Value::Object((*line).clone()))).collect();
        said.extend(reports.iter().cloned());
        Ok(Work {
            compiled: Some(total("compiled")?),
            reran: Some(total("reran")?),
            found: Some(total("found")?),
            link: Some(link),
            said: said.join("\n"),
            green: Some(total("green")?),
            reports: Some(reports.len() as u64),
            causes: reports,
        })
    }

    /// The input of every function's code generation in the last build of `ws`, by the
    /// instance's content key (`blaze codegen-inputs --format=json`).
    pub fn codegen_inputs(&self, ws: &Path, env: &Env) -> Result<BTreeMap<String, String>, Fault> {
        let ran = self.command("the codegen inputs", "codegen-inputs", &["--format=json".to_owned()], ws, env, 600.0)?;
        let said = ran.said(3000);
        match json_answer("the codegen inputs", ran)? {
            Value::Object(inputs) if inputs.values().all(Value::is_string) => {
                Ok(inputs.into_iter().map(|(k, v)| (k, v.as_str().unwrap_or("").to_owned())).collect())
            }
            _ => failed(format!("`blaze codegen-inputs --format=json` printed no object of hex strings:\n{said}")),
        }
    }

    /// The length in seconds of the longest chain of work the last cold build of `ws` waited on.
    pub fn critical_path_s(&self, ws: &Path, env: &Env) -> Result<f64, Fault> {
        let home = env.get("BLAZE_HOME").cloned().unwrap_or_default();
        let last = self.records().worked.get(&home).and_then(|kept| kept.last().cloned());
        let Some((label, line)) = last else {
            return failed(format!("the critical path of the last build of {}: no build of it went through the harness", shown(ws)));
        };
        let line = required(&label, line.as_ref())?;
        Ok(work_count(&label, line, "critical_path_us")? as f64 / 1e6)
    }

    /// The daemon's tables for `ws` by name (`blaze tables --format=json`).
    pub fn tables(&self, ws: &Path, env: &Env) -> Result<BTreeMap<String, Table>, Fault> {
        let ran = self.command("the tables", "tables", &["--format=json".to_owned()], ws, env, 600.0)?;
        let said = ran.said(3000);
        let Value::Object(answer) = json_answer("the tables", ran)? else {
            return failed(format!("`blaze tables --format=json` printed no object:\n{said}"));
        };
        let mut out = BTreeMap::new();
        for (name, sizes) in answer {
            let label = format!("`blaze tables` table {name}");
            let Value::Object(sizes) = &sizes else {
                return failed(format!("{label}: {} is not {{\"elements\": N, \"bytes\": N}}", repr(&sizes)));
            };
            out.insert(name, Table { elements: work_count(&label, sizes, "elements")?, bytes: work_count(&label, sizes, "bytes")? });
        }
        Ok(out)
    }

    /// Writes to `to` what the last build of `ws` left in the query database (`blaze trace`).
    pub fn trace(&self, ws: &Path, env: &Env, to: &Path) -> Result<(), Fault> {
        let _ = std::fs::remove_file(to);
        self.command("the trace", "trace", &["--out".to_owned(), shown(to)], ws, env, 600.0)?.checked("the trace")?;
        if !to.is_file() {
            return failed(format!("`blaze trace --out {}` exited 0 and wrote no file", shown(to)));
        }
        Ok(())
    }
}

/// `os.path.realpath`: the real path where the path is there, else as given.
fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(crate::pypath::abspath(&shown(path))))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(it) => it,
            _ => Map::new(),
        }
    }

    #[test]
    fn help_lines_are_read_whole() {
        let listed = "Commands:\n  build, b   Build\n  debug sql QUERY  ask\n  tables-of  x\n  status\n";
        assert!(help_lists(listed, "build") && help_lists(listed, "debug sql") && help_lists(listed, "status"));
        assert!(!help_lists(listed, "tables") && !help_lists(listed, "bui"));
    }

    #[test]
    fn the_profile_is_cargos() {
        let words = |s: &str| argv(&s.split_whitespace().collect::<Vec<_>>());
        assert_eq!(profile_of("build", &words("")), "dev");
        assert_eq!(profile_of("test", &words("")), "test");
        assert_eq!(profile_of("build", &words("-r")), "release");
        assert_eq!(profile_of("build", &words("--profile dist --release")), "dist");
        assert_eq!(profile_of("build", &words("--profile=x")), "x");
        assert_eq!(profile_of("run", &words("-- --release")), "dev");
        assert_eq!(dev_backend_args("default", "build", &words("-p a")), words("-p a"));
        assert_eq!(dev_backend_args("stencil", "check", &words("-p a")), words("-p a"));
        assert_eq!(
            dev_backend_args("stencil", "run", &words("-p a -- x")),
            argv(&["-p", "a", "-Zcodegen-backend", "--config", "profile.dev.codegen-backend=\"stencil\"", "--", "x"])
        );
    }

    #[test]
    fn work_lines_are_read_and_checked() {
        let out = "x\n{\"reason\": \"blaze-work\", \"reran\": 1}\n{\"reason\": \"other\", \"x\": \"blaze-work\"}\n{bad \"blaze-work\"\n\
            {\"reason\": \"blaze-report\", \"rule\": \"why:edit\"}\n{\"reason\": \"blaze-report\", \"rule\": \"lint\"}\n";
        assert_eq!(worked_lines(out).len(), 1);
        assert_eq!(report_lines(out), vec!["{\"reason\": \"blaze-report\", \"rule\": \"why:edit\"}".to_owned()]);
        let good = obj(json!({"reran": 1, "green": 0, "compiled": 2, "found": 3, "critical_path_us": 9, "link": "placed"}));
        assert!(required("b", Some(&good)).is_ok());
        let said = required("b", None).unwrap_err();
        assert!(said.said().starts_with("b: no `blaze-work` line"));
        let mut bad = good.clone();
        bad.insert("reran".to_owned(), json!(true));
        assert_eq!(
            work_count("b", &bad, "reran").unwrap_err().said(),
            "b: `blaze-work` has reran True, not a count: {'reran': True, 'green': 0, 'compiled': 2, 'found': 3, 'critical_path_us': 9, 'link': 'placed'}"
        );
        let mut odd = good;
        odd.insert("link".to_owned(), json!("full"));
        assert!(required("b", Some(&odd)).unwrap_err().said().contains("has link 'full', not \"patched\" or \"placed\""));
    }

    #[test]
    fn a_build_directory_name_is_blazes() {
        assert_eq!(build_dir_name(Path::new("/w/my ws.é")), "my_ws._-");
        assert_eq!(build_dir_name(Path::new("/")), "");
        let home = crate::TempDir::new("blaze-script-engine-").unwrap();
        let builds = home.path().join("build");
        std::fs::create_dir_all(builds.join(format!("ws-{}", "a".repeat(32)))).unwrap();
        std::fs::create_dir_all(builds.join("ws-nothex")).unwrap();
        assert_eq!(default_build_dir(home.path(), Path::new("/x/ws")).unwrap(), builds.join(format!("ws-{}", "a".repeat(32))));
        assert!(default_build_dir(home.path(), Path::new("/x/other")).unwrap_err().said().contains("0 directories named `other-<32 hex digits>`"));
    }

    #[test]
    fn a_status_is_read_as_storelib_reads_it() {
        let none = r#"{"format": 1, "version": "v", "api_schema": 3, "home": "/h", "state": "/h/v2", "socket": "/h/v2.sock", "daemon": null}"#;
        let status = blaze_status_of(none).unwrap();
        assert!(status.daemon.is_none() && status.home == "/h");
        assert_eq!(blaze_status_of(&none.replace("\"format\": 1", "\"format\": 2")).unwrap_err().said(), "`blaze status --json` is of format 2, this harness reads 1");
        assert_eq!(
            blaze_status_of(&none.replace("\"daemon\"", "\"extra\": 1, \"daemon\"")).unwrap_err().said(),
            "`blaze status --json` has fields this harness does not know: extra"
        );
        assert!(blaze_status_of(&none.replace("\"/h\"", "1")).unwrap_err().said().starts_with("`blaze status --json`'s `home` is 1, not str: {'format': 1"));
        let running = none.replace(
            "null",
            r#"{"pid": 7, "build": "ab", "exe": "/b", "uptime_ms": 1, "memory_bytes": 2, "budget_bytes": 3, "bugs": 0, "requests": [1], "views": 0, "history": null, "settings": {"b": "x", "a": null}}"#,
        );
        let daemon = blaze_status_of(&running).unwrap().daemon.unwrap();
        assert_eq!((daemon.pid, daemon.settings), (7, vec![("a".to_owned(), None), ("b".to_owned(), Some("x".to_owned()))]));
    }

    #[test]
    fn a_run_is_timed_and_its_deadline_kept() {
        let env = scrubbed();
        let here = Path::new(".");
        let done = run("s", &argv(&["sh", "-c", "printf 'a\\r\\nb'; echo e >&2; exit 3"]), here, &env, 10.0, false).unwrap();
        assert_eq!((done.returncode, done.stdout.as_str(), done.stderr.as_str()), (3, "a\nb", "e\n"));
        assert!(run("s", &argv(&["sh", "-c", "exit 3"]), here, &env, 10.0, true).unwrap_err().said().starts_with("s: `sh -c exit 3` in . exited 3:"));
        let started = Instant::now();
        let late = run("s", &argv(&["sh", "-c", "echo early; sleep 5"]), here, &env, 0.3, false).unwrap_err();
        assert_eq!(late.said(), "s: `sh -c echo early; sleep 5` in . took more than 0 s:\nearly\n");
        assert!(started.elapsed() < Duration::from_secs(4));
        let killed = run("s", &argv(&["sh", "-c", "kill -9 $$"]), here, &env, 10.0, false).unwrap();
        assert_eq!(killed.returncode, -9);
        assert!(run("s", &argv(&["/nonexistent/x"]), here, &env, 10.0, false).unwrap_err().said().ends_with("did not start: [Errno 2] No such file or directory: '/nonexistent/x'"));
    }

    #[test]
    fn files_differ_where_they_differ() {
        let dir = crate::TempDir::new("blaze-script-engine-").unwrap();
        let (a, b, c) = (dir.path().join("a"), dir.path().join("b"), dir.path().join("c"));
        std::fs::write(&a, b"abcd").unwrap();
        std::fs::write(&b, b"abXd").unwrap();
        std::fs::write(&c, b"ab").unwrap();
        assert_eq!(first_difference(&a, &b).unwrap(), "byte 0x2: 0x63 against 0x58");
        assert_eq!(first_difference(&a, &c).unwrap(), "one ends at byte 0x2");
        assert_eq!(first_difference(&a, &a).unwrap(), "the same");
    }

    #[test]
    fn python_strings_are_cut_by_characters() {
        assert_eq!(last_chars("héllo", 3), "llo");
        assert_eq!(last_chars("hé", 3), "hé");
        assert_eq!(first_chars("héllo", 2), "hé");
        assert_eq!(dumps_sorted(&json!({"b": 1, "a": {"d": 2, "c": [1]}})), r#"{"a": {"c": [1], "d": 2}, "b": 1}"#);
        assert_eq!(layout(Path::new("/h")).store, PathBuf::from("/h/v2/store"));
    }
}

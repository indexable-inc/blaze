//! `blaze serve` (docs/v2/plan/lanes/cargo/notes.md section 6): v2's daemon. One loop over the
//! platform's driver holds the API server, the session, the VFS, the database, the store and
//! the plane, and builds what the CLI and agents ask through them.
//!
//! - Its socket is `<home>/v2.sock` ([`home`]);
//!   `--socket PATH` names another, `--http ADDR` also listens on TCP with the bearer token in
//!   `BLAZE_API_TOKEN`.
//! - What it keeps lives in [`state`]: the VFS's journal, the database's image, the store and
//!   the plane's estimates. A start recovers the VFS from the journal and the database from the
//!   image. An image of another blaze starts that state empty by a rule, said once at the start
//!   ([`daemon::Started`]); an image that is damaged, or does not match its journal, stops the
//!   start naming it ([`daemon::StartError`]), and one found corrupt while running stops the
//!   daemon ([`daemon::Failed`]).
//! - It stops on `blaze stop` ([`blaze_api::Request::Stop`]) or SIGTERM, writing its image first.
//! - It keeps the profile history in `<state>/profile` on the thread `blaze-history`, samples
//!   its CPUs, and answers `blaze explore`, `blaze debug live` and `blaze debug sql --live`
//!   itself ([`observe`]).

mod daemon;
mod fetch;
mod gc;
mod heads;
mod image;
mod job;
mod mounts;
pub mod observe;
/// The seed's one-shot build: this engine in the CLI's process, for its command alone.
#[cfg(not(feature = "daemon"))]
pub(crate) mod once;
mod plane;
pub(crate) mod signals;
mod still;
#[cfg(test)]
mod tests;
mod unit;
pub mod why;
mod work;

#[cfg(feature = "daemon")]
use std::{ffi::OsString, process::ExitCode};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, bail};
use blaze_core::{Confine, FileName};

pub use daemon::{Daemon, Options};

/// What the line a daemon writes to its log once it listens starts with, then where and its
/// pid: `blaze serve: listening on /run/blaze/v2.sock (pid 7)`. The client finds the daemon's
/// last start by it, and harness/storelib.py's `LISTENING` that it listens.
#[cfg(feature = "daemon")]
pub const LISTENING: &str = "blaze serve: listening on ";

/// Where the daemon is and what it runs.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Its state ([`state`]).
    pub state: PathBuf,
    /// `None` for the one-shot build, which takes its own process's requests alone.
    pub socket: Option<PathBuf>,
    /// blaze's own executable: the sandbox's launcher.
    pub launcher: PathBuf,
}

/// Where the daemon's socket is ([`blaze_api::client::socket`]).
pub fn socket() -> anyhow::Result<PathBuf> {
    blaze_api::client::socket().map_err(anyhow::Error::msg)
}

/// Where the daemon's socket lives ([`blaze_api::client::home`]).
pub fn home() -> anyhow::Result<PathBuf> {
    blaze_api::client::home().map_err(anyhow::Error::msg)
}

/// Where the daemon keeps what outlives it: `v2` under blaze's root
/// ([`blaze_manifest::home::root`]: `$BLAZE_HOME`, else `blaze` in the user's cache directory,
/// `XDG_CACHE_HOME` or `~/.cache`). Unlike the socket it is never in `XDG_RUNTIME_DIR`, which
/// does not outlive the session.
pub fn state() -> anyhow::Result<PathBuf> {
    Ok(blaze_manifest::home::state(&root()?))
}

/// Where every workspace's default build directory is: `build` under blaze's root, beside
/// [`state`] and not in it ([`blaze_manifest::home::builds`]).
pub fn builds() -> anyhow::Result<PathBuf> {
    Ok(blaze_manifest::home::builds(&root()?))
}

/// Where the daemon keeps the packages it fetches, mounted and on disk: `sources` under blaze's
/// root, beside [`state`] ([`blaze_manifest::home::sources`]). Never `$CARGO_HOME`.
pub fn sources() -> anyhow::Result<PathBuf> {
    Ok(blaze_manifest::home::sources(&root()?))
}

/// blaze's root of this process ([`blaze_manifest::home::root`]), which [`state`],
/// [`builds`] and [`sources`] are under.
fn root() -> anyhow::Result<PathBuf> {
    blaze_manifest::home::process_root()
        .map_err(anyhow::Error::msg)?
        .context("no BLAZE_HOME, XDG_CACHE_HOME or HOME says where blaze keeps its state and build directories")
}

/// Where every daemon of this machine, whatever its `BLAZE_HOME`, keeps what they share:
/// `blaze` in the user's cache directory (`XDG_CACHE_HOME`, or `~/.cache`).
pub fn machine() -> anyhow::Result<PathBuf> {
    let dir = match std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()) {
        Some(caches) => PathBuf::from(caches).join("blaze"),
        None => {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
            PathBuf::from(home.context("no XDG_CACHE_HOME or HOME says where this machine's daemons share the standard library")?)
                .join(".cache/blaze")
        }
    };
    // A relative one is taken against the current directory, as cargo takes its homes (review
    // round 6, C4).
    std::path::absolute(&dir).with_context(|| format!("the machine's shared directory {} against the current directory", dir.display()))
}

/// Which blaze `exe` is: the hash of its bytes, so two builds of one commit, a dirty tree's,
/// are two. What the daemons of one build share is under it, as only that build made it.
pub fn build_id(exe: &Path) -> anyhow::Result<blaze_ids::ContentId> {
    Ok(build_of(exe)?.id)
}

/// [`build_id`] of `exe` with the file it hashed ([`executable`]), as one open file was both:
/// one that changed while it was read is an error.
pub fn build_of(exe: &Path) -> anyhow::Result<blaze_api::Build> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(exe).with_context(|| format!("reading blaze itself, {}", exe.display()))?;
    let before = executable(exe, &file)?;
    let mut hasher = blaze_ids::Hasher::new("blaze v2 build id 1");
    let mut chunk = vec![0u8; 1 << 20]; // alloc: one buffer, once per start
    loop {
        let read = file.read(&mut chunk).with_context(|| format!("reading blaze itself, {}", exe.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    if executable(exe, &file)? != before {
        bail!("blaze itself, {}, changed while it was read for its build id", exe.display());
    }
    Ok(blaze_api::Build { id: hasher.finish(), exe: before })
}

/// The open file `file` of `exe` as a [`blaze_api::Executable`]: what changes when its bytes do.
pub fn executable(exe: &Path, file: &std::fs::File) -> anyhow::Result<blaze_api::Executable> {
    let meta = file.metadata().with_context(|| format!("reading what blaze itself, {}, is", exe.display()))?;
    let modified =
        meta.modified().with_context(|| format!("the modification time of blaze itself, {}", exe.display()))?;
    let modified_ns = modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |it| it.as_nanos() as u64);
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    let path = exe.to_str().with_context(|| format!("blaze itself, {}, is not at a UTF-8 path", exe.display()))?;
    Ok(blaze_api::Executable { path: path.to_owned(), len: meta.len(), modified_ns, inode })
}

/// The daemon of `state` as a writer of a shared store: one per `BLAZE_HOME`, as one daemon
/// at a time has a state directory.
fn writer_of(state: &Path) -> anyhow::Result<blaze_ids::ContentId> {
    let text = state.to_str().with_context(|| format!("{} is not UTF-8", state.display()))?;
    Ok(blaze_ids::Hasher::new("blaze v2 shared store writer 1").part(text.as_bytes()).finish())
}

/// An absolute path as the driver names it: the driver's root is `/`.
pub fn file_name(path: &Path) -> anyhow::Result<FileName> {
    let text = path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))?;
    let Some(rest) = text.strip_prefix('/') else { bail!("{text} is not an absolute path") };
    FileName::new(rest.trim_end_matches('/')).with_context(|| format!("{text} has an empty, `.` or `..` part"))
}

#[cfg(feature = "daemon")]
struct Args {
    socket: Option<PathBuf>,
    http: Option<SocketAddr>,
    /// Listen on the socket systemd or launchd passed, never one of its own.
    activated: bool,
    /// The descriptor of the pipe the CLI that started it waits on, told one byte once it
    /// listens ([`crate::cli::daemon`]).
    ready: Option<i32>,
    /// `--stall-bound SECS`: the stall watchdog's bound ([`daemon::Options::stall`]).
    stall: Option<std::time::Duration>,
}

#[cfg(feature = "daemon")]
fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Args> {
    let mut parsed = Args { socket: None, http: None, activated: false, ready: None, stall: None };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg.into_string().map_err(|it| anyhow::anyhow!("{} is not UTF-8", it.to_string_lossy()))?;
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = || -> anyhow::Result<String> {
            match inline.clone() {
                Some(value) => Ok(value),
                None => args
                    .next()
                    .with_context(|| format!("{flag} takes a value"))?
                    .into_string()
                    .map_err(|it| anyhow::anyhow!("{} is not UTF-8", it.to_string_lossy())),
            }
        };
        match flag.as_str() {
            "--activated" if inline.is_none() => parsed.activated = true,
            "--socket" => parsed.socket = Some(PathBuf::from(value()?)),
            "--ready-fd" => {
                let fd = value()?;
                parsed.ready = Some(fd.parse().with_context(|| format!("--ready-fd {fd} is no descriptor"))?);
            }
            "--http" => {
                let at = value()?;
                parsed.http = Some(at.parse().with_context(|| format!("--http {at} is no address and port"))?);
            }
            "--stall-bound" => {
                let secs = value()?;
                let bound = secs.parse::<u64>().ok().filter(|it| *it > 0).with_context(|| {
                    format!("--stall-bound {secs}: the stall bound is a whole number of seconds above 0")
                })?;
                parsed.stall = Some(std::time::Duration::from_secs(bound));
            }
            _ => bail!(
                "blaze serve takes `--socket PATH`, `--http ADDR`, `--activated`, `--ready-fd N` and \
                 `--stall-bound SECS`, not `{arg}`"
            ),
        }
    }
    Ok(parsed)
}

/// The socket systemd or launchd passed `blaze serve --activated` (the units of
/// `blaze daemon install`, [`blaze_api::units`]).
#[cfg(all(unix, feature = "daemon"))]
fn activated() -> anyhow::Result<blaze_api::activation::Passed> {
    use blaze_api::activation::{Manager, take};
    let manager = Manager::here().context(
        "blaze serve --activated: no service manager blaze is socket-activated by runs here (systemd on Linux, launchd on macOS)",
    )?;
    // SAFETY: `cli::main` dispatches `serve` before anything else runs, and `serve` calls this
    // first, before it starts a thread: no other thread touches the environment. Nothing in
    // the process has opened or owns the descriptors the manager passed (systemd's from 3).
    Ok(unsafe { take(manager) }?)
}

/// What `blaze serve --ready-fd N` writes on that pipe: a byte as it enters each phase of its
/// start, in order, then [`Phase::Listening`]'s once its socket is bound. That last is the
/// word of the CLI that started it to connect, which it never does before; when the start
/// stops short, the CLI names the last phase it read.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Phase {
    /// Nothing said yet: the process started, and finds its executable's build and its state.
    Launched,
    /// It waits, at most [`BEFORE_ENDS`], for the daemon before it on its state to end.
    Waiting,
    /// It reads its state: the VFS journal, the daemon image and its store, and binds.
    State,
    /// It installs its store and sets up the toolchain's proc macros.
    Setup,
    Listening,
}

impl Phase {
    pub const fn byte(self) -> u8 {
        match self {
            Phase::Launched => b'0',
            Phase::Waiting => b'w',
            Phase::State => b's',
            Phase::Setup => b'p',
            Phase::Listening => b'\n',
        }
    }

    pub fn of(byte: u8) -> Option<Phase> {
        [Phase::Launched, Phase::Waiting, Phase::State, Phase::Setup, Phase::Listening]
            .into_iter()
            .find(|it| it.byte() == byte)
    }

    /// What the daemon does in it, as a line says.
    pub const fn doing(self) -> &'static str {
        match self {
            Phase::Launched => "finding its build and its state",
            Phase::Waiting => "waiting for the daemon before it, which writes its image, to end",
            Phase::State => "reading its state (the VFS journal, the daemon image and the store)",
            Phase::Setup => "installing its store and the toolchain's proc macros",
            Phase::Listening => "listening",
        }
    }
}

/// How long a daemon that starts waits for the one before it on the same state to end: that
/// one writes its image as it does, gigabytes of it.
pub const BEFORE_ENDS: Duration = Duration::from_secs(300);

/// A daemon on the same state as one still alive: it never starts over one.
#[derive(Debug)]
pub struct StillRuns {
    /// The pid the lock's file names, as the daemon that holds it wrote.
    pub pid: String,
    pub lock: PathBuf,
}

impl std::fmt::Display for StillRuns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the daemon before this one on its state (pid {}) still holds {} after {}s: it has not ended, \
             and this one does not start over it; `kill {}` ends it",
            self.pid,
            self.lock.display(),
            BEFORE_ENDS.as_secs(),
            self.pid
        )
    }
}

impl std::error::Error for StillRuns {}

/// The daemon's hold of its state, `serve.lock` in it, which names its pid: locked for the
/// process's life (the kernel lets it go as the process ends, however it ends), so a daemon
/// started while the one before it still ends (writing its image, then letting its socket's
/// name go) waits for it rather than takes its place. At most [`BEFORE_ENDS`]; after,
/// [`StillRuns`].
#[cfg(unix)]
fn hold(state: &Path, ready: Option<&mut std::fs::File>) -> anyhow::Result<std::fs::File> {
    use std::{
        io::{Read as _, Seek as _, Write as _},
        os::unix::fs::OpenOptionsExt as _,
    };
    let lock = state.join("serve.lock");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock)
        .with_context(|| format!("opening the daemon's lock {}", lock.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::Error(error)) => bail!("locking the daemon's state at {}: {error}", lock.display()),
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut written = String::new();
            // alloc: once a start that waits
            let pid = match file.rewind().and_then(|()| file.read_to_string(&mut written)) {
                Ok(_) if !written.trim().is_empty() => written.trim().to_owned(),
                Ok(_) => "not yet written".to_owned(),
                Err(error) => format!("unread: {error}"),
            };
            say(ready, Phase::Waiting);
            eprintln!(
                "blaze serve: pid {} waits at most {}s for the daemon before it on this state (pid {pid}), \
                 which holds {}, to end",
                std::process::id(),
                BEFORE_ENDS.as_secs(),
                lock.display()
            );
            // A second descriptor of the same open file, whose lock is this one's: `lock` has no
            // timed wait, so it waits on a thread of its own, and this one at most `BEFORE_ENDS`
            // for its word. Past that this process ends, the thread with it.
            let other = file.try_clone().with_context(|| format!("opening the daemon's lock {} again", lock.display()))?;
            let (locked, word) = crossbeam_channel::bounded(1);
            std::thread::Builder::new()
                .name("blaze-serve-lock".to_owned()) // alloc: once a start that waits
                .spawn(move || drop(locked.send(other.lock())))
                .with_context(|| format!("starting the thread that waits on {}", lock.display()))?;
            match word.recv_timeout(BEFORE_ENDS) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => bail!("locking the daemon's state at {}: {error}", lock.display()),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => return Err(StillRuns { pid, lock }.into()),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    bail!("the thread waiting on the daemon's lock {} ended without a word", lock.display())
                }
            }
        }
    }
    file.set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| write!(file, "{}", std::process::id()))
        .with_context(|| format!("writing this daemon's pid to its lock {}", lock.display()))?;
    Ok(file)
}

/// Says `phase` on the ready pipe, if there is one. The CLI that waits may be gone, and then
/// nobody is to be told.
fn say(ready: Option<&mut std::fs::File>, phase: Phase) {
    if let Some(ready) = ready {
        _ = std::io::Write::write_all(ready, &[phase.byte()]);
    }
}

/// The pipe of `blaze serve --ready-fd N`, made this process's: close-on-exec from here on.
#[cfg(all(unix, feature = "daemon"))]
fn ready(fd: i32) -> anyhow::Result<std::fs::File> {
    use std::os::fd::FromRawFd as _;
    if fd < 3 {
        bail!("blaze serve --ready-fd {fd}: a standard stream is not the pipe");
    }
    // SAFETY: takes a number and flags and reads no memory of this process.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        bail!("blaze serve --ready-fd {fd}: {}", std::io::Error::last_os_error());
    }
    // SAFETY: open (the call above found it), passed for this alone by the CLI that started
    // the process, and owned by nothing else in it: `serve` takes it before it opens anything.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// `blaze serve [--socket PATH] [--http ADDR] [--activated] [--ready-fd N] [--stall-bound SECS]`.
#[cfg(feature = "daemon")]
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "daemon")]
fn serve(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<()> {
    let args = parse(args)?;
    // First, before any thread starts: taking the passed socket reads and clears the
    // environment.
    #[cfg(unix)]
    let activated = match args.activated {
        true => Some(activated()?),
        false => None,
    };
    #[cfg(not(unix))]
    if args.activated {
        bail!("blaze serve --activated: socket activation is systemd's and launchd's, and this system has neither");
    }
    // First too: nothing this process starts may inherit the pipe.
    #[cfg(unix)]
    let ready = args.ready.map(ready).transpose()?;
    #[cfg(not(unix))]
    let ready = match args.ready {
        Some(_) => bail!("blaze serve --ready-fd: the CLI starts the daemon so on Unix only"),
        None => None,
    };
    let socket = match args.socket {
        Some(socket) => std::path::absolute(&socket).with_context(|| format!("the socket {}", socket.display()))?,
        None => socket()?,
    };
    let mut ready = ready;
    let serving = Serving {
        socket: Some(socket),
        http: args.http,
        #[cfg(unix)]
        activated,
        stall: args.stall,
    };
    let engine = engine(serving, ready.as_mut())?;
    let mut running = start(engine, "blaze serve", ready.as_mut())?;
    let ran = listen(&mut running.daemon, ready);
    // The daemon is gone, and with it the history's sender: its thread's last flush ends it.
    running.end()?;
    ran
}

/// Where the engine takes its requests, and the bound of its stall watchdog.
pub(crate) struct Serving {
    /// The socket `blaze serve` listens on; `None` for the one-shot build ([`once`]), which
    /// takes its own process's requests alone.
    pub(crate) socket: Option<PathBuf>,
    /// HTTP too, with the bearer token in `BLAZE_API_TOKEN`.
    pub(crate) http: Option<SocketAddr>,
    /// The socket systemd or launchd passed, listened on at `socket`.
    #[cfg(unix)]
    pub(crate) activated: Option<blaze_api::activation::Passed>,
    /// `--stall-bound SECS` ([`daemon::Options::stall`]); `None` is [`blaze_profile::Watchdog::BOUND`].
    pub(crate) stall: Option<Duration>,
}

/// The engine `blaze serve` runs and the one-shot build runs in its own process: one setup
/// for both ([`engine`]), so the one-shot build is the daemon's engine, never a second path.
pub(crate) struct Engine {
    options: Options,
    steps: Arc<unit::Real>,
    runner: plane::Runner<job::Step<unit::Real>>,
    driver: blaze_core::Native,
    keeper: observe::Keeper,
    held: Held,
}

/// What the engine holds for its life: the state's lock, which the next engine on it waits on,
/// and the lease of the shared standard library's store.
struct Held {
    #[cfg(unix)]
    _lock: std::fs::File,
    _std_lease: blaze_gc::lease::Lease,
}

/// The engine, its state found and held, its driver made, before it starts: `ready`, the pipe
/// of `blaze serve --ready-fd`, hears if it waits for the engine before it on the same state.
pub(crate) fn engine(serving: Serving, ready: Option<&mut std::fs::File>) -> anyhow::Result<Engine> {
    let state = state()?;
    std::fs::create_dir_all(&state).with_context(|| format!("making the daemon's state directory {}", state.display()))?;
    // Held until the engine ends: the next one on this state waits on it.
    #[cfg(unix)]
    let lock = hold(&state, ready)?;
    #[cfg(not(unix))]
    let _ = ready;
    // Where a stall's trace is written (`daemon::Failed::Stalled`): by the loop, which makes no
    // directories.
    let stalls = state.join("stalls");
    std::fs::create_dir_all(&stalls).with_context(|| format!("making {}", stalls.display()))?;
    if let Some(parent) = serving.socket.as_deref().and_then(Path::parent) {
        std::fs::create_dir_all(parent).with_context(|| format!("making the socket's directory {}", parent.display()))?;
    }
    let token = match serving.http {
        Some(_) => Some(
            std::env::var("BLAZE_API_TOKEN")
                .ok()
                .filter(|it| !it.is_empty())
                .context("--http needs the bearer token its clients send in BLAZE_API_TOKEN")?,
        ),
        None => None,
    };
    let paths = Paths {
        state,
        socket: serving.socket,
        launcher: std::env::current_exe().context("the path of blaze itself, which launches the sandbox")?,
    };
    // dyn: the sandbox's `Confine`, asked once per spawn
    let confine: Arc<dyn Confine> = Arc::new(blaze_usercode::confine::Launcher { program: paths.launcher.clone() });
    let threads = std::thread::available_parallelism().context("the number of cores the daemon's plane works on")?.get();
    // Read once: the explorer and live SQL run this DuckDB; set wrong, the daemon does not start.
    let duckdb = observe::duckdb(std::env::var_os(observe::DUCKDB)).map_err(|why| anyhow::anyhow!(why))?;
    let (profiling, keeper) = observe::keep_history(&paths.state, duckdb).map_err(|why| anyhow::anyhow!(why))?;
    // Fixed for the daemon's life before any job: a build's tasks only read it.
    let sysroot = crate::toolchain::sysroot()
        .with_context(|| format!("blaze's toolchain ({}) is not on this machine: `blaze toolchain` installs it", crate::toolchain::name()))?;
    tracing::info!(sysroot = %sysroot.display(), "blaze's toolchain");
    let build = build_of(&paths.launcher)?;
    let std = machine()?.join("std").join(format!("{:x}", build.id));
    std::fs::create_dir_all(&std).with_context(|| format!("making the standard library's shared directory {}", std.display()))?;
    // Its lease, held for the daemon's life: no collector, this daemon's or another
    // process's, renames the shared store away while this daemon may read or write it
    // (docs/gc.md, "Leases"). Its record says when a daemon last started on it.
    let std_lease = blaze_gc::lease::Lease::hold(&std).map_err(|why| anyhow::anyhow!("the standard library's shared directory: {why}"))?;
    if let Some(why) = blaze_gc::record::touch(&std, None, &[], blaze_gc::record::now_ns()).map_err(|why| anyhow::anyhow!(why))? {
        tracing::warn!(target: "blaze", dir = %std.display(), "the shared store's record did not read and was written afresh: {why}");
    }
    let host = crate::toolchain::host()
        .context("blaze has no target triple for this machine's architecture and system")?
        .to_owned();
    // What kept expansions are keyed by: the whole blaze, as `unit::deps::build` is, since the
    // macro server and bridge that expand them are past the code generator's packages.
    let toolchain = build.id;
    let sources = sources()?;
    // A daemon that stopped mid-write left a package's half-written directory, which nothing
    // reads: removed before any fetch writes beside it.
    let swept = fetch::sweep_parts(&sources).map_err(|why| anyhow::anyhow!(why))?;
    if let Some(first) = swept.first() {
        tracing::info!(target: "blaze", removed = swept.len(), first = %first.display(), "half-written fetched packages removed");
    }
    let options = Options {
        state: file_name(&paths.state)?,
        std: file_name(&std)?,
        builds: file_name(&builds()?)?,
        sources: file_name(&sources)?,
        writer: writer_of(&paths.state)?,
        socket: paths.socket.as_deref().map(file_name).transpose()?,
        http: serving.http.zip(token),
        budget: Some(blaze_graph::arena::machine_budget(1 << 30).context("the memory the daemon's tasks may hold")?),
        threads,
        io: 16,
        env: ["PATH", "HOME"]
            .into_iter()
            .filter_map(|name| match std::env::var(name) {
                Ok(value) => Some(Ok((name.to_owned(), value))),
                Err(std::env::VarError::NotPresent) => None,
                Err(error) => Some(Err(anyhow::anyhow!("the daemon's ${name}: {error}"))),
            })
            .collect::<anyhow::Result<_>>()?,
        host,
        toolchain,
        color: false,
        build,
        outside: blaze_vfs::outside::dirs(),
        stall: serving.stall.unwrap_or(blaze_profile::Watchdog::BOUND),
        #[cfg(unix)]
        activated: serving.activated,
        profiling: Some(profiling),
    };
    // A sampler that can't be made is no other path: every dump and history says it is off.
    if let Err(why) = blaze_profile::sampler::start(blaze_profile::sampler::HZ) {
        tracing::warn!("{why}");
        eprintln!("blaze serve: {why}");
    }
    signals::catch();
    signals::record_ends();
    plane::record_panics();
    // Each query run is progress to the stall watchdog: a check inside queries for minutes is
    // no stall. Set once per process, so a second daemon in one (tests) finds it set by the
    // first; a hook something else set first would leave the watchdog counting nothing.
    #[expect(
        clippy::disallowed_types,
        reason = "blaze_query's run hook is set once per process; this keeps whether this process's set took"
    )]
    static WATCHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*WATCHED.get_or_init(|| blaze_query::on_each_run(blaze_profile::moved).is_ok()) {
        anyhow::bail!(
            "blaze_query::on_each_run already has a hook other than the daemon's, so the stall \
             watchdog would not see query runs as progress"
        );
    }
    let steps = Arc::new(unit::Real::new(&paths, sources).map_err(|why| anyhow::anyhow!(why))?);
    let scratch = paths.state.join("spawn");
    let runner = plane::Runner::Threads(plane::Threads::new(options.io, options.threads));
    let driver = blaze_core::native(std::path::Path::new("/"), threads)
        .context("starting this machine's native driver (io_uring, kqueue or IOCP)")?;
    let driver = driver
        .confined(confine)
        .scratch(&scratch)
        // alloc: an error path
        .with_context(|| format!("making {}, where each sandboxed spawn gets its directory", scratch.display()))?;
    let held = Held {
        #[cfg(unix)]
        _lock: lock,
        _std_lease: std_lease,
    };
    Ok(Engine { options, steps, runner, driver, keeper, held })
}

#[cfg(all(feature = "daemon", not(any(target_os = "linux", target_os = "macos", windows))))]
compile_error!(
    "blaze serve (blaze-cargo's `daemon` feature) runs on a native driver: io_uring (Linux), kqueue (macOS) or \
     IOCP (Windows); the wasm seed is built without it (`--no-default-features --features seed`) and builds one-shot"
);

/// The engine started: its daemon, and what the engine holds until it ends ([`Running::end`]).
pub(crate) struct Running {
    pub(crate) daemon: Daemon<blaze_core::Native, unit::Real>,
    keeper: observe::Keeper,
    _held: Held,
}

impl Running {
    /// The daemon gone, and with it the history's sender: its thread's last flush ends it.
    pub(crate) fn end(self) -> anyhow::Result<()> {
        let Running { daemon, keeper, _held } = self;
        drop(daemon);
        keeper.join().map_err(|why| anyhow::anyhow!(why))
    }
}

/// The engine's daemon started over its state, its store installed and its collector set:
/// what `who` (`blaze serve`, or the one-shot build's `blaze`) says of the start goes to
/// stderr, and `ready` hears each phase.
pub(crate) fn start(engine: Engine, who: &str, mut ready: Option<&mut std::fs::File>) -> anyhow::Result<Running> {
    let Engine { options, steps, runner, driver, keeper, held } = engine;
    match begin(driver, options, steps, runner, who, ready.as_deref_mut()) {
        Ok(daemon) => Ok(Running { daemon, keeper, _held: held }),
        Err(error) => {
            // The daemon was never made, or is gone: the history's sender with it.
            keeper.join().map_err(|why| anyhow::anyhow!(why))?;
            Err(error)
        }
    }
}

fn begin(
    driver: blaze_core::Native,
    options: Options,
    steps: Arc<unit::Real>,
    runner: plane::Runner<job::Step<unit::Real>>,
    who: &str,
    mut ready: Option<&mut std::fs::File>,
) -> anyhow::Result<Daemon<blaze_core::Native, unit::Real>> {
    say(ready.as_deref_mut(), Phase::State);
    let mut daemon = Daemon::start(driver, options, steps, runner)?;
    say(ready, Phase::Setup);
    let started = daemon.started();
    if started.cold == Some(daemon::Cold::Interned) {
        bail!("{who}: the daemon image's symbols: {}", daemon::Cold::Interned);
    }
    if let Some(restored) = &started.restored {
        eprintln!("{who}: restored the daemon image {restored}");
    }
    if !started.whole() {
        eprintln!("{who}: started: {started}");
    }
    daemon.install_store().map_err(|why| anyhow::anyhow!("the daemon's store: {why}"))?;
    // What blaze keeps on disk, collected in the background (docs/gc.md): the build dirs
    // under blaze's root and the shared stores of the machine's directory.
    let root = builds()?.parent().map(std::path::Path::to_path_buf).context("blaze's build directories have no root")?;
    // What no build dir may be or hold, made once here: one that does not make stops the start,
    // named (review round 6, C5).
    job::protected(&root).map_err(|why| anyhow::anyhow!("{who}: {why}"))?;
    // `<root>/gc.json` sets the levels of free space and whether the daemon collects on its
    // own; one that does not read stops the start.
    let settings = blaze_gc::settings::GcSettings::read(&root, blaze_gc::pace::Levels::default()).map_err(|why| anyhow::anyhow!(why))?;
    if settings.auto {
        daemon.collect(gc::Gc::new(gc::areas(&root, &machine()?), &state()?, settings.levels));
    } else {
        eprintln!(
            "{who}: {} sets \"auto\": false: the daemon collects nothing on its own (`blaze cache gc` still does when run)",
            root.join(blaze_gc::settings::SETTINGS).display()
        );
    }
    Ok(daemon)
}

/// `blaze serve`'s loop: the daemon listens, answers until it stops, and says how it stopped.
#[cfg(feature = "daemon")]
fn listen(daemon: &mut Daemon<blaze_core::Native, unit::Real>, mut ready: Option<std::fs::File>) -> anyhow::Result<()> {
    // `start` bound the socket; a turn hears how the bind went, so a refused one stops here,
    // named, before the CLI is told it listens and connects at once.
    let turning = daemon.turn();
    if let Some(why) = daemon.failed() {
        bail!("blaze serve: listening at {}: {why}", daemon.listening());
    }
    if !turning {
        bail!("blaze serve: stopped in its first turn, before it listened at {}", daemon.listening());
    }
    eprintln!("{LISTENING}{} (pid {})", daemon.listening(), std::process::id());
    // Its last word: the pipe closes as it goes.
    say(ready.take().as_mut(), Phase::Listening);
    let mut told = false;
    loop {
        if signals::signalled() && !told {
            told = true;
            let pid = std::process::id();
            let by = signals::stopped_by().unwrap_or_default();
            eprintln!("blaze serve: pid {pid} asked to stop by {by}: writing the image first");
            daemon.stop();
        }
        if signals::dump_asked() {
            tracing::warn!("SIGUSR1: {}", blaze_profile::watchdog::running());
        }
        if !daemon.turn() {
            break;
        }
    }
    match daemon.failed() {
        Some(why) => bail!("{why}"),
        None => {
            eprintln!("blaze serve: pid {} stopped", std::process::id());
            Ok(())
        }
    }
}

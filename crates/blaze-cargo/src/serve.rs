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
mod heads;
mod image;
mod job;
mod mounts;
pub mod observe;
mod plane;
mod signals;
#[cfg(test)]
mod tests;
mod unit;
mod work;

use std::{
    ffi::OsString,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use anyhow::{Context, bail};
use blaze_core::{Confine, FileName};

pub use daemon::{Daemon, Options};

/// What the line a daemon writes to its log once it listens starts with, then where and its
/// pid: `blaze serve: listening on /run/blaze/v2.sock (pid 7)`. The client finds the daemon's
/// last start by it, and harness/storelib.py's `LISTENING` that it listens.
pub const LISTENING: &str = "blaze serve: listening on ";

/// Where the daemon is and what it runs.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Its state ([`state`]).
    pub state: PathBuf,
    pub socket: PathBuf,
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

/// Where the daemon keeps what outlives it: `$BLAZE_HOME/v2`, else `blaze/v2` in the user's
/// cache directory (`XDG_CACHE_HOME`, or `~/.cache`). Unlike the socket it is never in
/// `XDG_RUNTIME_DIR`, which does not outlive the session.
pub fn state() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home).join("v2"));
    }
    if let Some(caches) = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(caches).join("blaze/v2"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    Ok(PathBuf::from(home.context("no BLAZE_HOME, XDG_CACHE_HOME or HOME says where the daemon keeps its state")?)
        .join(".cache/blaze/v2"))
}

/// Where every daemon of this machine, whatever its `BLAZE_HOME`, keeps what they share:
/// `blaze` in the user's cache directory (`XDG_CACHE_HOME`, or `~/.cache`).
pub fn machine() -> anyhow::Result<PathBuf> {
    if let Some(caches) = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(caches).join("blaze"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    Ok(PathBuf::from(home.context("no XDG_CACHE_HOME or HOME says where this machine's daemons share the standard library")?)
        .join(".cache/blaze"))
}

/// Which blaze `exe` is: the hash of its bytes, so two builds of one commit, a dirty tree's,
/// are two. What the daemons of one build share is under it, as only that build made it.
pub fn build_id(exe: &Path) -> anyhow::Result<blaze_ids::ContentId> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(exe).with_context(|| format!("reading blaze itself, {}", exe.display()))?;
    let mut hasher = blaze_ids::Hasher::new("blaze v2 build id 1");
    let mut chunk = vec![0u8; 1 << 20]; // alloc: one buffer, once per start
    loop {
        let read = file.read(&mut chunk).with_context(|| format!("reading blaze itself, {}", exe.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hasher.finish())
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
    // The driver's root / is joined with the name, and C:/dir joined to it is itself: a drive path is its own name.
    #[cfg(windows)]
    let (text, rest) = match (path.is_absolute(), text.replace('\\', "/")) {
        (true, name) => (text, name),
        (false, _) => bail!("{text} is not an absolute path"),
    };
    #[cfg(not(windows))]
    let Some(rest) = text.strip_prefix('/') else { bail!("{text} is not an absolute path") };
    FileName::new(rest.trim_end_matches('/')).with_context(|| format!("{text} has an empty, `.` or `..` part"))
}

struct Args {
    socket: Option<PathBuf>,
    http: Option<SocketAddr>,
    /// Listen on the socket systemd or launchd passed, never one of its own.
    activated: bool,
}

fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Args> {
    let mut parsed = Args { socket: None, http: None, activated: false };
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
            "--http" => {
                let at = value()?;
                parsed.http = Some(at.parse().with_context(|| format!("--http {at} is no address and port"))?);
            }
            _ => bail!("blaze serve takes `--socket PATH`, `--http ADDR` and `--activated`, not `{arg}`"),
        }
    }
    Ok(parsed)
}

/// The socket systemd or launchd passed `blaze serve --activated` (the units of
/// `blaze daemon install`, [`blaze_api::units`]).
#[cfg(unix)]
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

/// `blaze serve [--socket PATH] [--http ADDR] [--activated]`.
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

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
    let socket = match args.socket {
        Some(socket) => std::path::absolute(&socket).with_context(|| format!("the socket {}", socket.display()))?,
        None => socket()?,
    };
    let state = state()?;
    std::fs::create_dir_all(&state).with_context(|| format!("making the daemon's state directory {}", state.display()))?;
    // Where a stall's trace is written (`daemon::Failed::Stalled`): by the loop, which makes no
    // directories.
    let stalls = state.join("stalls");
    std::fs::create_dir_all(&stalls).with_context(|| format!("making {}", stalls.display()))?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("making the socket's directory {}", parent.display()))?;
    }
    let token = match args.http {
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
        socket,
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
    let build = build_id(&paths.launcher)?;
    let std = machine()?.join("std").join(format!("{build:x}"));
    std::fs::create_dir_all(&std).with_context(|| format!("making the standard library's shared directory {}", std.display()))?;
    let options = Options {
        state: file_name(&paths.state)?,
        std: file_name(&std)?,
        writer: writer_of(&paths.state)?,
        socket: file_name(&paths.socket)?,
        http: args.http.zip(token),
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
        host: crate::toolchain::host()
            .context("blaze has no target triple for this machine's architecture and system")?
            .to_owned(),
        color: false,
        outside: blaze_vfs::outside::dirs(),
        #[cfg(unix)]
        activated,
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
    let steps = Arc::new(unit::Real::new(&paths).map_err(|why| anyhow::anyhow!(why))?);
    let scratch = paths.state.join("spawn");
    let runner = plane::Runner::Threads(plane::Threads::new(options.io, options.threads));
    let driver = blaze_core::native(std::path::Path::new("/"), threads)
        .context("starting this machine's native driver (io_uring, kqueue or IOCP)")?;
    let driver = driver
        .confined(confine)
        .scratch(&scratch)
        // alloc: an error path
        .with_context(|| format!("making {}, where each sandboxed spawn gets its directory", scratch.display()))?;
    let ran = run(driver, options, steps, runner);
    // The daemon is gone, and with it the history's sender: its thread's last flush ends it.
    keeper.join().map_err(|why| anyhow::anyhow!(why))?;
    ran
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("blaze serve runs on a native driver: io_uring (Linux), kqueue (macOS) or IOCP (Windows)");

/// The loop over this target's native driver, its root `/`, until it stops.
fn run(
    driver: blaze_core::Native,
    options: Options,
    steps: Arc<unit::Real>,
    runner: plane::Runner<job::Step<unit::Real>>,
) -> anyhow::Result<()> {
    let host = options.host.clone();
    let mut daemon = Daemon::start(driver, options, steps, runner)?;
    let started = daemon.started();
    if started.cold == Some(daemon::Cold::Interned) {
        bail!("blaze serve: the daemon image's symbols: {}", daemon::Cold::Interned);
    }
    if !started.whole() {
        eprintln!("blaze serve: started: {started}");
    }
    daemon.install_store().map_err(|why| anyhow::anyhow!("the daemon's store: {why}"))?;
    // Once a process: every expansion from here on reads the daemon's store with no lock.
    let toolchain = hir_ty_codegen::v2::toolchain(&host, &[], hir_ty_codegen::v2::Runs::Elsewhere)
        .map_err(|why| anyhow::anyhow!("the toolchain proc macros are built with, for {host}: {why}"))?
        .id;
    proc_macro_api::kept::install(proc_macro_api::kept::Expansions {
        store: daemon.store().reader(),
        toolchain,
        environment: proc_macro_api::kept::Environment::new(),
    })
    .map_err(|why| anyhow::anyhow!("keeping proc-macro expansions: {why}"))?;
    eprintln!("{LISTENING}{} (pid {})", daemon.listening(), std::process::id());
    loop {
        if signals::signalled() {
            daemon.stop();
        }
        if !daemon.turn() {
            break;
        }
    }
    match daemon.failed() {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
}

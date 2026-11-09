//! `blaze serve` (docs/v2/plan.md section 6): v2's daemon. One loop over the
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
//!   its CPUs, and answers `blaze explore`, `blaze debug live` and a reader's ask to flush the
//!   history (`blaze_api::Request::Flush`) itself ([`observe`]).

mod breakdown;
pub mod ceiling;
mod daemon;
mod fetch;
mod fetch_url;
mod gc;
mod heads;
mod image;
mod job;
#[cfg(all(unix, feature = "native"))]
mod machine;
mod mounts;
mod native;
pub mod observe;
/// The seed's one-shot build: this engine in the CLI's process, for its command alone.
#[cfg(not(feature = "native"))]
pub(crate) mod once;
mod plane;
mod pulse;
pub mod settings;
pub(crate) mod signals;
mod still;
#[cfg(all(unix, feature = "native"))]
mod stop;
#[cfg(test)]
mod tests;
mod unit;
pub mod why;
mod work;

#[cfg(feature = "native")]
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
#[cfg(all(unix, feature = "native"))]
pub use machine::Machine;

/// Where blaze keeps no machine's memory ledger (the seed's one-shot build, which is one
/// process of its own; Windows, which has no `/tmp/blaze-<uid>` yet): there is no place in one
/// to hold, so [`Options::machine`] is always `None`.
#[cfg(not(all(unix, feature = "native")))]
pub enum Machine {}

/// What the line a daemon writes to its log once it listens starts with, then where and its
/// pid: `blaze serve: listening on /run/blaze/v2.sock (pid 7)`. The client finds the daemon's
/// last start by it, and harness/storelib.py's `LISTENING` that it listens.
#[cfg(feature = "native")]
pub const LISTENING: &str = "blaze serve: listening on ";

/// What the daemon's line that says how it started begins with, after `blaze serve: `: written
/// when its warm state is gone, or the daemon before it did not stop, for the client that
/// started it to quote.
pub const STARTED: &str = "started: ";

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
/// [`builds`] and [`sources`] are under; `blaze status --json` names it as `home`.
pub fn root() -> anyhow::Result<PathBuf> {
    blaze_manifest::home::process_root()
        .map_err(anyhow::Error::msg)?
        .context("no BLAZE_HOME, XDG_CACHE_HOME or HOME says where blaze keeps its state and build directories")
}

/// Where every daemon of this machine, whatever its `BLAZE_HOME`, keeps what they share:
/// `blaze` in the user's cache directory (`XDG_CACHE_HOME`, or `~/.cache`), when that is on
/// the file system of blaze's root ([`root`]); else `shared` under the root. What is placed
/// there is cloned from the daemon's store, and a clone does not cross file systems (blaze
/// never copies instead), so daemons share it only within one file system.
pub fn machine() -> anyhow::Result<PathBuf> {
    let dir = user_shared()?;
    let root = root()?;
    if same_file_system(&dir, &root)? { Ok(dir) } else { Ok(root.join("shared")) }
}

/// Whether `a` and `b` are on one file system, each judged by the nearest of it and its
/// ancestors that exists (a directory not made yet is made on its parent's).
#[cfg(unix)]
fn same_file_system(a: &Path, b: &Path) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let device = |path: &Path| -> anyhow::Result<u64> {
        let mut at = path;
        loop {
            match std::fs::metadata(at) {
                Ok(it) => return Ok(it.dev()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    at = at.parent().with_context(|| {
                        format!("no ancestor of {} exists to say its file system", path.display())
                    })?;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("the file system of {}", at.display()));
                }
            }
        }
    };
    Ok(device(a)? == device(b)?)
}

/// Whether `a` and `b`, both absolute, are on one volume: the same drive or share prefix
/// (ReFS clones within a volume alone).
#[cfg(not(unix))]
fn same_file_system(a: &Path, b: &Path) -> anyhow::Result<bool> {
    let prefix = |path: &Path| match path.components().next() {
        Some(std::path::Component::Prefix(it)) => Ok(it.as_os_str().to_ascii_uppercase()),
        _ => Err(anyhow::anyhow!("{} names no volume to say its file system", path.display())),
    };
    Ok(prefix(a)? == prefix(b)?)
}

/// `blaze` in the user's cache directory (`XDG_CACHE_HOME`, or `~/.cache`), absolute.
fn user_shared() -> anyhow::Result<PathBuf> {
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
    std::path::absolute(&dir).with_context(|| {
        format!("the machine's shared directory {} against the current directory", dir.display())
    })
}

/// The content identity of the code that computes what the front end keeps in the store, and
/// of how it was built (`blaze_codegen_code::FRONT_END`): what def-map images and the check
/// outcomes, bodies and constants served beside them, the goal and item stores and kept
/// proc-macro expansions are keyed by. blaze-engine's build script works it out with the code
/// generator's in one walk and the program hands it over with [`set_front_end_code`] before any
/// command runs, so an edit outside the front end's packages (the code generator, the CLI, the
/// daemon, the lints) keeps every image, goal and expansion stored. Unit records and the
/// standard library's shared directory are keyed by it with the code generator's and the
/// record's ([`RecordMakers`]): a record is made by blaze-cargo itself, past the front end.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FrontEndCode(pub blaze_ids::ContentId);

impl FrontEndCode {
    /// Of the 64 hex digits `blaze_codegen_code::emit_all` sets `BLAZE_FRONT_END_CODE` to.
    pub fn from_hex(hex: &str) -> Result<FrontEndCode, String> {
        code_of_hex(hex, "the front end's code identity", "BLAZE_FRONT_END_CODE").map(FrontEndCode)
    }
}

/// The content ID of `hex`, 64 hex digits a build script of `blaze_codegen_code` set `env` to;
/// `what` names it in the error.
fn code_of_hex(hex: &str, what: &str, env: &str) -> Result<blaze_ids::ContentId, String> {
    let refused = || format!("{what} `{hex}` is not 64 hex digits (`{env}`)");
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(refused());
    }
    let mut bytes = [0u8; 32];
    for (byte, at) in bytes.iter_mut().zip((0..64).step_by(2)) {
        *byte = u8::from_str_radix(&hex[at..at + 2], 16).map_err(|_| refused())?;
    }
    Ok(blaze_ids::ContentId(bytes))
}

#[expect(
    clippy::disallowed_types,
    reason = "which front end the process is, handed over once by the program's main so that no edit outside the front end changes this crate"
)]
static FRONT_END_CODE: std::sync::OnceLock<FrontEndCode> = std::sync::OnceLock::new();

/// Hands over the code identity of the process's front end: once, before the first job.
/// Handing over another one after is refused, naming both: one process is one front end.
pub fn set_front_end_code(code: FrontEndCode) -> Result<(), String> {
    match FRONT_END_CODE.get_or_init(|| code) {
        it if *it == code => Ok(()),
        // alloc: an error, once
        it => Err(format!(
            "the front end's code identity is {:x} already, and {:x} was handed over: one process is one front end",
            it.0, code.0
        )),
    }
}

/// The code identity [`set_front_end_code`] was handed, or an error naming who should have.
/// blaze-cargo's own tests are one made-up front end, handed over on first ask.
pub fn front_end_code() -> Result<FrontEndCode, String> {
    #[cfg(test)]
    set_front_end_code(FrontEndCode(blaze_ids::ContentId([0x5f; 32])))?;
    FRONT_END_CODE.get().copied().ok_or_else(|| {
        "the front end was handed no code identity (`blaze_cargo::serve::set_front_end_code`): the program \
         that runs it hands over its build script's `BLAZE_FRONT_END_CODE` before any job, and blaze keys no \
         image, goal or expansion by the executable's bytes in its place"
            // alloc: an error
            .to_owned()
    })
}

/// The content identity of the code that makes a unit's record (`blaze_codegen_code::RECORD`):
/// blaze-cargo and every package it is built from but those the identity cuts (the explorer, its
/// page, the plane's history, the client's rendering), and how it was built. blaze-engine's build
/// script works it out in the walk that works out [`FrontEndCode`], and the program hands it
/// over with [`set_record_code`] before any command runs, so an edit of blaze-engine or a cut
/// package keeps every record and the standard library's shared directory.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RecordCode(pub blaze_ids::ContentId);

impl RecordCode {
    /// Of the 64 hex digits `blaze_codegen_code::emit_all` sets `BLAZE_RECORD_CODE` to.
    pub fn from_hex(hex: &str) -> Result<RecordCode, String> {
        code_of_hex(hex, "the code identity of what makes a unit's record", "BLAZE_RECORD_CODE")
            .map(RecordCode)
    }
}

#[expect(
    clippy::disallowed_types,
    reason = "which code makes the process's records, handed over once by the program's main so that no edit outside it changes this crate"
)]
static RECORD_CODE: std::sync::OnceLock<RecordCode> = std::sync::OnceLock::new();

/// Hands over the code identity of what makes the process's records: once, before the first
/// job. Handing over another one after is refused, naming both.
pub fn set_record_code(code: RecordCode) -> Result<(), String> {
    match RECORD_CODE.get_or_init(|| code) {
        it if *it == code => Ok(()),
        // alloc: an error, once
        it => Err(format!(
            "the code identity of what makes a unit's record is {:x} already, and {:x} was handed over: one \
             process is one maker of records",
            it.0, code.0
        )),
    }
}

/// The code identity [`set_record_code`] was handed, or an error naming who should have.
/// blaze-cargo's own tests are one made-up maker, handed over on first ask.
pub fn record_code() -> Result<RecordCode, String> {
    #[cfg(test)]
    set_record_code(RecordCode(blaze_ids::ContentId([0x7c; 32])))?;
    RECORD_CODE.get().copied().ok_or_else(|| {
        "the process was handed no code identity of what makes a unit's record \
         (`blaze_cargo::serve::set_record_code`): the program that runs it hands over its build script's \
         `BLAZE_RECORD_CODE` before any job, and blaze keys no record by the executable's bytes in its place"
            // alloc: an error
            .to_owned()
    })
}

/// What makes a unit's record, by the content identity of each part's code: the front end
/// (its diagnostics and what the code generator compiles of it), the code generator (the code
/// it makes) and what makes the record itself (blaze-cargo with the lints, the linker and the
/// record's encoding, [`RecordCode`]). What a unit's record key and the standard library's
/// shared directory are keyed by in place of the executable's bytes: every byte of a record or
/// of a file in that directory is made by code of these three closures from the inputs the key
/// names beside them, and the executable's other code (blaze-engine, the cut packages) makes
/// none of it (`blaze_codegen_code::RECORD_CUT`, `RECORD_REVIEWED`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RecordMakers {
    pub front_end: blaze_ids::ContentId,
    pub codegen: blaze_ids::ContentId,
    pub record: blaze_ids::ContentId,
}

impl RecordMakers {
    /// The three as one ID: what the standard library's shared directory is named by.
    pub fn id(&self) -> blaze_ids::ContentId {
        blaze_ids::Hasher::new(RECORD_MAKERS)
            .id(&self.front_end)
            .id(&self.codegen)
            .id(&self.record)
            .finish()
    }
}

/// The domain of [`RecordMakers::id`].
const RECORD_MAKERS: &str = "blaze v2 record makers 1";

/// The process's [`RecordMakers`], or an error naming the identity it was not handed.
/// The code generator's code identity in blaze-cargo's own tests: one value, since the process
/// holds one and every test that asks for it hands over this ([`record_makers`],
/// `answers::own`'s `-vV` test).
#[cfg(test)]
pub(crate) const TEST_CODEGEN_CODE: hir_ty_codegen::v2::CodegenCode =
    hir_ty_codegen::v2::CodegenCode(blaze_ids::ContentId([0x5a; 32]));

/// blaze-cargo's own tests are one made-up code generator, [`TEST_CODEGEN_CODE`], handed over on
/// first ask, as `answers::own`'s tests hand it over, so either may ask first.
pub fn record_makers() -> Result<RecordMakers, String> {
    #[cfg(test)]
    hir_ty_codegen::v2::set_code(TEST_CODEGEN_CODE)?;
    Ok(RecordMakers {
        front_end: front_end_code()?.0,
        codegen: hir_ty_codegen::v2::code()?.0,
        record: record_code()?.0,
    })
}

/// Which blaze `exe` is, the hash of its bytes, with the file it hashed ([`executable`]), as
/// one open file was both: one that changed while it was read is an error. What a client
/// compares with the daemon's to know it speaks to the same blaze; no stored value is keyed by
/// it ([`RecordMakers`]).
pub fn build_of(exe: &Path) -> anyhow::Result<blaze_api::Build> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(exe)
        .with_context(|| format!("reading blaze itself, {}", exe.display()))?;
    let before = executable(exe, &file)?;
    let mut hasher = blaze_ids::Hasher::new("blaze v2 build id 1");
    let mut chunk = vec![0u8; 1 << 20]; // alloc: one buffer, once per start
    loop {
        let read = file
            .read(&mut chunk)
            .with_context(|| format!("reading blaze itself, {}", exe.display()))?;
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
    let meta = file
        .metadata()
        .with_context(|| format!("reading what blaze itself, {}, is", exe.display()))?;
    let modified = meta
        .modified()
        .with_context(|| format!("the modification time of blaze itself, {}", exe.display()))?;
    let modified_ns =
        modified.duration_since(std::time::UNIX_EPOCH).map_or(0, |it| it.as_nanos() as u64);
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    let path = exe
        .to_str()
        .with_context(|| format!("blaze itself, {}, is not at a UTF-8 path", exe.display()))?;
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
    // The driver's root / is joined with the name, and C:/dir joined to it is itself: a drive path is its own
    // name, as the daemon's `/C:/dir` is once its root is taken off.
    #[cfg(windows)]
    let (text, rest) = match (path.is_absolute(), text.replace('\\', "/")) {
        (true, name) => (text, name),
        (false, name) if name.starts_with('/') => (text, name[1..].to_owned()),
        (false, _) => bail!("{text} is not an absolute path"),
    };
    #[cfg(not(windows))]
    let Some(rest) = text.strip_prefix('/') else { bail!("{text} is not an absolute path") };
    FileName::new(rest.trim_end_matches('/'))
        .with_context(|| format!("{text} has an empty, `.` or `..` part"))
}

#[cfg(feature = "native")]
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
    /// `--memory-gb GB`: the memory it runs under ([`told_memory`]).
    memory: Option<(u64, String)>,
}

#[cfg(feature = "native")]
fn parse(args: impl IntoIterator<Item = OsString>) -> anyhow::Result<Args> {
    let mut parsed =
        Args { socket: None, http: None, activated: false, ready: None, stall: None, memory: None };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let arg = arg
            .into_string()
            .map_err(|it| anyhow::anyhow!("{} is not UTF-8", it.to_string_lossy()))?;
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_owned(), Some(value.to_owned()))
            }
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
                parsed.ready =
                    Some(fd.parse().with_context(|| format!("--ready-fd {fd} is no descriptor"))?);
            }
            "--http" => {
                let at = value()?;
                parsed.http = Some(
                    at.parse().with_context(|| format!("--http {at} is no address and port"))?,
                );
            }
            "--stall-bound" => {
                let secs = value()?;
                let bound = secs.parse::<u64>().ok().filter(|it| *it > 0).with_context(|| {
                    format!(
                        "--stall-bound {secs}: the stall bound is a whole number of seconds above 0"
                    )
                })?;
                parsed.stall = Some(std::time::Duration::from_secs(bound));
            }
            "--memory-gb" => parsed.memory = Some(told_memory("--memory-gb", &value()?)?),
            _ => bail!(
                "blaze serve takes `--socket PATH`, `--http ADDR`, `--activated`, `--ready-fd N`, \
                 `--stall-bound SECS` and `--memory-gb GB`, not `{arg}`"
            ),
        }
    }
    Ok(parsed)
}

/// The variable whoever starts the daemon tells it the memory it runs under with, in GiB, as
/// `--memory-gb` does: what a client that starts the daemon passes on with the rest of its
/// environment, so a runner that stops the daemon past a footprint (its `daemon_max_gb`) sets
/// it once and every daemon its commands start is told.
pub(crate) const MEMORY_VAR: &str = "BLAZE_DAEMON_MEMORY_GB";

/// The limit `gb` says, given as `what` (`--memory-gb`, or [`MEMORY_VAR`]): a number of GiB
/// above 0, a fraction allowed (a runner's `daemon_max_gb` may be one), as the bytes it is and
/// the name the budget is said with ([`blaze_graph::arena::machine_budget`]'s `told`). An error
/// naming `what` and the text when it is no such number, never a default in its place.
pub(crate) fn told_memory(what: &str, gb: &str) -> anyhow::Result<(u64, String)> {
    let bytes = gb
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|it| it.is_finite() && *it > 0.0)
        .map(|it| it * f64::from(1u32 << 30))
        .filter(|it| *it >= 1.0 && *it < u64::MAX as f64)
        .with_context(|| {
            format!("{what} {gb:?}: the memory the daemon runs under is a number of GiB above 0")
        })?;
    // alloc: the limit's name, once per daemon start
    Ok((bytes as u64, format!("{what}={}", gb.trim())))
}

/// The limit the engine was told: `--memory-gb`'s (`flag`), else [`MEMORY_VAR`]'s (`var`, as
/// the environment has it), else none, when the budgets are the machine's.
pub(crate) fn told_limit(
    flag: Option<(u64, String)>,
    var: Option<std::ffi::OsString>,
) -> anyhow::Result<Option<(u64, String)>> {
    if flag.is_some() {
        return Ok(flag);
    }
    let Some(var) = var else { return Ok(None) };
    let text = var
        .into_string()
        .map_err(|it| anyhow::anyhow!("{MEMORY_VAR} {} is not UTF-8", it.to_string_lossy()))?;
    told_memory(MEMORY_VAR, &text).map(Some)
}

/// The machine budget the engine was told: [`machine::MACHINE_VAR`]'s (`var`, as the
/// environment has it) in GiB, else none, when it is made from the machine's limits
/// ([`blaze_graph::arena::machine_wide`]).
#[cfg(all(unix, feature = "native"))]
fn told_machine(var: Option<std::ffi::OsString>) -> anyhow::Result<Option<(u64, String)>> {
    let Some(var) = var else { return Ok(None) };
    let text = var.into_string().map_err(|it| {
        anyhow::anyhow!("{} {} is not UTF-8", machine::MACHINE_VAR, it.to_string_lossy())
    })?;
    told_memory(machine::MACHINE_VAR, &text).map(Some)
}

/// What a start says of the limit it runs under (`budget`, [`blaze_graph::arena::machine_budget`]'s):
/// its bytes and what told it, and when a build's release is due under it
/// ([`blaze_graph::admission::Squeeze`]): past the limit's line, or one subsystem a release
/// lowers past its fair share.
pub(crate) fn limit_said(budget: &blaze_graph::Budget) -> String {
    use blaze_graph::admission::{LINE, RELEASED, fair_share};
    let gib = |bytes: u64| bytes as f64 / f64::from(1u32 << 30);
    // alloc: the start's line, once per daemon start
    let shares = RELEASED
        .iter()
        .map(|&tag| format!("{} {:.2}", tag.name(), gib(fair_share(tag, budget.bytes))))
        .collect::<Vec<_>>()
        .join(", ");
    // alloc: as `shares`
    format!(
        "memory limit {:.2} GiB ({}): a build lets go of its front end once blaze holds {:.2} GiB, or once one of these holds past its share (GiB): {shares}",
        gib(budget.bytes),
        budget.from,
        gib(budget.bytes - budget.bytes / LINE),
    )
}

/// The socket systemd or launchd passed `blaze serve --activated` (the units of
/// `blaze daemon install`, [`blaze_api::units`]).
#[cfg(all(unix, feature = "native"))]
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
/// stops short, the CLI names the last phase it read, and the last [`INSIDE`] record, which
/// the start writes between phases to say what its thread is inside.
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

/// The tag of the other record `blaze serve --ready-fd N` writes on that pipe, between its
/// phases' bytes until it listens: what the thread of its start is inside now ([`Ready`]'s
/// reporter, every [`REPORT_EVERY`]). The tag, then the text's length as a little-endian `u16`,
/// then that many bytes of UTF-8, at most [`INSIDE_MAX`]: "`open store` (1.2s open)", or none
/// when the thread is inside no span. No [`Phase`] has this byte.
pub const INSIDE: u8 = b'i';

/// The most bytes of text an [`INSIDE`] record carries; longer text is cut at a character's
/// boundary at or before it.
pub const INSIDE_MAX: usize = 512;

/// Appends to `out` the [`INSIDE`] record that says `text`, cut to [`INSIDE_MAX`] bytes.
pub fn inside_record(text: &str, out: &mut Vec<u8>) {
    let mut end = text.len().min(INSIDE_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    out.push(INSIDE);
    // At most `INSIDE_MAX`, which a `u16` holds.
    out.extend_from_slice(&(end as u16).to_le_bytes());
    out.extend_from_slice(&text.as_bytes()[..end]);
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
/// engine's life and let go of as it drops ([`stdx::file_lock::Locked`], so a child spawned
/// meanwhile keeps nothing), or by the kernel as the process ends, however it ends, so a daemon
/// started while the one before it still ends (writing its image, then letting its socket's
/// name go) waits for it rather than takes its place. At most [`BEFORE_ENDS`]; after,
/// [`StillRuns`].
/// How the daemon before this one on its state ended, as `serve.lock` says once it is let go
/// of: a daemon writes its pid there as it starts, and ` stopped` after it once its stop wrote
/// its image ([`Running::stopped`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Before {
    /// No daemon ran on this state, or its lock says nothing readable.
    None,
    Stopped(u32),
    /// It ended without finishing its stop: killed (a SIGKILL, as an out-of-memory kill sends),
    /// crashed, or ended by its stop's bound; the image is the last it wrote before.
    Ended(u32),
}

impl Before {
    /// What `serve.lock` held.
    pub(crate) fn of(lock: &str) -> Before {
        let mut words = lock.split_whitespace();
        match (words.next().and_then(|it| it.parse().ok()), words.next()) {
            (Some(pid), Some(STOPPED)) => Before::Stopped(pid),
            (Some(pid), None) => Before::Ended(pid),
            _ => Before::None,
        }
    }
}

impl std::fmt::Display for Before {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Before::None => f.write_str("no daemon ran on this state before it"),
            Before::Stopped(pid) => {
                write!(f, "the daemon before it on this state (pid {pid}) stopped")
            }
            Before::Ended(pid) => write!(
                f,
                "the daemon before it on this state (pid {pid}) ended without finishing its stop (killed, as an \
                 out-of-memory kill or `kill -9` does, crashed, or ended by its stop's bound; its log says which): \
                 what it did after its last image is gone"
            ),
        }
    }
}

/// What a daemon adds to its pid in `serve.lock` once its stop wrote its image.
const STOPPED: &str = "stopped";

#[cfg(unix)]
fn hold(
    state: &Path,
    ready: Option<&Ready>,
) -> anyhow::Result<(stdx::file_lock::Locked, Before)> {
    use std::{
        io::{Read as _, Seek as _, Write as _},
        os::unix::fs::OpenOptionsExt as _,
    };
    let lock = state.join("serve.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock)
        .with_context(|| format!("opening the daemon's lock {}", lock.display()))?;
    let tried = stdx::file_lock::Locked::try_lock(file)
        .with_context(|| format!("locking the daemon's state at {}", lock.display()))?;
    let mut file = match tried {
        Ok(file) => file,
        Err(mut file) => {
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
            // `lock` has no timed wait, so the file waits on a thread of its own, and this one at
            // most `BEFORE_ENDS` for it to come back locked. Past that this process ends, the
            // thread with it.
            let (locked, word) = crossbeam_channel::bounded(1);
            std::thread::Builder::new()
                .name("blaze-serve-lock".to_owned()) // alloc: once a start that waits
                .spawn(move || drop(locked.send(stdx::file_lock::Locked::lock(file))))
                .with_context(|| format!("starting the thread that waits on {}", lock.display()))?;
            match word.recv_timeout(BEFORE_ENDS) {
                Ok(Ok(file)) => file,
                Ok(Err(error)) => {
                    bail!("locking the daemon's state at {}: {error}", lock.display())
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    return Err(StillRuns { pid, lock }.into());
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    bail!(
                        "the thread waiting on the daemon's lock {} ended without a word",
                        lock.display()
                    )
                }
            }
        }
    };
    let mut before = String::new();
    file.rewind().and_then(|()| file.read_to_string(&mut before)).with_context(|| {
        format!("reading how the daemon before this one ended from its lock {}", lock.display())
    })?;
    let before = Before::of(&before);
    file.set_len(0)
        .and_then(|()| file.rewind())
        .and_then(|()| write!(file, "{}", std::process::id()))
        .with_context(|| format!("writing this daemon's pid to its lock {}", lock.display()))?;
    Ok((file, before))
}

/// The pipe of `blaze serve --ready-fd N` ([`Phase`]), and the thread that says on it, every
/// [`REPORT_EVERY`] until the daemon listens, what the thread of its start is inside
/// ([`INSIDE`]). One lock holds the pipe for both, so no record cuts into another, and none
/// follows [`Phase::Listening`]'s byte: the pipe is closed as that is said, or as this drops
/// with a start that failed, and the reporter ends at its next tick, finding it gone.
pub(crate) struct Ready {
    pipe: Arc<blaze_core::lock::Lock<Option<std::fs::File>>>,
}

impl Ready {
    /// The pipe, its reporter started: the calling thread is the start's, which it reports on.
    #[cfg(all(unix, feature = "native"))]
    fn new(pipe: std::fs::File) -> anyhow::Result<Ready> {
        let pipe = Arc::new(blaze_core::lock::Lock::new("serve.ready.pipe", Some(pipe)));
        let starting = blaze_profile::starting::Starting::here();
        let reported = Arc::clone(&pipe);
        std::thread::Builder::new()
            .name("blaze-serve-start".to_owned()) // alloc: once a start
            .spawn(move || report(&reported, starting))
            .context(
                "starting the thread that says on the ready pipe what the daemon's start is inside",
            )?;
        Ok(Ready { pipe })
    }

    /// Says `phase`, closing the pipe once it is [`Phase::Listening`]. The CLI that waits may be
    /// gone, and then nobody is to be told.
    fn say(&self, phase: Phase) {
        let mut pipe = self.pipe.lock();
        if let Some(file) = pipe.as_mut() {
            _ = std::io::Write::write_all(file, &[phase.byte()]);
        }
        if phase == Phase::Listening {
            *pipe = None;
        }
    }
}

impl Drop for Ready {
    /// A start that ends without listening closes the pipe: the CLI hears it end.
    fn drop(&mut self) {
        *self.pipe.lock() = None;
    }
}

/// Says `phase` on the ready pipe, if there is one ([`Ready::say`]).
fn say(ready: Option<&Ready>, phase: Phase) {
    if let Some(ready) = ready {
        ready.say(phase);
    }
}

/// How often [`Ready`]'s reporter says what the start's thread is inside.
#[cfg(all(unix, feature = "native"))]
const REPORT_EVERY: Duration = Duration::from_millis(500);

/// The most reports [`Ready`]'s reporter writes: a start's phases are bounded (the CLI waits
/// [`BEFORE_ENDS`] and a minute a phase), so a start still going past ten minutes on top of
/// that has long been given up on, and the reporter stops, saying so in the log.
#[cfg(all(unix, feature = "native"))]
const REPORTS_MAX: u64 = (BEFORE_ENDS.as_secs() + 600) * 1000 / REPORT_EVERY.as_millis() as u64;

/// [`Ready`]'s reporter: every [`REPORT_EVERY`], an [`INSIDE`] record of what `starting` is
/// inside, until the pipe is gone (the daemon listens, or its start failed), a write fails (the
/// CLI that waited is gone) or [`REPORTS_MAX`]. Each tick waits for an absolute deadline taken
/// as the tick before ended, so nothing restarts a wait that is interrupted.
#[cfg(all(unix, feature = "native"))]
fn report(
    pipe: &blaze_core::lock::Lock<Option<std::fs::File>>,
    starting: blaze_profile::starting::Starting,
) {
    use blaze_core::wait::{Clock, Deadline, Monotonic, sleep_until};
    let clock = Monotonic::new();
    // alloc: one record's bytes, reused by every report of a start
    let mut record = Vec::with_capacity(3 + INSIDE_MAX);
    let mut deadline = Deadline::after(Clock::now(&clock), REPORT_EVERY);
    for _ in 0..REPORTS_MAX {
        if let Err(error) =
            sleep_until("the next report of what the daemon's start is inside", deadline, &clock)
        {
            eprintln!(
                "blaze serve: the report on its ready pipe of what its start is inside stops: {error}"
            );
            return;
        }
        deadline = Deadline::after(Clock::now(&clock), REPORT_EVERY);
        // alloc: the report's text, every 500 ms of a start
        let text = starting.inside().map(|it| it.to_string()).unwrap_or_default();
        record.clear();
        inside_record(&text, &mut record);
        let mut pipe = pipe.lock();
        // Gone: the daemon listens, or its start failed.
        let Some(file) = pipe.as_mut() else { return };
        if std::io::Write::write_all(file, &record).is_err() {
            // The CLI that waited is gone: nobody is to be told.
            return;
        }
    }
    eprintln!(
        "blaze serve: the report on its ready pipe of what its start is inside stops after {REPORTS_MAX} reports, \
         {}s: the start has not listened, and the CLI that started it gave up long ago",
        REPORTS_MAX * REPORT_EVERY.as_millis() as u64 / 1000
    );
}

/// The pipe of `blaze serve --ready-fd N`, made this process's: close-on-exec from here on.
#[cfg(all(unix, feature = "native"))]
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
#[cfg(feature = "native")]
pub fn main(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "native")]
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
        bail!(
            "blaze serve --activated: socket activation is systemd's and launchd's, and this system has neither"
        );
    }
    // First too: nothing this process starts may inherit the pipe.
    #[cfg(unix)]
    let ready = args.ready.map(ready).transpose()?.map(Ready::new).transpose()?;
    #[cfg(not(unix))]
    let ready = match args.ready {
        Some(_) => bail!("blaze serve --ready-fd: the CLI starts the daemon so on Unix only"),
        None => None,
    };
    let socket = match args.socket {
        Some(socket) => std::path::absolute(&socket)
            .with_context(|| format!("the socket {}", socket.display()))?,
        None => socket()?,
    };
    let serving = Serving {
        socket: Some(socket),
        http: args.http,
        #[cfg(unix)]
        activated,
        stall: args.stall,
        memory: args.memory,
    };
    let engine = engine(serving, ready.as_ref())?;
    // The hard ceiling (docs/memory-budget.md section 7, Hard), set before the daemon restores
    // its image or takes a request: the kernel's limit where this process may set one, and the
    // stop's guard's watch, at the budget itself (the machine's 3/4, or `--memory-gb`). In
    // `blaze serve` alone, whose process is the daemon's own: never the one-shot build's or a
    // test's, whose process is another's too.
    if let Some(budget) = &engine.options.budget {
        ceiling::hold(budget.bytes, &budget.from)?;
    }
    let mut running = start(engine, "blaze serve", ready.as_ref())?;
    // blaze's update check (docs/install.md, "Updates"), off the loop, for the daemon's life.
    let _updates =
        crate::release::watch_updates().context("starting the daemon's update check thread")?;
    let ran = listen(&mut running.daemon, ready);
    let marked = match ran {
        Ok(()) => running.stopped(),
        Err(_) => Ok(()),
    };
    // The daemon is gone, and with it the history's sender: its thread's last flush ends it.
    running.end()?;
    ran.and(marked)
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
    /// `--memory-gb GB` ([`told_limit`]); `None` reads [`MEMORY_VAR`].
    pub(crate) memory: Option<(u64, String)>,
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
    lock: stdx::file_lock::Locked,
    /// How the daemon before this one on the state ended, as its lock said.
    before: Before,
    _std_lease: blaze_gc::lease::Lease,
}

/// The engine, its state found and held, its driver made, before it starts: `ready`, the pipe
/// of `blaze serve --ready-fd`, hears if it waits for the engine before it on the same state.
pub(crate) fn engine(
    serving: Serving,
    ready: Option<&Ready>,
) -> anyhow::Result<Engine> {
    let state = state()?;
    std::fs::create_dir_all(&state)
        .with_context(|| format!("making the daemon's state directory {}", state.display()))?;
    // Held until the engine ends: the next one on this state waits on it.
    #[cfg(unix)]
    let (lock, before) = hold(&state, ready)?;
    #[cfg(not(unix))]
    let (_, before) = (ready, Before::None);
    // Where a stall's trace is written (`daemon::Failed::Stalled`): by the loop, which makes no
    // directories.
    let stalls = state.join("stalls");
    std::fs::create_dir_all(&stalls).with_context(|| format!("making {}", stalls.display()))?;
    if let Some(parent) = serving.socket.as_deref().and_then(Path::parent) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("making the socket's directory {}", parent.display()))?;
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
        launcher: std::env::current_exe()
            .context("the path of blaze itself, which launches the sandbox")?,
    };
    // dyn: the sandbox's `Confine`, asked once per spawn
    let confine: Arc<dyn Confine> =
        Arc::new(blaze_usercode::confine::Launcher { program: paths.launcher.clone() });
    // The process's one budget (`BLAZE_CORES`), read as `blaze serve` started and its priority
    // lowered then ([`crate::cli::main`]): a command setting another is refused before any
    // request, with every other setting it differs in ([`settings`]).
    let cores =
        blaze_core::cores::get().context("the number of cores the daemon's plane works on")?;
    tracing::info!(target: "blaze", "{cores}");
    let threads = cores.budget.get();
    // Read once: the explorer and live SQL run this DuckDB; set wrong, the daemon does not start.
    let duckdb =
        observe::duckdb(std::env::var_os(observe::DUCKDB)).map_err(|why| anyhow::anyhow!(why))?;
    // Read once: what the history writes of DEBUG events (`BLAZE_HISTORY_LEVEL`, docs/profiling.md);
    // a value that names no level stops the daemon, naming it.
    let events = blaze_profile::logs::Policy::from_env().map_err(|why| anyhow::anyhow!(why))?;
    let (profiling, keeper) =
        observe::keep_history(&paths.state, duckdb, events).map_err(|why| anyhow::anyhow!(why))?;
    // Fixed for the daemon's life before any job: a build's tasks only read it.
    let sysroot = crate::toolchain::sysroot().with_context(|| {
        // alloc: once, on the error that stops the daemon from starting
        let at = crate::toolchain::own::Release::ours().and_then(|it| it.dir());
        let at =
            at.map_or_else(|| "on this machine".to_owned(), |it| format!("at {}", it.display()));
        format!(
            "blaze's toolchain ({}) is not whole {at}: `blaze toolchain` installs it",
            crate::toolchain::name()
        )
    })?;
    tracing::info!(sysroot = %sysroot.display(), "blaze's toolchain");
    // Which blaze this is, for clients to compare: no stored value is keyed by it.
    let build = build_of(&paths.launcher)?;
    // Keyed by what makes std's records and outputs ([`RecordMakers`]), never the executable's
    // bytes: the shared store holds std units' records and outputs alone, and each std key's
    // outputs are files at fixed paths under it, so two blazes share it only when every byte
    // either writes there is made by the same code from the same inputs (AGENTS.md rule 25).
    let makers = record_makers().map_err(|why| anyhow::anyhow!(why))?;
    let std = machine()?.join("std").join(format!("{:x}", makers.id()));
    std::fs::create_dir_all(&std).with_context(|| {
        format!("making the standard library's shared directory {}", std.display())
    })?;
    // Its lease, held for the daemon's life: no collector, this daemon's or another
    // process's, renames the shared store away while this daemon may read or write it
    // (docs/gc.md, "Leases"). Its record says when a daemon last started on it.
    let std_lease = blaze_gc::lease::Lease::hold(&std)
        .map_err(|why| anyhow::anyhow!("the standard library's shared directory: {why}"))?;
    if let Some(why) = blaze_gc::record::touch(&std, None, &[], blaze_gc::record::now_ns())
        .map_err(|why| anyhow::anyhow!(why))?
    {
        tracing::warn!(target: "blaze", dir = %std.display(), "the shared store's record did not read and was written afresh: {why}");
    }
    let host = crate::toolchain::host()
        .context("blaze has no target triple for this machine's architecture and system")?
        .to_owned();
    // What kept expansions are keyed by: the front end's code ([`front_end_code`]), whose
    // closure holds the macro server and bridge that expand them (blaze-macro-eval,
    // proc-macro-api), as images and stored goals are.
    let toolchain = front_end_code().map_err(|why| anyhow::anyhow!(why))?.0;
    let sources = sources()?;
    // A daemon that stopped mid-write left a package's half-written directory, which nothing
    // reads: removed before any fetch writes beside it.
    let swept = fetch::sweep_parts(&sources).map_err(|why| anyhow::anyhow!(why))?;
    if let Some(first) = swept.first() {
        tracing::info!(target: "blaze", removed = swept.len(), first = %first.display(), "half-written fetched packages removed");
    }
    // The limit as told (`--memory-gb` first), read once: what the budget is made of and what
    // the daemon says it was started with ([`settings`]), so a client asking another is told.
    let told = told_limit(serving.memory, std::env::var_os(MEMORY_VAR))?;
    let settings = settings::of_daemon(told.as_ref(), |name| std::env::var_os(name));
    // The machine's memory ledger joined (docs/memory-budget.md, section 7): the machine
    // budget every daemon of this user shares, and this daemon's ceiling under it, the plane's
    // budget. A ledger that cannot be joined stops the start, naming it.
    #[cfg(all(unix, feature = "native"))]
    let (machine, budget) = {
        let told_machine = told_machine(std::env::var_os(machine::MACHINE_VAR))?;
        let joined = machine::Machine::join(told_machine, told, &crate::version(), &build, &root()?)
            .context("joining the machine's memory ledger")?;
        tracing::info!(target: "blaze", "{}", joined.machine.said());
        (Some(joined.machine), joined.ceiling)
    };
    #[cfg(not(all(unix, feature = "native")))]
    let (machine, budget) = (
        None,
        blaze_graph::arena::machine_budget(1 << 30, told)
            .context("the memory the daemon's tasks may hold")?,
    );
    let options = Options {
        state: file_name(&paths.state)?,
        std: file_name(&std)?,
        builds: file_name(&builds()?)?,
        sources: file_name(&sources)?,
        writer: writer_of(&paths.state)?,
        socket: paths.socket.as_deref().map(file_name).transpose()?,
        http: serving.http.zip(token),
        budget: Some(budget),
        machine,
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
        settings,
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
    // The heap's: one allocation in every 4 MiB a thread allocates, with its stack, as
    // `alloc_samples` rows (docs/memory-footprint.md, step 1). Said, as the CPU sampler is.
    if let Err(why) = blaze_profile::heap::start() {
        tracing::warn!("{why}");
        eprintln!("blaze serve: {why}");
    }
    signals::catch();
    signals::record_ends();
    // After `record_ends`, which leaves a signal something handles alone: the crash record's
    // handler goes in front of what is there and hands each signal on to it.
    blaze_profile::crash::install();
    plane::record_panics();
    plane::record_alloc_failures();
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
        .with_context(|| {
            format!("making {}, where each sandboxed spawn gets its directory", scratch.display())
        })?;
    let held = Held {
        #[cfg(unix)]
        lock,
        before,
        _std_lease: std_lease,
    };
    Ok(Engine { options, steps, runner, driver, keeper, held })
}

#[cfg(all(feature = "native", not(any(target_os = "linux", target_os = "macos", windows))))]
compile_error!(
    "blaze serve (blaze-cargo's `native` feature) runs on a native driver: io_uring (Linux), kqueue (macOS) or \
     IOCP (Windows); the wasm seed is built without it (`--no-default-features --features seed`) and builds one-shot"
);

/// The engine started: its daemon, and what the engine holds until it ends ([`Running::end`]).
pub(crate) struct Running {
    pub(crate) daemon: Daemon<blaze_core::Native, unit::Real>,
    keeper: observe::Keeper,
    held: Held,
}

impl Running {
    /// Its stop wrote its image: its lock says so to the daemon after it ([`Before`]).
    pub(crate) fn stopped(&mut self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::io::{Seek as _, Write as _};
            let file = &mut *self.held.lock;
            file.set_len(0)
                .and_then(|()| file.rewind())
                .and_then(|()| write!(file, "{} {STOPPED}", std::process::id()))
                .context("writing that this daemon stopped to its lock serve.lock")?;
        }
        Ok(())
    }

    /// The daemon gone, and with it the history's sender: its thread's last flush ends it.
    pub(crate) fn end(self) -> anyhow::Result<()> {
        let Running { daemon, keeper, held } = self;
        let _held = held;
        drop(daemon);
        keeper.join().map_err(|why| anyhow::anyhow!(why))
    }
}

/// The engine's daemon started over its state, its store installed and its collector set:
/// what `who` (`blaze serve`, or the one-shot build's `blaze`) says of the start goes to
/// stderr, and `ready` hears each phase.
pub(crate) fn start(
    engine: Engine,
    who: &str,
    ready: Option<&Ready>,
) -> anyhow::Result<Running> {
    let Engine { options, steps, runner, driver, keeper, held } = engine;
    match begin(driver, options, steps, runner, who, held.before, ready) {
        Ok(daemon) => Ok(Running { daemon, keeper, held }),
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
    before: Before,
    ready: Option<&Ready>,
) -> anyhow::Result<Daemon<blaze_core::Native, unit::Real>> {
    say(ready, Phase::State);
    let limit = options.budget.as_ref().map(limit_said);
    #[cfg(all(unix, feature = "native"))]
    let shared = options.machine.as_ref().map(|it| it.said().to_owned()); // alloc: the start's line
    let mut daemon = Daemon::start(driver, options, steps, runner)?;
    say(ready, Phase::Setup);
    // Each start says the limit it runs under, so a run's log shows it was told one (B5).
    if let Some(limit) = limit {
        eprintln!("{who}: {limit}");
    }
    #[cfg(all(unix, feature = "native"))]
    if let Some(shared) = shared {
        eprintln!("{who}: {shared}");
    }
    let started = daemon.started();
    if started.cold == Some(daemon::Cold::Interned) {
        bail!("{who}: the daemon image's symbols: {}", daemon::Cold::Interned);
    }
    if let Some(restored) = &started.restored {
        eprintln!("{who}: restored the daemon image {restored}");
    }
    // What a client that started it quotes (`cli::daemon::start_said`): why the warm state is
    // gone, and how the daemon before it ended.
    if !started.whole() || matches!(before, Before::Ended(_)) {
        eprintln!("{who}: {STARTED}{started}; {before}");
    }
    daemon.install_store().map_err(|why| anyhow::anyhow!("the daemon's store: {why}"))?;
    // What blaze keeps on disk, collected in the background (docs/gc.md): the build dirs
    // under blaze's root and the shared stores of the machine's directory.
    let root = builds()?
        .parent()
        .map(std::path::Path::to_path_buf)
        .context("blaze's build directories have no root")?;
    // What no build dir may be or hold, made once here: one that does not make stops the start,
    // named (review round 6, C5).
    job::protected(&root).map_err(|why| anyhow::anyhow!("{who}: {why}"))?;
    // `<root>/gc.json` sets the levels of free space and whether the daemon collects on its
    // own; one that does not read stops the start.
    let settings = blaze_gc::settings::GcSettings::read(&root, blaze_gc::pace::Levels::default())
        .map_err(|why| anyhow::anyhow!(why))?;
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
#[cfg(feature = "native")]
fn listen(
    daemon: &mut Daemon<blaze_core::Native, unit::Real>,
    ready: Option<Ready>,
) -> anyhow::Result<()> {
    // `start` bound the socket; a turn hears how the bind went, so a refused one stops here,
    // named, before the CLI is told it listens and connects at once.
    let turning = daemon.turn();
    if let Some(why) = daemon.failed() {
        bail!("blaze serve: listening at {}: {why}", daemon.listening());
    }
    if !turning {
        bail!(
            "blaze serve: stopped in its first turn, before it listened at {}",
            daemon.listening()
        );
    }
    // Armed before the CLI is told it listens: from then on a SIGTERM ends the daemon within
    // its stall bound, however stuck, saying what it was inside, and a loop stuck inside a turn
    // is reported with the children it leaves unreaped.
    let guard = stop::arm(daemon.stall_bound(), daemon.pulse())?;
    eprintln!("{LISTENING}{} (pid {})", daemon.listening(), std::process::id());
    // Its last word: the pipe closes as it goes, and the reporter of what the start is inside
    // writes nothing after it.
    say(ready.as_ref(), Phase::Listening);
    drop(ready);
    let (mut told, mut guarded) = (false, false);
    loop {
        if signals::signalled() && !told {
            told = true;
            let pid = std::process::id();
            let by = signals::stopped_by().unwrap_or_default();
            eprintln!(
                "blaze serve: pid {pid} asked to stop by {by}: writing the image first; a stop in which nothing moves \
                 for {} s, or a second signal, ends it with what it is inside written here",
                daemon.stall_bound().as_secs()
            );
            daemon.stop();
        }
        if daemon.stopping() && !guarded {
            guarded = true;
            signals::stop_begun();
        }
        if signals::dump_asked() {
            tracing::warn!("SIGUSR1: {}", blaze_profile::watchdog::running());
        }
        if !daemon.turn() {
            break;
        }
    }
    guard.disarm();
    match daemon.failed() {
        Some(why) => bail!("{why}"),
        None => {
            eprintln!("blaze serve: pid {} stopped", std::process::id());
            Ok(())
        }
    }
}

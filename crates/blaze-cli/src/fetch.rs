//! The lock's units and the `Fetch` tasks' work (blaze_drive::fetch_task; fetch.md).
//!
//! - **The plan.** The layout reads `Cargo.lock` as one of its reads (units.rs), so a lock
//!   edit moves the units' source and with it the certificate. [`Lockfile::read`] plans it once
//!   a command, and only when the layout was read. Each unit's key is a `Fetch` subject, asked
//!   for by the `UnitGraph` task only once the graph reaches it (graph.rs).
//! - **The records.** One file a unit, `<home>/.blaze/v2/sources/<64 hex key>`, outside the
//!   packs: tempo's are hundreds of MiB, and a pack is merged and read whole. One store a
//!   machine, so a second project or worktree fetches and adopts nothing another holds.
//!   Whichever place gave the bytes (cargo's archive or the registry), the record is written
//!   whole under a temporary name and renamed onto its key.
//!   - A small record (a crate) is published unsynced.
//!   - A large one (a git commit) is written in place through `blaze_core::reserve`, synced:
//!     the fetch is the cold path, its wait the network's.
//! - **What a record proves.** Nothing until it is checked. `lookup` reads the record whole
//!   and checks its BLAKE3 (`PackView::open_source`). One that fails is removed, and the unit
//!   is fetched again. `lookup` runs only when the `Fetch` task runs, which is when its own
//!   record is missing: a cold home, or a lock edit.
//! - **The machine** is one a command, made at the graph's first ask. It wants each level's
//!   units at once ([`Lockfile::want_ahead`]), so their fetches overlap up to its in-flight
//!   limit, and never a unit the graph does not reach. Each task then steps it until its own
//!   unit's outcome is known.
//! - **Without `fetch`** no socket is opened. A unit whose record the store holds stands; one it
//!   lacks is the [`CANNOT_FETCH`] finding, kept as a record: another build has another tool id
//!   and so other keys.

use blaze_drive::fetch_task::{self, Index};
use blaze_drive::{Name, TaskFailure, Why};
use blaze_fetch::local::{self, Miss, Try};
use blaze_fetch::pack::PackView;
use blaze_fetch::plan::{self, Unit};
use blaze_ids::ContentId;
use blaze_manifest::lock::Lock;
use blaze_own::Alloc;
use blaze_text::Writes as _;
use bstd::cx::Cx;
use bstd::fs::{Content, Dir, Durable, FsError, FsKind, ReadOnly, ReadWrite};
use bstd::mem::{Bytes, Frame};
use bstd::path::Path;

use crate::CliError;
use crate::work::{Did, Made, held_str};

/// The finding number of a `Cargo.lock` the lock reader or the fetch plan refuses. Its one
/// argument is the refusal.
pub const LOCK_UNREAD: u16 = 11;
/// The finding number of a unit whose bytes are not what the lock pins: the same key meets the
/// same bytes again, so it is kept as a record. Its one argument is the unit and why.
pub const SOURCE_REFUSED: u16 = 12;
/// The finding number of a unit the store lacks in a build without `fetch`. Its one argument
/// names the unit.
pub const CANNOT_FETCH: u16 = 13;

/// Where the units' records are: the machine's sources store, under the user's home (fetch.md,
/// "Where bytes come from", place 1; piece 3), beneath blaze's folder there (store.rs
/// `HOME_FOLDER`). One record a unit, keyed from the lock's text alone, so every project and
/// every worktree on the machine reads the same bytes in place and none fetches or adopts a
/// unit another already holds. Never under a checkout.
pub const SOURCES: &str = ".blaze/v2/sources";

/// The sources store ([`SOURCES`] beneath the home main.rs read, rule 75), through the effect
/// layer's second tree (`Fs::home`). No home is a typed stop, never a folder chosen instead.
/// Sweeps any stale `.part-<tag>` reservation scratch files left behind by crashed processes
/// (preserving scratch files whose encoded PID is still alive).
pub(crate) fn sources_dir(cx: &Cx, env: &FetchEnv<'_>) -> Result<Dir<ReadWrite>, CliError> {
    let Some(home) = env.home else {
        return Err(CliError::Text { what: "HOME is not set: the sources store (fetch.rs `SOURCES`) is beneath it" });
    };
    let dir = env.fs.home(cx, home, &Path::new(SOURCES)?).map_err(|error| match error.why {
        bstd::fs::HomeWhy::Fs(error) => CliError::Fs(error),
        _ => CliError::Text { what: "HOME could not be opened for the sources store (fetch.rs `SOURCES`): not absolute, with `.` or `..`, or no second tree" },
    })?;
    let simulated = cx.is_simulated();
    dir.sweep_parts(cx, |_name, tag| {
        let pid = u32::try_from(tag >> 32).unwrap_or(0);
        if simulated {
            pid == 0
        } else {
            pid > 0 && crate::gc::PidProbe::probe(&crate::gc::OsPidProbe, pid) == crate::gc::PidState::Dead
        }
    })?;
    Ok(dir)
}

/// The sources store's folder as the machine names it, `<home>/.blaze/v2/sources`, in `frame`:
/// for the readers that map a record in place (graph.rs `Mapped`) and the reservations' driver.
/// `None` when no home is set.
pub(crate) fn sources_native<'f>(env: &FetchEnv<'_>, frame: &'f Frame) -> Option<bstd::mem::String<&'f Frame>> {
    let home = env.home?;
    Some(bstd::mem::format_in!(frame, "{}/{SOURCES}", home.trim_end_matches('/')))
}

const REFUSED_DOMAIN: &str = "blaze-cli fetch refused 1";

/// The lock's units, planned once a command.
pub struct Lockfile<'f> {
    /// The lock's text, as read: the `UnitGraph` task's source reads it (graph.rs).
    pub text: &'f [u8],
    /// The lock, read once a command; the `UnitGraph` task resolves over it.
    pub lock: &'f Lock<'static>,
    units: &'f [Unit<'f, 'static>],
    index: Index<'static>,
    /// Cargo's archive cache, looked at once a command, at the first unit the sources store
    /// lacks.
    cargo: CargoCache,
    /// The sources store ([`sources_dir`]), opened at the command's first `Fetch` task.
    sources: Option<Dir<ReadWrite>>,
    #[cfg(feature = "fetch")]
    net: Option<net::Net<'f>>,
}

impl<'f> Lockfile<'f> {
    /// The fetched source record `key`, mapped read-only from the sources store through the
    /// directory `Fetch` opened (opened here when this command's fetches were all satisfied
    /// by an earlier one): the unit graph reads its records in place through the same
    /// directory the fetch wrote them by, never through a path of the host's (lane BD4: a
    /// simulated world has no such path, so no graph was made of a registry crate there).
    pub(crate) fn map_source(&mut self, cx: &Cx, env: &FetchEnv<'_>, key: &ContentId) -> Result<blaze_core::Mapping, CliError> {
        let dir = match &mut self.sources {
            Some(dir) => dir,
            slot => slot.insert(sources_dir(cx, env)?),
        };
        Ok(dir.map_read(cx, &record_path(key)?)?)
    }

    /// Maps the fetched source record `key` if present as a regular file in the sources store,
    /// returning `Ok(None)` when absent (or a directory). Combines the existence check and mmap
    /// into a single open+fstat+mmap instead of statting and then re-opening.
    pub(crate) fn try_map_source(&mut self, cx: &Cx, env: &FetchEnv<'_>, key: &ContentId) -> Result<Option<blaze_core::Mapping>, CliError> {
        let dir = match &mut self.sources {
            Some(dir) => dir,
            slot => slot.insert(sources_dir(cx, env)?),
        };
        match dir.map_read(cx, &record_path(key)?) {
            Ok(mapping) => Ok(Some(mapping)),
            Err(error) if matches!(error.kind, FsKind::NotFound | FsKind::IsADirectory) => Ok(None),
            Err(error) => Err(CliError::Fs(error)),
        }
    }

    /// The lock's text planned. A refusal is `Err` with its words, which the caller makes a
    /// [`LOCK_UNREAD`] finding.
    ///
    /// The lock and the plan are kept in the command's frame for the command: the machine
    /// borrows the units, and both live as long as the request does. Their owners are on the
    /// process's `heap` (the machine's allocator must be the units'), so they go with the process.
    pub fn read(text: &'f [u8], heap: &'static blaze_own::Heap, frame: &'f Frame) -> Result<Lockfile<'f>, bstd::mem::String<&'f Frame>> {
        let lock = match Lock::read(text, heap.alloc()) {
            Ok(lock) => lock,
            Err(refused) => return Err(bstd::mem::format_in!(frame, "{refused}")),
        };
        let lock: &'f Lock<'static> = std::boxed::Box::leak(std::boxed::Box::new_in(lock, frame));
        let planned = match plan::plan(lock, heap.alloc()) {
            Ok(planned) => planned,
            Err(refused) => {
                let mut words = blaze_own::String::new_in(heap.alloc());
                blaze_text::Writes::show(&mut words, &refused);
                return Err(bstd::mem::String::from_str_in(words.as_str(), frame));
            }
        };
        let planned: &'f plan::Plan<'f, 'static> = std::boxed::Box::leak(std::boxed::Box::new_in(planned, frame));
        let units: &'f [Unit<'f, 'static>] = &planned.units;
        Ok(Lockfile {
            text,
            lock,
            units,
            index: fetch_task::index(units, heap.alloc()),
            cargo: CargoCache::new(),
            sources: None,
            #[cfg(feature = "fetch")]
            net: None,
        })
    }

    /// Short human-readable crate or repository name for a `Fetch` task (for live status display).
    pub(crate) fn display_name(&self, name: Name) -> Option<&str> {
        let at = usize::try_from(fetch_task::unit_of(&self.index, name)?).ok()?;
        match self.units.get(at)? {
            Unit::Crate { name, .. } => Some(name),
            Unit::Git { repo, .. } => {
                let trimmed = repo.trim_end_matches('/').trim_end_matches(".git");
                Some(trimmed.rsplit('/').next().unwrap_or(trimmed))
            }
        }
    }

    /// The units of the `Fetch` subjects a level of the graph asked for, wanted of the
    /// command's machine at once, before their tasks run one after another: each task then
    /// steps the machine, so the level's fetches overlap up to its in-flight limit. Nothing is
    /// looked up or sent here. A subject no unit has is left to its task, which names it.
    pub(crate) fn want_ahead(&mut self, subjects: &[ContentId], env: &FetchEnv<'_>) {
        #[cfg(feature = "fetch")]
        {
            let units = self.units;
            let net = match &mut self.net {
                Some(net) => net,
                none => none.insert(net::Net::new(units, env)),
            };
            for subject in subjects {
                if let Some(at) = fetch_task::unit_of(&self.index, Name::new(blaze_drive::Kind::Fetch, *subject)) {
                    net.want(at);
                }
            }
        }
        #[cfg(not(feature = "fetch"))]
        let _ = (subjects, env);
    }
}

/// What the fetch needs of the command beyond the plan: its clock, for the machine's
/// deadlines, and where the checkout is, for the records' driver.
pub struct FetchEnv<'f> {
    pub clock: bstd::clock::Clock,
    /// The checkout, relative to where blaze was started ("" for there).
    pub checkout: &'f str,
    /// The checkout's absolute root, resolved once at the command's edge
    /// (engine_script_reads.rs `Served::at_edge`): what a script's served tree is rooted at.
    pub served: crate::engine_script_reads::Served<'f>,
    /// `SSL_CERT_FILE` and `SSL_CERT_DIR`, read at the edge (rule 75): where the HTTPS
    /// client's roots are looked for, besides the machine's store.
    pub cert_file: Option<&'f str>,
    pub cert_dir: Option<&'f str>,
    /// `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS` and the two config variables of cargo's rustflags
    /// rule, read at the edge (rule 75): the units' `--cfg`s (graph.rs `target`).
    pub rustflags: blaze_manifest::rustflags::RustflagsEnv<'f>,
    /// The user's home, read at the edge (rule 75): the shared store is under it (the module
    /// store, macros.rs). `None` when `HOME` is not set.
    pub home: Option<&'f str>,
    /// Cargo's home, read at the edge (rule 75; D78): `CARGO_HOME`, else `<HOME>/.cargo`; `None`
    /// when neither is set. Its `registry/cache` is the second place a crate's archive comes
    /// from (fetch.md, "Where bytes come from"), adopted only by the lock's pin.
    pub cargo_home: Option<&'f str>,
    /// The checkout's root as the edge resolved it (`Served::at_edge`, one realpath; never
    /// `PWD`, rule 75). `None` in a simulated world. No stage reads it today: the shelf is opened
    /// from `home` alone (toolchain.rs `HomeShelf::open`).
    pub start: Option<&'f str>,
    /// The toolchain this command opens, given by `main` (`blaze_toolchain::PINNED`) and never
    /// read deeper (rule 75): the target's cfg, the units' output and the names world take it.
    pub toolchain: blaze_toolchain::ToolchainId,
    /// The command's files: the toolchain's shelf and the names step read through them.
    pub fs: bstd::fs::Fs,
    /// The macOS SDK, resolved once at the edge (rule 75; sdk.rs, blaze-apple-sdk): `SDKROOT`,
    /// else what xcrun prints. The native build-script runner (D112) takes its folder and the
    /// scripts' keys its identity (scripts.rs); a refusal fails only a script placed native,
    /// with its words.
    pub sdk: Result<blaze_apple_sdk::Sdk<'f>, blaze_apple_sdk::SdkRefused<'f>>,
    /// The folder of the running blaze, read once at the edge (rule 75; main's ruling for N7):
    /// the fact shim (`blaze-fact-shim`) is looked for beside it (scripts.rs). A native script
    /// is refused naming why when it is not known.
    pub exe_dir: Result<&'f str, ExeDirWhy>,
    /// Optional LAN UDP peer-to-peer `.crate` fetch configuration, read at the edge (rule 75:
    /// `BLAZE_NO_LAN_PEER=1` disables it; disabled in offline unit tests).
    pub lan_peer: LanPeerConfig,
    /// The process's heap, handed from `main` (D102, D120; never reached for deeper): what the
    /// command keeps beyond a task's frame (the lock, its plan, the fetch machine) and what a
    /// task hands to the store is made on it.
    pub heap: &'static blaze_own::Heap,
}

/// Configuration for optional parallel LAN UDP peer-to-peer `.crate` racing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LanPeerConfig {
    /// Whether LAN UDP peer discovery is enabled.
    pub enabled: bool,
    /// UDP discovery port (default `blaze_fetch::peer::DEFAULT_UDP_PORT` = 41729).
    pub udp_port: u16,
    /// Optional direct unicast probe target (for deterministic loopback integration tests).
    pub unicast_target: Option<core::net::SocketAddr>,
}

impl LanPeerConfig {
    /// Default enabled configuration on port 41729.
    pub const DEFAULT: LanPeerConfig = LanPeerConfig {
        enabled: true,
        udp_port: blaze_fetch::peer::DEFAULT_UDP_PORT,
        unicast_target: None,
    };

    /// Disabled configuration (no UDP socket opened).
    pub const DISABLED: LanPeerConfig = LanPeerConfig {
        enabled: false,
        udp_port: blaze_fetch::peer::DEFAULT_UDP_PORT,
        unicast_target: None,
    };

    /// Parses `BLAZE_NO_LAN_PEER`: `"1"`, `"true"`, or `"yes"` disables LAN peer probing.
    pub fn from_env(no_lan_peer: Option<&str>) -> LanPeerConfig {
        let disabled = matches!(no_lan_peer, Some("1" | "true" | "TRUE" | "yes" | "YES"));
        LanPeerConfig { enabled: !disabled, ..LanPeerConfig::DEFAULT }
    }
}

/// Why the running blaze's own folder is not known (FetchEnv::exe_dir).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExeDirWhy {
    /// `std::env::current_exe` failed with this kind.
    Unknown { kind: std::io::ErrorKind },
    /// Its path has no parent folder, or that folder's path is not UTF-8.
    NotUtf8,
    /// This caller did not read it (a test that runs no native script).
    NotRead,
}

impl core::fmt::Display for ExeDirWhy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExeDirWhy::Unknown { kind } => write!(f, "the running blaze's own path is not known (current_exe: {kind})"),
            ExeDirWhy::NotUtf8 => f.write_str("the running blaze's own folder is not a UTF-8 path"),
            ExeDirWhy::NotRead => f.write_str("the running blaze's own folder was not read by this caller (a test)"),
        }
    }
}

/// One `Fetch` task: its unit's record held in the machine's sources store (never the
/// checkout's: [`SOURCES`]), or the finding of why not.
pub(crate) fn run<'b>(
    cx: &Cx,
    lockfile: &mut Lockfile<'_>,
    env: &FetchEnv<'_>,
    name: Name,
    held: &'b Frame,
) -> Result<Did<'b>, CliError> {
    let cx = cx.span("fetch: one lock entry's sources");
    let Some(at) = fetch_task::unit_of(&lockfile.index, name) else {
        return Ok(Did::Failed(TaskFailure { name, why: Why::Refused { what: "a `Fetch` name no unit of the lock has", rule: "fetch_task::unit_of" } }));
    };
    let unit = &lockfile.units[at as usize];
    // The sources store, where every place's record is looked for and written.
    let root: &Dir<ReadWrite> = match &mut lockfile.sources {
        Some(sources) => &*sources,
        none => &*none.insert(sources_dir(&cx, env)?),
    };
    #[cfg(feature = "fetch")]
    let made = {
        let units = lockfile.units;
        let net = match &mut lockfile.net {
            Some(net) => net,
            none => none.insert(net::Net::new(units, env)),
        };
        // The machine's `lookup` tries the places on this machine first (net.rs `Scope::lookup`);
        // what they gave is named beside the network's outcome when no place gave the unit.
        match net.outcome(&cx, root, &mut lockfile.cargo, env, units, &lockfile.index, at)? {
            Ok(outcome) => {
                let mut made = fetch_task::made(unit, &outcome, env.heap.alloc()).ok_or(CliError::Text { what: "the fetch machine stopped with a unit pending" })?;
                if let fetch_task::Made::Failed { message, .. } = &mut made
                    && let Some(tried) = net.tried(at)
                {
                    message.str("; before the network: ").str(tried);
                }
                made
            }
            Err(untrusted) => {
                let mut words = blaze_own::String::new_in(env.heap.alloc());
                words.show(unit);
                if let Some(tried) = net.tried(at) {
                    words.str(" (before the network: ").str(tried).str(")");
                }
                let words = bstd::mem::format_in!(held, "{}: {untrusted}", words.as_str());
                return Ok(Did::Hole(TaskFailure { name, why: Why::Refused { what: "its sources were not fetched now; the next command asks again", rule: "fetch_task::made" } }, held_str(words.as_str(), held)));
            }
        }
    };
    // fetch.md's order of places, each checked against the unit's one key (D78): the sources
    // store, then cargo's archive (adopted only by the lock's SHA-256); this build has no third
    // place (the registry). Not a fallback (rule 3): every place gives the same bytes at the same
    // key or is refused, and a unit no place gives fails naming every place tried.
    #[cfg(not(feature = "fetch"))]
    let made = match held_record(&cx, root, unit)? {
        true => fetch_task::Made::Held { out: fetch_task::source(unit), down: 0 },
        false => {
            let mut tries = blaze_own::Vec::new_in(env.heap.alloc());
            tries.push(Try { place: STORE_PLACE, miss: Miss::Absent });
            match adopt_local(&cx, root, &mut lockfile.cargo, env, unit, &mut tries)? {
                true => fetch_task::Made::Held { out: fetch_task::source(unit), down: 0 },
                false => {
                    let mut message = blaze_own::String::new_in(env.heap.alloc());
                    message.show(&local::Tried { unit, tries: tries.as_slice() }).str("; this build cannot fetch (built without `fetch`)");
                    fetch_task::Made::Failed { message, lasting: false }
                }
            }
        }
    };
    let mut record = Made::new_in(name, held);
    match made {
        fetch_task::Made::Held { out, .. } => record.out = out,
        fetch_task::Made::Failed { message, lasting } => {
            let rule = match cfg!(feature = "fetch") {
                true => SOURCE_REFUSED,
                false => CANNOT_FETCH,
            };
            // Not lasting, with the network: the network's or the store's fault, which the
            // next command may not meet. No record; the job fails with the words, and the
            // stage owes it (engine_units.rs `owes`), so the answer's certificate does not stand
            // and the next command runs it again. (Until 2026-11-20 nothing owed it: the
            // answer was written with the failure in it and the next command's certificate
            // replayed it, turso's 152 fetches, though the sources were in cargo's cache.)
            if !lasting && cfg!(feature = "fetch") {
                return Ok(Did::Hole(TaskFailure { name, why: Why::Refused { what: "its sources were not fetched now (the network's or the store's fault); the next command asks again", rule: "fetch_task::made" } }, held_str(message.as_str(), held)));
            }
            record.find(rule, (0, 0), &[message.as_str()]);
            record.out = ContentId::of(REFUSED_DOMAIN, &name.subject.0);
        }
    }
    Ok(Did::Done(record))
}

/// Holds a mapped fetched source record alive on the process heap for the duration of the command
/// so both unit-graph discovery (`graph.rs`) and world construction (`names.rs`) read the same
/// single `mmap` without re-opening or re-mapping the file.
pub(crate) fn hold_mapping(mapping: blaze_core::Mapping, heap: Alloc<'static>) -> &'static [u8] {
    let mapping: &'static blaze_core::Mapping = blaze_own::Box::leak_static(blaze_own::Box::new_in(mapping, heap));
    mapping.bytes()
}

/// The record file's name at `key`: its 64 hex digits.
pub(crate) fn key_hex(key: &ContentId) -> [u8; 64] {
    let mut hex = [0u8; 64];
    for (i, byte) in key.0.iter().enumerate() {
        hex[2 * i] = b"0123456789abcdef"[usize::from(byte >> 4)];
        hex[2 * i + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
    }
    hex
}

/// The path of the record at `key` in the sources store ([`sources_dir`]): its 64 hex digits.
fn record_path(key: &ContentId) -> Result<Path, CliError> {
    let hex = key_hex(key);
    let hex = core::str::from_utf8(&hex).map_err(|_| CliError::Text { what: "a key's hex is not UTF-8" })?;
    Ok(Path::new(hex)?)
}

/// Whether the sources store `root` ([`sources_dir`]) has a record at `key`: one stat, no read.
/// For a stored `Fetch` row that stands (engine_units.rs `fetch_stands`): every record is
/// published whole under its key ([`write_record`]'s rename) and its bytes were checked once
/// when it was fetched or adopted ([`held_record`]), so a file there is the record the row names
/// (rule 74: checked once, never on each read).
pub(crate) fn holds(cx: &Cx, root: &Dir<ReadWrite>, key: &ContentId) -> Result<bool, CliError> {
    let path = record_path(key)?;
    Ok(root.stat(cx, &path)?.is_some_and(|meta| !meta.dir))
}

/// Whether the sources store `root` ([`sources_dir`]) holds `unit`'s record whole: read and
/// checked against its BLAKE3. One that fails the check is removed (it is made again).
fn held_record(cx: &Cx, root: &Dir<ReadWrite>, unit: &Unit<'_, '_>) -> Result<bool, CliError> {
    let path = record_path(&unit.key())?;
    let Some(meta) = root.stat(cx, &path)?.filter(|meta| !meta.dir) else {
        return Ok(false);
    };
    let reserve = usize::try_from(meta.len).map_err(|_| CliError::Text { what: "a source record past the address space" })?;
    let scratch = Frame::with_reserve(reserve.max(1 << 16).saturating_add(1 << 16), bstd::mem::Tag::Other)
        .map_err(|_| CliError::Text { what: "the frame a source record is checked in could not be reserved" })?;
    let mut bytes = Bytes::new_in(&scratch, "a source record, checked");
    match root.read_all(cx, &path, &mut bytes) {
        Ok(_) => {}
        Err(error) if error.kind == FsKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    if PackView::open_source(&unit.source(), bytes.as_slice()).is_ok() {
        return Ok(true);
    }
    match root.remove(cx, &path, bstd::fs::Durable::No) {
        Ok(()) => Ok(false),
        Err(error) if error.kind == FsKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// `record` published at `path` (a record's, [`record_path`]), as every source record is,
/// whichever place its bytes came from: whole bytes under a unique temporary name, renamed onto
/// the key (`Dir::publish`), so a reader never sees a part; unsynced (a lost record is made
/// again from its place).
fn write_record(cx: &Cx, root: &Dir<ReadWrite>, path: &Path, record: &[u8]) -> Result<(), FsError> {
    root.publish(cx, path, Content::bytes(record), Durable::No).map(|_| ())
}

/// The first place of fetch.md's order, as a finding names it.
const STORE_PLACE: &str = "blaze's sources store";

/// The folder of cargo's home opened as a second tree, and the folder in it listed: together
/// `local::CARGO_CACHE`. The listing is asked of the parent so that a cargo home without a
/// cache is a miss named "absent", not an error.
const CARGO_REGISTRY: &str = "registry";
const CARGO_CACHE_IN_REGISTRY: &str = "cache";

/// Cargo's bare git clones under its home (`local::CARGO_GIT_DB`), opened through the parent
/// folder `git` so a cargo home without `git/db` is a miss named "absent", not an error.
const CARGO_GIT: &str = "git";
const CARGO_DB_IN_GIT: &str = "db";
const CARGO_GIT_OBJECTS: &str = "objects";
const CARGO_GIT_PACK: &str = "pack";
/// Maximum byte length of a single `.idx` file read from cargo's `git/db` (rule 18).
const MOST_GIT_IDX_BYTES: u64 = 64 << 20;
/// Maximum cumulative `.idx` bytes retained across all packs of one repository (rule 18).
const MOST_GIT_IDX_TOTAL_BYTES: u64 = 128 << 20;
/// Maximum number of indexed packs retained for one repository (rule 18).
const MOST_GIT_PACKS: usize = 1024;

/// Cargo's local caches on this machine: `<cargo home>/registry/cache` for `.crate` archives
/// (D78; fetch.md, "Where bytes come from", place 2) and `<cargo home>/git/db` for bare git
/// repositories (fetch.md, piece 14). Each is looked at once a command, lazily on the first
/// missing unit of its kind, through the effect layer's second tree (`Fs::home`), never `std::fs`.
/// Every traversal and read inside `git/db` uses the no-symlink confined APIs (`entries_confined`,
/// `read_all_confined`, `read_range_confined`). Nothing is believed of the place: only a
/// `.crate`'s SHA-256 or a git object's verified SHA-1 reachable from the lock's pinned commit.
pub(crate) struct CargoCache {
    looked: Option<Looked>,
    git: Option<LookedGit>,
    /// Single-folder `.idx` cache (`(folder_index, odb)`): at most one candidate folder's
    /// indexes are retained at any time, bounded independently by `MOST_GIT_PACKS` /
    /// `MOST_GIT_IDX_TOTAL_BYTES` so unrelated or same-stem repositories never share a budget
    /// or accumulate command-wide heap.
    git_odb: Option<(usize, GitDbOdb)>,
}

impl CargoCache {
    pub(crate) fn new() -> CargoCache {
        CargoCache { looked: None, git: None, git_odb: None }
    }
}

/// What looking at cargo's crate cache found.
enum Looked {
    /// No archive can come from it: the one place a finding names, and why (no cargo home, the
    /// home not opened, no cache, or no crates.io folder in it).
    Closed { shown: blaze_own::String<Alloc<'static>>, miss: Miss },
    /// The cache's parent folder, read only, and crates.io's folders in the cache
    /// (`local::crates_io_folder`), sorted by the names' bytes so every machine tries them in one
    /// order: each as the person knows it (`<cargo home>/registry/cache/<folder>`) with where its
    /// own name starts.
    Open { registry: Dir<ReadOnly>, folders: blaze_own::Vec<(blaze_own::String<Alloc<'static>>, usize), Alloc<'static>> },
}

/// Why a local cargo cache folder could not be searched (Copy so borrowing never calls `.clone()`).
#[derive(Clone, Copy)]
enum ClosedMiss {
    Absent,
    Unreadable,
}

impl ClosedMiss {
    fn to_miss(self) -> Miss {
        match self {
            ClosedMiss::Absent => Miss::Absent,
            ClosedMiss::Unreadable => Miss::Unreadable,
        }
    }
}

/// What looking at cargo's bare git db (`<cargo home>/git/db`) found.
enum LookedGit {
    Closed { shown: blaze_own::String<Alloc<'static>>, miss: ClosedMiss },
    Open {
        git: Dir<ReadOnly>,
        shown: blaze_own::String<Alloc<'static>>,
        folders: blaze_own::Vec<GitDbFolder, Alloc<'static>>,
    },
}

struct GitDbFolder {
    /// `<cargo home>/git/db/<folder>` and byte offset where `<folder>` starts.
    shown: blaze_own::String<Alloc<'static>>,
    name_at: usize,
}

impl GitDbFolder {
    fn name(&self) -> &str {
        self.shown.as_str().get(self.name_at..).unwrap_or("")
    }
}

struct GitPackEntry {
    /// Relative path `db/<folder>/objects/pack/pack-<40hex>.pack` beneath `<cargo home>/git`.
    pack_path: Path,
    /// Raw `.idx` bytes (loaded once per matching repo via `read_all_confined`).
    idx_bytes: blaze_own::Vec<u8, Alloc<'static>>,
    /// Verified `.pack` file byte length from `entries_confined`.
    pack_len: u64,
}

enum GitDbOdb {
    Unreadable,
    Ready { packs: blaze_own::Vec<GitPackEntry, Alloc<'static>> },
}

impl CargoCache {
    /// What the crate cache holds, looked at on the first crate miss of the command.
    fn looked(&mut self, cx: &Cx, env: &FetchEnv<'_>) -> Result<&Looked, CliError> {
        Ok(match &mut self.looked {
            Some(looked) => &*looked,
            none => &*none.insert(Looked::open(cx, env)?),
        })
    }
}

/// Loads confined `.idx` tables for a single bare repository folder under `<cargo home>/git/db`,
/// applying `MOST_GIT_PACKS` and `MOST_GIT_IDX_TOTAL_BYTES` strictly to that folder.
fn load_git_folder_odb(
    cx: &Cx,
    git: &Dir<ReadOnly>,
    folder_name: &str,
    global: Alloc<'static>,
) -> Result<GitDbOdb, CliError> {
    let db = Path::new(CARGO_DB_IN_GIT)?;
    let objects = Path::new(CARGO_GIT_OBJECTS)?;
    let pack = Path::new(CARGO_GIT_PACK)?;
    let Ok(folder_path) = Path::new(folder_name) else {
        return Ok(GitDbOdb::Unreadable);
    };
    let pack_dir = db.join(&folder_path).join(&objects).join(&pack);
    match git.entries_confined(cx, &pack_dir) {
        Ok(Some(entries)) => {
            let mut packs = blaze_own::Vec::new_in(global);
            let mut folder_packs: usize = 0;
            let mut folder_idx_bytes: u64 = 0;
            let mut failed = false;
            for i in 0..entries.len() {
                let name = entries.name(i);
                if local::git_pack_file(name) {
                    if entries.kind(i) != bstd::fs::EntryKind::File {
                        failed = true;
                        break;
                    }
                    let Some(stem) = name.strip_suffix(".pack") else { continue };
                    let idx_name_str = bstd::mem::format_in!(cx.frame(), "{stem}.idx");
                    if entries.find(idx_name_str.as_str()).is_none() {
                        failed = true;
                        break;
                    }
                    continue;
                }
                if !local::git_idx_file(name) {
                    continue;
                }
                if entries.kind(i) != bstd::fs::EntryKind::File {
                    failed = true;
                    break;
                }
                if folder_packs >= MOST_GIT_PACKS {
                    failed = true;
                    break;
                }
                let idx_len = entries.len_of(i);
                let next_idx_bytes = folder_idx_bytes.saturating_add(idx_len);
                if idx_len > MOST_GIT_IDX_BYTES || next_idx_bytes > MOST_GIT_IDX_TOTAL_BYTES {
                    failed = true;
                    break;
                }
                let Some(stem) = name.strip_suffix(".idx") else { continue };
                let pack_name_str = bstd::mem::format_in!(cx.frame(), "{stem}.pack");
                let Some(pack_i) = entries.find(pack_name_str.as_str()) else {
                    failed = true;
                    break;
                };
                if entries.kind(pack_i) != bstd::fs::EntryKind::File {
                    failed = true;
                    break;
                }
                let pack_len = entries.len_of(pack_i);
                if pack_len < 32 {
                    failed = true;
                    break;
                }
                let (Ok(idx_leaf), Ok(pack_leaf)) = (Path::new(name), Path::new(pack_name_str.as_str())) else {
                    failed = true;
                    break;
                };
                let idx_path = pack_dir.join(&idx_leaf);
                let pack_path = pack_dir.join(&pack_leaf);
                let Ok(idx_usize) = usize::try_from(idx_len) else {
                    failed = true;
                    break;
                };
                let reserve = idx_usize.saturating_mul(2).max(1 << 16).saturating_add(1 << 16);
                let scratch = Frame::with_reserve(reserve, bstd::mem::Tag::Other)
                    .map_err(|_| CliError::Text { what: "the frame a cargo git pack .idx is read in could not be reserved" })?;
                let mut raw = Bytes::new_in(&scratch, "a cargo git pack .idx, confined read");
                match git.read_all_confined(cx, &idx_path, &mut raw) {
                    Ok(_) => {}
                    Err(error) if error.stopped().is_some() => return Err(CliError::Fs(error)),
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
                let actual_len = u64::try_from(raw.len()).unwrap_or(u64::MAX);
                let actual_total = folder_idx_bytes.saturating_add(actual_len);
                if actual_len > MOST_GIT_IDX_BYTES || actual_total > MOST_GIT_IDX_TOTAL_BYTES {
                    failed = true;
                    break;
                }
                let mut idx_bytes = blaze_own::Vec::new_in(global);
                idx_bytes
                    .try_extend_from_slice(raw.as_slice())
                    .map_err(|_| CliError::Text { what: "a cargo git pack .idx could not be allocated" })?;
                folder_packs = folder_packs.saturating_add(1);
                folder_idx_bytes = actual_total;
                packs
                    .try_push(GitPackEntry { pack_path, idx_bytes, pack_len })
                    .map_err(|_| CliError::Text { what: "a cargo git pack entry could not be allocated" })?;
            }
            if failed {
                Ok(GitDbOdb::Unreadable)
            } else {
                Ok(GitDbOdb::Ready { packs })
            }
        }
        // No `objects/pack` directory yet: loose objects under `objects/` may still hold the commit.
        Ok(None) => Ok(GitDbOdb::Ready { packs: blaze_own::Vec::new_in(global) }),
        Err(error) if error.stopped().is_some() => Err(CliError::Fs(error)),
        Err(_) => Ok(GitDbOdb::Unreadable),
    }
}

impl LookedGit {
    fn open(cx: &Cx, env: &FetchEnv<'_>) -> Result<LookedGit, CliError> {
        let global = env.heap.alloc();
        let mut shown = blaze_own::String::new_in(global);
        let Some(home) = env.cargo_home else {
            shown.str("cargo's git db (neither CARGO_HOME nor HOME is set)");
            return Ok(LookedGit::Closed { shown, miss: ClosedMiss::Absent });
        };
        shown.str(home.trim_end_matches('/')).str("/").str(local::CARGO_GIT_DB);
        let said = |shown: &mut blaze_own::String<Alloc<'static>>, error: &dyn core::fmt::Display| {
            core::fmt::Write::write_fmt(shown, format_args!(" ({error})")).map_err(|_| CliError::Text { what: "a place's words could not be written" })
        };
        let git = match env.fs.home(cx, home, &Path::new(CARGO_GIT)?) {
            Ok(git) => git.read_only(),
            Err(error) => {
                said(&mut shown, &error)?;
                return Ok(LookedGit::Closed { shown, miss: ClosedMiss::Unreadable });
            }
        };
        let entries = match git.entries_confined(cx, &Path::new(CARGO_DB_IN_GIT)?) {
            Ok(Some(entries)) => entries,
            Ok(None) => return Ok(LookedGit::Closed { shown, miss: ClosedMiss::Absent }),
            Err(error) if error.stopped().is_some() => return Err(CliError::Fs(error)),
            Err(error) => {
                said(&mut shown, &error)?;
                return Ok(LookedGit::Closed { shown, miss: ClosedMiss::Unreadable });
            }
        };
        let mut folders = blaze_own::Vec::new_in(global);
        for (name, kind) in entries.iter() {
            // Only real directories under `git/db`: symlinks are never traversed.
            if kind != bstd::fs::EntryKind::Dir {
                continue;
            }
            let mut place = blaze_own::String::new_in(global);
            place.str(shown.as_str()).str("/");
            let name_at = place.as_str().len();
            place.str(name);
            folders.push(GitDbFolder { shown: place, name_at });
        }
        if folders.is_empty() {
            shown.str(" (no repository folder in it)");
            return Ok(LookedGit::Closed { shown, miss: ClosedMiss::Absent });
        }
        Ok(LookedGit::Open { git, shown, folders })
    }
}

impl Looked {
    fn open(cx: &Cx, env: &FetchEnv<'_>) -> Result<Looked, CliError> {
        // Kept for the command (the `CargoCache` is the lock's): on the process's heap.
        let global = env.heap.alloc();
        let mut shown = blaze_own::String::new_in(global);
        let Some(home) = env.cargo_home else {
            shown.str("cargo's archive cache (neither CARGO_HOME nor HOME is set)");
            return Ok(Looked::Closed { shown, miss: Miss::Absent });
        };
        shown.str(home.trim_end_matches('/')).str("/").str(local::CARGO_CACHE);
        let said = |shown: &mut blaze_own::String<Alloc<'static>>, error: &dyn core::fmt::Display| {
            core::fmt::Write::write_fmt(shown, format_args!(" ({error})")).map_err(|_| CliError::Text { what: "a place's words could not be written" })
        };
        let registry = match env.fs.home(cx, home, &Path::new(CARGO_REGISTRY)?) {
            Ok(registry) => registry.read_only(),
            Err(error) => {
                said(&mut shown, &error)?;
                return Ok(Looked::Closed { shown, miss: Miss::Unreadable });
            }
        };
        let entries = match registry.entries(cx, &Path::new(CARGO_CACHE_IN_REGISTRY)?) {
            Ok(Some(entries)) => entries,
            Ok(None) => return Ok(Looked::Closed { shown, miss: Miss::Absent }),
            Err(error) => {
                said(&mut shown, &error)?;
                return Ok(Looked::Closed { shown, miss: Miss::Unreadable });
            }
        };
        let mut folders = blaze_own::Vec::new_in(global);
        // In the listing's order: sorted by the names' bytes (`Dir::entries`).
        for (name, _) in entries.iter().filter(|(name, _)| local::crates_io_folder(name)) {
            let mut place = blaze_own::String::new_in(global);
            place.str(shown.as_str()).str("/");
            let at = place.as_str().len();
            place.str(name);
            folders.push((place, at));
        }
        if folders.is_empty() {
            shown.str(" (no crates.io folder in it)");
            return Ok(Looked::Closed { shown, miss: Miss::Absent });
        }
        Ok(Looked::Open { registry, folders })
    }
}

/// The second place of fetch.md's order for `unit`, after the sources store lacked it:
/// - for a registry crate, its archive in `<cargo home>/registry/cache`, adopted only when its
///   SHA-256 is the lock's (`local::adopt`, D78);
/// - for a git commit, packfiles in matching `<cargo home>/git/db/<repo>-<16hex>/objects/pack`,
///   adopted only when an object hashes to the lock's pinned commit SHA-1 (`local::adopt_git_pack`,
///   fetch.md piece 14).
///
/// The record is written at the unit's key exactly as the network path's record is
/// ([`write_record`]): the same bytes at the same key, so no reader can tell which place gave
/// them. `true` when adopted; else every place tried, and what it gave, is pushed on `tries` for
/// the finding. Nothing is adopted without the pin, and nothing refused is passed over in silence.
fn adopt_local<'c>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    cargo: &'c mut CargoCache,
    env: &FetchEnv<'_>,
    unit: &Unit<'_, '_>,
    tries: &mut blaze_own::Vec<Try<'c>, Alloc<'static>>,
) -> Result<bool, CliError> {
    match unit {
        Unit::Crate { .. } => adopt_local_crate(cx, root, cargo, env, unit, tries),
        Unit::Git { repo, .. } => adopt_local_git(cx, root, cargo, env, unit, repo, tries),
    }
}

fn adopt_local_crate<'c>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    cargo: &'c mut CargoCache,
    env: &FetchEnv<'_>,
    unit: &Unit<'_, '_>,
    tries: &mut blaze_own::Vec<Try<'c>, Alloc<'static>>,
) -> Result<bool, CliError> {
    let looked: &'c Looked = cargo.looked(cx, env)?;
    let (registry, folders) = match looked {
        Looked::Closed { shown, miss } => {
            tries.push(Try { place: shown.as_str(), miss: miss.clone() });
            return Ok(false);
        }
        Looked::Open { registry, folders } => (registry, folders),
    };
    let archive = match local::archive_name(unit) {
        Ok(archive) => archive,
        Err(refused) => {
            tries.push(Try { place: local::CARGO_CACHE, miss: Miss::Refused(refused) });
            return Ok(false);
        }
    };
    let archive = Path::new(archive.as_str())?;
    let cache = Path::new(CARGO_CACHE_IN_REGISTRY)?;
    for (shown, at) in folders.iter() {
        let place = shown.as_str();
        let path = cache.join(&Path::new(&place[*at..])?).join(&archive);
        let len = match registry.stat(cx, &path) {
            Ok(Some(meta)) if !meta.dir => meta.len,
            Ok(_) => {
                tries.push(Try { place, miss: Miss::Absent });
                continue;
            }
            Err(_) => {
                tries.push(Try { place, miss: Miss::Unreadable });
                continue;
            }
        };
        let Ok(len) = usize::try_from(len) else {
            tries.push(Try { place, miss: Miss::Unreadable });
            continue;
        };
        let scratch = Frame::with_reserve(len.max(1 << 16).saturating_add(1 << 16), bstd::mem::Tag::Other)
            .map_err(|_| CliError::Text { what: "the frame a cargo archive is read in could not be reserved" })?;
        let mut bytes = Bytes::new_in(&scratch, "an archive in cargo's cache, checked against the lock");
        if registry.read_all(cx, &path, &mut bytes).is_err() {
            tries.push(Try { place, miss: Miss::Unreadable });
            continue;
        }
        match local::adopt(unit, bytes.as_slice(), &blaze_fetch::fetch::Limits::REAL.unpack, env.heap.alloc()) {
            Ok((record, _)) => {
                write_record(cx, root, &record_path(&unit.key())?, record.as_slice())?;
                return Ok(true);
            }
            Err(refused) => tries.push(Try { place, miss: Miss::Refused(refused) }),
        }
    }
    Ok(false)
}

/// Confined `GitObjectSource` backed by `<cargo home>/git/db/<folder>/objects`: reads loose
/// objects via `Dir::read_all_confined` and packfile slices via `Dir::read_range_confined` so no
/// symlink inside `CARGO_HOME` is ever followed and no unreachable pack bytes are ever read.
struct CargoGitSource<'a> {
    cx: &'a Cx,
    git: &'a Dir<ReadOnly>,
    objects_dir: Path,
    packs: &'a [GitPackEntry],
    /// Upper bound for a single loose object file, derived from `git_pack.most_object` plus
    /// worst-case zlib framing overhead.
    most_loose_bytes: u64,
    /// Set when a confined loose or pack read fails due to OS I/O, symlink refusal, or truncation.
    io_failed: bool,
    /// Set when reading a loose object or pack range fails due to frame/heap allocation exhaustion.
    alloc_failed: Option<&'static str>,
}

impl local::GitObjectSource<Alloc<'static>> for CargoGitSource<'_> {
    fn read_loose(
        &mut self,
        oid: &local::GitOid,
        out: &mut blaze_own::Vec<u8, Alloc<'static>>,
    ) -> Result<bool, local::GitStoreWhy> {
        out.clear();
        let hex = oid.hex();
        let (Ok(dir_str), Ok(file_str)) = (core::str::from_utf8(&hex[..2]), core::str::from_utf8(&hex[2..])) else {
            return Err(local::GitStoreWhy::LooseHeader);
        };
        let (Ok(dir_part), Ok(file_part)) = (Path::new(dir_str), Path::new(file_str)) else {
            return Err(local::GitStoreWhy::LooseHeader);
        };
        let path = self.objects_dir.join(&dir_part).join(&file_part);
        let Ok(reserve) = usize::try_from(self.most_loose_bytes) else {
            self.alloc_failed = Some("a cargo git loose object exceeds addressable memory");
            return Err(local::GitStoreWhy::LooseHeader);
        };
        let Ok(scratch) = Frame::with_reserve(reserve.saturating_add(1 << 16), bstd::mem::Tag::Other) else {
            self.alloc_failed = Some("the frame a cargo git loose object is read in could not be reserved");
            return Err(local::GitStoreWhy::LooseHeader);
        };
        let mut bytes = Bytes::new_in(&scratch, "a cargo git loose object, confined read");
        match self.git.read_all_confined(self.cx, &path, &mut bytes) {
            Ok(_) => {
                if out.try_extend_from_slice(bytes.as_slice()).is_err() {
                    self.alloc_failed = Some("a cargo git loose object could not be allocated");
                    return Err(local::GitStoreWhy::LooseHeader);
                }
                Ok(true)
            }
            Err(error) if error.kind == FsKind::NotFound => Ok(false),
            Err(error) if matches!(error.stopped(), Some(bstd::Stopped::OutOfMemory { .. })) => {
                self.alloc_failed = Some("a cargo git loose object exceeded its read frame");
                Err(local::GitStoreWhy::LooseHeader)
            }
            Err(_) => {
                self.io_failed = true;
                Err(local::GitStoreWhy::LooseHeader)
            }
        }
    }

    fn read_pack_range(
        &mut self,
        pack_idx: usize,
        offset: u64,
        len: u64,
        out: &mut blaze_own::Vec<u8, Alloc<'static>>,
    ) -> Result<bool, local::GitStoreWhy> {
        out.clear();
        let Some(pack) = self.packs.get(pack_idx) else {
            self.io_failed = true;
            return Err(local::GitStoreWhy::IdxShort);
        };
        let Ok(len_usize) = usize::try_from(len) else {
            self.alloc_failed = Some("a cargo git pack range exceeds addressable memory");
            return Err(local::GitStoreWhy::IdxShort);
        };
        let reserve = len_usize.saturating_mul(2).max(1 << 16).saturating_add(1 << 16);
        let Ok(scratch) = Frame::with_reserve(reserve, bstd::mem::Tag::Other) else {
            self.alloc_failed = Some("the frame a cargo git pack range is read in could not be reserved");
            return Err(local::GitStoreWhy::IdxShort);
        };
        let mut bytes = Bytes::new_in(&scratch, "a cargo git pack range, confined read");
        match self.git.read_range_confined(self.cx, &pack.pack_path, offset, len_usize, &mut bytes) {
            Ok(read) if read == len_usize => {
                if out.try_extend_from_slice(bytes.as_slice()).is_err() {
                    self.alloc_failed = Some("a cargo git pack range could not be allocated");
                    return Err(local::GitStoreWhy::IdxShort);
                }
                Ok(true)
            }
            Err(error) if error.stopped().is_some() => {
                self.alloc_failed = Some("a cargo git pack range read stopped");
                Err(local::GitStoreWhy::IdxShort)
            }
            _ => {
                self.io_failed = true;
                Err(local::GitStoreWhy::IdxShort)
            }
        }
    }
}

fn adopt_local_git<'c>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    cargo: &'c mut CargoCache,
    env: &FetchEnv<'_>,
    unit: &Unit<'_, '_>,
    repo: &str,
    tries: &mut blaze_own::Vec<Try<'c>, Alloc<'static>>,
) -> Result<bool, CliError> {
    if cargo.git.is_none() {
        cargo.git = Some(LookedGit::open(cx, env)?);
    }
    let Some(looked) = cargo.git.as_ref() else {
        return Ok(false);
    };
    let (git, db_shown, folders) = match looked {
        LookedGit::Closed { shown, miss } => {
            tries.push(Try { place: shown.as_str(), miss: miss.to_miss() });
            return Ok(false);
        }
        LookedGit::Open { git, shown, folders } => (git, shown, folders),
    };
    let cached_odb = &mut cargo.git_odb;
    let db = Path::new(CARGO_DB_IN_GIT)?;
    let objects = Path::new(CARGO_GIT_OBJECTS)?;
    let mut matched = false;
    for (folder_idx, folder) in folders.iter().enumerate() {
        let folder_name = folder.name();
        if !local::git_db_folder(repo, folder_name) {
            continue;
        }
        matched = true;
        let place = folder.shown.as_str();
        if !matches!(cached_odb, Some((idx, _)) if *idx == folder_idx) {
            // Release any previously cached folder's indexes before loading this candidate so
            // peak retained `.idx` memory is always bounded to one folder's cap and candidate
            // folders never share a budget.
            *cached_odb = None;
            let loaded = load_git_folder_odb(cx, git, folder_name, env.heap.alloc())?;
            *cached_odb = Some((folder_idx, loaded));
        }
        let packs = match cached_odb.as_ref() {
            Some((_, GitDbOdb::Ready { packs })) => packs.as_slice(),
            Some((_, GitDbOdb::Unreadable)) | None => {
                tries.push(Try { place, miss: Miss::Unreadable });
                continue;
            }
        };
        let Ok(folder_path) = Path::new(folder_name) else {
            tries.push(Try { place, miss: Miss::Unreadable });
            continue;
        };
        let objects_dir = db.join(&folder_path).join(&objects);
        let mut tables: blaze_own::Vec<(&[u8], u64), Alloc<'static>> = blaze_own::Vec::new_in(env.heap.alloc());
        for entry in packs.iter() {
            tables
                .try_push((entry.idx_bytes.as_slice(), entry.pack_len))
                .map_err(|_| CliError::Text { what: "a cargo git pack table could not be allocated" })?;
        }
        let unpack_limits = if cx.is_simulated() {
            // Simulated `MappingWrite` backs reservations with a heap buffer, so bound simulation
            // reservations while keeping the exact same `Dir::reserve` + `pack::Fixed` path.
            let mut sim = blaze_fetch::fetch::Limits::REAL.unpack;
            sim.pack.most_files = 1024;
            sim.pack.most_bytes = 2 << 20;
            sim.pack.most_path = 512;
            sim
        } else {
            blaze_fetch::fetch::Limits::REAL.unpack
        };
        let most_obj = unpack_limits.git_pack.most_object;
        let most_loose_bytes = most_obj.saturating_add(most_obj / 64).saturating_add(4096);
        let target = record_path(&unit.key())?;
        let room = blaze_fetch::unpack::git_room(&unpack_limits);
        let mut source = CargoGitSource { cx, git, objects_dir, packs, most_loose_bytes, io_failed: false, alloc_failed: None };
        match root.reserve(cx, &target, room, |buf| {
            local::adopt_git_db_in(
                unit,
                tables.as_slice(),
                &mut source,
                &unpack_limits,
                blaze_fetch::pack::Fixed::new(buf),
                env.heap.alloc(),
            )
            .map(|(fixed, _)| fixed.written())
        })? {
            Ok(()) => return Ok(true),
            Err(_) if source.alloc_failed.is_some() => {
                if let Some(what) = source.alloc_failed {
                    return Err(CliError::Text { what });
                }
            }
            Err(refused) if refused.is_alloc() => {
                return Err(CliError::Text { what: "a cargo git record could not be allocated" });
            }
            Err(_) if source.io_failed => tries.push(Try { place, miss: Miss::Unreadable }),
            Err(refused) => tries.push(Try { place, miss: Miss::Refused(refused) }),
        }
    }
    if !matched {
        tries.push(Try { place: db_shown.as_str(), miss: Miss::Absent });
    }
    Ok(false)
}

/// The command adopts: cargo's archive taken by the lock's pin only (fetch.md piece 2; D78);
/// the record in the home's sources store, shared by worktrees (piece 3).
#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;

#[cfg(test)]
thread_local! {
    /// A test's network: `false` and the HTTPS client is not made (`net::client`), so a unit no
    /// place on the machine holds fails as with no network, deterministically and with no
    /// socket opened (only ever in this crate's tests; macOS reads no `SSL_CERT_FILE`, so a test
    /// cannot break the client by its environment).
    pub(crate) static NETWORK: core::cell::Cell<bool> = const { core::cell::Cell::new(true) };
}

#[cfg(feature = "fetch")]
mod net {
    //! The machine and its IO: blaze-http's driver thread for HTTPS + optional parallel LAN peer
    //! HTTP/UDP race, the records' files, the command's clock.

    use core::net::{Ipv4Addr, SocketAddr};
    use std::collections::HashMap;
    use std::{process, time::Duration};

    use blaze_core::reserve::{Next, Reserve, Step as Reserving};
    use blaze_core::{Done, FileName, Tag};
    use blaze_drive::fetch_task;
    use blaze_drive::{Kind as TaskKind, Name};
    use blaze_fetch::fetch::{Ask, Outcome};
    use blaze_fetch::peer::{HAVE_MAX_BYTES, MULTICAST_GROUP, decode_has, encode_have};
    use blaze_fetch::plan::Unit;
    use blaze_fetch::{FetchIo, Limits, Machine, Method, NetFailure, Reply, Reserved, StoreFailure, Woke};
    use blaze_http::driver::{FetchEvent, Fetched, Handle, PeerDriver};
    use blaze_http::{Kind, Proxies, Request, Step, mailbox};
    use blaze_ids::ContentId;
    use blaze_own::Alloc;
    use bstd::cx::Cx;
    use bstd::fs::{Dir, FsKind, ReadWrite};

    use blaze_fetch::local::{Miss, Tried, Try};

    use super::{CargoCache, FetchEnv, LanPeerConfig, STORE_PLACE, SOURCES, adopt_local, held_record, record_path, write_record};
    use crate::CliError;

    const USER_AGENT: &str = concat!("blaze/", env!("CARGO_PKG_VERSION"));
    /// How long one step waits when no deadline is due: the machine's own bound.
    const STEP_MOST_NS: u64 = 1_000_000_000;
    /// Steps past which a unit that has not settled is a hang (rule 18): the machine's requests
    /// each end by their deadline (`Limits::REAL.ask_ns`, each tried once, D172), so a unit
    /// settles within that many seconds' steps: the advert's and the pack's for a git unit, a
    /// resized ask past `most` at most twice.
    const STEPS_MOST: u64 = 4 * 120;

    /// Why HTTPS is not set up: each names what.
    #[derive(Clone, Debug)] // clone: an error's words, once a unit that needed them
    pub(super) enum Untrusted {
        /// The machine's trust anchors could not be read or made into rustls's roots
        /// (`blaze_http::roots::system`): its words name every place looked. Not lasting: the
        /// machine's store or the environment may change before the next command.
        Tls(std::string::String),
        /// blaze-http's driver thread did not start.
        Driver(std::string::String),
    }

    impl core::fmt::Display for Untrusted {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Untrusted::Tls(why) => write!(f, "HTTPS could not be set up: {why}"),
                Untrusted::Driver(why) => write!(f, "blaze-http's driver thread did not start: {why}"),
            }
        }
    }

    /// The machine and its IO, made at the command's first `Fetch` task.
    pub(super) struct Net<'f> {
        machine: Machine<'f, 'f, 'static>,
        io: Io,
        steps: u64,
    }

    impl<'f> Net<'f> {
        /// The machine over the lock's plan, wanting nothing yet: a unit is wanted when the
        /// graph reaches it ([`Lockfile::want_ahead`]) or its task asks ([`Net::outcome`]), so
        /// a lock entry nothing reaches is never looked up or fetched.
        pub(super) fn new(units: &'f [Unit<'f, 'static>], env: &FetchEnv<'_>) -> Net<'f> {
            let mut lan = env.lan_peer;
            #[cfg(test)]
            if !super::NETWORK.get() {
                lan.enabled = false;
            }
            let mut limits = Limits::REAL;
            limits.peer_race = lan.enabled;
            let alloc = env.heap.alloc();
            let machine = Machine::wanting(units, limits, alloc);
            let (to, answers) = blaze_core::chan::unbounded::<FetchEvent>();
            // The sources store beneath the home (`sources_native`). Empty with no home: then
            // every reservation fails as the store's, though `run` has stopped the command
            // first (`sources_dir` refuses no home).
            let mut sources = std::string::String::new();
            if let Some(home) = env.home {
                sources.push_str(home.trim_end_matches('/'));
                sources.push('/');
                sources.push_str(SOURCES);
            }
            Net {
                machine,
                io: Io {
                    clock: env.clock.clone(),
                    client: None,
                    to: Some(to),
                    answers,
                    lan,
                    heap: env.heap,
                    last: None,
                    kept: HashMap::new(),
                    disk: None,
                    sources,
                    cert_file: env.cert_file.map(std::ffi::OsString::from),
                    cert_dir: env.cert_dir.map(std::ffi::OsString::from),
                    tried: blaze_own::Vec::new_in(alloc),
                },
                steps: 0,
            }
        }

        /// Wants unit `at` (nothing done when it is wanted already).
        pub(super) fn want(&mut self, at: u32) {
            self.machine.want(at);
        }

        /// The places on this machine tried for unit `at` before the network, and what each gave
        /// (`local::Tried`'s words), when its `lookup` found none that held it.
        pub(super) fn tried(&self, at: u32) -> Option<&str> {
            self.io.tried.iter().find(|(unit, _)| *unit == at).map(|(_, words)| words.as_str())
        }

        /// Steps the machine until unit `at` has its outcome; `Err` with the words when HTTPS
        /// could not be set up and the unit needs a request (every such unit after the first).
        #[expect(clippy::too_many_arguments, reason = "the task's handles: each one the machine's IO needs")]
        pub(super) fn outcome(
            &mut self,
            cx: &Cx,
            root: &Dir<ReadWrite>,
            cargo: &mut CargoCache,
            env: &FetchEnv<'_>,
            units: &'f [Unit<'f, 'static>],
            index: &[(ContentId, u32)],
            at: u32,
        ) -> Result<Result<Outcome, Untrusted>, CliError> {
            self.machine.want(at);
            let mut scope = Scope { io: &mut self.io, cx, root, cargo, env, units, index, failed: None };
            loop {
                if !matches!(self.machine.outcome(at), Outcome::Pending) {
                    return Ok(Ok(self.machine.outcome(at).clone()));
                }
                if let Some(Err(untrusted)) = &scope.io.client {
                    return Ok(Err(untrusted.clone()));
                }
                if self.steps >= STEPS_MOST {
                    return Err(CliError::Text { what: "the fetch machine did not settle a unit within its steps (rule 18)" });
                }
                self.steps += 1;
                blaze_fetch::drive::step(&mut self.machine, &mut scope, STEP_MOST_NS);
                if let Some(error) = scope.failed.take() {
                    return Err(error);
                }
            }
        }
    }

    /// The parts of the IO that outlive a task.
    struct Io {
        clock: bstd::clock::Clock,
        /// Made at the first request, never when every unit's record stands.
        client: Option<Result<Handle, Untrusted>>,
        to: Option<blaze_core::chan::Sender<FetchEvent>>,
        answers: blaze_core::chan::Receiver<FetchEvent>,
        lan: LanPeerConfig,
        heap: &'static blaze_own::Heap,
        /// The answer the last wait gave: its unit and body.
        last: Option<(u32, std::vec::Vec<u8>)>,
        /// Answers kept for a reservation, by unit, until it is made.
        kept: HashMap<u32, std::vec::Vec<u8>>,
        /// The records' driver for reservations, made at the first.
        disk: Option<blaze_core::Native>,
        /// The sources store's folder as the machine names it (`<home>/.blaze/v2/sources`).
        sources: std::string::String,
        /// `SSL_CERT_FILE` and `SSL_CERT_DIR` as the command was given them (rule 75).
        cert_file: Option<std::ffi::OsString>,
        cert_dir: Option<std::ffi::OsString>,
        /// For each unit whose `lookup` found it on no place of this machine: the places tried
        /// and what each gave, in words, for its finding if the network does not give it either.
        tried: blaze_own::Vec<(u32, blaze_own::String<Alloc<'static>>), Alloc<'static>>,
    }

    impl Io {
        fn ensure_client(&mut self) -> Option<&Handle> {
            if self.client.is_none() {
                let to = self.to.take();
                let made = match to {
                    Some(events) => client(
                        blaze_http::roots::Places { cert_file: self.cert_file.as_deref(), cert_dir: self.cert_dir.as_deref() },
                        self.lan,
                        events,
                        self.heap,
                    ),
                    None => Err(Untrusted::Driver("fetch event sender already taken".into())),
                };
                self.client = Some(made);
            }
            self.client.as_ref().and_then(|res| res.as_ref().ok())
        }
    }

    /// The IO for one task's steps: what the machine's effects need of this task's context.
    struct Scope<'s, 'f> {
        io: &'s mut Io,
        cx: &'s Cx,
        /// The sources store (`sources_dir`), the first place `lookup` tries and where `put`
        /// writes.
        root: &'s Dir<ReadWrite>,
        /// Cargo's archive cache, the second place `lookup` tries.
        cargo: &'s mut CargoCache,
        env: &'s FetchEnv<'s>,
        units: &'f [Unit<'f, 'static>],
        index: &'s [(ContentId, u32)],
        /// The first error the store gave that is the tool's, not the unit's.
        failed: Option<CliError>,
    }

    impl FetchIo for Scope<'_, '_> {
        fn now(&self) -> u64 {
            self.io.clock.now().since_start().as_nanos() as u64
        }

        /// fetch.md's order of places, each checked against the unit's one key (D78): the sources
        /// store, then cargo's archive (adopted only by the lock's SHA-256), and on a miss of
        /// both the machine's next step, the registry over HTTPS. Not a fallback (rule 3): every
        /// place gives the same bytes at the same key or is refused, and what each gave is kept
        /// for the finding of a unit no place gives.
        fn lookup(&mut self, key: &ContentId) -> bool {
            // By the plan's sorted index: a scan would hash every unit's key at each lookup.
            let (units, index) = (self.units, self.index);
            let Some(at) = fetch_task::unit_of(index, Name::new(TaskKind::Fetch, *key)) else { return false };
            let Some(unit) = units.get(at as usize) else { return false };
            match held_record(self.cx, self.root, unit) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(error) => {
                    self.failed.get_or_insert(error);
                    return false;
                }
            }
            let mut tries = blaze_own::Vec::new_in(self.env.heap.alloc());
            tries.push(Try { place: STORE_PLACE, miss: Miss::Absent });
            match adopt_local(self.cx, self.root, &mut *self.cargo, self.env, unit, &mut tries) {
                Ok(true) => true,
                Ok(false) => {
                    let mut words = blaze_own::String::new_in(self.env.heap.alloc());
                    blaze_text::Writes::show(&mut words, &Tried { unit, tries: tries.as_slice() });
                    drop(tries);
                    self.io.tried.retain(|(unit, _)| *unit != at);
                    self.io.tried.push((at, words));
                    false
                }
                Err(error) => {
                    self.failed.get_or_insert(error);
                    false
                }
            }
        }

        fn put(&mut self, key: &ContentId, record: &[u8]) -> Result<(), StoreFailure> {
            let path = record_path(key).map_err(|_| StoreFailure::Io)?;
            match write_record(self.cx, self.root, &path, record) {
                Ok(_) => Ok(()),
                Err(error) if matches!(error.kind, FsKind::OverQuota(_)) => Err(StoreFailure::Full),
                Err(_) => Err(StoreFailure::Io),
            }
        }

        fn send(&mut self, ask: Ask, method: Method, url: &str, headers: &[(&str, &str)], body: &[u8], most: u64, deadline_ns: u64) {
            let within = Duration::from_nanos(deadline_ns.saturating_sub(self.now()));
            let Some(handle) = self.io.ensure_client() else { return };
            let request = Request {
                method: match method {
                    Method::Get => blaze_http::Method::Get,
                    Method::Post => blaze_http::Method::Post,
                },
                url: url.to_owned(),
                headers: headers.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect(),
                body: body.to_vec(),
                gzip: false,
                most,
                https_only: true,
                range: None,
            };
            handle.fetch_for_race(request, within, key_of(ask), false);
        }

        fn peer_probe(&mut self, nonce: u64, hashes: &[[u8; 32]]) {
            if !self.io.lan.enabled {
                return;
            }
            let lan = self.io.lan;
            let Some(handle) = self.io.ensure_client() else { return };
            let mut wire = [0u8; HAVE_MAX_BYTES];
            let Some(len) = encode_have(nonce, hashes, &mut wire) else { return };
            let Some(payload) = wire.get(..len) else { return };
            if let Some(target) = lan.unicast_target {
                handle.send_udp(&[target], payload);
            } else {
                let targets = [
                    SocketAddr::from((MULTICAST_GROUP, lan.udp_port)),
                    SocketAddr::from((Ipv4Addr::BROADCAST, lan.udp_port)),
                ];
                handle.send_udp(&targets, payload);
            }
        }

        fn send_peer(&mut self, ask: Ask, url: &str, most: u64, deadline_ns: u64) {
            if !self.io.lan.enabled {
                return;
            }
            let within = Duration::from_nanos(deadline_ns.saturating_sub(self.now()));
            let Some(handle) = self.io.ensure_client() else { return };
            let request = Request {
                method: blaze_http::Method::Get,
                url: url.into(),
                headers: Default::default(),
                body: Default::default(),
                gzip: false,
                most,
                https_only: false,
                range: None,
            };
            handle.fetch_for_race(request, within, key_of(ask), true);
        }

        fn cancel(&mut self, ask: Ask) {
            if let Some(Ok(handle)) = self.io.client.as_ref() {
                handle.cancel_fetch(key_of(ask));
            }
        }

        fn wait(&mut self, until_ns: Option<u64>) -> Woke<'_> {
            self.io.last = None;
            if matches!(self.io.client, Some(Err(_))) {
                return Woke::Other;
            }
            let wait = Duration::from_nanos(until_ns.map_or(STEP_MOST_NS, |at| at.saturating_sub(self.now())));
            match mailbox::answer("a dependency's HTTPS answer", &self.io.answers, wait) {
                Ok(Some(FetchEvent::Http { key, fetched })) => {
                    let ask = Ask { unit: (key >> 32) as u32, seq: key as u32 };
                    let reply = match fetched {
                        Fetched::Answered { status, body, .. } => {
                            let (_, body) = self.io.last.insert((ask.unit, body));
                            Reply::Body { status, bytes: body.as_slice() }
                        }
                        Fetched::Failed(error) => Reply::Failed(failure(&error.kind)),
                        Fetched::TimedOut => Reply::Failed(NetFailure::Timeout),
                    };
                    Woke::Answer { ask, reply }
                }
                Ok(Some(FetchEvent::Datagram { from, bytes })) => match decode_has(&bytes) {
                    Some(has) => Woke::PeerReply { peer: from, has },
                    None => Woke::Other,
                },
                // The channel's sender is held here or by the driver thread.
                Ok(None) => Woke::Other,
                Err(_due) => Woke::Due,
            }
        }

        fn keep(&mut self, unit: u32) {
            if let Some((of, body)) = self.io.last.take()
                && of == unit
            {
                self.io.kept.insert(unit, body);
            }
        }

        fn reserve(
            &mut self,
            unit: u32,
            key: &ContentId,
            room: u64,
            write: &mut dyn FnMut(&[u8], Result<&mut [u8], StoreFailure>) -> Option<u64>,
        ) -> Reserved {
            let body = self.io.kept.remove(&unit).unwrap_or_default();
            let made = match &mut self.io.disk {
                Some(disk) => Ok(disk),
                _ if self.io.sources.is_empty() => Err(()),
                none => {
                    let folder = std::path::Path::new(self.io.sources.as_str());
                    match std::fs::create_dir_all(folder).and_then(|()| blaze_core::native(folder, 2)) {
                        Ok(disk) => Ok(none.insert(disk)),
                        Err(_) => Err(()),
                    }
                }
            };
            let Ok(disk) = made else {
                _ = write(&body, Err(StoreFailure::Io));
                return Reserved::Abandoned;
            };
            let Some(name) = FileName::new(&hex(key)) else {
                _ = write(&body, Err(StoreFailure::Io));
                return Reserved::Abandoned;
            };
            let tag = Tag((u64::from(process::id()) << 32) | u64::from(unit));
            let (mut reserve, first) = Reserve::new(name, room, tag);
            let mut done = ask_disk(disk, first);
            loop {
                let Some(answer) = done else {
                    return Reserved::Unkept(StoreFailure::Io);
                };
                match reserve.answer(answer) {
                    Ok(Next::Write(mut mapping)) => match write(&body, Ok(mapping.bytes_mut())) {
                        Some(len) => match reserve.finish(mapping, len) {
                            Ok(effect) => done = ask_disk(disk, effect),
                            Err(_) => return Reserved::Unkept(StoreFailure::Io),
                        },
                        None => {
                            if let Some(effect) = reserve.abandon(Some(mapping)) {
                                _ = ask_disk(disk, effect);
                            }
                            return Reserved::Abandoned;
                        }
                    },
                    Ok(Next::Ask(effect)) => done = ask_disk(disk, effect),
                    Ok(Next::Stored) => return Reserved::Kept,
                    Ok(Next::Abandoned) => return Reserved::Abandoned,
                    Err(_) => {
                        let mapped = reserve.step() != Reserving::Mapping;
                        if let Some(effect) = reserve.abandon(None) {
                            _ = ask_disk(disk, effect);
                        }
                        return match mapped {
                            false => {
                                _ = write(&body, Err(StoreFailure::Io));
                                Reserved::Abandoned
                            }
                            true => Reserved::Unkept(StoreFailure::Io),
                        };
                    }
                }
            }
        }
    }

    /// The HTTPS + optional LAN peer client, made at the first request (never at start, and never
    /// when every unit's record stands): reads the machine's roots once and shares the single
    /// `blaze_http::driver::Handle` driver thread with the non-blocking UDP probe socket.
    fn client(
        places: blaze_http::roots::Places<'_>,
        lan: LanPeerConfig,
        events: blaze_core::chan::Sender<FetchEvent>,
        heap: &'static blaze_own::Heap,
    ) -> Result<Handle, Untrusted> {
        #[cfg(test)]
        if !super::NETWORK.get() {
            return Err(Untrusted::Driver(std::string::String::from("no network in this test (fetch.rs `NETWORK`)")));
        }
        fn tls_err(setup: impl core::fmt::Display) -> Untrusted {
            Untrusted::Tls(setup.to_string())
        }
        let tls = blaze_http::roots::system(places).map_err(tls_err)?;
        let wan = blaze_http::Client::new(blaze_http::Config::new(tls, USER_AGENT, Duration::from_secs(60)));
        // LAN peer client: empty root store (plain HTTP only), 0 redirects (SSRF protection),
        // no HTTP/SOCKS proxies, shorter stall timeout.
        let peer_tls = blaze_http::roots::with_roots(blaze_http::roots::empty_roots()).map_err(tls_err)?;
        let mut peer_cfg = blaze_http::Config::new(peer_tls, USER_AGENT, Duration::from_secs(15));
        peer_cfg.redirects = 0;
        peer_cfg.proxies = Proxies::none();
        let peer_client = blaze_http::Client::new(peer_cfg);
        let udp_bind = if lan.enabled {
            let bind_ip = match lan.unicast_target {
                Some(SocketAddr::V4(v4)) if v4.ip().is_loopback() => Ipv4Addr::LOCALHOST,
                _ => Ipv4Addr::UNSPECIFIED,
            };
            Some(SocketAddr::from((bind_ip, 0)))
        } else {
            None
        };
        Handle::spawn_with_peer(
            wan,
            Limits::REAL.in_flight_most as usize,
            PeerDriver { client: peer_client, udp_bind, events, heap },
        )
        .map_err(|error| Untrusted::Driver(error.to_string()))
    }

    fn key_of(ask: Ask) -> u64 {
        u64::from(ask.unit) << 32 | u64::from(ask.seq)
    }

    /// What a blaze-http error is to the machine: only no connection, a broken one and no
    /// answer in time are transient.
    fn failure(kind: &Kind) -> NetFailure {
        match kind {
            Kind::Io { step: Step::Connect | Step::Proxy, .. } => NetFailure::Connect,
            Kind::Io { step: Step::Tls, .. } => NetFailure::Refused,
            Kind::Io { .. } => NetFailure::Reset,
            Kind::NoAnswer { .. } | Kind::Stalled { .. } => NetFailure::Timeout,
            Kind::TooBig { said, .. } => NetFailure::TooBig { said: *said },
            _ => NetFailure::Refused,
        }
    }

    /// One effect of a reservation run to its completion on the records' driver; `None` when
    /// the driver stalled or gave none.
    fn ask_disk(disk: &mut blaze_core::Native, effect: blaze_core::Effect) -> Option<Done> {
        let mut effects = vec![effect];
        let mut got = None;
        blaze_core::run_until_idle(disk, &mut effects, |completion, _| got = Some(completion.done)).ok()?;
        got
    }

    fn hex(key: &ContentId) -> std::string::String {
        super::key_hex(key).iter().map(|digit| char::from(*digit)).collect()
    }
}

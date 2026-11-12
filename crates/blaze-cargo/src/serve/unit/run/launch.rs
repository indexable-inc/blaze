//! A program run by blaze's launcher (docs/v2/plan/design/launcher.md, steps 4 and 5): the
//! daemon's driver of `blaze_launch_host::Program`.
//!
//! A binary built for `edit` with [`VAR`] set is not linked. Its file at cargo's path is a
//! clone of blaze's launcher (`blaze-launcher`, beside blaze's own executable) and a sidecar
//! beside it names the program and this daemon's launch socket ([`link`]). Started, the
//! launcher connects here and asks for the program's code a function at a time; each later
//! build of the program is given to its [`Program`], which says what every running launcher
//! is told: an edit to apply, or to start again.
//!
//! What this module owns is the IO: the socket, a thread that accepts, and for each launcher
//! a thread that reads its messages and one that writes what it is told. The core decides
//! under one named lock a program; frames are queued to a launcher's writer under that lock,
//! so a launcher is written what the core decided, in the order it decided it.
//!
//! A launched program's unit is replayed from its record only where the core holds the code
//! of the build that record is of, and is built otherwise ([`not_replayed`]), so the core
//! holds the code of exactly the build that answered. A launcher started by hand against a daemon that holds none of it (a file from
//! before the daemon restarted, and no build since) stops and says so.
//!
//! Not built, each a refusal by name and never a quiet path: test binaries, which stay dev
//! executables; the eager background check (launcher.md, section 4): a build is whole before
//! its program can start.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use blaze_codegen::driver::Built;
use blaze_codegen::plan::Policy;
use blaze_core::lock::Lock;
use blaze_hash::SharedMap;
use blaze_ids::ContentId;
use blaze_launch::wire::{self, Ask, Tell};
use blaze_launch_host::{Conn, Program, Tables, Told};
use blaze_units::schema::Mode;
use crossbeam_channel::Sender;

use crate::serve::unit::{Job, codegen, graph::flag};

/// The variable that makes a binary built for `edit` a launcher's program (`1` or `on`), in
/// the daemon's own environment: the switch of launcher.md's step 4 until what the module's
/// documentation lists as not built is, when it becomes what `edit` means.
pub(crate) const VAR: &str = "BLAZE_LAUNCH";

/// Whether [`VAR`] is on for this daemon.
pub(crate) fn on() -> bool {
    matches!(std::env::var(VAR).as_deref(), Ok("1" | "on"))
}

/// Whether `unit`, built as `policy` says, is a launcher's program: a binary's or example's
/// program (not a test binary, not a build script) of this machine's own target, under a
/// profile that is not a release one, with [`VAR`] on.
pub(crate) fn launched(job: &Job, unit: usize, policy: &Policy) -> Result<bool, String> {
    if !on() {
        return Ok(false);
    }
    let u = job.unit(unit).map_err(|it| it.what)?;
    let target = &u.target;
    let bin = u.mode == Mode::Build
        && !target.is_custom_build()
        && !job.graph.has(unit as u32, flag::TEST)
        && target.kind.iter().chain(&target.crate_types).any(|it| it == "bin");
    if !bin || super::allows_live(&u.profile.name, policy).is_err() {
        return Ok(false);
    }
    Ok(codegen::triple(job, u) == job.input.config.host)
}

/// Whether `unit`, whose kept record names `outputs` (path, key, whether it is executable)
/// and whose whole record key is `key` now, is a launcher's program whose record must not be
/// replayed: the unit is then built, so that the program's core holds the code of exactly
/// this build before a launcher asks for it.
///
/// A record says which file a build made, and a launched program's file is the launcher's,
/// the same bytes for every build of it: a replay gives the core nothing. So a record is
/// replayed only where the core already holds the code of the build that record is of: the
/// program's last launch link was of a build with this same record key (a key covers every
/// input of its unit, rule keys-cover-every-input). A source edited and then put back has
/// the first build's key while the core holds the second's (found by the measurement's check
/// of its output, 2026-11-12); a daemon that restarted holds none. `false` with [`VAR`] off,
/// for a unit that makes no program, and for one built any other way.
pub(crate) fn not_replayed(
    job: &Job,
    unit: usize,
    outputs: &[(String, blaze_cas::Key, bool)],
    key: Option<blaze_cas::Key>,
) -> Result<bool, String> {
    if !on() || !outputs.iter().any(|it| it.2) {
        return Ok(false);
    }
    let policy = codegen::toolchain(job, unit).and_then(|it| codegen::policy(job, unit, it))?;
    if policy.mode != blaze_codegen::plan::Mode::Launched {
        return Ok(false);
    }
    let plan = job.plan(unit).map_err(|it| it.what)?;
    let made = super::super::artifact::made(plan);
    let programs = job.shared.launches.programs.pin();
    Ok(outputs.iter().any(|(path, _, executable)| {
        *executable
            && made.contains(&path.as_str())
            && !programs.get(&program_id(path)).is_some_and(|held| {
                let core = held.core.lock();
                key.is_some() && core.record == key
            })
    }))
}

/// One program this daemon built for a launcher, and the launchers running it.
struct Held {
    /// The path its link was asked for: what errors name it by.
    path: String,
    core: Lock<Core>,
}

struct Core {
    program: Program,
    /// Each launcher's writer, by connection.
    writers: Vec<(Conn, Sender<Vec<u8>>)>,
    /// The serial of the span tables the program was last given (`LinkInputs::serial`).
    spans: Option<u64>,
    /// The whole record key of the unit's build the program was last given: the build whose
    /// code the core holds ([`not_replayed`]).
    record: Option<blaze_cas::Key>,
}

/// The daemon's launcher programs, by id, and the socket their launchers connect to: made
/// once, for the daemon's life ([`super::super::Shared`]).
pub(crate) struct Launches {
    /// Where the socket is: the daemon's state directory's `launch`.
    dir: PathBuf,
    /// Whether the accept thread runs.
    listening: Lock<bool>,
    programs: Arc<SharedMap<[u8; 32], Arc<Held>>>,
    /// The last connection's number.
    conns: Arc<std::sync::atomic::AtomicU64>,
    /// blaze's launcher, read once a daemon: its bytes and the store's key of them.
    launcher: Lock<Option<(Arc<Vec<u8>>, blaze_cas::Key)>>,
}

/// The id of the program whose link is asked for at `path`: what its sidecar names.
fn program_id(path: &str) -> [u8; 32] {
    ContentId::of("blaze launch program 1", path.as_bytes()).0
}

impl Launches {
    pub(crate) fn new(dir: PathBuf) -> Launches {
        Launches {
            dir,
            listening: Lock::new("serve.launches.listening", false),
            programs: Arc::new(SharedMap::default()),
            conns: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            launcher: Lock::new("serve.launches.launcher", None),
        }
    }

    /// blaze's launcher (`blaze-launcher`, beside blaze's own executable `blaze`): its bytes
    /// and their key, read at the first program's link.
    fn launcher(&self, blaze: &std::path::Path, path: &str) -> Result<(Arc<Vec<u8>>, blaze_cas::Key), String> {
        // clone: two counts, once a link
        if let Some(held) = self.launcher.lock().clone() {
            return Ok(held);
        }
        let launcher = blaze
            .parent()
            .map(|it| it.join("blaze-launcher"))
            .ok_or_else(|| format!("{} has no directory to find blaze-launcher in", blaze.display()))?;
        // Read outside the lock: two links that race read it twice and keep one.
        let file = std::fs::read(&launcher).map_err(|why| {
            format!(
                "{path} is built to run under blaze's launcher ({VAR} is on), and the launcher {} does not read: {why}",
                launcher.display()
            )
        })?;
        let key = crate::serve::unit::compile::output_key(&file);
        let held = (Arc::new(file), key);
        *self.launcher.lock() = Some(held.clone()); // clone: two counts
        Ok(held)
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("sock")
    }

    /// The socket, listened on from the first program's link.
    fn listen(&self) -> Result<PathBuf, String> {
        let path = self.socket();
        let mut listening = self.listening.lock();
        if *listening {
            return Ok(path);
        }
        let what = |it: std::io::Error| format!("listening for launchers at {}: {it}", path.display());
        std::fs::create_dir_all(&self.dir).map_err(what)?;
        // A socket file a daemon before this one left: nothing listens on it.
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
            Err(why) => return Err(what(why)),
        }
        let listener = UnixListener::bind(&path).map_err(what)?;
        let (programs, conns) = (Arc::clone(&self.programs), Arc::clone(&self.conns));
        std::thread::Builder::new()
            .name("blaze launch accept".to_owned())
            .spawn(move || accept(&listener, &programs, &conns))
            .map_err(what)?;
        *listening = true;
        Ok(path)
    }

    /// A build of the program linked at `path`: given to its core, and each running launcher
    /// written what the core says. Gives what each was told.
    fn built(
        &self,
        path: &str,
        built: &Built,
        entry: &str,
        record: Option<blaze_cas::Key>,
        tables: impl FnOnce(Option<u64>) -> Result<(u64, Given), String>,
    ) -> Result<Vec<Told>, String> {
        let id = program_id(path);
        // clone: a count, once per build
        let held = self.programs.pin().get(&id).cloned();
        let held = match held {
            Some(held) => held,
            None => {
                let made = Arc::new(Held {
                    path: path.to_owned(), // alloc: once per program
                    core: Lock::new(
                        "serve.launches.program",
                        // alloc: once per program, its tables
                        Core { program: Program::new(), writers: Vec::new(), spans: None, record: None },
                    ),
                });
                self.programs.pin().insert(id, Arc::clone(&made));
                made
            }
        };
        // The lock is held for the core's decision alone: no IO, no query; what it decides
        // is queued to each launcher's writer under it, so the order of frames is the order
        // of decisions.
        let mut core = held.core.lock();
        // Until this build is the core's, no record is the core's: a failure below leaves
        // the next build to be made, never replayed.
        core.record = None;
        let (serial, given) = tables(core.spans)?;
        let _span = tracing::info_span!(target: "blaze", "launch link: the program takes the build").entered();
        let told = match &given {
            Given::Whole(all) => core.program.built(built, entry, Tables::Whole(all)),
            Given::Changed { tables, removed } => {
                // alloc: the changed tables borrowed, once a build
                let tables: Vec<(ContentId, &[u8])> = tables.iter().map(|it| (it.0, &it.1[..])).collect();
                core.program.built(built, entry, Tables::Changed { tables: &tables, removed })
            }
        }
        .map_err(|why| format!("{}: {why}", held.path))?;
        core.spans = Some(serial);
        core.record = record;
        let mut what = Vec::with_capacity(told.len()); // alloc: once a build
        for (conn, frames, said) in told {
            if let Some((_, writer)) = core.writers.iter().find(|it| it.0 == conn)
                && !frames.is_empty()
            {
                // A writer that is gone is a launcher that ended: its reader takes it out.
                drop(writer.send(frames));
            }
            what.push(said);
        }
        Ok(what)
    }
}

/// The span tables a build gives its program.
pub(crate) enum Given {
    Whole(Vec<(ContentId, Vec<u8>)>),
    Changed { tables: Vec<(ContentId, Vec<u8>)>, removed: Vec<ContentId> },
}

/// The accept thread: a reader thread for each launcher that connects.
fn accept(
    listener: &UnixListener,
    programs: &Arc<SharedMap<[u8; 32], Arc<Held>>>,
    conns: &Arc<std::sync::atomic::AtomicU64>,
) {
    loop {
        // The wait: for a launcher to connect, for the daemon's life.
        let _span = tracing::info_span!(target: "blaze", "launch accept").entered();
        let socket = match listener.accept() {
            Ok((socket, _)) => socket,
            Err(why) if why.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(why) => {
                tracing::error!(target: "blaze", "accepting a launcher failed: {why}");
                return;
            }
        };
        let conn = Conn(conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1);
        let programs = Arc::clone(programs);
        let started = std::thread::Builder::new()
            .name("blaze launch read".to_owned())
            .spawn(move || serve(conn, socket, &programs));
        if let Err(why) = started {
            tracing::error!(target: "blaze", "a launcher's reader thread did not start: {why}");
        }
    }
}

/// One whole frame's item from `from`, or `None` at its end.
fn frame(from: &mut UnixStream, pending: &mut Vec<u8>) -> Result<Option<Vec<u8>>, String> {
    loop {
        match wire::split(pending).map_err(|why| format!("a launcher's frame: {why}"))? {
            Some((item, took)) => {
                let item = item.to_vec(); // alloc: one message of a launcher's
                drop(pending.drain(..took));
                return Ok(Some(item));
            }
            None => {
                let mut chunk = [0u8; 4096];
                // The wait: for the launcher's next bytes; it ends when the launcher writes
                // or its process ends.
                match from.read(&mut chunk) {
                    Ok(0) => return Ok(None),
                    Ok(got) => pending.extend_from_slice(&chunk[..got]),
                    Err(why) if why.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(why) => return Err(format!("reading a launcher's socket: {why}")),
                }
            }
        }
    }
}

/// A launcher's reader: its `Hello` names the program; from then on each of its messages is
/// the core's, and what the core says goes to its writer.
fn serve(conn: Conn, mut socket: UnixStream, programs: &SharedMap<[u8; 32], Arc<Held>>) {
    let _span = tracing::info_span!(target: "blaze", "launch serve", conn = conn.0).entered();
    let mut pending = Vec::new(); // alloc: the launcher's bytes not yet a whole frame
    let hello = match frame(&mut socket, &mut pending) {
        Ok(Some(item)) => item,
        Ok(None) => return,
        Err(why) => return tracing::error!(target: "blaze", "launcher {}: {why}", conn.0),
    };
    let program = match Ask::decode(&hello) {
        Ok((_, Ask::Hello { program, .. })) => program,
        Ok((_, other)) => {
            return tracing::error!(target: "blaze", "launcher {} began with {other:?}, not Hello", conn.0);
        }
        Err(why) => return tracing::error!(target: "blaze", "launcher {}'s first message: {why}", conn.0),
    };
    // clone: a count, once per launcher
    let held = programs.pin().get(&program.0).cloned();
    let Some(held) = held else {
        // This daemon built no such program since it started: the launcher is told, and
        // stops naming it.
        let why = "this daemon holds no code of the program (it was built by a daemon that has since \
                   stopped, and its build was kept): change a source file of it and build it again";
        let mut out = Vec::new(); // alloc: one frame
        let unheld = Tell::Unheld { why };
        if unheld.encode(1, &mut out).is_ok() {
            // The launcher may be gone already; there is then no one to tell.
            drop(socket.write_all(&out));
        }
        return;
    };
    let writer = match socket.try_clone() {
        Ok(writer) => writer,
        Err(why) => return tracing::error!(target: "blaze", "launcher {}'s socket: {why}", conn.0),
    };
    let (send, queued) = crossbeam_channel::unbounded::<Vec<u8>>();
    let written = std::thread::Builder::new().name("blaze launch write".to_owned()).spawn(move || {
        let mut writer = writer;
        // The wait: for the core's next frames for this launcher; it ends when the reader
        // drops the queue's sender, as the launcher's socket ends.
        while let Ok(frames) = queued.recv() {
            if writer.write_all(&frames).is_err() {
                return;
            }
        }
    });
    let written = match written {
        Ok(written) => written,
        Err(why) => {
            return tracing::error!(target: "blaze", "launcher {}'s writer thread did not start: {why}", conn.0);
        }
    };
    {
        let mut core = held.core.lock();
        core.program.connected(conn);
        core.writers.push((conn, send.clone())); // clone: the queue's sender, once a launcher
    }
    let mut item = Some(hello);
    let ended = loop {
        let Some(message) = item.take() else { break Ok(()) };
        let said = {
            let mut core = held.core.lock();
            let mut out = Vec::new(); // alloc: the frames of one answer
            let said = core.program.said(conn, &message, &mut out).map(|()| {
                if !out.is_empty() {
                    // Queued under the lock: in the order the core decided.
                    drop(send.send(out));
                }
            });
            // A start that could not be made one message is the same start, slower: said
            // where what was slow is read.
            if let Some(why) = core.program.unimaged() {
                tracing::warn!(
                    target: "blaze",
                    "{}: its launchers are sent its start as records, not as an image of placed code: {why}",
                    held.path
                );
            }
            said
        };
        if let Err(why) = said {
            break Err(why);
        }
        match frame(&mut socket, &mut pending) {
            Ok(next) => item = next,
            Err(why) => break Err(why),
        }
    };
    if let Err(why) = ended {
        tracing::error!(target: "blaze", "launcher {} of {}: {why}", conn.0, held.path);
        // The launcher is told why it is served no more, so the program stops naming it.
        let mut out = Vec::new(); // alloc: one frame
        let unheld = Tell::Unheld { why: &why };
        if unheld.encode(u64::MAX, &mut out).is_ok() {
            // Queued after everything the core decided for it: written last.
            drop(send.send(out));
        }
    }
    {
        let mut core = held.core.lock();
        core.program.disconnected(conn);
        core.writers.retain(|it| it.0 != conn);
    }
    // Both senders are gone now, so the writer ends once it wrote what was queued. The wait:
    // for that; it ends when the queue is drained or the launcher's socket refuses a write
    // (a launcher that is alive and reads nothing holds this one thread, and nothing else).
    drop(send);
    drop(written.join());
    // The launcher's process is told nothing more: closing its socket ends its code service,
    // which it says by name if a stub still fires.
    drop(socket.shutdown(std::net::Shutdown::Both));
}

/// What a launcher program's "link" makes: the launcher's own bytes for the file at the
/// program's path, the sidecar's for each path the program runs from, and what the running
/// launchers were told of this build.
pub(crate) struct Linked {
    /// The launcher binary, the file at cargo's path, and the store's key of it.
    pub(crate) file: Arc<Vec<u8>>,
    pub(crate) key: blaze_cas::Key,
    /// The sidecar's text, the same beside every path.
    pub(crate) sidecar: Vec<u8>,
    pub(crate) told: Vec<Told>,
    /// Whether this is the program's first build in this daemon.
    pub(crate) first: bool,
}

/// The sidecar's path for the program file at `path`.
pub(crate) fn sidecar_of(path: &str) -> String {
    format!("{path}.blaze-program") // alloc: once per path a link
}

/// "Links" the launcher program at `path`: nothing is laid out. The build is given to the
/// program's core, the running launchers are told, and the file's and the sidecar's bytes
/// are handed back for the loop to place.
pub(crate) fn link(
    job: &Job,
    unit: usize,
    path: &str,
    generated: &codegen::Generated<'_>,
) -> Result<Linked, String> {
    let _span = tracing::info_span!(target: "blaze", "launch link", unit = %path).entered();
    let launches = &job.shared.launches;
    let socket = launches.listen()?;
    let (file, key) = launches.launcher(&job.shared.launcher, path)?;
    let first = launches.programs.pin().get(&program_id(path)).is_none();
    let record = tracing::info_span!(target: "blaze", "launch link: the unit's record key")
        .in_scope(|| super::super::compile::record_key(job, unit))
        .map_err(|it| it.what)?;
    let told = launches.built(path, generated.built, "main", record, |held| {
        // What `LinkInputs` says, against the snapshot the program holds: the same snapshot
        // (nothing changed), a change of it, every table made anew (`work.whole`), or a
        // snapshot the program does not hold, when every table is asked for.
        let side = tracing::info_span!(target: "blaze", "launch link: the build's span tables")
            .in_scope(|| generated.side())?;
        // clone (each below): tables into the program's own copy
        let changed = || Given::Changed {
            tables: side.tables.iter().map(|it| (it.anchor, it.bytes.clone())).collect(),
            removed: side.removed.clone(),
        };
        match (side.base, held) {
            (None, Some(held)) if held == side.serial => {
                // alloc: none, both empty
                Ok((side.serial, Given::Changed { tables: Vec::new(), removed: Vec::new() }))
            }
            (Some(base), Some(held)) if base == held => Ok((side.serial, changed())),
            (None, _) if side.work.whole.is_some() => Ok((
                side.serial,
                Given::Whole(side.tables.iter().map(|it| (it.anchor, it.bytes.clone())).collect()),
            )),
            _ => Ok((side.serial, Given::Whole(generated.spans()?.to_vec()))),
        }
    })?;
    use std::fmt::Write as _;
    let mut sidecar = String::new(); // alloc: once a link
    // A `String` takes every write.
    let _ = writeln!(sidecar, "socket={}", socket.display());
    let _ = write!(sidecar, "program=");
    for byte in program_id(path) {
        let _ = write!(sidecar, "{byte:02x}");
    }
    sidecar.push('\n');
    Ok(Linked { file, key, sidecar: sidecar.into_bytes(), told, first })
}

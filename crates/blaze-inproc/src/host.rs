//! The daemon's side of a run: the executable written to cargo's path, the command that
//! spawns it through `blaze-core`'s `Effect::Spawn`, and the handshake that lays the code
//! memory over it before `main` (the crate docs' "The start of a run").
//!
//! A [`Host`] listens on a socket of its own (a named pipe on Windows) in a directory only
//! its user can reach. [`Host::launch`] makes the command, with the socket's address and a
//! random token in `BLAZE_INPROC`, and a [`Pending`] that becomes the run's [`Session`] once
//! the start hook connected from the process the OS says it is, said the token, was given the
//! memory and laid it over its image. Each connection's handshake runs on a thread of the
//! host: no plane worker blocks on a run.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use blaze_core::{Command, Sandbox};
use parking_lot::Mutex;

use crate::channel::Channel;
use crate::frame::{Reader, Writer, kinds};
use crate::handshake::{Grant, HOST_VAR, Hello, Lookup, Overlay, Place, VERSION, decode_map, hex};
use crate::sys::{self, Rights};
use crate::{Access, CodeMemory, CodeRange, Error, FixedMap, ImageLookup, ImageWrite, ask};

/// How long a start hook may take between its frames during the handshake.
const HANDSHAKE: Duration = Duration::from_secs(30);

/// The executable a run starts: `len` bytes of the image part of its code memory, the file the
/// linker wrote, to go at cargo's `path`. `key` names those bytes (the store's content hash of
/// the link's output); `store`, when given, is the store's file of exactly them, verified by
/// the store against `key`, which a filesystem with clones shares blocks with.
pub struct Executable<'a> {
    pub path: &'a Path,
    pub key: [u8; 32],
    pub len: u64,
    pub store: Option<&'a Path>,
}

/// What [`Host::write_executable`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrote {
    /// The file at the path is the one written for this key, unchanged since: nothing.
    Kept,
    /// A clone of the store's file: no bytes copied.
    Cloned,
    /// The bytes, once.
    Written,
}

/// What a written file was when it was written: a file changed since (a rebuild by another
/// tool, `cargo clean`) is written again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Identity {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Identity {
    fn of(path: &Path) -> Option<Identity> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        Some(Identity {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        })
    }
}

/// A run to start: `program`, the executable written from `image`'s code memory
/// ([`Host::write_executable`]), with the `maps` laid over its reservations too.
pub struct Launch<'a> {
    pub image: &'a CodeMemory,
    /// Each a dependency image's (`blaze-premap`'s): the run must be unslid while there is one.
    pub maps: Vec<FixedMap<'a>>,
    /// The words of the `maps`' private copies the start hook stores once they are mapped and
    /// before the program runs, and those it binds to a name in the run's global scope:
    /// `blaze_premap::start::Start`'s writes and lookups. Each 8 bytes inside an
    /// [`Access::Copy`] map, else the launch is an error.
    pub writes: Vec<ImageWrite>,
    pub lookups: Vec<ImageLookup<'a>>,
    pub program: PathBuf,
    /// The program's arguments after `argv[0]`, which is `program`.
    pub args: Vec<OsString>,
    /// The program's whole environment.
    pub env: Vec<(OsString, OsString)>,
    pub dir: PathBuf,
    pub stdin: Vec<u8>,
    pub sandbox: Sandbox,
    pub memory: Option<u64>,
}

/// What waits for a start hook to connect, by token.
struct Waiting {
    objects: Vec<(sys::Object, Rights)>,
    /// The daemon's view of the asks object, `objects[2]`.
    asks: sys::View,
    grant: Grant,
    encoded: Vec<u8>,
    done: crossbeam_channel::Sender<Result<Session, Error>>,
}

/// The launches whose start hook has not connected yet. Locked only to add, take or drop one
/// entry; nothing is called with it held.
type Table = Arc<Mutex<HashMap<[u8; 16], Waiting>>>;

pub struct Host {
    address: String,
    waiting: Table,
    /// The executables written, by path: the key and the file as written.
    written: Mutex<HashMap<PathBuf, ([u8; 32], Identity)>>,
    temporaries: AtomicU64,
}

impl Host {
    /// A host listening in `dir` (made if missing, reachable by this user only).
    pub fn new(dir: &Path) -> Result<Host, Error> {
        let (listener, address) = sys::Listener::bind(dir)?;
        let waiting: Table = Arc::default();
        let table = Arc::clone(&waiting);
        std::thread::Builder::new()
            .name("blaze-inproc host".to_owned())
            .spawn(move || accept(&listener, &table))
            .map_err(|it| Error::new(format!("starting the host's thread: {it}")))?;
        Ok(Host { address, waiting, written: Mutex::default(), temporaries: AtomicU64::new(0) })
    }

    /// Puts `executable` at its path from `memory`: nothing when this host wrote the same key
    /// there and the file is unchanged since, else a clone of the store's file or the bytes
    /// of the image part, into a new file renamed over the path (a process still running the
    /// old one keeps it). The bytes are the image part's only while it is pristine
    /// ([`CodeMemory::is_pristine`]): a memory patched since is an error, never a file that
    /// differs from what the overlay swaps in.
    pub fn write_executable(&self, memory: &CodeMemory, executable: &Executable<'_>) -> Result<Wrote, Error> {
        let path = executable.path;
        let mut written = self.written.lock();
        if let Some((key, identity)) = written.get(path)
            && *key == executable.key
            && Identity::of(path) == Some(*identity)
        {
            return Ok(Wrote::Kept);
        }
        if !memory.is_pristine() {
            return Err(Error::new(format!(
                "writing {}: the code memory's image part changed since it was laid out, so it is no longer the file the \
                 linker wrote",
                path.display()
            )));
        }
        if executable.len == 0 || executable.len > memory.image_len() {
            return Err(Error::new(format!(
                "writing {}: an executable of {:#x} bytes from an image part of {:#x}",
                path.display(),
                executable.len,
                memory.image_len()
            )));
        }
        let bytes = memory.bytes(CodeRange { offset: 0, len: executable.len })?;
        let io = |what: &str, at: &Path, it: std::io::Error| Error::new(format!("{what} {}: {it}", at.display()));
        let name = path.file_name().ok_or_else(|| Error::new(format!("an executable path {} with no file name", path.display())))?;
        let mut temporary = name.to_owned();
        temporary.push(format!(
            ".blaze-{}-{}.tmp",
            std::process::id(),
            self.temporaries.fetch_add(1, Ordering::Relaxed)
        ));
        let temporary = path.with_file_name(temporary);
        let wrote = match executable.store {
            Some(store) if sys::clone_file(store, &temporary)? => {
                let cloned = std::fs::read(&temporary).map_err(|it| io("reading the clone", &temporary, it))?;
                if cloned != bytes {
                    let _ = std::fs::remove_file(&temporary);
                    return Err(Error::new(format!(
                        "the store's file {} for {} is not the image the code memory holds",
                        store.display(),
                        path.display()
                    )));
                }
                Wrote::Cloned
            }
            _ => {
                std::fs::write(&temporary, bytes).map_err(|it| io("writing", &temporary, it))?;
                Wrote::Written
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))
                .map_err(|it| io("making executable", &temporary, it))?;
        }
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io("renaming the new executable over", path, error));
        }
        let identity = Identity::of(path).ok_or_else(|| Error::new(format!("{} is gone right after it was written", path.display())))?;
        written.insert(path.to_owned(), (executable.key, identity));
        Ok(wrote)
    }

    /// The command that starts `launch`, for an `Effect::Spawn`, and what becomes its
    /// session.
    pub fn launch(&self, launch: &Launch<'_>) -> Result<(Command, Pending), Error> {
        let (objects, grant) = grant(launch)?;
        let asks = objects[2].0.map_here(Access::Shared)?;
        ask::prepare(&asks);
        let mut encoded = Vec::new();
        grant.encode(&mut encoded);

        let mut token = [0u8; 16];
        sys::random(&mut token)?;
        let (done, wait) = crossbeam_channel::unbounded();
        self.table().insert(token, Waiting { objects, asks, grant, encoded, done });

        let mut var = OsString::from(&self.address);
        var.push("\t");
        var.push(hex(&token));
        let mut env: Vec<(OsString, OsString)> = launch.env.iter().filter(|(name, _)| name != HOST_VAR).cloned().collect();
        env.push((HOST_VAR.into(), var));
        let mut sandbox = launch.sandbox.clone();
        if let Sandbox::On { read, write, services, .. } = &mut sandbox {
            read.push(launch.program.clone());
            services.extend(sys::service());
            // Connecting to a Unix socket is a write of the socket alone: the program gets
            // neither the directory nor the other runs' sockets in it. A named pipe is not a
            // path the sandbox's file rules cover.
            if cfg!(unix) {
                write.push(PathBuf::from(&self.address));
            }
        }
        let command = Command {
            program: launch.program.clone(),
            args: launch.args.clone(),
            env,
            dir: launch.dir.clone(),
            stdin: launch.stdin.clone(),
            sandbox,
            memory: launch.memory,
        };
        Ok((command, Pending { token, wait, waiting: Arc::clone(&self.waiting) }))
    }

    fn table(&self) -> parking_lot::MutexGuard<'_, HashMap<[u8; 16], Waiting>> {
        self.waiting.lock()
    }
}

/// The objects a run of `launch` is given and its grant: every code segment from the image
/// part, the arena, the cells and the fixed maps in their reservations; the control words and
/// a new asks object, mapped by the start hook itself.
fn grant(launch: &Launch<'_>) -> Result<(Vec<(sys::Object, Rights)>, Grant), Error> {
    let memory = launch.image;
    let layout = memory
        .layout()
        .ok_or_else(|| Error::new("a launch of a code memory not laid out (CodeMemory::lay_out)"))?;
    let asks = sys::Object::create("the asks", ask::BYTES.next_multiple_of(sys::page()))?;
    let mut objects = vec![
        (memory.object().duplicate()?, Rights::Code),
        (memory.control().duplicate()?, Rights::Words),
        (asks, Rights::Words),
    ];
    let mut overlays: Vec<Overlay> = layout
        .code
        .iter()
        .map(|it| Overlay {
            object: 0,
            offset: it.offset,
            len: it.len,
            at: it.linked,
            access: Access::Execute,
            place: Place::Code,
            image: false,
        })
        .collect();
    if let Some((range, at)) = memory.arena_part()
        && range.len != 0
    {
        overlays.push(Overlay {
            object: 0,
            offset: range.offset,
            len: range.len,
            at,
            access: Access::Execute,
            place: Place::Reserved,
            image: false,
        });
    }
    if let (Some(object), Some((range, at))) = (memory.cells_object(), memory.cells_part()) {
        objects.push((object.duplicate()?, Rights::Words));
        overlays.push(Overlay {
            object: (objects.len() - 1) as u32,
            offset: 0,
            len: range.len,
            at,
            access: Access::Shared,
            place: Place::Reserved,
            image: false,
        });
    }
    let mut seen: Vec<(*const CodeMemory, u32)> = Vec::new();
    for map in &launch.maps {
        let page = map.memory.page();
        if !(map.at | map.range.offset | map.range.len).is_multiple_of(page) || map.range.len == 0 {
            return Err(Error::new(format!(
                "a fixed map of {:#x} bytes at {:#x} from offset {:#x}: not whole pages of {page:#x}",
                map.range.len, map.at, map.range.offset
            )));
        }
        if map.access == Access::Shared {
            return Err(Error::new(format!("a fixed map at {:#x} shared writable", map.at)));
        }
        if map.range.end() > map.memory.cells().offset {
            return Err(Error::new(format!(
                "a fixed map of {:#x}..{:#x}: past its memory's image and arena",
                map.range.offset,
                map.range.end()
            )));
        }
        let memory = map.memory as *const CodeMemory;
        let object = match seen.iter().find(|(it, _)| *it == memory) {
            Some(&(_, object)) => object,
            None => {
                objects.push((map.memory.object().duplicate()?, Rights::Code));
                let object = (objects.len() - 1) as u32;
                seen.push((memory, object));
                object
            }
        };
        overlays.push(Overlay {
            object,
            offset: map.range.offset,
            len: map.range.len,
            at: map.at,
            access: map.access,
            place: Place::Reserved,
            image: true,
        });
    }
    overlays.sort_by_key(|it| it.at);
    for pair in overlays.windows(2) {
        if pair[0].range().end > pair[1].at {
            return Err(Error::new(format!(
                "overlays at {:#x} and {:#x} overlap: a fixed map over the arena, the cells or another map",
                pair[0].at, pair[1].at
            )));
        }
    }
    for overlay in &overlays {
        let reserved = layout.reserved.iter().any(|it| it.start <= overlay.at && overlay.range().end <= it.end);
        if overlay.place == Place::Reserved && !reserved {
            return Err(Error::new(format!(
                "an overlay of {:#x} bytes at {:#x}: not inside a reservation of the executable",
                overlay.len, overlay.at
            )));
        }
    }
    let mut names = String::with_capacity(launch.lookups.iter().map(|it| it.name.len()).sum());
    let mut lookups = Vec::with_capacity(launch.lookups.len());
    for it in &launch.lookups {
        let start = u32::try_from(names.len())
            .ok()
            .filter(|start| u32::try_from(it.name.len()).is_ok_and(|len| start.checked_add(len).is_some()))
            .ok_or_else(|| Error::new(format!("the names of a launch's lookups run past 4 GiB at `{}`", it.name)))?; // alloc: error path
        names.push_str(it.name);
        lookups.push(Lookup { place: it.place, addend: it.addend, weak: it.weak, name: start, len: it.name.len() as u32 });
    }
    let grant = Grant {
        format: layout.format,
        header: layout.header,
        span: layout.span.clone(),
        objects: objects.iter().map(|(object, rights)| (object.len(), *rights)).collect(),
        overlays,
        writes: launch.writes.clone(),
        lookups,
        names,
    };
    grant.check_binds()?;
    Ok((objects, grant))
}

/// A launch whose start hook has not finished its handshake.
pub struct Pending {
    token: [u8; 16],
    wait: crossbeam_channel::Receiver<Result<Session, Error>>,
    waiting: Table,
}

impl Pending {
    /// Waits until the start hook laid the memory over the image, or `timeout`. Blocking: call
    /// it on a thread of the run's, not a plane worker.
    pub fn wait(self, timeout: Duration) -> Result<Session, Error> {
        let got = self.wait.recv_timeout(timeout);
        self.waiting.lock().remove(&self.token);
        match got {
            Ok(session) => session,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                Err(Error::new(format!("the start hook did not attach within {timeout:?}")))
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(Error::new("the host stopped before the start hook attached")),
        }
    }
}

/// A started run: its process, its slide, and what reads and answers its asks.
pub struct Session {
    pub pid: u32,
    /// What the process adds to a linked address ([`CodeMemory::linked`]).
    pub slide: u64,
    /// The thread that runs `main`.
    pub thread: u64,
    channel: Channel,
    /// Holds what acts in the process (Windows maps from here) while the run lives.
    run: Arc<ask::Run>,
}

impl Session {
    /// Where its asks are read, on one thread, and what answers them, from any.
    pub fn split(self) -> (ask::Reader, ask::Sender) {
        (ask::Reader::new(self.channel, Arc::clone(&self.run)), ask::Sender::new(self.run))
    }

    /// Ends the process, `why` its run's error ([`Session::killed`]): for a run whose asks
    /// can't be answered. Before it is reaped: on macOS a process ID is all that names it.
    pub fn kill(&self, why: &str) -> Result<(), Error> {
        self.run.kill(why)
    }

    /// Why [`Session::kill`] or [`ask::Sender::kill`] ended the process, the first time.
    pub fn killed(&self) -> Option<String> {
        self.run.killed()
    }
}

/// How long a started process's start hook may take to connect and attach.
const START: Duration = Duration::from_secs(60);

/// A program placed in a code memory, which any number of processes run: what the daemon's
/// dev `run` and test paths hold for an executable. Each [`Program::command`] writes the
/// executable to the command's path when it is not there yet and is a launch of it; a thread
/// of its own waits for the start hook, attaches the process to the memory and hands the
/// session on (to whoever answers the process's asks: `blaze-lazy`'s runner).
pub struct Program {
    host: Arc<Host>,
    memory: Arc<Mutex<CodeMemory>>,
    image: Image,
    sessions: crossbeam_channel::Sender<Result<Session, Error>>,
    starts: Arc<Mutex<Starts>>,
}

/// The executable of a [`Program`], as [`Executable`] names it, for any path.
#[derive(Clone, Debug)]
pub struct Image {
    pub key: [u8; 32],
    pub len: u64,
    pub store: Option<PathBuf>,
}

/// One [`Program::command`]'s process, for [`Program::exited`]: the spawn's completion names
/// no process ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Started(u64);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Start {
    Waiting,
    Attached(u32),
    /// Ended before its start hook finished the handshake.
    Ended,
}

#[derive(Default)]
struct Starts {
    next: u64,
    starts: HashMap<u64, Start>,
}

impl Program {
    /// `memory` laid out ([`CodeMemory::lay_out`]), its image part the executable `image`
    /// names.
    pub fn new(host: Arc<Host>, memory: Arc<Mutex<CodeMemory>>, image: Image, sessions: crossbeam_channel::Sender<Result<Session, Error>>) -> Program {
        Program { host, memory, image, sessions, starts: Arc::default() }
    }

    pub fn memory(&self) -> &Arc<Mutex<CodeMemory>> {
        &self.memory
    }

    /// The command that runs what `command` would have, its program the executable at its
    /// path: arguments, environment, directory, stdin, sandbox and memory limit stay. `forks`
    /// says the process forks children that run the program (a fork server): see
    /// [`CodeMemory::attach_forking`].
    pub fn command(&self, command: Command, forks: bool) -> Result<(Command, Started), Error> {
        self.command_with(command, forks, Vec::new(), Vec::new(), Vec::new()) // alloc: none, empty
    }

    /// As [`Program::command`], with dependency images mapped into the run: `maps` laid over
    /// the executable's reservation for fixed maps (`blaze_premap::map::Memory::maps`), and
    /// their open words written and bound before the program runs (`blaze_premap::start::plan`'s
    /// writes and lookups, [`Launch::writes`]). The maps' memory is only borrowed for the launch:
    /// the grant holds references of its own.
    pub fn command_with(
        &self,
        command: Command,
        forks: bool,
        maps: Vec<FixedMap<'_>>,
        writes: Vec<ImageWrite>,
        lookups: Vec<ImageLookup<'_>>,
    ) -> Result<(Command, Started), Error> {
        let (command, pending) = {
            let memory = lock(&self.memory);
            let executable = Executable {
                path: &command.program,
                key: self.image.key,
                len: self.image.len,
                store: self.image.store.as_deref(),
            };
            self.host.write_executable(&memory, &executable)?;
            let launch = Launch {
                image: &memory,
                maps,
                writes,
                lookups,
                program: command.program,
                args: command.args,
                env: command.env,
                dir: command.dir,
                stdin: command.stdin,
                sandbox: command.sandbox,
                memory: command.memory,
            };
            self.host.launch(&launch)?
        };
        let started = {
            let mut starts = lock_starts(&self.starts);
            starts.next += 1;
            let id = starts.next;
            starts.starts.insert(id, Start::Waiting);
            Started(id)
        };
        let memory = Arc::clone(&self.memory);
        let starts = Arc::clone(&self.starts);
        let sessions = self.sessions.clone();
        std::thread::Builder::new()
            .name("blaze-inproc start".to_owned())
            .spawn(move || {
                let session = pending.wait(START).and_then(|session| {
                    let mut starts = lock_starts(&starts);
                    match starts.starts.get(&started.0) {
                        Some(Start::Waiting) => {}
                        _ => {
                            starts.starts.remove(&started.0);
                            return Err(Error::new(format!(
                                "process {} ended while its start hook attached",
                                session.pid
                            )));
                        }
                    }
                    let mut memory = lock(&memory);
                    match forks {
                        true => memory.attach_forking(session.pid),
                        false => memory.attach(session.pid),
                    }
                    starts.starts.insert(started.0, Start::Attached(session.pid));
                    Ok(session)
                });
                if session.is_err() {
                    let mut starts = lock_starts(&starts);
                    match starts.starts.get(&started.0) {
                        Some(Start::Waiting) => {
                            starts.starts.insert(started.0, Start::Ended);
                        }
                        Some(Start::Ended) => {
                            starts.starts.remove(&started.0);
                        }
                        _ => {}
                    }
                }
                let _ = sessions.send(session);
            })
            .map_err(|it| Error::new(format!("starting the thread that waits for a start hook: {it}")))?;
        Ok((command, started))
    }

    /// The process of `started` exited, and every child it forked too: its threads hold no
    /// fence any more.
    pub fn exited(&self, started: Started) {
        let mut starts = lock_starts(&self.starts);
        match starts.starts.get(&started.0).copied() {
            Some(Start::Attached(pid)) => {
                starts.starts.remove(&started.0);
                lock(&self.memory).exited(pid);
            }
            Some(Start::Waiting) => {
                starts.starts.insert(started.0, Start::Ended);
            }
            Some(Start::Ended) => {
                starts.starts.remove(&started.0);
            }
            None => {}
        }
    }
}

fn lock_starts(starts: &Mutex<Starts>) -> parking_lot::MutexGuard<'_, Starts> {
    starts.lock()
}

fn lock(memory: &Mutex<CodeMemory>) -> parking_lot::MutexGuard<'_, CodeMemory> {
    memory.lock()
}

fn accept(listener: &sys::Listener, waiting: &Table) {
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(it) => it,
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
        };
        let waiting = Arc::clone(waiting);
        let started = std::thread::Builder::new()
            .name("blaze-inproc handshake".to_owned())
            .spawn(move || handshake(Channel::new(stream), peer, &waiting));
        if let Err(error) = started {
            eprintln!("blaze-inproc: starting a handshake thread: {error}");
        }
    }
}

/// One start hook's handshake. A peer that names no waiting launch is dropped: it gets
/// nothing.
fn handshake(mut channel: Channel, peer: u32, waiting: &Table) {
    if channel.set_timeout(Some(HANDSHAKE)).is_err() {
        return;
    }
    let hello = match channel.recv().and_then(|frame| match frame.kind {
        kinds::HELLO => Hello::decode(frame.body),
        other => Err(Error::new(format!("a start hook's first frame of kind {:#06x}", other.0))),
    }) {
        Ok(it) => it,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    let Some(launch) = waiting.lock().remove(&hello.token) else {
        return;
    };
    let mut seq = 0;
    let session = match serve(&mut channel, peer, &hello, &launch, &mut seq) {
        Ok((remote, bells, thread)) => {
            let _ = channel.set_timeout(None);
            let slide = hello.header.wrapping_sub(launch.grant.header);
            let run = Arc::new(ask::Run::new(launch.asks, bells, remote, peer));
            Ok(Session { pid: peer, slide, thread, channel, run })
        }
        Err(error) => {
            let mut body = Vec::new();
            Writer::new(&mut body).str(&error.to_string());
            let _ = channel.send(kinds::REFUSED, seq, &body);
            Err(error)
        }
    };
    let _ = launch.done.send(session);
}

/// Serves one start hook until it started, `seq` the sequence number of the last frame it
/// sent: what a refusal answers.
fn serve(
    channel: &mut Channel,
    peer: u32,
    hello: &Hello,
    launch: &Waiting,
    seq: &mut u32,
) -> Result<(sys::Remote, sys::Bells, u64), Error> {
    if hello.pid != peer {
        return Err(Error::new(format!(
            "a start hook that says it is process {} on a connection from process {peer}",
            hello.pid
        )));
    }
    if hello.version != VERSION {
        return Err(Error::new(format!("a start hook of version {}, where this blaze speaks {VERSION}", hello.version)));
    }
    if hello.page != sys::page() {
        return Err(Error::new(format!("a start hook of pages of {:#x} bytes, where the daemon's are {:#x}", hello.page, sys::page())));
    }
    let slide = hello.header.wrapping_sub(launch.grant.header);
    let objects: Vec<(&sys::Object, Rights)> = launch.objects.iter().map(|(it, rights)| (it, *rights)).collect();
    let remote = sys::send_objects(channel, peer, &launch.encoded, &objects)?;
    let (bells, names) = sys::Bells::new(&remote, ask::SLOTS)?;
    ask::name_bells(&launch.asks, &names);
    loop {
        let frame = channel.recv()?;
        let (kind, body) = (frame.kind, frame.body.to_vec());
        *seq = frame.seq;
        let seq = *seq;
        let bad = |it: crate::frame::FrameError| Error::new(format!("process {peer}'s start hook: {it}"));
        let mut answer = Vec::with_capacity(8);
        match kind {
            kinds::MAP => match decode_map(&body).and_then(|(object, access)| remote.map(object, access)) {
                Ok(address) => {
                    Writer::new(&mut answer).u64(address);
                    channel.send(kinds::MAPPED, seq, &answer)?;
                }
                Err(error) => {
                    Writer::new(&mut answer).str(&error.to_string());
                    channel.send(kinds::REFUSED, seq, &answer)?;
                }
            },
            kinds::OVERLAY => {
                Reader::new(&body).finish().map_err(bad)?;
                remote.overlay(slide, &launch.grant)?;
                channel.send(kinds::OVERLAID, seq, &[])?;
            }
            kinds::STARTED => {
                let mut read = Reader::new(&body);
                let thread = read.u64().map_err(bad)?;
                read.finish().map_err(bad)?;
                return Ok((remote, bells, thread));
            }
            kinds::FAILED => {
                let why = Reader::new(&body).str().map_err(bad)?.to_owned();
                return Err(Error::new(format!("process {peer}'s start hook could not attach: {why}")));
            }
            other => return Err(Error::new(format!("process {peer}'s start hook sent a frame of kind {:#06x}", other.0))),
        }
    }
}

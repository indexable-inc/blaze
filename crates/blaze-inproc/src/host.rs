//! The daemon's side of a run: the command that spawns the executable its build placed at
//! cargo's path through `blaze-core`'s `Effect::Spawn` (the host writes no file: the build's
//! placement is the path's one writer, [`StartError`]), and the handshake that lays the code
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use blaze_core::{Command, Sandbox};
use blaze_core::lock::{Lock, LockGuard};

use crate::channel::Channel;
use crate::frame::{Reader, Writer, kinds};
use crate::handshake::{Grant, HOST_VAR, Hello, Lookup, Overlay, Place, VERSION, decode_map, hex};
use crate::sys::{self, Rights};
use crate::{Access, CodeMemory, Error, FixedMap, ImageLookup, ImageWrite, ask};

/// How long a start hook may take between its frames during the handshake.
const HANDSHAKE: Duration = Duration::from_secs(30);

/// Why a program was not started from its path ([`Program::command`]).
///
/// The invariant: an output path has one writer, the build's placement. The host writes no
/// file: it has no effect that writes one. A program is started from its path only where the
/// build's record of that path holds the key of the file the program's memory is, `held`;
/// anywhere else it is this refusal, never a write of the memory's bytes there in its place.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StartError {
    /// The build's record of `path` holds `placed` (`None`: no record, a launch asked before
    /// its job's placements ended, which is a bug of the caller's), and the memory's file is
    /// `held`.
    NotPlaced { path: PathBuf, placed: Option<[u8; 32]>, held: [u8; 32] },
    /// The launch itself failed.
    Launch(Error),
}

impl StartError {
    /// The one rule of a start ([`Program::command`]): a program whose memory's file is `held`
    /// starts from `path` only where the build's record of `path` holds `placed == held`;
    /// otherwise [`StartError::NotPlaced`], naming the path and both keys. What every launch
    /// checks, and what a model of the host in a test checks the same.
    pub fn check(path: &std::path::Path, placed: Option<[u8; 32]>, held: [u8; 32]) -> Result<(), StartError> {
        match placed == Some(held) {
            true => Ok(()),
            // alloc: error path, the path
            false => Err(StartError::NotPlaced { path: path.to_path_buf(), placed, held }),
        }
    }
}

impl From<Error> for StartError {
    fn from(error: Error) -> StartError {
        StartError::Launch(error)
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::NotPlaced { path, placed: Some(placed), held } => write!(
                f,
                "{} holds the build's output {}, and the code memory it is started on holds {}: a program starts \
                 only from the file its build placed (the code memory's host writes no file)",
                path.display(),
                Hex(placed),
                Hex(held)
            ),
            StartError::NotPlaced { path, placed: None, held } => write!(
                f,
                "{} holds no output its build placed and looked at, and the code memory it is started on holds {}: \
                 a program starts only once its job's placements have ended, from the file placed (the code \
                 memory's host writes no file)",
                path.display(),
                Hex(held)
            ),
            StartError::Launch(error) => write!(f, "{error}"),
        }
    }
}

/// A content key in lowercase hex, as the store names it.
struct Hex<'a>(&'a [u8; 32]);

impl std::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A run to start: `program`, the executable the build placed at its path, whose file
/// `image`'s code memory is, with the `maps` laid over its reservations too.
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
type Table = Arc<Lock<HashMap<[u8; 16], Waiting>>>;

/// A count of threads alive, one more while each [`Alive`] of it lives.
#[derive(Clone, Default)]
struct Threads(Arc<AtomicUsize>);

/// One thread counted in a [`Threads`] from when it is about to be spawned until it ends
/// (or could not be spawned), however it ends.
struct Alive(Threads);

impl Threads {
    fn one_more(&self) -> Alive {
        self.0.fetch_add(1, Ordering::Relaxed);
        Alive(self.clone()) // clone: a count
    }

    fn alive(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The daemon's side of dev runs: it lays a code memory over a program the kernel started from
/// its file. It writes no file, and has nothing that could: the file at a program's path is
/// its build's output, placed by the build alone ([`StartError`]).
pub struct Host {
    address: String,
    waiting: Table,
    /// The handshake threads alive: one a start hook connected and not yet started or
    /// refused, each ended within [`HANDSHAKE`] of its hook's last frame.
    handshakes: Threads,
}

impl Host {
    /// A host listening in `dir` (made if missing, reachable by this user only).
    pub fn new(dir: &Path) -> Result<Host, Error> {
        let (listener, address) = sys::Listener::bind(dir)?;
        // alloc: once per host
        let waiting: Table = Arc::new(Lock::new("inproc.host.waiting", HashMap::new()));
        let table = Arc::clone(&waiting);
        let handshakes = Threads::default();
        let counted = handshakes.clone(); // clone: a count
        std::thread::Builder::new()
            .name("blaze-inproc host".to_owned())
            .spawn(move || accept(&listener, &table, &counted))
            .map_err(|it| Error::new(format!("starting the host's thread: {it}")))?;
        Ok(Host { address, waiting, handshakes })
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
        let mut env: Vec<(OsString, OsString)> =
            launch.env.iter().filter(|(name, _)| name != HOST_VAR).cloned().collect();
        env.push((HOST_VAR.into(), var));
        let mut sandbox = launch.sandbox.clone();
        if let Sandbox::On { read, services, .. } = &mut sandbox {
            read.push(launch.program.clone());
            services.extend(sys::service());
            // The host's socket is a readable entry, never a writable one: the sandbox makes
            // a readable socket one its process may connect to by that exact path and nothing
            // more (Linux: bound read-only into its view, and the supervisor lets `connect`
            // through to exactly that path; macOS: a `(remote unix-socket (path-literal ..))`
            // rule), as it does the fork server's. It gets neither the directory nor the other
            // runs' sockets in it, and may not bind, unlink or replace the socket. A writable
            // entry would have been a writable directory to make (macOS) or a bind the Linux
            // sandbox refuses for a socket, and either way more than a connect.
            //
            // What the run can now do that it could not: connect to this host and, within
            // [`HANDSHAKE`], send a first frame. The host answers a connection only from a
            // process of the daemon's user (the kernel's peer credentials), only for a HELLO
            // naming a token of a launch still waiting, and each token once (the first HELLO
            // naming it takes it from the table): anything else is dropped with nothing sent.
            // The run's own token is taken by its start hook before `main`, so the program's
            // code holds no token it can use. Another launch's token is in that process's
            // environment alone; a sandbox that keeps a run from reading another process's
            // environment (Linux: a PID namespace with its own `/proc`, or Landlock's scoping
            // of `ptrace` access when confined) keeps the host from being reached for anything
            // but nothing. A named pipe (Windows) is not a path the sandbox's file rules cover.
            if cfg!(unix) {
                read.push(PathBuf::from(&self.address));
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

    fn table(&self) -> LockGuard<'_, HashMap<[u8; 16], Waiting>> {
        self.waiting.lock()
    }

    /// How many launches wait for their start hook to connect, each holding its grant's
    /// objects: at most those started in the last [`START`], since each one's wait ends by
    /// then and drops it ([`Pending::wait`]).
    pub fn waiting(&self) -> usize {
        self.table().len()
    }

    /// How many handshake threads are alive: one for each start hook connected whose
    /// handshake has not ended. None once every launch attached or was refused.
    pub fn handshakes(&self) -> usize {
        self.handshakes.alive()
    }
}

/// The objects a run of `launch` is given and its grant: every code segment from the image
/// part, the arena, the cells and the fixed maps in their reservations; the control words and
/// a new asks object, mapped by the start hook itself.
fn grant(launch: &Launch<'_>) -> Result<(Vec<(sys::Object, Rights)>, Grant), Error> {
    let memory = launch.image;
    let layout = memory.layout().ok_or_else(|| {
        Error::new("a launch of a code memory not laid out (CodeMemory::lay_out)")
    })?;
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
        let reserved = layout
            .reserved
            .iter()
            .any(|it| it.start <= overlay.at && overlay.range().end <= it.end);
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
            .filter(|start| {
                u32::try_from(it.name.len()).is_ok_and(|len| start.checked_add(len).is_some())
            })
            .ok_or_else(|| {
                Error::new(format!(
                    "the names of a launch's lookups run past 4 GiB at `{}`",
                    it.name
                ))
            })?; // alloc: error path
        names.push_str(it.name);
        lookups.push(Lookup {
            place: it.place,
            addend: it.addend,
            weak: it.weak,
            name: start,
            len: it.name.len() as u32,
        });
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
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                Err(Error::new("the host stopped before the start hook attached"))
            }
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
/// dev `run` and test paths hold for an executable. Each [`Program::command`] is a launch of
/// the file its build placed at the command's path, where that file is the memory's
/// ([`StartError`]); a thread of its own waits for the start hook, attaches the process to the
/// memory and hands the session on (to whoever answers the process's asks: `blaze-lazy`'s
/// runner).
pub struct Program {
    host: Arc<Host>,
    memory: Arc<Lock<CodeMemory>>,
    /// Changed only with `memory` locked ([`Program::relaid`]), so a launch, which reads it
    /// with `memory` locked, compares the placed key with the file of the memory it maps.
    image: Lock<Image>,
    sessions: crossbeam_channel::Sender<Result<Session, Error>>,
    starts: Arc<Lock<Starts>>,
    /// The threads alive that wait for a start hook ([`Program::command`]): one a process
    /// started and not yet attached, each ended when its hook attached or within [`START`].
    starting: Threads,
}

/// The executable of a [`Program`]: `len` bytes of the image part of its code memory, the file
/// the linker wrote, which `key` names (the store's content hash of the link's output, the key
/// the build places that file under).
#[derive(Clone, Debug)]
pub struct Image {
    pub key: [u8; 32],
    pub len: u64,
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
    pub fn new(
        host: Arc<Host>,
        memory: Arc<Lock<CodeMemory>>,
        image: Image,
        sessions: crossbeam_channel::Sender<Result<Session, Error>>,
    ) -> Program {
        Program {
            host,
            memory,
            image: Lock::new("inproc.program.image", image),
            sessions,
            starts: Arc::new(Lock::new("inproc.program.starts", Starts::default())), // alloc: once per program
            starting: Threads::default(),
        }
    }

    /// The executable is now `image`: `memory`, which the caller holds locked, was committed
    /// into and laid out again ([`CodeMemory::lay_out`]), so its image part is that file. Runs
    /// started from here on start only from that file placed at their path; runs already
    /// started keep theirs.
    pub fn relaid(&self, memory: &CodeMemory, image: Image) -> Result<(), Error> {
        if !memory.is_pristine() {
            return Err(Error::new(
                "a program's executable changed to a code memory not laid out since its last write"
                    .to_owned(),
            ));
        }
        *self.image.lock() = image;
        Ok(())
    }

    pub fn memory(&self) -> &Arc<Lock<CodeMemory>> {
        &self.memory
    }

    /// The content key of the executable its memory's image part is now ([`Image::key`]).
    pub fn file(&self) -> [u8; 32] {
        self.image.lock().key
    }

    /// The command that runs what `command` would have, its program the executable its build
    /// placed at its path: arguments, environment, directory, stdin, sandbox and memory limit
    /// stay. `forks` says the process forks children that run the program (a fork server): see
    /// [`CodeMemory::attach_forking`].
    ///
    /// `placed` is the key the build's record holds for the command's path, read once the
    /// job's placements have ended (`None`: it holds none). The program starts from that file
    /// only where it is the memory's file, this program's [`Image::key`]; anything else is
    /// [`StartError::NotPlaced`] naming the path and both keys. Nothing is written: the build
    /// is the one writer of the path, so its next look finds the file as it placed it and its
    /// outputs' watch hears nothing.
    pub fn command(
        &self,
        command: Command,
        forks: bool,
        placed: Option<[u8; 32]>,
    ) -> Result<(Command, Started), StartError> {
        let (command, pending) = {
            let memory = lock(&self.memory);
            // Compared with `memory` locked, as the image is read: a link that commits into
            // the memory changes both under that lock ([`Program::relaid`]).
            let held = self.image.lock().key;
            StartError::check(&command.program, placed, held)?;
            let launch = Launch {
                image: &memory,
                // alloc: none, empty: no dependency image is mapped into a dev run yet
                maps: Vec::new(),
                writes: Vec::new(),
                lookups: Vec::new(),
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
        let alive = self.starting.one_more();
        std::thread::Builder::new()
            .name("blaze-inproc start".to_owned())
            .spawn(move || {
                let _alive = alive;
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
            .map_err(|it| {
                Error::new(format!("starting the thread that waits for a start hook: {it}"))
            })?;
        Ok((command, started))
    }

    /// Whether the start of `started` ended with its start hook never attached (the process
    /// never ran, ended first, or its hook did not connect within [`START`]), forgetting it:
    /// for a caller that is told of no exit of that process (`blaze run`'s program is its
    /// CLI's process, which the daemon does not reap), so nothing of a start that failed is
    /// kept for the program's life.
    pub fn forget_ended(&self, started: Started) -> bool {
        let mut starts = lock_starts(&self.starts);
        match starts.starts.get(&started.0) {
            Some(Start::Ended) => {
                starts.starts.remove(&started.0);
                true
            }
            _ => false,
        }
    }

    /// How many threads wait for a start hook of this program's processes: none once every
    /// process started has attached or its wait ended.
    pub fn starting(&self) -> usize {
        self.starting.alive()
    }

    /// How many starts the program still holds word of: processes whose start hook has not
    /// attached yet, attached ones not yet said exited, and failed ones not yet forgotten.
    /// What a test of a caller reads to see that its launches leave nothing behind.
    pub fn starts_held(&self) -> usize {
        lock_starts(&self.starts).starts.len()
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

fn lock_starts(starts: &Lock<Starts>) -> LockGuard<'_, Starts> {
    starts.lock()
}

fn lock(memory: &Lock<CodeMemory>) -> LockGuard<'_, CodeMemory> {
    memory.lock()
}

fn accept(listener: &sys::Listener, waiting: &Table, handshakes: &Threads) {
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(it) => it,
            Err(error) => {
                eprintln!("{error}");
                continue;
            }
        };
        let waiting = Arc::clone(waiting);
        let alive = handshakes.one_more();
        let started = std::thread::Builder::new()
            .name("blaze-inproc handshake".to_owned())
            .spawn(move || {
                let _alive = alive;
                handshake(Channel::new(stream), peer, &waiting)
            });
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
    if !is_peer(peer, hello.pid)? {
        return Err(Error::new(format!(
            "a start hook that says it is process {} on a connection from process {peer}",
            hello.pid
        )));
    }
    if hello.version != VERSION {
        return Err(Error::new(format!(
            "a start hook of version {}, where this blaze speaks {VERSION}",
            hello.version
        )));
    }
    if hello.page != sys::page() {
        return Err(Error::new(format!(
            "a start hook of pages of {:#x} bytes, where the daemon's are {:#x}",
            hello.page,
            sys::page()
        )));
    }
    let slide = hello.header.wrapping_sub(launch.grant.header);
    let objects: Vec<(&sys::Object, Rights)> =
        launch.objects.iter().map(|(it, rights)| (it, *rights)).collect();
    let remote = sys::send_objects(channel, peer, &launch.encoded, &objects)?;
    let (bells, names) = sys::Bells::new(&remote, ask::SLOTS)?;
    ask::name_bells(&launch.asks, &names);
    loop {
        let frame = channel.recv()?;
        let (kind, body) = (frame.kind, frame.body.to_vec());
        *seq = frame.seq;
        let seq = *seq;
        let bad =
            |it: crate::frame::FrameError| Error::new(format!("process {peer}'s start hook: {it}"));
        let mut answer = Vec::with_capacity(8);
        match kind {
            kinds::MAP => {
                match decode_map(&body).and_then(|(object, access)| remote.map(object, access)) {
                    Ok(address) => {
                        Writer::new(&mut answer).u64(address);
                        channel.send(kinds::MAPPED, seq, &answer)?;
                    }
                    Err(error) => {
                        Writer::new(&mut answer).str(&error.to_string());
                        channel.send(kinds::REFUSED, seq, &answer)?;
                    }
                }
            }
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
                return Err(Error::new(format!(
                    "process {peer}'s start hook could not attach: {why}"
                )));
            }
            other => {
                return Err(Error::new(format!(
                    "process {peer}'s start hook sent a frame of kind {:#06x}",
                    other.0
                )));
            }
        }
    }
}

/// Whether the start hook on a connection from `peer` (a process ID in the daemon's PID
/// namespace, from the kernel) saying it is `said` (its own `getpid`) speaks of itself. A
/// sandboxed run on Linux is in a PID namespace of its own, where `getpid` answers its ID
/// there; the kernel lists its ID in every namespace from the daemon's down in `NSpid` of
/// `/proc/<peer>/status`, the last its own. What a hook says is not what authenticates it
/// (the token and the peer's user are): this catches a hook that names another process.
fn is_peer(peer: u32, said: u32) -> Result<bool, Error> {
    if said == peer {
        return Ok(true);
    }
    if !cfg!(target_os = "linux") {
        return Ok(false);
    }
    let path = format!("/proc/{peer}/status");
    let status = std::fs::read_to_string(&path).map_err(|it| {
        Error::new(format!("reading {path}, the process IDs of process {peer}: {it}"))
    })?;
    Ok(innermost(&status) == Some(said))
}

/// The last ID of the `NSpid` line of a `/proc/<pid>/status`: the process's ID in its own
/// PID namespace. `None` where there is no such line or it is not IDs.
pub(crate) fn innermost(status: &str) -> Option<u32> {
    let line = status.lines().find_map(|it| it.strip_prefix("NSpid:"))?;
    line.split_whitespace().last()?.parse().ok()
}

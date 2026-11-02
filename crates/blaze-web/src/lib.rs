//! The build page (docs/web.md): the daemon keeps the functions of each workspace's program
//! from its builds' records in a Flecs world ([`world`]), on a thread of its own ([`Graphs`])
//! made once a page is first asked for, and serves the page on 127.0.0.1 ([`serve`]), with the
//! records of every build over a websocket as they come and Flecs's REST over the world.
//!
//! A record costs the daemon a lookup of its workspace's root, which takes no lock once the
//! root is known ([`Roots`]), and one send on a channel that takes none: keeping it, applying
//! it and encoding it are the page's thread's. That thread takes what came in batches and sends
//! each page one frame of all the records of a batch, encoded once for every page.
//!
//! Its one unsafe module is [`flecs`], the Flecs C calls behind a safe API
//! (docs/v2/plan/design/unsafe.md); every other module forbids unsafe code.

#![deny(unsafe_code)]
#![feature(random)]

mod flecs;
mod http;
mod kept;
#[cfg(test)]
mod tests;
pub mod world;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::{
    io::Write,
    net::TcpListener,
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
};

use blaze_protocol::events::{Event, Record};
use kept::{KEPT, Kept};
use parking_lot::{Mutex, MutexGuard};
use world::{Answer, Applied, Program, World};

/// The most bytes a page may have waiting to be sent: one that falls this far behind is hung
/// up on, and starts over from a snapshot when it connects again.
const BEHIND: usize = 256 << 20;

/// The most asks the world's thread takes before it sends the pages what they told.
const BATCH: usize = 4096;

/// A frame of records is sent once it is this long.
const FRAME: usize = 1 << 20;

/// The world's thread, started by the first record or page: what the daemon and the pages ask
/// it. It keeps what the world is made from until a page is first asked for: a daemon no page
/// is asked of keeps no world.
#[derive(Default)]
pub struct Graphs {
    roots: Arc<Roots>,
    thread: OnceLock<Result<crossbeam_channel::Sender<Asked>, String>>,
}

enum Asked {
    Record { root: u32, build: u64, record: Record },
    Start(crossbeam_channel::Sender<Result<(), String>>),
    Follow(crossbeam_channel::Sender<Result<Following, String>>),
    Rest {
        method: String,
        path: String,
        answer: crossbeam_channel::Sender<Result<Answer, String>>,
    },
    #[cfg(test)]
    Kept(crossbeam_channel::Sender<Vec<(u64, kept::Kind)>>),
}

/// What a page is sent, in order, and how many bytes of it it has not taken yet.
pub struct Following {
    told: crossbeam_channel::Receiver<Arc<[u8]>>,
    waiting: Arc<AtomicUsize>,
}

/// What a page is sent next.
pub enum Next {
    /// A snapshot, or a CBOR sequence of records (docs/web.md, "The wire").
    Message(Arc<[u8]>),
    /// Nothing came in the time waited.
    Nothing,
    /// The world hung up on it: it fell too far behind.
    Gone,
}

impl Following {
    /// The next message, waiting at most `wait` for one.
    pub fn next(&self, wait: std::time::Duration) -> Next {
        match self.told.recv_timeout(wait) {
            Ok(message) => self.took(message),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Next::Nothing,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Next::Gone,
        }
    }

    /// The next message if one is there already; a page hung up on is told by [`Self::next`].
    pub fn ready(&self) -> Option<Arc<[u8]>> {
        let message = self.told.try_recv().ok()?;
        self.waiting.fetch_sub(message.len(), Ordering::Relaxed);
        Some(message)
    }

    fn took(&self, message: Arc<[u8]>) -> Next {
        self.waiting.fetch_sub(message.len(), Ordering::Relaxed);
        Next::Message(message)
    }
}

impl Graphs {
    /// Hands `record` of build `build` of the workspace at `root` to the world; it never
    /// waits for it.
    pub fn record(&self, root: &Path, build: u64, record: Record) {
        let Some(root) = self.roots.intern(root) else { return };
        if let Ok(to) = self.thread() {
            let _ = to.send(Asked::Record { root, build, record });
        }
    }

    /// The messages a page is sent from now on: the snapshot of the workspace shown, its last
    /// build's records, and every one after as it comes.
    pub fn follow(&self) -> Result<Following, String> {
        let (to, following) = crossbeam_channel::unbounded();
        self.ask(Asked::Follow(to))?;
        following.recv().map_err(|_| gone())?
    }

    /// What Flecs's REST answers `method` of `path`, over the world.
    pub fn rest(&self, method: &str, path: &str) -> Result<Answer, String> {
        let (answer, answered) = crossbeam_channel::unbounded();
        // alloc: the edge, once per REST request
        self.ask(Asked::Rest { method: method.to_owned(), path: path.to_owned(), answer })?;
        answered.recv().map_err(|_| gone())?
    }

    /// Makes the world, from what was kept until now.
    fn start(&self) -> Result<(), String> {
        let (to, started) = crossbeam_channel::unbounded();
        self.ask(Asked::Start(to))?;
        started.recv().map_err(|_| gone())?
    }

    /// The build and kind of each record kept of the workspace whose build began first.
    #[cfg(test)]
    fn kept(&self) -> Vec<(u64, kept::Kind)> {
        let (to, kept) = crossbeam_channel::unbounded();
        self.ask(Asked::Kept(to)).unwrap();
        kept.recv().unwrap()
    }

    fn ask(&self, asked: Asked) -> Result<(), String> {
        self.thread()?.send(asked).map_err(|_| gone())
    }

    fn thread(&self) -> Result<&crossbeam_channel::Sender<Asked>, String> {
        let roots = &self.roots;
        // clone: an Arc's count, once, as the thread starts; the error's is an error path
        self.thread.get_or_init(|| spawn(roots.clone())).as_ref().map_err(Clone::clone)
    }
}

fn gone() -> String {
    // alloc: an error path
    "the build page's world is gone".to_owned()
}

/// The roots of the workspaces records are of, each numbered from its first record on, and
/// append-only: a record of a root seen before finds it with acquire loads and no lock or
/// shared write (docs/v2/plan/design/concurrency.md). Level `l` holds `FIRST << l` roots,
/// made when the first of them is added; a root is added under `adding`, once a workspace.
struct Roots {
    levels: [OnceLock<Box<[OnceLock<Box<Path>>]>>; LEVELS],
    len: AtomicU32,
    /// The root of the last record: the next is nearly always of it.
    last: AtomicU32,
    adding: Mutex<()>,
}

const FIRST: usize = 16;
/// Enough levels for every `u32`.
const LEVELS: usize = 29;

impl Default for Roots {
    fn default() -> Roots {
        Roots {
            levels: [const { OnceLock::new() }; LEVELS],
            len: AtomicU32::new(0),
            last: AtomicU32::new(0),
            adding: Mutex::new(()),
        }
    }
}

impl Roots {
    /// The level root `id` is in, and where in it.
    fn place(id: u32) -> (usize, usize) {
        let n = id as usize / FIRST + 1;
        let level = (usize::BITS - 1 - n.leading_zeros()) as usize;
        (level, id as usize - FIRST * ((1 << level) - 1))
    }

    fn get(&self, id: u32) -> Option<&Path> {
        let (level, at) = Roots::place(id);
        self.levels.get(level)?.get()?.get(at)?.get().map(|it| &**it)
    }

    fn find(&self, root: &Path) -> Option<u32> {
        let len = self.len.load(Ordering::Acquire);
        (0..len).rev().find(|&id| self.get(id) == Some(root))
    }

    /// The number of `root`, or none when 2³² roots are taken.
    fn intern(&self, root: &Path) -> Option<u32> {
        let last = self.last.load(Ordering::Relaxed);
        if self.get(last) == Some(root) {
            return Some(last);
        }
        let id = match self.find(root) {
            Some(id) => id,
            None => {
                let _adding = lock(&self.adding);
                match self.find(root) {
                    Some(id) => id,
                    None => {
                        let id = self.len.load(Ordering::Acquire);
                        if id == u32::MAX {
                            eprintln!("build page: no number is left for {}", root.display());
                            return None;
                        }
                        let (level, at) = Roots::place(id);
                        let slots = self.levels[level].get_or_init(|| {
                            // alloc: once per level, 29 in all, as its first root is added
                            (0..FIRST << level).map(|_| OnceLock::new()).collect()
                        });
                        let _ = slots[at].set(root.into());
                        self.len.store(id + 1, Ordering::Release);
                        id
                    }
                }
            }
        };
        self.last.store(id, Ordering::Relaxed);
        Some(id)
    }
}

/// Starts the world's thread.
fn spawn(roots: Arc<Roots>) -> Result<crossbeam_channel::Sender<Asked>, String> {
    let (to, asked) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("blaze-web-world".into())
        .spawn(move || Thread { roots, kept: Kept::default(), world: None }.run(asked))
        // alloc: an error path
        .map_err(|e| format!("no thread for the build page's world: {e}"))?;
    Ok(to)
}

/// The world's thread's own: what it keeps until a page is asked for, then the world, or why
/// it could not be made.
struct Thread {
    roots: Arc<Roots>,
    kept: Kept,
    world: Option<Result<State, String>>,
}

impl Thread {
    fn run(mut self, asked: crossbeam_channel::Receiver<Asked>) {
        while let Ok(first) = asked.recv() {
            self.answer(first);
            for next in asked.try_iter().take(BATCH - 1) {
                self.answer(next);
            }
            if let Some(Ok(state)) = &mut self.world {
                state.flush();
            }
        }
    }

    fn answer(&mut self, asked: Asked) {
        match asked {
            Asked::Record { root, build, record } => match &mut self.world {
                None => self.kept.keep(root, build, &record),
                Some(Ok(state)) => state.record(&self.roots, root, build, &record),
                Some(Err(_)) => {}
            },
            Asked::Start(answer) => {
                let _ = answer.send(self.state().map(|_| ()));
            }
            Asked::Follow(answer) => {
                let _ = answer.send(self.state().map(State::follow));
            }
            Asked::Rest { method, path, answer } => {
                let _ = answer.send(self.state().and_then(|it| it.world.rest(&method, &path)));
            }
            #[cfg(test)]
            Asked::Kept(answer) => {
                let _ = answer.send(self.kept.kinds(0));
            }
        }
    }

    /// The world, made the first time from what was kept, once the pages are sent all they
    /// were told so far. A world that cannot be made is said to every page after.
    fn state(&mut self) -> Result<&mut State, String> {
        let made = match self.world.take() {
            Some(made) => made,
            None => World::new().and_then(|world| {
                let mut state = State::new(world);
                let roots = &self.roots;
                std::mem::take(&mut self.kept)
                    .drain(|root, build, record| state.record(roots, root, build, &record))
                    // alloc: an error path, said to each page asked for after
                    .map_err(|unkept| unkept.to_string())?;
                Ok(state)
            }),
        };
        match self.world.insert(made) {
            Ok(state) => {
                state.flush();
                Ok(state)
            }
            // clone: an error path, said to each page asked for after
            Err(why) => Err(why.clone()),
        }
    }
}

/// The world and the pages that follow it.
struct State {
    world: World,
    programs: Vec<Program>,
    /// Each program's root, as [`Roots`] numbers it.
    roots: Vec<u32>,
    /// The workspace whose build began last: the one the pages show.
    shown: Option<usize>,
    followers: Vec<Follower>,
    /// What the followers are sent next: a CBOR sequence of records, one frame once flushed,
    /// and what encodes a record into it.
    pending: Vec<u8>,
    writer: blaze_protocol::cbor::Writer,
    /// The gzipped snapshot of the program of a root as of its `changes`.
    snapshot: Option<(u32, u64, Arc<[u8]>)>,
}

struct Follower {
    to: crossbeam_channel::Sender<Arc<[u8]>>,
    waiting: Arc<AtomicUsize>,
}

impl State {
    fn new(world: World) -> State {
        // alloc: none until they fill, once per daemon
        let (programs, roots) = (Vec::new(), Vec::new());
        // alloc: as above
        let (followers, pending) = (Vec::new(), Vec::new());
        let writer = blaze_protocol::cbor::Writer::new();
        State { world, programs, roots, shown: None, followers, pending, writer, snapshot: None }
    }

    fn record(&mut self, roots: &Roots, root: u32, build: u64, record: &Record) {
        let at = match self.roots.iter().position(|&it| it == root) {
            Some(at) => at,
            None => {
                if self.programs.len() >= KEPT {
                    let oldest = (0..self.programs.len())
                        .filter(|&it| Some(it) != self.shown)
                        .min_by_key(|&it| self.programs[it].build());
                    if let Some(oldest) = oldest {
                        let program = self.programs.remove(oldest);
                        let gone = self.roots.remove(oldest);
                        self.world.forget(program);
                        if self.snapshot.as_ref().is_some_and(|it| it.0 == gone) {
                            self.snapshot = None;
                        }
                        self.shown = self.shown.map(|it| if it > oldest { it - 1 } else { it });
                    }
                }
                let Some(path) = roots.get(root) else { return };
                let program = self.world.program(path);
                self.programs.push(program);
                self.roots.push(root);
                self.programs.len() - 1
            }
        };
        let began = matches!(record.event, Event::Began { .. });
        let told = self.programs[at].last_told().0;
        let applied = self.world.apply(&mut self.programs[at], build, record);
        let moved = began && self.shown != Some(at);
        if moved {
            self.shown = Some(at);
        }
        if self.shown != Some(at) || self.followers.is_empty() {
            return;
        }
        if moved || applied == Applied::Anew {
            self.flush();
            let Some(snapshot) = self.snapshot(at) else { return };
            // clone: an Arc's count, once per page a build begins
            self.followers.retain(|it| it.send(snapshot.clone()));
        }
        // The functions of a record that made the program anew are in its snapshot.
        if applied != Applied::Anew {
            let (now, last) = self.programs[at].last_told();
            if now != told {
                self.pending.extend_from_slice(last);
            } else if matches!(record.event, Event::Functions { .. }) {
                if let Err(error) = self.writer.append(&mut self.pending, record) {
                    eprintln!("build page: a record does not encode: {error}");
                }
            }
            if self.pending.len() >= FRAME {
                self.flush();
            }
        }
    }

    /// Sends the followers the records not yet sent, as one frame.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let frame: Arc<[u8]> = Arc::from(&self.pending[..]);
        self.pending.clear();
        // clone: an Arc's count, once per page a frame: the frame is encoded once for all
        self.followers.retain(|it| it.send(frame.clone()));
    }

    fn follow(&mut self) -> Following {
        self.flush();
        let (to, told) = crossbeam_channel::unbounded();
        let follower = Follower { to, waiting: Arc::default() };
        // clone: an Arc's count, once per page
        let waiting = follower.waiting.clone();
        match self.shown {
            Some(at) => {
                if let Some(snapshot) = self.snapshot(at) {
                    follower.send(snapshot);
                }
                let records = self.programs[at].told();
                if !records.is_empty() {
                    follower.send(records.into());
                }
            }
            None => {
                let program = self.world.program(Path::new(""));
                if let Some(snapshot) = gzip(&self.world.snapshot(&program)) {
                    follower.send(snapshot);
                }
                self.world.forget(program);
            }
        }
        self.followers.push(follower);
        Following { told, waiting }
    }

    /// The snapshot of program `at`, gzipped (docs/web.md, "The snapshot"): made again only
    /// once the program has changed.
    fn snapshot(&mut self, at: usize) -> Option<Arc<[u8]>> {
        let (root, changes) = (self.roots[at], self.programs[at].changes());
        if let Some((of, as_of, snapshot)) = &self.snapshot
            && (*of, *as_of) == (root, changes)
        {
            // clone: an Arc's count
            return Some(snapshot.clone());
        }
        let snapshot = gzip(&self.world.snapshot(&self.programs[at]))?;
        // clone: an Arc's count
        self.snapshot = Some((root, changes, snapshot.clone()));
        Some(snapshot)
    }
}

impl Follower {
    /// Whether the page is still there and not too far behind to send it `message`.
    fn send(&self, message: Arc<[u8]>) -> bool {
        if self.waiting.fetch_add(message.len(), Ordering::Relaxed) >= BEHIND {
            return false;
        }
        self.to.send(message).is_ok()
    }
}

fn gzip(bytes: &[u8]) -> Option<Arc<[u8]>> {
    let mut gzip = flate2::write::GzEncoder::new(
        Vec::with_capacity(bytes.len() / 8),
        flate2::Compression::fast(),
    );
    gzip.write_all(bytes).ok()?;
    Some(gzip.finish().ok()?.into())
}

/// A lock whose holder panicked holds what it held whole: it is only ever taken to add a root.
fn lock<T>(it: &Mutex<T>) -> MutexGuard<'_, T> {
    it.lock()
}

/// The page's server, once started.
pub struct Served {
    /// Where the page is, its token in it.
    pub url: String,
}

/// Serves the page of `graphs` on a port of 127.0.0.1 the system picks, on threads of its
/// own, until the process exits, its world made from the records kept until now. `asked` is
/// called for each request: the daemon is in use.
// dyn: the daemon's hook, called once per connection
pub fn serve(graphs: Arc<Graphs>, asked: Arc<dyn Fn() + Send + Sync>) -> std::io::Result<Served> {
    graphs.start().map_err(std::io::Error::other)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let mut token = [0u8; 16];
    std::random::Rng::fill_bytes(&mut std::random::SystemRng, &mut token);
    // alloc: once per server, its token and where the page is
    let token: String = token.iter().map(|it| format!("{it:02x}")).collect();
    // alloc: as above
    let url = format!("http://127.0.0.1:{port}/{token}/");
    // alloc: once per server
    let site = Arc::new(http::Site { port, token, graphs });
    std::thread::Builder::new().name("blaze-web".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            asked();
            // clone: an Arc's count, once per connection
            let site = site.clone();
            let spawned = std::thread::Builder::new()
                .name("blaze-web-request".into())
                .spawn(move || site.answer(stream));
            if let Err(error) = spawned {
                eprintln!("build page: no thread for a request: {error}");
            }
        }
    })?;
    Ok(Served { url })
}

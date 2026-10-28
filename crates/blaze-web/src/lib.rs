//! The build page (docs/web.md): the daemon keeps the functions of each workspace's program
//! from its builds' records in a Flecs world ([`world`]), on a thread of its own ([`Graphs`]),
//! and once asked serves a page of it on 127.0.0.1 ([`serve`]), with the records of every
//! build over a websocket as they come and Flecs's REST over the world.

mod http;
#[cfg(test)]
mod tests;
pub mod world;

use std::{
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};

use blaze_protocol::events::{Event, Record};
use world::{Answer, Applied, Program, World};

/// How many workspaces' programs the daemon keeps.
const KEPT: usize = 16;

/// The most messages a page may have waiting to be sent: one that falls this far behind is
/// hung up on, and starts over from a snapshot when it connects again.
const BEHIND: usize = 1 << 16;

/// The world's thread, started by the first record: what the daemon and the pages ask it.
#[derive(Default)]
pub struct Graphs {
    asked: OnceLock<Result<mpsc::Sender<Asked>, String>>,
}

enum Asked {
    Record { root: PathBuf, build: u64, record: Record },
    Follow(mpsc::Sender<Following>),
    Rest { method: String, path: String, answer: mpsc::Sender<Result<Answer, String>> },
}

/// The world's thread's own.
struct State {
    world: World,
    programs: Vec<Program>,
    /// The workspace whose build began last: the one the pages show.
    shown: Option<usize>,
    followers: Vec<Follower>,
}

struct Follower {
    to: mpsc::Sender<Arc<[u8]>>,
    waiting: Arc<AtomicUsize>,
}

/// What a page is sent, in order, and how many of them it has not taken yet.
pub struct Following {
    told: mpsc::Receiver<Arc<[u8]>>,
    waiting: Arc<AtomicUsize>,
}

/// What a page is sent next.
pub enum Next {
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
            Ok(message) => {
                self.waiting.fetch_sub(1, Ordering::Relaxed);
                Next::Message(message)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Next::Nothing,
            Err(mpsc::RecvTimeoutError::Disconnected) => Next::Gone,
        }
    }
}

impl Graphs {
    /// Hands `record` of build `build` of the workspace at `root` to the world; it never
    /// waits for it.
    pub fn record(&self, root: &Path, build: u64, record: Record) {
        let _ = self.ask(Asked::Record { root: root.to_owned(), build, record });
    }

    /// The messages a page is sent from now on: the snapshot of the workspace shown, its last
    /// build's records, and every one after as it comes.
    pub fn follow(&self) -> Result<Following, String> {
        let (to, following) = mpsc::channel();
        self.ask(Asked::Follow(to))?;
        following.recv().map_err(|_| "the build page's world is gone".to_owned())
    }

    /// What Flecs's REST answers `method` of `path`, over the world.
    pub fn rest(&self, method: &str, path: &str) -> Result<Answer, String> {
        let (answer, answered) = mpsc::channel();
        self.ask(Asked::Rest { method: method.to_owned(), path: path.to_owned(), answer })?;
        answered.recv().map_err(|_| "the build page's world is gone".to_owned())?
    }

    fn ask(&self, asked: Asked) -> Result<(), String> {
        let to = self.asked.get_or_init(start).as_ref().map_err(Clone::clone)?;
        to.send(asked).map_err(|_| "the build page's world is gone".to_owned())
    }
}

/// Starts the world's thread. The world is made on it, so a world that cannot be made is said
/// back before the thread waits for anything.
fn start() -> Result<mpsc::Sender<Asked>, String> {
    let (to, asked) = mpsc::channel();
    let (made, making) = mpsc::channel();
    std::thread::Builder::new()
        .name("blaze-web-world".into())
        .spawn(move || {
            let world = match World::new() {
                Ok(world) => {
                    let _ = made.send(Ok(()));
                    world
                }
                Err(why) => {
                    let _ = made.send(Err(why));
                    return;
                }
            };
            let mut state = State { world, programs: Vec::new(), shown: None, followers: Vec::new() };
            for asked in asked {
                state.answer(asked);
            }
        })
        .map_err(|e| format!("no thread for the build page's world: {e}"))?;
    making.recv().map_err(|_| "the build page's world did not start".to_owned())??;
    Ok(to)
}

impl State {
    fn answer(&mut self, asked: Asked) {
        match asked {
            Asked::Record { root, build, record } => self.record(&root, build, &record),
            Asked::Follow(to) => {
                let _ = to.send(self.follow());
            }
            Asked::Rest { method, path, answer } => {
                let _ = answer.send(self.world.rest(&method, &path));
            }
        }
    }

    fn record(&mut self, root: &Path, build: u64, record: &Record) {
        let at = match self.programs.iter().position(|it| it.root() == root) {
            Some(at) => at,
            None => {
                if self.programs.len() >= KEPT {
                    let oldest = (0..self.programs.len())
                        .filter(|&it| Some(it) != self.shown)
                        .min_by_key(|&it| self.programs[it].build());
                    if let Some(oldest) = oldest {
                        let program = self.programs.remove(oldest);
                        self.world.forget(program);
                        self.shown = self.shown.map(|it| if it > oldest { it - 1 } else { it });
                    }
                }
                let program = self.world.program(root);
                self.programs.push(program);
                self.programs.len() - 1
            }
        };
        let began = matches!(record.event, Event::Began { .. });
        let applied = self.world.apply(&mut self.programs[at], build, record);
        let moved = began && self.shown != Some(at);
        if moved {
            self.shown = Some(at);
        }
        if self.shown != Some(at) || self.followers.is_empty() {
            return;
        }
        let message: Arc<[u8]> = if moved || applied == Applied::Anew {
            match self.snapshot(at) {
                Some(it) => it,
                None => return,
            }
        } else {
            match blaze_protocol::cbor::to_vec(record) {
                Ok(item) => item.into(),
                Err(_) => return,
            }
        };
        self.followers.retain(|it| it.send(message.clone()));
    }

    fn follow(&mut self) -> Following {
        let (to, told) = mpsc::channel();
        let follower = Follower { to, waiting: Arc::default() };
        let waiting = follower.waiting.clone();
        let at = match self.shown {
            Some(at) => at,
            None => {
                self.programs.push(self.world.program(Path::new("")));
                self.programs.len() - 1
            }
        };
        if let Some(snapshot) = self.snapshot(at) {
            follower.send(snapshot);
        }
        for record in self.programs[at].records() {
            if let Ok(item) = blaze_protocol::cbor::to_vec(record) {
                follower.send(item.into());
            }
        }
        if self.shown.is_none() {
            let program = self.programs.remove(at);
            self.world.forget(program);
        }
        self.followers.push(follower);
        Following { told, waiting }
    }

    /// The snapshot of program `at`, gzipped (docs/web.md, "The snapshot").
    fn snapshot(&self, at: usize) -> Option<Arc<[u8]>> {
        let snapshot = self.world.snapshot(&self.programs[at]);
        let mut gzip = flate2::write::GzEncoder::new(
            Vec::with_capacity(snapshot.len() / 8),
            flate2::Compression::fast(),
        );
        gzip.write_all(&snapshot).ok()?;
        Some(gzip.finish().ok()?.into())
    }
}

impl Follower {
    /// Whether the page is still there and not too far behind to send it `message`.
    fn send(&self, message: Arc<[u8]>) -> bool {
        if self.waiting.fetch_add(1, Ordering::Relaxed) >= BEHIND {
            return false;
        }
        self.to.send(message).is_ok()
    }
}

/// The page's server, once started.
pub struct Served {
    /// Where the page is, its token in it.
    pub url: String,
}

/// Serves the page of `graphs` on a port of 127.0.0.1 the system picks, on threads of its
/// own, until the process exits. `asked` is called for each request: the daemon is in use.
pub fn serve(
    graphs: Arc<Graphs>,
    asked: Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<Served> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let mut token = [0u8; 16];
    getrandom::fill(&mut token).map_err(|error| std::io::Error::other(error.to_string()))?;
    let token: String = token.iter().map(|it| format!("{it:02x}")).collect();
    let url = format!("http://127.0.0.1:{port}/{token}/");
    let site = Arc::new(http::Site { port, token, graphs });
    std::thread::Builder::new().name("blaze-web".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            asked();
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

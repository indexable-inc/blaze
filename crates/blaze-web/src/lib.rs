//! The build page (docs/web.md): the daemon keeps the graph of each workspace's functions
//! from its builds' records ([`Graphs`]), and once asked serves a page of it on 127.0.0.1
//! ([`serve`]), with the records of every build over a websocket as they come.

mod graph;
mod http;
#[cfg(test)]
mod tests;

use std::{
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};

use blaze_protocol::events::{Event, Record};

pub use graph::{Applied, Graph};

/// How many workspaces' graphs the daemon keeps.
const KEPT: usize = 16;

/// The most messages a page may have waiting to be sent: one that falls this far behind is
/// hung up on, and starts over from a snapshot when it connects again.
const BEHIND: usize = 1 << 16;

/// The graphs of the workspaces built last, and the pages that follow them.
#[derive(Default)]
pub struct Graphs {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    graphs: Vec<Graph>,
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
    /// The graphs hung up on it: it fell too far behind.
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
    /// Takes in `record` of build `build` of the workspace at `root`, and tells the pages.
    pub fn record(&self, root: &Path, build: u64, record: &Record) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let at = match state.graphs.iter().position(|it| it.root() == root) {
            Some(at) => at,
            None => {
                if state.graphs.len() >= KEPT {
                    let oldest = (0..state.graphs.len())
                        .filter(|&it| Some(it) != state.shown)
                        .min_by_key(|&it| state.graphs[it].build());
                    if let Some(oldest) = oldest {
                        state.graphs.remove(oldest);
                        state.shown = state.shown.map(|it| if it > oldest { it - 1 } else { it });
                    }
                }
                state.graphs.push(Graph::new(root));
                state.graphs.len() - 1
            }
        };
        let began = matches!(record.event, Event::Began { .. });
        let applied = state.graphs[at].apply(build, record);
        let moved = began && state.shown != Some(at);
        if moved {
            state.shown = Some(at);
        }
        if state.shown != Some(at) || state.followers.is_empty() {
            return;
        }
        let message: Arc<[u8]> = if moved || applied == Applied::Anew {
            state.graphs[at].snapshot().into()
        } else {
            match blaze_protocol::cbor::to_vec(record) {
                Ok(item) => item.into(),
                Err(_) => return,
            }
        };
        state.followers.retain(|it| it.send(message.clone()));
    }

    /// The messages a page is sent from now on: the snapshot of the workspace shown, its last
    /// build's records, and every one after as it comes.
    pub fn follow(&self) -> Following {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (to, told) = mpsc::channel();
        let follower = Follower { to, waiting: Arc::default() };
        let waiting = follower.waiting.clone();
        let empty = Graph::new(Path::new(""));
        let graph = state.shown.map_or(&empty, |at| &state.graphs[at]);
        follower.send(graph.snapshot().into());
        for record in graph.records() {
            if let Ok(item) = blaze_protocol::cbor::to_vec(record) {
                follower.send(item.into());
            }
        }
        state.followers.push(follower);
        Following { told, waiting }
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
    getrandom::fill(&mut token).map_err(std::io::Error::other)?;
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

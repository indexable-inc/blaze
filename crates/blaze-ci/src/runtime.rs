//! Runs the [`Service`] with threads and channels:
//!
//! - the webhook listener ([`listen`]): deliveries checked, deduplicated and handed on;
//! - the pin connection ([`pins`]): one long-lived connection to the daemon holding every
//!   checkout's root open;
//! - a worker per job ([`worker`]): the checkout (fetch, diff, the what-if, the head), then the
//!   job's requests on a connection of its own, so cancelling one is a
//!   [`blaze_api::Request::Cancel`] on it;
//! - [`POST_LANES`] GitHub threads posting check runs, each on its connection and each run on
//!   one of them, retrying with backoff;
//! - the service's thread, the one [`serve`] runs on.
//!
//! The daemon is reached on its Unix socket only; nothing here writes the store or passes it a
//! secret. The installation token only fetches into the VM's clone.

mod listen;
mod pins;
mod worker;

#[cfg(test)]
mod tests;

use std::{
    convert::Infallible,
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use blaze_api::{ApiError, Request, Response, ViewRef, client::Client};
use blaze_hash::HashMap;
use blaze_vfs::{Op, View};
use parking_lot::Mutex;

use crate::{
    CheckRun,
    checkout::Git,
    config::{Config, Secret, SnapshotConfig},
    github::{App, Checks, GitHub, GitHubError, Key},
    service::{Input, JobId, Output, RunId, Service, Settings, State},
    snapshot,
};

/// The most bytes of ops one request carries, well under the daemon's
/// [`blaze_api::LONGEST_REQUEST`]; a larger change goes in several.
const MOST_BATCH: usize = 8 << 20;

/// What an op costs in a request besides its paths and bytes.
const OP_OVERHEAD: usize = 64;

/// How many times a GitHub call is tried again, waiting 1, 2, 4, 8 and 16 seconds.
const GITHUB_RETRIES: u32 = 5;

/// The head whose root is the empty tree: never written.
pub const EMPTY_HEAD: &str = "ci/empty";

/// What every thread shares.
pub(crate) struct Shared {
    pub(crate) socket: PathBuf,
    pub(crate) git: Mutex<Git>,
    pub(crate) github: Arc<Mutex<GitHub>>,
    pub(crate) pins: crossbeam_channel::Sender<pins::Command>,
    pub(crate) service: crossbeam_channel::Sender<Input>,
}

/// Runs the service until it can't: returns only with why it stopped.
pub fn serve(config: &Config) -> Result<Infallible, String> {
    let key = Secret::read(&config.app.key_file)
        .and_then(|it| it.text(&config.app.key_file))
        .map_err(|error| error.what)?;
    // alloc: error path
    let key = Key::from_pem(&key).map_err(|error| format!("{}: {error}", config.app.key_file.display()))?;
    // clone: once, at start, the App's host
    let app = App { id: config.app.id, installation: config.app.installation, key, api: config.app.api.clone() };
    // clone: once, at start; the error is an error path
    let github = GitHub::new(app, config.repo.clone()).map_err(|error| error.to_string())?;
    let secret = Secret::read(&config.webhook.secret_file).map_err(|error| error.what)?;
    let state = load(&config.state)?;
    let listener = TcpListener::bind(config.webhook.listen)
        // alloc: error path
        .map_err(|error| format!("the webhook could not listen on {}: {error}", config.webhook.listen))?;

    let (to_service, inputs) = crossbeam_channel::unbounded();
    let (to_pins, pin_commands) = crossbeam_channel::unbounded();
    let mut lanes = Vec::with_capacity(POST_LANES);
    for lane in 0..POST_LANES {
        let (to_lane, posts) = crossbeam_channel::unbounded();
        // alloc: clone: a lane's client and sender, once at start
        let (client, service) = (Arc::new(Mutex::new(github.lane())), to_service.clone());
        thread::Builder::new()
            .name(format!("blaze-ci github {lane}")) // alloc: once per lane, at start
            .spawn(move || post(client, posts, service, Duration::from_secs(1)))
            // alloc: a failed start
            .map_err(|error| format!("GitHub thread {lane} did not start: {error}"))?;
        lanes.push(to_lane);
    }
    let to_github = Posts { lanes };
    let shared = Arc::new(Shared { // alloc: once, at start, what every worker shares
        socket: config.daemon.socket.clone(), // clone: once, at start
        // clone: once, at start
        git: Mutex::new(Git { dir: config.git.dir.clone(), remote: config.git.remote.clone() }),
        github: Arc::new(Mutex::new(github)), // alloc: once, at start
        pins: to_pins,
        service: to_service.clone(), // clone: once, at start
    });

    {
        let socket = config.daemon.socket.clone(); // clone: once, at start, the pin thread's socket
        let service = to_service.clone(); // clone: once, at start, the pin thread's way back
        thread::Builder::new()
            .name("blaze-ci pins".to_owned()) // alloc: once, at start, the thread's name
            .spawn(move || pins::run(socket, pin_commands, service))
            // alloc: error path
            .map_err(|error| format!("the pin thread did not start: {error}"))?;
    }
    {
        let service = to_service.clone(); // clone: once, at start, the webhook's way to the service
        let repo = config.repo.clone(); // clone: once, at start, the webhook's repo
        thread::Builder::new()
            .name("blaze-ci webhook".to_owned()) // alloc: once, at start, the thread's name
            // alloc: once, at start, the secret the webhook keeps
            .spawn(move || listen::run(listener, Arc::new(secret), repo, service))
            // alloc: error path
            .map_err(|error| format!("the webhook thread did not start: {error}"))?;
    }
    log(&format!( // alloc: once, at start, the line saying what is served
        "serving {} on {}, the daemon at {}",
        config.repo,
        config.webhook.listen,
        config.daemon.socket.display()
    ));

    let mut out = Vec::new(); // alloc: once, at start, the outputs reused for every step
    let mut service = Service::new(Settings::of(config), state, &mut out);
    let mut workers = HashMap::default();
    loop {
        let mut changed = false;
        for output in out.drain(..) {
            dispatch(config, &shared, &to_github, &mut workers, &mut changed, output);
        }
        if changed && let Err(what) = save(&config.state, service.state()) {
            log(&what);
        }
        // alloc: error path
        let input = inputs.recv().map_err(|_| "every thread feeding the service ended".to_owned())?;
        let now = now();
        service.step(now, input, &mut out);
        for input in inputs.try_iter().take(MOST_STEPS - 1) {
            service.step(now, input, &mut out);
        }
    }
}

/// The most inputs the service takes before doing what they said: a burst of responses is
/// stepped through at once, and the state it changed is written once (one fsync), not once each.
const MOST_STEPS: usize = 1024;

/// A post to GitHub.
pub(crate) enum Post {
    Create { run: RunId, check: CheckRun },
    Update { run: RunId, id: u64, check: CheckRun },
}

impl Post {
    fn run(&self) -> RunId {
        match self {
            Post::Create { run, .. } | Post::Update { run, .. } => *run,
        }
    }
}

/// How many connections check runs post on at once. GitHub has no call that creates or updates
/// several check runs, so the runs of a job that change together are that many requests in
/// flight, not that many one after another; well under GitHub's 100 concurrent requests.
pub(crate) const POST_LANES: usize = 8;

/// The GitHub lanes, each a thread running [`post`] on a connection of its own. A run's posts
/// all go to one lane, so they are sent in order and a superseded update is still dropped.
pub(crate) struct Posts {
    pub(crate) lanes: Vec<crossbeam_channel::Sender<Post>>,
}

impl Posts {
    pub(crate) fn send(&self, post: Post) {
        let lane = (post.run().0 % self.lanes.len() as u64) as usize;
        let _ = self.lanes[lane].send(post);
    }
}

/// Does `output`; [`Output::Save`] only sets `changed`, so the state is written once for many.
fn dispatch(
    config: &Config,
    shared: &Arc<Shared>,
    to_github: &Posts,
    workers: &mut HashMap<JobId, crossbeam_channel::Sender<worker::Command>>,
    changed: &mut bool,
    output: Output,
) {
    match output {
        Output::Pin { root } => {
            let _ = shared.pins.send(pins::Command::Open { view: ViewRef::Root(View { root }), reply: None });
        }
        Output::Unpin { root } => {
            let _ = shared.pins.send(pins::Command::Close { root });
        }
        Output::Checkout { job, commit, fetch, from, head } => {
            // clone: once per job, a count bumped
            let commands = worker::start(job, shared.clone());
            let _ = commands.send(worker::Command::Checkout { commit, fetch, from, head });
            workers.insert(job, commands);
        }
        Output::Send { job, run, request } => {
            let sent = workers.get(&job).is_some_and(|commands| {
                commands.send(worker::Command::Send { run, request }).is_ok()
            });
            if !sent {
                // alloc: error path
                let what = format!("the worker of job {} is gone, so its request was not sent", job.0);
                let error = Response::Error(ApiError::new(what, None));
                let _ = shared.service.send(Input::Response { job, run, response: error });
                let _ = shared.service.send(Input::Ended { job, run, ok: false });
            }
        }
        Output::Cancel { job } => {
            if let Some(commands) = workers.get(&job) {
                let _ = commands.send(worker::Command::Cancel);
            }
        }
        Output::EndJob { job } => {
            if let Some(commands) = workers.remove(&job) {
                let _ = commands.send(worker::Command::End);
            }
        }
        Output::Create { run, check, .. } => to_github.send(Post::Create { run, check }),
        Output::Update { run, id, check } => to_github.send(Post::Update { run, id, check }),
        Output::Save => *changed = true,
        Output::Snapshot { name } => {
            // clone: once per snapshot, its thread's socket
            let socket = config.daemon.socket.clone();
            let snapshot = config.snapshot.clone(); // clone: once per snapshot, its thread's argv
            let service = shared.service.clone(); // clone: once per snapshot, its thread's way back
            thread::spawn(move || {
                let _ = service.send(Input::Snapshotted(take_snapshot(&socket, &snapshot, &name)));
            });
        }
        Output::RemoveSnapshot { id } => {
            // clone: once per snapshot removed, its thread's argv
            let snapshot = config.snapshot.clone();
            // clone: once per snapshot removed, its thread's way back
            let service = shared.service.clone();
            thread::spawn(move || {
                let result = snapshot::remove(&snapshot, &id).map_err(|error| error.what);
                let _ = service.send(Input::Removed { id, result });
            });
        }
        Output::Log(line) => log(&line),
    }
}

/// The daemon's state made durable, then the VM's snapshot taken.
fn take_snapshot(socket: &Path, config: &SnapshotConfig, name: &str) -> Result<snapshot::Kept, String> {
    let mut client = Client::unix(socket)
        // alloc: error path
        .map_err(|error| format!("could not reach the daemon at {}: {error}", socket.display()))?;
    let mut durable = false;
    let mut said = Vec::new(); // alloc: none until the daemon says an error
    let ok = client
        .call(Request::Snapshot, |response| match response {
            Response::Snapshotted { .. } => durable = true,
            Response::Error(error) => said.push(api_error(error)),
            _ => {}
        })
        // alloc: error path
        .map_err(|error| format!("asking the daemon at {} to snapshot: {error}", socket.display()))?;
    if !(ok && durable) {
        return Err(format!( // alloc: error path
            "the daemon at {} did not make its state durable: {}",
            socket.display(),
            said_or(&said, "it said nothing more")
        ));
    }
    snapshot::take(config, name, now()).map_err(|error| error.what)
}

/// Posts check runs in order, each tried [`GITHUB_RETRIES`] times more with backoff from
/// `backoff`; a run that never posts is logged by name.
///
/// Whatever is waiting is taken at once, and an update with a later update of the same check
/// run behind it is never sent: an update carries the run's whole state, so the later one says
/// all of it. A run that goes queued, in progress and done while an earlier post is retried, or
/// while its creation is answered, costs one request, not three.
pub(crate) fn post(
    github: Arc<Mutex<impl Checks>>,
    posts: crossbeam_channel::Receiver<Post>,
    service: crossbeam_channel::Sender<Input>,
    backoff: Duration,
) {
    let mut waiting: Vec<Post> = Vec::new(); // alloc: once per lane, reused for every post
    let mut last: HashMap<u64, usize> = HashMap::default();
    while let Ok(first) = posts.recv() {
        waiting.push(first);
        waiting.extend(posts.try_iter());
        last.clear();
        for (at, post) in waiting.iter().enumerate() {
            if let Post::Update { id, .. } = post {
                last.insert(*id, at);
            }
        }
        for (at, post) in waiting.drain(..).enumerate() {
            if let Post::Update { id, .. } = &post
                && last.get(id) != Some(&at)
            {
                continue;
            }
            send(&*github, post, &service, backoff);
        }
    }
}

/// One post, retried; a creation's id goes back to the service.
fn send(
    github: &Mutex<impl Checks>,
    post: Post,
    service: &crossbeam_channel::Sender<Input>,
    backoff: Duration,
) {
    match post {
        Post::Create { run, check } => {
            let result = retry(backoff, || github.lock().create(&check));
            let result = result.map_err(|error| {
                let what = format!( // alloc: error path
                    "creating check run {} on {} failed {} times: {error}",
                    check.name,
                    check.commit,
                    GITHUB_RETRIES + 1
                );
                log(&what);
                what
            });
            let _ = service.send(Input::Created { run, result });
        }
        Post::Update { run, id, check } => {
            let result = retry(backoff, || github.lock().update(id, &check));
            if let Err(error) = result {
                log(&format!( // alloc: error path
                    "updating check run {} ({id}, run {}) on {} to {:?} failed {} times: {error}",
                    check.name,
                    run.0,
                    check.commit,
                    check.status,
                    GITHUB_RETRIES + 1
                ));
            }
        }
    }
}

fn retry<T>(backoff: Duration, mut attempt: impl FnMut() -> Result<T, GitHubError>) -> Result<T, GitHubError> {
    for tried in 0..GITHUB_RETRIES {
        match attempt() {
            Ok(it) => return Ok(it),
            // GitHub's answer says nothing of when a retry would pass.
            Err(_) => crate::hold(waited(backoff, tried)),
        }
    }
    attempt()
}

/// The wait after failed try `tried` (from 0) of a post: `backoff`, doubled each time.
pub(crate) fn waited(backoff: Duration, tried: u32) -> Duration {
    backoff.saturating_mul(1 << tried)
}

/// `ops` cut into requests of at most `most` bytes each, in order; always at least one, maybe
/// empty. Each op is moved into its batch, never copied. A file too large for one is written in
/// pieces: a [`Op::Write`] of its start, which keeps the file's own buffer, then [`Op::Edit`]s
/// appending the rest, the only bytes copied.
pub(crate) fn batches(ops: Vec<Op>, most: usize) -> Result<Vec<Vec<Op>>, String> {
    let mut batches = vec![Vec::new()]; // alloc: once per checkout, its request batches
    let mut size = 0;
    let mut add = |op: Op, cost: usize, batches: &mut Vec<Vec<Op>>| {
        if size + cost > most && batches.last().is_some_and(|it| !it.is_empty()) {
            batches.push(Vec::new()); // alloc: once per batch past the first
            size = 0;
        }
        size += cost;
        batches.last_mut().expect("one at least").push(op);
    };
    for op in ops {
        let cost = match &op {
            Op::Write { path, bytes, .. } | Op::Edit { path, bytes, .. } => path.len() + bytes.len() + OP_OVERHEAD,
            Op::Delete { path } => path.len() + OP_OVERHEAD,
            Op::Rename { from, to } => from.len() + to.len() + OP_OVERHEAD,
        };
        match op {
            Op::Write { path, mut bytes, mode } if cost > most => {
                let piece = most.checked_sub(path.len() + OP_OVERHEAD).filter(|it| *it > 0).ok_or_else(|| {
                    // alloc: error path
                    format!("the path {path} is too long for a request of the daemon's")
                })?;
                let first = piece.min(bytes.len());
                let mut at = first as u64;
                let edits: Vec<Op> = bytes[first..]
                    .chunks(piece)
                    .map(|piece| {
                        // alloc: once per piece of a file too big for one request
                        let edit = Op::Edit { path: path.clone(), at, len: 0, bytes: piece.to_vec() };
                        at += piece.len() as u64;
                        edit
                    })
                    .collect(); // alloc: once per file too big for one request
                bytes.truncate(first);
                add(Op::Write { path, bytes, mode }, most, &mut batches);
                for edit in edits {
                    add(edit, most, &mut batches);
                }
            }
            op => add(op, cost, &mut batches),
        }
    }
    Ok(batches)
}

/// The state file, or the empty state when there is none yet.
pub(crate) fn load(path: &Path) -> Result<State, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            // alloc: error path
            .map_err(|error| format!("the state file {} is not one: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        // alloc: error path
        Err(error) => Err(format!("could not read the state file {}: {error}", path.display())),
    }
}

/// Writes the state file whole or not at all: a temporary file beside it, synced, renamed over.
pub(crate) fn save(path: &Path, state: &State) -> Result<(), String> {
    // dyn: error path, once per failed save
    let failed = |what: &str, error: &dyn std::fmt::Display| {
        // alloc: error path
        format!("could not save the state to {}: {what}: {error}", path.display())
    };
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| failed("encoding it", &error))?;
    let mut temp = path.as_os_str().to_owned(); // alloc: once per save, beside one fsync
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = fs::File::create(&temp).map_err(|error| failed("creating the temporary file", &error))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| failed("writing the temporary file", &error))?;
    drop(file);
    fs::rename(&temp, path).map_err(|error| failed("renaming the temporary file over it", &error))?;
    if let Some(dir) = path.parent().filter(|it| !it.as_os_str().is_empty()) {
        fs::File::open(dir)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| failed("syncing its directory so the rename lasts", &error))?;
    }
    Ok(())
}

pub(crate) fn api_error(error: &ApiError) -> String {
    match &error.at {
        Some(at) => format!("{} (at {at})", error.what), // alloc: error path
        None => error.what.clone(), // alloc: error path
    }
}

pub(crate) fn said_or(said: &[String], nothing: &str) -> String {
    if said.is_empty() { nothing.to_owned() } else { said.join("; ") } // alloc: error path
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |it| it.as_secs())
}

#[allow(clippy::print_stderr)]
pub(crate) fn log(line: &str) {
    eprintln!("blaze-ci: {line}");
}

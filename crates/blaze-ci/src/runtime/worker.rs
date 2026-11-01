//! A job's worker: its checkout, then its requests on a connection of its own to the daemon.
//!
//! The connection is a Unix stream split in two: this thread writes requests (and
//! [`Request::Cancel`]s, at any time), a reader thread cuts the responses into frames and hands
//! them back here. A request ends with its `Done`; when the daemon can't be reached or hangs
//! up, each request still running ends with an error naming the socket.

use std::{
    io::{self, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::Arc,
    thread,
};

use blaze_api::{
    ApiError, Envelope, Request, Response, ViewRef,
    client::Client,
    frames::{self, Frames},
};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, radix};
use blaze_protocol::cbor;
use blaze_vfs::{Op, View};

use super::{EMPTY_HEAD, MOST_BATCH, Shared, api_error, batches, pins, said_or};
use crate::{
    Checkout,
    service::{Input, JobId, RunId},
};

pub(crate) enum Command {
    Checkout { commit: String, fetch: Vec<String>, from: Option<Checkout>, head: Option<String> },
    Send { run: RunId, request: Request },
    /// Cancel every request still running.
    Cancel,
    /// From the reader.
    Received(Envelope<Response>),
    /// From the reader: the connection ended.
    Lost(String),
    End,
}

/// Starts the job's worker; it ends at [`Command::End`].
pub(crate) fn start(job: JobId, shared: Arc<Shared>) -> crossbeam_channel::Sender<Command> {
    let (commands, receive) = crossbeam_channel::unbounded();
    let me = commands.clone(); // clone: once per job, the reader's way back
    thread::spawn(move || Worker::new(job, shared, me).run(&receive));
    commands
}

struct Worker {
    job: JobId,
    shared: Arc<Shared>,
    me: crossbeam_channel::Sender<Command>,
    /// The writing half of the job's connection.
    stream: Option<UnixStream>,
    /// Ids increase over every connection the job makes, so a late frame of an old one can't be
    /// taken for a new request's.
    next_id: u64,
    /// Sorted by id where it is walked, so cancels and failures go out in request order.
    running: HashMap<u64, RunId>,
    /// Each request is encoded here, then written: one buffer for the job's every request.
    out: Outgoing,
    /// The ids being cancelled or failed, sorted: kept from one cancel to the next.
    ids: Vec<(u64, RunId)>,
}

impl Worker {
    fn new(job: JobId, shared: Arc<Shared>, me: crossbeam_channel::Sender<Command>) -> Worker {
        let running = HashMap::default();
        // alloc: once per job
        Worker { job, shared, me, stream: None, next_id: 1, running, out: Outgoing::default(), ids: Vec::new() }
    }

    fn run(mut self, commands: &crossbeam_channel::Receiver<Command>) {
        while let Ok(command) = commands.recv() {
            match command {
                Command::Checkout { commit, fetch, from, head } => {
                    let result = checkout(&self.shared, &commit, &fetch, from.as_ref(), head.as_deref());
                    let _ = self.shared.service.send(Input::CheckedOut { job: self.job, result });
                }
                Command::Send { run, request } => match self.send(request) {
                    Ok(id) => {
                        self.running.insert(id, run);
                    }
                    Err(what) => self.fail(run, what),
                },
                Command::Cancel => {
                    self.sorted_running();
                    for at in 0..self.ids.len() {
                        let id = self.ids[at].0;
                        // A connection that broke is reported by the reader as `Lost`.
                        let _ = self.send(Request::Cancel { id });
                    }
                }
                Command::Received(envelope) => self.received(envelope),
                Command::Lost(why) => {
                    self.stream = None;
                    self.sorted_running();
                    self.running.clear();
                    for at in 0..self.ids.len() {
                        // clone: error path, once per run lost
                        self.fail(self.ids[at].1, why.clone());
                    }
                }
                Command::End => break,
            }
        }
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// A response handed to the service under its run, or the run's end at its `Done`: no
    /// allocation but the service channel's, a block for every 31 sends.
    fn received(&mut self, envelope: Envelope<Response>) {
        let Some(&run) = self.running.get(&envelope.id) else { return };
        let job = self.job;
        match envelope.body {
            Response::Done { ok } => {
                self.running.remove(&envelope.id);
                let _ = self.shared.service.send(Input::Ended { job, run, ok });
            }
            response => {
                let _ = self.shared.service.send(Input::Response { job, run, response });
            }
        }
    }

    /// `ids` as the requests running, in the order they were sent.
    fn sorted_running(&mut self) {
        self.ids.clear();
        self.ids.extend(self.running.iter().map(|(&id, &run)| (id, run)));
        radix::sort_copy_by_key(&mut self.ids, |&(id, _)| id);
    }

    /// Ends `run`'s request with `what`, at the daemon's socket.
    fn fail(&self, run: RunId, what: String) {
        let at = Some(self.shared.socket.display().to_string()); // alloc: error path
        let response = Response::Error(ApiError::new(what, at));
        let _ = self.shared.service.send(Input::Response { job: self.job, run, response });
        let _ = self.shared.service.send(Input::Ended { job: self.job, run, ok: false });
    }

    /// Sends `request` on the job's connection, made the first time: encoded into the job's one
    /// buffer and written whole. The socket's name is only written out for an error.
    fn send(&mut self, request: Request) -> Result<u64, String> {
        let socket = self.shared.socket.display();
        if self.stream.is_none() {
            let stream = UnixStream::connect(&self.shared.socket)
                // alloc: error path
                .map_err(|error| format!("could not reach the daemon at {socket}: {error}"))?;
            let reader = stream
                .try_clone()
                // alloc: error path
                .map_err(|error| format!("the connection to the daemon at {socket}: {error}"))?;
            let me = self.me.clone(); // clone: once per connection, its reader's
            let named = socket.to_string(); // alloc: once per connection, its reader's
            thread::spawn(move || read(reader, &named, &me));
            self.stream = Some(stream);
        }
        let id = self.next_id;
        self.next_id += 1;
        let stream = self.stream.as_mut().expect("connected");
        match self.out.send(&mut *stream, id, request) {
            Ok(()) => Ok(id),
            Err(Sent::Encoding(error)) => Err(api_error(&error)),
            Err(Sent::Writing(error)) => {
                let _ = stream.shutdown(Shutdown::Both);
                self.stream = None;
                // alloc: error path
                Err(format!("sending to the daemon at {socket} failed: {error}"))
            }
        }
    }
}

/// The frame a request is encoded into and the encoder's own scratch, kept from one request to
/// the next, so a warm request allocates nothing.
#[derive(Default)]
pub(crate) struct Outgoing {
    writer: cbor::Writer,
    frame: Vec<u8>,
}

/// Why a request was not sent.
pub(crate) enum Sent {
    Encoding(ApiError),
    Writing(io::Error),
}

impl Outgoing {
    /// `request` under `id`, framed and written whole to `to`.
    pub(crate) fn send(&mut self, to: &mut impl Write, id: u64, request: Request) -> Result<(), Sent> {
        self.frame.clear();
        frames::encode_into(&mut self.writer, &mut self.frame, &Envelope::new(id, request)).map_err(Sent::Encoding)?;
        to.write_all(&self.frame).map_err(Sent::Writing)
    }
}

/// Reads the connection's responses until it ends.
fn read(mut stream: UnixStream, socket: &str, to: &crossbeam_channel::Sender<Command>) {
    let mut frames = Frames::new();
    loop {
        let most = frames.wanted(64 << 10);
        match frames.fill(&mut stream, most) {
            Ok(0) => {
                // alloc: error path
                let _ = to.send(Command::Lost(format!("the daemon at {socket} hung up")));
                return;
            }
            Ok(_) => {}
            Err(error) => {
                // alloc: error path
                let _ = to.send(Command::Lost(format!("reading from the daemon at {socket} failed: {error}")));
                return;
            }
        }
        match drain(&mut frames, |envelope| to.send(Command::Received(envelope)).is_ok()) {
            Ok(true) => {}
            Ok(false) => return,
            Err(said) => {
                let _ = stream.shutdown(Shutdown::Both);
                // alloc: error path
                let _ = to.send(Command::Lost(format!("the daemon at {socket} sent {said}")));
                return;
            }
        }
    }
}

/// Every whole frame `frames` holds, decoded in place and handed to `each` in order: `Ok(false)`
/// once `each` refuses one (the worker is gone), an error for a frame that is not a response.
/// Each response owns only what it carries, and is handed on: a `Done` allocates nothing.
pub(crate) fn drain(
    frames: &mut Frames,
    mut each: impl FnMut(Envelope<Response>) -> bool,
) -> Result<bool, String> {
    loop {
        match frames.next_ref() {
            Ok(Some(item)) => match frames::decode::<Response>(item) {
                Ok(envelope) => {
                    if !each(envelope) {
                        return Ok(false);
                    }
                }
                Err(refused) => return Err(api_error(&refused.error)),
            },
            Ok(None) => return Ok(true),
            Err(error) => return Err(api_error(&error)),
        }
    }
}

/// The commit's tree as a root held open on the pin connection: fetched, diffed from `from`
/// (the empty tree for `None`), applied to `from`'s root as what-ifs; with `head`, the same
/// ops committed to that head, which must reach the same root.
///
/// The what-ifs run on a connection of the checkout's own, so checkouts of many jobs apply at
/// once; only the result goes through the pin connection, as one cheap open of a root the
/// daemon already has. Git's lock covers only the fetch: reading objects while another fetch
/// writes packs is safe in git.
fn checkout(
    shared: &Shared,
    commit: &str,
    fetch: &[String],
    from: Option<&Checkout>,
    head: Option<&str>,
) -> Result<ContentId, String> {
    let same = from.is_some_and(|from| from.commit == commit);
    // clone: once per checkout, so the diff runs outside the lock
    let git = shared.git.lock().clone();
    let ops = if same {
        Vec::new() // alloc: none, an empty list
    } else {
        let had = git.has(commit).map_err(|error| {
            // alloc: error path
            format!("asking {} whether it has {commit} failed: {error}", git.dir.display())
        })?;
        if !had {
            let token = shared
                .github
                .lock()
                .token()
                // alloc: error path
                .map_err(|error| format!("minting the installation token to fetch {commit} failed: {error}"))?;
            let git = shared.git.lock();
            git.fetch(fetch, Some(&token)).map_err(|error| {
                // alloc: error path
                format!("fetching {} into {} failed: {error}", fetch.join(" "), git.dir.display())
            })?;
        }
        git.ops(from.map(|it| it.commit.as_str()), commit).map_err(|error| {
            // alloc: clone: error path
            let from = from.map_or("the empty tree".to_owned(), |it| it.commit.clone());
            // alloc: error path
            format!("diffing {from} to {commit} in {} failed: {error}", git.dir.display())
        })?
    };
    let mut cut = batches(ops, MOST_BATCH)?;
    let socket = shared.socket.display();
    let mut client = Client::unix(&shared.socket)
        // alloc: error path
        .map_err(|error| format!("could not reach the daemon at {socket} to check out {commit}: {error}"))?;
    let base = match from {
        Some(from) => from.root,
        // alloc: once per checkout from nothing, the request's head
        None => open(&mut client, &shared.socket, ViewRef::Head(EMPTY_HEAD.to_owned()))
            // alloc: error path
            .map_err(|why| format!("opening the empty tree ({EMPTY_HEAD}) failed: {why}"))?,
    };
    let root = apply(&mut client, &shared.socket, base, &mut cut, head.is_some()).map_err(|why| match from {
        // alloc: error path
        Some(from) => format!("applying the diff from {} to root {:x} failed: {why}", from.commit, from.root),
        None => format!("writing the tree of {commit} failed: {why}"), // alloc: error path
    })?;
    // Held on the pin connection before this one ends and lets go of it.
    let root = pin(shared, ViewRef::Root(View { root }))
        // alloc: error path
        .map_err(|why| format!("holding the tree of {commit} (root {root:x}) open failed: {why}"))?;
    if let Some(head) = head
        && let Err(why) = commit_head(&mut client, &shared.socket, head, commit, base, cut, root)
    {
        unpin(shared, root);
        return Err(why);
    }
    Ok(root)
}

/// [`pins::open`] on the checkout's connection.
fn open(client: &mut Client, socket: &std::path::Path, view: ViewRef) -> Result<ContentId, String> {
    // alloc: error path
    pins::open(client, view).map_err(|error| format!("the daemon at {}: {error}", socket.display()))?
}

/// `batches` applied to `base` as what-ifs, one at a time, each result held on `client` until it
/// ends. Each batch is moved into its request, or copied when `keep` (the same ops go on to be
/// committed), one batch at a time so a copy never holds more than one request's bytes.
fn apply(
    client: &mut Client,
    socket: &std::path::Path,
    base: ContentId,
    batches: &mut [Vec<Op>],
    keep: bool,
) -> Result<ContentId, String> {
    let mut at = base;
    for batch in batches.iter_mut().filter(|it| !it.is_empty()) {
        // clone: once per batch of a default branch's checkout, whose ops are also committed
        let ops = if keep { batch.clone() } else { std::mem::take(batch) };
        // alloc: once per batch, the request's own view, which `ViewRef` boxes
        let view = ViewRef::WhatIf { base: Box::new(ViewRef::Root(View { root: at })), ops };
        at = open(client, socket, view)?;
    }
    Ok(at)
}

/// Commits `batches` to `head` from `base`, one at a time, and checks the head reached `root`:
/// one that was not at `base` is rebased to a tree that is not the commit's, an error.
fn commit_head(
    client: &mut Client,
    socket: &std::path::Path,
    head: &str,
    commit: &str,
    base: ContentId,
    batches: Vec<Vec<Op>>,
    root: ContentId,
) -> Result<(), String> {
    let socket = socket.display();
    let mut at = base;
    for batch in batches {
        let mut committed = None;
        let mut said = Vec::new(); // alloc: none until the daemon says an error
        let ok = client
            // alloc: once per batch, the request's head
            .call(Request::Commit { head: head.to_owned(), base: at, ops: batch }, |response| match response {
                Response::Committed { root, .. } => committed = Some(*root),
                Response::Error(error) => said.push(api_error(error)),
                _ => {}
            })
            // alloc: error path
            .map_err(|error| format!("committing {commit} to the head {head} at {socket}: {error}"))?;
        at = match (ok, committed) {
            (true, Some(root)) => root,
            _ => {
                // alloc: error path
                return Err(format!(
                    "committing {commit} to the head {head} failed: {}",
                    said_or(&said, "the daemon said nothing more")
                ));
            }
        };
    }
    if at != root {
        // alloc: error path
        return Err(format!(
            "the head {head} reached root {at:x} and the tree of {commit} is root {root:x}: the head \
             was not at root {base:x}, where the service last left it"
        ));
    }
    Ok(())
}

fn pin(shared: &Shared, view: ViewRef) -> Result<ContentId, String> {
    let (reply, answer) = crossbeam_channel::unbounded();
    shared
        .pins
        .send(pins::Command::Open { view, reply: Some(reply) })
        .map_err(|_| "the pin connection's thread ended".to_owned())?; // alloc: error path
    answer.recv().map_err(|_| "the pin connection's thread ended".to_owned())? // alloc: error path
}

fn unpin(shared: &Shared, root: ContentId) {
    let _ = shared.pins.send(pins::Command::Close { root });
}

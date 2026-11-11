//! A client's worker thread: it owns the connection, turns the JS thread's commands into
//! requests and the daemon's events into posts, and touches nothing of JS but through its
//! [`Poster`].

use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Duration,
};

use blaze_api::{Envelope, json};
use blaze_client::{
    Error, Fault, Next, Waker,
    foreign::{self, Calls, Told},
};
use crossbeam_channel::{Receiver, TryRecvError};

use crate::js::Poster;

/// What the JS thread tells a client's worker.
pub(crate) enum Command {
    Call { call: u64, method: String, options: String },
    Close,
}

/// What a client's worker tells the JS thread.
pub(crate) enum Post {
    Connected(Result<Waker, Error>),
    /// A response of call `call`, as one line of the API's JSON.
    Event { call: u64, json: String },
    /// Call `call` is over: its answer's JSON.
    Ended { call: u64, result: Result<String, Error> },
    /// The worker is gone, nothing more comes: `why` when it panicked.
    Exited { why: Option<String> },
}

/// What a panic said.
pub(crate) fn said(panic: &(dyn Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|it| (*it).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// A client's worker thread: connects as `options` say, then turns commands into requests
/// and events into posts until it is told to close. Its last post, whatever happened, is
/// [`Post::Exited`]: after it the thread calls `poster` no more.
pub(crate) fn work(poster: &Poster, commands: &Receiver<Command>, options: &str) {
    let ran = catch_unwind(AssertUnwindSafe(|| run(poster, commands, options)));
    poster.post(Post::Exited { why: ran.err().map(|panic| said(&*panic)) });
}

fn run(poster: &Poster, commands: &Receiver<Command>, options: &str) {
    let mut client = match foreign::connect(options.as_bytes()) {
        Ok(client) => client,
        Err(error) => return poster.post(Post::Connected(Err(error))),
    };
    match client.waker() {
        Ok(waker) => poster.post(Post::Connected(Ok(waker))),
        Err(error) => {
            let what = format!("making the client's waker: {error}");
            return poster.post(Post::Connected(Err(Error::Fault(Fault::Io { what }))));
        }
    }
    let mut calls = Calls::new();
    // Each request under way and the call it is.
    let mut ids: Vec<(u64, u64)> = Vec::new();
    let (mut over, mut closing) = (false, false);
    // Bounded by its commands: it ends at a close, and waits only where a wake or the
    // daemon's bytes end the wait.
    loop {
        loop {
            match commands.try_recv() {
                Ok(Command::Call { call, method, options }) => {
                    match calls.begin(&mut client, &method, options.as_bytes()) {
                        Ok(id) => ids.push((id, call)),
                        Err(error) => poster.post(Post::Ended { call, result: Err(error) }),
                    }
                }
                Ok(Command::Close) | Err(TryRecvError::Disconnected) => {
                    closing = true;
                    client.shut();
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if over {
            if closing || client.idle(Duration::MAX).is_err() {
                return;
            }
            continue;
        }
        match client.next(Duration::MAX) {
            Ok(Next::Event(event)) => match calls.on(event) {
                Told::Response { id, response } => {
                    if let Some(&(_, call)) = ids.iter().find(|(it, _)| *it == id) {
                        let json = json::response(&Envelope::new(id, response));
                        poster.post(Post::Event { call, json });
                    }
                }
                Told::Ended { id, result } => {
                    if let Some(at) = ids.iter().position(|(it, _)| *it == id) {
                        let (_, call) = ids.swap_remove(at);
                        poster.post(Post::Ended { call, result: result.map(|it| it.json()) });
                    }
                }
                Told::Other(_) => {}
            },
            Ok(Next::Woken | Next::Deadline) => {}
            // The connection is over, and every call under way was told its end before this.
            Err(_) => over = true,
        }
    }
}

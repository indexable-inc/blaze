//! `blaze fetch-url` ([`blaze_api::Request::FetchUrl`]): bytes by their digest, from the
//! daemon's store when it has them, else fetched, checked and kept there by blaze-fetch; then,
//! when the request names a directory (`out`), written out there read-only, named by their
//! BLAKE3. Nothing is written out unasked: the store is the one copy the daemon keeps (the
//! store has no collector of values today, so a kept object stays until the store is
//! removed), and a download's or a checkout's scratch is the fetch's own and removed as it
//! ends.
//!
//! The loop hands each request to a thread of its own, which may wait on the network for
//! minutes, and sends the answers it gets back each turn ([`FetchUrls::take_answers`]); the
//! loop turns at least every 50ms, so an answer waits no longer than that. A panic in a fetch
//! fails that request alone, answered with an error naming it (AGENTS.md rule 20). Two
//! requests of the same digest download once (blaze-fetch's single-flight).

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::Arc,
};

use blaze_api::{ApiError, Reply, RequestRef, Response};
use blaze_fetch::{DEFAULT_MOST_BYTES, Fetcher, StoreOf};
use blaze_registry::{
    NetConfig,
    http::{Http, HyperHttp},
    pinned,
};
use crossbeam_channel::{Receiver, Sender};

const WHAT: &str = "blaze fetch-url";

pub struct FetchUrls {
    send: Sender<Reply>,
    answers: Receiver<Reply>,
}

/// What a request asks for, read from its fields.
pub struct Asked {
    pub url: Option<String>,
    pub repo: Option<String>,
    pub rev: Option<String>,
    pub digest: String,
    /// The most bytes the download may be; [`DEFAULT_MOST_BYTES`] when not given.
    pub most_bytes: Option<u64>,
    /// An absolute directory to write the bytes out in.
    pub out: Option<String>,
}

impl FetchUrls {
    pub fn new() -> FetchUrls {
        let (send, answers) = crossbeam_channel::unbounded();
        FetchUrls { send, answers }
    }

    /// Answers `to` with what `asked` names, once it is fetched: [`Response::Stored`] and a
    /// done, or an error naming the source and why.
    pub fn ask(&self, to: RequestRef, asked: Asked) {
        let send = self.send.clone();
        // alloc: a thread's name, once a request
        let spawned =
            std::thread::Builder::new().name("blaze fetch-url".to_owned()).spawn(move || {
                // The request is the job: a panic fails it with an error, and the daemon and every
                // other fetch go on.
                let answer = catch_unwind(AssertUnwindSafe(|| {
                    let state = super::state()
                        .map_err(|e| format!("the daemon's state directory: {e:#}"))?; // alloc: error path
                    fetch(&state, asked)
                }))
                .unwrap_or_else(|payload| {
                    let panic = super::plane::panicked(&*payload);
                    Err(format!("the fetch {panic}; only this request failed")) // alloc: error path
                });
                answer_with(&send, to, answer);
            });
        if let Err(e) = spawned {
            answer_with(&self.send, to, Err(format!("no thread to fetch on: {e}"))); // alloc: error path
        }
    }

    /// The answers the fetches sent since the last turn, for the loop to send on.
    pub fn take_answers(&self) -> Vec<Reply> {
        self.answers.try_iter().collect() // alloc: a turn's answers, almost always none
    }
}

fn answer_with(send: &Sender<Reply>, to: RequestRef, answer: Result<Response, String>) {
    let (response, ok) = match answer {
        Ok(response) => (response, true),
        Err(what) => (Response::Error(ApiError::new(what, Some(WHAT.to_owned()))), false), // alloc: error path
    };
    // The loop holds the receiver for as long as it runs: a send fails only as it stops.
    _ = send.send(Reply { to, response });
    _ = send.send(Reply { to, response: Response::Done { ok } });
}

/// What `asked` names, fetched into the store and, when it names `out`, written out there:
/// downloads and checkouts go to `fetch-url/scratch` under the state directory `state`, each
/// in a file or directory of its own that is removed as the fetch ends (a checkout's clone
/// with it: there is no cargo home of the daemon's to keep one in).
fn fetch(state: &Path, asked: Asked) -> Result<Response, String> {
    let Asked { url, repo, rev, digest, most_bytes, out } = asked;
    // The same check the CLI ran: a request may come from any client of the socket.
    let blaze_fetch::Asked { source, most_bytes, out } =
        blaze_fetch::request::from_fields(url, repo, rev, &digest, most_bytes, out)?;
    if let Some(out) = &out
        && !Path::new(out).is_absolute()
    {
        return Err(format!("`out` is `{out}`, and the daemon takes only an absolute directory")); // alloc: error path
    }
    // dyn: the I/O driver, as blaze-fetch takes it
    let http: Arc<dyn Http> = HyperHttp::shared().map_err(|e| format!("{e:#}"))?; // alloc: error path
    let net = Arc::new(pinned::Net::new(Arc::clone(&http)));
    let git = Arc::new(pinned::Git::new(NetConfig::default(), http));
    let fetcher = Fetcher::new(StoreOf::Installed, net, state.join("fetch-url").join("scratch"))
        .with_git(git)
        .with_most(most_bytes.unwrap_or(DEFAULT_MOST_BYTES));
    let object = fetcher.get(&source).map_err(|e| e.to_string())?; // alloc: error path
    let path = match out {
        Some(out) => {
            let at = fetcher.materialize(&object, Path::new(&out)).map_err(|e| e.to_string())?; // alloc: error path
            Some(at.display().to_string()) // alloc: the answer, once a request
        }
        None => None,
    };
    Ok(Response::Stored { path, blake3: object.blake3.hex() })
}

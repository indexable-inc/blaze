//! `blaze store export|import|install` while the daemon holds the store
//! ([`blaze_api::Request::Store`]): the same command the CLI runs over a store no daemon
//! holds (`crate::cli::store::run`), run here over the daemon's own. Lane packs-1112.
//!
//! The loop hands each request to a thread of its own, which reads sources for as long as
//! they take, and sends the answers it gets back each turn ([`Packings::take_answers`]), as
//! `fetch_url` does. The thread reaches the store two ways ([`Held`]):
//!
//! - reads and puts through the process's handle (`blaze_cas::Handle::installed`): verified
//!   reads that take no lock, puts through the one writer off the loop, readable at once and
//!   durable by the daemon's next commit;
//! - what only the store's owner can do, asked of the loop and done on its next turn
//!   ([`Packings::turn`], called before the collector's step): a pin grown
//!   (`blaze_cas::Store::pin_add`, once the store's pins are read, which the loop has done
//!   on demand: a daemon that has built nothing answers an import as its first request),
//!   a pin's keys, every key, a flush.
//!
//! **The collector's guard.** An import asks for its pin first and puts its first record only
//! once the loop answered that the pin is in the store. The loop writes the pin before the
//! collector's step of the same turn, and a put the pin names is in the store only after
//! it: at no time can the collector see a record of the pack that is not pinned.
//!
//! Each wait of the thread for the loop is a span naming what it waits for, with a deadline
//! ([`LOOP_WITHIN`]); a loop that does not answer by then fails the command, naming the ask.
//! A panic fails that request alone (AGENTS.md rule 20).

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    time::Duration,
};

use blaze_api::{ApiError, Diagnostic, Reply, RequestRef, Response};
use blaze_cas::{Key, Kind, Store};
use blaze_ids::ContentId;
use crossbeam_channel::{Receiver, Sender};

use super::stored::{self, Records};

const WHAT: &str = "blaze store";
/// How long a command waits for the loop to do what it asked. The loop turns every 50 ms; a
/// pin waits for the store's pins to be read, which the loop asks for on demand and bounds
/// itself (`serve::collect::PINS_BOUND`, 30 s): this is that and room for the turns.
const LOOP_WITHIN: Duration = Duration::from_secs(60);

/// What a command asks of the store's owner.
enum Ask {
    Pin { name: String, keys: Vec<Key> },
    Pinned { name: String },
    Keys,
    Flush,
}

impl Ask {
    fn said(&self) -> &'static str {
        match self {
            Ask::Pin { .. } => "write the pack's pin",
            Ask::Pinned { .. } => "read a pin's keys",
            Ask::Keys => "list the store's keys",
            Ask::Flush => "commit what was put",
        }
    }
}

enum Answered {
    Done(Result<(), String>),
    Pinned(Result<Vec<Key>, Vec<String>>),
    Keys(Vec<Key>),
}

type Asked = (Ask, Sender<Answered>);

pub struct Packings {
    send: Sender<Reply>,
    answers: Receiver<Reply>,
    asks: Sender<Asked>,
    due: Receiver<Asked>,
    /// Asks that wait for the store's pins to be read.
    waiting: Vec<Asked>,
}

impl Packings {
    pub fn new() -> Packings {
        let (send, answers) = crossbeam_channel::unbounded();
        let (asks, due) = crossbeam_channel::unbounded();
        Packings { send, answers, asks, due, waiting: Vec::new() } // alloc: empty, allocates nothing
    }

    /// Runs the command `words` and answers `to` with what it did: a note a line and a done,
    /// or an error naming why.
    pub fn ask(&self, to: RequestRef, words: Vec<String>) {
        let (send, asks) = (self.send.clone(), self.asks.clone());
        // alloc: a thread's name, once a request
        let spawned = std::thread::Builder::new().name("blaze store".to_owned()).spawn(move || {
            let answer = catch_unwind(AssertUnwindSafe(|| {
                let Some(handle) = blaze_cas::Handle::installed() else {
                    return Err("this daemon's store has no handle for a pack command to read and put through".to_owned());
                };
                let words: Vec<&str> = words.iter().map(String::as_str).collect(); // alloc: once a request
                crate::cli::store::run(&words, &mut Held { handle, asks })
            }))
            .unwrap_or_else(|payload| {
                let panic = super::super::plane::panicked(&*payload);
                Err(format!("the command {panic}; only this request failed")) // alloc: error path
            });
            answer_with(&send, to, answer);
        });
        if let Err(e) = spawned {
            answer_with(&self.send, to, Err(format!("no thread to run the command on: {e}"))); // alloc: error path
        }
    }

    /// The answers the commands sent since the last turn, for the loop to send on.
    pub fn take_answers(&self) -> Vec<Reply> {
        self.answers.try_iter().collect() // alloc: a turn's answers, almost always none
    }

    /// Does what the commands asked of the store's owner: called on the loop, before the
    /// collector's step of the turn. A pin is written, and a pin's keys read, only once the
    /// store's pins are read. A daemon that has built nothing has not read them, so an ask
    /// that needs them has them read on demand: by the collector's scan on a worker
    /// (`Collects::read_pins`, bounded by `serve::collect::PINS_BOUND`, asked again each turn
    /// while it reads), or inline in a daemon with no collector. A read that fails is the
    /// ask's error, naming what it waited on.
    pub fn turn(
        &mut self,
        store: &mut Store,
        collects: Option<&mut super::super::collect::Collects>,
        now: Duration,
        effects: &mut Vec<blaze_core::Effect>,
    ) {
        use super::super::collect::PinsRead;
        self.waiting.extend(self.due.try_iter());
        if self.waiting.is_empty() {
            return;
        }
        let _span = tracing::info_span!("pack command asks of the store", asks = self.waiting.len()).entered();
        let needs = self.waiting.iter().any(|(ask, _)| matches!(ask, Ask::Pin { .. } | Ask::Pinned { .. }));
        let read = match (needs, collects) {
            (false, _) => PinsRead::Ready,
            (true, Some(collects)) => collects.read_pins(store, now),
            (true, None) => {
                if !store.pins_read()
                    && let Some(mut scan) = store.pins_take()
                {
                    scan.run();
                    store.pins_put(scan);
                }
                match store.pins_read() {
                    true => PinsRead::Ready,
                    // A commit or an index swap is in flight: asked again next turn, to the
                    // asker's deadline.
                    false => PinsRead::Reading,
                }
            }
        };
        let mut later = Vec::new(); // alloc: empty unless an ask waits for the pins
        for (ask, to) in self.waiting.drain(..) {
            let answer = match ask {
                Ask::Pin { .. } | Ask::Pinned { .. } if !matches!(read, PinsRead::Ready) => {
                    match &read {
                        // clone: an error, once an ask that waited for the pins
                        PinsRead::Failed(why) => _ = to.send(Answered::Done(Err(why.clone()))),
                        _ => later.push((ask, to)),
                    }
                    continue;
                }
                Ask::Pin { name, keys } => Answered::Done(stored::pin_grown(store, &name, &keys)),
                Ask::Pinned { name } => Answered::Pinned(stored::pinned_in(store, &name)),
                Ask::Keys => Answered::Keys(stored::keys_of(store)),
                Ask::Flush => {
                    store.flush(effects);
                    Answered::Done(Ok(()))
                }
            };
            // The asker may have given up at its deadline: then no one reads the answer.
            _ = to.send(answer);
        }
        self.waiting = later;
    }
}

impl Default for Packings {
    fn default() -> Packings {
        Packings::new()
    }
}

fn answer_with(send: &Sender<Reply>, to: RequestRef, answer: Result<String, String>) {
    let ok = answer.is_ok();
    match answer {
        Ok(said) => {
            for line in said.lines() {
                // alloc: a line of the command's answer
                _ = send.send(Reply { to, response: Response::Diagnostic(Diagnostic::note(line.to_owned())) });
            }
        }
        // alloc: error path
        Err(what) => _ = send.send(Reply { to, response: Response::Error(ApiError::new(what, Some(WHAT.to_owned()))) }),
    }
    // The loop holds the receiver for as long as it runs: a send fails only as it stops.
    _ = send.send(Reply { to, response: Response::Done { ok } });
}

/// The daemon's store as a pack command reaches it from its thread.
struct Held {
    handle: &'static blaze_cas::Handle,
    asks: Sender<Asked>,
}

impl Held {
    /// Asks the loop and waits for its answer, in a span naming the ask, to a deadline.
    fn of_loop(&self, ask: Ask) -> Result<Answered, String> {
        let said = ask.said();
        let _span = tracing::info_span!("pack command waits for the daemon's loop", to = said).entered();
        let (to, answered) = crossbeam_channel::bounded(1);
        if self.asks.send((ask, to)).is_err() {
            return Err(format!("the daemon is stopping: it did not {said}")); // alloc: error path
        }
        answered.recv_timeout(LOOP_WITHIN).map_err(|_| {
            // alloc: error path
            format!(
                "the daemon's loop did not {said} within {}s",
                LOOP_WITHIN.as_secs()
            )
        })
    }

    fn done(&self, ask: Ask) -> Result<(), String> {
        match self.of_loop(ask)? {
            Answered::Done(done) => done,
            _ => Err("the daemon's loop answered another ask".to_owned()),
        }
    }
}

impl Records for Held {
    fn content(&mut self, key: &Key) -> Result<Option<ContentId>, String> {
        self.handle.read(key, |entry| entry.content).map_err(|corrupt| corrupt.to_string())
    }

    fn read(&mut self, key: &Key, with: &mut dyn FnMut(Kind, ContentId, &[u8])) -> Result<bool, String> {
        match self.handle.read(key, |entry| with(entry.kind, entry.content, entry.bytes)) {
            Ok(found) => Ok(found.is_some()),
            Err(corrupt) => Err(corrupt.to_string()),
        }
    }

    fn put(&mut self, key: Key, kind: Kind, bytes: &[u8]) -> Result<(), String> {
        match self.handle.put(key, kind, bytes) {
            Ok(_) => Ok(()),
            Err(mismatch) => Err(stored::differs(&mismatch)),
        }
    }

    fn pinned(&mut self, name: &str) -> Result<Result<Vec<Key>, Vec<String>>, String> {
        match self.of_loop(Ask::Pinned { name: name.to_owned() })? {
            Answered::Pinned(pinned) => Ok(pinned),
            Answered::Done(Err(why)) => Err(why),
            _ => Err("the daemon's loop answered another ask".to_owned()),
        }
    }

    fn keys(&mut self) -> Result<Vec<Key>, String> {
        match self.of_loop(Ask::Keys)? {
            Answered::Keys(keys) => Ok(keys),
            _ => Err("the daemon's loop answered another ask".to_owned()),
        }
    }

    fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), String> {
        self.done(Ask::Pin { name: name.to_owned(), keys: keys.to_vec() }) // alloc: once an import
    }

    /// The commit is asked of the loop and under way when this returns; it is the store's
    /// own, as after any build's puts.
    fn flush(&mut self) -> Result<(), String> {
        self.done(Ask::Flush)
    }
}

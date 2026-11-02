//! The webhook's listener, behind the VM's proxy: one request per connection, read on a kept
//! handler thread and checked in place ([`webhook::receive_ref`]), answered 202 or with the
//! refusal's status, deduplicated by delivery id (GitHub redelivers), and its event handed to
//! the service.
//!
//! A warm delivery allocates only the event it hands on: the request is read into the handler's
//! parser, the delivery is lent from it, its id is remembered in a slot reused in place, and the
//! answer is written into the handler's own buffer.

use std::{
    fmt::Write as _,
    hash::BuildHasher,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use blaze_api::http::{self, RequestParser};
use blaze_hash::FixedState;
use hashbrown::HashTable;
use parking_lot::Mutex;

use super::log;
use crate::{
    config::Secret,
    service::Input,
    webhook::{self, DeliveryRef, Verifier},
};

/// How many delivery ids are remembered.
pub(crate) const REMEMBERED: usize = 10_000;

/// How many connections are read at once; more are answered 503.
pub(crate) const MOST_CONNECTIONS: usize = 64;

/// How long a peer may take to send or read.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The room each remembered id's slot is made with: GitHub's ids are 36-byte UUIDs.
const ID_ROOM: usize = 64;

/// What every answer says besides its status and words.
const HEADERS: [(&str, &str); 2] = [("Content-Type", "text/plain; charset=utf-8"), ("Connection", "close")];

/// The last `most` delivery ids, in a ring of strings reused in place: once every slot has held
/// an id, remembering another moves no memory unless it is longer than the slot's room. Each
/// lookup is one hash of the id (ids come only from deliveries whose signature checked) and
/// one compare per matching hash. About 100 bytes an id: its slot's 64 and the table's share.
pub(crate) struct Seen {
    ids: Vec<String>,
    /// The slot the next id goes in once `ids` is full: the oldest's.
    next: usize,
    most: usize,
    /// Slots by their id's hash. Sized for twice `most`, so the tombstones of forgotten ids are
    /// cleared by rehashing in place, never by growing.
    slots: HashTable<u32>,
}

impl Seen {
    pub(crate) fn new(most: usize) -> Seen {
        assert!(most > 0 && u32::try_from(most).is_ok(), "remember between 1 and 2^32 ids, not {most}");
        // alloc: once, at start
        Seen { ids: Vec::with_capacity(most), next: 0, most, slots: HashTable::with_capacity(2 * most) }
    }

    /// Whether `id` is new; it is remembered either way.
    pub(crate) fn first(&mut self, id: &str) -> bool {
        let hash = FixedState.hash_one(id);
        let ids = &self.ids;
        if self.slots.find(hash, |&slot| ids[slot as usize] == id).is_some() {
            return false;
        }
        let slot = if self.ids.len() < self.most {
            // alloc: a slot's string, once for each of the first `most` ids
            let mut kept = String::with_capacity(id.len().max(ID_ROOM));
            kept.push_str(id);
            self.ids.push(kept);
            self.ids.len() - 1
        } else {
            let slot = self.next;
            self.next = (slot + 1) % self.most;
            let oldest = &mut self.ids[slot];
            let old = FixedState.hash_one(oldest.as_str());
            if let Ok(entry) = self.slots.find_entry(old, |&it| it as usize == slot) {
                entry.remove();
            }
            oldest.clear();
            oldest.push_str(id);
            slot
        };
        let ids = &self.ids;
        self.slots.insert_unique(hash, slot as u32, |&it| FixedState.hash_one(ids[it as usize].as_str()));
        true
    }
}

pub(crate) struct Listener {
    pub(crate) verifier: Verifier,
    pub(crate) repo: String,
    pub(crate) service: crossbeam_channel::Sender<Input>,
    pub(crate) seen: Mutex<Seen>,
    open: AtomicUsize,
}

impl Listener {
    pub(crate) fn new(
        secret: Arc<Secret>,
        repo: String,
        service: crossbeam_channel::Sender<Input>,
    ) -> Listener {
        Listener::remembering(secret, repo, service, REMEMBERED)
    }

    /// A listener remembering the last `most` delivery ids.
    pub(crate) fn remembering(
        secret: Arc<Secret>,
        repo: String,
        service: crossbeam_channel::Sender<Input>,
        most: usize,
    ) -> Listener {
        let verifier = Verifier::new(secret.expose());
        Listener { verifier, repo, service, seen: Mutex::new(Seen::new(most)), open: AtomicUsize::new(0) }
    }
}

impl Listener {
    /// Connections taken and not yet answered.
    #[cfg(test)]
    pub(crate) fn open(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }
}

pub(crate) fn run(listener: TcpListener, secret: Arc<Secret>, repo: String, service: crossbeam_channel::Sender<Input>) {
    // alloc: once, at start
    serve(listener, Arc::new(Listener::new(secret, repo, service)));
}

/// Accepts connections and hands each to a handler thread. Handlers are started only when
/// every one is busy, up to [`MOST_CONNECTIONS`], and then kept: a delivery costs no thread
/// start (a stack mapped, a clone, an exit), only a channel hand-off to a parked handler.
/// `open` counts connections taken and not yet answered, and there are never fewer handlers
/// than that, so a connection handed off always has a handler free for it.
pub(crate) fn serve(listener: TcpListener, shared: Arc<Listener>) {
    let (to_handlers, streams) = crossbeam_channel::unbounded::<TcpStream>();
    let mut handlers = 0;
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                log(&format!("the webhook's accept failed: {error}")); // alloc: error path
                // Nothing says when a failed accept (out of descriptors, say) would pass.
                crate::hold(Duration::from_millis(100));
                continue;
            }
        };
        let open = shared.open.fetch_add(1, Ordering::AcqRel) + 1;
        if open > MOST_CONNECTIONS {
            refuse(&shared, &mut stream, "too many deliveries at once; GitHub redelivers");
            continue;
        }
        if open > handlers {
            // clone: once per handler started
            let (its_shared, its_streams) = (shared.clone(), streams.clone());
            let started = thread::Builder::new()
                .name(format!("blaze-ci webhook {handlers}")) // alloc: once per handler started
                .spawn(move || handler(&its_shared, &its_streams));
            if let Err(error) = started {
                log(&format!("a webhook handler did not start: {error}")); // alloc: a failed start
                refuse(&shared, &mut stream, "no thread to read the delivery on; GitHub redelivers");
                continue;
            }
            handlers += 1;
        }
        to_handlers.send(stream).expect("the handlers' receiver is held here");
    }
}

/// Answers 503 on the accepting thread and lets go of the connection's place in `open`.
fn refuse(shared: &Listener, stream: &mut TcpStream, said: &str) {
    shared.open.fetch_sub(1, Ordering::AcqRel);
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    let mut answer = Answer::default();
    answer.said.push_str(said);
    answer.write(503);
    let _ = stream.write_all(&answer.out);
}

/// One handler: each connection in turn, with one parser and one answer for all of them.
fn handler(shared: &Listener, streams: &crossbeam_channel::Receiver<TcpStream>) {
    let mut parser = RequestParser::new(webhook::MOST_PAYLOAD);
    let mut answer = Answer::default();
    for mut stream in streams {
        parser.reset();
        let _ = stream.set_read_timeout(Some(TIMEOUT));
        let _ = stream.set_write_timeout(Some(TIMEOUT));
        exchange(&mut stream, shared, &mut parser, &mut answer);
        drop(stream);
        shared.open.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A handler's answer, its words and its bytes, kept from one connection to the next.
pub(crate) struct Answer {
    said: String,
    out: Vec<u8>,
}

/// Room for the words of any answer to a delivery whose id fits a [`Seen`] slot, and for the
/// response around them (status line, headers, length): a warm exchange grows neither buffer.
const SAID_ROOM: usize = 256;
const OUT_ROOM: usize = 512;

impl Default for Answer {
    fn default() -> Answer {
        // alloc: once per handler, at setup.
        Answer { said: String::with_capacity(SAID_ROOM), out: Vec::with_capacity(OUT_ROOM) }
    }
}

impl Answer {
    /// `out` as the whole response: `status`, then `said` and a newline as its body.
    fn write(&mut self, status: u16) {
        self.said.push('\n');
        self.out.clear();
        http::response_into(&mut self.out, status, &HEADERS, self.said.as_bytes());
    }
}

/// One connection's request read from `stream` into `parser`, checked in place and answered.
pub(crate) fn exchange(
    stream: &mut (impl Read + Write),
    shared: &Listener,
    parser: &mut RequestParser,
    answer: &mut Answer,
) {
    loop {
        answer.said.clear();
        let status = match parser.next_ref() {
            Ok(Some(request)) => Some(match webhook::receive_ref(&shared.verifier, &request) {
                Ok(delivery) => accept(shared, delivery, &mut answer.said),
                Err(error) => {
                    answer.said.push_str(&error.what);
                    error.status
                }
            }),
            Ok(None) => None,
            Err(error) => {
                answer.said.push_str(&error.what);
                Some(error.status)
            }
        };
        if let Some(status) = status {
            answer.write(status);
            let _ = stream.write_all(&answer.out);
            return;
        }
        match parser.read_from(&mut *stream) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// The status a checked delivery is answered with, its words written to `said`; its event goes
/// to the service once, the one thing made of it that is its own.
pub(crate) fn accept(shared: &Listener, delivery: DeliveryRef<'_>, said: &mut String) -> u16 {
    let Some(event) = delivery.event else {
        said.push_str("nothing for CI to run");
        return 202;
    };
    if *delivery.repo != *shared.repo {
        let _ = write!(said, "a delivery for {}, and this service is for {}", delivery.repo, shared.repo);
        return 403;
    }
    if !shared.seen.lock().first(delivery.id) {
        let _ = write!(said, "delivery {} was already received", delivery.id);
        return 202;
    }
    if shared.service.send(Input::Event(event.into_owned())).is_err() {
        said.push_str("the service has stopped");
        return 503;
    }
    said.push_str("accepted");
    202
}

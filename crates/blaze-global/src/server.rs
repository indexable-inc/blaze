//! The global cache's side: answers fetches from its store, takes publishes signed by a
//! pinned publisher, and keeps and serves bundle manifests. Sans-IO over [`blaze_core`]'s
//! `Listen`, `Send` and `Recv`, as the [`crate::Client`] is.
//!
//! The store keeps each value under its key and the value's signed record under
//! [`record_key`], put after the value, so a record found means its value is there. A
//! publish is checked as a fetch is (the record's signature by a pinned publisher, then the
//! bytes against its content hash; nothing is allocated for a value whose record fails) and
//! answered only once every put it made is durable. Responses on a connection go in the order
//! asked, so one waiting to be durable holds back those after it.
//!
//! A request that is not one (malformed, too large, of an unknown path) or one the store can't
//! answer is answered with its status and why, and the connection closed: the pieces' windows
//! of a request not read through can't be trusted after it.

use std::collections::VecDeque;

use blaze_cas::{Key, Kind, Store, Ticket};
use blaze_core::{Address, Completion, Conn, Done, Effect, Tag};
use blaze_format::cursor::Reader;
use blaze_ids::ContentId;
use blaze_mem::{Tag as MemTag, frame};

use crate::{
    Tags,
    bundle::{BundleId, ENTRIES_MOST, Manifest},
    client::{BUNDLE_PATH, BUNDLE_PUBLISH_PATH, FETCH_PATH, PUBLISH_PATH},
    http,
    sign::{RECORD_LEN, SIGNED_LEN, Signed, Trust},
    tls::{Channel, Channels, TlsServer},
    wire::{self, BATCH_MOST, BODY_MOST, Fields as _, RecvWindow, SendWindow},
};

/// The most connections it serves at once; one more is hung up on.
pub const PEERS_MOST: usize = 1024;
/// A connection is not read from while its responses not yet sent hold this many bytes.
const QUEUED_MOST: usize = 256 << 20;
const RECV_LEAST: usize = 1 << 20;
const RECV_MOST: usize = 1 << 26;
/// The body of a request of an unknown path read to answer it.
const OTHER_BODY_MOST: u64 = 4 << 10;

const RECORD_KEY_DOMAIN: &str = "blaze v2 global record key 1";

/// The key its store keeps `key`'s signed record under.
pub fn record_key(key: &Key) -> Key {
    Key(ContentId::of(RECORD_KEY_DOMAIN, &key.0.0))
}

pub struct Server<C: Channels = TlsServer> {
    at: Address,
    channels: C,
    trust: Trust,
    tags: Tags,
    listen: Option<Tag>,
    peers: Vec<Peer<C::Channel>>,
    problems: Vec<String>,
}

struct Peer<Ch> {
    conn: Conn,
    channel: Ch,
    sending: Option<Tag>,
    recv: Option<Tag>,
    staged: Vec<u8>,
    plain: Vec<u8>,
    waiting: VecDeque<Response>,
    /// The bytes of `waiting`.
    queued: usize,
    send_window: SendWindow,
    recv_window: RecvWindow,
    /// It answered a request with a close, or the peer closed: no more is read.
    closing: bool,
}

struct Response {
    head: Vec<u8>,
    body: Vec<u8>,
    /// Sent once each is durable.
    tickets: Vec<Ticket>,
    close: bool,
}

impl Response {
    fn new(status: u16, body: Vec<u8>, tickets: Vec<Ticket>) -> Response {
        let close = status != 200;
        // alloc: the edge: a response's buffers, one a request, sent after it.
        let mut head = Vec::new();
        http::write_response_head(status, body.len(), close, &mut head);
        Response { head, body, tickets, close }
    }

    fn refused(status: u16, why: &str) -> Response {
        // alloc: an error's message, made only when it fails.
        let mut body = why.as_bytes().to_vec();
        let mut end = body.len().min(wire::REASON_MOST);
        while !why.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        // alloc: the edge: a response's buffers, one a request, sent after it.
        Response::new(status, body, Vec::new())
    }

    fn len(&self) -> usize {
        self.head.len() + self.body.len()
    }
}

enum Role {
    Send(usize),
    Recv(usize),
}

impl<C: Channels> Server<C> {
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn new(at: Address, channels: C, trust: Trust, tags: u64) -> Server<C> {
        // alloc: setup: once a server or a connection.
        Server { at, channels, trust, tags: Tags::new(tags), listen: None, peers: Vec::new(), problems: Vec::new() }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    pub fn start(&mut self, effects: &mut Vec<Effect>) {
        assert!(self.listen.is_none(), "a server started twice");
        let tag = self.tags.next();
        self.listen = Some(tag);
        // alloc: setup: once a server or a connection.
        effects.push(Effect::Listen { at: self.at.clone(), tag });
    }

    /// Stops listening and hangs up on every connection.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        if let Some(tag) = self.listen {
            effects.push(Effect::Unlisten { tag });
        }
        for peer in self.peers.drain(..) {
            effects.push(Effect::Hangup { conn: peer.conn, tag: self.tags.next() });
        }
    }

    pub fn listening(&self) -> bool {
        self.listen.is_some()
    }

    /// What went wrong since the last call, for its log: a listen that ended, a connection
    /// dropped.
    pub fn problems(&mut self) -> Vec<String> {
        std::mem::take(&mut self.problems)
    }

    pub fn complete(&mut self, store: &mut Store, completion: Completion, effects: &mut Vec<Effect>) {
        let Completion { tag, done } = completion;
        if self.listen == Some(tag) {
            match done {
                Done::Accepted { conn } => self.accept(conn, effects),
                Done::Unlistened => self.listen = None,
                done => {
                    self.listen = None;
                    // alloc: an error's message, made only when it fails.
                    self.problems.push(format!("listening at {} ended: {done:?}", self.at));
                }
            }
            return;
        }
        let role = self.peers.iter().enumerate().find_map(|(at, peer)| {
            if peer.sending == Some(tag) {
                Some(Role::Send(at))
            } else if peer.recv == Some(tag) {
                Some(Role::Recv(at))
            } else {
                None
            }
        });
        match role {
            Some(Role::Send(at)) => match done {
                Done::Sent(spare) => {
                    let peer = &mut self.peers[at];
                    peer.sending = None;
                    // The buffer sent is the next staged one, its room kept.
                    if peer.staged.capacity() == 0 {
                        peer.staged = spare;
                    }
                    self.pump(at, store, effects);
                }
                // alloc: an error's message, made only when it fails.
                done => self.drop_peer(at, &format!("sending failed: {done:?}"), effects),
            },
            Some(Role::Recv(at)) => {
                self.peers[at].recv = None;
                match done {
                    Done::Read(bytes) if bytes.is_empty() => {
                        let peer = &mut self.peers[at];
                        peer.closing = true;
                        if peer.waiting.is_empty() && peer.sending.is_none() && peer.staged.is_empty() {
                            self.remove(at, effects);
                        } else {
                            self.pump(at, store, effects);
                        }
                    }
                    Done::Read(bytes) => match self.received(at, store, &bytes, effects) {
                        Ok(()) => self.pump(at, store, effects),
                        Err(why) => self.drop_peer(at, &why, effects),
                    },
                    // alloc: an error's message, made only when it fails.
                    done => self.drop_peer(at, &format!("receiving failed: {done:?}"), effects),
                }
            }
            None => {}
        }
    }

    /// Sends the responses whose puts became durable: call it with the store's durable
    /// tickets ([`Store::complete`]'s answer) not empty.
    pub fn durable(&mut self, store: &Store, effects: &mut Vec<Effect>) {
        let mut at = self.peers.len();
        while at > 0 {
            at -= 1;
            if self.peers[at].waiting.front().is_some_and(|it| !it.tickets.is_empty()) {
                self.pump(at, store, effects);
            }
        }
    }

    fn accept(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        if self.peers.len() >= PEERS_MOST {
            // alloc: an error's message, made only when it fails.
            self.problems.push(format!("a connection hung up on: {PEERS_MOST} are served already"));
            effects.push(Effect::Hangup { conn, tag: self.tags.next() });
            return;
        }
        let channel = match self.channels.channel() {
            Ok(channel) => channel,
            Err(why) => {
                // alloc: an error's message, made only when it fails.
                self.problems.push(format!("a connection hung up on: {why}"));
                effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                return;
            }
        };
        let tag = self.tags.next();
        // alloc: none here; the driver reserves the room the bytes come into.
        effects.push(Effect::Recv { conn, most: RECV_LEAST, into: Vec::new(), tag });
        self.peers.push(Peer {
            conn,
            channel,
            sending: None,
            recv: Some(tag),
            // alloc: setup: once a server or a connection.
            staged: Vec::new(),
            // alloc: setup: once a server or a connection.
            plain: Vec::new(),
            waiting: VecDeque::new(),
            queued: 0,
            send_window: SendWindow::default(),
            recv_window: RecvWindow::default(),
            closing: false,
        });
    }

    fn drop_peer(&mut self, at: usize, why: &str, effects: &mut Vec<Effect>) {
        let peer = &self.peers[at];
        // alloc: an error's message, made only when it fails.
        self.problems.push(format!("a connection dropped with {} responses unsent: {why}", peer.waiting.len()));
        self.remove(at, effects);
    }

    fn remove(&mut self, at: usize, effects: &mut Vec<Effect>) {
        let peer = self.peers.swap_remove(at);
        effects.push(Effect::Hangup { conn: peer.conn, tag: self.tags.next() });
    }

    fn received(&mut self, at: usize, store: &mut Store, bytes: &[u8], effects: &mut Vec<Effect>) -> Result<(), String> {
        let Server { peers, trust, .. } = self;
        let peer = &mut peers[at];
        peer.channel.received(bytes, &mut peer.plain)?;
        let mut start = 0;
        while !peer.closing {
            let rest = &peer.plain[start..];
            let head = match http::request_head(rest, body_most) {
                Ok(Some(head)) => head,
                Ok(None) => break,
                Err(bad) => {
                    push(peer, Response::refused(bad.status, &bad.what));
                    break;
                }
            };
            if rest.len() < head.head + head.body {
                break;
            }
            let body = &rest[head.head..head.head + head.body];
            let response = answer(&head.path, body, store, trust, &mut peer.send_window, &mut peer.recv_window, effects);
            start += head.head + head.body;
            push(peer, response);
        }
        peer.plain.drain(..start);
        if peer.channel.peer_closed() {
            peer.closing = true;
        }
        Ok(())
    }

    /// Sends what can go, keeps a receive in flight while it may read, and hangs up once a
    /// closing connection has sent everything.
    fn pump(&mut self, at: usize, store: &Store, effects: &mut Vec<Effect>) {
        let Server { peers, tags, .. } = self;
        let peer = &mut peers[at];
        let result = (|| -> Result<(), String> {
            while let Some(front) = peer.waiting.front() {
                if !front.tickets.iter().all(|ticket| store.durable(*ticket)) {
                    break;
                }
                let response = peer.waiting.pop_front().expect("a front");
                peer.queued -= response.len();
                peer.channel.send(&response.head)?;
                peer.channel.send(&response.body)?;
                if response.close {
                    peer.closing = true;
                    peer.queued = 0;
                    peer.waiting.clear();
                }
            }
            peer.channel.outgoing(&mut peer.staged)
        })();
        if let Err(why) = result {
            self.drop_peer(at, &why, effects);
            return;
        }
        if peer.sending.is_none() && !peer.staged.is_empty() {
            let tag = tags.next();
            peer.sending = Some(tag);
            effects.push(Effect::Send { conn: peer.conn, bytes: std::mem::take(&mut peer.staged), tag });
        }
        if peer.closing {
            if peer.waiting.is_empty() && peer.sending.is_none() {
                self.remove(at, effects);
            }
            return;
        }
        if peer.recv.is_none() && peer.queued < QUEUED_MOST {
            let owed = match http::request_head(&peer.plain, body_most) {
                Ok(Some(head)) => (head.head + head.body).saturating_sub(peer.plain.len()),
                _ => 0,
            };
            let tag = tags.next();
            peer.recv = Some(tag);
            let most = owed.clamp(RECV_LEAST, RECV_MOST);
            // alloc: none here; the driver reserves the room the bytes come into.
            effects.push(Effect::Recv { conn: peer.conn, most, into: Vec::new(), tag });
        }
    }
}

fn push<Ch>(peer: &mut Peer<Ch>, response: Response) {
    if response.close {
        peer.closing = true;
    }
    peer.queued += response.len();
    peer.waiting.push_back(response);
}

fn body_most(path: &str) -> u64 {
    match path {
        FETCH_PATH => (8 + 4 + 32 * BATCH_MOST) as u64,
        PUBLISH_PATH => BODY_MOST,
        BUNDLE_PATH => 8 + 32,
        BUNDLE_PUBLISH_PATH => (8 + 32 + 32 + 64 + 4 + RECORD_LEN * ENTRIES_MOST) as u64,
        _ => OTHER_BODY_MOST,
    }
}

fn answer(
    path: &str,
    body: &[u8],
    store: &mut Store,
    trust: &Trust,
    send_window: &mut SendWindow,
    recv_window: &mut RecvWindow,
    effects: &mut Vec<Effect>,
) -> Response {
    if !store.ready() {
        let why = match store.broken() {
            // alloc: an error's message, made only when it fails.
            Some(broken) => broken.to_string(),
            // alloc: an error's message, made only when it fails.
            None => "the cache's store is not open yet".to_owned(),
        };
        return Response::refused(503, &why);
    }
    let answered = match path {
        FETCH_PATH => fetch(body, store, send_window),
        PUBLISH_PATH => publish(body, store, trust, recv_window, effects),
        BUNDLE_PATH => bundle(body, store),
        BUNDLE_PUBLISH_PATH => bundle_publish(body, store, trust, effects),
        // alloc: an error's message, made only when it fails.
        _ => Err(Response::refused(404, &format!("{path}: the global cache has no such path"))),
    };
    answered.unwrap_or_else(|refused| refused)
}

/// The value of `key` and its signed record: `Ok(None)` when it has no record of it.
fn found<'s>(store: &'s Store, key: &Key) -> Result<Option<(Signed, &'s [u8])>, Response> {
    // alloc: an error's message, made only when it fails.
    let failed = |why: String| Response::refused(500, &format!("the cache's value of {:x}: {why}", key.0));
    // alloc: an error's message, made only when it fails.
    let Some(record) = store.get(&record_key(key)).map_err(|corrupt| failed(corrupt.to_string()))? else {
        return Ok(None);
    };
    let bytes: &[u8; SIGNED_LEN] =
        // alloc: an error's message, made only when it fails.
        record.bytes.try_into().map_err(|_| failed(format!("a record of {} bytes", record.bytes.len())))?;
    let signed = Signed::decode(bytes).map_err(failed)?;
    // alloc: an error's message, made only when it fails.
    let value = store.get(key).map_err(|corrupt| failed(corrupt.to_string()))?;
    let value = value.ok_or_else(|| failed("its record is kept and it is not".into()))?;
    if value.content != signed.record.content || value.kind != signed.record.kind || signed.record.key != *key {
        // alloc: an error's message, made only when it fails.
        return Err(failed(format!("its record names {:?}, and the value kept is {:x} of kind {:?}", signed.record, value.content, value.kind)));
    }
    Ok(Some((signed, value.bytes)))
}

fn fetch(body: &[u8], store: &mut Store, window: &mut SendWindow) -> Result<Response, Response> {
    frame::task(MemTag::Store, |temp| {
        let keys = wire::parse_fetch_ask_in(body, temp).map_err(|why| Response::refused(400, &why))?;
        let mut answers = Vec::with_capacity_in(keys.len(), temp);
        let mut bytes = 0;
        let mut deferring = false;
        for key in &keys {
            let kept = if deferring { None } else { found(store, key)? };
            if let Some((signed, _)) = &kept {
                deferring = !wire::takes(bytes, signed.record.len);
                if !deferring {
                    bytes += signed.record.len;
                }
            }
            answers.push((key, deferring, kept));
        }
        // alloc: the response's body, which the connection sends after the request.
        let mut out = Vec::with_capacity(12 + answers.len() * (33 + SIGNED_LEN) + bytes as usize);
        wire::header(wire::FETCH_ANSWER, &mut out);
        out.extend_from_slice(&(answers.len() as u32).to_le_bytes());
        for (key, deferred, kept) in answers {
            match (deferred, kept) {
                (true, _) => {
                    out.push(wire::DEFERRED);
                    out.extend_from_slice(&key.0.0);
                }
                (false, None) => {
                    out.push(wire::MISSING);
                    out.extend_from_slice(&key.0.0);
                }
                (false, Some((signed, value))) => {
                    out.push(wire::FOUND);
                    out.extend_from_slice(&key.0.0);
                    out.extend_from_slice(&signed.encode());
                    window.write(value, &mut out);
                }
            }
        }
        // alloc: the edge: a response's buffers, one a request, sent after it.
        Ok(Response::new(200, out, Vec::new()))
    })
}

fn publish(
    body: &[u8],
    store: &mut Store,
    trust: &Trust,
    window: &mut RecvWindow,
    effects: &mut Vec<Effect>,
) -> Result<Response, Response> {
    // alloc: an error's message, made only when it fails.
    let bad = |why: String| Response::refused(400, &format!("a malformed publish: {why}"));
    let mut reader = Reader::new(body);
    reader.header(wire::PUBLISH_ASK, "a publish").map_err(bad)?;
    let count = reader.count(SIGNED_LEN + 4, BATCH_MOST, "values").map_err(bad)?;
    let mut statuses = Vec::with_capacity(count);
    // alloc: the edge: a response's buffers, one a request, sent after it.
    let mut tickets = Vec::new();
    for _ in 0..count {
        let signed = reader.signed().map_err(bad)?;
        // alloc: an error's message, made only when it fails.
        let checked = trust.verify(&signed).map_err(|refused| refused.to_string());
        let value = window.read(&mut reader, signed.record.len, checked.is_ok()).map_err(bad)?;
        let status = checked.and_then(|()| keep(store, &signed, value.expect("a value whose record is checked is kept"), &mut tickets));
        statuses.push((signed.record.key, status));
    }
    reader.end("a publish").map_err(bad)?;
    if !tickets.is_empty() {
        store.flush(effects);
    }
    // alloc: the edge: a response's buffers, one a request, sent after it.
    let mut out = Vec::new();
    wire::publish_answer(&statuses, &mut out);
    Ok(Response::new(200, out, tickets))
}

/// Puts a value whose record is checked: the value, then its record.
fn keep(store: &mut Store, signed: &Signed, bytes: Vec<u8>, tickets: &mut Vec<Ticket>) -> Result<(), String> {
    let record = signed.record;
    let found = blaze_cas::content_of(&bytes);
    if found != record.content {
        // alloc: an error's message, made only when it fails.
        return Err(format!(
            "the global cache refuses {:x}: its bytes hash to {found:x}, not the {:x} its record names",
            record.key.0, record.content
        ));
    }
    let value = store.put(record.key, record.kind, bytes).map_err(|mismatch| {
        // alloc: an error's message, made only when it fails.
        format!("the global cache keeps another value of {:x}: {:x}, not {:x}", record.key.0, mismatch.kept, mismatch.given)
    })?;
    // alloc: an error's message, made only when it fails.
    let kept = store.put(record_key(&record.key), Kind::Query, signed.encode().to_vec()).map_err(|_| {
        // alloc: an error's message, made only when it fails.
        format!("the global cache keeps another signed record of {:x} (another publisher's)", record.key.0)
    })?;
    tickets.extend([value, kept]);
    Ok(())
}

fn bundle(body: &[u8], store: &Store) -> Result<Response, Response> {
    // alloc: an error's message, made only when it fails.
    let bad = |why: String| Response::refused(400, &format!("a malformed bundle request: {why}"));
    let mut reader = Reader::new(body);
    reader.header(wire::BUNDLE_ASK, "a bundle request").map_err(bad)?;
    let id = BundleId(ContentId(*reader.array().map_err(wire::truncated).map_err(bad)?));
    reader.end("a bundle request").map_err(bad)?;
    // alloc: an error's message, made only when it fails.
    let manifest = store.get(&id.manifest_key()).map_err(|corrupt| Response::refused(500, &corrupt.to_string()))?;
    // alloc: the edge: a response's buffers, one a request, sent after it.
    let mut out = Vec::new();
    wire::header(wire::BUNDLE_ANSWER, &mut out);
    match manifest {
        None => out.push(wire::MISSING),
        Some(entry) => {
            out.reserve(1 + entry.bytes.len());
            out.push(wire::FOUND);
            out.extend_from_slice(entry.bytes);
        }
    }
    // alloc: the edge: a response's buffers, one a request, sent after it.
    Ok(Response::new(200, out, Vec::new()))
}

/// Keeps a manifest signed by a pinned publisher whose every record is the one it keeps.
fn bundle_publish(body: &[u8], store: &mut Store, trust: &Trust, effects: &mut Vec<Effect>) -> Result<Response, Response> {
    // alloc: an error's message, made only when it fails.
    let bad = |why: String| Response::refused(400, &format!("a malformed bundle publish: {why}"));
    let mut reader = Reader::new(body);
    reader.header(wire::BUNDLE_PUBLISH, "a bundle publish").map_err(bad)?;
    let manifest = Manifest::decode(&mut reader).map_err(bad)?;
    reader.end("a bundle publish").map_err(bad)?;
    let key = Key(manifest.id.0);
    let status = manifest
        .verify(trust)
        // alloc: an error's message, made only when it fails.
        .map_err(|why| format!("the manifest of {:x} is refused: {why}", key.0))
        .and_then(|()| {
            for entry in &manifest.entries {
                match found(store, &entry.key) {
                    Ok(Some((signed, _))) if signed.record == *entry => {}
                    Ok(Some((signed, _))) => {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!("it lists {:?} for {:x}, and the cache keeps {:?}", entry, entry.key.0, signed.record));
                    }
                    // alloc: an error's message, made only when it fails.
                    Ok(None) => return Err(format!("it lists {:x}, which the cache has no value of", entry.key.0)),
                    // alloc: an error's message, made only when it fails.
                    Err(response) => return Err(String::from_utf8_lossy(&response.body).into_owned()),
                }
            }
            Ok(())
        });
    // alloc: the edge: a response's buffers, one a request, sent after it.
    let mut tickets = Vec::new();
    let status = status.and_then(|()| {
        // alloc: the edge: a response's buffers, one a request, sent after it.
        let mut bytes = Vec::new();
        manifest.encode(&mut bytes);
        let ticket = store
            .put(manifest.id.manifest_key(), Kind::Output, bytes)
            // alloc: an error's message, made only when it fails.
            .map_err(|_| format!("the cache keeps another manifest of {:x}", key.0))?;
        tickets.push(ticket);
        Ok(())
    });
    if !tickets.is_empty() {
        store.flush(effects);
    }
    // alloc: the edge: a response's buffers, one a request, sent after it.
    let mut out = Vec::new();
    wire::publish_answer(&[(key, status)], &mut out);
    Ok(Response::new(200, out, tickets))
}

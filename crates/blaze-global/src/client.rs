//! A machine's side of the global cache: fetches of public keys, publishes from the builders,
//! and bundles, over up to [`CONNECTIONS`] connections to one [`Endpoint`], each carrying up
//! to [`PIPELINE`] requests at once, each request naming many keys.
//!
//! The [`Client`] is the store's [`Remote`] for public keys: [`Client::fetch`] asks
//! [`Store::fetch`] with it, and the answer is a completion the store takes. Before a value
//! is handed to the store its record's signature is checked (or, for a key a verified bundle
//! manifest lists, that its record is the one listed), then its bytes against the record's
//! content hash. A value failing either is answered `Done::Failed` naming the key and why:
//! the store refuses it ([`blaze_cas::Fetched::Refused`]), and it is asked of nothing else.
//! A malformed response, one for other keys than were asked, or a hangup fails what its
//! connection still owes, naming the endpoint and each key; nothing sent is asked again.

use std::collections::VecDeque;

use blaze_cas::{Key, Remote, Store, remote::Answer};
use blaze_core::{Address, Completion, Conn, Done, Effect, IoError, Tag};
use blaze_format::cursor::{Put as _, Reader};
use blaze_mem::{Tag as MemTag, frame, scratch};

use crate::{
    ByHash, Refused, Tags,
    bundle::{BundleId, Manifest},
    http,
    public::{Private, Public, Secrets},
    sign::{Publisher, Record, Signed, Trust},
    tls::{Channel, Channels, TlsClient},
    wire::{self, BATCH_MOST, BODY_MOST, Fields as _, RecvWindow, SendWindow},
};

/// The most connections to the cache at once: enough that their receive windows (about
/// 6 MiB each) cover a 10 Gbit/s link at 50 ms. As many are opened as what is queued takes.
pub const CONNECTIONS: usize = 16;
/// The connections a bundle's ask opens with it, so they are up when its manifest is.
const BUNDLE_WARM: usize = 4;
/// The most requests in flight on one connection.
pub const PIPELINE: usize = 4;
/// The most keys one fetch of keys of unknown length names.
pub const BATCH_KEYS: usize = 256;
const RECV_LEAST: usize = 1 << 20;
/// The most room a response's head reserves for the rest of it.
const RESERVE_MOST: usize = wire::BATCH_BYTES as usize + (1 << 20);
const RECV_MOST: usize = 1 << 26;

pub(crate) const FETCH_PATH: &str = "/v1/fetch";
pub(crate) const PUBLISH_PATH: &str = "/v1/publish";
pub(crate) const BUNDLE_PATH: &str = "/v1/bundle";
pub(crate) const BUNDLE_PUBLISH_PATH: &str = "/v1/bundle/publish";

/// Where the cache is: the address to connect to, and the host name its certificate is for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Endpoint {
    pub at: Address,
    pub host: String,
}

/// How a publish ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Published {
    pub tag: Tag,
    pub key: Key,
    pub result: Result<(), String>,
}

/// How a request for a bundle's manifest ended: `Ok(None)` when the cache has none, else
/// the manifest, its signature checked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GotBundle {
    pub tag: Tag,
    pub id: BundleId,
    pub result: Result<Option<Manifest>, String>,
}

pub struct Client<C: Channels = TlsClient> {
    endpoint: Endpoint,
    channels: C,
    trust: Trust,
    tags: Tags,
    links: Vec<Link<C::Channel>>,
    /// Fetches not yet asked: the store's tag and the key.
    fetches: VecDeque<(Tag, Key)>,
    publishes: VecDeque<(Tag, Signed, Vec<u8>)>,
    bundles: VecDeque<BundleWork>,
    /// The key [`Client::fetch`] is asking the store to fetch through it.
    allowed: Option<Key>,
    /// In [`Client::fetch_many`]: what the store asks waits to be asked together.
    batching: bool,
    /// The records of verified manifests.
    listed: ByHash<Key, Record>,
    published: Vec<Published>,
    got: Vec<GotBundle>,
    /// The store's completions that came of a call with no completions to return (a fetch
    /// whose connection failed as it was asked): returned by the next [`Client::complete`].
    undelivered: Vec<Completion>,
    /// Connects given up while in flight: a connection one of them makes is hung up.
    abandoned: Vec<Tag>,
}

enum BundleWork {
    Get { tag: Tag, id: BundleId },
    Publish { tag: Tag, manifest: Manifest },
}

enum Link<Ch> {
    Connecting(Tag),
    Up(Box<Up<Ch>>),
}

struct Up<Ch> {
    conn: Conn,
    channel: Ch,
    sending: Option<Tag>,
    recv: Option<Tag>,
    /// Bytes for the network not yet handed to a send.
    staged: Vec<u8>,
    /// Plaintext received and not yet a whole response.
    plain: Vec<u8>,
    /// What each request in flight asked, oldest first: responses come in this order.
    asked: VecDeque<Asked>,
    send_window: SendWindow,
    recv_window: RecvWindow,
}

enum Asked {
    Fetch(Vec<(Tag, Key)>),
    Publish(Vec<(Tag, Key)>),
    Bundle(Tag, BundleId),
    BundlePublish(Tag, BundleId),
}

/// Where the outcomes of what a connection asked go.
struct Outcomes<'a> {
    host: &'a str,
    store: &'a mut Vec<Completion>,
    published: &'a mut Vec<Published>,
    got: &'a mut Vec<GotBundle>,
}

impl Outcomes<'_> {
    fn fail(&mut self, asked: Asked, why: &str) {
        let host = self.host;
        // alloc: an error's message, made only when it fails.
        let failed = |key: &Key| format!("the global cache at {host}: {why}; asked for {:x}", key.0);
        match asked {
            Asked::Fetch(keys) => self.store.extend(
                keys.iter().map(|(tag, key)| Completion { tag: *tag, done: Done::Failed(IoError(failed(key))) }),
            ),
            Asked::Publish(keys) => self
                .published
                .extend(keys.iter().map(|(tag, key)| Published { tag: *tag, key: *key, result: Err(failed(key)) })),
            Asked::Bundle(tag, id) => self.got.push(GotBundle { tag, id, result: Err(failed(&Key(id.0))) }),
            Asked::BundlePublish(tag, id) => {
                self.published.push(Published { tag, key: Key(id.0), result: Err(failed(&Key(id.0))) })
            }
        }
    }
}

enum Role {
    Connect(usize),
    Send(usize),
    Recv(usize),
    Abandoned,
    Stale,
}

impl<C: Channels> Client<C> {
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn new(endpoint: Endpoint, channels: C, trust: Trust, tags: u64) -> Client<C> {
        Client {
            endpoint,
            channels,
            trust,
            tags: Tags::new(tags),
            // alloc: setup: once a client.
            links: Vec::new(),
            fetches: VecDeque::new(),
            publishes: VecDeque::new(),
            bundles: VecDeque::new(),
            allowed: None,
            batching: false,
            listed: ByHash::default(),
            // alloc: setup: once a client.
            published: Vec::new(),
            // alloc: setup: once a client.
            got: Vec::new(),
            // alloc: setup: once a client.
            undelivered: Vec::new(),
            // alloc: setup: once a client.
            abandoned: Vec::new(),
        }
    }

    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    /// Asks `store` to fetch `public` through the cache: `None` when it has the value or is
    /// fetching it. The store's completion is among what [`Client::complete`] returns.
    pub fn fetch(&mut self, store: &mut Store, public: &Public, effects: &mut Vec<Effect>) -> Option<Tag> {
        self.fetch_many(store, std::iter::once(public), effects).pop().map(|(_, tag)| tag)
    }

    /// [`Client::fetch`] of each of `publics`, asked together so they go in as few requests
    /// as their batches take. The keys the store asked for, with their tags.
    pub fn fetch_many<'p>(
        &mut self,
        store: &mut Store,
        publics: impl IntoIterator<Item = &'p Public>,
        effects: &mut Vec<Effect>,
    ) -> Vec<(Key, Tag)> {
        self.batching = true;
        // alloc: setup: once a bundle.
        let mut tags = Vec::new();
        for public in publics {
            let key = public.key();
            self.allowed = Some(key);
            tags.extend(store.fetch(&key, self, effects).map(|tag| (key, tag)));
        }
        self.allowed = None;
        self.batching = false;
        self.pump_held(effects);
        tags
    }

    /// Publishes `value` as the value of `public`, signed by `publisher`, unless it holds one
    /// of `secrets`; how it went comes through [`Client::published`] with the tag returned.
    pub fn publish(
        &mut self,
        public: &Public,
        value: Vec<u8>,
        publisher: &Publisher,
        secrets: &Secrets,
        effects: &mut Vec<Effect>,
    ) -> Result<Tag, Private> {
        secrets.check(public.key(), &value)?;
        let signed = publisher.sign(Record::of(public.key(), public.kind(), &value));
        let tag = self.tags.next();
        self.publishes.push_back((tag, signed, value));
        self.pump_held(effects);
        Ok(tag)
    }

    /// Asks for the manifest of `id`; it comes through [`Client::bundles`].
    pub fn bundle(&mut self, id: BundleId, effects: &mut Vec<Effect>) -> Tag {
        let tag = self.tags.next();
        self.bundles.push_back(BundleWork::Get { tag, id });
        self.pump_held(effects);
        self.connect(BUNDLE_WARM.saturating_sub(self.links.len()), effects);
        tag
    }

    /// Publishes a signed manifest; how it went comes through [`Client::published`], keyed by
    /// the bundle's ID.
    pub fn publish_bundle(&mut self, manifest: Manifest, effects: &mut Vec<Effect>) -> Tag {
        let tag = self.tags.next();
        self.bundles.push_back(BundleWork::Publish { tag, manifest });
        self.pump_held(effects);
        tag
    }

    /// The publishes answered since the last call.
    pub fn published(&mut self) -> Vec<Published> {
        std::mem::take(&mut self.published)
    }

    /// The manifests answered since the last call.
    pub fn bundles(&mut self) -> Vec<GotBundle> {
        std::mem::take(&mut self.got)
    }

    /// The record a verified manifest lists for `key`.
    pub fn listed(&self, key: &Key) -> Option<&Record> {
        self.listed.get(key)
    }

    /// Takes a completion of its own. Returns the store's: each the completion of a tag
    /// [`Store::fetch`] asked with, for [`Store::complete`].
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Completion> {
        let mut answers = std::mem::take(&mut self.undelivered);
        let Completion { tag, done } = completion;
        match self.role(tag) {
            Role::Abandoned => {
                self.abandoned.retain(|it| *it != tag);
                if let Done::Connected { conn } = done {
                    effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                }
            }
            Role::Connect(at) => match done {
                Done::Connected { conn } => match self.channels.channel() {
                    Ok(channel) => {
                        // alloc: setup: once a connection.
                        self.links[at] = Link::Up(Box::new(Up {
                            conn,
                            channel,
                            sending: None,
                            recv: None,
                            // alloc: setup: once a connection.
                            staged: Vec::new(),
                            // alloc: setup: once a connection.
                            plain: Vec::new(),
                            asked: VecDeque::new(),
                            send_window: SendWindow::default(),
                            recv_window: RecvWindow::default(),
                        }));
                        self.pump_all(&mut answers, effects);
                    }
                    Err(why) => {
                        effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                        self.connect_failed(at, &why, &mut answers);
                    }
                },
                // alloc: an error's message, made only when it fails.
                Done::Failed(error) => self.connect_failed(at, &format!("could not connect: {error}"), &mut answers),
                // alloc: an error's message, made only when it fails.
                other => self.connect_failed(at, &format!("a connect completed as {other:?}"), &mut answers),
            },
            Role::Send(at) => match done {
                Done::Sent(spare) => {
                    if let Link::Up(up) = &mut self.links[at] {
                        up.sending = None;
                        // The buffer sent is the next staged one, its room kept.
                        if up.staged.capacity() == 0 {
                            up.staged = spare;
                        }
                    }
                    self.pump_all(&mut answers, effects);
                }
                Done::Broken { error, .. } | Done::Failed(error) => {
                    // alloc: an error path.
                    self.drop_link(at, &format!("sending failed: {error}"), &mut answers, effects)
                }
                // alloc: an error's message, made only when it fails.
                other => self.drop_link(at, &format!("a send completed as {other:?}"), &mut answers, effects),
            },
            Role::Recv(at) => {
                if let Link::Up(up) = &mut self.links[at] {
                    up.recv = None;
                }
                match done {
                    Done::Read(bytes) if bytes.is_empty() => {
                        self.drop_link(at, "the server hung up", &mut answers, effects)
                    }
                    Done::Read(bytes) => match self.received(at, &bytes, &mut answers) {
                        Ok(true) => self.pump_all(&mut answers, effects),
                        Ok(false) => self.drop_link(at, "the server closed the connection", &mut answers, effects),
                        Err(why) => self.drop_link(at, &why, &mut answers, effects),
                    },
                    Done::Broken { error, .. } | Done::Failed(error) => {
                        // alloc: an error path.
                        self.drop_link(at, &format!("receiving failed: {error}"), &mut answers, effects)
                    }
                    // alloc: an error's message, made only when it fails.
                    other => self.drop_link(at, &format!("a receive completed as {other:?}"), &mut answers, effects),
                }
            }
            Role::Stale => {}
        }
        answers
    }

    /// Hangs up; everything not yet answered fails, the store's fetches returned here.
    pub fn close(&mut self, effects: &mut Vec<Effect>) -> Vec<Completion> {
        let mut answers = std::mem::take(&mut self.undelivered);
        self.fail_queued("the client closed", &mut answers);
        while !self.links.is_empty() {
            self.drop_link(0, "the client closed", &mut answers, effects);
        }
        answers
    }

    fn role(&self, tag: Tag) -> Role {
        if self.abandoned.contains(&tag) {
            return Role::Abandoned;
        }
        for (at, link) in self.links.iter().enumerate() {
            match link {
                Link::Connecting(it) if *it == tag => return Role::Connect(at),
                Link::Up(up) if up.sending == Some(tag) => return Role::Send(at),
                Link::Up(up) if up.recv == Some(tag) => return Role::Recv(at),
                _ => {}
            }
        }
        Role::Stale
    }

    fn connect_failed(&mut self, at: usize, why: &str, answers: &mut Vec<Completion>) {
        self.links.remove(at);
        if self.links.is_empty() {
            self.fail_queued(why, answers);
        }
    }

    /// Fails everything not yet asked.
    fn fail_queued(&mut self, why: &str, answers: &mut Vec<Completion>) {
        let mut outcomes =
            Outcomes { host: &self.endpoint.host, store: answers, published: &mut self.published, got: &mut self.got };
        // alloc: an error path: what was queued fails once no connection is left.
        let fetches: Vec<(Tag, Key)> = self.fetches.drain(..).collect();
        if !fetches.is_empty() {
            outcomes.fail(Asked::Fetch(fetches), why);
        }
        // alloc: an error path: what was queued fails once no connection is left.
        let publishes: Vec<(Tag, Key)> = self.publishes.drain(..).map(|(tag, signed, _)| (tag, signed.record.key)).collect();
        if !publishes.is_empty() {
            outcomes.fail(Asked::Publish(publishes), why);
        }
        for work in self.bundles.drain(..) {
            match work {
                BundleWork::Get { tag, id } => outcomes.fail(Asked::Bundle(tag, id), why),
                BundleWork::Publish { tag, manifest } => outcomes.fail(Asked::BundlePublish(tag, manifest.id), why),
            }
        }
    }

    /// Gives up link `at`, failing what it owes. What is queued goes on another link.
    fn drop_link(&mut self, at: usize, why: &str, answers: &mut Vec<Completion>, effects: &mut Vec<Effect>) {
        match self.links.remove(at) {
            Link::Connecting(tag) => self.abandoned.push(tag),
            Link::Up(up) => {
                effects.push(Effect::Hangup { conn: up.conn, tag: self.tags.next() });
                // alloc: an error's message, made only when it fails.
                let why = format!("{why} with {} requests unanswered", up.asked.len());
                let mut outcomes = Outcomes {
                    host: &self.endpoint.host,
                    store: answers,
                    published: &mut self.published,
                    got: &mut self.got,
                };
                for asked in up.asked {
                    outcomes.fail(asked, &why);
                }
            }
        }
        if self.links.is_empty() && self.queued() {
            self.pump_all(answers, effects);
        }
    }

    fn queued(&self) -> bool {
        !self.fetches.is_empty() || !self.publishes.is_empty() || !self.bundles.is_empty()
    }

    /// Takes bytes received on link `at` and answers each whole response. `Ok(false)` when
    /// the server is done with the connection.
    fn received(&mut self, at: usize, bytes: &[u8], answers: &mut Vec<Completion>) -> Result<bool, String> {
        let Client { links, trust, listed, fetches, published, got, endpoint, .. } = self;
        let Link::Up(up) = &mut links[at] else { return Ok(true) };
        let up = &mut **up;
        up.channel.received(bytes, &mut up.plain)?;
        let mut outcomes = Outcomes { host: &endpoint.host, store: answers, published, got };
        let mut open = !up.channel.peer_closed();
        let mut start = 0;
        let mut owed = 0;
        let result = loop {
            let rest = &up.plain[start..];
            let head = match http::response_head(rest, BODY_MOST) {
                Ok(Some(head)) => head,
                Ok(None) => break Ok(()),
                // alloc: an error's message, made only when it fails.
                Err(why) => break Err(format!("a malformed response: {why}")),
            };
            if rest.len() < head.head + head.body {
                owed = head.head + head.body - rest.len();
                break Ok(());
            }
            let Some(asked) = up.asked.pop_front() else {
                // alloc: an error's message, made only when it fails.
                break Err("a response to nothing asked".to_owned());
            };
            let body = &rest[head.head..head.head + head.body];
            start += head.head + head.body;
            if head.status != 200 {
                // alloc: an error's message, made only when it fails.
                let said = String::from_utf8_lossy(&body[..body.len().min(wire::REASON_MOST)]).into_owned();
                // alloc: an error's message, made only when it fails.
                outcomes.fail(asked, &format!("it answered {}: {said}", head.status));
            } else if let Err(why) = answer(asked, body, trust, listed, &mut up.recv_window, fetches, &mut outcomes) {
                break Err(why);
            }
            if head.close {
                open = false;
                break Ok(());
            }
        };
        up.plain.drain(..start);
        // The rest of a response grows the buffer once, not by doubling; past a batch's bytes
        // (a value longer than a batch) it grows as the value comes.
        up.plain.reserve(owed.min(RESERVE_MOST));
        result?;
        up.channel.outgoing(&mut up.staged)?;
        Ok(open)
    }

    /// [`Client::pump_all`] from a call that returns no completions: what fails waits for
    /// the next [`Client::complete`].
    fn pump_held(&mut self, effects: &mut Vec<Effect>) {
        // alloc: pushed to only when a link fails.
        let mut answers = Vec::new();
        self.pump_all(&mut answers, effects);
        self.undelivered.extend(answers);
    }

    /// Has the links ask what is queued, one request each in turn so the work spreads over
    /// them, sends what each staged, and opens at once as many more as what is still queued
    /// takes.
    fn pump_all(&mut self, answers: &mut Vec<Completion>, effects: &mut Vec<Effect>) {
        let mut head = scratch::lend_bytes();
        let mut body = scratch::lend_bytes();
        let mut asking = true;
        while asking && self.queued() {
            asking = false;
            let mut at = 0;
            while at < self.links.len() {
                match self.ask(at, &mut head, &mut body) {
                    Ok(asked) => {
                        asking |= asked;
                        at += 1;
                    }
                    Err(why) => self.drop_link(at, &why, answers, effects),
                }
            }
        }
        let mut at = 0;
        while at < self.links.len() {
            match self.flush(at, effects) {
                Ok(()) => at += 1,
                Err(why) => self.drop_link(at, &why, answers, effects),
            }
        }
        let connecting = self.links.iter().filter(|link| matches!(link, Link::Connecting(_))).count();
        let more = self.queued_requests().div_ceil(PIPELINE).saturating_sub(connecting);
        self.connect(more, effects);
    }

    /// Opens up to `more` connections, as many as [`CONNECTIONS`] leaves room for.
    fn connect(&mut self, more: usize, effects: &mut Vec<Effect>) {
        for _ in 0..more.min(CONNECTIONS.saturating_sub(self.links.len())) {
            let tag = self.tags.next();
            // alloc: setup: once a connection.
            effects.push(Effect::Connect { at: self.endpoint.at.clone(), tag });
            self.links.push(Link::Connecting(tag));
        }
    }

    /// About how many requests what is queued goes in.
    fn queued_requests(&self) -> usize {
        self.bundles.len() + usize::from(!self.publishes.is_empty()) + self.fetches.len().div_ceil(BATCH_KEYS)
    }

    /// Asks one request of what is queued on link `at`, if it is up and has room: whether it
    /// asked one.
    fn ask(&mut self, at: usize, head: &mut Vec<u8>, body: &mut Vec<u8>) -> Result<bool, String> {
        let Client { links, fetches, publishes, bundles, listed, endpoint, .. } = self;
        let Link::Up(up) = &mut links[at] else { return Ok(false) };
        let up = &mut **up;
        if up.asked.len() >= PIPELINE {
            return Ok(false);
        }
        body.clear();
        let (path, asked) = if let Some(work) = bundles.pop_front() {
            match work {
                BundleWork::Get { tag, id } => {
                    wire::header(wire::BUNDLE_ASK, body);
                    body.extend_from_slice(&id.0.0);
                    (BUNDLE_PATH, Asked::Bundle(tag, id))
                }
                BundleWork::Publish { tag, manifest } => {
                    wire::header(wire::BUNDLE_PUBLISH, body);
                    manifest.encode(body);
                    (BUNDLE_PUBLISH_PATH, Asked::BundlePublish(tag, manifest.id))
                }
            }
        } else if !publishes.is_empty() {
            let mut bytes = 0;
            // alloc: what the batch publishes, kept with its request until the answer.
            let mut keys = Vec::new();
            wire::header(wire::PUBLISH_ASK, body);
            let count_at = body.len();
            body.put_u32(0);
            while let Some((_, _, value)) = publishes.front() {
                let len = value.len() as u64;
                if !keys.is_empty() && (!wire::takes(bytes, len) || keys.len() == BATCH_MOST) {
                    break;
                }
                bytes += len;
                let (tag, signed, value) = publishes.pop_front().expect("a front");
                body.extend_from_slice(&signed.encode());
                up.send_window.write(&value, body);
                keys.push((tag, signed.record.key));
            }
            body.patch_u32(count_at, keys.len() as u32);
            (PUBLISH_PATH, Asked::Publish(keys))
        } else if !fetches.is_empty() {
            let mut bytes = 0;
            // alloc: what the batch asks, kept with its request until the answer.
            let mut keys = Vec::new();
            while let Some((_, key)) = fetches.front() {
                let len = listed.get(key).map(|it| it.len);
                let most = if len.is_some() { BATCH_MOST } else { BATCH_KEYS };
                if !keys.is_empty() && (keys.len() >= most || !wire::takes(bytes, len.unwrap_or(0))) {
                    break;
                }
                bytes += len.unwrap_or(0);
                keys.push(fetches.pop_front().expect("a front"));
            }
            wire::fetch_ask_with(keys.len(), keys.iter().map(|(_, key)| *key), body);
            (FETCH_PATH, Asked::Fetch(keys))
        } else {
            return Ok(false);
        };
        head.clear();
        http::write_request_head(&endpoint.host, path, body.len(), head);
        up.channel.send(head)?;
        up.channel.send(body)?;
        up.asked.push_back(asked);
        Ok(true)
    }

    /// Sends what link `at` staged, and keeps a receive in flight, so a hangup is seen when
    /// nothing is owed.
    fn flush(&mut self, at: usize, effects: &mut Vec<Effect>) -> Result<(), String> {
        let Client { links, tags, .. } = self;
        let Link::Up(up) = &mut links[at] else { return Ok(()) };
        let up = &mut **up;
        up.channel.outgoing(&mut up.staged)?;
        if up.sending.is_none() && !up.staged.is_empty() {
            let tag = tags.next();
            up.sending = Some(tag);
            effects.push(Effect::Send { conn: up.conn, bytes: std::mem::take(&mut up.staged), tag });
        }
        if up.recv.is_none() {
            let tag = tags.next();
            up.recv = Some(tag);
            let owed = match http::response_head(&up.plain, BODY_MOST) {
                Ok(Some(head)) => (head.head + head.body).saturating_sub(up.plain.len()),
                _ => 0,
            };
            let most = owed.clamp(RECV_LEAST, RECV_MOST);
            // alloc: none here; the driver reserves the room the bytes come into.
            effects.push(Effect::Recv { conn: up.conn, most, into: Vec::new(), tag });
        }
        Ok(())
    }
}

/// Answers what `asked` asked from the response `body`, failing what it can't. An `Err` is
/// a response that is not one to what was asked: the connection can't be trusted after it.
fn answer(
    asked: Asked,
    body: &[u8],
    trust: &Trust,
    listed: &mut ByHash<Key, Record>,
    window: &mut RecvWindow,
    fetches: &mut VecDeque<(Tag, Key)>,
    outcomes: &mut Outcomes<'_>,
) -> Result<(), String> {
    match asked {
        Asked::Fetch(keys) => {
            let mut answered = 0;
            let mut reader = Reader::new(body);
            let result = fetched(&keys, &mut answered, &mut reader, trust, listed, window, fetches, outcomes);
            if let Err(why) = &result {
                // alloc: an error's message, made only when it fails.
                outcomes.fail(Asked::Fetch(keys[answered..].to_vec()), &format!("a malformed answer: {why}"));
            }
            result
        }
        Asked::Publish(keys) => {
            let checked = wire::parse_publish_answer(body, keys.len()).and_then(|statuses| {
                if statuses.len() != keys.len() {
                    // alloc: an error's message, made only when it fails.
                    return Err(format!("{} statuses for {} values", statuses.len(), keys.len()));
                }
                match keys.iter().zip(&statuses).find(|((_, key), (said, _))| said != key) {
                    Some(((_, key), (said, _))) => {
                        // alloc: an error's message, made only when it fails.
                        Err(format!("a status for {:x} in place of one for {:x}", said.0, key.0))
                    }
                    None => Ok(statuses),
                }
            });
            match checked {
                Ok(statuses) => {
                    for ((tag, key), (_, result)) in keys.into_iter().zip(statuses) {
                        outcomes.published.push(Published { tag, key, result });
                    }
                    Ok(())
                }
                Err(why) => {
                    // alloc: an error's message, made only when it fails.
                    outcomes.fail(Asked::Publish(keys), &format!("a malformed answer: {why}"));
                    Err(why)
                }
            }
        }
        Asked::Bundle(tag, id) => {
            let result = bundle_answer(&mut Reader::new(body), id, trust);
            if let Ok(Some(manifest)) = &result {
                listed.reserve(manifest.entries.len());
                for entry in &manifest.entries {
                    listed.insert(entry.key, *entry);
                }
            }
            let failed = result.as_ref().err().cloned();
            outcomes.got.push(GotBundle { tag, id, result });
            failed.map_or(Ok(()), Err)
        }
        Asked::BundlePublish(tag, id) => {
            let key = Key(id.0);
            let checked = wire::parse_publish_answer(body, 1).and_then(|statuses| match <[_; 1]>::try_from(statuses) {
                Ok([(said, result)]) if said == key => Ok(result),
                // alloc: an error's message, made only when it fails.
                _ => Err("an answer for another bundle".to_owned()),
            });
            match checked {
                Ok(result) => {
                    outcomes.published.push(Published { tag, key, result });
                    Ok(())
                }
                Err(why) => {
                    // alloc: an error's message, made only when it fails.
                    outcomes.fail(Asked::BundlePublish(tag, id), &format!("a malformed answer: {why}"));
                    Err(why)
                }
            }
        }
    }
}

fn bundle_answer(reader: &mut Reader<'_>, id: BundleId, trust: &Trust) -> Result<Option<Manifest>, String> {
    reader.header(wire::BUNDLE_ANSWER, "a bundle's answer")?;
    let manifest = match reader.u8().map_err(wire::truncated)? {
        wire::MISSING => None,
        wire::FOUND => {
            let manifest = Manifest::decode(reader)?;
            if manifest.id != id {
                // alloc: an error's message, made only when it fails.
                return Err(format!("the manifest of {:x} in place of {:x}", manifest.id.0, id.0));
            }
            // alloc: an error's message, made only when it fails.
            manifest.verify(trust).map_err(|why| format!("the manifest of {:x} is refused: {why}", id.0))?;
            Some(manifest)
        }
        // alloc: an error's message, made only when it fails.
        other => return Err(format!("an unknown answer {other}")),
    };
    reader.end("a bundle's answer")?;
    Ok(manifest)
}

/// The answers of one fetch, in the order asked; `answered` counts the keys answered, so on
/// an `Err` the caller fails the rest.
#[allow(clippy::too_many_arguments)]
fn fetched(
    keys: &[(Tag, Key)],
    answered: &mut usize,
    reader: &mut Reader<'_>,
    trust: &Trust,
    listed: &ByHash<Key, Record>,
    window: &mut RecvWindow,
    fetches: &mut VecDeque<(Tag, Key)>,
    outcomes: &mut Outcomes<'_>,
) -> Result<(), String> {
    reader.header(wire::FETCH_ANSWER, "a fetch's answer")?;
    let count = reader.count(33, keys.len(), "answers")?;
    if count != keys.len() {
        // alloc: an error's message, made only when it fails.
        return Err(format!("{count} answers for {} keys", keys.len()));
    }
    frame::task(MemTag::Store, |temp| {
        let mut deferred = Vec::new_in(temp);
        for &(tag, key) in keys {
            let which = reader.u8().map_err(wire::truncated)?;
            let said = reader.key()?;
            if said != key {
                // alloc: an error's message, made only when it fails.
                return Err(format!("an answer for {:x} in place of one for {:x}", said.0, key.0));
            }
            match which {
                wire::MISSING => {
                    outcomes.store.push(Completion { tag, done: Done::Read(Answer::Missing { key }.encode_framed()) });
                }
                wire::DEFERRED => deferred.push((tag, key)),
                wire::FOUND => {
                    let signed = reader.signed()?;
                    let record = signed.record;
                    if record.key != key {
                        // alloc: an error's message, made only when it fails.
                        return Err(format!("a record of {:x} in an answer for {:x}", record.key.0, key.0));
                    }
                    let checked = match listed.get(&key) {
                        Some(listed) if *listed == record => Ok(()),
                        Some(_) => Err(Refused { key, what: "its record is not the one its bundle's manifest lists".into() }),
                        None => trust.verify(&signed),
                    };
                    let value = window.read(reader, record.len, checked.is_ok())?;
                    // The bytes are hashed once, by the store, against the record's content:
                    // a value whose bytes are not that is refused there and never kept.
                    let done = match checked {
                        // alloc: an error's message, made only when it fails.
                        Err(refused) => Done::Failed(IoError(refused.to_string())),
                        Ok(()) => {
                            let bytes = value.expect("a value whose record is checked is kept");
                            Done::Read(Answer::Value { key, kind: record.kind, content: record.content, bytes }.encode_framed())
                        }
                    };
                    outcomes.store.push(Completion { tag, done });
                }
                // alloc: an error's message, made only when it fails.
                other => return Err(format!("an unknown answer {other} (0 missing, 1 deferred, 2 found)")),
            }
            *answered += 1;
        }
        reader.end("a fetch's answer")?;
        for it in deferred.into_iter().rev() {
            fetches.push_front(it);
        }
        Ok(())
    })
}

impl<C: Channels> Remote for Client<C> {
    /// Only for a key [`Client::fetch`] is asking for, or one a verified manifest lists: the
    /// cache is asked for nothing that was not shown public.
    fn fetch(&mut self, key: &Key, tag: Tag, effects: &mut Vec<Effect>) -> blaze_cas::Asked {
        let allowed = self.allowed == Some(*key) || self.listed.contains_key(key);
        assert!(allowed, "the global cache is asked for {:x}, which was not shown public", key.0);
        self.fetches.push_back((tag, *key));
        if !self.batching {
            self.pump_held(effects);
        }
        blaze_cas::Asked::Sent
    }
}

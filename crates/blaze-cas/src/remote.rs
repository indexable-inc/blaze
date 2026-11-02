//! The remote tier (docs/v2/plan/lanes/store.md, "Remote"): a machine outside the pool (a laptop)
//! reads values from the store of a project's VM daemon, read-through. It is never on the VM's
//! hot path. [`Server`] runs in the VM's daemon and answers from its [`Store`]; [`Client`] runs
//! on the laptop as the store's [`Remote`]. Both are sans-IO: they push [`Effect`]s and take
//! [`Completion`]s, and keep no thread, clock or socket of their own.
//!
//! The wire, fixed-width and little-endian:
//!
//! - An **ask** is `b"BLZASK"`, the version as a `u16` (3), and the 32 bytes of the [`Key`].
//!   A client sends asks back to back on one connection without waiting (pipelined), and the
//!   asks of one turn of fetches in one send.
//! - An **answer frame** is its length as a `u64`, then [`Answer::encode`]'s bytes: `b"BLZANS"`,
//!   the version as a `u16` (3), a byte saying which answer (0 missing, 1 value), the key, and
//!   for a value its [`Kind::code`], its content hash, its length as a `u64` and its bytes
//!   ([`Answer::encode_framed`] is the whole frame).
//! - The asks of a batch are answered in any order: the server sends missing and short answers
//!   first, gathered into one send, then each long value in a send of its own, so a long value
//!   holds up no short answer asked with it. The client matches an answer to its fetch by key,
//!   and hands the fetch the whole frame, length prefix included, which `Store::take_fetched`
//!   reads in place. Either side refuses a peer of another version.
//! - A **filter frame** (new in version 3) is framed as an answer is: `b"BLZANS"`, the version,
//!   2, a byte saying whether it replaces the filters before it (1) or adds to them (0), the
//!   [`Store::epoch`] it covers as a `u64`, and a [`Fuse8`] of the first word of each key the
//!   server's store holds. The server sends a full filter first on each connection, ahead of
//!   any answer, then a delta of the keys put since ([`Server::refresh`]), and a full one again
//!   once the deltas pass a quarter of the full one's keys. A client asks only for a key one of
//!   its filters may hold, and answers any other `Missing` itself: a miss costs no round trip.
//!   A client with no filter yet asks for every key.
//! - **The invariant** that rules out a false negative: every key [`Store::get`] gives when
//!   [`Server::refresh`] (or the full filter of a first peer) reads the store is in a filter
//!   frame queued to every peer, ahead of any answer served after it. A delta takes the keys
//!   readable and not yet packed ([`Store::unpacked_keys`]) as well as those packed since
//!   ([`Store::new_keys`]), since a put is readable at once and packed only at a later flush.
//!   One connection keeps its order, so a client that answers `Missing` itself does so only
//!   for a key the server's store could not give when it last refreshed before the last frame
//!   the client took: a key put after that is asked once the next delta comes. The daemon
//!   calls `refresh` before it hands the server a batch of asks to serve. [`FilterCounts`]
//!   says what the filters saved and what they let through.
//! - Bounds: a value is at most [`LONGEST_VALUE`] bytes, and a client refuses a frame longer
//!   than that and its header before reading it. A server hangs up a connection whose stream is
//!   not whole asks, and stops answering and reading a connection while its unsent answers pass
//!   a bound.
//!
//! **Trust**: the store checks an answer's bytes against its `content` before it keeps the
//! value, so a value damaged on the way is caught. It cannot check that the key really maps to
//! that value: a server that lies about what a key's value is would be believed. So the channel
//! is what is trusted, and it is the user's own VM over a Unix socket (mode 0600) or a loopback
//! port forwarded over SSH. There is no TLS or authentication here, which is why
//! [`Server::new`] refuses a TCP address that is not loopback.
//!
//! No fallback: a fetch that can't be answered (no connection, the server hung up, a malformed
//! or too-long frame) completes [`Done::Failed`] naming what went wrong, for the store to see.

use std::collections::{VecDeque, hash_map};

use blaze_core::{Address, Completion, Conn, Done, Effect, IoError, RECV_MOST, Tag, Tags};
use blaze_format::cursor::{self, Put as _, Reader};
use blaze_hash::{ByHashSet, HashMap};
use blaze_ids::{ContentId, fuse::Fuse8, radix};

use crate::{Asked, ByHash, Corrupt, Entry, Key, Kind, Remote, Store};

/// The most bytes of one value an answer may carry; a longer one is refused before it is read.
pub const LONGEST_VALUE: u64 = 1 << 30;

const VERSION: u16 = 3;
const ASK_MAGIC: &[u8; 6] = b"BLZASK";
const ANSWER_MAGIC: &[u8; 6] = b"BLZANS";
const MISSING: u8 = 0;
const VALUE: u8 = 1;
const FILTER: u8 = 2;
/// Magic, version, which frame, whether it replaces, epoch.
const FILTER_HEADER: usize = 8 + 1 + 1 + 8;
/// Deltas a client probes besides the full filter, at most: past it the server sends a full one.
const FILTER_DELTAS: usize = 16;

/// Magic, version and key.
const ASK_LEN: usize = 8 + 32;
/// Magic, version, which answer, key.
const MISSING_LEN: usize = 8 + 1 + 32;
/// Magic, version, which answer, key, kind, content, length.
const VALUE_HEADER: usize = 8 + 1 + 32 + 1 + 32 + 8;
/// The longest answer frame a client takes, not counting its length prefix.
const LONGEST_FRAME: u64 = LONGEST_VALUE + VALUE_HEADER as u64;
/// The fewest bytes a client asks of one receive.
const CLIENT_RECV: usize = 1 << 20;
/// The most bytes a client asks of one receive, while a long frame is owed.
const CLIENT_RECV_MOST: usize = 1 << 26;
/// The most bytes a server asks of one receive: a batch of asks.
const SERVER_RECV: usize = ASK_LEN * 1024;
/// A server answers and reads no more asks from a connection while more than this of its
/// answers are unsent, so it holds at most this and one frame for it.
const QUEUED_MOST: usize = 1 << 26;
/// An answer this long or longer is a send of its own, after the short answers of its batch.
const COALESCE: usize = 1 << 18;
/// Short answers are gathered into sends of about this many bytes.
const COALESCE_MOST: usize = 1 << 20;
/// The most room a send's buffer keeps to gather short answers again: a gathering is sent at
/// [`COALESCE_MOST`] plus at most one short answer, and its buffer at most doubled past that.
const SHORT_ROOM: usize = 2 * (COALESCE_MOST + COALESCE);

/// What the server says about one key.
#[derive(PartialEq, Eq, Debug)]
pub enum Answer {
    /// The server's store has no value of `key`.
    Missing { key: Key },
    /// `content` is `Entry::content` of the server's verified get.
    Value { key: Key, kind: Kind, content: ContentId, bytes: Vec<u8> },
}

impl Answer {
    /// The server's answer for `key` from `store`: its value through `Store::get` (verified), or
    /// `Missing`. A corrupt value is the `Err`, never sent.
    pub fn of(store: &Store, key: &Key) -> Result<Answer, Corrupt> {
        Ok(match store.get(key)? {
            Some(entry) => Answer::Value {
                key: *key,
                kind: entry.kind,
                content: entry.content,
                // alloc: the owned answer, for tests and tools; `Server` writes answers in place.
                bytes: entry.bytes.to_vec(),
            },
            None => Answer::Missing { key: *key },
        })
    }

    /// The answer's bytes on the wire, without the frame's length prefix.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.write(&mut out);
        out
    }

    /// The whole answer frame: [`Answer::encode`]'s length as a `u64`, then its bytes. A
    /// fetch's `Done::Read` carries this.
    pub fn encode_framed(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.encoded_len());
        write_frame(&mut out, |out| self.write(out));
        out
    }

    fn encoded_len(&self) -> usize {
        match self {
            Answer::Missing { .. } => MISSING_LEN,
            Answer::Value { bytes, .. } => VALUE_HEADER + bytes.len(),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Answer::Missing { key } => write_missing(out, key),
            Answer::Value { key, kind, content, bytes } => {
                write_value(out, key, *kind, content, bytes)
            }
        }
    }

    /// Refuses anything malformed, truncated, with trailing bytes, an unknown kind code, or a
    /// value longer than `LONGEST_VALUE`, saying why. It does not check `content` against
    /// `bytes`: the store does that before it keeps the value.
    pub fn decode(bytes: &[u8]) -> Result<Answer, String> {
        Ok(match parse(bytes)? {
            Parsed::Missing { key } => Answer::Missing { key },
            Parsed::Value { key, kind, content, bytes } => {
                // alloc: the owned answer, for tests and tools; the store reads frames by `parse`.
                Answer::Value { key, kind, content, bytes: bytes.to_vec() }
            }
        })
    }
}

/// An answer read in place.
pub(crate) enum Parsed<'a> {
    Missing { key: Key },
    Value { key: Key, kind: Kind, content: ContentId, bytes: &'a [u8] },
}

impl Parsed<'_> {
    fn key(&self) -> Key {
        match self {
            Parsed::Missing { key } | Parsed::Value { key, .. } => *key,
        }
    }
}

/// A read past the end of an answer, in the words `parse`'s errors use.
fn truncated(error: cursor::Error) -> String {
    // alloc: an error path.
    format!("truncated: {error}")
}

/// [`Answer::decode`] without the copy: an answer's bytes (not its frame) read in place.
pub(crate) fn parse(bytes: &[u8]) -> Result<Parsed<'_>, String> {
    let mut reader = Reader::new(bytes);
    read_header(&mut reader)?;
    let which = reader.u8().map_err(truncated)?;
    let key = Key(ContentId(*reader.array().map_err(truncated)?));
    let parsed = match which {
        MISSING => Parsed::Missing { key },
        VALUE => {
            let code = reader.u8().map_err(truncated)?;
            // alloc: an error path.
            let kind = Kind::from_code(code).ok_or_else(|| format!("an unknown kind code {code}"))?;
            let content = ContentId(*reader.array().map_err(truncated)?);
            let len = reader.u64().map_err(truncated)?;
            if len > LONGEST_VALUE {
                // alloc: an error path.
                return Err(format!(
                    "a value of {len} bytes, longer than the {LONGEST_VALUE} an answer may carry"
                ));
            }
            let bytes = reader.take(len as usize).map_err(truncated)?;
            Parsed::Value { key, kind, content, bytes }
        }
        // alloc: an error path.
        other => return Err(format!("an unknown answer {other} (0 is missing, 1 a value)")),
    };
    if !reader.is_done() {
        // alloc: an error path.
        return Err(format!("{} trailing bytes after the answer", reader.left()));
    }
    Ok(parsed)
}

/// Reads an answer's or a filter's magic and version, refusing another.
fn read_header(reader: &mut Reader<'_>) -> Result<(), String> {
    let header = reader.array::<8>().map_err(truncated)?;
    if header[..6] != ANSWER_MAGIC[..] {
        // alloc: an error path.
        return Err(format!("not an answer: its magic is {:?}", header[..6].escape_ascii().to_string()));
    }
    let version = u16::from_le_bytes([header[6], header[7]]);
    if version != VERSION {
        // alloc: an error path.
        return Err(format!("an answer of version {version}; this blaze speaks version {VERSION}"));
    }
    Ok(())
}

/// A filter frame's bytes (not its frame) read: whether it replaces the filters before it,
/// the epoch it covers, and the filter.
fn parse_filter(bytes: &[u8]) -> Result<(bool, u64, Fuse8), String> {
    let mut reader = Reader::new(bytes);
    read_header(&mut reader)?;
    let which = reader.u8().map_err(truncated)?;
    if which != FILTER {
        // alloc: an error path.
        return Err(format!("an answer {which} where a filter was read"));
    }
    let replaces = match reader.u8().map_err(truncated)? {
        0 => false,
        1 => true,
        // alloc: an error path.
        other => return Err(format!("a filter that neither adds (0) nor replaces (1) but says {other}")),
    };
    let epoch = reader.u64().map_err(truncated)?;
    // alloc: an error path.
    let filter = Fuse8::decode(reader.take_rest()).map_err(|why| format!("a filter that {why}"))?;
    Ok((replaces, epoch, filter))
}

/// The filter frame of `filter`, covering `epoch`.
fn filter_frame(replaces: bool, epoch: u64, filter: &Fuse8) -> Vec<u8> {
    // alloc: a filter's frame, once per batch of the store a laptop reads, shared by its peers
    let mut out = Vec::with_capacity(8 + FILTER_HEADER + filter.encoded_len());
    write_frame(&mut out, |out| {
        out.extend_from_slice(&header(ANSWER_MAGIC));
        out.push(FILTER);
        out.push(u8::from(replaces));
        out.put_u64(epoch);
        filter.encode(out);
    });
    out
}

/// What a filter holds of `key`: its first word, which is uniform since a key is a hash.
fn word(key: &Key) -> u64 {
    u64::from_le_bytes(*key.0.0.first_chunk().expect("a key is 32 bytes"))
}

/// The filter of `keys`, in a frame.
fn filter_of(replaces: bool, epoch: u64, keys: &[Key]) -> Vec<u8> {
    // alloc: a filter's keys, once per filter built
    filter_frame(replaces, epoch, &Fuse8::of(keys.iter().map(word).collect()))
}

fn header(magic: &[u8; 6]) -> [u8; 8] {
    let mut header = [0; 8];
    header[..6].copy_from_slice(magic);
    header[6..].copy_from_slice(&VERSION.to_le_bytes());
    header
}

fn write_missing(out: &mut Vec<u8>, key: &Key) {
    out.reserve(MISSING_LEN);
    out.extend_from_slice(&header(ANSWER_MAGIC));
    out.push(MISSING);
    out.extend_from_slice(&key.0.0);
}

fn write_value(out: &mut Vec<u8>, key: &Key, kind: Kind, content: &ContentId, bytes: &[u8]) {
    out.reserve(VALUE_HEADER + bytes.len());
    out.extend_from_slice(&header(ANSWER_MAGIC));
    out.push(VALUE);
    out.extend_from_slice(&key.0.0);
    out.push(kind.code());
    out.extend_from_slice(&content.0);
    out.put_u64(bytes.len() as u64);
    crate::copy::append(out, bytes);
}

/// The bytes before a value in its answer frame: the frame's length, then the value's header.
pub const VALUE_FRAME_HEADER: usize = 8 + VALUE_HEADER;

/// Writes into the first [`VALUE_FRAME_HEADER`] bytes of `head` what
/// [`Answer::encode_framed`] writes before a value of `len` bytes. A buffer holding the value
/// after that many reserved bytes is then the frame as it is, for `Done::Read`, with no copy.
pub fn write_value_frame_header(head: &mut [u8], key: &Key, kind: Kind, content: &ContentId, len: usize) {
    let head = &mut head[..VALUE_FRAME_HEADER];
    head[..8].copy_from_slice(&((VALUE_HEADER + len) as u64).to_le_bytes());
    head[8..16].copy_from_slice(&header(ANSWER_MAGIC));
    head[16] = VALUE;
    head[17..49].copy_from_slice(&key.0.0);
    head[49] = kind.code();
    head[50..82].copy_from_slice(&content.0);
    head[82..].copy_from_slice(&(len as u64).to_le_bytes());
}

fn write_ask(out: &mut Vec<u8>, key: &Key) {
    out.extend_from_slice(&header(ASK_MAGIC));
    out.extend_from_slice(&key.0.0);
}

/// Appends one frame to `out`: the length of what `write` appends, then that.
fn write_frame(out: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>)) {
    let start = out.len();
    out.put_u64(0);
    write(out);
    let len = (out.len() - start - 8) as u64;
    out.patch_u64(start, len);
}

/// Appends the answer frame for `key`, written from its value (a verified get) in place.
fn write_answer(out: &mut Vec<u8>, key: &Key, entry: Option<&Entry<'_>>) {
    write_frame(out, |out| match entry {
        Some(entry) => write_value(out, key, entry.kind, &entry.content, entry.bytes),
        None => write_missing(out, key),
    });
}

/// How many more bytes complete the frame `buffer` starts with: 0 until its length prefix is
/// whole, and for a prefix over [`LONGEST_FRAME`], which is refused, not read.
fn owed(buffer: &[u8]) -> usize {
    let Some(prefix) = buffer.first_chunk::<8>() else { return 0 };
    let len = u64::from_le_bytes(*prefix);
    if len > LONGEST_FRAME {
        return 0;
    }
    (8 + len as usize).saturating_sub(buffer.len())
}

/// What a client's filters did with its fetches since it was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FilterCounts {
    /// Fetches answered `Missing` without asking: the round trips the filters saved.
    pub skipped: u64,
    /// Fetches asked because a filter may hold the key, answered `Missing`: its false positives.
    pub false_positives: u64,
    /// Fetches asked because a filter may hold the key, answered with a value.
    pub found: u64,
    /// Fetches asked before any filter came.
    pub unfiltered: u64,
}

/// The laptop's side: one connection to `at`, asks pipelined on it, answers matched by key.
pub struct Client {
    at: Address,
    tags: Tags,
    link: Link,
    /// Asks not yet handed to a send.
    out: Vec<u8>,
    /// The fetch of each key asked on this connection and not yet answered (answers come in
    /// any order), and whether a filter said the server may hold it.
    waiting: ByHash<Key, (Tag, bool)>,
    /// Fetches of a key already waiting, not asked again: each gets a copy of its answer.
    again: Vec<(Key, Tag)>,
    /// Connects given up while in flight: a connection one of them makes is hung up.
    abandoned: Vec<Tag>,
    /// What the server's store holds: its last full filter and the deltas since, kept across
    /// connections (a store only gains keys). A key none may hold is `Missing` without asking.
    filters: Vec<Fuse8>,
    /// The [`Store::epoch`] the filters cover; `None` until the first comes.
    epoch: Option<u64>,
    counts: FilterCounts,
}

enum Link {
    Down,
    Connecting(Tag),
    Up(Up),
}

struct Up {
    conn: Conn,
    sending: Option<Tag>,
    recv: Option<Tag>,
    /// Received and not yet a whole frame.
    buffer: Vec<u8>,
    /// Empty, its room kept: the next receive's `into`.
    spare: Vec<u8>,
}

enum Role {
    Connect,
    Send,
    Recv,
    Abandoned,
    /// Of a connection already given up: its hangup, or a send or receive that was in flight.
    Stale,
}

impl Client {
    /// Tags it asks with are `tags..tags + 2^32`.
    pub fn new(at: Address, tags: u64) -> Client {
        Client {
            at,
            tags: Tags::new(tags),
            link: Link::Down,
            // alloc: empty, allocates nothing; once per client, setup.
            out: Vec::new(),
            waiting: ByHash::default(),
            // alloc: empty, allocates nothing; once per client, setup.
            again: Vec::new(),
            // alloc: empty, allocates nothing; once per client, setup.
            abandoned: Vec::new(),
            // alloc: empty, allocates nothing; once per client, setup.
            filters: Vec::new(),
            epoch: None,
            counts: FilterCounts::default(),
        }
    }

    /// What its filters did with its fetches so far.
    pub fn counts(&self) -> FilterCounts {
        self.counts
    }

    /// Whether `tag` is one it asks with, so its completion goes to [`Client::complete`].
    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    /// The epoch of the server's store its filters cover, once one has come.
    pub fn epoch(&self) -> Option<u64> {
        self.epoch
    }

    /// Whether the server's store may hold `key`, as its filters say: true with no filter yet.
    pub fn may_hold(&self, key: &Key) -> bool {
        let word = word(key);
        self.epoch.is_none() || self.filters.iter().any(|filter| filter.contains(word))
    }

    /// Takes a completion of its own, and appends to `answers` the answers to fetches: each a
    /// `Completion` whose tag is the tag given to `Remote::fetch`, with `Done::Read` of the
    /// answer's whole frame as received (its length as a `u64`, then [`Answer::encode`]'s
    /// bytes), or `Done::Failed(..)` naming what went wrong (could not connect, the server hung
    /// up, a malformed or too-long frame, an answer to nothing asked). The caller hands each to
    /// `Store::complete`.
    pub fn complete(
        &mut self,
        completion: Completion,
        effects: &mut Vec<Effect>,
        answers: &mut Vec<Completion>,
    ) {
        let Completion { tag, done } = completion;
        match self.role(tag) {
            Role::Abandoned => {
                self.abandoned.retain(|it| *it != tag);
                if let Done::Connected { conn } = done {
                    effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                }
            }
            Role::Connect => {
                self.link = Link::Down;
                match done {
                    Done::Connected { conn } => {
                        self.link = Link::Up(Up {
                            conn,
                            sending: None,
                            recv: None,
                            // alloc: empty, allocates nothing; once per connection.
                            buffer: Vec::new(),
                            // alloc: empty, allocates nothing; once per connection.
                            spare: Vec::new(),
                        });
                        self.pump(effects);
                    }
                    Done::Failed(error) => {
                        // alloc: an error path.
                        self.fail(&format!("could not connect: {error}"), answers, effects)
                    }
                    other => {
                        // alloc: an error path.
                        self.fail(&format!("a connect completed as {other:?}"), answers, effects)
                    }
                }
            }
            Role::Send => match done {
                Done::Sent(spare) => {
                    if let Link::Up(up) = &mut self.link {
                        up.sending = None;
                    }
                    // The buffer sent holds the next asks, its room kept.
                    if self.out.capacity() == 0 {
                        self.out = spare;
                    }
                    self.pump(effects);
                }
                Done::Broken { error, .. } | Done::Failed(error) => {
                    // alloc: an error path.
                    self.fail(&format!("sending asks failed: {error}"), answers, effects)
                }
                other => {
                    // alloc: an error path.
                    self.fail(&format!("a send completed as {other:?}"), answers, effects)
                }
            },
            Role::Recv => {
                if let Link::Up(up) = &mut self.link {
                    up.recv = None;
                }
                match done {
                    Done::Read(bytes) if bytes.is_empty() => {
                        let owed = self.waiting.len() + self.again.len();
                        // alloc: an error path.
                        let why = format!("the server hung up with {owed} answers owed");
                        self.fail(&why, answers, effects);
                    }
                    Done::Read(mut bytes) => {
                        if let Link::Up(up) = &mut self.link {
                            if up.buffer.is_empty() {
                                up.spare = std::mem::replace(&mut up.buffer, bytes);
                            } else {
                                up.buffer.extend_from_slice(&bytes);
                                bytes.clear();
                                up.spare = bytes;
                            }
                        }
                        match self.take_answers(answers) {
                            Ok(()) => self.pump(effects),
                            Err(why) => self.fail(&why, answers, effects),
                        }
                    }
                    Done::Broken { error, .. } | Done::Failed(error) => {
                        // alloc: an error path.
                        self.fail(&format!("receiving answers failed: {error}"), answers, effects)
                    }
                    other => {
                        // alloc: an error path.
                        self.fail(&format!("a receive completed as {other:?}"), answers, effects)
                    }
                }
            }
            Role::Stale => {}
        }
    }

    /// Hangs up; every fetch still waiting is answered `Done::Failed`, appended to `answers`.
    pub fn close(&mut self, effects: &mut Vec<Effect>, answers: &mut Vec<Completion>) {
        self.fail("the client closed", answers, effects);
    }

    fn role(&self, tag: Tag) -> Role {
        if self.abandoned.contains(&tag) {
            return Role::Abandoned;
        }
        match &self.link {
            Link::Connecting(connect) if *connect == tag => Role::Connect,
            Link::Up(up) if up.sending == Some(tag) => Role::Send,
            Link::Up(up) if up.recv == Some(tag) => Role::Recv,
            _ => Role::Stale,
        }
    }

    /// Sends what is unsent and receives while connected, for answers and the filters the
    /// server sends unasked, one of each in flight.
    fn pump(&mut self, effects: &mut Vec<Effect>) {
        let Link::Up(up) = &mut self.link else { return };
        if up.sending.is_none() && !self.out.is_empty() {
            let tag = self.tags.next();
            up.sending = Some(tag);
            effects.push(Effect::Send { conn: up.conn, bytes: std::mem::take(&mut self.out), tag });
        }
        if up.recv.is_none() {
            let tag = self.tags.next();
            up.recv = Some(tag);
            let most = owed(&up.buffer).clamp(CLIENT_RECV, CLIENT_RECV_MOST);
            let into = std::mem::take(&mut up.spare);
            effects.push(Effect::Recv { conn: up.conn, most, into, tag });
        }
    }

    /// Answers the waiting fetches whose frames have arrived whole, each with its whole frame. A
    /// long frame at the start of the buffer is handed on without a copy; a short one is copied
    /// out, so the buffer keeps its room for the next receive and is never split once per frame.
    fn take_answers(&mut self, answers: &mut Vec<Completion>) -> Result<(), String> {
        let Link::Up(up) = &mut self.link else { return Ok(()) };
        let mut start = 0;
        let result = loop {
            let rest = &up.buffer[start..];
            let Some(prefix) = rest.first_chunk::<8>() else { break Ok(()) };
            let len = u64::from_le_bytes(*prefix);
            if len > LONGEST_FRAME {
                // alloc: an error path.
                break Err(format!(
                    "an answer frame of {len} bytes, more than the {LONGEST_FRAME} allowed"
                ));
            }
            let end = 8 + len as usize;
            if rest.len() < end {
                break Ok(());
            }
            if end > 16 && rest[16] == FILTER {
                match parse_filter(&rest[8..end]) {
                    Ok((replaces, epoch, filter)) => {
                        if replaces {
                            self.filters.clear();
                        }
                        self.filters.push(filter);
                        self.epoch = Some(epoch);
                        start += end;
                        continue;
                    }
                    // alloc: an error path.
                    Err(why) => break Err(format!("a malformed filter: {why}")),
                }
            }
            let (key, missing) = match parse(&rest[8..end]) {
                Ok(parsed) => (parsed.key(), matches!(parsed, Parsed::Missing { .. })),
                // alloc: an error path.
                Err(why) => break Err(format!("a malformed answer: {why}")),
            };
            let Some((tag, filtered)) = self.waiting.remove(&key) else {
                // alloc: an error path.
                break Err(format!("an answer for {:x}, which was not asked", key.0));
            };
            match (filtered, missing) {
                (true, true) => self.counts.false_positives += 1,
                (true, false) => self.counts.found += 1,
                (false, _) => {}
            }
            let frame = if start == 0 && end >= CLIENT_RECV {
                if end == up.buffer.len() {
                    std::mem::take(&mut up.buffer)
                } else {
                    let after = up.buffer.split_off(end);
                    std::mem::replace(&mut up.buffer, after)
                }
            } else {
                // alloc: the answer's own frame, which `Done::Read` hands to the store to keep.
                let frame = up.buffer[start..start + end].to_vec();
                start += end;
                frame
            };
            if !self.again.is_empty() {
                self.again.retain(|&(asked, again)| {
                    if asked != key {
                        return true;
                    }
                    // clone: each fetch's `Done::Read` owns its frame; one per key fetched again.
                    answers.push(Completion { tag: again, done: Done::Read(frame.clone()) });
                    false
                });
            }
            answers.push(Completion { tag, done: Done::Read(frame) });
        };
        up.buffer.drain(..start);
        // What the next receive can bring, not the whole frame: a length prefix is only what the
        // peer says, and the buffer grows with the bytes that do come.
        up.buffer.reserve(owed(&up.buffer).min(RECV_MOST));
        result
    }

    /// Fails every waiting fetch with `why` and gives up the connection; the next fetch makes
    /// another.
    fn fail(&mut self, why: &str, answers: &mut Vec<Completion>, effects: &mut Vec<Effect>) {
        let at = &self.at;
        let failed = |key: Key, tag| Completion {
            tag,
            // alloc: an error path.
            done: Done::Failed(IoError(format!(
                "the remote store at {at}: {why}; asked for {:x}",
                key.0
            ))),
        };
        answers.extend(self.waiting.drain().map(|(key, (tag, _))| failed(key, tag)));
        answers.extend(self.again.drain(..).map(|(key, tag)| failed(key, tag)));
        match std::mem::replace(&mut self.link, Link::Down) {
            Link::Down => {}
            Link::Connecting(tag) => self.abandoned.push(tag),
            Link::Up(up) => effects.push(Effect::Hangup { conn: up.conn, tag: self.tags.next() }),
        }
        self.out.clear();
    }
}

impl Remote for Client {
    /// [`Asked::Missing`] for a key the server's filters say its store does not hold. Else
    /// connects first if it has no connection, then asks: in the send still in `effects` when
    /// there is one, so a turn of fetches is one send. A key already waiting is not asked again.
    fn fetch(&mut self, key: &Key, tag: Tag, effects: &mut Vec<Effect>) -> Asked {
        if !self.may_hold(key) {
            self.counts.skipped += 1;
            return Asked::Missing;
        }
        let filtered = self.epoch.is_some();
        match self.waiting.entry(*key) {
            hash_map::Entry::Occupied(_) => {
                self.again.push((*key, tag));
                return Asked::Sent;
            }
            hash_map::Entry::Vacant(slot) => {
                slot.insert((tag, filtered));
            }
        }
        if !filtered {
            self.counts.unfiltered += 1;
        }
        if let Link::Down = self.link {
            let connect = self.tags.next();
            // clone: once per connection, connecting is the edge.
            effects.push(Effect::Connect { at: self.at.clone(), tag: connect });
            self.link = Link::Connecting(connect);
        }
        let unsent = match &self.link {
            Link::Up(Up { sending: Some(sending), .. }) => {
                let sending = *sending;
                effects.iter_mut().rev().find_map(|effect| match effect {
                    Effect::Send { tag: send, bytes, .. } if *send == sending => Some(bytes),
                    _ => None,
                })
            }
            _ => None,
        };
        match unsent {
            Some(bytes) => write_ask(bytes, key),
            None => {
                write_ask(&mut self.out, key);
                self.pump(effects);
            }
        }
        Asked::Sent
    }
}

/// The VM daemon's side: listens at `at`, answers asks from `store`.
pub struct Server {
    at: Address,
    tags: Tags,
    listen: Option<Tag>,
    /// Listens unlistened and not yet ended: a connection one of them accepts is hung up.
    closing: Vec<Tag>,
    failed: Option<IoError>,
    /// Sorted by connection once, where [`Server::stop`] hangs them up.
    peers: HashMap<Conn, Peer>,
    /// The connection of each send and receive in flight.
    ops: HashMap<Tag, Conn>,
    /// The frames of the filter of its store a peer is sent when it connects: a full one, then
    /// the deltas since. Empty until the first peer.
    filters: Vec<Vec<u8>>,
    /// Keys the full filter was built of.
    full_keys: usize,
    /// Keys the deltas since were built of.
    delta_keys: usize,
    /// Keys a filter sent holds while they were readable and not yet packed: their packing,
    /// which [`Store::new_keys`] lists, sends them no second time.
    announced: ByHashSet<Key>,
}

#[derive(Default)]
struct Peer {
    recv: Option<Tag>,
    /// The send in flight and its length.
    sending: Option<(Tag, usize)>,
    out: VecDeque<Vec<u8>>,
    /// Bytes of answers not yet sent, the one in flight with them.
    queued: usize,
    /// Received and not yet a whole ask.
    buffer: Vec<u8>,
    /// Empty, its room kept: the next receive's `into`.
    spare: Vec<u8>,
    /// The short answers of the batch being served, gathered for one send.
    short: Vec<u8>,
    /// The long answers of the batch being served, each a send of its own after the short ones.
    long: Vec<Vec<u8>>,
}

impl Server {
    /// Refuses (Err, saying why) a TCP address that is not loopback: there is no TLS or
    /// authentication here, so it is reached through a Unix socket (mode 0600, made by the
    /// driver) or a loopback port forwarded over SSH. Tags are `tags..tags + 2^32`.
    pub fn new(at: Address, tags: u64) -> Result<Server, String> {
        if let Address::Tcp(addr) = &at
            && !addr.ip().to_canonical().is_loopback()
        {
            // alloc: an error path.
            return Err(format!(
                "the remote store would listen on {addr}, which is not loopback: it has no TLS or \
                 authentication, so it is served on a Unix socket or a loopback port forwarded over SSH"
            ));
        }
        Ok(Server {
            at,
            tags: Tags::new(tags),
            listen: None,
            // alloc: empty, allocates nothing; once per server, setup.
            closing: Vec::new(),
            failed: None,
            peers: HashMap::default(),
            ops: HashMap::default(),
            // alloc: empty, allocates nothing; once per server, setup.
            filters: Vec::new(),
            full_keys: 0,
            delta_keys: 0,
            announced: ByHashSet::default(),
        })
    }

    /// Sends every peer a filter of the keys `store` can give that no filter sent holds: those
    /// put and not yet packed, and those packed since the last call that were not. A delta, or
    /// a full filter again once the deltas pass a quarter of the full one's keys or
    /// [`FILTER_DELTAS`] of them. The daemon calls it once before serving, which has the store
    /// watch its keys (a peer of a store that does not is refused), then before each batch of
    /// asks it serves, so no answer goes ahead of a filter of what the store held then.
    pub fn refresh(&mut self, store: &mut Store, effects: &mut Vec<Effect>) {
        store.watch_keys();
        let (epoch, packed) = store.new_keys();
        // Before the first peer, the full filter it is sent will hold these.
        if self.filters.is_empty() {
            return;
        }
        let keys = self.unannounced(packed, store.unpacked_keys());
        if keys.is_empty() {
            return;
        }
        self.delta_keys += keys.len();
        let frame = if self.filters.len() > FILTER_DELTAS || 4 * self.delta_keys > self.full_keys {
            self.rebuild(store);
            // clone: the full filter kept for the next peer and sent now; once per rebuild
            self.filters[0].clone()
        } else {
            let frame = filter_of(false, epoch, &keys);
            // clone: the delta kept for the next peer and sent now; once per batch
            self.filters.push(frame.clone());
            frame
        };
        // alloc: once per batch, the peers sent the filter, in connection order.
        let mut conns: Vec<Conn> = self.peers.keys().copied().collect();
        radix::sort_copy_by_key(&mut conns, |it| it.0);
        for conn in conns {
            let peer = self.peers.get_mut(&conn).expect("a peer listed just now");
            peer.queued += frame.len();
            // clone: each peer's send owns its frame; once per peer a batch
            peer.out.push_back(frame.clone());
            self.pump(conn, effects);
        }
    }

    /// Of the keys `packed` since the last refresh and those `unpacked` now, sorted, the ones
    /// no filter sent holds; an unpacked one is remembered, so its packing sends nothing.
    fn unannounced(&mut self, packed: Vec<Key>, unpacked: Vec<Key>) -> Vec<Key> {
        // alloc: once per refresh, the keys of its delta
        let mut fresh = Vec::new();
        for key in packed {
            if !self.announced.remove(&key) {
                fresh.push(key);
            }
        }
        for key in unpacked {
            if self.announced.insert(key) {
                fresh.push(key);
            }
        }
        fresh.sort_unstable();
        fresh.dedup();
        fresh
    }

    /// Builds the full filter of `store` again, in place of the one and the deltas before.
    fn rebuild(&mut self, store: &Store) {
        // Read before the keys, which a writer may put to meanwhile: every key remembered is
        // then in the filter. Read after, a key put between the two would be remembered and
        // missing from it, and no delta would ever send it.
        let unpacked = store.unpacked_keys();
        let keys = store.keys();
        self.full_keys = keys.len();
        self.delta_keys = 0;
        self.announced.clear();
        self.announced.extend(unpacked);
        self.filters.clear();
        self.filters.push(filter_of(true, store.epoch(), &keys));
    }

    /// Asks to listen.
    pub fn start(&mut self, effects: &mut Vec<Effect>) {
        if self.listen.is_some() {
            return;
        }
        let tag = self.tags.next();
        self.listen = Some(tag);
        self.failed = None;
        // clone: once per listen, setup.
        effects.push(Effect::Listen { at: self.at.clone(), tag });
    }

    /// Whether `tag` is one it asks with, so its completion goes to [`Server::complete`].
    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.owns(tag)
    }

    /// Why it stopped listening, when the listen ended by itself rather than by
    /// [`Server::stop`]: the daemon is no longer serving the remote tier and says so.
    pub fn failed(&self) -> Option<&IoError> {
        self.failed.as_ref()
    }

    /// Takes a completion of its own; answers the whole asks it has from `store`, as many as
    /// [`QUEUED_MOST`] lets it queue (the rest once sends finish). Returns the
    /// corrupt values it found (and did not send; it answered `Missing` for them).
    pub fn complete(
        &mut self,
        completion: Completion,
        store: &Store,
        effects: &mut Vec<Effect>,
    ) -> Vec<Corrupt> {
        // alloc: empty, allocates nothing; pushed to only for a corrupt value, an error path.
        let mut corrupt = Vec::new();
        let Completion { tag, done } = completion;
        if self.listen == Some(tag) {
            match done {
                Done::Accepted { conn } => {
                    if !store.watching_keys() {
                        effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                        self.stop(effects);
                        // alloc: an error path.
                        self.failed = Some(IoError(format!(
                            "the store served at {} as the remote store does not watch its keys \
                             (Server::refresh before serving), so its filter would hide what is put",
                            self.at
                        )));
                        return corrupt;
                    }
                    if self.filters.is_empty() {
                        self.rebuild(store);
                    }
                    let mut peer = Peer::default();
                    for frame in &self.filters {
                        peer.queued += frame.len();
                        // clone: the peer's sends own their frames; once per filter a connection
                        peer.out.push_back(frame.clone());
                    }
                    self.peers.insert(conn, peer);
                    self.pump(conn, effects);
                }
                Done::Failed(error) => {
                    self.listen = None;
                    // alloc: an error path.
                    self.failed = Some(IoError(format!(
                        "listening at {} for the remote store: {error}",
                        self.at
                    )));
                }
                other => {
                    self.listen = None;
                    // alloc: an error path.
                    self.failed = Some(IoError(format!(
                        "listening at {} for the remote store completed as {other:?}",
                        self.at
                    )));
                }
            }
            return corrupt;
        }
        if self.closing.contains(&tag) {
            match done {
                Done::Accepted { conn } => {
                    effects.push(Effect::Hangup { conn, tag: self.tags.next() });
                }
                _ => self.closing.retain(|it| *it != tag),
            }
            return corrupt;
        }
        let Some(conn) = self.ops.remove(&tag) else { return corrupt };
        let Some(peer) = self.peers.get_mut(&conn) else { return corrupt };
        if peer.sending.is_some_and(|(sending, _)| sending == tag) {
            match done {
                Done::Sent(spare) => {
                    if let Some((_, len)) = peer.sending.take() {
                        peer.queued -= len;
                    }
                    // The buffer sent gathers the next short answers, its room kept, unless it
                    // was a long answer's.
                    if peer.short.capacity() == 0 && spare.is_empty() && spare.capacity() <= SHORT_ROOM {
                        peer.short = spare;
                    }
                    self.serve(conn, store, &mut corrupt, effects);
                }
                _ => self.drop_peer(conn, effects),
            }
        } else {
            peer.recv = None;
            match done {
                Done::Read(mut bytes) if !bytes.is_empty() => {
                    if peer.buffer.is_empty() {
                        peer.spare = std::mem::replace(&mut peer.buffer, bytes);
                    } else {
                        peer.buffer.extend_from_slice(&bytes);
                        bytes.clear();
                        peer.spare = bytes;
                    }
                    self.serve(conn, store, &mut corrupt, effects);
                }
                _ => self.drop_peer(conn, effects),
            }
        }
        corrupt
    }

    /// Unlistens and hangs up every connection.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        if let Some(tag) = self.listen.take() {
            effects.push(Effect::Unlisten { tag });
            self.closing.push(tag);
        }
        // alloc: once per stop, the edge.
        let mut conns: Vec<Conn> = self.peers.drain().map(|it| it.0).collect();
        radix::sort_copy_by_key(&mut conns, |it| it.0);
        for conn in conns {
            effects.push(Effect::Hangup { conn, tag: self.tags.next() });
        }
        self.ops.clear();
    }

    /// Answers the whole asks `conn` has sent while no more than [`QUEUED_MOST`] bytes of its
    /// answers are unsent (the rest wait for a send to finish): short answers gathered into one
    /// send ahead of the batch's long ones. Hangs it up at one that is not an ask.
    fn serve(
        &mut self,
        conn: Conn,
        store: &Store,
        corrupt: &mut Vec<Corrupt>,
        effects: &mut Vec<Effect>,
    ) {
        let Some(peer) = self.peers.get_mut(&conn) else { return };
        let ask_header = header(ASK_MAGIC);
        let mut start = 0;
        let mut broken = false;
        while peer.queued <= QUEUED_MOST && peer.buffer.len() - start >= ASK_LEN {
            let ask = &peer.buffer[start..start + ASK_LEN];
            if ask[..8] != ask_header {
                broken = true;
                break;
            }
            let mut id = [0; 32];
            id.copy_from_slice(&ask[8..]);
            let key = Key(ContentId(id));
            start += ASK_LEN;
            let entry = store.get(&key).unwrap_or_else(|found| {
                corrupt.push(found);
                None
            });
            let len = entry.as_ref().map_or(MISSING_LEN, |entry| VALUE_HEADER + entry.bytes.len());
            if len < COALESCE {
                write_answer(&mut peer.short, &key, entry.as_ref());
                if peer.short.len() >= COALESCE_MOST {
                    peer.out.push_back(std::mem::take(&mut peer.short));
                }
            } else {
                let mut frame = Vec::with_capacity(8 + len);
                write_answer(&mut frame, &key, entry.as_ref());
                peer.long.push(frame);
            }
            peer.queued += 8 + len;
        }
        peer.buffer.drain(..start);
        if !peer.short.is_empty() {
            peer.out.push_back(std::mem::take(&mut peer.short));
        }
        peer.out.extend(peer.long.drain(..));
        if broken {
            self.drop_peer(conn, effects);
        } else {
            self.pump(conn, effects);
        }
    }

    /// Sends the next answer and receives more asks while few enough answers are unsent, one of
    /// each in flight.
    fn pump(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(peer) = self.peers.get_mut(&conn) else { return };
        if peer.sending.is_none()
            && let Some(bytes) = peer.out.pop_front()
        {
            let tag = self.tags.next();
            peer.sending = Some((tag, bytes.len()));
            self.ops.insert(tag, conn);
            effects.push(Effect::Send { conn, bytes, tag });
        }
        if peer.recv.is_none() && peer.queued <= QUEUED_MOST {
            let tag = self.tags.next();
            peer.recv = Some(tag);
            self.ops.insert(tag, conn);
            let into = std::mem::take(&mut peer.spare);
            effects.push(Effect::Recv { conn, most: SERVER_RECV, into, tag });
        }
    }

    fn drop_peer(&mut self, conn: Conn, effects: &mut Vec<Effect>) {
        let Some(peer) = self.peers.remove(&conn) else { return };
        if let Some(tag) = peer.recv {
            self.ops.remove(&tag);
        }
        if let Some((tag, _)) = peer.sending {
            self.ops.remove(&tag);
        }
        effects.push(Effect::Hangup { conn, tag: self.tags.next() });
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use std::collections::VecDeque;

    use blaze_core::{
        Address, Completion, Conn, Done, Effect, FileName, IoError, Tag, run_until_idle,
        sim::{Faults, Sim},
    };
    use blaze_ids::ContentId;

    use super::*;
    use crate::{Config, Corrupt, Key, Kind, Remote, Store};

    const CLIENT: u64 = 1 << 32;
    const SERVER: u64 = 2 << 32;

    fn key(name: &str) -> Key {
        Key(ContentId::of("blaze v2 remote test key 1", name.as_bytes()))
    }

    fn socket() -> Address {
        Address::Unix(FileName::new("remote.sock").unwrap())
    }

    fn ask(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_ask(&mut out, &key(name));
        out
    }

    fn unframe(frame: &[u8]) -> Answer {
        let len = u64::from_le_bytes(frame[..8].try_into().unwrap()) as usize;
        assert_eq!(frame.len(), 8 + len);
        Answer::decode(&frame[8..]).unwrap()
    }

    fn framed(write: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, write);
        out
    }

    /// The frames `stream` holds, back to back.
    fn frames(mut stream: &[u8]) -> Vec<Answer> {
        let mut answers = Vec::new();
        while !stream.is_empty() {
            let len = u64::from_le_bytes(stream[..8].try_into().unwrap()) as usize;
            answers.push(unframe(&stream[..8 + len]));
            stream = &stream[8 + len..];
        }
        answers
    }

    /// An open store holding `values`, watching its keys for a server's filters.
    fn stored(values: &[(&str, Kind, &[u8])]) -> Store {
        opened(Sim::new(7, Faults::default()), values).1
    }

    /// [`stored`] in `sim`, kept for the test to commit puts in.
    fn opened(mut sim: Sim, values: &[(&str, Kind, &[u8])]) -> (Sim, Store) {
        let mut effects = Vec::new();
        let config =
            Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 0 };
        let mut store = Store::open(config, &mut effects);
        drive(&mut sim, &mut store, &mut effects);
        assert!(store.ready());
        store.watch_keys();
        for (name, kind, bytes) in values {
            store.put(key(name), *kind, bytes.to_vec()).unwrap();
        }
        (sim, store)
    }

    fn drive(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) {
        run_until_idle(sim, effects, |completion, effects| {
            assert!(store.owns(completion.tag));
            store.complete(completion, effects);
        });
    }

    /// Commits what was put, so its batch's keys are new keys.
    fn commit(sim: &mut Sim, store: &mut Store) {
        let mut effects = Vec::new();
        store.flush(&mut effects);
        drive(sim, store, &mut effects);
    }

    /// What `client.complete` appends.
    fn completed(client: &mut Client, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Completion> {
        let mut answers = Vec::new();
        client.complete(completion, effects, &mut answers);
        answers
    }

    fn failure(completion: &Completion) -> &str {
        match &completion.done {
            Done::Failed(IoError(why)) => why,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    fn answer(completion: &Completion) -> Answer {
        match &completion.done {
            Done::Read(frame) => unframe(frame),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    const C: usize = 0;
    const S: usize = 1;

    /// One end of a connection in the test's network.
    struct End {
        conn: Conn,
        open: bool,
        recv: Option<(Tag, usize)>,
        incoming: Vec<u8>,
    }

    /// A network between one client and one server, run by hand: each side's effects become
    /// completions, and bytes sent reach the peer's receives at most `chunk` at a time.
    struct Net {
        client: Client,
        server: Server,
        effects: [Vec<Effect>; 2],
        inbox: [VecDeque<Completion>; 2],
        listen: Option<Tag>,
        links: Vec<[End; 2]>,
        chunk: usize,
        /// A side whose receives wait.
        hold: [bool; 2],
        answers: Vec<Completion>,
        corrupt: Vec<Corrupt>,
        /// Fetches the client answered `Missing` itself, by its filters.
        filtered: Vec<Tag>,
    }

    impl Net {
        fn new(chunk: usize) -> Net {
            let mut server = Server::new(socket(), SERVER).unwrap();
            let mut server_effects = Vec::new();
            server.start(&mut server_effects);
            let mut net = Net {
                client: Client::new(socket(), CLIENT),
                server,
                effects: [Vec::new(), Vec::new()],
                inbox: [VecDeque::new(), VecDeque::new()],
                listen: None,
                links: Vec::new(),
                chunk,
                hold: [false; 2],
                answers: Vec::new(),
                corrupt: Vec::new(),
                filtered: Vec::new(),
            };
            // The server listens before the test's first fetch connects, as a daemon's does
            // before any client starts.
            for effect in server_effects {
                net.effect(S, effect);
            }
            net
        }

        /// A fetch the client answers `Missing` itself is answered as the store would read it.
        fn fetch(&mut self, name: &str, tag: u64) {
            self.fetch_key(&key(name), Tag(tag));
        }

        fn fetch_key(&mut self, key: &Key, tag: Tag) {
            if self.client.fetch(key, tag, &mut self.effects[C]) == Asked::Missing {
                self.filtered.push(tag);
                let frame = Answer::Missing { key: *key }.encode_framed();
                self.answers.push(Completion { tag, done: Done::Read(frame) });
            }
        }

        fn run(&mut self, store: &Store) {
            loop {
                let mut moved = false;
                for side in [C, S] {
                    for effect in std::mem::take(&mut self.effects[side]) {
                        moved = true;
                        self.effect(side, effect);
                    }
                }
                moved |= self.deliver();
                while let Some(completion) = self.inbox[C].pop_front() {
                    moved = true;
                    assert!(self.client.owns(completion.tag));
                    self.client.complete(completion, &mut self.effects[C], &mut self.answers);
                }
                while let Some(completion) = self.inbox[S].pop_front() {
                    moved = true;
                    assert!(self.server.owns(completion.tag));
                    let corrupt = self.server.complete(completion, store, &mut self.effects[S]);
                    self.corrupt.extend(corrupt);
                }
                if !moved {
                    return;
                }
            }
        }

        fn end(&mut self, side: usize, conn: Conn) -> &mut [End; 2] {
            self.links.iter_mut().find(|link| link[side].conn == conn).unwrap()
        }

        fn effect(&mut self, side: usize, effect: Effect) {
            let done = |tag, done| Completion { tag, done };
            match effect {
                Effect::Listen { at, tag } if side == S => {
                    assert_eq!(at, socket());
                    self.listen = Some(tag);
                }
                Effect::Unlisten { tag } if side == S => {
                    assert_eq!(self.listen.take(), Some(tag));
                    self.inbox[S].push_back(done(tag, Done::Unlistened));
                }
                Effect::Connect { at, tag } if side == C => {
                    assert_eq!(at, socket());
                    let Some(listen) = self.listen else {
                        let failed = Done::Failed(IoError("nothing listens there".into()));
                        self.inbox[C].push_back(done(tag, failed));
                        return;
                    };
                    let n = self.links.len() as u64;
                    let end = |conn| End { conn: Conn(conn), open: true, recv: None, incoming: Vec::new() };
                    self.links.push([end(2 * n), end(2 * n + 1)]);
                    self.inbox[C].push_back(done(tag, Done::Connected { conn: Conn(2 * n) }));
                    self.inbox[S].push_back(done(listen, Done::Accepted { conn: Conn(2 * n + 1) }));
                }
                Effect::Send { conn, mut bytes, tag } => {
                    let link = self.end(side, conn);
                    let open = link[side].open && link[1 - side].open;
                    if open {
                        link[1 - side].incoming.extend_from_slice(&bytes);
                    }
                    bytes.clear();
                    let sent = match open {
                        true => Done::Sent(bytes),
                        false => Done::Broken { buffer: bytes, error: IoError("the peer hung up".into()) },
                    };
                    self.inbox[side].push_back(done(tag, sent));
                }
                Effect::Recv { conn, most, tag, .. } => {
                    let link = self.end(side, conn);
                    assert!(link[side].open, "a receive on a connection hung up");
                    assert!(link[side].recv.is_none(), "two receives in flight on one connection");
                    link[side].recv = Some((tag, most));
                }
                Effect::Hangup { conn, tag } => {
                    let link = self.end(side, conn);
                    link[side].open = false;
                    let recv = link[side].recv.take();
                    if let Some((recv, _)) = recv {
                        self.inbox[side].push_back(done(recv, Done::Read(Vec::new())));
                    }
                    self.inbox[side].push_back(done(tag, Done::HungUp));
                }
                other => panic!("side {side} asked for {other:?}"),
            }
        }

        fn deliver(&mut self) -> bool {
            let mut moved = false;
            for link in &mut self.links {
                for side in [C, S] {
                    if self.hold[side] {
                        continue;
                    }
                    let Some((tag, most)) = link[side].recv else { continue };
                    let read = if !link[side].incoming.is_empty() {
                        let n = most.min(self.chunk).min(link[side].incoming.len());
                        link[side].incoming.drain(..n).collect()
                    } else if !link[1 - side].open {
                        Vec::new()
                    } else {
                        continue;
                    };
                    link[side].recv = None;
                    self.inbox[side].push_back(Completion { tag, done: Done::Read(read) });
                    moved = true;
                }
            }
            moved
        }
    }

    fn effect_on(effects: &[Effect], conn: Conn, recv: bool) -> Option<Tag> {
        effects.iter().find_map(|effect| match effect {
            Effect::Recv { conn: on, tag, .. } if recv && *on == conn => Some(*tag),
            Effect::Send { conn: on, tag, .. } if !recv && *on == conn => Some(*tag),
            _ => None,
        })
    }

    fn sent_on(effects: &[Effect], conn: Conn) -> Option<&[u8]> {
        effects.iter().find_map(|effect| match effect {
            Effect::Send { conn: on, bytes, .. } if *on == conn => Some(&bytes[..]),
            _ => None,
        })
    }

    fn hung_up(effects: &[Effect], conn: Conn) -> bool {
        effects.iter().any(|effect| matches!(effect, Effect::Hangup { conn: on, .. } if *on == conn))
    }

    fn samples() -> Vec<Answer> {
        let mut answers = vec![Answer::Missing { key: key("gone") }];
        for (n, kind) in Kind::ALL.into_iter().enumerate() {
            let bytes: Vec<u8> = (0..n * 37).map(|it| it as u8).collect();
            answers.push(Answer::Value {
                key: key(kind.name()),
                kind,
                content: ContentId::of("blaze v2 remote test content 1", &bytes),
                bytes,
            });
        }
        answers
    }

    #[test]
    fn answers_round_trip() {
        for answer in samples() {
            assert_eq!(Answer::decode(&answer.encode()), Ok(answer));
        }
    }

    #[test]
    fn a_value_frame_header_is_what_encode_framed_writes_before_the_value() {
        for answer in samples() {
            let Answer::Value { key, kind, content, bytes } = &answer else { continue };
            let mut built = vec![0; VALUE_FRAME_HEADER];
            built.extend_from_slice(bytes);
            write_value_frame_header(&mut built, key, *kind, content, bytes.len());
            assert_eq!(built, answer.encode_framed(), "{answer:?}");
        }
    }

    #[test]
    fn a_framed_answer_is_its_length_then_its_encoding() {
        for answer in samples() {
            let body = answer.encode();
            let whole = answer.encode_framed();
            assert_eq!(whole[..8], (body.len() as u64).to_le_bytes());
            assert_eq!(whole[8..], body[..]);
            assert_eq!(whole, framed(|out| answer.write(out)));
            assert_eq!(unframe(&whole), answer);
            match (parse(&whole[8..]).unwrap(), &answer) {
                (Parsed::Missing { key }, Answer::Missing { key: asked }) => assert_eq!(key, *asked),
                (
                    Parsed::Value { key, kind, content, bytes },
                    Answer::Value { key: asked, kind: sent, content: hashed, bytes: value },
                ) => {
                    assert_eq!((key, kind, content), (*asked, *sent, *hashed));
                    assert_eq!(bytes, &value[..]);
                }
                _ => panic!("{answer:?} parsed as another answer"),
            }
        }
    }

    #[test]
    fn decode_refuses_what_is_not_an_answer() {
        let value = Answer::Value {
            key: key("a"),
            kind: Kind::Code,
            content: ContentId::of("blaze v2 remote test content 1", b"abc"),
            bytes: b"abc".to_vec(),
        };
        let good = value.encode();
        let refused = |bytes: &[u8], why: &str| {
            let error = Answer::decode(bytes).unwrap_err();
            assert!(error.contains(why), "{error:?} does not say {why:?}");
        };

        let mut magic = good.clone();
        magic[0] = b'X';
        refused(&magic, "magic");
        let mut ask_magic = good.clone();
        ask_magic[..6].copy_from_slice(ASK_MAGIC);
        refused(&ask_magic, "magic");

        let mut version = good.clone();
        version[6] = 1;
        refused(&version, "version 1");

        let mut which = good.clone();
        which[8] = 7;
        refused(&which, "unknown answer 7");

        let mut kind = good.clone();
        kind[8 + 1 + 32] = 200;
        refused(&kind, "unknown kind code 200");

        for answer in samples() {
            let bytes = answer.encode();
            for len in 0..bytes.len() {
                refused(&bytes[..len], "truncated");
            }
            let mut trailing = bytes.clone();
            trailing.push(0);
            refused(&trailing, "1 trailing bytes");
        }

        let mut long = good[..VALUE_HEADER].to_vec();
        long[VALUE_HEADER - 8..].copy_from_slice(&(LONGEST_VALUE + 1).to_le_bytes());
        refused(&long, "longer than");
        let mut longest = long.clone();
        longest[VALUE_HEADER - 8..].copy_from_slice(&LONGEST_VALUE.to_le_bytes());
        refused(&longest, "truncated");
    }

    #[test]
    fn a_fetch_is_answered_with_a_value_and_one_with_missing() {
        let store = stored(&[("a", Kind::Code, b"the code of a".as_slice())]);
        let mut net = Net::new(usize::MAX);
        net.fetch("a", 1);
        net.fetch("b", 2);
        net.run(&store);

        assert_eq!(net.answers.len(), 2);
        assert_eq!(net.answers[0].tag, Tag(1));
        let content = store.get(&key("a")).unwrap().unwrap().content;
        assert_eq!(
            answer(&net.answers[0]),
            Answer::Value { key: key("a"), kind: Kind::Code, content, bytes: b"the code of a".to_vec() }
        );
        assert_eq!(answer(&net.answers[0]), Answer::of(&store, &key("a")).unwrap());
        assert_eq!(net.answers[1].tag, Tag(2));
        assert_eq!(answer(&net.answers[1]), Answer::Missing { key: key("b") });
        assert!(net.corrupt.is_empty());
        assert_eq!(net.links.len(), 1);
    }

    /// Kinds the store keeps whole, not as chunks.
    const PLAIN: [Kind; 4] = [Kind::Query, Kind::Code, Kind::Debug, Kind::Run];

    fn many() -> (Store, Vec<String>) {
        let names: Vec<String> = (0..24).map(|n| format!("value {n}")).collect();
        let bodies: Vec<Vec<u8>> =
            (0..24).map(|n| (0..n * 131).map(|it| (it * 7 + n) as u8).collect()).collect();
        let kept: Vec<(&str, Kind, &[u8])> = names
            .iter()
            .zip(&bodies)
            .enumerate()
            .filter(|(n, _)| n % 3 != 1)
            .map(|(n, (name, bytes))| (name.as_str(), PLAIN[n % PLAIN.len()], &bytes[..]))
            .collect();
        (stored(&kept), names)
    }

    fn pipelined(chunk: usize) {
        let (store, names) = many();
        let mut net = Net::new(chunk);
        for (n, name) in names.iter().enumerate() {
            net.fetch(name, 100 + n as u64);
        }
        net.run(&store);

        assert_eq!(net.answers.len(), names.len());
        net.answers.sort_by_key(|it| it.tag);
        for (n, (completion, name)) in net.answers.iter().zip(&names).enumerate() {
            assert_eq!(completion.tag, Tag(100 + n as u64));
            let expected = Answer::of(&store, &key(name)).unwrap();
            assert_eq!(matches!(expected, Answer::Missing { .. }), n % 3 == 1);
            assert_eq!(answer(completion), expected);
        }
        assert_eq!(net.links.len(), 1);
    }

    #[test]
    fn pipelined_asks_are_all_answered() {
        pipelined(usize::MAX);
    }

    #[test]
    fn answers_reassemble_from_any_split() {
        for chunk in [1, 2, 7, 39, 40, 41, 83, 4096] {
            pipelined(chunk);
        }
    }

    #[test]
    fn asks_between_answers_reuse_the_connection() {
        let (store, names) = many();
        let mut net = Net::new(5);
        for (n, name) in names.iter().enumerate() {
            net.fetch(name, n as u64);
            net.run(&store);
            assert_eq!(net.answers.len(), n + 1);
            assert_eq!(answer(&net.answers[n]), Answer::of(&store, &key(name)).unwrap());
        }
        assert_eq!(net.links.len(), 1);
    }

    #[test]
    fn a_server_hang_up_fails_the_waiting_fetches_and_the_next_reconnects() {
        let store = stored(&[("a", Kind::Run, b"a run".as_slice())]);
        let mut net = Net::new(3);
        net.hold[S] = true;
        net.fetch("a", 1);
        net.fetch("b", 2);
        net.run(&store);
        assert!(net.answers.is_empty());

        net.server.stop(&mut net.effects[S]);
        net.run(&store);
        assert_eq!(net.answers.len(), 2);
        net.answers.sort_by_key(|it| it.tag);
        for (completion, tag) in net.answers.iter().zip([1, 2]) {
            assert_eq!(completion.tag, Tag(tag));
            assert!(failure(completion).contains("hung up"), "{completion:?}");
            assert!(failure(completion).contains("unix:remote.sock"), "{completion:?}");
        }
        assert!(net.server.failed().is_none());

        net.answers.clear();
        net.hold[S] = false;
        net.server.start(&mut net.effects[S]);
        net.run(&store);
        net.fetch("a", 3);
        net.run(&store);
        assert_eq!(net.links.len(), 2);
        assert_eq!(net.answers.len(), 1);
        assert_eq!(net.answers[0].tag, Tag(3));
        assert_eq!(answer(&net.answers[0]), Answer::of(&store, &key("a")).unwrap());
    }

    #[test]
    fn a_fetch_with_nothing_listening_fails_and_the_next_tries_again() {
        let store = stored(&[("a", Kind::Query, b"a query".as_slice())]);
        let mut net = Net::new(usize::MAX);
        net.server.stop(&mut net.effects[S]);
        net.run(&store);
        net.fetch("a", 1);
        net.run(&store);
        assert_eq!(net.answers.len(), 1);
        assert!(failure(&net.answers[0]).contains("could not connect"));

        net.answers.clear();
        net.server.start(&mut net.effects[S]);
        net.run(&store);
        net.fetch("a", 2);
        net.run(&store);
        assert_eq!(net.answers.len(), 1);
        assert_eq!(answer(&net.answers[0]), Answer::of(&store, &key("a")).unwrap());
    }

    /// A client with fetches of `names` asked and connected as `conn`, its send done.
    fn connected(names: &[&str], conn: Conn) -> (Client, Vec<Effect>, Tag) {
        let mut client = Client::new(socket(), CLIENT);
        let mut effects = Vec::new();
        for (n, name) in names.iter().enumerate() {
            client.fetch(&key(name), Tag(n as u64), &mut effects);
        }
        let [Effect::Connect { tag: connect, .. }] = effects[..] else {
            panic!("expected one connect, got {effects:?}")
        };
        effects.clear();
        let none = completed(&mut client, Completion { tag: connect, done: Done::Connected { conn } }, &mut effects);
        assert!(none.is_empty());
        let send = effect_on(&effects, conn, false).unwrap();
        let recv = effect_on(&effects, conn, true).unwrap();
        let asked: Vec<u8> = names.iter().flat_map(|name| ask(name)).collect();
        assert_eq!(sent_on(&effects, conn), Some(&asked[..]));
        effects.clear();
        assert!(completed(&mut client, Completion { tag: send, done: Done::Sent(Vec::new()) }, &mut effects).is_empty());
        (client, effects, recv)
    }

    #[test]
    fn a_client_refuses_a_frame_too_long_before_reading_it() {
        let conn = Conn(5);
        let (mut client, mut effects, recv) = connected(&["a", "b"], conn);
        let prefix = (LONGEST_FRAME + 1).to_le_bytes().to_vec();
        let answers = completed(&mut client, Completion { tag: recv, done: Done::Read(prefix) }, &mut effects);
        assert_eq!(answers.len(), 2);
        for answer in &answers {
            assert!(failure(answer).contains("more than"), "{answer:?}");
        }
        assert!(hung_up(&effects, conn));

        effects.clear();
        client.fetch(&key("a"), Tag(9), &mut effects);
        assert!(matches!(effects[..], [Effect::Connect { .. }]), "{effects:?}");
    }

    #[test]
    fn a_long_answer_is_received_in_reads_of_what_it_still_needs() {
        let conn = Conn(4);
        let (mut client, mut effects, recv) = connected(&["a"], conn);
        let bytes = vec![7; 10 << 20];
        let content = ContentId::of("blaze v2 remote test content 1", &bytes);
        let frame = framed(|out| write_value(out, &key("a"), Kind::Code, &content, &bytes));
        let first = Completion { tag: recv, done: Done::Read(frame[..100].to_vec()) };
        assert!(completed(&mut client, first, &mut effects).is_empty());
        let [Effect::Recv { most, tag: mut rest, .. }] = effects[..] else { panic!("{effects:?}") };
        assert_eq!(most, frame.len() - 100);
        let pieces: Vec<&[u8]> = frame[100..].chunks(RECV_MOST).collect();
        let (last, before) = pieces.split_last().expect("the frame is longer than its first read");
        for piece in before {
            effects.clear();
            let read = Completion { tag: rest, done: Done::Read(piece.to_vec()) };
            assert!(completed(&mut client, read, &mut effects).is_empty());
            let [Effect::Recv { tag, .. }] = effects[..] else { panic!("{effects:?}") };
            rest = tag;
            let Link::Up(up) = &client.link else { panic!("the connection is up") };
            assert!(up.buffer.capacity() >= up.buffer.len() + RECV_MOST.min(frame.len() - up.buffer.len()));
            assert!(up.buffer.capacity() <= 2 * (up.buffer.len() + RECV_MOST), "{}", up.buffer.capacity());
        }
        let Link::Up(up) = &client.link else { panic!("the connection is up") };
        let buffer = up.buffer.as_ptr();

        effects.clear();
        let last = Completion { tag: rest, done: Done::Read(last.to_vec()) };
        let answers = completed(&mut client, last, &mut effects);
        assert_eq!(answers, vec![Completion { tag: Tag(0), done: Done::Read(frame.clone()) }]);
        let Done::Read(handed) = &answers[0].done else { panic!("{answers:?}") };
        assert_eq!(handed.as_ptr(), buffer, "the frame is handed on without a copy");
        let [Effect::Recv { most, .. }] = effects[..] else { panic!("it receives on for filters: {effects:?}") };
        assert_eq!(most, CLIENT_RECV, "nothing is owed, so the least is asked");

        assert_eq!(owed(&(LONGEST_FRAME + 1).to_le_bytes()), 0);

        // A prefix that says a gigabyte reserves what one receive brings, not the gigabyte.
        let (mut client, mut effects, recv) = connected(&["a"], Conn(5));
        let prefix = Completion { tag: recv, done: Done::Read((1u64 << 30).to_le_bytes().to_vec()) };
        assert!(completed(&mut client, prefix, &mut effects).is_empty());
        let Link::Up(up) = &client.link else { panic!("the connection is up") };
        assert!(up.buffer.capacity() <= 8 + RECV_MOST, "{}", up.buffer.capacity());
        let [Effect::Recv { most, .. }] = effects[..] else { panic!("{effects:?}") };
        assert_eq!(most, CLIENT_RECV_MOST);
        assert_eq!(owed(&[1, 0, 0]), 0);
    }

    #[test]
    fn a_client_refuses_a_malformed_or_misplaced_answer() {
        let garbage = framed(|out| out.extend_from_slice(b"not an answer"));
        let other_key = framed(|out| write_missing(out, &key("b")));
        let unasked = [framed(|out| write_missing(out, &key("a"))), other_key.clone()].concat();
        for (stream, why, failed) in [
            (garbage, "malformed", 1),
            (other_key, "not asked", 1),
            (unasked, "not asked", 0),
        ] {
            let conn = Conn(6);
            let (mut client, mut effects, recv) = connected(&["a"], conn);
            let answers = completed(&mut client, Completion { tag: recv, done: Done::Read(stream) }, &mut effects);
            let failures: Vec<&Completion> =
                answers.iter().filter(|it| matches!(it.done, Done::Failed(_))).collect();
            assert_eq!(failures.len(), failed, "{answers:?}");
            if let Some(first) = failures.first() {
                assert!(failure(first).contains(why), "{first:?}");
            }
            assert!(hung_up(&effects, conn), "{why}: {effects:?}");
        }
    }

    #[test]
    fn a_client_closing_fails_what_waits() {
        let conn = Conn(7);
        let (mut client, mut effects, _) = connected(&["a"], conn);
        let mut answers = Vec::new();
        client.close(&mut effects, &mut answers);
        assert_eq!(answers.len(), 1);
        assert!(failure(&answers[0]).contains("closed"));
        assert!(hung_up(&effects, conn));

        let mut client = Client::new(socket(), CLIENT);
        let mut effects = Vec::new();
        client.fetch(&key("a"), Tag(1), &mut effects);
        client.fetch(&key("a"), Tag(2), &mut effects);
        let [Effect::Connect { tag: connect, .. }] = effects[..] else { panic!("{effects:?}") };
        effects.clear();
        answers.clear();
        client.close(&mut effects, &mut answers);
        answers.sort_by_key(|it| it.tag);
        assert_eq!(answers.iter().map(|it| it.tag).collect::<Vec<_>>(), [Tag(1), Tag(2)]);
        let late = Completion { tag: connect, done: Done::Connected { conn: Conn(8) } };
        assert!(completed(&mut client, late, &mut effects).is_empty());
        assert!(hung_up(&effects, Conn(8)), "a connection made after closing is hung up");
    }

    #[test]
    fn stop_hangs_up_every_connection_in_connection_order() {
        let store = stored(&[]);
        let mut server = Server::new(socket(), SERVER).unwrap();
        let mut effects = Vec::new();
        server.start(&mut effects);
        let Some(Effect::Listen { tag: listen, .. }) = effects.pop() else { panic!("{effects:?}") };
        let conns: Vec<u64> = (0..40u64).map(|n| n * 17 % 41 + 3).collect();
        for &conn in &conns {
            let accepted = Completion { tag: listen, done: Done::Accepted { conn: Conn(conn) } };
            assert!(server.complete(accepted, &store, &mut effects).is_empty());
        }
        effects.clear();
        server.stop(&mut effects);
        let hung_up: Vec<(Conn, Tag)> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Hangup { conn, tag } => Some((*conn, *tag)),
                _ => None,
            })
            .collect();
        let mut want: Vec<Conn> = conns.iter().map(|it| Conn(*it)).collect();
        want.sort();
        assert_eq!(hung_up.iter().map(|it| it.0).collect::<Vec<_>>(), want);
        assert!(hung_up.windows(2).all(|it| it[0].1 < it[1].1), "{hung_up:?}");
        assert!(server.peers.is_empty() && server.ops.is_empty());
    }

    #[test]
    fn the_asks_of_one_turn_are_one_send() {
        let conn = Conn(11);
        let (mut client, mut effects, _) = connected(&["first"], conn);
        assert!(effects.is_empty());
        let names: Vec<String> = (0..24).map(|n| format!("turn {n}")).collect();
        for (n, name) in names.iter().enumerate() {
            client.fetch(&key(name), Tag(100 + n as u64), &mut effects);
        }
        let sends: Vec<&[u8]> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send { bytes, .. } => Some(&bytes[..]),
                _ => None,
            })
            .collect();
        let asked: Vec<u8> = names.iter().flat_map(|name| ask(name)).collect();
        assert_eq!(asked.len(), 24 * ASK_LEN);
        assert_eq!(sends, [&asked[..]]);
        assert!(client.out.is_empty());
    }

    #[test]
    fn short_answers_are_not_held_up_by_a_long_one() {
        let long = vec![3; 10 << 20];
        let store = stored(&[
            ("long", Kind::Code, &long[..]),
            ("a", Kind::Query, b"a query".as_slice()),
            ("b", Kind::Run, b"a run".as_slice()),
        ]);
        let mut net = Net::new(usize::MAX);
        let names = ["long", "a", "b", "c"];
        for (n, name) in names.iter().enumerate() {
            net.fetch(name, n as u64);
        }
        net.run(&store);

        let tags: Vec<Tag> = net.answers.iter().map(|it| it.tag).collect();
        assert_eq!(tags.len(), 4);
        assert_eq!(tags[3], Tag(0), "the long answer comes last: {tags:?}");
        net.answers.sort_by_key(|it| it.tag);
        for (completion, name) in net.answers.iter().zip(names) {
            assert_eq!(answer(completion), Answer::of(&store, &key(name)).unwrap());
        }
        assert_eq!(net.links.len(), 1);
    }

    #[test]
    fn a_key_fetched_twice_is_asked_once_and_both_are_answered() {
        let conn = Conn(12);
        let (mut client, mut effects, recv) = connected(&["a"], conn);
        client.fetch(&key("a"), Tag(5), &mut effects);
        assert!(effects.is_empty(), "nothing more is asked: {effects:?}");
        assert!(client.out.is_empty());

        let frame = Answer::Missing { key: key("a") }.encode_framed();
        let mut answers = completed(&mut client, Completion { tag: recv, done: Done::Read(frame.clone()) }, &mut effects);
        answers.sort_by_key(|it| it.tag);
        assert_eq!(
            answers,
            [Tag(0), Tag(5)].map(|tag| Completion { tag, done: Done::Read(frame.clone()) })
        );
        assert!(matches!(effects[..], [Effect::Recv { .. }]), "it receives on for filters: {effects:?}");
    }

    /// A started server that has accepted `conns` and sent each its filter, and the tag of
    /// each one's receive.
    fn serving(conns: &[Conn]) -> (Server, Vec<Effect>, Vec<Tag>) {
        let mut server = Server::new(socket(), SERVER).unwrap();
        let mut effects = Vec::new();
        server.start(&mut effects);
        let [Effect::Listen { tag: listen, .. }] = effects[..] else { panic!("{effects:?}") };
        let store = stored(&[]);
        let mut recvs = Vec::new();
        for conn in conns {
            effects.clear();
            let accepted = Completion { tag: listen, done: Done::Accepted { conn: *conn } };
            assert!(server.complete(accepted, &store, &mut effects).is_empty());
            recvs.push(effect_on(&effects, *conn, true).unwrap());
            let (replaces, epoch, _) = parse_filter(&sent_on(&effects, *conn).unwrap()[8..]).unwrap();
            assert!(replaces && epoch == store.epoch(), "a full filter first");
            let send = effect_on(&effects, *conn, false).unwrap();
            effects.clear();
            let sent = Completion { tag: send, done: Done::Sent(Vec::new()) };
            assert!(server.complete(sent, &store, &mut effects).is_empty());
            assert!(effects.is_empty(), "{effects:?}");
        }
        effects.clear();
        (server, effects, recvs)
    }

    #[test]
    fn a_server_answers_each_connection_on_its_own() {
        let store = stored(&[
            ("a", Kind::Debug, b"debug of a".as_slice()),
            ("b", Kind::Query, b"a query".as_slice()),
        ]);
        let (one, two) = (Conn(1), Conn(2));
        let (mut server, mut effects, recvs) = serving(&[one, two]);
        for (conn, recv, name) in [(one, recvs[0], "a"), (two, recvs[1], "b")] {
            effects.clear();
            let read = Completion { tag: recv, done: Done::Read(ask(name)) };
            assert!(server.complete(read, &store, &mut effects).is_empty());
            let frame = sent_on(&effects, conn).unwrap();
            assert_eq!(unframe(frame), Answer::of(&store, &key(name)).unwrap());
            assert!(effect_on(&effects, conn, true).is_some(), "it reads on for more asks");
        }
    }

    #[test]
    fn a_server_hangs_up_a_stream_that_is_not_whole_asks() {
        let store = stored(&[("a", Kind::Code, b"code".as_slice())]);
        let (bad, partial) = (Conn(3), Conn(4));
        let (mut server, mut effects, recvs) = serving(&[bad, partial]);

        let mut not_an_ask = ask("a");
        not_an_ask[3] = b'!';
        let read = Completion { tag: recvs[0], done: Done::Read(not_an_ask) };
        server.complete(read, &store, &mut effects);
        assert!(hung_up(&effects, bad));
        assert!(sent_on(&effects, bad).is_none());

        effects.clear();
        let half = ask("a")[..ASK_LEN / 2].to_vec();
        server.complete(Completion { tag: recvs[1], done: Done::Read(half) }, &store, &mut effects);
        assert!(!hung_up(&effects, partial));
        let recv = effect_on(&effects, partial, true).unwrap();
        effects.clear();
        server.complete(Completion { tag: recv, done: Done::Read(Vec::new()) }, &store, &mut effects);
        assert!(hung_up(&effects, partial));
        assert!(sent_on(&effects, partial).is_none());
    }

    #[test]
    fn a_batch_of_short_answers_is_one_send() {
        let names: Vec<String> = (0..100).map(|n| format!("short {n}")).collect();
        let bodies: Vec<Vec<u8>> = (0..100).map(|n| (0..1024).map(|it| (it + n) as u8).collect()).collect();
        let kept: Vec<(&str, Kind, &[u8])> = names
            .iter()
            .zip(&bodies)
            .step_by(2)
            .map(|(name, bytes)| (name.as_str(), Kind::Query, &bytes[..]))
            .collect();
        let store = stored(&kept);
        let conn = Conn(10);
        let (mut server, mut effects, recvs) = serving(&[conn]);
        let asks: Vec<u8> = names.iter().flat_map(|name| ask(name)).collect();
        let read = Completion { tag: recvs[0], done: Done::Read(asks) };
        assert!(server.complete(read, &store, &mut effects).is_empty());

        let sends: Vec<&[u8]> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Send { bytes, .. } => Some(&bytes[..]),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 1, "one send for the batch");
        let expected: Vec<Answer> = names.iter().map(|name| Answer::of(&store, &key(name)).unwrap()).collect();
        assert_eq!(frames(sends[0]), expected);
        assert_eq!(expected.iter().filter(|it| matches!(it, Answer::Missing { .. })).count(), 50);
    }

    #[test]
    fn a_server_queues_a_bounded_number_of_answers_from_one_batch() {
        const LONG: usize = 40 << 20;
        let names = ["w", "x", "y", "z"];
        let bodies: Vec<Vec<u8>> = (0..4).map(|n| vec![n; LONG]).collect();
        let kept: Vec<(&str, Kind, &[u8])> =
            names.iter().zip(&bodies).map(|(name, bytes)| (*name, Kind::Code, &bytes[..])).collect();
        let store = stored(&kept);
        let conn = Conn(13);
        let (mut server, mut effects, recvs) = serving(&[conn]);
        let asks: Vec<u8> = names.iter().flat_map(|name| ask(name)).collect();
        let read = Completion { tag: recvs[0], done: Done::Read(asks) };
        assert!(server.complete(read, &store, &mut effects).is_empty());

        let frame = 8 + VALUE_HEADER + LONG;
        let peer = &server.peers[&conn];
        let queued = peer.out.len() + usize::from(peer.sending.is_some());
        assert!(queued <= 2, "{queued} frames queued from one batch");
        assert_eq!(peer.queued, queued * frame);
        assert!(effect_on(&effects, conn, true).is_none(), "no more asks are read while over the bound");

        let mut sent = Vec::new();
        while sent.len() < names.len() {
            let send = effect_on(&effects, conn, false).expect("a send while answers are owed");
            sent.push(unframe(sent_on(&effects, conn).unwrap()));
            effects.clear();
            let done = Completion { tag: send, done: Done::Sent(Vec::new()) };
            assert!(server.complete(done, &store, &mut effects).is_empty());
            assert!(server.peers[&conn].queued <= QUEUED_MOST + frame);
        }
        let expected: Vec<Answer> = names.iter().map(|name| Answer::of(&store, &key(name)).unwrap()).collect();
        assert_eq!(sent, expected);
        let peer = &server.peers[&conn];
        assert_eq!(peer.queued, 0);
        assert!(peer.out.is_empty() && peer.buffer.is_empty());
        assert!(peer.recv.is_some(), "it reads on once under the bound");
    }

    #[test]
    fn a_server_refuses_a_tcp_address_that_is_not_loopback() {
        for refused in ["192.0.2.1:9000", "0.0.0.0:9000", "[::]:9000", "[2001:db8::1]:9000"] {
            let at = Address::Tcp(refused.parse().unwrap());
            let error = Server::new(at, SERVER).err().unwrap();
            assert!(error.contains("not loopback"), "{error}");
        }
        for allowed in ["127.0.0.1:9000", "[::1]:9000", "[::ffff:127.0.0.1]:9000"] {
            assert!(Server::new(Address::Tcp(allowed.parse().unwrap()), SERVER).is_ok());
        }
        assert!(Server::new(socket(), SERVER).is_ok());
    }

    #[test]
    fn tags_stay_in_their_range() {
        let client = Client::new(socket(), CLIENT);
        assert!(client.owns(Tag(CLIENT)));
        assert!(client.owns(Tag(CLIENT + u64::from(u32::MAX))));
        assert!(!client.owns(Tag(CLIENT - 1)));
        assert!(!client.owns(Tag(SERVER)));
    }

    #[test]
    fn a_miss_the_filter_rules_out_is_answered_without_asking() {
        let store = stored(&[("a", Kind::Code, b"the code of a".as_slice())]);
        let mut net = Net::new(usize::MAX);
        net.fetch("a", 0);
        net.run(&store);
        assert_eq!(net.client.epoch(), Some(store.epoch()));
        assert!(net.client.may_hold(&key("a")));

        let names: Vec<String> = (0..200).map(|n| format!("absent {n}")).collect();
        for (n, name) in names.iter().enumerate() {
            net.fetch(name, 1 + n as u64);
        }
        let asked = net.client.waiting.len();
        net.run(&store);
        assert_eq!(net.answers.len(), 1 + names.len());
        for completion in &net.answers[1..] {
            assert!(matches!(answer(completion), Answer::Missing { .. }), "{completion:?}");
        }
        assert_eq!(net.filtered.len() + asked, names.len());
        assert!(asked <= 8, "{asked} of {} misses asked", names.len());
        assert_eq!(net.links.len(), 1);
    }

    #[test]
    fn a_delta_brings_what_a_batch_put_and_a_full_filter_replaces_the_deltas() {
        let (mut sim, mut store) = opened(Sim::new(8, Faults::default()), &[("a", Kind::Code, b"a".as_slice())]);
        commit(&mut sim, &mut store);
        let mut net = Net::new(usize::MAX);
        net.server.refresh(&mut store, &mut net.effects[S]);
        net.fetch("a", 0);
        net.run(&store);
        assert_eq!(net.client.filters.len(), 1);
        let (mut deltas, mut fulls) = (false, false);
        for n in 0..40u64 {
            let name = format!("later {n}");
            store.put(key(&name), Kind::Run, name.clone().into_bytes()).unwrap();
            commit(&mut sim, &mut store);
            let before = net.client.filters.len();
            net.server.refresh(&mut store, &mut net.effects[S]);
            net.run(&store);
            let after = net.client.filters.len();
            deltas |= after == before + 1;
            fulls |= after == 1;
            assert_eq!(net.client.epoch(), Some(store.epoch()));
            assert!(net.client.may_hold(&key(&name)), "{name} hidden");
            assert!(after <= FILTER_DELTAS + 1);

            net.fetch(&name, 100 + n);
            net.run(&store);
            let last = net.answers.last().unwrap();
            assert_eq!(last.tag, Tag(100 + n));
            assert_eq!(answer(last), Answer::of(&store, &key(&name)).unwrap());
        }
        assert!(deltas && fulls, "both deltas and full filters were sent");
        assert!(net.filtered.is_empty());
        assert_eq!(net.links.len(), 1);
    }

    /// The model: whatever batches the server's store puts, however its deltas and full filters
    /// fall, the client's filters never hide a key the epoch they cover holds.
    #[hegel::test]
    fn a_filter_never_hides_a_key_it_covers(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let first: Vec<u16> = tc.draw(gs::vecs(gs::integers()).max_size(60));
        let batches: Vec<Vec<u16>> = tc.draw(gs::vecs(gs::vecs(gs::integers()).max_size(30)).max_size(24));
        let name = |n: u16| format!("model {n}");
        let put = |store: &mut Store, n: u16| {
            store.put(key(&name(n)), Kind::Code, n.to_le_bytes().to_vec()).unwrap();
        };
        let (mut sim, mut store) = opened(Sim::new(9, Faults::default()), &[]);
        for &n in &first {
            put(&mut store, n);
        }
        commit(&mut sim, &mut store);
        let mut net = Net::new(usize::MAX);
        net.server.refresh(&mut store, &mut net.effects[S]);
        net.fetch("a probe", 0);
        net.run(&store);
        let mut covered: Vec<u16> = first;
        for batch in batches {
            for (at, &n) in covered.iter().enumerate() {
                assert!(net.client.may_hold(&key(&name(n))), "{n}, put {at}th, hidden");
            }
            for &n in &batch {
                put(&mut store, n);
            }
            commit(&mut sim, &mut store);
            net.server.refresh(&mut store, &mut net.effects[S]);
            net.run(&store);
            covered.extend(batch);
            assert!(net.client.epoch() <= Some(store.epoch()));
            assert!(net.client.filters.len() <= FILTER_DELTAS + 1);
        }
        for &n in &covered {
            assert!(net.client.may_hold(&key(&name(n))), "{n} hidden");
        }
    }

    #[test]
    fn a_server_of_a_store_that_does_not_watch_its_keys_stops_and_says_why() {
        let mut sim = Sim::new(7, Faults::default());
        let mut effects = Vec::new();
        let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 0 };
        let mut store = Store::open(config, &mut effects);
        drive(&mut sim, &mut store, &mut effects);
        assert!(!store.watching_keys());
        let mut net = Net::new(usize::MAX);
        net.fetch("a", 1);
        net.run(&store);
        let failed = net.server.failed().unwrap();
        assert!(failed.0.contains("does not watch its keys"), "{failed:?}");
        assert_eq!(net.answers.len(), 1);
        assert!(failure(&net.answers[0]).contains("hung up"), "{:?}", net.answers[0]);
    }

    #[test]
    fn a_client_takes_a_filter_and_refuses_a_malformed_one() {
        let frame = filter_of(true, 3, &[key("a")]);
        let conn = Conn(14);
        let (mut client, mut effects, recv) = connected(&["b"], conn);
        let mut bad = frame.clone();
        bad[17] = 9;
        let answers = completed(&mut client, Completion { tag: recv, done: Done::Read(bad) }, &mut effects);
        assert_eq!(answers.len(), 1);
        assert!(failure(&answers[0]).contains("malformed filter"), "{answers:?}");
        assert!(hung_up(&effects, conn));

        let (mut client, mut effects, recv) = connected(&["b"], Conn(15));
        assert!(completed(&mut client, Completion { tag: recv, done: Done::Read(frame) }, &mut effects).is_empty());
        assert_eq!(client.epoch(), Some(3));
        assert!(client.may_hold(&key("a")));
        assert!(client.waiting.contains_key(&key("b")), "a fetch asked before is still answered");
        let absent = (0..).map(|n| key(&format!("c{n}"))).find(|it| !client.may_hold(it)).unwrap();
        effects.clear();
        assert_eq!(client.fetch(&absent, Tag(7), &mut effects), Asked::Missing);
        assert!(effects.is_empty() && client.out.is_empty(), "nothing is asked");
        assert_eq!(client.counts().skipped, 1);
    }

    /// A put is readable at once and packed only at a later flush: the delta of the refresh
    /// between the two holds it, and its packing sends it no second time.
    #[test]
    fn a_put_not_yet_packed_is_in_the_next_delta() {
        let (mut sim, mut store) = opened(Sim::new(11, Faults::default()), &[]);
        let mut net = Net::new(usize::MAX);
        net.server.refresh(&mut store, &mut net.effects[S]);
        net.fetch("a probe", 0);
        net.run(&store);
        assert_eq!(net.client.filters.len(), 1, "the full filter of the first peer");

        store.put(key("fresh"), Kind::Code, b"fresh".to_vec()).unwrap();
        assert!(store.get(&key("fresh")).unwrap().is_some(), "readable before it is packed");
        assert!(store.unpacked_keys().contains(&key("fresh")));
        net.server.refresh(&mut store, &mut net.effects[S]);
        net.run(&store);
        assert_eq!(net.client.filters.len(), 2, "a delta of the put not yet packed");
        assert!(net.client.may_hold(&key("fresh")));

        commit(&mut sim, &mut store);
        assert!(!store.unpacked_keys().contains(&key("fresh")), "packed by the commit");
        net.server.refresh(&mut store, &mut net.effects[S]);
        net.run(&store);
        assert_eq!(net.client.filters.len(), 2, "its packing sends it no second time");
        net.fetch("fresh", 1);
        net.run(&store);
        let last = net.answers.last().unwrap();
        assert_eq!(last.tag, Tag(1));
        assert_eq!(answer(last), Answer::of(&store, &key("fresh")).unwrap());
        assert!(net.filtered.is_empty());
    }

    /// The model of the invariant in the module's docs: however puts, commits, refreshes and
    /// fetches interleave, a key the server's store could give when it last refreshed is never
    /// ruled out, packed or not, and a fetch of it is answered with its value. A key never put
    /// is answered `Missing`, by the filter or the server.
    #[hegel::test]
    fn no_key_the_server_could_give_at_its_last_refresh_is_ruled_out(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let ops: Vec<u16> = tc.draw(gs::vecs(gs::integers()).max_size(160));
        let name = |n: u16| format!("interleaved {n}");
        let (mut sim, mut store) = opened(Sim::new(12, Faults::default()), &[]);
        let mut net = Net::new(usize::MAX);
        net.server.refresh(&mut store, &mut net.effects[S]);
        net.fetch("a probe", 0);
        net.run(&store);
        let mut put: Vec<u16> = Vec::new();
        // What was put when the server last refreshed.
        let mut covered: Vec<u16> = Vec::new();
        for (at, op) in ops.into_iter().enumerate() {
            let n = (op / 4) % 300;
            match op % 4 {
                0 => {
                    store.put(key(&name(n)), Kind::Code, n.to_le_bytes().to_vec()).unwrap();
                    put.push(n);
                }
                1 => commit(&mut sim, &mut store),
                2 => {
                    net.server.refresh(&mut store, &mut net.effects[S]);
                    net.run(&store);
                    covered.clone_from(&put);
                    for &it in &covered {
                        assert!(net.client.may_hold(&key(&name(it))), "{it}, put before a refresh, hidden");
                    }
                }
                _ => {
                    let tag = Tag(1 + at as u64);
                    net.fetch(&name(n), tag.0);
                    net.run(&store);
                    let last = net.answers.last().unwrap();
                    assert_eq!(last.tag, tag);
                    let got = answer(last);
                    if covered.contains(&n) {
                        assert_eq!(got, Answer::of(&store, &key(&name(n))).unwrap(), "{n} covered");
                        assert!(matches!(got, Answer::Value { .. }), "{n} covered and answered {got:?}");
                    } else if !put.contains(&n) {
                        assert_eq!(got, Answer::Missing { key: key(&name(n)) });
                    }
                }
            }
        }
        let counts = net.client.counts();
        assert_eq!(counts.skipped, net.filtered.len() as u64);
    }

    #[test]
    fn filter_counts_say_what_was_skipped_and_what_got_through() {
        let store = stored(&[("a", Kind::Code, b"the code of a".as_slice())]);
        let mut net = Net::new(usize::MAX);
        net.fetch("a", 0);
        net.run(&store);
        assert_eq!(net.client.counts(), FilterCounts { unfiltered: 1, ..FilterCounts::default() });

        net.fetch("a", 1);
        const ABSENT: u64 = 2000;
        for n in 0..ABSENT {
            net.fetch(&format!("absent {n}"), 2 + n);
        }
        net.run(&store);
        assert_eq!(net.answers.len() as u64, 2 + ABSENT);
        let counts = net.client.counts();
        assert_eq!(counts.unfiltered, 1);
        assert_eq!(counts.found, 1);
        assert_eq!(counts.skipped, net.filtered.len() as u64);
        assert_eq!(counts.skipped + counts.false_positives, ABSENT);
        // About 1 in 256: 8 expected.
        assert!(counts.false_positives <= 40, "{counts:?}");
        assert_eq!(net.links.len(), 1);
    }

    /// A filter frame rules in about one key in 256 it was not built of, at any size, and
    /// every key it was.
    #[hegel::test]
    fn a_filter_frame_rules_in_about_one_absent_key_in_256(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let built: usize = tc.draw(gs::integers().max_value(3000));
        let salt: u64 = tc.draw(gs::integers());
        let keys: Vec<Key> = (0..built).map(|n| key(&format!("in {salt} {n}"))).collect();
        let frame = filter_of(true, 0, &keys);
        let (_, _, filter) = parse_filter(&frame[8..]).unwrap();
        assert!(keys.iter().all(|it| filter.contains(word(it))));
        const PROBES: usize = 4096;
        let wrong = (0..PROBES).filter(|n| filter.contains(word(&key(&format!("out {salt} {n}"))))).count();
        // 16 expected; 64 is past any run of these probes but a broken filter's.
        assert!(wrong <= 64, "{wrong} of {PROBES} absent keys ruled in by a filter of {built}");
    }
}

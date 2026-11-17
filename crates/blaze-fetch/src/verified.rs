//! Objects fetched from sources that are not trusted, every byte checked before it is kept
//! (docs/remote-cache.md, "Packs"). The core of it: a synchronous state machine on an injected
//! clock that owns no socket and no file. It hands back what to ask of which source and the
//! chunks it verified; a driver asks (HTTP against the remote-cache gateway, a file, a peer)
//! and writes what was verified where it is kept.
//!
//! **What an object is.** Bytes named by a root: the bytes cut into [`CHUNK`]-byte chunks, each
//! hashed (BLAKE3, under a domain of its own), and the root the hash of the object's length
//! and those chunk hashes in order ([`root_of`]). Two levels, not BLAKE3's own tree: the list
//! of chunk hashes (32 bytes a chunk: 16 KiB for 32 MiB) is fetched first and checked against
//! the root alone, and from then on every chunk is checked against its own hash, alone, in any
//! order, from any source. A chunk with a path of sibling hashes (Bao) would save fetching
//! the list for a reader of one range; a fetch of whole objects reads every chunk, so the
//! list is no extra byte, and the check needs nothing of BLAKE3's internals.
//!
//! **What it promises** (the invariants its simulation test checks, `verified/tests.rs`):
//! - No byte is given to be kept ([`Effect::Keep`]) that was not checked against the root:
//!   a chunk against its hash, the hashes against the root, lengths against the object's.
//! - A source that answers with bytes of the right length and the wrong hash lied: it is
//!   dropped for the rest of the fetch and what it was asked is asked of another.
//! - A source that fails, answers short, says it lacks a part, or does not answer by its
//!   deadline is asked less: [`STRIKES`] of those running and it is dropped. Lacking an object
//!   is no strike: a peer need not have everything.
//! - Sources come and go while it fetches ([`Fetch::add_source`], [`Fetch::remove_source`]):
//!   there is no index of who has what, each source is asked. A part is asked of the sources
//!   in turn; the fetch ends [`Outcome::Done`] when every chunk of every object was kept, and
//!   [`Outcome::Failed`], naming the object and why, once a part has no source left to ask
//!   (before the first source is given it waits: when to give up on finding one is its
//!   caller's, which knows whether it is offline).
//! - Every wait has a deadline on the injected clock ([`Fetch::next_deadline`]); nothing
//!   waits for a source for good.
//! - Nothing read from a source is trusted for a size: an object longer than [`OBJECT_MOST`]
//!   is refused before anything is asked, and an answer of another length than the part's is
//!   never hashed or kept.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::time::Duration;

use blaze_ids::{ContentId, Hasher};

/// The bytes of one chunk: what one answer carries and one check covers. Large enough that
/// the list of hashes is a thousandth of the object, small enough that a wrong byte costs one
/// chunk's refetch and many sources serve one object at once.
pub const CHUNK: u64 = 64 << 10;
/// The longest object fetched: its list of hashes is then at most 2 MiB, the most a source
/// is ever asked for in one answer besides a chunk.
pub const OBJECT_MOST: u64 = 4 << 30;
/// Failures running (an error, a short answer, a deadline passed) after which a source is
/// dropped.
pub const STRIKES: u32 = 3;
/// Asks in flight at one source at once.
pub const WINDOW: usize = 8;
/// How long a source has to answer one ask.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(20);

const LEAF_DOMAIN: &str = "blaze pack chunk 1";
const ROOT_DOMAIN: &str = "blaze pack object 1";

/// The hash of one chunk of an object.
pub fn chunk_hash(bytes: &[u8]) -> ContentId {
    ContentId::of(LEAF_DOMAIN, bytes)
}

/// The root of an object of `len` bytes whose chunks hash to `hashes`, in order.
pub fn root_of_hashes(len: u64, hashes: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(ROOT_DOMAIN);
    hasher.update(&len.to_le_bytes()).update(hashes);
    hasher.finish()
}

/// The list of an object's chunk hashes, 32 bytes each: what a source serves beside it.
pub fn hashes_of(bytes: &[u8]) -> Vec<u8> {
    // alloc: the list, which is the output; once per object published or checked whole
    let mut out = Vec::with_capacity(chunks_of(bytes.len() as u64) as usize * 32);
    for chunk in bytes.chunks(CHUNK as usize) {
        out.extend_from_slice(&chunk_hash(chunk).0);
    }
    out
}

/// The root of `bytes`: what names them.
pub fn root_of(bytes: &[u8]) -> ContentId {
    root_of_hashes(bytes.len() as u64, &hashes_of(bytes))
}

/// How many chunks an object of `len` bytes has.
pub const fn chunks_of(len: u64) -> u64 {
    len.div_ceil(CHUNK)
}

/// An object to fetch: its root and its length, both from a manifest the caller trusts (the
/// release's, signed) or from a record that names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Want {
    pub root: ContentId,
    pub len: u64,
}

/// A source, as the driver knows it: an index it gave with [`Fetch::add_source`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Source(pub u32);

/// One ask, to tell its answer by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Ask(pub u64);

/// What is asked of a source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    /// The list of the chunk hashes of the object of this root: `len` bytes.
    Hashes { root: ContentId, len: u64 },
    /// `len` bytes of the object of this root, from byte `at`.
    Chunk { root: ContentId, at: u64, len: u64 },
}

/// What the core asks its driver to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Ask `source` for `part`; its answer is told with `ask`.
    Ask { ask: Ask, source: Source, part: Part },
    /// `bytes` are the object's from `at`, checked: keep them.
    Keep { root: ContentId, at: u64, bytes: Vec<u8> },
    /// Every byte of the object was kept.
    Whole { root: ContentId },
    /// Bytes kept so far of the bytes wanted, for whoever shows progress.
    Progress { kept: u64, of: u64 },
    /// A source is asked nothing more, and why.
    Dropped { source: Source, why: Why },
}

/// Why a source was dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// It answered with bytes that are not what the root names.
    Lied,
    /// It failed, answered short or ran out its deadline [`STRIKES`] times running.
    Failing,
    /// The driver took it away ([`Fetch::remove_source`]).
    Gone,
}

/// How an ask ended, as the driver tells it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    Bytes(Vec<u8>),
    /// The source does not have the object.
    Lacks,
    /// The request failed (a connection, a status).
    Failed,
}

/// Where a fetch is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Fetching,
    Done,
    /// An object that could not be had, and why: nothing more is asked.
    Failed { root: ContentId, why: String },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// Not asked, or asked and given back.
    Open,
    Asked,
    Kept,
}

struct Object {
    want: Want,
    /// The chunk hashes, once fetched and checked against the root.
    hashes: Option<Vec<u8>>,
    /// The list of hashes is asked for.
    hashes_asked: bool,
    chunks: Vec<Slot>,
    kept: u64,
    whole: bool,
    /// The sources that said they lack it.
    lacking: Vec<Source>,
}

struct Peer {
    id: Source,
    strikes: u32,
    in_flight: usize,
}

struct Flying {
    ask: Ask,
    source: Source,
    object: usize,
    /// `None` for the list of hashes, else the chunk's index.
    chunk: Option<u64>,
    deadline: Duration,
}

/// A fetch of a set of objects from sources that come and go.
pub struct Fetch {
    now: Duration,
    objects: Vec<Object>,
    peers: Vec<Peer>,
    flying: Vec<Flying>,
    next_ask: u64,
    /// The source the next part is offered to first, so parts go to every source in turn.
    turn: usize,
    /// A source was given at some time: only then is having none left a failure. Before the
    /// first, the fetch waits for one, which is its caller's to give or to give up on.
    sourced: bool,
    outcome: Outcome,
    effects: Vec<Effect>,
    kept: u64,
    of: u64,
    /// A planted bug for the simulation's control: chunks are kept unchecked.
    #[cfg(test)]
    pub(crate) plant_unchecked: bool,
}

impl Fetch {
    /// A fetch of `wants` at `now`. An object past [`OBJECT_MOST`] fails it at once, by name.
    pub fn new(now: Duration, wants: &[Want]) -> Fetch {
        let mut outcome = Outcome::Fetching;
        // alloc: once per fetch, its objects
        let mut objects: Vec<Object> = Vec::with_capacity(wants.len());
        for want in wants {
            if want.len > OBJECT_MOST {
                outcome = Outcome::Failed {
                    root: want.root,
                    // alloc: the error's message
                    why: format!(
                        "it is {} bytes, more than the {OBJECT_MOST} one object may be",
                        want.len
                    ),
                };
                break;
            }
            if objects.iter().any(|it| it.want.root == want.root) {
                continue;
            }
            // No bytes need no source: the root says whether it is the empty object's.
            let empty = want.len == 0;
            if empty && root_of_hashes(0, &[]) != want.root {
                outcome = Outcome::Failed {
                    root: want.root,
                    why: "it is said to have no bytes, and its root is not that of no bytes".into(),
                };
                break;
            }
            objects.push(Object {
                want: *want,
                // alloc: empty, allocates nothing
                hashes: empty.then(Vec::new),
                hashes_asked: false,
                // alloc: once per object, a slot a chunk
                chunks: vec![Slot::Open; chunks_of(want.len) as usize],
                kept: 0,
                whole: false,
                // alloc: empty, allocates nothing
                lacking: Vec::new(),
            });
        }
        let of = objects.iter().map(|it| it.want.len).sum();
        let mut fetch = Fetch {
            now,
            objects,
            // alloc: empty, allocates nothing
            peers: Vec::new(),
            flying: Vec::new(),
            next_ask: 0,
            turn: 0,
            sourced: false,
            outcome,
            effects: Vec::new(),
            kept: 0,
            of,
            #[cfg(test)]
            plant_unchecked: false,
        };
        fetch.step();
        fetch
    }

    /// The clock moved to `now`: asks past their deadline are given up, a strike each.
    pub fn advance_clock(&mut self, now: Duration) {
        self.now = self.now.max(now);
        let mut at = 0;
        while at < self.flying.len() {
            if self.flying[at].deadline <= self.now {
                let flying = self.flying.swap_remove(at);
                self.give_back(&flying);
                self.strike(flying.source);
            } else {
                at += 1;
            }
        }
        self.step();
    }

    /// When the first ask in flight is given up, for the driver's wait.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.flying.iter().map(|it| it.deadline).min()
    }

    /// A source to ask from now on. One added twice is one source.
    pub fn add_source(&mut self, source: Source) {
        self.sourced = true;
        if !self.peers.iter().any(|it| it.id == source) {
            self.peers.push(Peer { id: source, strikes: 0, in_flight: 0 });
        }
        self.step();
    }

    /// A source is gone (a peer left the network): what it was asked is asked of another.
    pub fn remove_source(&mut self, source: Source) {
        self.drop_source(source, Why::Gone);
        self.step();
    }

    /// The answer to `ask`. One the core no longer waits for (its deadline passed, its source
    /// was dropped) is ignored.
    pub fn answered(&mut self, ask: Ask, answer: Answer) {
        let Some(at) = self.flying.iter().position(|it| it.ask == ask) else { return };
        let flying = self.flying.swap_remove(at);
        if let Some(peer) = self.peers.iter_mut().find(|it| it.id == flying.source) {
            peer.in_flight -= 1;
        }
        let object = &mut self.objects[flying.object];
        let (root, len) = (object.want.root, object.want.len);
        let bytes = match answer {
            Answer::Bytes(bytes) => bytes,
            Answer::Lacks => {
                object.lacking.push(flying.source);
                self.reopen(&flying);
                return self.step();
            }
            Answer::Failed => {
                self.reopen(&flying);
                self.strike(flying.source);
                return self.step();
            }
        };
        match flying.chunk {
            None => {
                // The length first: bytes of another length are never hashed.
                if bytes.len() as u64 != chunks_of(len) * 32 {
                    self.reopen(&flying);
                    self.strike(flying.source);
                } else if root_of_hashes(len, &bytes) != root {
                    self.reopen(&flying);
                    self.drop_source(flying.source, Why::Lied);
                } else {
                    self.objects[flying.object].hashes = Some(bytes);
                    self.settle(flying.source);
                }
            }
            Some(index) => {
                let at = index * CHUNK;
                let want = (len - at).min(CHUNK);
                let hashes = object.hashes.as_ref().expect("a chunk is asked once its hash is known");
                let hash = &hashes[index as usize * 32..index as usize * 32 + 32];
                #[cfg(test)]
                let unchecked = self.plant_unchecked;
                #[cfg(not(test))]
                let unchecked = false;
                if bytes.len() as u64 != want {
                    self.reopen(&flying);
                    self.strike(flying.source);
                } else if !unchecked && chunk_hash(&bytes).0 != *hash {
                    self.reopen(&flying);
                    self.drop_source(flying.source, Why::Lied);
                } else {
                    let object = &mut self.objects[flying.object];
                    object.chunks[index as usize] = Slot::Kept;
                    object.kept += want;
                    self.kept += want;
                    self.effects.push(Effect::Keep { root, at, bytes });
                    self.effects.push(Effect::Progress { kept: self.kept, of: self.of });
                    self.settle(flying.source);
                }
            }
        }
        self.step();
    }

    /// What the driver is to do, in order; taken once.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }

    /// A source answered well: its failures running start over.
    fn settle(&mut self, source: Source) {
        if let Some(peer) = self.peers.iter_mut().find(|it| it.id == source) {
            peer.strikes = 0;
        }
    }

    /// The part `flying` asked for is open again.
    fn reopen(&mut self, flying: &Flying) {
        let object = &mut self.objects[flying.object];
        match flying.chunk {
            None => object.hashes_asked = false,
            Some(index) => {
                if object.chunks[index as usize] == Slot::Asked {
                    object.chunks[index as usize] = Slot::Open;
                }
            }
        }
    }

    /// An ask given up at its deadline: its part is open again and its source has one fewer
    /// in flight.
    fn give_back(&mut self, flying: &Flying) {
        if let Some(peer) = self.peers.iter_mut().find(|it| it.id == flying.source) {
            peer.in_flight -= 1;
        }
        self.reopen(flying);
    }

    fn strike(&mut self, source: Source) {
        let Some(peer) = self.peers.iter_mut().find(|it| it.id == source) else { return };
        peer.strikes += 1;
        if peer.strikes >= STRIKES {
            self.drop_source(source, Why::Failing);
        }
    }

    fn drop_source(&mut self, source: Source, why: Why) {
        let Some(at) = self.peers.iter().position(|it| it.id == source) else { return };
        self.peers.remove(at);
        let mut at = 0;
        while at < self.flying.len() {
            if self.flying[at].source == source {
                let flying = self.flying.swap_remove(at);
                self.reopen(&flying);
            } else {
                at += 1;
            }
        }
        self.effects.push(Effect::Dropped { source, why });
    }

    /// Asks what can be asked now, says what became whole, and ends the fetch when it is
    /// done or a part has no source left.
    fn step(&mut self) {
        if self.outcome != Outcome::Fetching {
            return;
        }
        for at in 0..self.objects.len() {
            let object = &mut self.objects[at];
            if !object.whole
                && object.hashes.is_some()
                && object.chunks.iter().all(|it| *it == Slot::Kept)
            {
                object.whole = true;
                self.effects.push(Effect::Whole { root: object.want.root });
            }
        }
        if self.objects.iter().all(|it| it.whole) {
            self.outcome = Outcome::Done;
            return;
        }
        for at in 0..self.objects.len() {
            if self.objects[at].whole {
                continue;
            }
            if self.objects[at].hashes.is_none() {
                if !self.objects[at].hashes_asked
                    && let Some(source) = self.pick(at)
                {
                    self.objects[at].hashes_asked = true;
                    let Want { root, len } = self.objects[at].want;
                    self.ask(source, at, None, Part::Hashes { root, len: chunks_of(len) * 32 });
                }
                continue;
            }
            for index in 0..self.objects[at].chunks.len() {
                if self.objects[at].chunks[index] != Slot::Open {
                    continue;
                }
                let Some(source) = self.pick(at) else { break };
                self.objects[at].chunks[index] = Slot::Asked;
                let Want { root, len } = self.objects[at].want;
                let from = index as u64 * CHUNK;
                let part = Part::Chunk { root, at: from, len: (len - from).min(CHUNK) };
                self.ask(source, at, Some(index as u64), part);
            }
        }
        // An object with a part open, nothing of it in flight and no source that could be
        // asked for it (none left, or every one lacks it) is not to be had.
        for (at, object) in self.objects.iter().enumerate() {
            if object.whole || self.flying.iter().any(|it| it.object == at) {
                continue;
            }
            let askable = self.peers.iter().any(|peer| !object.lacking.contains(&peer.id));
            if !askable && self.sourced {
                // alloc: the error's message
                let why = match self.peers.len() {
                    0 => "no source is left to ask: each was dropped (it lied, kept failing or went away)".to_owned(),
                    n => format!("each of the {n} sources left says it does not have it"),
                };
                self.outcome = Outcome::Failed { root: object.want.root, why };
                return;
            }
        }
    }

    /// The source the next part of object `at` goes to: the next in turn with room in its
    /// window that did not say it lacks the object.
    fn pick(&mut self, at: usize) -> Option<Source> {
        let count = self.peers.len();
        for offset in 0..count {
            let peer = &self.peers[(self.turn + offset) % count];
            if peer.in_flight < WINDOW && !self.objects[at].lacking.contains(&peer.id) {
                self.turn = (self.turn + offset + 1) % count;
                return Some(peer.id);
            }
        }
        None
    }

    fn ask(&mut self, source: Source, object: usize, chunk: Option<u64>, part: Part) {
        let ask = Ask(self.next_ask);
        self.next_ask += 1;
        if let Some(peer) = self.peers.iter_mut().find(|it| it.id == source) {
            peer.in_flight += 1;
        }
        self.flying.push(Flying { ask, source, object, chunk, deadline: self.now + ANSWER_WITHIN });
        self.effects.push(Effect::Ask { ask, source, part });
    }
}

#[cfg(test)]
mod tests;

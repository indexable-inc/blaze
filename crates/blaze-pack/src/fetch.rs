//! Records by key from several sources, none of them believed.
//!
//! [`Fetch`] is given the records wanted, each as a manifest the user trusts lists it (key,
//! kind, length, content hash: [`Record`]), and sources as its driver knows them (a
//! directory, a base URL read by ranges, a peer: [`Source`] is only a number here). It says
//! which source to ask for which record ([`Effect::Ask`]) and hands back the records whose
//! bytes it checked ([`Effect::Keep`]). It opens nothing and reads no clock: the driver asks,
//! answers ([`Fetch::answered`]) and tells the time ([`Fetch::advance_clock`]).
//!
//! **What it promises** (the invariants `src/tests.rs` checks under simulation):
//! - *Nothing unchecked is kept.* Bytes are handed to be kept only when their length is the
//!   record's and they hash to the record's content hash.
//! - *A lie is a hard error.* Bytes of the right length and the wrong hash end the fetch
//!   [`Outcome::Lied`], naming the source and the key. Nothing of that answer is kept, and
//!   nothing more is asked: a source the user configured served bytes that are not what a
//!   trusted manifest names, which the user must hear of, not have papered over by another
//!   source. Records checked before it stay kept: they are what they are named.
//! - *A miss is an answer.* A record no source has ([`Answer::Lacks`] from each) is
//!   [`Outcome::Done`]'s `missed`: its unit is built. Not a fallback (AGENTS.md rule 3): a
//!   cache that lacks a record is the defined case, and building is what a miss means.
//! - *A source that errs is asked nothing more.* One failure ([`Answer::Failed`], a wrong
//!   length, a deadline passed) drops the source for this fetch, said once
//!   ([`Effect::Dropped`]); there is no second try at it. What it was asked is asked of the
//!   sources left, and is a miss when none has it. The error is the driver's to show.
//! - *Every wait ends.* Each ask has a deadline on the injected clock
//!   ([`Fetch::next_deadline`]); a fetch with sources always reaches an outcome.

use alloc::vec::Vec;
use core::time::Duration;

use crate::{Hashes, Id, Record};

/// Asks in flight at one source at once.
pub const WINDOW: usize = 8;
/// How long a source has to answer one ask.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(20);

/// A source, as the driver knows it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Source(pub u32);

/// One ask, to tell its answer by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Ask(pub u64);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Ask `source` for the bytes of record `index` of the wants; answer with `ask`.
    Ask { ask: Ask, source: Source, index: u32 },
    /// `bytes` are record `index`'s, checked: keep them.
    Keep { index: u32, bytes: Vec<u8> },
    /// `source` is asked nothing more, and why: show it once.
    Dropped { source: Source, why: Why },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// It answered with bytes that are not the record's ([`Outcome::Lied`] names the record).
    Lied,
    /// Its request failed.
    Failed,
    /// It answered with bytes of another length than the record's.
    WrongLength { index: u32, want: u64, got: u64 },
    /// It did not answer within [`ANSWER_WITHIN`].
    Late,
    /// The driver took it away.
    Gone,
}

/// How an ask ended, as the driver tells it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    Bytes(Vec<u8>),
    /// The source does not have the record.
    Lacks,
    /// The request failed.
    Failed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    Fetching,
    /// Every record was kept or is missed (no source left has it).
    Done { kept: u32, missed: u32 },
    /// `source` answered for the record of `key` with bytes that hash to `got`, not `want`.
    Lied { source: Source, key: Id, want: Id, got: Id },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Open,
    Asked,
    Kept,
    Missed,
}

struct Want {
    record: Record,
    slot: Slot,
    /// The sources that said they lack it.
    lacking: Vec<Source>,
}

struct Peer {
    id: Source,
    in_flight: usize,
}

struct Flying {
    ask: Ask,
    source: Source,
    index: u32,
    deadline: Duration,
}

pub struct Fetch {
    now: Duration,
    wants: Vec<Want>,
    peers: Vec<Peer>,
    flying: Vec<Flying>,
    next_ask: u64,
    turn: usize,
    /// A source was given at some time: only then is having none left a miss.
    sourced: bool,
    outcome: Outcome,
    effects: Vec<Effect>,
    /// A planted bug for the simulation's control: bytes are kept unchecked.
    #[cfg(test)]
    pub(crate) plant_unchecked: bool,
}

impl Fetch {
    /// A fetch of `wants` at `now`. A record wanted twice is fetched once.
    pub fn new(now: Duration, wants: &[Record]) -> Fetch {
        // alloc: once a fetch, its wants
        let mut kept: Vec<Want> = Vec::with_capacity(wants.len());
        for record in wants {
            if !kept.iter().any(|it| it.record.key == record.key) {
                // alloc: empty, allocates nothing
                kept.push(Want { record: *record, slot: Slot::Open, lacking: Vec::new() });
            }
        }
        let mut fetch = Fetch {
            now,
            wants: kept,
            // alloc: empty, allocates nothing
            peers: Vec::new(),
            flying: Vec::new(),
            next_ask: 0,
            turn: 0,
            sourced: false,
            outcome: Outcome::Fetching,
            effects: Vec::new(),
            #[cfg(test)]
            plant_unchecked: false,
        };
        fetch.step();
        fetch
    }

    /// The record of want `index`, as an [`Effect`] names it.
    pub fn want(&self, index: u32) -> &Record {
        &self.wants[index as usize].record
    }

    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }

    /// What the driver is to do, in order; taken once.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        core::mem::take(&mut self.effects)
    }

    /// When the first ask in flight is given up, for the driver's wait.
    pub fn next_deadline(&self) -> Option<Duration> {
        self.flying.iter().map(|it| it.deadline).min()
    }

    /// A source to ask from now on. One added twice is one source.
    pub fn add_source(&mut self, source: Source) {
        self.sourced = true;
        if !self.peers.iter().any(|it| it.id == source) {
            self.peers.push(Peer { id: source, in_flight: 0 });
        }
        self.step();
    }

    /// A source is gone: what it was asked is asked of another.
    pub fn remove_source(&mut self, source: Source) {
        self.drop_source(source, Why::Gone);
        self.step();
    }

    /// The clock moved to `now`: a source with an ask past its deadline is dropped.
    pub fn advance_clock(&mut self, now: Duration) {
        self.now = self.now.max(now);
        while let Some(late) = self.flying.iter().find(|it| it.deadline <= self.now).map(|it| it.source) {
            self.drop_source(late, Why::Late);
        }
        self.step();
    }

    /// The answer to `ask`. One no longer waited for (its source was dropped, the fetch
    /// ended) is ignored.
    pub fn answered(&mut self, ask: Ask, answer: Answer, hashes: &impl Hashes) {
        if self.outcome != Outcome::Fetching {
            return;
        }
        let Some(at) = self.flying.iter().position(|it| it.ask == ask) else { return };
        let flying = self.flying.swap_remove(at);
        if let Some(peer) = self.peers.iter_mut().find(|it| it.id == flying.source) {
            peer.in_flight -= 1;
        }
        let want = &mut self.wants[flying.index as usize];
        want.slot = Slot::Open;
        let record = want.record;
        match answer {
            Answer::Lacks => want.lacking.push(flying.source),
            Answer::Failed => self.drop_source(flying.source, Why::Failed),
            Answer::Bytes(bytes) => {
                let got = bytes.len() as u64;
                if got != record.len {
                    let why = Why::WrongLength { index: flying.index, want: record.len, got };
                    self.drop_source(flying.source, why);
                    return self.step();
                }
                #[cfg(test)]
                let unchecked = self.plant_unchecked;
                #[cfg(not(test))]
                let unchecked = false;
                let content = match unchecked {
                    true => record.content,
                    false => hashes.content(&bytes),
                };
                if content != record.content {
                    self.drop_source(flying.source, Why::Lied);
                    self.flying.clear();
                    self.outcome = Outcome::Lied {
                        source: flying.source,
                        key: record.key,
                        want: record.content,
                        got: content,
                    };
                    return;
                }
                self.wants[flying.index as usize].slot = Slot::Kept;
                self.effects.push(Effect::Keep { index: flying.index, bytes });
            }
        }
        self.step();
    }

    fn drop_source(&mut self, source: Source, why: Why) {
        // Its asks first, whether or not it is still a source: nothing in flight outlives
        // its source, so a wait on the clock always ends.
        let mut at = 0;
        while at < self.flying.len() {
            if self.flying[at].source == source {
                let flying = self.flying.swap_remove(at);
                self.wants[flying.index as usize].slot = Slot::Open;
            } else {
                at += 1;
            }
        }
        let Some(at) = self.peers.iter().position(|it| it.id == source) else { return };
        self.peers.remove(at);
        self.effects.push(Effect::Dropped { source, why });
    }

    /// Asks what can be asked now, and ends the fetch when every record is kept or missed.
    fn step(&mut self) {
        if self.outcome != Outcome::Fetching {
            return;
        }
        for index in 0..self.wants.len() {
            if self.wants[index].slot != Slot::Open {
                continue;
            }
            match self.pick(index) {
                Some(source) => {
                    self.wants[index].slot = Slot::Asked;
                    let ask = Ask(self.next_ask);
                    self.next_ask += 1;
                    if let Some(peer) = self.peers.iter_mut().find(|it| it.id == source) {
                        peer.in_flight += 1;
                    }
                    let deadline = self.now + ANSWER_WITHIN;
                    self.flying.push(Flying { ask, source, index: index as u32, deadline });
                    self.effects.push(Effect::Ask { ask, source, index: index as u32 });
                }
                None => {
                    // No source could be asked now. With none that could ever be (none left,
                    // or each says it lacks the record) it is a miss; with one whose window
                    // is full it waits for an answer.
                    let want = &self.wants[index];
                    let askable = self.peers.iter().any(|peer| !want.lacking.contains(&peer.id));
                    if !askable && self.sourced {
                        self.wants[index].slot = Slot::Missed;
                    }
                }
            }
        }
        let count = |slot: Slot| self.wants.iter().filter(|it| it.slot == slot).count() as u32;
        let (kept, missed) = (count(Slot::Kept), count(Slot::Missed));
        if (kept + missed) as usize == self.wants.len() && (self.sourced || self.wants.is_empty()) {
            self.outcome = Outcome::Done { kept, missed };
        }
    }

    /// The source the next ask of want `index` goes to: the next in turn with room in its
    /// window that did not say it lacks the record.
    fn pick(&mut self, index: usize) -> Option<Source> {
        let count = self.peers.len();
        for offset in 0..count {
            let peer = &self.peers[(self.turn + offset) % count];
            if peer.in_flight < WINDOW && !self.wants[index].lacking.contains(&peer.id) {
                self.turn = (self.turn + offset + 1) % count;
                return Some(peer.id);
            }
        }
        None
    }
}

//! The machine that answers "expand this call" and "run this script": from a record when one
//! is valid, else by running the work once.
//!
//! ```text
//! Ask(key) --lookup--> Hit(holds, record)               key equal, every listed answer equal
//!              |
//!              +-- each shape filed under the key, its asks answered now (inside `lookup`)
//!              |
//!              +-- Miss --begin--> Run(ticket)          the first asker of the key this request
//!                           |          |
//!                           |          +--settle--> Put --file--> the store --end--> wake
//!                           |          +--settle--> Raced / Refused        --end--> wake
//!                           +--> Wait                every other asker of the key, parked on it
//!                           +--> Done(address)       the key ran already this request
//!                           +--> Failed(refusal)     it ran and failed: no retry
//! ```
//!
//! Records are the core's two-level outside form (`blaze_keyed::outside`): the shapes of a key
//! at its slots, each value at its address, every record immutable. [`lookup`] and [`file`]
//! only carry the core's [`Lookup`] and [`Filer`] machines to the store and to this crate's
//! [`Answers`] (the one thing the core cannot do: decode this crate's asks and answer them).
//! A hit is the core's proof, a [`Holds`] of the request's snapshot; nothing here checks a
//! record that carries one. The hit path reads only the store and the answers, both fixed for
//! the request, and touches nothing shared and mutable. Only a miss reaches [`Flights`], which
//! the driver holds behind its one lock (blaze-core's `Lock`, named `expansion.flights`), for
//! the few instructions of a begin or an end.
//!
//! Bounded progress (rule 18): a key runs at most once per request ([`Begin::Done`] after it
//! ran; a ticket is consumed by its end), a wait that would close a cycle of waits is
//! [`Refusal::Cycle`], a key with more shapes than `outside::MAX_SHAPES` is the core's
//! `OutsideError::TooManyShapes`, each machine is stepped at most a bound of times set by
//! `MAX_SHAPES`, and a record promised by a run's end that the store then lacks is
//! [`Refusal::Lost`], never a silent rerun (rule 3).

use std::{
    alloc::{Allocator, Global},
    fmt,
    hash::Hash,
};

use blaze_hash::{ByHash, HashMap};
use blaze_keyed::{
    kind::{Key, Kind},
    outside::{self, Filer, Lookup, MAX_SHAPES, OutsideError, Settled, Want},
    proof::{self, Checks, Holds, Snapshot},
    record::{RecordError, Traced},
    store::{Store, StoreError},
};

use crate::{
    Digest, Subject,
    flat::Corrupt,
    host::HostCall,
    reads::{Answers, AskBuf, Asks, ReadList, Reads, Unanswerable},
};

/// Why a call or a run ends without a value, naming what it is about and the rule (rule 19).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The run stopped at its fuel limit.
    Fuel { subject: Subject, used: u64 },
    /// The run stopped at its memory limit.
    Memory { subject: Subject, bytes: u64 },
    /// The run made a call the sandbox refuses.
    Refused { subject: Subject, call: HostCall, why: &'static str },
    /// What the run read changed while it ran: no record is written, and the next request
    /// runs it against what is there then (an edit is a new input; nothing is retried here).
    Raced { subject: Subject, ask: AskBuf },
    /// An ask of a filed shape has no answer now.
    Unanswerable { subject: Subject, ask: AskBuf, why: &'static str },
    /// A wait that would close a cycle of waits: the key's run waits, through others, on its
    /// asker.
    Cycle { subject: Subject, key: Digest },
    /// A run's end promised its record at this address, and the store does not have it.
    Lost { subject: Subject, key: Digest, address: Digest },
    /// A payload under a key or an address that does not read.
    Corrupt { subject: Subject, key: Digest, corrupt: Corrupt },
    /// A record whose head is not the one asked for (the core's one key check).
    Record { subject: Subject, key: Digest, error: RecordError },
    /// The store failed.
    Store { subject: Subject, key: Digest, error: StoreError },
    /// The core's outside form refused: a shape or value that does not open, too many shapes,
    /// or two values at one address (the key misses an input, rule 21).
    Outside { subject: Subject, error: OutsideError },
    /// Other bytes under a key-traced record's key (a module): the key misses an input
    /// (rule 21).
    TwoValues { subject: Subject, key: Digest },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Fuel { subject, used } => write!(
                f,
                "{subject} ran out of fuel after {used} steps: an expansion or a run is bounded \
                 (rule bounded-progress)"
            ),
            Refusal::Memory { subject, bytes } => write!(
                f,
                "{subject} reached its memory limit at {bytes} bytes (rule bounded-progress)"
            ),
            Refusal::Refused { subject, call, why } => {
                write!(f, "{subject} {why} (host call {call}; rule sandbox-everything)")
            }
            Refusal::Raced { subject, ask } => write!(
                f,
                "{subject} read {ask}, which changed while it ran: its result is not kept, and \
                 the next build runs it against what is there then (rule keys-cover-every-input)"
            ),
            Refusal::Unanswerable { subject, ask, why } => write!(
                f,
                "{subject}'s record lists {ask}, which has no answer now: {why} (rule no-fallback)"
            ),
            Refusal::Cycle { subject, key } => write!(
                f,
                "{subject}, under the key {key}, waits on itself through the runs it waits on \
                 (rule bounded-progress)"
            ),
            Refusal::Lost { subject, key, address } => write!(
                f,
                "{subject}'s record under the key {key} was written at {address} this build and \
                 is not found there: it is not run again (rule no-fallback)"
            ),
            Refusal::Corrupt { subject, key, corrupt } => {
                write!(f, "{subject}, under the key {key}: {corrupt}")
            }
            Refusal::Record { subject, key, error } => {
                write!(f, "{subject}, under the key {key}: {error}")
            }
            Refusal::Store { subject, key, error } => {
                write!(f, "{subject}, under the key {key}: {error}")
            }
            Refusal::Outside { subject, error } => write!(f, "{subject}: {error}"),
            Refusal::TwoValues { subject, key } => write!(
                f,
                "{subject} gave other bytes under the key {key}: the key misses an input \
                 (rule keys-cover-every-input)"
            ),
        }
    }
}

pub(crate) fn store_error<K>(subject: &Subject, key: Key<K>) -> impl Fn(StoreError) -> Refusal + '_ {
    // The id is taken out here so the closure does not hold `K`.
    let key = key.id();
    move |error| Refusal::Store { subject: subject.clone(), key, error }
}

pub(crate) fn record_error<K>(subject: &Subject, key: Key<K>) -> impl Fn(RecordError) -> Refusal + '_ {
    let key = key.id();
    move |error| Refusal::Record { subject: subject.clone(), key, error }
}

fn outside_error(subject: &Subject) -> impl Fn(OutsideError) -> Refusal + '_ {
    move |error| Refusal::Outside { subject: subject.clone(), error }
}

/// What a lookup found.
pub enum Found<'s, K: Kind, H> {
    /// A value whose key is the asked key and whose every listed answer is the answer now: the
    /// core's proof of it, and the record, held in the asker's frame. [`payload`] opens it.
    Hit { holds: Holds<'s, K>, record: H },
    /// No valid value. The lookup, which remembers the shapes it read, files the run of this
    /// key ([`Lookup::filer`], then [`file`]) without reading a slot again.
    Miss(Lookup<'s, K>),
}

impl<K: Kind, H> fmt::Debug for Found<'_, K, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Found::Hit { holds, .. } => write!(f, "Hit({holds:?})"),
            Found::Miss(_) => f.write_str("Miss"),
        }
    }
}

/// The payload of a value found by [`lookup`] or [`read_back`], by its proof.
pub fn payload<'r, K: Kind<Root = Traced>>(
    subject: &Subject,
    holds: &Holds<'_, K>,
    record: &'r [u8],
) -> Result<&'r [u8], Refusal> {
    outside::payload_of(record, holds).map_err(record_error(subject, holds.key()))
}

/// Looks for a valid value of `key` in the snapshot `snap`: the core's [`Lookup`], its reads
/// from `store` into `frame`, the asks of each shape answered by `answers`.
pub fn lookup<'s, K: Kind<Root = Traced>, S: Store, A: Answers, F: Allocator + Copy>(
    snap: Snapshot<'s>,
    store: &S,
    answers: &mut A,
    subject: &Subject,
    key: Key<K>,
    checks: &mut Checks,
    frame: F,
) -> Result<Found<'s, K, S::Held<F>>, Refusal> {
    let mut machine = Lookup::new(snap, key);
    let mut now = Vec::new_in(frame);
    let mut hit = None;
    // A slot is one shape read (its asks answered at once) and one value read; then Done.
    for _ in 0..2 * MAX_SHAPES + 1 {
        match machine.want() {
            Want::ReadShape(at) => {
                let read = store.read::<K, F>(&at, frame).map_err(store_error(subject, at))?;
                let Some(shape) =
                    machine.shape(read.as_ref().map(|it| &*it.bytes), checks).map_err(outside_error(subject))?
                else {
                    continue;
                };
                now.clear();
                for ask in Asks::of(shape) {
                    let ask = ask
                        .map_err(|corrupt| Refusal::Corrupt { subject: subject.clone(), key: at.id(), corrupt })?;
                    let answer = answers.answer(&ask).map_err(|Unanswerable(why)| Refusal::Unanswerable {
                        subject: subject.clone(),
                        ask: AskBuf::of(&ask),
                        why,
                    })?;
                    now.push(answer);
                }
                machine.answers(&now, checks).map_err(outside_error(subject))?;
            }
            Want::ReadValue(address) => {
                let read = store.read::<K, F>(&address, frame).map_err(store_error(subject, address))?;
                machine.value(read.as_ref().map(|it| &*it.bytes), checks).map_err(outside_error(subject))?;
                hit = read.map(|it| it.bytes);
            }
            Want::Done => {
                return match machine.found() {
                    Some(outside::Found::Hit(holds)) => match hit {
                        Some(record) => Ok(Found::Hit { holds, record }),
                        None => Err(Refusal::Outside {
                            subject: subject.clone(),
                            error: OutsideError::Misused("a hit with no value read"),
                        }),
                    },
                    Some(outside::Found::Miss) => Ok(Found::Miss(machine)),
                    None => Err(Refusal::Outside {
                        subject: subject.clone(),
                        error: OutsideError::Misused("a lookup done with nothing found"),
                    }),
                };
            }
            Want::Answers | Want::PutShape(_) | Want::PutValue(_) => {
                return Err(Refusal::Outside {
                    subject: subject.clone(),
                    error: OutsideError::Misused("a lookup that asked for answers late or for a put"),
                });
            }
        }
    }
    Err(Refusal::Outside {
        subject: subject.clone(),
        error: OutsideError::TooManyShapes { key: key.id() },
    })
}

/// A run settled: where the core files it, the read list it made, and its value's payload.
pub struct Put<K> {
    pub settled: Settled<K>,
    pub list: ReadList,
    pub value: Vec<u8, Global>,
}

impl<K: Kind> fmt::Debug for Put<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Put")
            .field("key", &self.settled.key)
            .field("address", &self.settled.address)
            .field("list", &self.list)
            .finish_non_exhaustive()
    }
}

/// A run's reads and value, made into what is filed, once every read's answer after the run is
/// the answer the run had (the core's [`outside::settle`]): a read that answered two ways
/// within the run, or that answers otherwise now, is [`Refusal::Raced`] and nothing is filed
/// (outside.md section 3: reads were hashed as they were when a run ended, not as read).
pub fn settle<K: Kind, A: Answers>(
    subject: &Subject,
    key: Key<K>,
    reads: Reads,
    value: Vec<u8, Global>,
    answers: &mut A,
    checks: &mut Checks,
) -> Result<Put<K>, Refusal> {
    let list = reads
        .finish()
        .map_err(|moved| Refusal::Raced { subject: subject.clone(), ask: moved.0 })?;
    let mut now = Vec::with_capacity_in(list.answers().len(), Global);
    for (ask, _) in list.iter() {
        now.push(answers.answer(&ask).map_err(|Unanswerable(why)| Refusal::Unanswerable {
            subject: subject.clone(),
            ask: AskBuf::of(&ask),
            why,
        })?);
    }
    let settled = outside::settle(key, list.shape(), list.answers(), &now, checks).map_err(|raced| {
        let ask = list.iter().nth(raced.at as usize).expect("the core names an ask of the list").0;
        Refusal::Raced { subject: subject.clone(), ask: AskBuf::of(&ask) }
    })?;
    Ok(Put { settled, list, value })
}

/// Files a settled run on the store by the core's [`Filer`] (from [`Lookup::filer`] after the
/// lookup that missed, or [`Filer::new`]): its shape at the first slot that holds it or is
/// free, then its value at its address, with its read list as the core's facts.
pub fn file<K: Kind<Root = Traced>, S: Store, F: Allocator + Copy>(
    store: &S,
    subject: &Subject,
    mut filer: Filer<K>,
    put: &Put<K>,
    checks: &mut Checks,
    frame: F,
) -> Result<(), Refusal> {
    let shape = put.list.shape();
    // A slot is one read and one put; then the value's put and Done.
    for _ in 0..2 * MAX_SHAPES + 2 {
        match filer.want() {
            Want::ReadShape(at) => {
                let read = store.read::<K, F>(&at, frame).map_err(store_error(subject, at))?;
                filer.read(read.as_ref().map(|it| &*it.bytes), shape, checks).map_err(outside_error(subject))?;
            }
            Want::PutShape(at) => {
                let filed =
                    store.put::<K>(&at, &outside::write_shape(&at, shape)).map_err(store_error(subject, at))?;
                filer.put(filed).map_err(outside_error(subject))?;
            }
            Want::PutValue(address) => {
                let record = outside::write_value(&address, &put.list.facts(), &put.value);
                let filed = store.put::<K>(&address, &record).map_err(store_error(subject, address))?;
                filer.put(filed).map_err(outside_error(subject))?;
            }
            Want::Done => return Ok(()),
            Want::ReadValue(_) | Want::Answers => {
                return Err(Refusal::Outside {
                    subject: subject.clone(),
                    error: OutsideError::Misused("a filer that asked for a value or answers"),
                });
            }
        }
    }
    Err(Refusal::Outside {
        subject: subject.clone(),
        error: OutsideError::TooManyShapes { key: put.settled.key.id() },
    })
}

/// The right to run a key once and the duty to end it: not `Clone`, consumed by
/// [`Flights::end`].
#[derive(Debug, PartialEq, Eq)]
pub struct Ticket<K: Kind> {
    pub key: Key<K>,
}

/// What [`Flights::begin`] says to an asker that missed.
#[derive(Debug, PartialEq, Eq)]
pub enum Begin<K: Kind> {
    /// Run it, then [`Flights::end`] the ticket.
    Run(Ticket<K>),
    /// Another asker runs it: park on the key; [`Flights::end`] names this asker to wake.
    Wait,
    /// It ran this request, and its record is at this address ([`read_back`]).
    Done(Key<K>),
    /// It ran this request and failed: the same refusal, not a second run.
    Failed(Refusal),
}

enum Flight<K: Kind, W> {
    Running { runner: W, waiters: Vec<W, Global> },
    Done(Key<K>),
    Failed(Refusal),
}

/// The askers to wake when a flight ends, and how it ended.
#[derive(Debug)]
pub struct Woken<K: Kind, W> {
    pub waiters: Vec<W, Global>,
    pub outcome: Result<Key<K>, Refusal>,
}

/// The runs in flight, and those that ended, this request: one per key. `W` names an asker
/// (a task's id). Single-owner: the driver holds it behind one lock, and only a miss reaches
/// it.
pub struct Flights<K: Kind, W> {
    flights: ByHash<Key<K>, Flight<K, W>, Global>,
    /// What each parked asker waits on, for the cycle check.
    waiting: HashMap<W, Key<K>, Global>,
}

impl<K: Kind, W: Copy + Eq + Hash> Default for Flights<K, W> {
    fn default() -> Flights<K, W> {
        Flights { flights: ByHash::new_in(Global), waiting: HashMap::new_in(Global) }
    }
}

impl<K: Kind, W: Copy + Eq + Hash + fmt::Debug> Flights<K, W> {
    /// `asker`, which missed `key`, runs it, waits on it, or is told how it ended.
    pub fn begin(&mut self, subject: &Subject, key: Key<K>, asker: W) -> Begin<K> {
        let Some(flight) = self.flights.get_mut(&key) else {
            // alloc: the waiters of one key's run, for the run's life
            let waiters = Vec::new_in(Global);
            self.flights.insert(key, Flight::Running { runner: asker, waiters });
            return Begin::Run(Ticket { key });
        };
        match flight {
            Flight::Done(address) => Begin::Done(*address),
            Flight::Failed(refusal) => Begin::Failed(refusal.clone()),
            Flight::Running { runner, .. } => {
                let runner = *runner;
                if self.closes_cycle(runner, asker) {
                    return Begin::Failed(Refusal::Cycle { subject: subject.clone(), key: key.id() });
                }
                if let Some(Flight::Running { waiters, .. }) = self.flights.get_mut(&key) {
                    waiters.push(asker);
                }
                self.waiting.insert(asker, key);
                Begin::Wait
            }
        }
    }

    /// Whether `asker` waiting on a run of `runner` closes a cycle: `runner` is `asker`, or
    /// waits (through the runs it waits on) on a run of `asker`. Bounded by the flights in
    /// the table: a chain longer than that has repeated a key.
    fn closes_cycle(&self, runner: W, asker: W) -> bool {
        let mut at = runner;
        for _ in 0..=self.flights.len() {
            if at == asker {
                return true;
            }
            let Some(key) = self.waiting.get(&at) else { return false };
            match self.flights.get(key) {
                Some(Flight::Running { runner, .. }) => at = *runner,
                _ => return false,
            }
        }
        true
    }

    /// Ends a run: `Ok` with the address its driver filed its record at, or its refusal.
    /// Every asker parked on the key is named to wake, and later askers are told the same.
    #[expect(clippy::needless_pass_by_value, reason = "the ticket is consumed: a key's run ends once")]
    pub fn end(&mut self, ticket: Ticket<K>, outcome: Result<Key<K>, Refusal>) -> Woken<K, W> {
        let ended = match &outcome {
            Ok(address) => Flight::Done(*address),
            Err(refusal) => Flight::Failed(refusal.clone()),
        };
        let waiters = match self.flights.insert(ticket.key, ended) {
            Some(Flight::Running { waiters, .. }) => waiters,
            _ => panic!("a ticket of {:?} ended a flight that was not running", ticket.key),
        };
        for waiter in &waiters {
            self.waiting.remove(waiter);
        }
        Woken { waiters, outcome }
    }

    /// How many keys ran or run this request.
    pub fn len(&self) -> usize {
        self.flights.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flights.is_empty()
    }
}

/// The record of a key that ran this request, read back for an asker woken or told
/// [`Begin::Done`]: what the store has at the address, with the core's proof from its one key
/// check, or [`Refusal::Lost`].
pub fn read_back<'s, K: Kind, S: Store, F: Allocator>(
    snap: Snapshot<'s>,
    store: &S,
    subject: &Subject,
    key: Key<K>,
    address: Key<K>,
    frame: F,
) -> Result<(Holds<'s, K>, S::Held<F>), Refusal> {
    let Some(read) = store.read::<K, F>(&address, frame).map_err(store_error(subject, address))? else {
        return Err(Refusal::Lost { subject: subject.clone(), key: key.id(), address: address.id() });
    };
    let holds = proof::hit(snap, address, &read.bytes).map_err(record_error(subject, address))?.holds();
    Ok((holds, read.bytes))
}

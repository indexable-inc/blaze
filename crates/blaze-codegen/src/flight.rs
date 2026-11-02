//! Single flight across builds: each function a build misses in the store is compiled by one
//! build of the process, whichever asks first. A build that reaches it while that compile is
//! under way waits for it; one that reaches it after takes the shape the memo keeps its
//! template under, and prepares nothing. Two proc macros of a job that both reach `syn`'s
//! parser compile it once (docs/v2/plan/lanes/compile-time-sched/notes.md).
//!
//! A claim is held from the store miss until its build lands the compile ([`Claims::land`]) or
//! lets it go ([`Claims::release`], and on drop). A build lets go of every claim it holds
//! before it waits for another's, so no two builds wait on each other's claims. A claim let go
//! of is taken by the next build that asks, which compiles it.
//!
//! The claims a build takes between two releases are one round, held as a run of the
//! scheduler's ([`pump::Flights::hold`]) from its first claim to its release. A build that
//! waits for a function of another round parks its job on that round's run: its thread runs
//! other work meanwhile (the owner's own compiles, on the pool), the scheduler sees who waits
//! for whom, and the wait is a span naming the function and the build that owns it.
//!
//! The table is lock-free ([`SharedByHash`]): a claim is one insert of each function, a landing
//! and a release one compare-and-swap of each, and a wait reads with no lock and writes no
//! shared memory, so builds claiming at once never wait on each other's batches. A store hit
//! touches nothing. Kept in the [`Memo`] whose templates the shapes name: a landing outlives
//! no template.
//!
//! A release build's summaries fly the same way, by their store key: the summary a build made
//! is kept here before it lands, so a build that waited for it or asks after takes it and
//! prepares the function only if it compiles or imports it.
//!
//! [`Memo`]: crate::memo::Memo

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use blaze_graph::{Cancelled, Key, pump};
use blaze_hash::{Operation, SharedByHash, SharedMap};
use blaze_ids::ContentId;

use crate::{InstanceKey, summary::Summary};

/// A function by the key its compiled code is stored under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flight {
    /// Being compiled in the round of this number.
    Flying(u64),
    /// Compiled: its template is in the memo under this shape.
    Landed(ContentId),
}

#[derive(Default)]
pub struct Flights {
    table: SharedByHash<InstanceKey, Flight>,
    /// Round numbers, each once.
    rounds: AtomicU64,
    /// Each round's run in the scheduler, held from its first claim to its release: what a
    /// wait for one of its functions parks on.
    held: pump::Flights<()>,
    /// The build that holds each round under way, by its number, for the spans of the waits.
    owners: SharedMap<u64, String>,
    /// Each summary landed, by its flight key.
    summaries: SharedByHash<InstanceKey, Summary>,
}

/// What a build is told of a function it asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    /// This build compiles it.
    Ours,
    /// Another build is compiling it: wait for it ([`Flights::wait`]).
    Theirs,
    /// Compiled already, its template in the memo under this shape.
    Landed(ContentId),
}

/// The claims of one build: `owner` names it in the spans of the waits for its rounds.
pub struct Claims<'f> {
    flights: &'f Flights,
    owner: &'f str,
    /// The round under way, its number in `round`, while `run` holds it.
    round: u64,
    run: Option<pump::Held<'f, ()>>,
    held: Vec<InstanceKey>,
}

/// What fills a round's key after its number: 24 bytes, the type says.
const ROUND_TAG: &[u8; 24] = b"a codegen memo round....";

/// The scheduler's key of round `number`'s run: no content hash's.
fn round_key(number: u64) -> Key {
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&number.to_le_bytes());
    key[8..].copy_from_slice(ROUND_TAG);
    Key(key)
}

impl Flights {
    pub fn new() -> Flights {
        Flights::default()
    }

    /// The claims of the build `owner` names, none held yet.
    pub fn claims<'f>(&'f self, owner: &'f str) -> Claims<'f> {
        // alloc: none until a claim is held
        Claims { flights: self, owner, round: 0, run: None, held: Vec::new() }
    }

    /// Waits until each of `keys` landed or was let go of: its shape, or `None` for one let go
    /// of, which the waiting build asks for again. `None` overall once `stopped` says the
    /// waiting build stopped, which it looks at before each wait. Each wait parks this job on
    /// the run of the round that claimed the function, until the round's build lets go of it;
    /// a wait that would close a cycle of jobs gives way, unwinding with [`Cancelled`], so the
    /// task is posted again.
    pub fn wait(&self, keys: &[InstanceKey], stopped: impl Fn() -> bool) -> Option<Vec<Option<ContentId>>> {
        loop {
            let (mut first, mut count) = (None, 0usize);
            {
                let table = self.table.pin();
                for key in keys {
                    if let Some(Flight::Flying(round)) = table.get(key) {
                        count += 1;
                        first.get_or_insert((*key, *round));
                    }
                }
            }
            let Some((key, round)) = first else { break };
            if stopped() {
                return None;
            }
            let named = |it: &mut String| {
                use std::fmt::Write as _;
                let owners = self.owners.pin();
                let owner = owners.get(&round).map_or("a build whose round ended", String::as_str);
                _ = write!(it, "codegen: {count} functions {owner} compiles (round {round}), first {key:?}");
            };
            // A round's run is held before its first claim and ends after its release, so one
            // over by now runs nothing here, and its functions are read again.
            if self.held.get_or_run_named(round_key(round), named, || Ok(())).is_err() {
                // alloc: a give-way's payload, once per task that gives way
                std::panic::resume_unwind(Box::new(Cancelled));
            }
        }
        let table = self.table.pin();
        let shape = |key: &InstanceKey| match table.get(key) {
            Some(Flight::Landed(shape)) => Some(*shape),
            _ => None,
        };
        // alloc: per wait, sized by what it waited for
        Some(keys.iter().map(shape).collect())
    }

    /// How many functions have landed.
    pub fn landed(&self) -> usize {
        self.table.pin().values().filter(|it| matches!(it, Flight::Landed(_))).count()
    }

    /// Keeps each of `made`, a summary under the flight key a build claimed it by, for the
    /// builds that take it once the build lands it.
    pub fn keep_summaries(&self, made: Vec<(InstanceKey, Summary)>) {
        if made.is_empty() {
            return;
        }
        let summaries = self.summaries.pin();
        for (key, summary) in made {
            summaries.insert(key, summary);
        }
    }

    /// The summary kept under each of `keys`, landed: `None` for one no build kept.
    pub fn summaries(&self, keys: &[InstanceKey]) -> Vec<Option<Summary>> {
        let table = self.summaries.pin();
        // clone: each taken in place of the store's, which a build decodes into one of its own
        // alloc: per batch of summaries another build made, sized by it
        keys.iter().map(|key| table.get(key).cloned()).collect()
    }

    /// Keeps only the landings whose shape `kept` keeps, and the summaries of those kept: the
    /// memo's collection, between builds.
    pub(crate) fn retain(&mut self, kept: impl Fn(&ContentId) -> bool) {
        let table = self.table.pin();
        table.retain(|_, it| match it {
            Flight::Landed(shape) => kept(shape),
            Flight::Flying(_) => true,
        });
        self.summaries.pin().retain(|key, _| matches!(table.get(key), Some(Flight::Landed(_))));
    }
}

impl Claims<'_> {
    /// Asks for each of `keys`, each once: this build's to compile when no build has it, else
    /// another's under way, else landed.
    pub fn claim(&mut self, keys: &[InstanceKey]) -> Vec<Asked> {
        if self.run.is_none() {
            self.begin();
        }
        let build = self.round;
        let table = self.flights.table.pin();
        // alloc: per batch of store misses, sized by it
        let mut asked = Vec::with_capacity(keys.len());
        for key in keys {
            asked.push(match table.try_insert(*key, Flight::Flying(build)) {
                Ok(_) => {
                    self.held.push(*key);
                    Asked::Ours
                }
                Err(it) => match *it.current {
                    Flight::Flying(round) if round == build => Asked::Ours,
                    Flight::Flying(_) => Asked::Theirs,
                    Flight::Landed(shape) => Asked::Landed(shape),
                },
            });
        }
        asked
    }

    /// Begins a round: its run held in the scheduler before any function is claimed in it, so
    /// a build that finds one of them claimed always finds the run to wait on.
    fn begin(&mut self) {
        self.round = self.flights.rounds.fetch_add(1, Ordering::Relaxed);
        let (round, owner) = (self.round, self.owner);
        let run = self.flights.held.hold_named(round_key(round), |it| {
            use std::fmt::Write as _;
            _ = write!(it, "codegen round {round} of {owner}");
        });
        self.run = Some(run.expect("a round's number is its own"));
        // alloc: once a round, its owner's name for the spans of the waits on it
        self.flights.owners.pin().insert(self.round, self.owner.to_owned());
    }

    /// Lands each of `landed`, a function this build claimed and compiled into the memo under
    /// its shape, for every build that waits for it or asks after.
    pub fn land(&mut self, landed: &[(InstanceKey, ContentId)]) {
        if landed.is_empty() {
            return;
        }
        let (table, flying) = (self.flights.table.pin(), Flight::Flying(self.round));
        for (key, shape) in landed {
            let landing = |it: Option<(&InstanceKey, &Flight)>| match it {
                Some((_, it)) if *it == flying => Operation::Insert(Flight::Landed(*shape)),
                _ => Operation::Abort(()),
            };
            _ = table.compute(*key, landing);
        }
    }

    /// Lets go of every claim this build holds and has not landed (a compile that failed, or
    /// that a stopped build never began), and ends the round: each build waiting on it reads
    /// its functions again, asking again for one let go of.
    pub fn release(&mut self) {
        if !self.held.is_empty() {
            let (table, flying) = (self.flights.table.pin(), Flight::Flying(self.round));
            for key in self.held.drain(..) {
                _ = table.remove_if(&key, |_, it| *it == flying);
            }
        }
        if let Some(run) = self.run.take() {
            self.flights.owners.pin().remove(&self.round);
            run.end(&());
        }
    }
}

impl Drop for Claims<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use parking_lot::Mutex;

    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    fn shape(n: u8) -> ContentId {
        ContentId([n ^ 0x80; 32])
    }

    #[test]
    fn a_round_key_is_its_number_then_the_tag_and_each_round_its_own() {
        let Key(first) = round_key(1);
        assert_eq!(first[..8], 1u64.to_le_bytes());
        assert_eq!(&first[8..], ROUND_TAG);
        let Key(last) = round_key(u64::MAX);
        assert_eq!(last[..8], u64::MAX.to_le_bytes());
        assert_ne!(first, last);
    }

    #[test]
    fn the_first_to_ask_compiles_and_the_rest_are_told() {
        let flights = Flights::new();
        let mut first = flights.claims("a test's build");
        let mut second = flights.claims("a test's build");
        assert_eq!(first.claim(&[key(1), key(2)]), [Asked::Ours, Asked::Ours]);
        assert_eq!(first.claim(&[key(1)]), [Asked::Ours], "a build's own claim is its own");
        assert_eq!(second.claim(&[key(1), key(3)]), [Asked::Theirs, Asked::Ours]);
        first.land(&[(key(1), shape(1))]);
        assert_eq!(second.claim(&[key(1)]), [Asked::Landed(shape(1))]);
        assert_eq!(flights.landed(), 1);
    }

    #[test]
    fn a_claim_let_go_of_is_the_next_askers() {
        let flights = Flights::new();
        let mut first = flights.claims("a test's build");
        assert_eq!(first.claim(&[key(1)]), [Asked::Ours]);
        drop(first);
        let mut second = flights.claims("a test's build");
        assert_eq!(second.claim(&[key(1)]), [Asked::Ours]);
        second.release();
        assert_eq!(flights.wait(&[key(1)], || false), Some(vec![None]));
    }

    #[test]
    fn a_landing_outlives_no_template_the_memo_drops() {
        let mut flights = Flights::new();
        let mut claims = flights.claims("a test's build");
        claims.claim(&[key(1), key(2)]);
        claims.land(&[(key(1), shape(1)), (key(2), shape(2))]);
        drop(claims);
        flights.retain(|it| *it == shape(2));
        let mut again = flights.claims("a test's build");
        assert_eq!(again.claim(&[key(1), key(2)]), [Asked::Ours, Asked::Landed(shape(2))]);
    }

    fn summary(n: u8) -> Summary {
        Summary {
            key: key(n),
            size: u32::from(n),
            hint: crate::summary::Hint::None,
            calls: Vec::new(),
            addressed: Vec::new(),
            params: 0,
            unused: Vec::new(),
            attrs: crate::summary::Attrs::default(),
            body: crate::summary::Attrs::default(),
            external: true,
        }
    }

    /// A summary kept and landed is every later asker's, and goes with its landing.
    #[test]
    fn a_landed_summary_is_taken_and_collected_with_its_landing() {
        let mut flights = Flights::new();
        let mut claims = flights.claims("a test's build");
        assert_eq!(claims.claim(&[key(1), key(2)]), [Asked::Ours, Asked::Ours]);
        flights.keep_summaries(vec![(key(1), summary(1)), (key(2), summary(2))]);
        claims.land(&[(key(1), key(1).0), (key(2), key(2).0)]);
        drop(claims);
        let mut later = flights.claims("a test's build");
        assert_eq!(later.claim(&[key(1)]), [Asked::Landed(key(1).0)]);
        assert_eq!(flights.summaries(&[key(1), key(3)]), [Some(summary(1)), None]);
        drop(later);
        flights.retain(|it| *it == key(2).0);
        assert_eq!(flights.summaries(&[key(1), key(2)]), [None, Some(summary(2))]);
    }

    #[test]
    fn a_waiter_wakes_for_the_landing_and_a_stopped_one_gives_up() {
        let flights = &Flights::new();
        let mut owner = flights.claims("a test's build");
        owner.claim(&[key(1), key(2)]);
        let (asked, told) = crossbeam_channel::unbounded();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(move || {
                let mut theirs = flights.claims("a test's build");
                assert_eq!(theirs.claim(&[key(1), key(2)]), [Asked::Theirs, Asked::Theirs]);
                asked.send(()).unwrap();
                flights.wait(&[key(1), key(2)], || false)
            });
            told.recv().unwrap();
            blaze_graph::spin::busy(Duration::from_millis(20));
            owner.land(&[(key(1), shape(1))]);
            owner.release();
            assert_eq!(waiter.join().unwrap(), Some(vec![Some(shape(1)), None]));
        });
        let mut held = flights.claims("a test's build");
        held.claim(&[key(3)]);
        assert_eq!(flights.wait(&[key(3)], || true), None);
    }

    /// On a pool of one worker, a build waits for a function another build's round claimed
    /// while that build, a job parked before it lands, is woken: the landing can run only on the
    /// worker the wait is on, so the wait ends only if it parks its job rather than its thread
    /// (with the thread held, as by a lock's condition, it never ends). Each build claims,
    /// lands and lets go in its own job, as a build's compile does.
    #[test]
    fn a_build_waiting_for_another_rounds_function_leaves_its_thread_to_that_round() {
        const LONG: Duration = Duration::from_secs(60);
        let flights = std::sync::Arc::new(Flights::new());
        let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
        let go = blaze_graph::sched::Event::new();
        let on = go.waiter();
        let (claimed_tx, claimed) = crossbeam_channel::unbounded::<()>();
        let (owned_tx, owned) = crossbeam_channel::unbounded::<Result<(), String>>();
        {
            let flights = std::sync::Arc::clone(&flights);
            pool.submit(
                None,
                move || {
                    let mut owner = flights.claims("the owner");
                    assert_eq!(owner.claim(&[key(1)]), [Asked::Ours]);
                    claimed_tx.send(()).unwrap();
                    on.wait().expect("an event's wait closes no cycle");
                    owner.land(&[(key(1), shape(1))]);
                    owner.release();
                },
                move |it| _ = owned_tx.send(it.map_err(|_| "the owner's build panicked".to_owned())),
            );
        }
        claimed.recv_timeout(LONG).expect("the owner claims its function");
        let (asking_tx, asking) = crossbeam_channel::unbounded::<()>();
        let (waited_tx, waited) = crossbeam_channel::unbounded();
        {
            let flights = std::sync::Arc::clone(&flights);
            pool.submit(
                None,
                move || {
                    let mut theirs = flights.claims("the waiter");
                    assert_eq!(theirs.claim(&[key(1)]), [Asked::Theirs]);
                    theirs.release();
                    asking_tx.send(()).unwrap();
                    flights.wait(&[key(1)], || false)
                },
                move |it| _ = waited_tx.send(it.map_err(|_| "the waiting build panicked".to_owned())),
            );
        }
        asking.recv_timeout(LONG).expect("the waiter runs once the owner parks");
        go.fire();
        let waited = waited.recv_timeout(LONG).expect("the waiter parks its job, so the owner lands on the one worker");
        assert_eq!(waited, Ok(Some(vec![Some(shape(1))])));
        assert_eq!(owned.recv_timeout(LONG).expect("the owner's build ends"), Ok(()));
    }

    /// Builds that each hold a claim the other waits for: each lets go of or lands its own
    /// before it waits, so both end, and every function is compiled once between them.
    #[test]
    fn builds_waiting_on_each_other_both_end_and_compile_each_once() {
        let flights = Flights::new();
        let compiled = Mutex::new(Vec::new());
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for order in [[1u8, 2], [2, 1]] {
                let (flights, compiled, barrier) = (&flights, &compiled, &barrier);
                scope.spawn(move || {
                    let mut claims = flights.claims("a test's build");
                    let mut missing: Vec<InstanceKey> = order.iter().map(|it| key(*it)).collect();
                    // Each first claims the one the other claims second.
                    claims.claim(&missing[..1]);
                    barrier.wait();
                    while !missing.is_empty() {
                        let asked = claims.claim(&missing);
                        let mut theirs = Vec::new();
                        let mut landed = Vec::new();
                        for (key, asked) in missing.iter().zip(asked) {
                            match asked {
                                Asked::Ours => {
                                    compiled.lock().push(*key);
                                    landed.push((*key, shape(key.0.0[0])));
                                }
                                Asked::Theirs => theirs.push(*key),
                                Asked::Landed(_) => {}
                            }
                        }
                        claims.land(&landed);
                        claims.release();
                        let waited = flights.wait(&theirs, || false).unwrap();
                        missing = theirs.into_iter().zip(waited).filter(|it| it.1.is_none()).map(|it| it.0).collect();
                    }
                });
            }
        });
        let mut compiled = compiled.into_inner();
        compiled.sort();
        assert_eq!(compiled, [key(1), key(2)]);
    }
}

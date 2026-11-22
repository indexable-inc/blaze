//! The code relay's simulation (D194, rule 43): two to four children ask overlapping instances
//! one at a time over the relay, fake workers finish the makes in an order the tape picks, and
//! children take their answers, end early or end at the end, all interleaved by the tape.
//!
//! The oracle, against the serial driver (each child's asks made in turn on its own, the
//! content a function of the instance alone):
//!
//! - every ask of a child that did not end early is answered exactly once, with the bytes the
//!   serial driver makes for it (or refused where it refuses);
//! - nothing is answered to a child after it ended (the relay refuses that by name);
//! - a keyed instance is made at most once however many children ask it, and no worker is
//!   given a second make while one is in flight;
//! - nothing waits when every child has ended.
//!
//! Planted (code_pool_relay.rs `Plant`): a dropped wake, a publish to an ended child, a make
//! run twice for one key; each is caught by the invariant it names.

use std::collections::BTreeMap;
use std::{format, vec::Vec};

use blaze_sim::{Broken, Config, Tape, controls};

use super::*;
use crate::sim::alloc;

const KEYS: usize = 6;
/// Never late in the interleaved runs: the code wait is its own test.
const CODE_WAIT: u64 = 1 << 40;
const DEADLINE: Time = Time(1 << 50);
const MOST_MOVES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Order {
    /// The instance asked (`None`: unkeyed, the child's own).
    key: Option<usize>,
    child: usize,
    at: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct Refusal(usize);

#[derive(Debug)]
enum Invariant {
    /// The relay refused a step of a run the serial driver serves.
    Relay(RelayError),
    /// Child `child`'s ask `at` was never answered.
    Unanswered { child: usize, at: usize },
    /// Child `child`'s ask `at` was answered other than the serial driver answers it.
    Wrong { child: usize, at: usize },
    /// An answer came to child `child` with no ask open.
    Unasked { child: usize },
    /// Keyed instance `key` was made twice.
    TwoMakes { key: usize },
    /// Worker `worker` was given a make while its last was in flight.
    WorkerTwice { worker: usize },
    /// Something still waits after every child ended.
    Survived,
}

impl core::fmt::Display for Invariant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Relay(why) => write!(f, "the relay refused: {why}"),
            Self::Unanswered { child, at } => write!(f, "child {child}'s ask {at} was never answered: a waiter not woken"),
            Self::Wrong { child, at } => write!(f, "child {child}'s ask {at} was answered other than the serial driver answers it"),
            Self::Unasked { child } => write!(f, "an answer came to child {child}, which asked nothing"),
            Self::TwoMakes { key } => write!(f, "instance {key} was made twice"),
            Self::WorkerTwice { worker } => write!(f, "worker {worker} was given a second make while its first was in flight"),
            Self::Survived => f.write_str("the relay still waits after every child ended"),
        }
    }
}

blaze_sim::plants! {
    impl Plant breaks Invariant {
        None,
        DroppedWake => Invariant::Unanswered { .. },
        PublishExited => Invariant::Relay(RelayError::NotAsking { .. }),
        MakeTwice => Invariant::TwoMakes { .. },
    }
}

fn key_of(i: usize) -> CodeKey {
    CodeKey(ContentId::of("blaze code relay sim key", &[i as u8]))
}

/// What a make of `order` gives, on any worker (the serial driver's bytes too).
fn bytes_of(order: Order) -> Vec<u8> {
    match order.key {
        Some(i) => std::vec![i as u8; 1 + i],
        None => std::vec![0xa0, order.child as u8, order.at as u8],
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Made(Vec<u8>),
    /// The refusal of the key at this index: every asker of it sees the same one (rule 19).
    Refused(usize),
    Late,
}

struct Kid {
    script: Vec<Option<usize>>,
    next: usize,
    open: Option<usize>,
    live: bool,
}

#[derive(Clone, Copy)]
enum Move {
    Ask(usize),
    Finish(usize),
    Take(usize),
    Exit(usize),
}

struct Sim {
    relay: Relay<'static, Order, Refusal>,
    refuses: [bool; KEYS],
    kids: Vec<Kid>,
    inflight: Vec<(Worker, CodeKey, Order)>,
    makes: BTreeMap<usize, usize>,
    now: u64,
}

fn relayed<T>(result: Result<T, RelayError>) -> Result<T, Broken<Invariant>> {
    result.map_err(|why| Broken::Oracle(Invariant::Relay(why)))
}

impl Sim {
    fn tick(&mut self) -> Time {
        self.now += 1;
        Time(self.now)
    }

    /// The serial driver's answer of child `child`'s ask `at`.
    fn serial(&self, child: usize, at: usize) -> Seen {
        let key = self.kids[child].script[at];
        match key {
            Some(i) if self.refuses[i] => Seen::Refused(i),
            _ => Seen::Made(bytes_of(Order { key, child, at })),
        }
    }

    fn jobs(&mut self) -> Result<(), Broken<Invariant>> {
        while let Some((worker, key, order)) = self.relay.job() {
            if self.inflight.iter().any(|(w, ..)| *w == worker) {
                return Err(Broken::Oracle(Invariant::WorkerTwice { worker: worker.0 }));
            }
            if let Some(i) = order.key {
                let made = self.makes.entry(i).or_insert(0);
                *made += 1;
                if *made > 1 {
                    return Err(Broken::Oracle(Invariant::TwoMakes { key: i }));
                }
            }
            self.inflight.push((worker, key, order));
        }
        Ok(())
    }

    fn play(&mut self, step: Move) -> Result<(), Broken<Invariant>> {
        let now = self.tick();
        match step {
            Move::Ask(child) => {
                let kid = &mut self.kids[child];
                let at = kid.next;
                kid.next += 1;
                kid.open = Some(at);
                let key = kid.script[at];
                relayed(self.relay.ask(now, Child(child), key.map(key_of), Order { key, child, at }))?;
            }
            Move::Finish(j) => {
                let (worker, _, order) = self.inflight.swap_remove(j);
                match order.key {
                    Some(i) if self.refuses[i] => relayed(self.relay.refused(now, worker, Refusal(i)))?,
                    _ => {
                        let mut made = SVec::new_in(alloc());
                        made.extend_from_slice(&bytes_of(order));
                        relayed(self.relay.made(now, worker, made))?;
                    }
                }
            }
            Move::Take(child) => {
                let seen = relayed(self.relay.answer(Child(child), |reply| match reply {
                    Reply::Made(bytes) => Seen::Made(bytes.to_vec()),
                    Reply::Refused(Refusal(i)) => Seen::Refused(*i),
                    Reply::Late { .. } => Seen::Late,
                }))?
                .ok_or_else(|| Broken::Harness(format!("child {child}'s answer was ready and none came")))?;
                let Some(at) = self.kids[child].open.take() else { return Err(Broken::Oracle(Invariant::Unasked { child })) };
                if seen != self.serial(child, at) {
                    return Err(Broken::Oracle(Invariant::Wrong { child, at }));
                }
            }
            Move::Exit(child) => {
                self.kids[child].live = false;
                relayed(self.relay.exited(now, Child(child)))?;
            }
        }
        self.jobs()
    }

    fn moves(&self, tape: &Tape, into: &mut Vec<Move>) {
        into.clear();
        for (child, kid) in self.kids.iter().enumerate() {
            if !kid.live {
                continue;
            }
            if kid.open.is_none() && kid.next < kid.script.len() {
                into.push(Move::Ask(child));
            }
            if self.relay.ready(Child(child)) {
                into.push(Move::Take(child));
            }
        }
        into.extend((0..self.inflight.len()).map(Move::Finish));
        // Now and then a live child ends early, with its ask open or not.
        if !into.is_empty() && tape.one_in(5, "an early end is offered") {
            into.extend(self.kids.iter().enumerate().filter(|(_, kid)| kid.live).map(|(child, _)| Move::Exit(child)));
        }
    }
}

fn scenario(tape: &Tape, plant: Plant) -> Result<(), Broken<Invariant>> {
    let children = tape.range(2..5, "children") as usize;
    let workers = tape.range(1..5, "workers") as usize;
    let mut refuses = [false; KEYS];
    for refused in refuses.iter_mut() {
        *refused = tape.one_in(4, "an instance's make refuses");
    }
    let mut kids = Vec::new();
    for _ in 0..children {
        let asks = tape.range(1..7, "a child's asks") as usize;
        let script = (0..asks).map(|_| (!tape.one_in(5, "an unkeyed ask")).then(|| tape.range(0..KEYS as u64, "the instance asked") as usize)).collect();
        kids.push(Kid { script, next: 0, open: None, live: true });
    }
    let opts = Options { alloc: alloc(), hash_seed: tape.next_u64("hash seed"), children, workers, asks: 64, deadline: DEADLINE, code_wait: CODE_WAIT, most_bytes: 1 << 20 };
    let mut relay = relayed(Relay::new(Time(0), opts))?;
    relay.plant = plant;
    let mut sim = Sim { relay, refuses, kids, inflight: Vec::new(), makes: BTreeMap::new(), now: 0 };
    let mut moves = Vec::new();
    for _ in 0..MOST_MOVES {
        sim.moves(tape, &mut moves);
        if moves.is_empty() {
            break;
        }
        let step = moves[tape.range(0..moves.len() as u64, "the next move") as usize];
        sim.play(step)?;
    }
    if !moves.is_empty() {
        return Err(Broken::Harness(format!("the run did not settle in {MOST_MOVES} moves")));
    }
    for (child, kid) in sim.kids.iter().enumerate() {
        if kid.live && let Some(at) = kid.open {
            return Err(Broken::Oracle(Invariant::Unanswered { child, at }));
        }
    }
    for child in 0..children {
        if sim.kids[child].live {
            sim.kids[child].live = false;
            let now = sim.tick();
            relayed(sim.relay.exited(now, Child(child)))?;
        }
    }
    match sim.relay.wait() {
        None => Ok(()),
        Some(_) => Err(Broken::Oracle(Invariant::Survived)),
    }
}

/// The refinement and its three controls: every ask answered once with the serial driver's
/// content, nothing to an ended child, one make a key; each plant caught by its invariant.
#[test]
fn children_asking_overlapping_instances_match_the_serial_driver() {
    controls(Config::cases(256), scenario);
}

fn one(child: usize, key: usize, at: usize) -> Order {
    Order { key: Some(key), child, at }
}

fn made(order: Order) -> SVec<'static, u8> {
    let mut bytes = SVec::new_in(alloc());
    bytes.extend_from_slice(&bytes_of(order));
    bytes
}

/// The D194 fault, removed: child 0's make is long (never finished here), child 1 asks
/// another instance, and child 1 is answered while child 0's make still runs. Control: with one
/// worker, child 1's make waits for a worker (the pool names that wait) and no answer comes.
#[test]
fn one_childs_long_make_never_holds_anothers_answer() {
    for workers in [2usize, 1] {
        let opts = Options { alloc: alloc(), hash_seed: 7, children: 2, workers, asks: 8, deadline: DEADLINE, code_wait: CODE_WAIT, most_bytes: 1 << 20 };
        let mut relay: Relay<'static, Order, Refusal> = Relay::new(Time(0), opts).expect("a relay");
        relay.ask(Time(1), Child(0), Some(key_of(1)), one(0, 1, 0)).expect("child 0 asks");
        relay.ask(Time(2), Child(1), Some(key_of(2)), one(1, 2, 0)).expect("child 1 asks");
        let mut jobs = Vec::new();
        while let Some(job) = relay.job() {
            jobs.push(job);
        }
        match workers {
            2 => {
                assert_eq!(jobs.len(), 2, "each ask was not given a worker");
                let (worker, _, order) = jobs.into_iter().find(|(_, _, order)| order.child == 1).expect("child 1's make");
                relay.made(Time(3), worker, made(order)).expect("child 1's make lands");
                let got = relay.answer(Child(1), |reply| matches!(reply, Reply::Made(bytes) if bytes == bytes_of(order).as_slice())).expect("an answer");
                assert_eq!(got, Some(true), "child 1 was not answered while child 0's make runs");
                assert!(relay.asking(Child(0)) && !relay.ready(Child(0)), "child 0 was answered with its make still running");
            }
            _ => {
                assert_eq!(jobs.len(), 1, "one worker was given two makes");
                assert!(!relay.ready(Child(1)), "child 1 was answered with no worker to make it");
                assert!(matches!(relay.wait(), Some((Wait::Pool(PoolWait::Build { .. }) | Wait::Code { .. }, _))), "{:?}", relay.wait());
            }
        }
    }
}

/// The code wait: an ask whose code does not come by its deadline is answered `Late` naming the
/// key and how long it waited, its child is withdrawn (the make's later landing answers no one,
/// and is no error), and the driver's end of it is taken once. Control: one tick before the
/// deadline, no answer.
#[test]
fn a_late_ask_is_answered_late_and_withdrawn() {
    let opts = Options { alloc: alloc(), hash_seed: 3, children: 1, workers: 1, asks: 4, deadline: DEADLINE, code_wait: 10, most_bytes: 1 << 20 };
    let mut relay: Relay<'static, Order, Refusal> = Relay::new(Time(0), opts).expect("a relay");
    relay.ask(Time(1), Child(0), Some(key_of(4)), one(0, 4, 0)).expect("child 0 asks");
    assert_eq!(relay.wait(), Some((Wait::Code { child: Child(0), key: key_of(4) }, Time(11))));
    let (worker, _, order) = relay.job().expect("the make");
    relay.tick(Time(10)).expect("a tick");
    assert!(!relay.ready(Child(0)), "answered before its deadline");
    relay.tick(Time(11)).expect("a tick");
    let late = relay.answer(Child(0), |reply| match reply {
        Reply::Late { key, waited } => Some((key, waited)),
        _ => None,
    });
    assert_eq!(late, Ok(Some(Some((key_of(4), 10)))));
    relay.made(Time(12), worker, made(order)).expect("the late make lands on no one");
    assert!(!relay.ready(Child(0)), "a withdrawn child was answered");
    relay.exited(Time(13), Child(0)).expect("the driver ends it");
    assert_eq!(relay.exited(Time(14), Child(0)), Err(RelayError::ExitedChild(Child(0))));
    assert_eq!(relay.wait(), None);
}

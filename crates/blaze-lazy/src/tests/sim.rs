//! A deterministic simulation of the fetch protocol: run-process threads asking for the
//! functions they call one at a time, a network that drops, duplicates and reorders frames,
//! compiles that finish in any order and sometimes fail, and threads that ask again after a
//! timeout. Every frame goes through the wire's encoding. What must hold:
//!
//! - every function is compiled at most once;
//! - every `Ready` of a function carries the one address it was published at, and no
//!   function that failed is ever answered `Ready`;
//! - the run is stopped exactly once for each failed function some thread called, never for
//!   one nobody called;
//! - every thread ends: it got all its answers, or it called a failed function;
//! - the same seed gives the same trace.
//!
//! And the fetcher's priorities: an ask is taken before what is prefetched.

use std::sync::Arc;

use super::Rng;
use crate::{
    LazyId,
    fetch::{Action, Fetcher, Outcome, Priority, Waiter},
    wire::Frame,
};

struct Thread {
    calls: Vec<u32>,
    next: usize,
    /// The request in flight: its id and when it was last sent.
    asking: Option<(u64, u64)>,
    stopped: bool,
}

enum Message {
    ToDaemon { client: u32, bytes: Vec<u8> },
    ToThread { client: u32, bytes: Vec<u8> },
}

struct World {
    rng: Rng,
    fetcher: Fetcher,
    threads: Vec<Thread>,
    network: Vec<Message>,
    compiling: Vec<LazyId>,
    failing: Vec<bool>,
    started: Vec<u32>,
    stops: Vec<u32>,
    trace: u64,
    now: u64,
}

const TIMEOUT: u64 = 40;

fn target(lazy: u32) -> u64 {
    0x7000_0000 + u64::from(lazy) * 32
}

impl World {
    fn new(seed: u64) -> World {
        let mut rng = Rng::new(seed);
        let functions = 1 + rng.below(24) as u32;
        let failing = (0..functions).map(|_| rng.one_in(6)).collect();
        let threads = (0..1 + rng.below(8))
            .map(|_| Thread {
                calls: (0..rng.below(12)).map(|_| rng.below(u64::from(functions)) as u32).collect(),
                next: 0,
                asking: None,
                stopped: false,
            })
            .collect();
        let workers = 1 + rng.below(4) as u32;
        let mut fetcher = Fetcher::new(functions, workers).unwrap();
        for _ in 0..rng.below(6) {
            let priority = [Priority::Callee, Priority::History, Priority::Idle][rng.below(3) as usize];
            fetcher.want(LazyId(rng.below(u64::from(functions)) as u32), priority);
        }
        World {
            rng,
            fetcher,
            threads,
            network: Vec::new(),
            compiling: Vec::new(),
            failing,
            started: vec![0; functions as usize],
            stops: Vec::new(),
            trace: 0,
            now: 0,
        }
    }

    fn note(&mut self, what: u64) {
        self.trace = (self.trace ^ what).wrapping_mul(0x0100_0000_01b3);
    }

    fn done(&self) -> bool {
        self.threads.iter().all(|it| it.stopped || it.next == it.calls.len())
    }

    /// Threads that wait too long ask again, with the same request id; idle ones ask next.
    fn ask(&mut self) {
        for client in 0..self.threads.len() {
            let thread = &mut self.threads[client];
            if thread.stopped || thread.next == thread.calls.len() {
                continue;
            }
            let lazy = thread.calls[thread.next];
            let request = match thread.asking {
                Some((request, sent)) if self.now - sent > TIMEOUT => request,
                Some(_) => continue,
                None => (client as u64) << 32 | thread.next as u64,
            };
            thread.asking = Some((request, self.now));
            let mut bytes = Vec::new();
            Frame::Ask { request, lazy }.encode(&mut bytes);
            self.network.push(Message::ToDaemon { client: client as u32, bytes });
        }
    }

    fn act(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Answer { to, lazy, outcome } => {
                    let mut bytes = Vec::new();
                    match &outcome {
                        Outcome::Ready(at) => Frame::Ready { request: to.request, lazy, target: *at }.encode(&mut bytes),
                        Outcome::Failed(why) => {
                            Frame::Failed { request: to.request, lazy, why: &**why }.encode(&mut bytes)
                        }
                    }
                    self.network.push(Message::ToThread { client: to.client, bytes });
                }
                Action::Stop { lazy, .. } => {
                    assert!(self.failing[lazy as usize], "the run was stopped for {lazy}, which compiled");
                    self.stops.push(lazy);
                }
            }
        }
    }

    fn deliver(&mut self, message: Message) {
        match message {
            Message::ToDaemon { client, bytes } => {
                let Frame::Ask { request, lazy } = Frame::decode(&bytes).unwrap() else { panic!("not an ask") };
                self.note(request ^ u64::from(lazy) << 7);
                let mut actions = Vec::new();
                let seq = request as u32;
                self.fetcher.ask(Waiter { client, seq, request }, lazy, &mut actions);
                self.act(actions);
            }
            Message::ToThread { client, bytes } => {
                let frame = Frame::decode(&bytes).unwrap();
                let thread = &mut self.threads[client as usize];
                let (request, lazy) = match frame {
                    Frame::Ready { request, lazy, target: at } => {
                        assert_eq!(at, target(lazy), "function {lazy} answered at two addresses");
                        assert!(!self.failing[lazy as usize], "function {lazy} failed and was answered ready");
                        (request, lazy)
                    }
                    Frame::Failed { request, lazy, why } => {
                        assert!(self.failing[lazy as usize] && why.contains("no MIR"), "{why}");
                        (request, lazy)
                    }
                    Frame::Ask { .. } => panic!("an ask sent to a thread"),
                };
                let current = thread.asking.map(|it| it.0);
                if thread.stopped || current != Some(request) {
                    return;
                }
                assert_eq!(thread.calls[thread.next], lazy);
                thread.asking = None;
                if matches!(frame, Frame::Failed { .. }) {
                    thread.stopped = true;
                } else {
                    thread.next += 1;
                }
                self.note(request.rotate_left(17));
            }
        }
    }

    fn step(&mut self) {
        self.now += 1;
        self.ask();
        while let Some((id, _)) = self.fetcher.next() {
            self.started[id.index()] += 1;
            assert_eq!(self.started[id.index()], 1, "function {} compiled twice", id.0);
            self.compiling.push(id);
            self.note(u64::from(id.0) << 40);
        }
        match self.rng.below(3) {
            0 if !self.compiling.is_empty() => {
                let at = self.rng.below(self.compiling.len() as u64) as usize;
                let id = self.compiling.swap_remove(at);
                let result = if self.failing[id.index()] { Err("no MIR") } else { Ok(target(id.0)) };
                let mut actions = Vec::new();
                self.fetcher.done(id, result, &mut actions);
                self.act(actions);
            }
            _ if !self.network.is_empty() => {
                let at = self.rng.below(self.network.len() as u64) as usize;
                let message = self.network.swap_remove(at);
                if self.rng.one_in(8) {
                    self.note(0xd0d0);
                    return;
                }
                if self.rng.one_in(12) {
                    let copy = match &message {
                        Message::ToDaemon { client, bytes } => Message::ToDaemon { client: *client, bytes: bytes.clone() },
                        Message::ToThread { client, bytes } => Message::ToThread { client: *client, bytes: bytes.clone() },
                    };
                    self.network.push(copy);
                }
                self.deliver(message);
            }
            _ => {}
        }
    }

    /// Runs to the end; the trace.
    fn run(mut self) -> u64 {
        let mut steps = 0;
        while !self.done() {
            self.step();
            steps += 1;
            assert!(steps < 1_000_000, "the run did not end");
        }
        for (lazy, failing) in self.failing.iter().enumerate() {
            let called = self.threads.iter().any(|it| it.calls[..it.next.min(it.calls.len())].contains(&(lazy as u32)))
                || self.threads.iter().any(|it| it.stopped && it.calls[it.next] == lazy as u32);
            let stops = self.stops.iter().filter(|it| **it == lazy as u32).count();
            if *failing && self.threads.iter().any(|it| it.stopped && it.calls[it.next] == lazy as u32) {
                assert_eq!(stops, 1, "function {lazy} failed, was called, and stopped the run {stops} times");
            }
            if !called {
                assert_eq!(stops, 0, "function {lazy} was never called and stopped the run");
            }
        }
        for thread in &self.threads {
            if thread.stopped {
                assert!(self.failing[thread.calls[thread.next] as usize]);
                assert!(thread.calls[..thread.next].iter().all(|it| !self.failing[*it as usize]));
            } else {
                assert!(thread.calls.iter().all(|it| !self.failing[*it as usize]));
            }
        }
        assert!(self.fetcher.compiles() <= u64::from(self.fetcher.len()));
        self.trace
    }
}

#[test]
fn the_protocol_holds_under_dropped_duplicated_and_reordered_frames() {
    for seed in 0..400 {
        World::new(seed).run();
    }
}

#[test]
fn the_same_seed_gives_the_same_trace() {
    for seed in [3, 77, 1234] {
        assert_eq!(World::new(seed).run(), World::new(seed).run());
    }
}

/// The fetcher's waiters, in its slab, against a list per function in a map (how they were
/// kept before): random asks, resent asks and compiles ending well or not give the same
/// actions in the same order after every step.
#[test]
fn the_waiters_in_a_slab_answer_as_a_list_per_function_did() {
    #[derive(Clone, Copy, PartialEq)]
    enum Done {
        No,
        Ready(u64),
        Failed,
    }
    for seed in 0..200u64 {
        let mut rng = Rng::new(seed);
        let functions = 1 + rng.below(12) as u32;
        let mut fetcher = Fetcher::new(functions, 1 + rng.below(3) as u32).unwrap();
        let mut waiters: Vec<Vec<Waiter>> = vec![Vec::new(); functions as usize];
        let mut done = vec![Done::No; functions as usize];
        let mut stopped = vec![false; functions as usize];
        let mut compiling: Vec<LazyId> = Vec::new();
        let (mut out, mut expected) = (Vec::new(), Vec::new());
        for step in 0..300 {
            out.clear();
            expected.clear();
            match rng.below(3) {
                0 => {
                    let to = Waiter { client: rng.below(3) as u32, seq: rng.below(3) as u32, request: rng.below(2) };
                    let lazy = rng.below(u64::from(functions)) as u32;
                    fetcher.ask(to, lazy, &mut out);
                    let at = lazy as usize;
                    match done[at] {
                        Done::Ready(target) => expected.push(Action::Answer { to, lazy, outcome: Outcome::Ready(target) }),
                        Done::Failed => {
                            let why: Arc<str> = "no MIR".into();
                            expected.push(Action::Answer { to, lazy, outcome: Outcome::Failed(Arc::clone(&why)) });
                            if !std::mem::replace(&mut stopped[at], true) {
                                expected.push(Action::Stop { lazy, why });
                            }
                        }
                        Done::No if !waiters[at].contains(&to) => waiters[at].push(to),
                        Done::No => {}
                    }
                }
                1 => compiling.extend(fetcher.next().map(|it| it.0)),
                _ if compiling.is_empty() => {}
                _ => {
                    let id = compiling.swap_remove(rng.below(compiling.len() as u64) as usize);
                    let at = id.index();
                    let lazy = id.0;
                    if rng.one_in(4) {
                        fetcher.done(id, Err("no MIR"), &mut out);
                        done[at] = Done::Failed;
                        let why: Arc<str> = "no MIR".into();
                        let asked = std::mem::take(&mut waiters[at]);
                        for to in &asked {
                            expected.push(Action::Answer { to: *to, lazy, outcome: Outcome::Failed(Arc::clone(&why)) });
                        }
                        if !asked.is_empty() {
                            stopped[at] = true;
                            expected.push(Action::Stop { lazy, why });
                        }
                    } else {
                        let target = 0x1000 + u64::from(lazy) * 16;
                        fetcher.done(id, Ok(target), &mut out);
                        done[at] = Done::Ready(target);
                        for to in std::mem::take(&mut waiters[at]) {
                            expected.push(Action::Answer { to, lazy, outcome: Outcome::Ready(target) });
                        }
                    }
                }
            }
            assert_eq!(out, expected, "seed {seed}, step {step}");
        }
    }
}

#[test]
fn an_ask_is_taken_before_prefetches_and_moves_a_queued_function_up() {
    let mut fetcher = Fetcher::new(6, 2).unwrap();
    fetcher.want(LazyId(0), Priority::Idle);
    fetcher.want(LazyId(1), Priority::History);
    fetcher.want(LazyId(2), Priority::Callee);
    let mut actions = Vec::new();
    fetcher.ask(Waiter { client: 0, seq: 1, request: 1 }, 0, &mut actions);
    assert!(actions.is_empty());
    assert_eq!(fetcher.next(), Some((LazyId(0), Priority::Asked)), "the ask moved 0 up from idle");
    assert_eq!(fetcher.next(), Some((LazyId(2), Priority::Callee)));
    assert_eq!(fetcher.next(), None, "both workers busy");
    fetcher.done(LazyId(2), Ok(0x40), &mut actions);
    assert!(actions.is_empty(), "nobody waited for a prefetch");
    fetcher.done(LazyId(0), Ok(0x80), &mut actions);
    assert_eq!(actions, vec![Action::Answer {
        to: Waiter { client: 0, seq: 1, request: 1 },
        lazy: 0,
        outcome: Outcome::Ready(0x80)
    }]);
    assert_eq!(fetcher.next(), Some((LazyId(1), Priority::History)));
    assert_eq!(fetcher.next(), None, "one worker stays free for asks");
    fetcher.ask(Waiter { client: 1, seq: 9, request: 9 }, 3, &mut actions);
    assert_eq!(fetcher.next(), Some((LazyId(3), Priority::Asked)));
    assert_eq!(fetcher.compiles(), 4);
}

#[test]
fn an_ask_of_a_published_function_is_answered_at_once() {
    let mut fetcher = Fetcher::new(2, 1).unwrap();
    fetcher.want(LazyId(1), Priority::Callee);
    assert_eq!(fetcher.next(), Some((LazyId(1), Priority::Callee)));
    let mut actions = Vec::new();
    fetcher.done(LazyId(1), Ok(0x1230), &mut actions);
    let to = Waiter { client: 4, seq: 2, request: 2 };
    fetcher.ask(to, 1, &mut actions);
    assert_eq!(actions, vec![Action::Answer { to, lazy: 1, outcome: Outcome::Ready(0x1230) }]);
    assert_eq!(fetcher.target(LazyId(1)), Some(0x1230));
    assert_eq!(fetcher.next(), None);
}

#[test]
fn only_published_code_is_retargeted() {
    let mut fetcher = Fetcher::new(4, 1).unwrap();
    let mut actions = Vec::new();
    assert!(!fetcher.retarget(LazyId(0), 0x40), "lazy");
    assert!(!fetcher.retarget(LazyId(9), 0x40), "past the session");
    fetcher.want(LazyId(0), Priority::Asked);
    fetcher.want(LazyId(1), Priority::Asked);
    assert!(!fetcher.retarget(LazyId(0), 0x40), "queued");
    assert_eq!(fetcher.next(), Some((LazyId(0), Priority::Asked)));
    assert!(!fetcher.retarget(LazyId(0), 0x40), "compiling");
    fetcher.done(LazyId(0), Ok(0x10), &mut actions);
    assert_eq!(fetcher.next(), Some((LazyId(1), Priority::Asked)));
    fetcher.done(LazyId(1), Err("no MIR"), &mut actions);
    assert!(!fetcher.retarget(LazyId(1), 0x40), "failed");
    assert!(fetcher.retarget(LazyId(0), 0x40));
    assert_eq!(fetcher.target(LazyId(0)), Some(0x40));
    actions.clear();
    let to = Waiter { client: 1, seq: 1, request: 1 };
    fetcher.ask(to, 0, &mut actions);
    assert_eq!(actions, vec![Action::Answer { to, lazy: 0, outcome: Outcome::Ready(0x40) }]);
    assert_eq!(fetcher.compiles(), 2);
}
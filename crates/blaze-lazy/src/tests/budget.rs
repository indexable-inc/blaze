//! Allocation budgets of the hot paths: an ask of a published function, the frames, and the
//! plan's lookups allocate nothing; nor does a first ask once the runner is warm (its waiters
//! go in the fetcher's slab), a warm round of asks and answers, or a lone hit, answered by the
//! thread that read it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use blaze_alloc_count::{assert_at_most, measure};
use blaze_codegen::{Arch, CompiledFunction, InstanceKey};
use blaze_inproc::frame::Kind;
use blaze_items::{CodeHash, ItemKey};
use blaze_stencil::tier::Tier;
use parking_lot::{Condvar, Mutex};

use super::flight::{Addresses, Counting, session};
use super::{compiled, function, id, key};
use crate::{
    LazyId,
    fetch::{Action, Fetcher, Outcome, Waiter},
    plan::{Function, Plan},
    runner::{Answers, Compiler, Config, Runner},
    wire::{self, ASK_KIND, Frame, READY_KIND, READY_LEN},
};

/// The last answer, kept in place: sending allocates nothing.
struct Last {
    answer: Mutex<(u32, Kind, [u8; READY_LEN], usize)>,
    arrived: Condvar,
}

#[derive(Clone)]
struct Kept(Arc<Last>);

impl Answers for Kept {
    fn send(&self, _: u32, seq: u32, kind: Kind, body: &[u8]) {
        let mut answer = self.0.answer.lock();
        let len = body.len().min(READY_LEN);
        answer.2[..len].copy_from_slice(&body[..len]);
        answer.0 = seq;
        answer.1 = kind;
        answer.3 = body.len();
        self.0.arrived.notify_all();
    }

    fn stop(&self, _: Option<(InstanceKey, ItemKey)>, why: &str) {
        panic!("a run with nothing failing was stopped: {why}");
    }
}

impl Kept {
    fn new() -> Kept {
        Kept(Arc::new(Last { answer: Mutex::new((0, Kind(0), [0; READY_LEN], 0)), arrived: Condvar::new() }))
    }

    /// The answer to frame `seq`, once it comes: its kind and bytes.
    fn wait(&self, seq: u32) -> (Kind, [u8; READY_LEN], usize) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut answer = self.0.answer.lock();
        while answer.0 != seq {
            let left = deadline.checked_duration_since(Instant::now()).expect("an answer within a minute");
            self.0.arrived.wait_for(&mut answer, left);
        }
        (answer.1, answer.2, answer.3)
    }
}

fn target_of(n: u32) -> u64 {
    0x20_0000 + u64::from(n) * 0x40
}

fn runner(count: u32) -> (Runner<Counting, Addresses, Kept>, Kept) {
    let functions: Vec<Function> = (0..count).map(function).collect();
    let kept = Kept::new();
    let runner = Runner::start(
        session(&functions),
        &[],
        Counting::default(),
        Addresses((0..count).map(|n| (key(n), target_of(n))).collect()),
        kept.clone(),
        Config { workers: 2, idle: false },
        None,
    )
    .unwrap();
    (runner, kept)
}

#[test]
fn an_ask_of_a_published_function_allocates_nothing() {
    let (runner, kept) = runner(2);
    runner.frame(0, ASK_KIND, 1, &wire::ask(1, 0)).unwrap();
    assert_eq!(kept.wait(1).0, READY_KIND);
    let asked = wire::ask(2, 0);
    assert_at_most(0, "an ask of a published function", || runner.frame(0, ASK_KIND, 2, &asked).unwrap());
    let (kind, bytes, len) = kept.wait(2);
    assert_eq!((kind, len), (READY_KIND, READY_LEN));
    assert_eq!(Frame::decode(&bytes).unwrap(), Frame::Ready { request: 2, lazy: 0, target: target_of(0) });
    runner.close();
}

#[test]
fn a_first_ask_of_a_warm_runner_allocates_nothing() {
    let (runner, kept) = runner(2);
    runner.frame(0, ASK_KIND, 1, &wire::ask(1, 0)).unwrap();
    kept.wait(1);
    let asked = wire::ask(2, 1);
    assert_at_most(0, "a first ask, the runner warm", || runner.frame(0, ASK_KIND, 2, &asked).unwrap());
    let (kind, bytes, _) = kept.wait(2);
    assert_eq!(kind, READY_KIND);
    assert_eq!(Frame::decode(&bytes).unwrap(), Frame::Ready { request: 2, lazy: 1, target: target_of(1) });
    runner.close();
}

/// Has every function compiled already, as code that allocates nothing to hand over.
struct Empty;

impl Compiler for Empty {
    fn compile(&self, key: InstanceKey) -> Result<CompiledFunction, String> {
        Err(format!("{key:?}: every function is a hit"))
    }

    fn hit(&self, key: InstanceKey) -> Option<Result<CompiledFunction, String>> {
        Some(Ok(CompiledFunction {
            key,
            arch: Arch::X86_64,
            align: 16,
            code: Vec::new(),
            relocs: Default::default(),
            data: Vec::new(),
            inlined: Vec::new(),
            side: None,
            unwind: None,
            lines: Vec::new(),
            coverage: None,
            profile: None,
        }))
    }

    fn tier_up(&self, key: InstanceKey, _: Tier) -> Result<CompiledFunction, String> {
        Err(format!("{key:?}: not tiered"))
    }
}

#[test]
fn a_lone_hit_of_a_warm_runner_allocates_nothing() {
    let kept = Kept::new();
    let runner = Runner::start(
        session(&(0..3).map(function).collect::<Vec<_>>()),
        &[],
        Empty,
        Addresses((0..3).map(|n| (key(n), target_of(n))).collect()),
        kept.clone(),
        Config { workers: 1, idle: false },
        None,
    )
    .unwrap();
    runner.frame(0, ASK_KIND, 1, &wire::ask(1, 0)).unwrap();
    assert_eq!(kept.wait(1).0, READY_KIND);
    let asked = wire::ask(2, 1);
    super::hit::pause();
    assert_at_most(0, "a lone hit, answered by the reading thread", || runner.frame(0, ASK_KIND, 2, &asked).unwrap());
    let (kind, bytes, _) = kept.wait(2);
    assert_eq!(kind, READY_KIND);
    assert_eq!(Frame::decode(&bytes).unwrap(), Frame::Ready { request: 2, lazy: 1, target: target_of(1) });
    assert_eq!(runner.close().compiles, 2);
}

#[test]
fn the_fetchers_answer_of_a_published_function_allocates_nothing() {
    let mut fetcher = Fetcher::new(4, 2).unwrap();
    let mut out = Vec::with_capacity(8);
    let to = Waiter { client: 1, seq: 1, request: 1 };
    fetcher.ask(to, 2, &mut out);
    let (id, _) = fetcher.next().unwrap();
    out.clear();
    fetcher.done(id, Ok(0x1000), &mut out);
    out.clear();
    assert_at_most(0, "an ask of a published function", || {
        for seq in 2..100 {
            fetcher.ask(Waiter { seq, ..to }, 2, &mut out);
            out.clear();
        }
        fetcher.target(LazyId(2))
    });
    fetcher.ask(Waiter { seq: 100, ..to }, 2, &mut out);
    assert_eq!(out, [Action::Answer { to: Waiter { seq: 100, ..to }, lazy: 2, outcome: Outcome::Ready(0x1000) }]);
}

/// Asks of functions not compiled yet, four clients each, then their answers: once the slab,
/// the map and the queues have held a round, the next rounds allocate nothing.
#[test]
fn a_warm_round_of_asks_and_answers_allocates_nothing() {
    let mut fetcher = Fetcher::new(64, 4).unwrap();
    let mut out: Vec<Action> = Vec::with_capacity(64);
    let round = |fetcher: &mut Fetcher, out: &mut Vec<Action>, base: u32| {
        for lazy in base..base + 8 {
            for client in 0..4 {
                fetcher.ask(Waiter { client, seq: lazy, request: u64::from(client) }, lazy, out);
            }
        }
        while let Some((id, _)) = fetcher.next() {
            fetcher.done(id, Ok(0x1000 + u64::from(id.0)), out);
        }
        let answered = out.iter().filter(|it| matches!(it, Action::Answer { outcome: Outcome::Ready(_), .. })).count();
        out.clear();
        answered
    };
    assert_eq!(round(&mut fetcher, &mut out, 0), 32);
    let answered = assert_at_most(0, "a warm round of asks and answers", || {
        round(&mut fetcher, &mut out, 8) + round(&mut fetcher, &mut out, 16)
    });
    assert_eq!(answered, 64);
}

#[test]
fn frames_encode_and_decode_in_place() {
    let ask = wire::ask(7, 3);
    let ready = wire::ready(7, 3, 0x4000);
    let mut failed = Vec::with_capacity(64);
    Frame::Failed { request: 7, lazy: 3, why: "no" }.encode(&mut failed);
    assert_at_most(0, "the frames", || {
        assert_eq!(Frame::decode(&ask).unwrap(), Frame::Ask { request: 7, lazy: 3 });
        assert_eq!(Frame::decode(&ready).unwrap(), Frame::Ready { request: 7, lazy: 3, target: 0x4000 });
        assert_eq!(Frame::decode(&failed).unwrap(), Frame::Failed { request: 7, lazy: 3, why: "no" });
        (wire::ask(8, 4), wire::ready(8, 4, 1))
    });
}

#[test]
fn the_plans_lookups_allocate_nothing() {
    let functions: Vec<Function> = (0..64).map(function).collect();
    let plan = Plan::new(&functions, &[], &[]).unwrap();
    let reached: Vec<CodeHash> = (0..64).step_by(3).map(|n| CodeHash(id("blaze-lazy test code", n))).collect();
    let keys: Vec<InstanceKey> = (0..64).map(key).collect();
    let calls = compiled(0, &[1, 2, 3], &[4]);
    let (mut ids, mut callees, mut seen) = (Vec::with_capacity(64), Vec::with_capacity(8), plan.scratch());
    let ((), used) = measure(|| {
        for (n, key) in keys.iter().enumerate() {
            assert_eq!(plan.id(key), Some(LazyId(n as u32)));
        }
        plan.reached(&reached, &mut ids);
        plan.callees(&calls, &mut seen, &mut callees);
    });
    assert_eq!(used.allocations, 0, "the plan's lookups: {used:?}");
    assert_eq!(ids.len(), reached.len());
    assert_eq!(callees, [LazyId(1), LazyId(2), LazyId(3)]);
}

//! The hand-off from an ask to a worker: a worker with nothing to do spins a while, then
//! sleeps, and a queued compile wakes one sleeper. Asks that come while a worker spins, after
//! every worker went to sleep, and in a burst are each compiled once and answered; a close
//! ends spinning and sleeping workers alike at once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::flight::{Addresses, Counting, Mail, Mailbox, session};
use super::{compiled, function, key};
use crate::{
    fetch::Priority,
    plan::Function,
    runner::{Compiler, Config, Runner},
    wire::{self, ASK_KIND, Frame, READY_KIND},
};

fn target_of(n: u32) -> u64 {
    0x30_0000 + u64::from(n) * 0x40
}

fn start<C: Compiler>(count: u32, compiler: C, workers: u32) -> (Runner<C, Addresses, Mail>, Arc<Mailbox>) {
    let functions: Vec<Function> = (0..count).map(function).collect();
    let mail = Arc::new(Mailbox::default());
    let runner = Runner::start(
        session(&functions),
        &[],
        compiler,
        Addresses((0..count).map(|n| (key(n), target_of(n))).collect()),
        Mail(Arc::clone(&mail)),
        Config { workers, idle: false },
        None,
    )
    .unwrap();
    (runner, mail)
}

/// Waits until all `workers` of `runner` sleep.
fn asleep<C: Compiler>(runner: &Runner<C, Addresses, Mail>, workers: u32) {
    blaze_spin::SpinWait::within("every worker to sleep", Duration::from_secs(60), || runner.parked() == workers);
}

fn ready<C: Compiler>(runner: &Runner<C, Addresses, Mail>, mail: &Mailbox, seq: u32, lazy: u32) {
    runner.frame(0, ASK_KIND, seq, &wire::ask(u64::from(seq), lazy)).unwrap();
    let (kind, body) = mail.wait(0, seq);
    assert_eq!(kind, READY_KIND, "ask {seq} of function {lazy}");
    assert_eq!(Frame::decode(&body).unwrap(), Frame::Ready { request: u64::from(seq), lazy, target: target_of(lazy) });
}

#[test]
fn asks_while_spinning_and_after_every_worker_sleeps_are_each_answered() {
    for workers in [1, 2, 4] {
        let compiler = Counting::default();
        let (runner, mail) = start(8, compiler.clone(), workers);
        for lazy in 0..8 {
            ready(&runner, &mail, lazy + 1, lazy);
            // Even asks come while a worker spins; odd ones after every worker went to sleep.
            if lazy % 2 == 1 {
                asleep(&runner, workers);
            }
        }
        let closed = runner.close();
        assert_eq!(closed.compiles, 8, "{workers} workers");
        assert_eq!(compiler.seen.lock().len(), 8);
    }
}

#[test]
fn a_burst_of_asks_is_compiled_once_each_across_the_workers() {
    const FUNCTIONS: u32 = 64;
    let compiler = Counting { delay: Duration::from_millis(1), ..Counting::default() };
    let (runner, mail) = start(FUNCTIONS, compiler.clone(), 4);
    asleep(&runner, 4);
    for lazy in 0..FUNCTIONS {
        runner.frame(0, ASK_KIND, lazy + 1, &wire::ask(u64::from(lazy + 1), lazy)).unwrap();
    }
    for lazy in 0..FUNCTIONS {
        let (kind, _) = mail.wait(0, lazy + 1);
        assert_eq!(kind, READY_KIND);
    }
    let closed = runner.close();
    assert_eq!(closed.compiles, u64::from(FUNCTIONS));
    let seen = compiler.seen.lock();
    let distinct: std::collections::HashSet<_> = seen.iter().collect();
    assert_eq!((seen.len(), distinct.len()), (FUNCTIONS as usize, FUNCTIONS as usize), "each compiled once");
}

#[test]
fn prefetched_callees_are_taken_by_sleeping_workers() {
    #[derive(Clone)]
    struct Calls;
    impl Compiler for Calls {
        fn compile(&self, at: blaze_codegen::InstanceKey) -> Result<blaze_codegen::CompiledFunction, String> {
            let n = (0..8).find(|n| key(*n) == at).ok_or("not a function of the test")?;
            let callees: Vec<u32> = if n == 0 { (1..8).collect() } else { Vec::new() };
            Ok(compiled(n, &callees, &[]))
        }
        fn hit(&self, _: blaze_codegen::InstanceKey) -> Option<Result<blaze_codegen::CompiledFunction, String>> {
            None
        }
        fn tier_up(&self, at: blaze_codegen::InstanceKey, _: blaze_stencil::tier::Tier) -> Result<blaze_codegen::CompiledFunction, String> {
            Err(format!("{at:?}: not tiered"))
        }
    }
    let (runner, mail) = start(8, Calls, 3);
    asleep(&runner, 3);
    ready(&runner, &mail, 1, 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    while (1..8).any(|n| runner.target(&key(n)).is_none()) {
        assert!(Instant::now() < deadline, "the callees prefetched within a minute");
        std::thread::yield_now();
    }
    for lazy in 1..8 {
        ready(&runner, &mail, lazy + 1, lazy);
    }
    assert_eq!(runner.close().compiles, 8);
}

#[test]
fn a_prefetch_from_the_daemon_wakes_a_sleeping_worker() {
    let compiler = Counting::default();
    let (runner, _mail) = start(4, compiler.clone(), 2);
    asleep(&runner, 2);
    runner.want(&[crate::LazyId(3)], Priority::History);
    let deadline = Instant::now() + Duration::from_secs(60);
    while runner.target(&key(3)).is_none() {
        assert!(Instant::now() < deadline, "the prefetch within a minute");
        std::thread::yield_now();
    }
    assert_eq!(runner.close().compiles, 1);
}

#[test]
fn a_close_ends_spinning_and_sleeping_workers_at_once() {
    for sleeping in [false, true] {
        let (runner, mail) = start(2, Counting::default(), 4);
        ready(&runner, &mail, 1, 0);
        if sleeping {
            asleep(&runner, 4);
        }
        let start = Instant::now();
        runner.close();
        assert!(start.elapsed() < Duration::from_secs(5), "closed in {:?}", start.elapsed());
    }
}

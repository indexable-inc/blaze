//! A lone ask whose code the compiler has already is placed and answered by the thread that
//! read it, never compiled; its callees are still prefetched; a probe that panics fails the
//! function naming it; and asks for hits from many threads at once start each function once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use blaze_codegen::{CompiledFunction, InstanceKey};
use blaze_stencil::tier::Tier;
use parking_lot::Mutex;

use super::flight::{Addresses, Counting, Mail, Mailbox, session};
use super::{compiled, function, key};
use crate::{
    plan::Function,
    runner::{Compiler, Config, Runner},
    wire::{self, ASK_KIND, FAILED_KIND, Frame, READY_KIND},
};

pub(super) fn target_of(n: u32) -> u64 {
    0x50_0000 + u64::from(n) * 0x40
}

pub(super) fn start<C: Compiler>(count: u32, compiler: C, workers: u32) -> (Runner<C, Addresses, Mail>, Arc<Mailbox>) {
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

/// Past [`crate::runner`]'s `ALONE`, so the next ask is a lone one.
pub(super) fn pause() {
    blaze_spin::busy(Duration::from_millis(1));
}

#[test]
fn a_lone_hit_is_answered_before_the_frame_returns_and_never_compiled() {
    let compiler = Counting { hits: (0..4).map(key).collect(), ..Counting::default() };
    let (runner, mail) = start(4, compiler.clone(), 1);
    for lazy in 0..4 {
        pause();
        let seq = lazy + 1;
        runner.frame(0, ASK_KIND, seq, &wire::ask(u64::from(seq), lazy)).unwrap();
        assert!(mail.has(0, seq), "the hit for {lazy} answered on the reading thread");
        let (kind, body) = mail.wait(0, seq);
        assert_eq!(kind, READY_KIND);
        assert_eq!(Frame::decode(&body).unwrap(), Frame::Ready { request: u64::from(seq), lazy, target: target_of(lazy) });
        assert_eq!(runner.target(&key(lazy)), Some(target_of(lazy)));
    }
    let closed = runner.close();
    assert_eq!(closed.compiles, 4);
    assert!(compiler.seen.lock().is_empty(), "nothing compiled");
}

#[test]
fn a_miss_after_a_probe_is_compiled_by_a_worker() {
    let compiler = Counting { hits: vec![key(0)], ..Counting::default() };
    let (runner, mail) = start(2, compiler.clone(), 1);
    pause();
    runner.frame(0, ASK_KIND, 1, &wire::ask(1, 1)).unwrap();
    assert_eq!(mail.wait(0, 1).0, READY_KIND);
    assert_eq!(*compiler.seen.lock(), vec![key(1)]);
    assert_eq!(runner.close().compiles, 1);
}

#[test]
fn a_hits_callees_are_prefetched_by_the_workers() {
    /// Has function 0, which calls 1 to 7, compiled already; compiles the rest.
    #[derive(Clone, Default)]
    struct Memo(Arc<Mutex<Vec<InstanceKey>>>);
    impl Compiler for Memo {
        fn compile(&self, at: InstanceKey) -> Result<CompiledFunction, String> {
            let n = (1..8).find(|n| key(*n) == at).ok_or("compiled, not a miss")?;
            self.0.lock().push(at);
            Ok(compiled(n, &[], &[]))
        }
        fn hit(&self, at: InstanceKey) -> Option<Result<CompiledFunction, String>> {
            (at == key(0)).then(|| Ok(compiled(0, &(1..8).collect::<Vec<_>>(), &[])))
        }
        fn tier_up(&self, at: InstanceKey, _: Tier) -> Result<CompiledFunction, String> {
            Err(format!("{at:?}: not tiered"))
        }
    }
    let memo = Memo::default();
    let (runner, mail) = start(8, memo.clone(), 3);
    pause();
    runner.frame(0, ASK_KIND, 1, &wire::ask(1, 0)).unwrap();
    assert!(mail.has(0, 1));
    let deadline = Instant::now() + Duration::from_secs(60);
    while (1..8).any(|n| runner.target(&key(n)).is_none()) {
        assert!(Instant::now() < deadline, "the callees prefetched within a minute");
        std::thread::yield_now();
    }
    let seen = memo.0.lock().clone();
    let distinct: std::collections::HashSet<_> = seen.iter().copied().collect();
    assert_eq!(seen.len(), 7, "each callee compiled once");
    assert_eq!(distinct, (1..8).map(key).collect());
    assert_eq!(runner.close().compiles, 8);
}

#[test]
fn a_probe_that_panics_fails_the_function_naming_it_and_stops_the_run() {
    let compiler = Counting { hits: vec![key(1)], panic: vec![key(1)], ..Counting::default() };
    let (runner, mail) = start(2, compiler.clone(), 1);
    pause();
    runner.frame(0, ASK_KIND, 1, &wire::ask(1, 1)).unwrap();
    let (kind, body) = mail.wait(0, 1);
    assert_eq!(kind, FAILED_KIND);
    let Frame::Failed { lazy, why, .. } = Frame::decode(&body).unwrap() else { panic!("not a failure") };
    assert_eq!(lazy, 1);
    assert!(why.contains("the compiler panicked: boom") && why.contains("on its first call"), "{why}");
    assert_eq!(mail.stops().len(), 1);
    assert!(compiler.seen.lock().is_empty());
    assert_eq!(runner.close().failures.len(), 1);
}

#[test]
fn many_threads_asking_for_hits_at_once_start_each_function_once() {
    const FUNCTIONS: u32 = 16;
    const THREADS: u32 = 16;
    let compiler = Counting { hits: (0..FUNCTIONS).map(key).collect(), ..Counting::default() };
    let (runner, mail) = start(FUNCTIONS, compiler.clone(), 4);
    std::thread::scope(|scope| {
        for client in 0..THREADS {
            let (runner, mail) = (&runner, &mail);
            scope.spawn(move || {
                for step in 0..FUNCTIONS {
                    let (seq, lazy) = (step + 1, (step + client) % FUNCTIONS);
                    if step % 4 == 0 {
                        pause();
                    }
                    runner.frame(client, ASK_KIND, seq, &wire::ask(u64::from(seq), lazy)).unwrap();
                    let (kind, body) = mail.wait(client, seq);
                    assert_eq!(kind, READY_KIND);
                    assert_eq!(
                        Frame::decode(&body).unwrap(),
                        Frame::Ready { request: u64::from(seq), lazy, target: target_of(lazy) }
                    );
                }
            });
        }
    });
    let closed = runner.close();
    assert_eq!(closed.compiles, u64::from(FUNCTIONS));
    assert!(closed.failures.is_empty());
    assert!(mail.stops().is_empty());
}

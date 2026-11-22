//! The flip lock's wait (core.rs `lock_step`, thoughts2 5.3): a jittered doubling backoff from
//! 100 us to the old fixed 2 ms, ended by its summed time (60 s), its jitter drawn from the
//! process's nonce. Under the simulation another process holds the lock (`Sim::hold_lock`):
//! the commit lands within one capped step of the lock's release, the wait is counted
//! (`Plain::lock_waited`), a lock held for ever stops the store by name within the bound, and
//! two runs of one seed wait alike (rule 25: the schedule holds no clock or randomness of its own).
//! Each simulated test runs under both strategies of the seam (`LockStep`, rule 78): the backoff
//! and the old fixed 2 ms step, its control.

use std::{alloc::Global, time::Duration};

use blaze_core::{
    Driver, Effect, FileName, LockMode,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Namespace, Tag},
    store::StoreError,
};
use blaze_mem::Frame;

use super::{
    core::{LockStep, LockWaited, Plain, PlainConfig, TAGS},
    test_root,
};
use crate::Key;

const FIRST_NS: u64 = 100_000;
const MOST_NS: u64 = 2_000_000;
const BOUND_NS: u64 = 60_000_000_000;

/// Both strategies of the seam (rule 78): every simulated test below runs under each.
const STEPS: [LockStep; 2] = [LockStep::Backoff, LockStep::Fixed];

fn cfg(nonce: u64, lock_step: LockStep, root: &Frame) -> PlainConfig<&Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: TAGS,
        cap_bytes: u64::MAX,
        boot: 1,
        nonce,
        durable: false,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: super::Liveness::Everything,
        lock_step,
    }
}

fn flip_file() -> FileName {
    FileName::new("plain/flip").expect("a name")
}

/// Runs `store` until nothing of it is in flight, or `polls` polls; whether it settled.
fn drive(sim: &mut Sim, store: &mut Plain<&Frame>, effects: &mut Vec<Effect>, polls: u32) -> bool {
    let mut done = Vec::new_in(Global);
    for _ in 0..polls {
        sim.submit(effects);
        if sim.in_flight() == 0 {
            return true;
        }
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
        let wall = sim.wall_ns();
        for completion in done.drain(..) {
            store.complete(completion, wall, effects);
        }
    }
    false
}

/// An opened store with one record put, its commit asked while another process holds the flip
/// lock.
fn committing_under_a_held_lock<'f>(sim: &mut Sim, nonce: u64, lock_step: LockStep, root: &'f Frame) -> (Plain<&'f Frame>, Vec<Effect>) {
    let mut store = Plain::new(cfg(nonce, lock_step, root), root);
    let mut effects = Vec::new_in(Global);
    // A commit lays out against a whole store (core.rs `commit_records`): the eager open.
    store.open_whole(&mut effects);
    assert!(drive(sim, &mut store, &mut effects, 10_000), "the open settles");
    assert!(store.ready(), "the open ends ready: {:?}", store.broken());
    store.put(Key(ContentId::of("lock wait test", b"one")), Namespace::LOCAL, Tag(1), b"the record").expect("a put");
    sim.hold_lock(&flip_file(), LockMode::Exclusive);
    store.commit(&[], sim.wall_ns(), &mut effects).expect("a commit is asked");
    (store, effects)
}

/// The schedule: each step in `[base / 2, base]` with `base = min(FIRST << k, MOST)`; the first
/// under 100 us; from the sixth on, at the old fixed step's half or more; every nonce alike in
/// its bounds and its own in its draws.
#[test]
fn the_steps_double_from_100_us_to_2_ms_jittered_by_the_nonce() {
    for nonce in [0u64, 1, 100, u64::MAX, 0x5eed] {
        for k in 0..80u32 {
            let base = FIRST_NS.checked_shl(k.min(32)).unwrap_or(u64::MAX).min(MOST_NS);
            let step = super::core::lock_step(nonce, k);
            assert!(step >= base / 2 && step <= base, "nonce {nonce} ask {k}: {step} ns outside [{}, {base}]", base / 2);
            assert_eq!(step, super::core::lock_step(nonce, k), "a pure function of (nonce, k)");
        }
        assert!(super::core::lock_step(nonce, 0) <= FIRST_NS);
    }
    // The jitter parts two processes: of the first 64 asks, two nonces differ on most.
    let differ = (0..64).filter(|&k| super::core::lock_step(1, k) != super::core::lock_step(2, k)).count();
    assert!(differ > 48, "two nonces differ on {differ} of 64 steps");
}

/// The bound is a sum: the steps a flip may ask before 60 s are at most 60 s / 1 ms (each step
/// past the fifth at least the cap's half) plus the five short ones, and the sum of those it
/// asked stays under 60 s plus one step.
#[test]
fn the_wait_is_bounded_by_its_summed_time() {
    let nonce = 7;
    let (mut asks, mut summed) = (0u32, 0u64);
    while summed < BOUND_NS {
        summed += super::core::lock_step(nonce, asks);
        asks += 1;
        assert!(asks <= 60_006, "past the most asks the bound allows");
    }
    assert_eq!(summed, super::core::lock_waited_ns(nonce, asks), "the running sum is the oracle's");
    assert!(summed < BOUND_NS + MOST_NS, "{summed} ns summed");
    assert!(asks >= 30_000, "{asks} asks: the steps are at most the cap");
}

/// Invariant: a lock another process holds for a while delays the commit, which lands
/// within one capped step (plus the simulation's latency) of the release; the wait is counted.
#[test]
fn a_commit_lands_within_one_step_of_the_locks_release() {
    let root = test_root();
    for (seed, lock_step) in (0..8u64).flat_map(|seed| STEPS.map(|it| (seed, it))) {
        let mut sim = Sim::new(seed, Faults::default());
        let (mut store, mut effects) = committing_under_a_held_lock(&mut sim, 100 + seed, lock_step, &root);
        let held = Duration::from_millis(37);
        let release_at = sim.now() + held;
        let mut done = Vec::new_in(Global);
        while sim.now() < release_at {
            sim.submit(&mut effects);
            sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for completion in done.drain(..) {
                store.complete(completion, wall, &mut effects);
            }
            assert!(store.take_committed().is_none(), "seed {seed}: committed while the lock was held");
        }
        sim.release_lock(&flip_file());
        let released = sim.now();
        assert!(drive(&mut sim, &mut store, &mut effects, 100_000), "seed {seed}: the commit settles");
        assert!(matches!(store.take_committed(), Some(Ok(_))), "seed {seed}: the commit lands: {:?}", store.broken());
        let waited = store.lock_waited();
        assert!(waited.asks >= 5, "seed {seed}: {waited:?}");
        assert!(waited.ns >= held.as_nanos() as u64 / 2, "seed {seed}: {waited:?} for a hold of {held:?}");
        // From the release to the end: at most one step, then the flip's own effects.
        let after = sim.now() - released;
        assert!(after < Duration::from_millis(2) + Duration::from_secs(1), "seed {seed}: {after:?} after the release");
    }
}

/// A lock held for ever stops the store by name once the wait sums to 60 s, never later than one
/// step past it, and never earlier than the bound.
#[test]
fn a_lock_held_for_ever_stops_the_store_naming_the_flip_lock() {
    let root = test_root();
    for lock_step in STEPS {
        let mut sim = Sim::new(3, Faults::default());
        let (mut store, mut effects) = committing_under_a_held_lock(&mut sim, 11, lock_step, &root);
        let began = sim.now();
        assert!(drive(&mut sim, &mut store, &mut effects, 400_000), "{lock_step:?}: the wait ends by its bound");
        let stopped = matches!(store.take_committed(), Some(Err(StoreError::Stalled { waited_for }))
            if waited_for.contains("flip lock")) || matches!(store.broken(), Some(StoreError::Stalled { waited_for }) if waited_for.contains("flip lock"));
        assert!(stopped, "{lock_step:?}: the store stops naming the flip lock: {:?}", store.broken());
        let LockWaited { ns, .. } = store.lock_waited();
        assert!(ns >= BOUND_NS - MOST_NS && ns < BOUND_NS + MOST_NS, "{lock_step:?}: {ns} ns waited");
        assert!(sim.now() - began >= Duration::from_nanos(BOUND_NS - MOST_NS), "{lock_step:?}: stopped early: {:?}", sim.now() - began);
    }
}

/// The control is the old schedule exactly: every step 2 ms, so the bound is 30,000 asks.
#[test]
fn the_fixed_control_is_the_old_2_ms_step() {
    for k in [0u32, 1, 5, 29_999, 60_000] {
        assert_eq!(LockStep::Fixed.step_ns(7, k), MOST_NS);
        assert_eq!(LockStep::Backoff.step_ns(7, k), super::core::lock_step(7, k));
    }
}

/// Rule 25: one seed, one nonce, one wait: the steps hold no clock or randomness of their own.
#[test]
fn one_seed_waits_alike() {
    let root = test_root();
    let run = |lock_step| {
        let mut sim = Sim::new(5, Faults::default());
        let (mut store, mut effects) = committing_under_a_held_lock(&mut sim, 42, lock_step, &root);
        let mut done = Vec::new_in(Global);
        for _ in 0..200 {
            sim.submit(&mut effects);
            sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for completion in done.drain(..) {
                store.complete(completion, wall, &mut effects);
            }
        }
        sim.release_lock(&flip_file());
        assert!(drive(&mut sim, &mut store, &mut effects, 100_000));
        store.lock_waited()
    };
    for lock_step in STEPS {
        assert_eq!(run(lock_step), run(lock_step), "{lock_step:?}");
    }
}

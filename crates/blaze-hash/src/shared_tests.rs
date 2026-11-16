//! A pin of a shared table names the thread it is made on after its fiber moved: a
//! coroutine (corosensei's, as blaze's fibers are) pins a map, suspends, and is resumed on a
//! worker thread the tape picks, each pin after a move checked against the thread it runs on.
//! The crate is optimized in test builds too (`profile.dev.package`), so the compiler may keep
//! a thread-local's address across the suspend as it did in the daemon.

use std::thread;

use blaze_sim::{Config, Tape};
use corosensei::{Coroutine, CoroutineResult};
use crossbeam_channel::{Receiver, Sender, bounded};
use papaya::Guard as _;

use crate::SharedMap;

type Map = SharedMap<u32, u32>;

/// A coroutine moved between threads as blaze's fibers are. What is on its stack when it is
/// suspended is a `&'static` map and integers: no guard, and nothing tied to a thread but
/// what the compiler may keep of a thread-local's address, which is what the test looks for.
struct Fiber(Coroutine<usize, (usize, usize), ()>);

// SAFETY: as above, nothing on the coroutine's stack at a suspend is tied to its thread but
// what the test checks.
unsafe impl Send for Fiber {}

/// The fiber: `hops` times, a guard taken as a pin takes one (or, planted, papaya's pin
/// inlined into the loop), the thread it names and the thread it runs on yielded, an entry
/// written through a pin, and a suspend.
fn fiber<const INLINED: bool>(map: &'static Map, hops: u32) -> Fiber {
    Fiber(Coroutine::new(move |yielder, mut on: usize| {
        for at in 0..hops {
            let guard = if INLINED { map.guard_inlined() } else { map.guard() };
            let made = guard.thread_id();
            drop(guard);
            map.pin().insert(at, at);
            on = yielder.suspend((made, on));
        }
    }))
}

/// A worker thread: resumes each fiber it is sent with the reclaimer's id of its own thread,
/// and sends back what the fiber yielded, or `None` once it ended.
fn worker(map: &'static Map, fibers: Receiver<Fiber>, back: Sender<(Fiber, Option<(usize, usize)>)>) {
    // Taken outside any fiber: this thread's own id.
    let here = map.guard().thread_id();
    for mut fiber in fibers {
        let yielded = match fiber.0.resume(here) {
            CoroutineResult::Yield(it) => Some(it),
            CoroutineResult::Return(()) => None,
        };
        if back.send((fiber, yielded)).is_err() {
            return;
        }
    }
}

/// Runs a fiber of `hops` pins over `threads` workers, each resume on the worker the tape
/// picks: the first pin whose guard names another thread than the one it ran on.
fn moved_pins(tape: &Tape, threads: usize, hops: u32, inlined: bool) -> Result<(), String> {
    let map: &'static Map = Box::leak(Box::default());
    let (back_tx, back) = bounded(1);
    let workers: Vec<Sender<Fiber>> = (0..threads)
        .map(|at| {
            let (tx, rx) = bounded(1);
            let back_tx = back_tx.clone();
            thread::Builder::new()
                .name(format!("shared-tests-{at}"))
                .spawn(move || worker(map, rx, back_tx))
                .expect("a worker thread starts");
            tx
        })
        .collect();
    let mut fiber = match inlined {
        true => fiber::<true>(map, hops),
        false => fiber::<false>(map, hops),
    };
    let mut found = Ok(());
    loop {
        let on = tape.index(threads, "the worker that resumes the fiber");
        workers[on].send(fiber).expect("the worker takes the fiber");
        let (back_fiber, yielded) = back.recv().expect("the worker gives the fiber back");
        fiber = back_fiber;
        match yielded {
            Some((made, ran)) if made != ran && found.is_ok() => {
                found = Err(format!("a pin made on thread {ran} named thread {made}"));
            }
            Some(_) => {}
            None => break,
        }
    }
    drop(workers);
    assert_eq!(map.len(), hops as usize, "every pin's insert landed");
    found
}

#[test]
fn a_pin_names_the_thread_it_runs_on_after_its_fiber_moves() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let threads = 2 + tape.index(3, "threads");
        let hops = 1 + tape.below(16, "hops") as u32;
        if let Err(why) = moved_pins(tape, threads, hops, false) {
            panic!("{why}");
        }
    });
}

/// The planted bug: papaya's pin inlined into the fiber's loop, as it was into the codegen
/// walk's count. A fiber that moves at every resume makes a pin with the thread it left.
#[test]
fn the_check_catches_a_pin_inlined_across_a_move() {
    let tape = Tape::replay(&[0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1]);
    assert!(
        moved_pins(&tape, 2, 16, true).is_err(),
        "a pin inlined across the fiber's moves named the thread it ran on each time"
    );
}

//! Model tests of the table of runs ([`super::table`]) under `--cfg blaze_check`: every
//! interleaving of claim, join, end, give-way, cancel and grow on two and three threads, on
//! tables of one bucket (two entries), so a third run under way grows the table.
//! A waiter's suspension is a spin on its own state, each round a schedule point; the
//! scheduler's flights are numbers.

use std::sync::Arc;

use blaze_check::model;

use super::table::{Asked, Claim, Idle, Runs, Waiter};
use crate::{
    Key,
    spin::SpinWait,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        thread,
    },
};

fn key(n: u8) -> Key {
    Key([n; 32])
}

/// Flights claimed, numbered from 1, and the last one claimed.
#[derive(Default)]
struct Flights {
    next: AtomicU64,
}

impl Flights {
    fn claim(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Runs of one key under way at once, which is never more than one.
#[derive(Default)]
struct Once(AtomicU32);

impl Once {
    fn run(&self, value: u32) -> u32 {
        assert_eq!(self.0.fetch_add(1, Ordering::SeqCst), 0, "two runs of one key at once");
        self.0.fetch_sub(1, Ordering::SeqCst);
        value
    }
}

enum Got {
    /// Ran the key itself, on this value.
    Ran(u32),
    /// Waited and was written the runner's value.
    Waited(u32),
    /// Gave way.
    Left,
}

/// Task `me` asks for `key` and, given the run, ends it on `value`. A waiter waits for its end
/// and asks again after a cancel; with `give_way`, it leaves at once instead, as one whose wait
/// the scheduler refused.
fn ask(runs: &Runs<u32>, flights: &Flights, once: &Once, key: Key, me: u64, value: u32, give_way: bool) -> Got {
    loop {
        let waiter = Waiter::new();
        match runs.ask(key, me, None, &waiter, &mut |_, _| flights.claim()) {
            Asked::Run(claim) => {
                let value = once.run(value);
                runs.end(&claim, Some(&value));
                return Got::Ran(value);
            }
            Asked::Itself => panic!("task {me} asked for a key it runs"),
            Asked::Wait { .. } if give_way => {
                runs.leave(&key, &waiter);
                return Got::Left;
            }
            Asked::Wait { .. } => {
                SpinWait::until("the run's end", || !waiter.is_running());
                match waiter.ended().expect("ended") {
                    Some(value) => return Got::Waited(value),
                    None => continue,
                }
            }
        }
    }
}

fn hold(runs: &Runs<u32>, key: Key, me: u64) -> Claim<'_> {
    match runs.claim_if_idle(key, me, None) {
        Idle::Claimed(claim) => claim,
        Idle::Busy | Idle::Itself => panic!("key held already"),
    }
}

/// Two tasks ask for one key: one runs it, and the other waits and is written its value, or
/// asks after the run ended and runs it itself. Nothing is left running or linked.
#[test]
fn model_a_waiter_gets_the_runs_value_or_runs_the_key_itself() {
    model(|| {
        let (runs, flights, once) = (Arc::new(Runs::<u32>::new(1)), Arc::new(Flights::default()), Arc::new(Once::default()));
        let other = {
            let (runs, flights, once) = (runs.clone(), flights.clone(), once.clone());
            thread::spawn(move || ask(&runs, &flights, &once, key(1), 2, 20, false))
        };
        let mine = ask(&runs, &flights, &once, key(1), 1, 10, false);
        let theirs = other.join().unwrap();
        match (mine, theirs) {
            (Got::Ran(10), Got::Waited(10) | Got::Ran(20)) | (Got::Waited(20) | Got::Ran(10), Got::Ran(20)) => {}
            _ => panic!("a waiter was written another run's value"),
        }
        assert_eq!(runs.running(), 0);
        assert_eq!(runs.waiters(&key(1)), 0);
    });
}

/// A run held, then cancelled, while another task asks: the waiter is written the cancel and
/// asks again, running the key itself.
#[test]
fn model_a_cancelled_run_sends_its_waiter_to_ask_again() {
    model(|| {
        let (runs, flights, once) = (Arc::new(Runs::<u32>::new(1)), Arc::new(Flights::default()), Arc::new(Once::default()));
        let held = hold(&runs, key(1), 1);
        let other = {
            let (runs, flights, once) = (runs.clone(), flights.clone(), once.clone());
            thread::spawn(move || ask(&runs, &flights, &once, key(1), 2, 20, false))
        };
        assert_eq!(runs.end(&held, None).is_some(), flights.next.load(Ordering::SeqCst) == 1);
        assert!(matches!(other.join().unwrap(), Got::Ran(20)), "the waiter was written a value of a cancelled run");
        assert_eq!(runs.running(), 0);
    });
}

/// A waiter whose wait is refused leaves while the run ends: it unlinks itself, or waits for
/// the runner to write it its end, and the run's end returns either way.
#[test]
fn model_a_waiter_that_gives_way_leaves_while_the_run_ends() {
    model(|| {
        let (runs, flights, once) = (Arc::new(Runs::<u32>::new(1)), Arc::new(Flights::default()), Arc::new(Once::default()));
        let held = hold(&runs, key(1), 1);
        let other = {
            let (runs, flights, once) = (runs.clone(), flights.clone(), once.clone());
            thread::spawn(move || ask(&runs, &flights, &once, key(1), 2, 20, true))
        };
        runs.end(&held, Some(&10));
        assert!(matches!(other.join().unwrap(), Got::Left | Got::Ran(20)));
        assert_eq!(runs.running(), 0);
        assert_eq!(runs.waiters(&key(1)), 0);
    });
}

/// A run with a waiter, while a third task holds two more keys at once, growing the table:
/// the run and its waiter move with it, and the waiter is written the run's value.
#[test]
fn model_a_grow_moves_a_run_and_its_waiter() {
    model(|| {
        let (runs, flights, once) = (Arc::new(Runs::<u32>::new(1)), Arc::new(Flights::default()), Arc::new(Once::default()));
        let held = hold(&runs, key(1), 1);
        let waiter = {
            let (runs, flights, once) = (runs.clone(), flights.clone(), once.clone());
            thread::spawn(move || ask(&runs, &flights, &once, key(1), 2, 20, false))
        };
        let filler = {
            let runs = runs.clone();
            thread::spawn(move || {
                let (two, three) = (hold(&runs, key(2), 3), hold(&runs, key(3), 3));
                assert_eq!(runs.size().0, 2, "a third run under way grew the table");
                runs.end(&two, Some(&2));
                runs.end(&three, Some(&3));
            })
        };
        filler.join().unwrap();
        runs.end(&held, Some(&10));
        assert!(matches!(waiter.join().unwrap(), Got::Waited(10) | Got::Ran(20)));
        assert_eq!(runs.running(), 0);
    });
}

/// Two tasks claim three keys, which may grow the table between a claim and its end: each end
/// finds its run, in whichever table is current, and no run is lost.
#[test]
fn model_claims_and_ends_across_a_grow() {
    model(|| {
        let runs = Arc::new(Runs::<u32>::new(1));
        let other = {
            let runs = runs.clone();
            thread::spawn(move || {
                let claim = hold(&runs, key(2), 2);
                runs.end(&claim, Some(&2));
            })
        };
        let claim = hold(&runs, key(1), 1);
        let more = hold(&runs, key(3), 1);
        runs.end(&claim, Some(&1));
        runs.end(&more, Some(&3));
        other.join().unwrap();
        assert_eq!(runs.running(), 0);
    });
}

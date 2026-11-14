//! Single flight: one run per key at a time, a second asker waiting for it. Injected, so the
//! memo's core stays sans-IO: [`Scheduled`] parks waiters on the scheduler, [`Alone`] is for one
//! thread, and the tests' simulation brings its own.

use std::alloc::Global;

use blaze_core::lock::Lock;
use blaze_graph::{
    Key,
    pump::{Flights, NotHeld},
};

use crate::{ArenaId, Kind};

/// The key a run is single-flighted by: the query kind's id, the home arena and the key's
/// hash. Two keys sharing one only make one wait for the other and ask again.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FlightKey(pub [u8; 32]);

impl FlightKey {
    pub(crate) fn of(kind: &Kind, arena: ArenaId, hash: u64) -> FlightKey {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&kind.id().to_le_bytes());
        bytes[8..12].copy_from_slice(&arena.bits().to_le_bytes());
        bytes[12..20].copy_from_slice(&hash.to_le_bytes());
        FlightKey(bytes)
    }
}

/// What a run tells [`Flight::single`] when it returns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hold {
    /// The run's value is published (or it published nothing): let the key go.
    End,
    /// The run was a fixpoint member: its value is final only when its head settles, so the key
    /// stays held, by this task, until then ([`Flown::Kept`]). An asker from another task waits
    /// for the head instead of running the member a second time.
    Keep,
}

/// A key [`Flight::single`] kept held after its run (`Hold::Keep`): let go when dropped. The
/// head of the member's cycle keeps it in its book and drops it after it publishes its members
/// (or when it unwinds, and the waiters ask again).
pub struct Kept<'f>(Box<dyn Guard + 'f, Global>);

trait Guard {}

impl<T> Guard for T {}

impl<'f> Kept<'f> {
    /// `guard`'s drop lets the key go.
    pub fn new<T: 'f>(guard: T) -> Kept<'f> {
        Kept(Box::new_in(guard, Global))
    }
}

impl std::fmt::Debug for Kept<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kept")
    }
}

/// What [`Flight::single`] did.
#[derive(Debug)]
pub enum Flown<'f> {
    /// It ran `run` here, holding the key throughout, and let it go (`Hold::End`).
    Ran,
    /// It ran `run` here and holds the key still (`Hold::Keep`) until the guard drops.
    Kept(Kept<'f>),
    /// Another task held the key; it waited until that run ended (with a value or not).
    Waited,
    /// This task holds the key already: the ask is a cycle on this task's stack.
    Mine,
    /// Waiting would have closed a cycle across tasks: this task gives way.
    Cancelled,
}

/// One run per key at a time.
///
/// The invariant: while `run` runs inside `single(key, ..)`, and after it until the
/// [`Kept`] it returned (on `Hold::Keep`) drops, no other `run` of `key` starts, and an asker
/// of `key` from another task returns `Waited` only after that.
pub trait Flight: Send + Sync + 'static {
    /// Runs `run` holding `key`, or waits for the task that holds it. `named` writes what
    /// the key is, for a wait's span.
    fn single<'f>(
        &'f self,
        key: FlightKey,
        named: &dyn Fn(&mut String),
        run: &mut dyn FnMut() -> Hold,
    ) -> Flown<'f>;
}

/// The scheduler's single flight: a waiter suspends its fiber on the pool, holding no thread;
/// a wait that would close a cycle across tasks gives way (`pump::give_way`).
pub struct Scheduled {
    flights: Flights<()>,
}

impl Scheduled {
    pub fn new() -> Scheduled {
        Scheduled { flights: Flights::new() }
    }
}

impl Default for Scheduled {
    fn default() -> Scheduled {
        Scheduled::new()
    }
}

impl Flight for Scheduled {
    fn single<'f>(
        &'f self,
        key: FlightKey,
        named: &dyn Fn(&mut String),
        run: &mut dyn FnMut() -> Hold,
    ) -> Flown<'f> {
        let key = Key(key.0);
        match self.flights.hold_named(key, |out| named(out)) {
            Ok(held) => {
                // Unwinding drops `held` unended: its waiters ask again.
                match run() {
                    Hold::End => {
                        held.end(&());
                        Flown::Ran
                    }
                    // Dropped unended when the head lets it go: the waiters ask again and
                    // find the member's entry.
                    Hold::Keep => Flown::Kept(Kept::new(held)),
                }
            }
            Err(NotHeld::ByThisTask) => Flown::Mine,
            Err(NotHeld::Busy) => {
                // Waits for the holder; when it has ended by now, claims and ends at once.
                match self.flights.get_or_run_named(key, |out| named(out), || Ok(())) {
                    Ok(()) => Flown::Waited,
                    Err(blaze_graph::Cancelled) => Flown::Cancelled,
                }
            }
        }
    }
}

/// Single flight on one thread: nothing waits; a key held is this thread's own.
pub struct Alone {
    held: Lock<Vec<FlightKey, Global>>,
}

impl Alone {
    pub fn new() -> Alone {
        Alone { held: Lock::new("memo.alone.held", Vec::new_in(Global)) }
    }
}

impl Default for Alone {
    fn default() -> Alone {
        Alone::new()
    }
}

impl Flight for Alone {
    fn single<'f>(
        &'f self,
        key: FlightKey,
        _: &dyn Fn(&mut String),
        run: &mut dyn FnMut() -> Hold,
    ) -> Flown<'f> {
        {
            let mut held = self.held.lock();
            if held.contains(&key) {
                return Flown::Mine;
            }
            held.push(key);
        }
        struct Release<'a>(&'a Alone, FlightKey);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                let mut held = self.0.held.lock();
                let at = held.iter().rposition(|it| *it == self.1).expect("held while it ran");
                held.swap_remove(at);
            }
        }
        let release = Release(self, key);
        match run() {
            Hold::End => Flown::Ran,
            Hold::Keep => Flown::Kept(Kept::new(release)),
        }
    }
}

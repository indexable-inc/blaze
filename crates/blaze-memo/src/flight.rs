//! Single flight: one run per key at a time, a second asker waiting for it. Injected, so the
//! memo's core stays sans-IO: [`Scheduled`] parks waiters on the scheduler, [`Alone`] is for one
//! task, and the tests' simulation brings its own.
//!
//! Whose key it is: every flight answers "this task holds it already" ([`Flown::Mine`], a
//! cycle on the asker's own stack) only to the task that holds the key, and a task is the
//! scheduler's job ([`blaze_graph::sched::job_here`]): a pool's job on its fiber, which keeps
//! its identity as it moves between workers (as the stack of runs does, `task.rs`), or a thread
//! on no fiber, one job for its life. It is the one notion `pump::Flights` itself answers
//! `NotHeld::ByThisTask` by; no flight has a second one. An ask from another task is never
//! `Mine`: [`Scheduled`] waits for the holder (or gives way where waiting would close a cycle
//! of tasks), and [`Alone`], which cannot wait, refuses it with a panic naming the key.

use std::{alloc::Global, fmt};

use blaze_core::lock::Lock;
use blaze_graph::{
    Key,
    pump::{Flights, NotHeld},
    sched::{self, JobId},
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
    /// This task (the scheduler's job, see the module doc) holds the key already: the ask is a
    /// cycle on this task's stack. Never the answer to another task's ask.
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
        named: &dyn Fn(&mut dyn fmt::Write),
        run: &mut dyn FnMut() -> Hold,
    ) -> Flown<'f>;
}

/// The scheduler's single flight: a waiter suspends its fiber on the pool, holding no thread
/// (a thread on no fiber parks); a wait that would close a cycle across tasks gives way
/// (`pump::give_way`). `Mine` is `pump::NotHeld::ByThisTask`: the holder is the job.
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
        named: &dyn Fn(&mut dyn fmt::Write),
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

/// The single flight of one task: nothing waits, so a database on it is driven by one task at a
/// time (a test's database, `base_db::Db::for_tests`, whose batches run on the calling thread).
///
/// The contract, checked at every ask: each held key records its holder, the task as the
/// scheduler names it ([`blaze_graph::sched::job_here`], as `pump::Flights` names it). The
/// holder asking again is [`Flown::Mine`]: a cycle on its own stack. Another task asking a key
/// held is a database driven by several tasks, which this flight cannot serve: it panics, naming
/// the key, this flight and what to take instead ([`Scheduled`]), and never answers `Mine` (the
/// asker would be told it is in a cycle with a run on another task's stack) or waits.
pub struct Alone {
    held: Lock<Vec<(FlightKey, JobId), Global>>,
}

impl Alone {
    pub fn new() -> Alone {
        Alone { held: Lock::new("memo.alone.held", Vec::new_in(Global)) }
    }
}

/// The ask of a key `holder` holds by another task, `asker`, on [`Alone`]: refused, by name.
#[cold]
#[inline(never)]
fn another_holder(named: &dyn Fn(&mut dyn fmt::Write), asker: JobId, holder: JobId) -> ! {
    let mut what = String::new();
    named(&mut what);
    panic!(
        "{what} is asked by task {asker:?} while task {holder:?} runs it, on the single flight of \
         one task (`blaze_memo::Alone`), which cannot wait for another task: a database that \
         several tasks or threads drive takes the scheduler's flight (`blaze_memo::Scheduled`; \
         a test's database: `base_db::Db::for_tests_driven`)"
    )
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
        named: &dyn Fn(&mut dyn fmt::Write),
        run: &mut dyn FnMut() -> Hold,
    ) -> Flown<'f> {
        let me = sched::job_here();
        let holder = {
            let mut held = self.held.lock();
            let holder = held.iter().find(|(it, _)| *it == key).map(|(_, holder)| *holder);
            if holder.is_none() {
                held.push((key, me));
            }
            holder
        };
        // Decided with the lock let go: the panic and `named` run with no guard held.
        match holder {
            None => {}
            Some(holder) if holder == me => return Flown::Mine,
            Some(holder) => another_holder(named, me, holder),
        }
        struct Release<'a>(&'a Alone, FlightKey);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                let mut held = self.0.held.lock();
                let at =
                    held.iter().position(|(it, _)| *it == self.1).expect("held while it ran");
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

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::{Alone, Flight, FlightKey, Flown, Hold};

    fn named(out: &mut dyn std::fmt::Write) {
        out.write_str("the_query(the key)").expect("a name is written");
    }

    /// The invariant: on [`Alone`] a key's holder asking it again is `Mine`, and another task
    /// (here another thread, one job of the scheduler for its life) asking it while it is held
    /// is refused with a panic that names the key, the flight and what to take instead; never
    /// `Mine`. The control: once the holder let the key go, the other task's ask runs it.
    #[test]
    fn alone_refuses_another_holders_ask_by_name() {
        let alone = Alone::new();
        let key = FlightKey([7; 32]);
        let mut inner = None;
        let mut refused = None;
        let flown = alone.single(key, &named, &mut || {
            // The holder again: a cycle on its own stack.
            inner = Some(matches!(alone.single(key, &named, &mut || Hold::End), Flown::Mine));
            // Another task, while this one holds the key.
            refused = Some(std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        catch_unwind(AssertUnwindSafe(|| {
                            alone.single(key, &named, &mut || Hold::End);
                        }))
                        .err()
                        .and_then(|payload| payload.downcast_ref::<String>().cloned())
                    })
                    .join()
                    .expect("the refusal is caught on its thread")
            }));
            Hold::End
        });
        assert!(matches!(flown, Flown::Ran), "the holder ran its key: {flown:?}");
        assert_eq!(inner, Some(true), "the holder's own second ask is `Mine`");
        let why = refused.flatten().expect("another task's ask of a held key panics");
        for says in ["the_query(the key)", "`blaze_memo::Alone`", "`blaze_memo::Scheduled`"] {
            assert!(why.contains(says), "the refusal says {says}: {why}");
        }
        // The control: the key let go, another task's ask runs it.
        let after = std::thread::scope(|scope| {
            scope
                .spawn(|| matches!(alone.single(key, &named, &mut || Hold::End), Flown::Ran))
                .join()
                .expect("an ask of a free key does not panic")
        });
        assert!(after, "another task runs a key nobody holds");
    }
}

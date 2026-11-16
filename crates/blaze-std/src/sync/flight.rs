//! [`Flight`]: the first asker of a key computes it; everyone who asks while it runs waits for
//! that result under their own deadline; the result is then kept for later askers (std.md,
//! "Concurrency: what to reach for").
//!
//! ```text
//! let parsed = Flight::with_capacity_in("bstd.parsed", NonZeroUsize::new(256).unwrap(), alloc);
//! let module = parsed.ask(cx, path, |cx| parse(cx, &path))?;   // computed once, by one asker
//! ```
//!
//! - **Once per key, while it is kept.** At most one computation of a key runs at a time. An
//!   asker that finds it running waits for it ([`Flight::ask`] takes `cx`; a wait that stops
//!   is [`FlightError::Stopped`], and the computation goes on for the others).
//! - **A failure is shared, then forgotten.** A computation that returns an error, or panics,
//!   fails every asker waiting on it with that error ([`FlightError::Failed`],
//!   [`FlightError::Panicked`]) and leaves no entry: the next asker computes again.
//! - **Bounded.** The table holds at most `capacity` keys, its room allocated when it is made.
//!   A new key in a full table takes the place of the oldest finished one; when every key is
//!   still running, the asker gets [`FlightError::Full`] and nothing is computed.
//! - The table is a sans-IO core (`Table`) under one named lock, held only for a scan and a
//!   move, never across a computation. Each computation's outcome is its own cell
//!   (`blaze_own::Arc`), which its waiters hold, so evicting or failing an entry never strands
//!   one.
//! - **Cost.** A lookup is a scan of at most `capacity` keys under the lock (a table meant for
//!   thousands of keys wants a hashed core: same interface). A computation allocates its
//!   outcome cell; a kept value is cloned for each asker.
//! - Asking for a key from inside its own computation waits on itself: it ends at the deadline.

use core::{alloc::AllocatorClone, fmt, mem::DropGuard, num::NonZeroUsize};

use blaze_own::{Arc, Vec};

use crate::{
    Plant, Stopped,
    cx::Cx,
    sync::{Lock, Signal, wait_ready},
};

/// A table of keys computed once each, by their first asker. See the module's doc.
pub struct Flight<K, V, E, A: AllocatorClone> {
    table: Lock<Table<K, Arc<Attempt<V, E>, A>, A>>,
    alloc: A,
}

/// Why an ask gave no value.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error, given to every waiter
pub enum FlightError<E> {
    /// The computation of the key failed with this error.
    Failed { flight: &'static str, error: E },
    /// The computation of the key panicked (rule panics-fail-one-job: its own task failed too).
    Panicked { flight: &'static str },
    /// The table held `capacity` keys, every one still being computed.
    Full { flight: &'static str, capacity: usize },
    /// The wait for the table or for another asker's computation stopped so.
    Stopped(Stopped),
}

/// One computation's outcome, which its waiters hold.
struct Attempt<V, E> {
    outcome: Lock<Option<Result<V, FlightError<E>>>>,
    done: Signal,
}

/// The table: a sans-IO core, its every step a function of it and its arguments. `H` is what
/// an asker holds of a computation (the driver's outcome cell; a test's id).
pub(crate) struct Table<K, H, A: core::alloc::Allocator> {
    entries: Vec<Entry<K, H>, A>,
    capacity: NonZeroUsize,
    /// Numbers each computation, so a finish names the one it ends.
    next: u64,
    plant: Plant,
}

struct Entry<K, H> {
    key: K,
    attempt: u64,
    running: bool,
    held: H,
}

/// What [`Table::ask`] says to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ask<H> {
    /// Compute the key, as computation `attempt`, and finish it with [`Table::finish`].
    Compute { attempt: u64, held: H },
    /// Wait for this computation's outcome (one that ended has it already).
    Wait { attempt: u64, held: H },
    /// Every key is running and the table is full.
    Full,
}

impl<K: Eq, H: Clone, A: core::alloc::Allocator> Table<K, H, A> {
    pub(crate) fn with_capacity_in(capacity: NonZeroUsize, alloc: A, plant: Plant) -> Table<K, H, A> {
        Table { entries: Vec::with_capacity_in(capacity.get(), alloc), capacity, next: 0, plant }
    }

    /// `key` is asked for; `make` makes what its askers hold of a new computation.
    pub(crate) fn ask(&mut self, key: K, make: impl FnOnce() -> H) -> Ask<H> {
        if let Some(at) = self.entries.iter().position(|entry| entry.key == key) {
            let entry = &self.entries[at];
            let again = entry.running && self.plant == Plant::FlightComputesTwice;
            if !again {
                return Ask::Wait { attempt: entry.attempt, held: entry.held.clone() };
            }
            self.entries.remove(at);
        }
        if self.entries.len() == self.capacity.get() {
            // The oldest finished key gives its place; entries are in the order they came.
            match self.entries.iter().position(|entry| !entry.running) {
                Some(at) => drop(self.entries.remove(at)),
                None => return Ask::Full,
            }
        }
        let attempt = self.next;
        self.next += 1;
        let held = make();
        self.entries.push(Entry { key, attempt, running: true, held: held.clone() });
        Ask::Compute { attempt, held }
    }

    /// Computation `attempt` of `key` ended: kept when it `succeeded`, else forgotten so the
    /// next asker computes again. A computation whose entry is gone already changes nothing.
    pub(crate) fn finish(&mut self, key: &K, attempt: u64, succeeded: bool) {
        let Some(at) = self.entries.iter().position(|entry| entry.key == *key && entry.attempt == attempt) else {
            return;
        };
        match succeeded {
            true => self.entries[at].running = false,
            false => drop(self.entries.remove(at)),
        }
    }

    /// How many keys it holds.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The computations running now, by key.
    #[cfg(test)]
    pub(crate) fn running(&self) -> impl Iterator<Item = (&K, u64)> {
        self.entries.iter().filter(|entry| entry.running).map(|entry| (&entry.key, entry.attempt))
    }
}

impl<K: Eq + Clone, V: Clone, E: Clone, A: AllocatorClone> Flight<K, V, E, A> {
    /// A table named `name` of at most `capacity` keys, its room and its computations'
    /// outcome cells allocated in `alloc`.
    pub fn with_capacity_in(name: &'static str, capacity: NonZeroUsize, alloc: A) -> Flight<K, V, E, A> {
        Flight::planted(name, capacity, alloc, Plant::None)
    }

    pub(crate) fn planted(name: &'static str, capacity: NonZeroUsize, alloc: A, plant: Plant) -> Flight<K, V, E, A> {
        // clone: allocators that are clones of one another free each other's blocks.
        let table = Table::with_capacity_in(capacity, alloc.clone(), plant);
        Flight { table: Lock::new(name, table), alloc }
    }

    /// The table's name.
    pub fn name(&self) -> &'static str {
        self.table.name()
    }

    /// The value of `key`: kept, computed now by `compute` (this asker is the first), or waited
    /// for (another asker is computing it), each under `cx`'s deadline.
    pub fn ask(&self, cx: &Cx, key: K, compute: impl FnOnce(&Cx) -> Result<V, E>) -> Result<V, FlightError<E>> {
        let flight = self.name();
        let mut table = self.table.lock(cx).map_err(FlightError::Stopped)?;
        // clone: the key, kept by the table and named again at the finish.
        let asked = table.ask(key.clone(), || {
            // clone: allocators that are clones of one another free each other's blocks.
            let attempt = Attempt { outcome: Lock::new(flight, None), done: Signal::new(flight) };
            Arc::new_in(attempt, self.alloc.clone())
        });
        drop(table);
        match asked {
            Ask::Full => Err(FlightError::Full { flight, capacity: self.capacity() }),
            Ask::Wait { held, .. } => {
                // clone: the outcome, one copy for each asker.
                match wait_ready(cx, &held.outcome, &held.done, |outcome| outcome.clone()) {
                    Ok(outcome) => outcome,
                    Err(stopped) => Err(FlightError::Stopped(stopped)),
                }
            }
            Ask::Compute { attempt, held } => {
                // A panic in `compute` fails its waiters and frees the key, then unwinds on.
                let unwinding = DropGuard::new((), |()| {
                    self.end(&key, attempt, &held, Err(FlightError::Panicked { flight }));
                });
                let made = compute(cx).map_err(|error| FlightError::Failed { flight, error });
                DropGuard::dismiss(unwinding);
                // clone: the outcome, one copy kept for the waiters.
                self.end(&key, attempt, &held, made.clone());
                made
            }
        }
    }

    /// Computation `attempt` of `key` ended with `outcome`: its waiters are told, and the table
    /// keeps or forgets the key. Both locks are held only for a move.
    fn end(&self, key: &K, attempt: u64, held: &Attempt<V, E>, outcome: Result<V, FlightError<E>>) {
        let succeeded = outcome.is_ok();
        *held.outcome.lock_briefly() = Some(outcome);
        held.done.notify_all();
        self.table.lock_briefly().finish(key, attempt, succeeded);
    }

    /// The most keys it holds.
    pub fn capacity(&self) -> usize {
        self.table.lock_briefly().capacity.get()
    }

    /// How many keys it holds now.
    pub fn len(&self) -> usize {
        self.table.lock_briefly().len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<E: fmt::Display> fmt::Display for FlightError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlightError::Failed { flight, error } => write!(f, "bstd::sync::flight {flight}: the computation failed: {error}"),
            FlightError::Panicked { flight } => write!(f, "bstd::sync::flight {flight}: the computation panicked (rule panics-fail-one-job)"),
            FlightError::Full { flight, capacity } => {
                write!(f, "bstd::sync::flight {flight}: all {capacity} keys are being computed; none can give its place")
            }
            FlightError::Stopped(stopped) => write!(f, "bstd::sync::flight: a wait stopped: {stopped}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> core::error::Error for FlightError<E> {}

impl<K, V, E, A: AllocatorClone> fmt::Debug for Flight<K, V, E, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Flight").field("name", &self.table.name()).finish()
    }
}

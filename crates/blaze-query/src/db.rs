//! The dense query engine: queries keyed by [`Id`], memos in place in pages by ID, a warm hit of
//! four dependent loads, durability kinds, early cutoff by `==`, single flight through
//! `blaze_graph`'s plane, fixpoint cycles, captured reads, LRU eviction, and provenance. What the front end
//! runs on (`#[blaze_query::tracked]` and the rest generate against it, see `plumbing`); the
//! content-keyed engine of the crate root is a separate thing.
//!
//! Modules, one concern each: `id` (the ID space), `ingredient` (numbering and vtables),
//! `dense` (columns and the page heap), `row` (a memo in place: the value-in-place rule),
//! `storage` (the database's tables, graph and view), `local` (the thread's query stack),
//! `function` (hit, verify, run, cycles, eviction), `verify` (the claim-free walk), `input`,
//! `interned`, `tracked`, `capture`, `value`, `provenance` (what each memo did in a view, and
//! why it ran).

use std::{fmt, panic::UnwindSafe};

mod capture;
mod counts;
mod dense;
mod function;
mod id;
mod ingredient;
mod input;
mod interned;
mod local;
mod provenance;
mod row;
mod storage;
#[cfg(test)]
mod tests;
mod tracked;
mod value;
mod verify;

pub use capture::{Read, ReadKind, Reads, capture_reads, replay_reads, unshareable_reads};
pub use function::completed;
pub use id::{AsId, FromId, FromIdIn, Id};
pub use input::Setter;
pub use interned::NamesIds;
pub use provenance::{Cause, Outcome, Provenance};
pub use storage::{Database, Durability, Hook, Storage, View};
pub use value::Value;

/// What generated code calls: the ingredient traits, their statics' vtables, and the typed
/// entry points.
pub mod plumbing {
    pub use blaze_mem::Tag;

    pub use super::{
        function::{
            CycleKind, Function, MAX_ROUNDS, evict_values, evict_values_of, fetch, set_lru_capacity,
        },
        ingredient::{Ingredient, Kind, Vtable},
        input::{
            Input, InputSetter, input_fields, input_new, input_new_singleton, input_read,
            input_set, input_singleton,
        },
        interned::{Interned, intern, intern_by, interned_arguments, interned_fields},
        owner_is,
        tracked::{Tracked, tracked_fields, tracked_identity, tracked_new},
        value::{
            HashEqLike, IdOfAsId, IdOfNamed, IdOfOther, IdsOf, Lookup, ReturnsAsDeref,
            ReturnsAsRef, value_hash,
        },
    };
}

/// What [`Storage::memory_usage`] reports of one ingredient.
#[derive(Clone, Copy, Debug)]
pub struct MemoryUsage {
    pub name: &'static str,
    pub rows: u64,
    pub row_bytes: u64,
    pub value_bytes: u64,
}

/// A row of an ingredient: what events and reads name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DatabaseKeyIndex {
    ingredient: u32,
    key: Id,
}

impl DatabaseKeyIndex {
    pub(crate) fn new(ingredient: u32, key: Id) -> DatabaseKeyIndex {
        DatabaseKeyIndex { ingredient, key }
    }

    pub fn ingredient_index(self) -> u32 {
        self.ingredient
    }

    pub fn key_index(self) -> Id {
        self.key
    }
}

impl fmt::Debug for DatabaseKeyIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({:?})", ingredient::name(self.ingredient), self.key)
    }
}

/// What the storage tells its hook, on cold paths only.
#[derive(Clone, Debug)]
pub struct Event {
    pub kind: EventKind,
}

#[derive(Clone, Debug)]
pub enum EventKind {
    WillExecute { database_key: DatabaseKeyIndex },
    DidValidateMemoizedValue { database_key: DatabaseKeyIndex },
    DidInternValue { key: DatabaseKeyIndex },
    WillIterateCycle { database_key: DatabaseKeyIndex, iteration: u32 },
    WillCheckCancellation,
}

/// Why a query call unwound instead of returning: the engine unwinds with it
/// (`resume_unwind`, no panic hook), and [`Cancelled::catch`] turns it back into a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cancelled {
    /// A writer called [`Storage::stop_readers`].
    PendingWrite,
    /// Waiting for a key another task runs would have closed a cycle of tasks: this one gives
    /// way, and meets the cycle on its own stack when it runs again.
    WouldWaitOnItself {
        /// The key whose wait was refused; `None` only from a test that gives way by hand.
        asked: Option<DatabaseKeyIndex>,
    },
    /// A host rethrew a job's panic.
    PropagatedPanic,
}

impl Cancelled {
    pub fn throw(self) -> ! {
        std::panic::resume_unwind(Box::new(self)) // alloc: an unwind's payload
    }

    /// Runs `f`; a [`Cancelled`] it unwinds with is returned, any other panic goes on.
    pub fn catch<T>(f: impl FnOnce() -> T + UnwindSafe) -> Result<T, Cancelled> {
        std::panic::catch_unwind(f).map_err(|payload| match payload.downcast::<Cancelled>() {
            Ok(cancelled) => *cancelled,
            Err(payload) => std::panic::resume_unwind(payload),
        })
    }
}

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cancelled::PendingWrite => f.write_str("pending write"),
            Cancelled::WouldWaitOnItself { asked: Some(asked) } => {
                write!(f, "would wait on itself, asking for {asked:?}")
            }
            Cancelled::WouldWaitOnItself { asked: None } => f.write_str("would wait on itself"),
            Cancelled::PropagatedPanic => f.write_str("propagated panic"),
        }
    }
}

impl std::error::Error for Cancelled {}

/// What a fixpoint's `cycle_fn` is told of the round.
pub struct Cycle<'a> {
    pub(crate) heads: &'a [Id],
    pub(crate) id: Id,
    pub(crate) iteration: u32,
}

impl Cycle<'_> {
    /// The cycle's heads: the query recovering first, then its inner heads.
    pub fn head_ids(&self) -> impl Iterator<Item = Id> + '_ {
        self.heads.iter().copied()
    }

    /// The query recovering.
    pub fn id(&self) -> Id {
        self.id
    }

    /// The round, from 1.
    pub fn iteration(&self) -> u32 {
        self.iteration
    }
}

/// Whether `id`'s page is `ingredient`'s: what each `FromIdIn` of a generated type checks.
#[inline]
pub fn owner_is(storage: &Storage, id: Id, ingredient: &'static ingredient::Ingredient) -> bool {
    storage.owner(id) == Some(ingredient.index())
}

/// The running query made a read the engine can't see: it runs again in every view, and its
/// captures give `None`.
pub fn report_untracked_read<D: Database>(db: &D) {
    let storage = db.storage();
    local::with(|local| {
        if let Some(frame) = local.recording(storage) {
            frame.untracked = true;
            frame.unshareable = true;
            frame.changed = frame.changed.max(storage.view_bits());
        }
    });
}

/// Unwinds with [`Cancelled::PendingWrite`] if a writer stopped readers.
pub fn unwind_if_cancelled<D: Database>(db: &D) {
    let storage = db.storage();
    storage.event(|| EventKind::WillCheckCancellation);
    if storage.stopped() {
        Cancelled::PendingWrite.throw();
    }
}

/// Runs `f`, a wait that may pump other jobs onto this thread (`Db::run_all`): a query pumped
/// inside starts a stack of its own, so its reads and cycle checks never see this thread's.
pub fn pump_barrier<R>(f: impl FnOnce() -> R) -> R {
    let _barrier = local::barrier();
    f()
}

/// `task(at)` for each `at` in `0..count`, each a job of its own on the pool this thread's
/// loops run on, inside a [`pump_barrier`]. A task that gave way
/// ([`Cancelled::WouldWaitOnItself`]: its wait would have closed a cycle of tasks) runs again
/// here once the others ended, on this thread and outside the barrier, where it meets the
/// cycle on its own stack: never in another round on the pool, which would meet the same cycle
/// while this caller (or a task below it) holds what it waits for. A rerun that gives way
/// again unwinds to this caller, which gives way in turn; the first other panic of a job is
/// raised once every job has ended.
///
/// Every loop over jobs that ask queries goes through here (or [`par_map`]): a give-way that
/// left a job some other way would reach a caller with no query below it as a panic with no
/// message.
pub fn par_for_each(count: usize, task: impl Fn(usize) + Sync) {
    let again = pump_barrier(|| {
        blaze_graph::par::for_each_giving_way(count, &task, |panic| {
            matches!(panic.downcast_ref::<Cancelled>(), Some(Cancelled::WouldWaitOnItself { .. }))
        })
    });
    for at in again {
        task(at);
    }
}

/// `f(at)` for each `at` in `0..count`, in order, run as [`par_for_each`] runs its tasks.
pub fn par_map<R: Send>(count: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let made: Vec<parking_lot::Mutex<Option<R>>> =
        // alloc: a slot per result, once per loop
        (0..count).map(|_| parking_lot::Mutex::new(None)).collect();
    par_for_each(count, |at| {
        let it = f(at);
        *made[at].lock() = Some(it);
    });
    made.into_iter()
        .map(|it| it.into_inner().expect("every task ran"))
        // alloc: the results, once per loop
        .collect()
}

/// The thread's query stack, carried with a job on a fiber across its waits.
pub(crate) fn carried() -> blaze_graph::sched::Carried {
    blaze_graph::sched::Carried { take: local::take, put: local::put }
}

/// Puts `db`'s storage where [`with_attached`] finds it (a `Debug` impl's database) while `f`
/// runs on this thread.
pub fn attach<D: Database, R>(db: &D, f: impl FnOnce() -> R) -> R {
    struct Restore(*const Storage);

    impl Drop for Restore {
        fn drop(&mut self) {
            let before = self.0;
            local::with(|local| local.attached = before);
        }
    }

    let storage: *const Storage = db.storage();
    let _restore = Restore(local::with(|local| std::mem::replace(&mut local.attached, storage)));
    f()
}

/// `f` with the storage [`attach`] put on this thread, if any.
pub fn with_attached<R>(f: impl FnOnce(Option<&Storage>) -> R) -> R {
    let storage = local::with(|local| local.attached);
    // SAFETY: `attach` keeps the pointer only while its `&D` borrow (and so the storage) lives,
    // on this thread, restoring the one before when it returns or unwinds.
    f(unsafe { storage.as_ref() })
}

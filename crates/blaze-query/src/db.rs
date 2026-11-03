//! The dense query engine: queries keyed by [`Id`], memos in place in pages by ID, a warm hit of
//! four dependent loads, durability kinds, early cutoff by `==`, single flight through
//! `blaze_graph`'s plane, fixpoint cycles, captured reads, capped values evicted by W-TinyLFU
//! admission (`admission`), and provenance. What the front end
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

mod admission;
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
mod slot;
mod storage;
#[cfg(test)]
mod tests;
mod tracked;
mod value;
mod verify;

pub use admission::Eviction;
pub use capture::{
    Read, ReadKind, Reads, capture_reads, for_span_text, replay_reads, unshareable_reads,
};
pub use counts::{Counts, take as take_counts};
pub use function::completed;
pub use id::{AsId, FromId, FromIdIn, Id};
pub use input::Setter;
pub use interned::NamesIds;
pub use provenance::{Cause, Outcome, Provenance};
pub use slot::{Filled, Slot, SlotClaim, SlotError, SlotName, SlotRefused};
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
            HashEqLike, IdOfAsId, IdOfNamed, IdOfOther, IdsOf, KeyText, KeyTextDebug, KeyTextId,
            KeyTextOther, Lookup, ReturnsAsDeref, ReturnsAsRef, value_hash,
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

/// Why [`with_fuel`]'s work stopped: it brought `steps` rows up to date that were not already,
/// and was bringing `query`'s row of `key` up to date `depth` frames deep when none was left.
/// `value` is that key as its type shows it ([`function::Function::write_key`], read while the
/// database was attached), so the error names the item and not only its number (rule 16).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OutOfFuel {
    pub steps: u64,
    pub query: &'static str,
    pub key: Id,
    pub value: String,
    pub depth: usize,
}

impl fmt::Display for OutOfFuel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let OutOfFuel { steps, query, key, value, depth } = self;
        write!(
            f,
            "ran out of fuel after bringing {steps} rows up to date, at `{query}` of {value} ({key:?}), {depth} frames deep"
        )
    }
}

impl std::error::Error for OutOfFuel {}

/// Runs `f`, which may bring at most `steps` rows that are not up to date in this view up to
/// date (a row run or verified, each time it is reached), on this task: what bounds a walk
/// whose verification would otherwise not end in a useful time (rule 15). Out of fuel, the
/// row being reached unwinds, as a panic in it does (its row and the cycle it is in are left
/// empty, and a later call runs them again), and `f` gives way with [`OutOfFuel`]. Ended, it
/// gives what `f` gave and the steps it spent, what a caller records to know its margin.
/// Nested, the inner bound is at most what the outer has left, and what it spends is the
/// outer's.
///
/// Only this task is charged: a job pumped onto its thread while it waits, or one it hands to
/// the pool, runs without its bound, and a bound such a pumped job sets is its own, neither
/// taken from nor charged to the waiting task's.
pub fn with_fuel<R>(steps: u64, f: impl FnOnce() -> R) -> Result<(R, u64), OutOfFuel> {
    let outer = local::with(|local| local.fuel(steps));
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let ours = local::with(|local| local.unfuel(outer));
    let spent = ours.map_or(0, |it| it.given - it.left);
    match ran {
        Ok(value) => Ok((value, spent)),
        Err(payload) => match payload.downcast::<OutOfFuel>() {
            Ok(out) => Err(*out),
            Err(payload) => std::panic::resume_unwind(payload),
        },
    }
}

/// Unwinds the task out of fuel ([`with_fuel`]), at `F`'s row of `key`, which it names as
/// [`function::Function::write_key`] shows it with `db` attached. The bound is set aside while
/// the key is written, so a `Debug` that reads a row is not charged and can't run out again
/// here, and a panic in it leaves the key named by its number (rule 17).
#[cold]
#[inline(never)]
pub(crate) fn out_of_fuel<F: function::Function>(db: &F::Db, steps: u64, index: u32, key: F::Key) -> ! {
    let depth = local::with(|local| local.frames.len());
    let id = key.as_id();
    let fuel = local::with(|local| local.fuel.take());
    let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut value = String::new(); // alloc: an error's text, once per bound that trips
        attach(db, || F::write_key(key, &mut value)).map(|()| value)
    }));
    local::with(|local| local.fuel = fuel);
    let value = match written {
        Ok(Ok(value)) => value,
        Ok(Err(_)) | Err(_) => format!("{id:?} (its key's `Debug` failed)"), // alloc: an error's text, once per bound that trips
    };
    let out = OutOfFuel { steps, query: ingredient::name(index), key: id, value, depth };
    std::panic::resume_unwind(Box::new(out)) // alloc: an unwind's payload
}

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
    blaze_graph::sched::Carried { take: local::take, put: local::put, end: local::end }
}

/// Lends `db` to the `Debug` impls that read a database (the engine's own, through
/// [`with_attached`], and a front end's, through [`with_attached_db`]) while `f` runs: the one
/// way a database is attached. Every query's run has its own database attached; a driver
/// attaches where it formats outside one. It is part of the task's stack, so a job that waits inside `f`
/// and resumes on another thread still has it there, and the one attached before (another
/// database, or none) is put back when `f` returns or unwinds.
///
/// Text a user reads is rendered with the database in hand; attaching is for `Debug`, and for
/// an error's text made inside `f`, never a way to make a query's output depend on the thread
/// that ran it.
pub fn attach<D: Database, R>(db: &D, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<local::Attached>);

    impl Drop for Restore {
        fn drop(&mut self) {
            let before = self.0;
            local::with(|local| local.attached = before);
        }
    }

    let lent = local::Attached {
        db: std::ptr::NonNull::from(db).cast(),
        ty: std::any::TypeId::of::<D>(),
        storage: std::ptr::NonNull::from(db.storage()),
    };
    let _restore = Restore(local::with(|local| local.attached.replace(lent)));
    f()
}

/// `f` with the storage of the database [`attach`] lent this task, if any.
pub fn with_attached<R>(f: impl FnOnce(Option<&Storage>) -> R) -> R {
    let attached = local::with(|local| local.attached);
    // SAFETY: `attach` keeps the pointer only while its `&D` borrow (and so the storage) lives,
    // in this task's stack, restoring the one before when it returns or unwinds.
    f(attached.map(|it| unsafe { it.storage.as_ref() }))
}

/// `f` with the database [`attach`] lent this task, if one is attached and it is a `D`.
pub fn with_attached_db<D: Database, R>(f: impl FnOnce(Option<&D>) -> R) -> R {
    let attached = local::with(|local| local.attached);
    let db = attached.filter(|it| it.ty == std::any::TypeId::of::<D>());
    // SAFETY: as in `with_attached`; the type was `D` where `attach` erased it.
    f(db.map(|it| unsafe { it.db.cast::<D>().as_ref() }))
}

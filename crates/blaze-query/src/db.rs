//! The dense query engine: queries keyed by [`Id`], memos in place in pages by ID, a warm hit of
//! four dependent loads, durability kinds, early cutoff by `==`, single flight through
//! `blaze_graph`'s plane, fixpoint cycles, captured reads, values kept by their bytes under
//! their memory tag's budget (`budget`), and provenance. What the front end
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

mod budget;
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
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests;
mod tracked;
mod value;
mod verify;

pub use capture::{
    Read, ReadKind, Reads, capture_reads, for_span_text, replay_reads, unshareable_reads,
};
pub use counts::{Counts, take as take_counts};
pub use function::completed;
pub use id::{AsId, FromId, FromIdIn, Id};
pub use input::Setter;
pub use interned::NamesIds;
/// The most reads one query run held and the most pending rows one task held, process-wide:
/// what the query stack's runaway bounds are set against (the daemon's memory line says them).
pub use local::{HighWater as StackHighWater, high_water as stack_high_water};
pub use provenance::{
    Cause, LOG_MOST, Logged, LoggedRun, Outcome, Provenance, RowNamer, Taken, log_runs, name_rows,
    take_log_in,
};
pub use slot::{Filled, Slot, SlotClaim, SlotError, SlotName, SlotRefused};
pub use storage::{Database, Durability, Hook, Storage, Unread, View};
pub use value::Value;

/// What generated code calls: the ingredient traits, their statics' vtables, and the typed
/// entry points.
pub mod plumbing {
    pub use blaze_mem::Tag;

    pub use super::{
        function::{
            CycleKind, Function, MAX_ROUNDS, durable, evict_values, evict_values_of, fetch,
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

/// What [`Storage::memory_usage`] reports of one ingredient, or of one of the dependency
/// graph's columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemoryUsage {
    pub name: &'static str,
    /// Rows made: a function's are its pages times 1024 rows, one page per ID page of
    /// its keys that it has any key in, so a function keyed by an ID of a sparse page has most
    /// of its rows empty.
    pub rows: u64,
    /// Rows in use: a function's that ran (each holds a node), every row of another
    /// ingredient. What `rows` holds past this is pages made for keys never run.
    pub used: u64,
    /// The bytes of every row made, a value kept in place in its row included.
    pub row_bytes: u64,
    /// What values hold beside their rows: a budgeted (`lru`) function's weight, what its
    /// `weigh` counts; 0 for anything else, whose values' own heap is not counted.
    pub value_bytes: u64,
}

impl MemoryUsage {
    /// What it holds: its rows' bytes and what its values hold beside them.
    pub fn held(&self) -> u64 {
        self.row_bytes + self.value_bytes
    }
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
        /// The key whose wait was refused; `None` for a wait on what no query key names: a
        /// proc macro's session's turn (`blaze-macro-eval`'s `turn`), or a test that gives way
        /// by hand.
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
/// database was attached), so the error names the item and not only its number (rule 19).
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
/// whose verification would otherwise not end in a useful time (rule 18). Out of fuel, the
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

/// Why [`with_steps`]'s walk stopped: it had taken `steps` distinct steps, all it was given,
/// and was taking another, `at`, as the walk named it ([`step`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OutOfSteps {
    pub steps: u64,
    pub at: String,
}

impl fmt::Display for OutOfSteps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let OutOfSteps { steps, at } = self;
        write!(f, "took {steps} distinct steps, all it was given, and was taking another: {at}")
    }
}

impl std::error::Error for OutOfSteps {}

/// Runs `f`, a walk that may take at most `steps` distinct steps on this task, each a key it
/// gives [`step`]: what bounds a walk by a measure of the walk itself (rule 18), where
/// [`with_fuel`] bounds the engine's work under it. A step is taken once however often it is
/// given, whether the rows it stands for were run, verified or already up to date, so a walk
/// that gives a step for each thing it reaches takes the same steps on a cold database, a warm
/// one and a busy one: its count, and whether it ran out, depend only on what it walks.
/// Out of steps, the step being given unwinds, as a panic there does (the rows open under it
/// are left empty, and a later call runs them again), and `f` gives way with [`OutOfSteps`].
/// Ended, it gives what `f` gave and the steps it took. Nested, the inner bound is at most what
/// the outer has left, and its steps are the outer's too.
///
/// Only this task is bounded, as with [`with_fuel`]: a job pumped onto its thread while it
/// waits, or one it hands to the pool, gives its steps to no one's bound but its own.
pub fn with_steps<R>(steps: u64, f: impl FnOnce() -> R) -> Result<(R, u64), OutOfSteps> {
    let outer = local::with(|local| local.steps(steps));
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let taken = local::with(|local| local.unsteps(outer));
    match ran {
        Ok(value) => Ok((value, taken)),
        Err(payload) => match payload.downcast::<OutOfSteps>() {
            Ok(out) => Err(*out),
            Err(payload) => std::panic::resume_unwind(payload),
        },
    }
}

/// The step `key` of the walk [`with_steps`] bounds on this task; nothing outside one, where
/// `key` is never made (a walk gives a step on its hot path, bounded or not). A key already
/// taken costs nothing. With none left, unwinds with [`OutOfSteps`], `at` saying what
/// the step was: `at` runs only then, with the bound set aside, so a `Debug` that reads a row
/// or gives a step can't run out again there, and a panic in it leaves the step unnamed.
#[inline]
pub fn step(key: impl FnOnce() -> u128, at: impl FnOnce() -> String) {
    if !local::with(|local| local.stepping()) {
        return;
    }
    let key = key();
    if let Some(steps) = local::with(|local| local.step(key)) {
        out_of_steps(steps, key, at);
    }
}

#[cold]
#[inline(never)]
fn out_of_steps(steps: u64, key: u128, at: impl FnOnce() -> String) -> ! {
    let bound = local::with(|local| local.steps.take());
    let named = std::panic::catch_unwind(std::panic::AssertUnwindSafe(at));
    local::with(|local| local.steps = bound);
    // alloc: an error's text, once per bound that trips
    let at = named.unwrap_or_else(|_| format!("the step {key:#x} (its name panicked)"));
    std::panic::resume_unwind(Box::new(OutOfSteps { steps, at })) // alloc: an unwind's payload
}

/// Unwinds the task out of fuel ([`with_fuel`]), at `F`'s row of `key`, which it names as
/// [`function::Function::write_key`] shows it with `db` attached. The bound is set aside while
/// the key is written, so a `Debug` that reads a row is not charged and can't run out again
/// here, and a panic in it leaves the key named by its number (rule 20).
#[cold]
#[inline(never)]
pub(crate) fn out_of_fuel<F: function::Function>(
    db: &F::Db,
    steps: u64,
    index: u32,
    key: F::Key,
) -> ! {
    let depth = local::with(|local| local.frames.len());
    let id = key.as_id();
    let fuel = local::with(|local| local.fuel.take());
    let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut value = String::new(); // alloc: an error's text, once per bound that trips
        attach(db, || function::write_key_text(&mut value, id, |out| F::write_key(key, out)))
            .then_some(value)
    }));
    local::with(|local| local.fuel = fuel);
    let value = match written {
        Ok(Some(value)) => value,
        Ok(None) | Err(_) => format!("{id:?} (its key's `Debug` failed)"), // alloc: an error's text, once per bound that trips
    };
    let out = OutOfFuel { steps, query: ingredient::name(index), key: id, value, depth };
    std::panic::resume_unwind(Box::new(out)) // alloc: an unwind's payload
}

/// What a [`Runaway`] held past its bound.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Over {
    /// The reads one run recorded: those it holds on its thread's edge stack and the repeats
    /// dropped from it as it filled (`local::RUN_READS`).
    Reads,
    /// The rows a verify proved on its assumption and holds pending (`local::PENDING_FLOOR`
    /// past the storage's node count).
    Pending,
    /// The rounds one refresh of a row went without the row coming up to date.
    Rounds,
}

/// Why a query unwound instead of growing its task's query stack without end (rule 18): it
/// held `held` of what [`Over`] names, past the `bound` it may hold. `query` and `key` name it,
/// with `value` the key as its type shows it (read while the database was attached). It is
/// the `depth`-th of the `frames` queries running or verifying on its task's stack (outermost
/// 1), and `most` is what it held most often, most first, with how many times (each read's
/// query or input field and key, counted over its last [`Runaway::WINDOW`] entries): the loop's
/// body. The query and every query under it on the task's stack fail, their rows left empty to
/// run again, as a panic in them would (rule 20), and a host reports this text
/// ([`unwind_text`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Runaway {
    pub over: Over,
    pub query: &'static str,
    pub key: Id,
    pub value: String,
    pub depth: usize,
    pub frames: usize,
    pub held: u64,
    pub bound: u64,
    pub most: Vec<(String, u64)>,
}

impl Runaway {
    /// The entries at the top of the stack that `most` is counted over.
    pub const WINDOW: usize = 1 << 20;
    /// The most dependencies `most` names.
    pub const MOST: usize = 8;
}

impl fmt::Display for Runaway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Runaway { over, query, key, value, depth, frames, held, bound, most } = self;
        match over {
            Over::Reads => write!(
                f,
                "`{query}` of {value} ({key:?}) held {held} reads in one run, past the {bound} a \
                 run may hold on its query stack (rule 18): it keeps reading"
            )?,
            Over::Pending => write!(
                f,
                "the verify of `{query}` of {value} ({key:?}) held {held} rows proved on its \
                 assumption pending, past its bound of {bound} (the storage's rows and a margin, \
                 rule 18): rows proved again and again while it is open"
            )?,
            Over::Rounds => write!(
                f,
                "`{query}` of {value} ({key:?}) went round its refresh {held} times without coming \
                 up to date, past the bound of {bound} (rule 18)"
            )?,
        }
        write!(f, "; it is query {depth} of the {frames} on its task's query stack")?;
        for (at, (what, count)) in most.iter().enumerate() {
            f.write_str(if at == 0 { "; what it held most: " } else { ", " })?;
            write!(f, "{what} {count} times")?;
        }
        Ok(())
    }
}

impl std::error::Error for Runaway {}

/// Why a query unwound when asked for a key its database never gave: no ingredient owns the
/// key's page, so its row would sit in a page no sweep or drop can name. It fails its job
/// alone, as a panic would (rule 20), and a host reports this text ([`unwind_text`]).
///
/// It names what the next occurrence needs to find where the key came from (the first one,
/// 2026-11-09, `FunctionSignature::kept` asked for `Id(0)` once while compiling a crate, said
/// only its query): the query that asked on the task's stack, the view, how many pages the
/// database had given, the thread, and the call stack of the ask. A key of 0 is no ID:
/// [`Id::new`] refuses it, so safe code cannot hold one, and it was read as an ID out of
/// memory that was zero (a row, an interned value or a record read before its writer filled
/// it, or after its memory was let go). Any other is an ID of another database, or a word
/// decoded without [`Id::decode`] and its page's owner.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NeverGiven {
    pub query: &'static str,
    /// The key's bits.
    pub key: u32,
    pub page: usize,
    /// The bound of the pages the database had given: its IDs' pages are from 1 to below it.
    pub pages: usize,
    /// The database's view, as its rows' `verified` words hold it.
    pub view: u32,
    /// The query running or verifying on top of the task's query stack, with its key: the
    /// one whose code asked. `None` when the ask came from outside any query.
    pub asker: Option<String>,
    /// The queries running or verifying on the task's stack.
    pub frames: usize,
    pub thread: String,
    /// The call stack of the ask ([`std::backtrace::Backtrace::force_capture`]).
    pub backtrace: String,
}

impl fmt::Display for NeverGiven {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let NeverGiven { query, key, page, pages, view, asker, frames, thread, backtrace } = self;
        write!(
            f,
            "`{query}` was asked for the key Id({key}), an ID this database never gave (its page \
             {page} has no owner; every page this database gave is from 1 to below {pages}, and \
             it is at view {view}): "
        )?;
        match *key {
            0 => f.write_str(
                "0 is no ID, and `Id::new` never makes it, so it was read as an ID out of memory \
                 that was zero: a row, an interned value or a record read before its writer \
                 filled it, or after its memory was let go",
            )?,
            _ => f.write_str(
                "a key of another database, forged, or decoded without checking its page's owner",
            )?,
        }
        match asker {
            Some(asker) => write!(
                f,
                "; asked by {asker}, the top of the {frames} queries on its task's query stack"
            )?,
            None => f.write_str("; asked from outside any query")?,
        }
        write!(f, ", on thread `{thread}`; the call stack of the ask:\n{backtrace}")
    }
}

impl std::error::Error for NeverGiven {}

/// Unwinds with a [`NeverGiven`]: ingredient `index` was asked for the key `id`, whose page no
/// ingredient owns. The asking query's key is written with a barrier on top of the task's
/// stack, as [`unwind_runaway`]'s names are.
#[cold]
#[inline(never)]
pub(crate) fn never_given(storage: &Storage, index: u32, id: Id) -> ! {
    let (asker, frames) = local::with(|local| (local.top_query(), local.task_frames()));
    let barrier = local::barrier();
    let asker = asker.map(|(ingredient, bits)| {
        let name = ingredient::name(ingredient);
        match Id::new(bits) {
            Some(key) => format!("`{name}` of {}", key_text(storage, ingredient, key)), // alloc: an error's text, once
            None => format!("`{name}` of the key {bits}, which is no ID either"), // alloc: as above
        }
    });
    drop(barrier);
    let thread = std::thread::current();
    let out = NeverGiven {
        query: ingredient::name(index),
        key: id.as_u32(),
        page: id.page(),
        pages: storage.pages_made(),
        view: storage.view_bits(),
        asker,
        frames,
        thread: thread.name().unwrap_or("(unnamed)").to_owned(), // alloc: an error's text, once
        backtrace: std::backtrace::Backtrace::force_capture().to_string(), // alloc: as above
    };
    tracing::error!(target: "blaze", never_given = %out, "a query was asked for a key its database never gave and fails");
    std::panic::resume_unwind(Box::new(out)) // alloc: an unwind's payload
}

/// The text of an unwind the engine raised ([`Runaway`], [`NeverGiven`], [`OutOfFuel`],
/// [`OutOfSteps`], [`Cancelled`]), for a host reporting a job's panic; `None` for any other
/// payload (a `panic!`'s text is `stdx::panic_text`'s).
// dyn: a panic's payload, read once per panicked job
pub fn unwind_text(payload: &(dyn std::any::Any + Send)) -> Option<String> {
    // alloc: an error's text, once per panicked job
    if let Some(it) = payload.downcast_ref::<Runaway>() {
        Some(it.to_string())
    } else if let Some(it) = payload.downcast_ref::<NeverGiven>() {
        Some(it.to_string())
    } else if let Some(it) = payload.downcast_ref::<OutOfFuel>() {
        Some(it.to_string())
    } else if let Some(it) = payload.downcast_ref::<OutOfSteps>() {
        Some(it.to_string())
    } else {
        payload.downcast_ref::<Cancelled>().map(Cancelled::to_string)
    }
}

/// What a task past a bound held, read off its stack: the frame of the query that fails, and
/// the node keys it held most.
struct Stuck {
    over: Over,
    ingredient: u32,
    id: Id,
    depth: usize,
    frames: usize,
    held: u64,
    bound: u64,
    most: Vec<(u64, u64)>,
}

/// The `Runaway::MOST` nodes `nodes` holds most often, most first (ties by node, so the order
/// is the same on every run), as their keys, with their counts.
fn most_held(storage: &Storage, nodes: impl Iterator<Item = u32>) -> Vec<(u64, u64)> {
    // alloc: an error's counts, once per bound that trips
    let mut counts =
        hashbrown::HashMap::<u32, u64, blaze_hash::FixedState>::with_hasher(blaze_hash::FixedState);
    for node in nodes {
        *counts.entry(node).or_insert(0) += 1;
    }
    let mut most: Vec<(u32, u64)> = counts.into_iter().collect(); // alloc: as above
    most.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    most.truncate(Runaway::MOST);
    // alloc: as above
    most.into_iter().map(|(node, count)| (storage.key_of(node), count)).collect()
}

/// Does what a read or a replay left its caller ([`local::Note`]), once the caller has let go
/// of the stack: fails a run past its bound, names a run that set the high-water mark.
#[inline(always)]
pub(crate) fn noted(storage: &Storage, note: local::Note) {
    match note {
        local::Note::Fine => {}
        local::Note::Most(held) => name_most_reads(storage, held),
        local::Note::Runaway => runaway_reads(storage),
    }
}

/// Names the run on top of the task's stack, which set the most reads one run held, `held`
/// (`Local::full` said so), as `` `query` of key `` in the memory report
/// ([`local::high_water`]). The key is written as [`unwind_runaway`] writes one, under a
/// barrier, so what its `Debug` reads is recorded into no run; only as a new mark is set.
#[cold]
#[inline(never)]
fn name_most_reads(storage: &Storage, held: u64) {
    let top =
        local::with(|local| local.frames.last().map(|it| (it.ingredient, Id::from_u32(it.id))));
    let Some((ingredient, id)) = top else { return };
    local::name_most_reads(held, || {
        let barrier = local::barrier();
        let value = key_text(storage, ingredient, id);
        drop(barrier);
        format!("`{}` of {value}", ingredient::name(ingredient)) // alloc: a name, once a new mark
    });
}

/// Fails the run on top of the task's stack, which recorded more reads than it may
/// (`Local::read`, `Local::replay`, which said so): unwinds with [`Runaway`] naming it and
/// what it holds most. Its held reads have had their repeats dropped as the stack filled, so
/// `held` counts those dropped too.
#[cold]
#[inline(never)]
pub(crate) fn runaway_reads(storage: &Storage) -> ! {
    let stuck = local::with(|local| {
        let at = local.frames.len() - 1;
        let frame = &local.frames[at];
        let start = frame.edges_start as usize;
        let (depth, frames) = local.task_depth(at);
        let reads = &local.edges[start..];
        let window = &reads[reads.len().saturating_sub(Runaway::WINDOW)..];
        Stuck {
            over: Over::Reads,
            ingredient: frame.ingredient,
            id: Id::from_u32(frame.id),
            depth,
            frames,
            held: reads.len() as u64 + frame.dropped,
            bound: local.bounds.run_reads,
            most: most_held(storage, window.iter().copied()),
        }
    });
    unwind_runaway(storage, stuck)
}

/// Fails the verify at stack index `head`, whose pending list is past its bound for a storage
/// of `rows` nodes (`Local::push_pending` said so): unwinds with [`Runaway`] naming it and the
/// rows pending most often.
#[cold]
#[inline(never)]
pub(crate) fn runaway_pending(storage: &Storage, head: u32, rows: u64) -> ! {
    let stuck = local::with(|local| {
        let frame = &local.frames[head as usize];
        let (depth, frames) = local.task_depth(head as usize);
        let pending = &local.pending[local.pending.len().saturating_sub(Runaway::WINDOW)..];
        Stuck {
            over: Over::Pending,
            ingredient: frame.ingredient,
            id: Id::from_u32(frame.id),
            depth,
            frames,
            held: local.pending.len() as u64,
            bound: local.pending_bound(rows),
            most: most_held(storage, pending.iter().map(|it| it.node)),
        }
    });
    unwind_runaway(storage, stuck)
}

/// Fails the refresh of row `id` of `index`, which went round `rounds` times, past `bound`,
/// without coming up to date: unwinds with [`Runaway`] naming it.
#[cold]
#[inline(never)]
pub(crate) fn runaway_rounds(storage: &Storage, index: u32, id: Id, rounds: u64, bound: u64) -> ! {
    let frames = local::with(|local| local.task_frames()) + 1;
    let stuck = Stuck {
        over: Over::Rounds,
        ingredient: index,
        id,
        depth: frames,
        frames,
        held: rounds,
        bound,
        most: Vec::new(), // alloc: none, empty
    };
    unwind_runaway(storage, stuck)
}

/// Names what `stuck` read off the stack and unwinds with it. The names are written with a
/// barrier on top of the task's stack: what a key's `Debug` reads is recorded into no run (the
/// failing run's stack is at its bound), meets no frame below it as a cycle, and is charged to
/// no fuel or step bound; a `Debug` that panics or gives way leaves its key named by its ID.
#[cold]
#[inline(never)]
fn unwind_runaway(storage: &Storage, stuck: Stuck) -> ! {
    let Stuck { over, ingredient, id, depth, frames, held, bound, most } = stuck;
    let barrier = local::barrier();
    let value = key_text(storage, ingredient, id);
    // alloc: an error's names, once per bound that trips
    let most = most.into_iter().map(|(key, count)| (node_text(storage, key), count)).collect();
    drop(barrier);
    let query = ingredient::name(ingredient);
    let out = Runaway { over, query, key: id, value, depth, frames, held, bound, most };
    tracing::error!(target: "blaze", runaway = %out, "a query held past its bound and fails");
    std::panic::resume_unwind(Box::new(out)) // alloc: an unwind's payload
}

/// Row `id` of ingredient `index`'s key as its type shows it ([`ingredient::Vtable`]'s
/// `write_key`), or its ID.
pub(crate) fn key_text(storage: &Storage, index: u32, id: Id) -> String {
    let write = ingredient::vtable(index).and_then(|it| it.write_key);
    let written = write.map(|write| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out = String::new(); // alloc: an error's text, once per bound that trips
            write(storage, id, &mut out);
            out
        }))
    });
    match written {
        Some(Ok(text)) => text,
        Some(Err(_)) => format!("{id:?} (its key's `Debug` failed)"), // alloc: as above
        None => format!("{id:?}"),                                    // alloc: as above
    }
}

/// The dependency a node key names: a query's row (`` `query`(key) ``) or an input's or
/// tracked struct's field (`` `Input.field`(Id) ``).
fn node_text(storage: &Storage, key: u64) -> String {
    let (index, id, field) = storage::key_parts(key);
    let name = ingredient::name(index);
    match ingredient::vtable(index).map(|it| (it.kind, it.fields)) {
        Some((ingredient::Kind::Input | ingredient::Kind::Tracked, fields)) => {
            let field = fields.get(field as usize).copied().unwrap_or("?");
            format!("`{name}.{field}`({id:?})") // alloc: an error's text, once per bound that trips
        }
        _ => format!("`{name}`({})", key_text(storage, index, id)), // alloc: as above
    }
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

/// A read the engine can't see, named: what was read and why no tracked read stands for it.
/// The only way a value comes to run again in every view ([`report_untracked_read`]). It has no
/// default and no "unknown" form: a caller that can't say what it read has no value to make
/// untracked, and one that can names it here, so the trace of a row that reruns every view
/// says why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Untracked {
    /// What was read and why it can't be a tracked read (`"the absence of the
    /// `SourceNames` singleton"`).
    pub what: &'static str,
}

/// The running query made the read `why` names, which the engine can't see: it runs again in
/// every view, at durability `LOW`, and its captures give `None`. The read is told to the trace
/// (`blaze_query` target), naming the query, and the run's first such read to the run log
/// ([`log_runs`]), so the profile's `provenance` table names what makes a row rerun.
pub fn report_untracked_read<D: Database>(db: &D, why: Untracked) {
    let storage = db.storage();
    let query = local::with(|local| {
        let frame = local.recording(storage)?;
        let first = !frame.untracked;
        frame.untracked = true;
        frame.unshareable = true;
        frame.changed = frame.changed.max(storage.view_bits());
        Some((frame.ingredient, frame.node, first))
    });
    if let Some((index, node, first)) = query {
        if first && provenance::logging() {
            storage.run_log.log(
                storage.key_of(node),
                0,
                storage.view_bits(),
                Logged::UntrackedRead(why.what),
            );
        }
        tracing::debug!(
            target: "blaze_query",
            query = ingredient::name(index),
            untracked = why.what,
            "an untracked read: it runs again in every view"
        );
    }
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

/// `task(at)` for each `at` in `0..count`, each run in a job on the pool this thread's loops
/// run on (at most [`blaze_graph::par::WINDOW`] per worker at once), inside a [`pump_barrier`]. A task that gave way
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
    // The caller's attached database (or none), lent to each task too: a task formats what
    // its caller would, on whichever worker it runs, not whatever that worker had.
    let lent = Lent(local::with(|local| local.attached));
    let task = |at: usize| lent.within(|| task(at));
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
#[expect(clippy::disallowed_types, reason = "a result's slot, each locked once by the task that fills it")]
pub fn par_map<R: Send>(count: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let made: Vec<blaze_core::lock::Lock<Option<R>>> =
        // alloc: a slot per result, once per loop
        (0..count).map(|_| blaze_core::lock::Lock::new("query.par_map.slot", None)).collect();
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
/// attaches where it formats outside one, and a loop's tasks ([`par_for_each`], [`par_map`])
/// have their caller's. It is part of the task's stack, so a job that waits inside `f`
/// and resumes on another thread still has it there, and the one attached before (another
/// database, or none) is put back when `f` returns or unwinds.
///
/// Text a user reads is rendered with the database in hand; attaching is for `Debug`, and for
/// an error's text made inside `f`, never a way to make a query's output depend on the thread
/// that ran it.
pub fn attach<D: Database, R>(db: &D, f: impl FnOnce() -> R) -> R {
    let lent = local::Attached {
        db: std::ptr::NonNull::from(db).cast(),
        ty: std::any::TypeId::of::<D>(),
        storage: std::ptr::NonNull::from(db.storage()),
    };
    Lent(Some(lent)).within(f)
}

/// A database [`attach`] lent (or none), to lend again: the one place a task's attached
/// database is set and put back.
#[derive(Clone, Copy)]
struct Lent(Option<local::Attached>);

// SAFETY: a `Lent` is made from a borrow (`attach`'s `&D`, or the caller's own attached one in
// `par_for_each`) that outlives every task it is lent to, since `par_for_each` returns only
// once each has ended; and a database is shared by the pool's workers as it is, so reading it
// from another thread is what its queries already do.
unsafe impl Send for Lent {}
// SAFETY: as above.
unsafe impl Sync for Lent {}

impl Lent {
    /// `f` with this attached (or none), the one attached before put back when `f` returns or
    /// unwinds.
    fn within<R>(self, f: impl FnOnce() -> R) -> R {
        struct Restore(Option<local::Attached>);

        impl Drop for Restore {
            fn drop(&mut self) {
                let before = self.0;
                local::with(|local| local.attached = before);
            }
        }

        let _restore = Restore(local::with(|local| std::mem::replace(&mut local.attached, self.0)));
        f()
    }
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

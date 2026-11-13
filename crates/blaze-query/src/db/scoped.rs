//! Task-scoped functions (`#[blaze_query::tracked(scope = task)]`): a memo that lives for one
//! task and keeps nothing in the [`Storage`] (docs/v2/plan/design/end-state/memory.md: "a value
//! read once is frame scratch, not a record and not a memo").
//!
//! A scoped function's values are a table owned by the outermost [`task_scope`] open on the
//! task, allocated in that scope's `blaze_mem` frame, shared by every call inside it, and
//! dropped whole when it ends. It has no row, no node, no edges and no reverse edges, and its
//! key is its arguments themselves (nothing is interned for it).
//!
//! **Its reads are its caller's.** A scoped run pushes no frame: every read it makes lands in
//! the running persistent query's frame (the one that, transitively, called it), as if that
//! query had made it. While it runs, that frame's dedup floor and what it folds (durability,
//! `changed`, the replayed summaries, whether it may be shared) start afresh, as a
//! [`super::capture_reads`] frame does, so the run's reads are the stack above where it began,
//! each once: the scope keeps them with the value ([`Kept`]), and a later call inside the
//! scope, from whatever persistent query, pushes them into that query's frame
//! ([`replay`]). So a persistent caller's verification covers every input of every scoped value
//! it used (rule 21), and nothing outlives the scope. A run made with no persistent query on the
//! stack has no frame to read into and keeps no reads; a persistent query that meets that value
//! later runs it again under its own frame.
//!
//! A value read by its run that may not be shared (it read a provisional row of a persistent
//! cycle, made a tracked struct, made an untracked read: the frame's `unshareable`) is handed
//! out and not kept.
//!
//! **Cycles.** The scope keeps a stack of its running runs. A call of a key on that stack is a
//! cycle: a function with `cycle_initial` iterates from that value at the run it reached (its
//! head) until a round's value equals the last ([`Scoped::equal`]), at most [`MAX_ROUNDS`]; one
//! without panics naming the key. A run that read a value of a round not yet final (the head's,
//! or one made from it) keeps its value only for that round of that head. A cycle that passes
//! through a persistent query (a scoped run, a persistent run above it, and the scoped key
//! again) is refused by name: the persistent row would be kept with a value made from a round
//! that is not final, and the persistent engine cannot iterate a head it does not see.
//!
//! **Where a scope is.** In the task's query stack ([`local::Local::scope`]), so it moves with
//! the task across fibers. A job pumped onto the thread while the task waits (above an unlifted
//! barrier) is another task and does not see it, and the tasks of a [`super::par_for_each`]
//! run on other workers with none: each opens its own where it calls a scoped function. Calls
//! inside one scope are one task's, one at a time, so the tables are cells, never locks.

use std::{
    any::TypeId,
    cell::{Cell, RefCell},
    hash::Hash,
    mem::MaybeUninit,
    ptr::{self, NonNull},
};

use blaze_hash::FixedState;
use blaze_mem::{Tag, frame::Frame};
use blaze_report::say::{Code, Words};

use super::{
    Database, Storage,
    function::{CycleKind, MAX_ROUNDS},
    local::{self, Local, NONE},
    storage::{Summary, or},
};

/// One task-scoped function: what `#[blaze_query::tracked(scope = task)]` generates.
pub trait Scoped: 'static {
    type Db: Database;
    /// Its arguments, as they are stored: the table's key, never interned.
    type Key: Hash + Eq + Clone + 'static;
    type Value: 'static;
    const NAME: &'static str;
    /// [`CycleKind::Panic`] or [`CycleKind::Fixpoint`]: the macro refuses `cycle_result` and
    /// `cycle_fn` on a scoped function.
    const CYCLE: CycleKind;
    const TAG: Tag;
    fn execute(db: &Self::Db, key: &Self::Key) -> Self::Value;
    /// The value a cycle through `key` starts from.
    fn cycle_initial(db: &Self::Db, key: &Self::Key) -> Self::Value;
    fn equal(old: &Self::Value, new: &Self::Value) -> bool;
    /// Writes `key` as an error about it names it (rule 19).
    fn write_key(key: &Self::Key, out: &mut dyn std::fmt::Write) -> std::fmt::Result;
    /// Told each value that becomes final in a scope, once, after its entry is written:
    /// never a round's value of a cycle not yet final, nor one its run may not share (it is
    /// handed out and not kept). What publishes a value past its scope (a table shared by
    /// tasks) hooks in here (`settled = <path>` on the macro).
    fn settled(_db: &Self::Db, _key: &Self::Key, _value: &Self::Value) {}
}

/// The open scope as the task's stack keeps it: its lifetime erased, bounded by the
/// [`task_scope`] call that made it, which takes it off the stack before it ends.
pub(crate) type ScopePtr = NonNull<Scope<'static>>;

/// One [`task_scope`]: its tables, its running runs, and what it was opened in.
pub(crate) struct Scope<'f> {
    storage: *const Storage,
    /// The unlifted barriers on the task's stack when it opened: a job above more of them is
    /// another task's ([`open_scope`]).
    walls: u32,
    /// The storage's view when it opened. A scope holds a `&Db`, so no write can come while it
    /// is open; a call that sees another view is refused, never served stale.
    view: u32,
    /// The scope open before this one on the task (of another storage, or below a barrier).
    outer: Option<ScopePtr>,
    frame: &'f Frame,
    tables: RefCell<hashbrown::HashMap<TypeId, Erased, FixedState, &'f Frame>>,
    runs: RefCell<Vec<Run, &'f Frame>>,
    serial: Cell<u64>,
}

/// A scoped function's table with its type erased, and how it is dropped.
struct Erased {
    table: NonNull<()>,
    drop: unsafe fn(NonNull<()>),
}

/// A scoped function's table: its entries by key, and every value made in the scope (kept
/// until the scope ends, as a reference handed out may still be read: [`fetch_scoped`]'s `hand`
/// runs after a later run may replace the entry).
struct Table<'f, F: Scoped> {
    entries: RefCell<hashbrown::HashMap<F::Key, Entry<F::Value>, FixedState, &'f Frame>>,
    values: RefCell<Vec<NonNull<F::Value>, &'f Frame>>,
}

/// A key of a scoped function, in its scope.
enum Entry<V> {
    /// Running on the scope's stack, at this index of [`Scope::runs`].
    Running(u32),
    /// Made in round `round` of the cycle headed at run `head` (whose serial is `serial`):
    /// its value holds only while that head is in that round.
    Provisional { value: NonNull<V>, head: u32, serial: u64, round: u32 },
    /// Final in this scope, with the reads its run made (`None`: it ran with no persistent
    /// query on the stack, so none were kept).
    Final { value: NonNull<V>, kept: Option<Kept> },
}

impl<V> Clone for Entry<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V> Copy for Entry<V> {}

/// What a scoped run read: the distinct nodes, in read order, in the scope's frame, and what
/// they folded into the frame that recorded them.
#[derive(Clone, Copy)]
struct Kept {
    nodes: NonNull<[u32]>,
    durability: u8,
    changed: u32,
    summary: Summary,
    /// The recording frame's [`local::Frame::skip_at`]: reads at it or above were not
    /// pushed (a witness or covered frame's), so `nodes` holds them only for a frame that
    /// skips them too ([`servable`]).
    skip_at: u8,
}

/// Whether a final entry's value may be handed to a caller whose recording frame skips reads
/// from `top` (`None`: no frame records): one with none recording needs no reads; one with a
/// frame needs the value's reads kept, of every durability its frame records.
fn servable(kept: Option<Kept>, top: Option<u8>) -> bool {
    match (kept, top) {
        (_, None) => true,
        (Some(kept), Some(top)) => top <= kept.skip_at,
        (None, Some(_)) => false,
    }
}

/// A scoped run on the scope's stack.
#[derive(Clone, Copy)]
struct Run {
    /// The task's frame count when it began: a reentry that sees more went through a
    /// persistent query.
    frames: u32,
    /// The lowest index of a run on the scope's stack whose round-value it read, [`NONE`] for
    /// none.
    head: u32,
    /// Its own key was asked for while it ran: it heads a cycle.
    reentered: bool,
    /// Its round, from 1, while it heads a cycle.
    round: u32,
    /// A number no other run of the scope had.
    serial: u64,
    /// Its value for the round, while it heads a cycle: a `NonNull<F::Value>` in its table.
    provisional: Option<NonNull<()>>,
}

impl<'f> Scope<'f> {
    fn next_serial(&self) -> u64 {
        let serial = self.serial.get() + 1;
        self.serial.set(serial);
        serial
    }

    /// The table of `F`, made on first use.
    fn table<F: Scoped>(&self) -> &Table<'f, F> {
        let of = TypeId::of::<F>();
        if let Some(found) = self.tables.borrow().get(&of) {
            // SAFETY: the table under `TypeId::of::<F>()` was made below as a `Table<F>`, in the
            // scope's frame, and lives until the scope drops.
            return unsafe { found.table.cast::<Table<'f, F>>().as_ref() };
        }
        let table = self.frame.alloc(Table::<F> {
            entries: RefCell::new(hashbrown::HashMap::with_hasher_in(FixedState, self.frame)),
            values: RefCell::new(Vec::new_in(self.frame)),
        });
        let table = NonNull::from(table);
        self.tables
            .borrow_mut()
            .insert(of, Erased { table: table.cast(), drop: drop_table::<F> });
        // SAFETY: as above.
        unsafe { table.as_ref() }
    }

    /// Whether run `head` is on the stack with `serial`, in round `round`.
    fn live(&self, head: u32, serial: u64, round: u32) -> bool {
        self.runs
            .borrow()
            .get(head as usize)
            .is_some_and(|run| run.serial == serial && run.round == round)
    }

    /// The run on top read a value of the round of the run at `head`.
    fn depend_on(&self, head: u32) {
        let mut runs = self.runs.borrow_mut();
        let top = runs.last_mut().expect("a round's value is read only inside a run of the scope");
        top.head = top.head.min(head);
    }

    fn check_view<F: Scoped>(&self, storage: &Storage, key: &F::Key) {
        if self.view != storage.view_bits() {
            // alloc: the panic's text
            refuse(
                blaze_report::say!(
                    "task-scoped ",
                    Code(F::NAME),
                    Words(&format!("({}) was asked for in view {} of a scope opened in view {}", key_text::<F>(key), storage.view_bits(), self.view)), // alloc: as above
                    ": a scope's values are of its view",
                )
                .flat(),
            );
        }
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        for (_, table) in self.tables.get_mut().drain() {
            // SAFETY: each table is dropped once, here, as the scope ends: nothing of the scope
            // is read after (`task_scope` took it off the task's stack first).
            unsafe { (table.drop)(table.table) };
        }
    }
}

/// [`Erased::drop`] of `F`: every value made in the scope, then the table.
unsafe fn drop_table<F: Scoped>(table: NonNull<()>) {
    let table = table.cast::<Table<'_, F>>();
    // SAFETY: a `Table<F>` placed in the scope's frame (`Scope::table`), dropped once; each
    // value was placed in the frame by `keep` and is dropped once, here. The frame runs no
    // destructor of what it holds, so these are the only drops.
    unsafe {
        for value in table.as_ref().values.borrow().iter() {
            ptr::drop_in_place(value.as_ptr());
        }
        ptr::drop_in_place(table.as_ptr());
    }
}

impl<F: Scoped> Table<'_, F> {
    /// Places `value` in the scope's frame, to be dropped with the scope.
    fn keep(&self, frame: &Frame, value: F::Value) -> NonNull<F::Value> {
        let value = NonNull::from(frame.alloc(value));
        self.values.borrow_mut().push(value);
        value
    }
}

/// Runs `f` inside a task scope of `db`'s storage: every task-scoped function called while it
/// runs keeps its values in the scope's table, dropped when `f` returns or unwinds. A scope
/// already open on this task for this storage is used as it is (the outermost owns the table).
///
/// Open it at a task's entry, where no persistent query is on the stack (a walk's driver); a
/// persistent query that calls a scoped function is called inside one.
pub fn task_scope<D: Database, R>(db: &D, f: impl FnOnce() -> R) -> R {
    let storage = db.storage();
    if local::with(|local| open_scope(local, storage).is_some()) {
        return f();
    }
    blaze_mem::frame::task(Tag::Query, |frame| {
        /// Takes the scope off the task's stack as `task_scope` ends, before the scope drops.
        struct Close(Option<ScopePtr>);

        impl Drop for Close {
            fn drop(&mut self) {
                let outer = self.0;
                local::with(|local| local.scope = outer);
            }
        }

        let (walls, outer) = local::with(|local| (local.walls(), local.scope));
        let scope = Scope {
            storage: ptr::from_ref(storage),
            walls,
            view: storage.view_bits(),
            outer,
            frame,
            tables: RefCell::new(hashbrown::HashMap::with_hasher_in(FixedState, frame)),
            runs: RefCell::new(Vec::new_in(frame)),
            serial: Cell::new(0),
        };
        let at = NonNull::from(&scope).cast::<Scope<'static>>();
        local::with(|local| local.scope = Some(at));
        // Dropped before `scope`, declared after it.
        let _close = Close(outer);
        f()
    })
}

/// The scope of `storage` open on this task, if any: one opened above as many barriers as
/// the stack has now. A scope below a barrier this one is above is another task's (this job
/// was pumped onto its thread while it waits).
fn open_scope(local: &Local, storage: *const Storage) -> Option<ScopePtr> {
    let walls = local.walls();
    let mut at = local.scope;
    while let Some(scope) = at {
        // SAFETY: a scope on the stack is alive: `task_scope` takes it off before it drops.
        let scope_ref = unsafe { scope.as_ref() };
        if scope_ref.walls != walls {
            return None;
        }
        if scope_ref.storage == storage {
            return Some(scope);
        }
        at = scope_ref.outer;
    }
    None
}

fn key_text<F: Scoped>(key: &F::Key) -> String {
    let mut text = String::new(); // alloc: an error's text
    if F::write_key(key, &mut text).is_err() {
        text.push_str("<a key whose text failed>");
    }
    text
}

#[cold]
#[inline(never)]
fn refuse(text: String) -> ! {
    panic!("{text}")
}

/// The value of the task-scoped function `F` at `key`, handed to `hand` (the generated
/// function's copy or clone of it): from the scope's table, or run.
///
/// # Panics
///
/// With no task scope of `db`'s storage open on this task, naming the function and the key: a
/// scoped function keeps no row to fall back on. Also on a cycle `F` cannot serve (no
/// `cycle_initial`, or one through a persistent query), a fixpoint past [`MAX_ROUNDS`], and as
/// its run does.
pub fn fetch_scoped<F: Scoped, R>(
    db: &F::Db,
    key: F::Key,
    hand: impl FnOnce(&F::Value) -> R,
) -> R {
    let storage = db.storage();
    let Some(scope) = local::with(|local| open_scope(local, storage)) else {
        // alloc: the panic's text
        refuse(
            blaze_report::say!(
                "task-scoped ",
                Code(F::NAME),
                Words(&format!("({})", key_text::<F>(&key))), // alloc: as above
                " was asked for with no task scope open on this task: open one with ",
                Code("blaze_query::db::task_scope"),
                " at the task's entry (a scoped function keeps no row to fall back on)",
            )
            .flat(),
        );
    };
    // SAFETY: the scope is open on this task, so its `task_scope` call is below this one on
    // the task's stack and the scope lives until after this returns.
    let scope = unsafe { scope.as_ref() };
    scope.check_view::<F>(storage, &key);
    let table = scope.table::<F>();
    let recording = local::with(|local| local.recording(storage).map(|it| it.skip_at));
    let found = table.entries.borrow().get(&key).copied();
    match found {
        Some(Entry::Final { value, kept }) if servable(kept, recording) => {
            if recording.is_some() && let Some(kept) = kept {
                replay(storage, kept);
            }
            // SAFETY: a value of the table, alive until the scope drops.
            return hand(unsafe { value.as_ref() });
        }
        Some(Entry::Provisional { value, head, serial, round })
            if scope.live(head, serial, round) =>
        {
            scope.depend_on(head);
            // SAFETY: as above.
            return hand(unsafe { value.as_ref() });
        }
        Some(Entry::Running(at)) => return reenter::<F, R>(db, storage, scope, table, at, &key, hand),
        // Made with no query to read into (or under a frame that skipped reads this caller's
        // records), a round gone, or never: run.
        _ => {}
    }
    execute::<F, R>(db, storage, scope, table, key, hand)
}

/// A call of a key running on the scope's stack, at `at`: a cycle.
fn reenter<F: Scoped, R>(
    db: &F::Db,
    storage: &Storage,
    scope: &Scope<'_>,
    table: &Table<'_, F>,
    at: u32,
    key: &F::Key,
    hand: impl FnOnce(&F::Value) -> R,
) -> R {
    if F::CYCLE != CycleKind::Fixpoint {
        // alloc: the panic's text
        refuse(
            blaze_report::say!(
                "task-scoped ",
                Code(F::NAME),
                Words(&format!("({})", key_text::<F>(key))), // alloc: as above
                " was asked for while it runs, and has no ",
                Code("cycle_initial"),
            )
            .flat(),
        );
    }
    let run = scope.runs.borrow()[at as usize];
    let crossed = local::with(|local| {
        (local.frames.len() as u32 != run.frames)
            .then(|| local.frames.get(run.frames as usize).map(|it| (it.ingredient, it.id)))
    });
    if let Some(through) = crossed {
        let through = match through {
            Some((ingredient, id)) => {
                super::key_text(storage, ingredient, super::Id::from_u32(id))
            }
            None => "<a frame gone>".to_owned(), // alloc: the panic's text
        };
        // alloc: the panic's text
        refuse(
            blaze_report::say!(
                "a cycle through task-scoped ",
                Code(F::NAME),
                Words(&format!("({}) goes through the persistent query {through}", key_text::<F>(key))), // alloc: as above
                ": a scoped fixpoint cannot hold a persistent row to its rounds",
            )
            .flat(),
        );
    }
    let value = match run.provisional {
        Some(value) => value.cast::<F::Value>(),
        None => {
            let initial = blaze_mem::frame::task(F::TAG, |_| F::cycle_initial(db, key));
            let value = table.keep(scope.frame, initial);
            scope.runs.borrow_mut()[at as usize].provisional = Some(value.cast());
            value
        }
    };
    scope.runs.borrow_mut()[at as usize].reentered = true;
    scope.depend_on(at);
    // SAFETY: a value of the table, alive until the scope drops.
    hand(unsafe { value.as_ref() })
}

/// What the recording frame was when a scoped run began, put back when it ends.
#[derive(Clone, Copy)]
struct Opened {
    frame: u32,
    start: u32,
    dedup_floor: u32,
    durability: u8,
    changed: u32,
    summary: Summary,
    unshareable: bool,
    /// The recording frame's [`local::Frame::skip_at`].
    skip_at: u8,
}

/// Starts the run's reads afresh on the recording frame, if there is one.
fn open_reads(storage: &Storage) -> Option<Opened> {
    local::with(|local| {
        let start = local.edges.len() as u32;
        let at = local.frames.len().checked_sub(1)? as u32;
        let frame = local.recording(storage)?;
        let opened = Opened {
            frame: at,
            start,
            dedup_floor: frame.dedup_floor,
            durability: frame.durability,
            changed: frame.changed,
            summary: frame.summary,
            unshareable: frame.unshareable,
            skip_at: frame.skip_at,
        };
        frame.dedup_floor = start;
        frame.durability = u8::MAX;
        frame.changed = 0;
        frame.summary = [0; 2];
        frame.unshareable = false;
        Some(opened)
    })
}

/// What a run's reads came to.
enum Closed {
    /// No persistent query was on the stack: none were recorded.
    NoQuery,
    /// The run read what may not be shared: its value is handed out, not kept.
    Unshareable,
    Kept(Kept),
}

/// Folds the run's reads back into the recording frame and copies them into the scope.
fn close_reads(storage: &Storage, frame: &Frame, opened: Option<Opened>) -> Closed {
    let Some(opened) = opened else { return Closed::NoQuery };
    let start = opened.start as usize;
    let taken = local::with(|local| {
        let before = local.edges.len();
        let at = opened.frame as usize;
        assert_eq!(local.frames.len(), at + 1, "a scoped run ended above its recording frame");
        let top = &mut local.frames[at];
        let ran = (top.durability, top.changed, top.summary, top.unshareable);
        fold(top, opened, ran.3);
        if ran.3 {
            return None;
        }
        local.dedup_from(start);
        local.frames[at].dropped += (before - local.edges.len()) as u64;
        Some((local.edges.len() - start, ran.0, ran.1, ran.2))
    });
    let Some((len, durability, changed, summary)) = taken else { return Closed::Unshareable };
    // Placed outside the stack's lend: a frame's allocation may wait under the process's cap.
    let nodes = frame.alloc_uninit_slice::<u32>(len);
    local::with(|local| {
        for (to, from) in nodes.iter_mut().zip(&local.edges[start..start + len]) {
            to.write(*from);
        }
        // The planted bug: the run's reads are not its caller's.
        #[cfg(test)]
        if probe::DROP_FORWARDED_READS.with(Cell::get) {
            local.truncate_edges(start);
        }
    });
    // SAFETY: every element was written just above.
    let nodes = unsafe { &mut *(ptr::from_mut::<[MaybeUninit<u32>]>(nodes) as *mut [u32]) };
    Closed::Kept(Kept {
        nodes: NonNull::from(nodes),
        durability,
        changed,
        summary,
        skip_at: opened.skip_at,
    })
}

/// Puts back on `top` what [`open_reads`] set aside, folding in what the run read.
fn fold(top: &mut local::Frame, opened: Opened, unshareable: bool) {
    top.durability = top.durability.min(opened.durability);
    top.changed = top.changed.max(opened.changed);
    top.summary = or(top.summary, opened.summary);
    top.unshareable = unshareable || opened.unshareable;
    top.dedup_floor = opened.dedup_floor;
}

/// Makes the running query depend on what a scoped value's run read, as if it had read it.
fn replay(storage: &Storage, kept: Kept) {
    // SAFETY: the nodes are in the scope's frame, alive until the scope drops; this runs inside it.
    let nodes = unsafe { kept.nodes.as_ref() };
    // The planted bug: a hit gives its caller none of the value's reads.
    #[cfg(test)]
    if probe::DROP_FORWARDED_READS.with(Cell::get) {
        return;
    }
    let note = local::with(|local| {
        let Some(top) = local.recording(storage) else { return local::Note::Fine };
        top.durability = top.durability.min(kept.durability);
        top.changed = top.changed.max(kept.changed);
        top.summary = or(top.summary, kept.summary);
        local.push_reads(storage, nodes)
    });
    super::noted(storage, note);
}

/// Cleans up a run that unwinds (a give-way, a cancel, a panic of its body): its entry and the
/// runs above it go, and the recording frame gets back what it set aside, marked unshareable
/// (its open captures saw only part of the run).
struct Unwind<'a, 's, 't, F: Scoped> {
    scope: &'a Scope<'s>,
    table: &'a Table<'t, F>,
    key: &'a F::Key,
    at: u32,
    opened: Option<Opened>,
    armed: bool,
}

impl<F: Scoped> Drop for Unwind<'_, '_, '_, F> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut entries) = self.table.entries.try_borrow_mut() {
            entries.remove(self.key);
        }
        if let Ok(mut runs) = self.scope.runs.try_borrow_mut() {
            runs.truncate(self.at as usize);
        }
        if let Some(opened) = self.opened {
            local::with(|local| {
                if let Some(top) = local.frames.get_mut(opened.frame as usize) {
                    fold(top, opened, true);
                }
            });
        }
    }
}

/// Runs `key` in the scope, iterating while it heads a cycle, and keeps what it made.
fn execute<F: Scoped, R>(
    db: &F::Db,
    storage: &Storage,
    scope: &Scope<'_>,
    table: &Table<'_, F>,
    key: F::Key,
    hand: impl FnOnce(&F::Value) -> R,
) -> R {
    let frames = local::with(|local| local.frames.len() as u32);
    let at = {
        let mut runs = scope.runs.borrow_mut();
        let run = Run {
            frames,
            head: NONE,
            reentered: false,
            round: 1,
            serial: scope.next_serial(),
            provisional: None,
        };
        runs.push(run);
        (runs.len() - 1) as u32
    };
    // clone: the key, held by the table while its run uses the caller's
    table.entries.borrow_mut().insert(key.clone(), Entry::Running(at));
    let opened = open_reads(storage);
    let mut unwind = Unwind { scope, table, key: &key, at, opened, armed: true };
    let value = loop {
        let new = blaze_mem::frame::task(F::TAG, |_| super::attach(db, || F::execute(db, &key)));
        let run = scope.runs.borrow()[at as usize];
        // It heads a cycle when its own key was asked for and no value it read is of a lower
        // head's round.
        if !(run.reentered && (run.head == NONE || run.head == at)) {
            break new;
        }
        let last = run.provisional.expect("a reentered run has its round's value").cast::<F::Value>();
        // SAFETY: a value of the table, alive until the scope drops.
        if F::equal(unsafe { last.as_ref() }, &new) {
            break new;
        }
        if run.round >= MAX_ROUNDS {
            // alloc: the panic's text
            refuse(
                blaze_report::say!(
                    "the fixpoint of task-scoped ",
                    Code(F::NAME),
                    Words(&format!("({}) did not converge in {MAX_ROUNDS} rounds", key_text::<F>(&key))), // alloc: as above
                )
                .flat(),
            );
        }
        let next = table.keep(scope.frame, new);
        let mut runs = scope.runs.borrow_mut();
        let run = &mut runs[at as usize];
        run.provisional = Some(next.cast());
        run.round += 1;
        run.reentered = false;
        run.head = NONE;
    };
    unwind.armed = false;
    let run = scope.runs.borrow_mut().pop().expect("the run `execute` pushed");
    let closed = close_reads(storage, scope.frame, opened);
    let value = table.keep(scope.frame, value);
    let entry = if run.head < at {
        // A value of a lower head's round: kept for that round, and its reader is of it too.
        scope.depend_on(run.head);
        let head = scope.runs.borrow()[run.head as usize];
        Some(Entry::Provisional { value, head: run.head, serial: head.serial, round: head.round })
    } else {
        match closed {
            Closed::Unshareable => None,
            Closed::NoQuery => Some(Entry::Final { value, kept: None }),
            Closed::Kept(kept) => Some(Entry::Final { value, kept: Some(kept) }),
        }
    };
    let settled = matches!(entry, Some(Entry::Final { .. }));
    // The planted bug: a round's value of a cycle is told as settled too.
    #[cfg(test)]
    let settled = settled
        || (matches!(entry, Some(Entry::Provisional { .. }))
            && probe::SETTLES_PROVISIONAL.with(Cell::get));
    {
        let mut entries = table.entries.borrow_mut();
        match entry {
            Some(entry) => {
                *entries.get_mut(&key).expect("the entry `execute` made") = entry;
            }
            None => {
                entries.remove(&key);
            }
        }
    }
    if settled {
        // No borrow of the scope is held: the hook may ask for scoped values itself.
        // SAFETY: a value of the table, alive until the scope drops.
        F::settled(db, &key, unsafe { value.as_ref() });
    }
    // SAFETY: a value of the table, alive until the scope drops.
    hand(unsafe { value.as_ref() })
}

#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    thread_local! {
        /// The planted bug: on this thread a scoped run's reads are taken off its caller's
        /// frame once kept, and a hit replays none of them.
        pub(crate) static DROP_FORWARDED_READS: Cell<bool> = const { Cell::new(false) };
        /// The planted bug: on this thread [`super::Scoped::settled`] is told a cycle's round
        /// values too, not only the final ones.
        pub(crate) static SETTLES_PROVISIONAL: Cell<bool> = const { Cell::new(false) };
    }
}

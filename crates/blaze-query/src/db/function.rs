//! Tracked functions: a column of [`Row`]s by key ID, the warm hit, verifying, running,
//! backdating, cycles and eviction.
//!
//! - **Hit**: the ingredient's number, the storage's directory slot, the row page, the row's
//!   `verified` (equal to the storage's clock): four dependent loads, no lock, no shared write.
//!   Inside a query, one push of the row's node onto the thread's edge stack.
//! - **Verify**: a row of an earlier view is verified first by its durability (one compare),
//!   then by its input summary (one 128-bit AND with the fields written since), then by a
//!   claim-free walk of its edges in read order ([`super::verify`]), which stops at
//!   the first changed one; only a row that must run takes its flight, and verifies its edges
//!   again there, running each changed dependency (whose new value may be backdated).
//! - **Run**: under the key's flight, with `RUNNING` set; the new value is backdated (its old
//!   `changed` kept) when it equals the old one (`==`, or the 64-bit hash once evicted), or the
//!   row was verified in this view without its value; never at a lower durability, never in a
//!   cycle.
//! - **Cycles** resolve on the task that meets them (a cycle across tasks is a wait that would
//!   close a cycle of tasks: one gives way, `Cancelled::WouldWaitOnItself`, and meets the cycle
//!   on its own stack when it runs again). Heads are stack positions; a frame's `head` is the
//!   lowest it depends on, so nested cycles merge into the outermost head, which iterates.
//!   A read of a provisional row records no edge ([`read_provisional`]); each run of the cycle
//!   hands its other reads on to its reader in read order, and so to the head ([`hand_over`]);
//!   the converged head stores them all, over every round, as its edges, and each member's one
//!   edge is its head ([`finalize`]). So a later view verifies a final cycle as any row: when
//!   nothing it read changed the whole cycle is reused, else it runs again from its head. A
//!   provisional row (a round not yet converged) is never verified.

use std::{
    cell::Cell,
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering},
};

use blaze_mem::Tag;
use crossbeam_queue::SegQueue;

use super::{
    AsId, Cancelled, Cycle, Database, DatabaseKeyIndex, EventKind, FromIdIn, Id, MemoryUsage,
    budget::Held,
    counts,
    id::PAGE_LEN,
    ingredient::{self, Explicit, Ingredient, Kind, Vtable, Weighed},
    local::{self, Frame, Kind as FrameKind, Member, NONE, Pending},
    provenance::Cause,
    row::{
        self, CYCLE, DURABILITY, HASHED, HEAD, Header, PROVISIONAL, Pages, RUNNING, Row, STALE,
        UNTRACKED, VALUE, VIEW_MASK, row_at, span,
    },
    storage::{
        ALL, Durability, FIELD, Relocate, Restamp, Storage, Summary, Table, color, key_parts,
        node_key, or,
    },
    verify,
};

/// How a function meets a cycle through itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CycleKind {
    /// Panics, naming the query.
    Panic,
    /// `cycle_result`: every query of the cycle gets its fallback.
    Result,
    /// `cycle_initial`/`cycle_fn`: fixpoint iteration from the head.
    Fixpoint,
}

/// The most rounds of a fixpoint before it panics, naming its head.
pub const MAX_ROUNDS: u32 = 200;

/// One tracked function. `Key` is `Id`-backed: the one argument's type, or the hidden
/// interned tuple of several (or of none: a singleton key).
pub trait Function: 'static {
    type Db: Database;
    type Key: AsId + FromIdIn + Copy + Send + Sync + 'static;
    type Value: Send + Sync + 'static;
    const NAME: &'static str;
    const FILE: &'static str;
    /// `lru`: the values are kept by their bytes ([`Function::weigh`]) under their tag's budget
    /// ([`super::budget`]).
    const BUDGETED: bool;
    /// `lru` or `lru = 0` given: the values may be evicted, and without `NO_EQ` `hash` is real.
    const EVICTABLE: bool;
    const NO_EQ: bool;
    const CYCLE: CycleKind;
    const TAG: Tag;
    fn ingredient() -> &'static Ingredient;
    fn execute(db: &Self::Db, key: Self::Key) -> Self::Value;
    fn cycle_result(db: &Self::Db, id: Id, key: Self::Key) -> Self::Value;
    fn cycle_initial(db: &Self::Db, id: Id, key: Self::Key) -> Self::Value;
    fn cycle_recover(
        db: &Self::Db,
        cycle: &Cycle<'_>,
        last: &Self::Value,
        new: Self::Value,
        key: Self::Key,
    ) -> Self::Value;
    fn equal(old: &Self::Value, new: &Self::Value) -> bool;
    fn hash(value: &Self::Value) -> u64;
    /// The heap bytes `value` holds, which dropping it frees (`weigh = ..`); only a
    /// [`Function::BUDGETED`] function's values are weighed.
    fn weigh(value: &Self::Value) -> usize;
    /// Writes `key` as an error about its row names it (rule 19): its value as its `Debug`
    /// shows it, read through the attached database ([`super::attach`]), or its ID where its
    /// type has none. Generated queries write their key's `Debug` (each argument's, for a key of
    /// several); this default writes the ID.
    fn write_key(key: Self::Key, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{:?}", key.as_id())
    }
}

/// A function's table: its rows, how many hold a value, and for a function kept by bytes
/// (`lru`), what its values weigh and an index of the keys that hold one, read only by the
/// view boundary's pass ([`super::budget`]).
pub(crate) struct FnTable<F: Function> {
    pages: Pages,
    values: AtomicUsize,
    /// What the values weigh ([`Function::weigh`]), as each was made and let go: signed, as a
    /// value that grew after it was weighed (an outline's bodies, parsed when walked) takes off
    /// more than it put on, until the next pass weighs every value again.
    bytes: AtomicI64,
    /// The keys that took a value since the view boundary last folded them into
    /// [`FnTable::held`] ([`gained`]): a boundary costs the values made since, never the values
    /// held or the rows made. A daemon's build opens thousands of views (each input write is
    /// one), and a walk of every row of `parse_macro_expansion` at each was most of a
    /// self-build's time (sb3slow).
    fresh: SegQueue<u32>,
    /// Keys that hold a value or held one, unsorted, a key perhaps more than once: what a pass
    /// reads, sorted and cut to the keys that hold one then, and at twice the values held.
    #[expect(clippy::disallowed_types, reason = "held to push a key that now holds a value, or to take the list at a pass")]
    held: blaze_core::lock::Lock<Vec<u32>>,
    _of: PhantomData<fn() -> F>,
}

impl<F: Function> Table for FnTable<F> {
    #[expect(clippy::disallowed_types, reason = "makes the locks its fields allow")]
    fn new(storage: &Storage, index: u32) -> FnTable<F> {
        storage.check_db::<F::Db>(F::NAME);
        if F::BUDGETED {
            storage.add_weighed(index);
        }
        FnTable {
            pages: Pages::new(Tag::Query),
            values: AtomicUsize::new(0),
            bytes: AtomicI64::new(0),
            fresh: SegQueue::new(),
            // alloc: none until a value is held, then a budgeted function's index
            held: blaze_core::lock::Lock::new("query.function.held", Vec::new()),
            _of: PhantomData,
        }
    }

    fn pages(&self) -> &Pages {
        &self.pages
    }
}

type RowOf<F> = Row<<F as Function>::Value>;

impl Vtable {
    pub const fn function<F: Function>() -> Vtable {
        Vtable {
            name: F::NAME,
            file: F::FILE,
            kind: Kind::Function,
            fields: &[],
            row_size: size_of::<RowOf<F>>(),
            ids_of: None,
            made: None,
            memory: memory::<F>,
            used: Some(used::<F> as fn(&Storage, u32) -> u64),
            verify: Some(verify_edge::<F> as unsafe fn(*const (), &Storage, Id) -> Verified),
            reset: Some(reset_row::<F> as unsafe fn(&Storage, u32, Id)),
            weighed: if F::BUDGETED {
                Some(Weighed {
                    tag: F::TAG,
                    weight: weight::<F>,
                    held: held_values::<F>,
                    evict: evict_held::<F>,
                })
            } else {
                None
            },
            explicit: if F::EVICTABLE && !F::BUDGETED {
                Some(Explicit {
                    tag: F::TAG,
                    values: explicit_values::<F>,
                    evict: evict_table::<F>,
                })
            } else {
                None
            },
            unread: if F::EVICTABLE {
                Some(evict_unread::<F> as fn(&Storage, u32, u32) -> (usize, u64))
            } else {
                None
            },
            relocate: Some(relocate::<F> as fn(&mut Storage, u32, &mut Relocate)),
            restamp: Some(restamp::<F> as fn(&Storage, u32, &mut Restamp<'_>)),
            write_key: Some(write_key::<F> as fn(&Storage, Id, &mut String)),
            drop: drop_table::<F>,
        }
    }
}

/// The value of `key`: a hit, or verified, or run.
#[inline]
pub fn fetch<'db, F: Function>(db: &'db F::Db, key: F::Key) -> &'db F::Value {
    let storage = db.storage();
    let id = key.as_id();
    let clock = storage.clock();
    let dir = storage.dir(F::ingredient().index());
    if !dir.is_null() {
        // SAFETY: a published directory is readable whole for `MAX_PAGES` entries
        // (`Pages::dir`), and `id.page() < MAX_PAGES` (masked, `Id::page`); it lives as long
        // as the storage.
        let page = unsafe { &*dir.add(id.page()) }.load(Ordering::Acquire);
        if let Some(page) = NonNull::new(page) {
            // SAFETY: the page was made for this function's rows, `id.slot() < PAGE_LEN`.
            let row = unsafe { row_at::<RowOf<F>>(page, id.slot()) };
            if u64::from(row.header.verified.load(Ordering::Acquire)) == clock {
                read(storage, &row.header);
                // SAFETY: `verified == view | VALUE`, loaded with acquire: the value is
                // published in this view and no writer may start in it (`row`'s rule 2).
                return unsafe { row.value() };
            }
        }
    }
    fetch_cold::<F>(db, key)
}

/// Whether the row of `key` is current in this view (verified, with its value) and durable:
/// it read nothing below kind `HIGH` and made no untracked read. Such a row's value changes
/// only in a view a `HIGH` write opens, so a caller that fetched it, saw `true` here and
/// kept [`Storage::durable_view`] knows, for as long as that view is the same, that the row
/// holds that value: no fetch, no verification. `false` for a row never run, not current
/// (fetch it first), or of a lower kind. Records no read: for a caller outside any query.
pub fn durable<F: Function>(db: &F::Db, key: F::Key) -> bool {
    let storage = db.storage();
    let id = key.as_id();
    let dir = storage.dir(F::ingredient().index());
    if dir.is_null() {
        return false;
    }
    // SAFETY: a published directory is readable whole for `MAX_PAGES` entries
    // (`Pages::dir`), and `id.page() < MAX_PAGES` (masked, `Id::page`); it lives as long
    // as the storage. As [`fetch`].
    let page = unsafe { &*dir.add(id.page()) }.load(Ordering::Acquire);
    let Some(page) = NonNull::new(page) else { return false };
    // SAFETY: the page was made for this function's rows, `id.slot() < PAGE_LEN`.
    let row = unsafe { row_at::<RowOf<F>>(page, id.slot()) };
    u64::from(row.header.verified.load(Ordering::Acquire)) == storage.clock()
        && row.header.meta.load(Ordering::Relaxed) & UNTRACKED == 0
        && row.header.durability() >= Durability::HIGH.0
}

/// Records a read of a row's current value into the running query, if any.
#[inline(always)]
fn read(storage: &Storage, header: &Header) {
    let note = local::with(|local| {
        if local.frames.is_empty() {
            return local::Note::Fine;
        }
        let node = header.node.load(Ordering::Relaxed);
        local.read(storage, node, header.durability(), header.changed.load(Ordering::Relaxed))
    });
    super::noted(storage, note);
}

/// A read of a provisional row of the cycle headed at stack index `head`: a read inside the
/// cycle, told apart here, where it is made. It records no edge: the cycle's rows are proved
/// by the reads the cycle made outside itself, which its runs hand over in read order
/// ([`hand_over`]); the reader joins the cycle (its `head`).
fn read_provisional(storage: &Storage, header: &Header, head: u32) {
    local::with(|local| {
        let changed = header.changed.load(Ordering::Relaxed);
        if let Some(frame) = local.recording(storage) {
            frame.durability = frame.durability.min(header.durability());
            frame.changed = frame.changed.max(changed);
            frame.head = frame.head.min(head);
            frame.unshareable = true;
        }
    });
}

/// The most bytes of a key's text [`write_key`] writes: a key's `Debug` can run to gigabytes (a
/// name-resolution key's macro scope did, 2026-11-03), and its text goes into errors and the
/// daemon's memory line, so the text stops here, marked as cut.
pub(crate) const KEY_TEXT_MOST: usize = 1024;

/// A writer that takes at most [`KEY_TEXT_MOST`] bytes, then refuses the rest, so a huge
/// `Debug` stops at once instead of being written out and thrown away.
struct Capped<'a> {
    out: &'a mut String,
    left: usize,
    cut: bool,
}

impl std::fmt::Write for Capped<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if s.len() <= self.left {
            self.out.push_str(s);
            self.left -= s.len();
            return Ok(());
        }
        let mut end = self.left;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.out.push_str(&s[..end]);
        self.left = 0;
        self.cut = true;
        Err(std::fmt::Error)
    }
}

/// Writes a key's text with `write` onto `out`, cut at [`KEY_TEXT_MOST`] bytes and marked
/// with `id` when cut. False when `write` failed on its own (its `Debug` failed), with `out`
/// left as it was.
pub(crate) fn write_key_text(
    out: &mut String,
    id: Id,
    write: impl FnOnce(&mut dyn std::fmt::Write) -> std::fmt::Result,
) -> bool {
    use std::fmt::Write as _;
    let start = out.len();
    let mut capped = Capped { out: &mut *out, left: KEY_TEXT_MOST, cut: false };
    let written = write(&mut capped);
    if capped.cut {
        _ = write!(out, "... (cut at {KEY_TEXT_MOST} bytes; {id:?})");
        return true;
    }
    if written.is_err() {
        out.truncate(start);
        return false;
    }
    true
}

/// [`Vtable::write_key`] of `F`: its key `id` as [`Function::write_key`] shows it
/// ([`write_key_text`]), its ID when `id` is not a key of `F`'s in `storage` or its `Debug`
/// fails.
fn write_key<F: Function>(storage: &Storage, id: Id, out: &mut String) {
    use std::fmt::Write as _;
    let written = F::Key::from_id_in(storage, id)
        .is_some_and(|key| write_key_text(out, id, |out| F::write_key(key, out)));
    if !written {
        _ = write!(out, "{id:?}");
    }
}

/// What a row is to a task that wants it current.
enum State {
    /// Verified in this view (with its value, if asked).
    Current,
    /// A provisional row of a cycle whose head is at this stack index of this task.
    Provisional(u32),
    /// On this task's stack at this index: a cycle through it.
    OnStack(u32),
    /// Proved unchanged assuming the verify at this stack index is: the caller's verify
    /// depends on that one too.
    Assumed,
}

/// What verifying a dependency for a reader's edge found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Verified {
    /// Changed, or can't be told here (running on this task, provisional): the edge changed.
    Changed,
    /// Verified (or run) in this view: its color says whether it changed.
    Current,
    /// Unchanged if a verify lower on this task's stack (a cycle's) turns out unchanged, which
    /// the caller's frame now depends on.
    Assumed,
}

#[cold]
#[inline(never)]
fn fetch_cold<'db, F: Function>(db: &'db F::Db, key: F::Key) -> &'db F::Value {
    let storage = db.storage();
    let (row, index) = row_or_make::<F>(storage, key.as_id());
    match refresh::<F>(db, storage, index, row, key, true) {
        State::Current => {
            read(storage, &row.header);
            // SAFETY: `refresh` saw `verified == view | VALUE` with acquire (rule 2).
            unsafe { row.value() }
        }
        State::Provisional(head) => {
            read_provisional(storage, &row.header, head);
            // SAFETY: a provisional row of this task's own cycle, between its writes (rule 4).
            unsafe { row.value() }
        }
        State::OnStack(at) => reenter::<F>(db, storage, row, at, key),
        State::Assumed => unreachable!("a fetch is never proved on an assumption"),
    }
}

/// The row of key `id`, its page made when it has none.
///
/// # Panics
///
/// When no ingredient owns `id`'s page: an ID this storage never gave (page 0, a page not yet
/// made, past [`Id::MAX_U32`]), forged or decoded unchecked. Its row would sit in a page no
/// sweep or drop can name. The unwind is typed ([`super::NeverGiven`]) and fails the asking
/// job alone.
fn row_or_make<F: Function>(storage: &Storage, id: Id) -> (&RowOf<F>, u32) {
    let (table, index) = storage.table_or_make::<FnTable<F>>(F::ingredient());
    if storage.owner(id).is_none() {
        super::never_given(storage, index, id);
    }
    let (size, align) = row::layout::<RowOf<F>>();
    let page = table.pages.page_or_make(id.page(), &storage.heap, size, align);
    // SAFETY: the page was made for `RowOf<F>` rows; `id.slot() < PAGE_LEN`.
    (unsafe { row_at::<RowOf<F>>(page, id.slot()) }, index)
}

/// Brings `row` up to this view: with its value when `value`. Returns how the caller may read
/// it; without `value`, `Provisional` and `OnStack` mean its edge counts as changed.
fn refresh<F: Function>(
    db: &F::Db,
    storage: &Storage,
    index: u32,
    row: &RowOf<F>,
    key: F::Key,
    value: bool,
) -> State {
    let id = key.as_id();
    let header = &row.header;
    let mut rounds = 0;
    loop {
        if storage.stopped() {
            Cancelled::PendingWrite.throw();
        }
        rounds += 1;
        if rounds > REFRESH_ROUNDS {
            super::runaway_rounds(storage, index, id, rounds - 1, REFRESH_ROUNDS);
        }
        let view = storage.view_bits();
        let word = header.verified.load(Ordering::Acquire);
        if word & (RUNNING | PROVISIONAL) == 0
            && word & VIEW_MASK == view
            && (word & VALUE != 0 || !value)
        {
            return State::Current;
        }
        // A row this task's open verify already proved on an assumption: the same answer
        // again, not its edges walked again (and not a row brought up to date).
        if !value
            && local::with(|local| {
                let head = local.pending_head(std::ptr::from_ref(header), word)?;
                depend_on(local, head);
                Some(())
            })
            .is_some()
        {
            return State::Assumed;
        }
        if let Some(steps) = local::with(local::Local::burn) {
            super::out_of_fuel::<F>(db, steps, index, key);
        }
        let node = header.node.load(Ordering::Relaxed);
        if let Some(at) = local::with(|local| local.find(storage, node)) {
            if !value && local::with(|local| assume(local, at)) {
                return State::Assumed;
            }
            return State::OnStack(at);
        }
        if word & PROVISIONAL != 0 {
            let head = header.extra.load(Ordering::Relaxed) as u32;
            match local::with(|local| local.find(storage, head)) {
                Some(at) if header.meta.load(Ordering::Relaxed) & STALE == 0 => {
                    return State::Provisional(at);
                }
                Some(_) => {}
                None => {
                    wait_for(storage, head);
                    continue;
                }
            }
        } else if word & RUNNING == 0
            && node != 0
            && word & VIEW_MASK != view
            && verify::unclaimed(storage, header, word, node)
        {
            continue;
        }
        let db_ptr = std::ptr::from_ref(db).cast::<()>();
        let mut assumed = None;
        in_flight(storage, index, id, || {
            assumed = claimed::<F>(db_ptr, storage, index, row, key, value);
            complete_one();
        });
        if let Some(head) = assumed {
            local::with(|local| depend_on(local, head));
            return State::Assumed;
        }
    }
}

/// The most times one [`refresh`] may go round before the row comes up to date (rule 18). A
/// round ends in an answer or follows progress: a compare-and-swap another task won, a cycle
/// head's flight that ended, a claim-free walk that marked the row, a run or verify under the
/// flight. So a refresh goes round a few times; this bound turns a row that never comes up to
/// date (a provisional row left with no head, a word that keeps changing back) into a
/// [`super::Runaway`] naming it, instead of a spin that holds the check forever.
const REFRESH_ROUNDS: u64 = 1 << 16;

/// Whether the verify asking (the top frame) may assume the verify at `at` unchanged; if so it
/// now depends on it.
fn assume(local: &mut local::Local, at: u32) -> bool {
    if !local.may_assume(at) {
        return false;
    }
    depend_on(local, at);
    true
}

fn depend_on(local: &mut local::Local, head: u32) {
    if let Some(frame) = local.frames.last_mut().filter(|it| it.kind == FrameKind::Verify) {
        frame.head = frame.head.min(head);
    }
}

/// A count of [`completed`]'s, alone on its cache line.
#[repr(align(128))]
struct Slot(AtomicU64);

const SLOTS: usize = 64;

/// [`completed`]'s counts, a slot per thread (threads past [`SLOTS`] share), so no two workers
/// write one cache line on each query.
static COMPLETED: [Slot; SLOTS] = [const { Slot(AtomicU64::new(0)) }; SLOTS];
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

std::thread_local! {
    static SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn complete_one() {
    let at = SLOT.with(|slot| {
        if slot.get() == usize::MAX {
            slot.set(NEXT_SLOT.fetch_add(1, Ordering::Relaxed) % SLOTS);
        }
        slot.get()
    });
    COMPLETED[at].0.fetch_add(1, Ordering::Relaxed);
}

/// The queries this process's databases brought up to date under their flight (each run or
/// verified there), on every thread: it grows while query work moves, so the daemon's
/// watchdog fails a build where tasks run and it stays still.
pub fn completed() -> u64 {
    COMPLETED.iter().map(|it| it.0.load(Ordering::Relaxed)).sum()
}

/// Runs `run` holding the flight of row `id` of `index`, or waits for whoever holds it.
fn in_flight(storage: &Storage, index: u32, id: Id, run: impl FnOnce()) {
    let barrier = local::barrier();
    let awaited = |it: &mut String| {
        use std::fmt::Write as _;
        _ = write!(it, "{:?}", DatabaseKeyIndex::new(index, id));
    };
    let got = storage.flights.get_or_run_named(Storage::flight(index, id), awaited, || {
        let _lifted = barrier.lift();
        run();
        Ok(())
    });
    drop(barrier);
    if got.is_err() {
        Cancelled::WouldWaitOnItself { asked: Some(DatabaseKeyIndex::new(index, id)) }.throw();
    }
}

/// Waits until the cycle headed by `head` (another task's) has ended.
fn wait_for(storage: &Storage, head: u32) {
    let (index, id, _) = key_parts(storage.key_of(head));
    in_flight(storage, index, id, || {});
}

/// The flight holder's part of [`refresh`]: verify the row's edges with its dependencies run
/// as needed, or run it. The stack index of the verify it assumed unchanged when it proved the
/// row on that assumption (only without `value`), else `None`.
fn claimed<F: Function>(
    db: *const (),
    storage: &Storage,
    index: u32,
    row: &RowOf<F>,
    key: F::Key,
    value: bool,
) -> Option<u32> {
    let header = &row.header;
    let view = storage.view_bits();
    let word = header.verified.load(Ordering::Acquire);
    let meta = header.meta.load(Ordering::Relaxed);
    if word & PROVISIONAL != 0 && meta & STALE == 0 {
        return None;
    }
    debug_assert_eq!(word & RUNNING, 0, "a row runs only in its flight");
    let current = word & VIEW_MASK == view && word & PROVISIONAL == 0;
    if current && (word & VALUE != 0 || !value) {
        return None;
    }
    let id = key.as_id();
    let node = node_of(storage, index, id, header);
    if current {
        storage.record_run(node, Cause::Evicted);
        run::<F>(db, storage, index, row, key, None, true);
        return None;
    }
    let verifiable = word & VIEW_MASK != 0 && word & PROVISIONAL == 0 && meta & UNTRACKED == 0;
    if !verifiable {
        let cause = if word & VIEW_MASK == 0 {
            Cause::New
        } else if word & PROVISIONAL != 0 {
            Cause::Cycle
        } else {
            Cause::Untracked
        };
        storage.record_run(node, cause);
        run::<F>(db, storage, index, row, key, None, false);
        return None;
    }
    if storage.unchanged_since(meta as u8 & DURABILITY as u8, word & VIEW_MASK)
        || storage.inputs_unchanged(storage.node(node), word & VIEW_MASK)
    {
        if mark(storage, header, word, node, index, id) && value && word & VALUE == 0 {
            storage.record_run(node, Cause::Evicted);
            run::<F>(db, storage, index, row, key, None, true);
        }
        return None;
    }
    let at =
        local::with(|local| local.push(Frame::new(storage, FrameKind::Verify, node, index, id)));
    let mut guard = Running { storage, header, word, at, index, id, armed: true };
    let changed = verify_claimed(db, storage, header, node, word & VIEW_MASK);
    let (reentered, head) = local::with(|local| {
        let frame = &local.frames[at as usize];
        (frame.reentered, frame.head)
    });
    guard.armed = false;
    drop(guard);
    if changed.is_none() && !reentered {
        if head < at {
            debug_assert!(!value, "a fetch's verify never depends on one below it");
            let rows = u64::from(storage.nodes_made());
            let over = local::with(|local| {
                let head_serial = local.frames[head as usize].serial;
                local.repoint_pending(local.frames[at as usize].pending_start, at, head);
                let pending = Pending {
                    header: std::ptr::from_ref(header),
                    word,
                    node,
                    ingredient: index,
                    id: id.as_u32(),
                    head,
                    head_serial,
                };
                let over = local.push_pending(pending, rows);
                local.pop(at);
                over
            });
            if over {
                super::runaway_pending(storage, head, rows);
            }
            return Some(head);
        }
        let from = local::with(|local| {
            let from = local.frames[at as usize].pending_start;
            local.pop(at);
            from
        });
        mark_pending(storage, from);
        if mark(storage, header, word, node, index, id) && value && word & VALUE == 0 {
            storage.record_run(node, Cause::Evicted);
            run::<F>(db, storage, index, row, key, None, true);
        }
        return None;
    }
    storage.record_run(node, changed.map_or(Cause::Cycle, Cause::Changed));
    local::with(|local| local.verify_to_run(at));
    run::<F>(db, storage, index, row, key, Some(at), false);
    None
}

/// Marks the rows from `from` on the pending list verified: the verify they assumed was proved.
fn mark_pending(storage: &Storage, from: u32) {
    while let Some(pending) = local::with(|local| local.pop_pending(from)) {
        // SAFETY: a pending row's header is in a page of `storage`, which outlives this call.
        let header = unsafe { &*pending.header };
        mark(
            storage,
            header,
            pending.word,
            pending.node,
            pending.ingredient,
            Id::from_u32(pending.id),
        );
    }
}

/// The row's node, made on its first run (by the flight holder).
fn node_of(storage: &Storage, index: u32, id: Id, header: &Header) -> u32 {
    match header.node.load(Ordering::Relaxed) {
        0 => {
            let node = storage.new_nodes(1);
            storage.set_key(node, node_key(index, id, 0));
            // Release: the node's key is written before any reader can see the node.
            header.node.store(node, Ordering::Release);
            node
        }
        node => node,
    }
}

/// Marks a row verified in this view, keeping whether it has its value. `false` when another
/// task changed its word meanwhile (the caller looks again).
pub(crate) fn mark(
    storage: &Storage,
    header: &Header,
    word: u32,
    node: u32,
    index: u32,
    id: Id,
) -> bool {
    let view = storage.view_bits();
    let to = view | (word & VALUE);
    if header.verified.compare_exchange(word, to, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return false;
    }
    storage.paint(node, color(view, header.changed.load(Ordering::Relaxed)));
    counts::count(index, counts::VERIFIED);
    if storage.has_hook() {
        storage.event(|| EventKind::DidValidateMemoizedValue {
            database_key: DatabaseKeyIndex::new(index, id),
        });
    }
    true
}

/// Checks a row's edges in read order holding its flight, bringing each dependency up to this
/// view (running it if it changed): the node of the first that changed since `verified`, or
/// `None` when none did, and then the row's summary has its dependencies' current ones.
fn verify_claimed(
    db: *const (),
    storage: &Storage,
    header: &Header,
    row_node: u32,
    verified: u32,
) -> Option<u32> {
    let edges = header.edges.load(Ordering::Acquire);
    let (start, len) = (span::start(edges), span::len(edges));
    let mut summary = [0; 2];
    for at in start..start + len {
        let node = storage.edge(at);
        let row = storage.node(node);
        let mut seen = row.color();
        let view = storage.view_bits();
        let painted = (seen >> 32) as u32;
        if painted < FIELD && painted != view {
            let (index, id, _) = key_parts(row.key());
            let Some(verify) = ingredient::vtable(index).and_then(|it| it.verify) else {
                return Some(node);
            };
            // SAFETY: `db` is this storage's database as every function of it names it
            // (`Storage::check_db`), and `id` is a key of ingredient `index` (its node's key).
            match unsafe { verify(db, storage, id) } {
                Verified::Changed => return Some(node),
                Verified::Assumed => {
                    summary = or(summary, row.summary());
                    continue;
                }
                Verified::Current => {}
            }
            seen = row.color();
            if (seen >> 32) as u32 != view {
                return Some(node);
            }
        }
        if seen as u32 > verified {
            return Some(node);
        }
        summary = or(summary, row.summary());
    }
    storage.grow_summary(row_node, summary);
    None
}

/// [`Vtable::verify`] of `F`: brings row `id` up to this view for a reader's edge.
unsafe fn verify_edge<F: Function>(db: *const (), storage: &Storage, id: Id) -> Verified {
    // SAFETY: the caller's contract: `db` is a `&F::Db` that owns `storage`.
    let db = unsafe { &*db.cast::<F::Db>() };
    let Some(key) = F::Key::from_id_in(storage, id) else { return Verified::Changed };
    let (row, index) = row_or_make::<F>(storage, id);
    match refresh::<F>(db, storage, index, row, key, false) {
        State::Current => Verified::Current,
        State::Assumed => Verified::Assumed,
        State::Provisional(_) | State::OnStack(_) => Verified::Changed,
    }
}

/// What a run leaves on its frame.
struct Ran {
    head: u32,
    reentered: bool,
    unconverged: bool,
    untracked: bool,
    durability: u8,
    changed: u32,
    round: u32,
    members: u32,
    node: u32,
}

fn ran(at: u32) -> Ran {
    local::with(|local| {
        debug_assert_eq!(local.frames.len() as u32, at + 1, "a run's frames above it ended");
        let frame = &local.frames[at as usize];
        Ran {
            head: frame.head,
            reentered: frame.reentered,
            unconverged: frame.unconverged,
            untracked: frame.untracked,
            durability: frame.durability,
            changed: frame.changed,
            round: frame.round,
            members: frame.members_start,
            node: frame.node,
        }
    })
}

/// Undoes a run that unwinds: its cycle's rows are emptied, its own row too if a cycle wrote
/// it, else its word is put back.
struct Running<'a> {
    storage: &'a Storage,
    header: &'a Header,
    word: u32,
    at: u32,
    index: u32,
    id: Id,
    armed: bool,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let members = local::with(|local| {
            let frame = &local.frames[self.at as usize];
            let (members, pending) = (frame.members_start, frame.pending_start);
            let cycle = frame.cycle.start as usize;
            local.truncate_pending(pending as usize);
            // The unwinding cycle's reads go with its rows.
            local.cycle_edges.truncate(cycle);
            members
        });
        empty_members(self.storage, members);
        if self.header.verified.load(Ordering::Relaxed) & PROVISIONAL != 0 {
            if let Some(reset) = ingredient::vtable(self.index).and_then(|it| it.reset) {
                // SAFETY: the row is this task's (its flight is held here), provisional.
                unsafe { reset(self.storage, self.index, self.id) };
            }
        } else {
            self.header.verified.store(self.word, Ordering::Release);
        }
        local::with(|local| local.pop(self.at));
    }
}

/// Empties the cycle members from `from` on the member list: a cycle's unwinding. A head that
/// a later round ran to a final value (it read no provisional row) is left as it is: it is
/// published, so another task may be reading it.
fn empty_members(storage: &Storage, from: u32) {
    loop {
        let Some(member) = local::with(|local| {
            (local.members.len() as u32 > from).then(|| local.members.pop()).flatten()
        }) else {
            return;
        };
        if member_header(storage, member)
            .is_some_and(|it| it.verified.load(Ordering::Relaxed) & PROVISIONAL != 0)
        {
            reset_member(storage, member);
        }
    }
}

fn reset_member(storage: &Storage, member: Member) {
    let id = Id::from_u32(member.id);
    if let Some(reset) = ingredient::vtable(member.ingredient).and_then(|it| it.reset) {
        // SAFETY: a member is a provisional row this task's cycle made (rule 4).
        unsafe { reset(storage, member.ingredient, id) };
    }
}

/// The untyped header of a member row.
fn member_header(storage: &Storage, member: Member) -> Option<&Header> {
    let id = Id::from_u32(member.id);
    let size = ingredient::vtable(member.ingredient)?.row_size;
    let dir = storage.dir(member.ingredient);
    if dir.is_null() {
        return None;
    }
    // SAFETY: a published directory is readable whole for `MAX_PAGES` entries
    // (`Pages::dir`), and `id.page() < MAX_PAGES` (masked, `Id::page`); it lives as long as
    // the storage.
    let page = NonNull::new(unsafe { &*dir.add(id.page()) }.load(Ordering::Acquire))?;
    // SAFETY: the page is the member function's, made with rows of `size` bytes.
    Some(unsafe { row::header_at(page, id.slot(), size) })
}

/// Runs `key` holding its flight, its frame pushed (or the verify frame `frame` turned into
/// it). `keep`: the row was verified in this view without its value, so its stamp stands.
fn run<F: Function>(
    db: *const (),
    storage: &Storage,
    index: u32,
    row: &RowOf<F>,
    key: F::Key,
    frame: Option<u32>,
    keep: bool,
) {
    // SAFETY: as in `verify_edge`: `db` is this storage's `&F::Db`.
    let db = unsafe { &*db.cast::<F::Db>() };
    let header = &row.header;
    let id = key.as_id();
    let node = node_of(storage, index, id, header);
    let mut word = header.verified.load(Ordering::Acquire);
    if word & RUNNING == 0 {
        let meta = header.meta.load(Ordering::Relaxed);
        if meta & STALE != 0 {
            header.meta.store(meta & !STALE, Ordering::Relaxed);
        }
        if header
            .verified
            .compare_exchange(word, word | RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            if let Some(at) = frame {
                local::with(|local| local.pop(at));
            }
            return;
        }
    } else {
        word &= !RUNNING;
    }
    let at = frame.unwrap_or_else(|| {
        local::with(|local| local.push(Frame::new(storage, FrameKind::Run, node, index, id)))
    });
    let mut running = Running { storage, header, word, at, index, id, armed: true };
    storage.event(|| EventKind::WillExecute { database_key: DatabaseKeyIndex::new(index, id) });
    counts::count(index, counts::EXECUTED);
    loop {
        // A run makes its text (an error's, a diagnostic's) with its own database lent to the
        // `Debug`s that read one, whichever worker runs it: the text is the same on every thread
        // (rules 20 and 25), and nothing inside a query formats with none attached.
        let new = blaze_mem::frame::task(F::TAG, |_| super::attach(db, || F::execute(db, key)));
        crate::ran_one();
        let done = ran(at);
        if done.head == NONE {
            finish::<F>(storage, row, id, &done, at, word, new, keep);
            break;
        }
        if done.head < at {
            participate::<F>(db, storage, row, &done, at, word, new, key);
            break;
        }
        debug_assert!(done.reentered && done.head == at);
        if head_round::<F>(db, storage, row, &done, at, new, key) {
            break;
        }
    }
    running.armed = false;
    local::with(|local| local.pop(at));
}

/// The durability a run read: `HIGH` when it read nothing, `LOW` when untracked.
fn durability_of(done: &Ran) -> u32 {
    if done.untracked { 0 } else { u32::from(done.durability.min(Durability::HIGH.0)) }
}

/// Stores the run's edges as the row's, forgetting its old ones, and counts its reads to its
/// query ([`counts::reads`]): those it recorded, those the row keeps, and those it held at its
/// end. Counted here, once a run, so a read costs nothing more.
fn store_edges(storage: &Storage, header: &Header, at: u32) {
    let (span, ingredient, reads, note) = local::with(|local| {
        debug_assert_eq!(
            at as usize + 1,
            local.frames.len(),
            "a run's edges are stored while it is the top frame"
        );
        let (most, note) = local.note_peak(at as usize);
        let frame = &local.frames[at as usize];
        let (start, owner) =
            (frame.edges_start as usize, (frame.ingredient, Id::from_u32(frame.id)));
        let recorded = (local.edges.len() - start) as u64 + frame.dropped;
        local.dedup_from(start);
        let kept = (local.edges.len() - start) as u64;
        let reads = counts::Reads { recorded, kept, most };
        (storage.push_edges(&local.edges[start..], owner), owner.0, reads, note)
    });
    // The run is still on top: a new mark names it.
    super::noted(storage, note);
    counts::reads(ingredient, reads);
    let old = header.edges.swap(span, Ordering::Relaxed);
    storage.forget_edges(old);
}

/// Sets the summary of the run at `at` (node `node`): the OR of its edges' and its replayed
/// reads', all bits when it made an untracked read.
fn summarize(storage: &Storage, node: u32, at: u32, untracked: bool) {
    let summary = if untracked {
        ALL
    } else {
        local::with(|local| {
            let frame = &local.frames[at as usize];
            let start = frame.edges_start as usize;
            or(frame.summary, storage.summary_of_edges(&local.edges[start..]))
        })
    };
    storage.set_summary(node, summary);
}

/// A run outside any cycle: backdate, write, publish.
fn finish<F: Function>(
    storage: &Storage,
    row: &RowOf<F>,
    id: Id,
    done: &Ran,
    at: u32,
    word: u32,
    new: F::Value,
    keep: bool,
) {
    let header = &row.header;
    let view = storage.view_bits();
    let old_meta = header.meta.load(Ordering::Relaxed);
    let old_durability = old_meta & DURABILITY;
    let durability = durability_of(done);
    let now = header.verified.load(Ordering::Relaxed);
    let had = now & VALUE != 0;
    let final_old = had && now & PROVISIONAL == 0;
    let made_before = word & VIEW_MASK != 0 && word & PROVISIONAL == 0 && old_meta & CYCLE == 0;
    let backdate = made_before
        && durability >= old_durability
        && (keep
            // SAFETY: the old value is this view's flight holder's to read (rule 1).
            || (!F::NO_EQ && final_old && F::equal(unsafe { row.value() }, &new))
            || (F::EVICTABLE
                && !F::NO_EQ
                && !had
                && old_meta & HASHED != 0
                && F::hash(&new) == header.extra.load(Ordering::Relaxed)));
    let changed = if backdate { header.changed.load(Ordering::Relaxed) } else { view };
    let table = table_of::<F>(storage);
    if had {
        // SAFETY: the flight holder with `RUNNING` set drops a value of an earlier view, or a
        // provisional one of its own cycle that no frame reads any more (rules 1, 3, 4).
        unsafe { drop_value(table, row) };
    } else {
        gained(table, id);
    }
    let meta = durability | if done.untracked { UNTRACKED } else { 0 };
    // SAFETY: the flight holder, `RUNNING` set; the value was dropped above (rule 1).
    unsafe { put(table, row, new) };
    // A head whose last round read no row of its cycle (or a verify turned run that was met
    // by a cycle and then read none of it) ends outside any cycle: what it gathered as a head
    // is no one's now. It is the top frame, so all of the gathering above its start is its.
    local::with(|local| {
        let start = local.frames[at as usize].cycle.start as usize;
        local.cycle_edges.truncate(start);
    });
    // Nor are the rows it headed: the stale inner heads its earlier rounds kept, and members a
    // verify met for it. Their values came from its provisional ones, and with no head left to
    // finalize them, each would wait for it ever after (`refresh`'s `wait_for`, bounded only by
    // `REFRESH_ROUNDS`). They are emptied, as an unwinding cycle's are, before this row
    // publishes, so a task waiting on this flight finds them never made.
    drop_orphans(storage, done.members, done.node);
    store_edges(storage, header, at);
    summarize(storage, done.node, at, done.untracked);
    header.changed.store(changed, Ordering::Relaxed);
    header.meta.store(meta, Ordering::Relaxed);
    storage.paint(done.node, color(view, changed));
    // Release: the value, stamp and edges are published with the word (rule 2).
    header.verified.store(view | VALUE, Ordering::Release);
}

/// Empties the provisional rows from `from` on the member list whose head is node `head`, a run
/// that ended outside its cycle ([`finish`]), and drops them and every row no longer provisional
/// from the list. A provisional row of another head (one lower on the stack, which a verify met
/// while this run was on it) stays, for that head to finalize or empty.
fn drop_orphans(storage: &Storage, from: u32, head: u32) {
    if local::with(|local| local.members.len() as u32 <= from) {
        return;
    }
    let members = local::with(|local| local.members.split_off(from as usize)); // alloc: a head that ended outside its cycle, once
    for member in members {
        let Some(header) = member_header(storage, member) else { continue };
        if header.verified.load(Ordering::Relaxed) & PROVISIONAL == 0 {
            continue;
        }
        if header.extra.load(Ordering::Relaxed) as u32 == head {
            reset_member(storage, member);
        } else {
            local::with(|local| local.members.push(member));
        }
    }
}

/// A run that read a provisional row of a cycle headed below it: it becomes a member of that
/// cycle, provisional, owned by the head.
fn participate<F: Function>(
    db: &F::Db,
    storage: &Storage,
    row: &RowOf<F>,
    done: &Ran,
    at: u32,
    word: u32,
    new: F::Value,
    key: F::Key,
) {
    let header = &row.header;
    let id = key.as_id();
    let head = local::with(|local| local.frames[done.head as usize].node);
    let value = match (F::CYCLE, done.reentered) {
        (CycleKind::Result, _) => blaze_mem::frame::task(F::TAG, |_| F::cycle_result(db, id, key)),
        (CycleKind::Fixpoint, true) => {
            let round = local::with(|local| local.frames[done.head as usize].round);
            let heads = heads_of(storage, done.members, id);
            let cycle = Cycle { heads: &heads, id, iteration: round };
            // SAFETY: the row's provisional value, this task's own (rule 4).
            let last = unsafe { row.value() };
            let value =
                blaze_mem::frame::task(F::TAG, |_| F::cycle_recover(db, &cycle, last, new, key));
            if !F::equal(last, &value) {
                local::with(|local| local.frames[done.head as usize].unconverged = true);
            }
            value
        }
        (CycleKind::Fixpoint | CycleKind::Panic, _) => new,
    };
    let table = table_of::<F>(storage);
    if header.verified.load(Ordering::Relaxed) & VALUE != 0 {
        // SAFETY: the flight holder with `RUNNING` set replaces a value of an earlier view or
        // its own provisional one, which no frame reads any more (rules 1, 4).
        unsafe { drop_value(table, row) };
    } else {
        gained(table, id);
    }
    // SAFETY: as above, the row holds no value now.
    unsafe { put(table, row, value) };
    // A provisional row keeps no edges: its reads outside the cycle go on to its reader in
    // read order, and on to the head ([`hand_over`]); a final member's one edge is its head
    // ([`finalize`]). So no run of the cycle copies what the runs below it read.
    let note = local::with(|local| {
        let (_, note) = local.note_peak(at as usize);
        hand_over(storage, local, at, done.head);
        note
    });
    super::noted(storage, note);
    storage.forget_edges(header.edges.swap(0, Ordering::Relaxed));
    storage.set_summary(done.node, ALL);
    header.changed.store(done.changed, Ordering::Relaxed);
    let meta = durability_of(done) | if done.reentered { HEAD } else { 0 };
    header.meta.store(meta, Ordering::Relaxed);
    header.extra.store(u64::from(head), Ordering::Relaxed);
    if done.reentered {
        let own = done.node;
        let members = local::with(|local| local.members[done.members as usize..].to_vec()); // alloc: an inner head's merge, once a round
        for member in members {
            if let Some(member) = member_header(storage, member)
                && member.extra.load(Ordering::Relaxed) as u32 == own
            {
                member.extra.store(u64::from(head), Ordering::Relaxed);
            }
        }
    }
    local::with(|local| {
        local.members.push(Member { ingredient: F::ingredient().index(), id: id.as_u32() })
    });
    // Release: other tasks see `PROVISIONAL` and the head to wait for, never the value.
    header.verified.store((word & VIEW_MASK) | VALUE | PROVISIONAL, Ordering::Release);
}

/// The IDs of the heads of a cycle: `id` and every head member from `from`.
fn heads_of(storage: &Storage, from: u32, id: Id) -> Vec<Id> {
    let members = local::with(|local| local.members[from as usize..].to_vec()); // alloc: a fixpoint round's heads
    let mut heads = vec![id]; // alloc: as above
    for member in members {
        if member_header(storage, member)
            .is_some_and(|it| it.meta.load(Ordering::Relaxed) & HEAD != 0)
        {
            heads.push(Id::from_u32(member.id));
        }
    }
    heads
}

/// One round of the outermost head of a cycle ended: `true` when it converged and every row of
/// the cycle is final, `false` when it runs again.
fn head_round<F: Function>(
    db: &F::Db,
    storage: &Storage,
    row: &RowOf<F>,
    done: &Ran,
    at: u32,
    new: F::Value,
    key: F::Key,
) -> bool {
    let header = &row.header;
    let id = key.as_id();
    // SAFETY: the head's provisional value, this task's own, read by no frame now (rule 4).
    let last = unsafe { row.value() };
    let (value, converged) = match F::CYCLE {
        CycleKind::Result => {
            drop(new);
            (blaze_mem::frame::task(F::TAG, |_| F::cycle_result(db, id, key)), !done.unconverged)
        }
        CycleKind::Fixpoint => {
            let heads = heads_of(storage, done.members, id);
            let cycle = Cycle { heads: &heads, id, iteration: done.round };
            let value =
                blaze_mem::frame::task(F::TAG, |_| F::cycle_recover(db, &cycle, last, new, key));
            let converged = F::equal(last, &value) && !done.unconverged;
            (value, converged)
        }
        CycleKind::Panic => unreachable!("{} re-entered with no cycle handling", F::NAME),
    };
    let table = table_of::<F>(storage);
    // SAFETY: the head replaces its own provisional value between rounds (rule 4).
    unsafe { drop_value(table, row) };
    // SAFETY: as above; the row holds no value now.
    unsafe { put(table, row, value) };
    let view = storage.view_bits();
    if converged {
        let reads = store_cycle_edges(storage, header, at);
        let meta = finalize(storage, done.members, done.node, reads);
        storage.set_summary(done.node, reads.summary);
        header.changed.store(view, Ordering::Relaxed);
        header.meta.store(meta, Ordering::Relaxed);
        header.extra.store(0, Ordering::Relaxed);
        storage.paint(done.node, color(view, view));
        // Release: the final value, published (rule 2).
        header.verified.store(view | VALUE, Ordering::Release);
        return true;
    }
    assert!(
        done.round < MAX_ROUNDS,
        "the fixpoint of {}({id:?}) did not converge in {MAX_ROUNDS} rounds",
        F::NAME
    );
    let round = done.round + 1;
    storage.event(|| EventKind::WillIterateCycle {
        database_key: DatabaseKeyIndex::new(F::ingredient().index(), id),
        iteration: round,
    });
    // This round's reads go to the cycle's, each once, before they are cut below.
    let note = local::with(|local| {
        // Its most before the dedup lowers what it holds.
        let (_, note) = local.note_peak(at as usize);
        let start = local.frames[at as usize].edges_start as usize;
        local.dedup_from(start);
        let bound = fold_round(local, at);
        if bound == local::Note::Runaway { bound } else { note }
    });
    super::noted(storage, note);
    next_round(storage, done.members);
    let note = local::with(|local| {
        // Its most over the rounds: this round's reads are cut below.
        let (_, note) = local.note_peak(at as usize);
        let edges = local.edges.len();
        let frame = &mut local.frames[at as usize];
        let start = frame.edges_start as usize;
        frame.round = round;
        frame.head = NONE;
        frame.unconverged = false;
        frame.untracked = false;
        frame.durability = u8::MAX;
        frame.changed = 0;
        frame.summary = [0; 2];
        frame.dedup_floor = frame.edges_start;
        frame.dropped = 0;
        frame.dedup_at = local::next_dedup(start, 0);
        debug_assert!(edges >= start);
        local.truncate_edges(start);
        local.renew_serial(at);
        note
    });
    super::noted(storage, note);
    false
}

/// A new round of a cycle: its members are emptied, but heads, whose values carry over (as
/// stale, to run again before they are read).
fn next_round(storage: &Storage, from: u32) {
    let members = local::with(|local| local.members.split_off(from as usize)); // alloc: a fixpoint round's members
    for member in members {
        let Some(header) = member_header(storage, member) else { continue };
        if header.verified.load(Ordering::Relaxed) & PROVISIONAL == 0 {
            continue;
        }
        let meta = header.meta.load(Ordering::Relaxed);
        if meta & HEAD != 0 {
            header.meta.store(meta | STALE, Ordering::Relaxed);
            local::with(|local| local.members.push(member));
        } else {
            reset_member(storage, member);
        }
    }
}

/// Makes every member from `from` final (a stale head is emptied instead): the lowest
/// durability of the cycle on each, a new stamp, and one edge, to its head (node `head`), with
/// the summary of what the whole cycle read (`reads`): a member is proved unchanged exactly when
/// its head is, whose edges are the cycle's reads. Returns the `meta` of the cycle's rows.
fn finalize(storage: &Storage, from: u32, head: u32, reads: Converged) -> u32 {
    let members = local::with(|local| local.members.split_off(from as usize)); // alloc: a cycle's end, once
    let view = storage.view_bits();
    let mut lowest = reads.durability;
    for &member in &members {
        if let Some(header) = member_header(storage, member)
            && header.verified.load(Ordering::Relaxed) & PROVISIONAL != 0
            && header.meta.load(Ordering::Relaxed) & STALE == 0
        {
            lowest = lowest.min(header.meta.load(Ordering::Relaxed) & DURABILITY);
        }
    }
    for member in members {
        let Some(header) = member_header(storage, member) else { continue };
        let word = header.verified.load(Ordering::Relaxed);
        if word & PROVISIONAL == 0 {
            continue;
        }
        if header.meta.load(Ordering::Relaxed) & STALE != 0 {
            reset_member(storage, member);
            continue;
        }
        let node = header.node.load(Ordering::Relaxed);
        let span = storage.push_edges(&[head], (member.ingredient, Id::from_u32(member.id)));
        storage.forget_edges(header.edges.swap(span, Ordering::Relaxed));
        storage.set_summary(node, reads.summary);
        header.changed.store(view, Ordering::Relaxed);
        header.meta.store(lowest | CYCLE | reads.untracked, Ordering::Relaxed);
        header.extra.store(0, Ordering::Relaxed);
        storage.paint(node, color(view, view));
        // Release: the member's value, edge and stamp are final in this view (rule 2).
        header.verified.store(view | VALUE, Ordering::Release);
    }
    lowest | CYCLE | reads.untracked
}

/// What a converged cycle read outside itself, over every round ([`store_cycle_edges`]).
#[derive(Clone, Copy)]
struct Converged {
    /// The OR of its reads' summaries and its replayed ones', all bits when untracked.
    summary: Summary,
    /// The lowest durability it read: `LOW` when untracked, `HIGH` when it read nothing.
    durability: u32,
    /// [`UNTRACKED`] when a run of it made an untracked read, else 0.
    untracked: u32,
}

/// Hands what the run at `at` read outside its cycle, now that it ended a member of the cycle
/// headed at `head` (below it), on toward the head:
///
/// - what it carries (durability, an untracked read, replayed summaries) to the head's frame;
/// - its reads in read order: an inner head's earlier rounds' first, then its own run's (which
///   hold what the runs below it handed it, at their places), then what it was handed while it
///   ran and could not place;
/// - left on the stack as its reader's when its reader is a recording run (the next frame below
///   but barriers): its reader's read of this row comes next, so they sit where they were made,
///   and the reader, which joins the cycle by that read, hands them on in turn;
/// - else (a verify asked: the head's own, before it runs, or a row whose run may not read this
///   one) put straight into the head's gathering, after its earlier rounds: every read reaches
///   the head however the cycle was met.
///
/// So the head's edges are what the cycle read in read order (a read whose key came from an
/// earlier read comes after it), except a run met only by a verify, whose reads come after the
/// head's round they were made in.
fn hand_over(storage: &Storage, local: &mut local::Local, at: u32, head: u32) {
    debug_assert!(head < at, "a member's head is below it");
    debug_assert_eq!(at as usize + 1, local.frames.len(), "a member hands over as the top frame");
    let frame = &local.frames[at as usize];
    let start = frame.edges_start as usize;
    let (gathered, round) = (frame.cycle.start as usize, frame.cycle.round as usize);
    let durability = frame.durability.min(frame.cycle.durability);
    let untracked = frame.untracked | frame.cycle.untracked;
    let summary = or(frame.summary, frame.cycle.summary);
    let cycle = &mut local.frames[head as usize].cycle;
    cycle.durability = cycle.durability.min(durability);
    cycle.untracked |= untracked;
    cycle.summary = or(cycle.summary, summary);
    if local.cycle_edges.len() > gathered {
        // An inner head that merges: its earlier rounds before its run's reads, what it was
        // handed this round after them.
        let local::Local { edges, cycle_edges, .. } = &mut *local;
        let round = round.clamp(gathered, cycle_edges.len());
        // alloc: none once warm: the edge stack grows to its high-water mark once per thread
        edges.extend_from_slice(&cycle_edges[round..]);
        edges.splice(start..start, cycle_edges.drain(gathered..round)).for_each(drop);
        cycle_edges.truncate(gathered);
    }
    let top = local.edges.len();
    let mut below = at as usize;
    loop {
        below -= 1;
        let frame = &mut local.frames[below];
        match frame.kind {
            FrameKind::Barrier { .. } => {
                // Its pop must not cut the reads it is under.
                frame.edges_start = top as u32;
            }
            FrameKind::Run if std::ptr::eq(frame.storage, storage) => {
                // The reader's: its count of them is its own from here, so it drops their
                // repeats when it has read as many again ([`local::next_dedup`]), not at once.
                let from = frame.edges_start as usize;
                frame.dedup_at = frame.dedup_at.max(local::next_dedup(from, top - from));
                local.frames[at as usize].edges_start = top as u32;
                return;
            }
            FrameKind::Run | FrameKind::Verify => {
                give_head(local, head, start);
                return;
            }
        }
    }
}

/// Puts the reads on the stack from `start` into the gathering of the head at `head`, at its
/// end: under the gatherings of the frames above the head, which move up past them.
fn give_head(local: &mut local::Local, head: u32, start: usize) {
    let local::Local { edges, cycle_edges, frames, .. } = local;
    let given = edges.len() - start;
    if given == 0 {
        return;
    }
    let end = cycle_edges.len();
    let into = frames.get(head as usize + 1).map_or(end, |it| (it.cycle.start as usize).min(end));
    // alloc: none once warm: the cycle reads grow to their high-water mark once per thread
    cycle_edges.splice(into..into, edges[start..].iter().copied()).for_each(drop);
    for frame in &mut frames[head as usize + 1..] {
        frame.cycle.start += given as u32;
        frame.cycle.round += given as u32;
    }
}

/// Ends a round of the head at `at` (the top frame) in its gathering: this round's reads on
/// the stack (in read order, holding what its members handed it) go after its earlier rounds'
/// and before what was given it this round from verifies ([`give_head`]); then each is kept once,
/// and the next round's gifts start after. [`local::Note::Runaway`] when the gathering holds
/// more reads than one run may ([`local::RUN_READS`]): fail the head.
fn fold_round(local: &mut local::Local, at: u32) -> local::Note {
    let frame = &mut local.frames[at as usize];
    let (start, gathered, round) =
        (frame.edges_start as usize, frame.cycle.start as usize, frame.cycle.round as usize);
    let (durability, untracked, summary) = (frame.durability, frame.untracked, frame.summary);
    frame.cycle.durability = frame.cycle.durability.min(durability);
    frame.cycle.untracked |= untracked;
    frame.cycle.summary = or(frame.cycle.summary, summary);
    let local::Local { edges, cycle_edges, .. } = &mut *local;
    let round = round.max(gathered).min(cycle_edges.len());
    // alloc: none once warm: the cycle reads grow to their high-water mark once per thread
    cycle_edges.splice(round..round, edges[start..].iter().copied()).for_each(drop);
    let held = local.cycle_union(gathered).len() as u64;
    let next = local.cycle_edges.len() as u32;
    local.frames[at as usize].cycle.round = next;
    if held >= local.bounds.run_reads { local::Note::Runaway } else { local::Note::Fine }
}

/// Stores what the converged cycle headed by the run at `at` read outside itself over every
/// round, its last round folded in ([`fold_round`]), each once in read order, as the head's
/// edges, and lets go of them on the stack. Counts the head's last round's reads to its query,
/// as [`store_edges`] does a run's, with the edges it keeps.
fn store_cycle_edges(storage: &Storage, header: &Header, at: u32) -> Converged {
    let (span, ingredient, reads, note, read) = local::with(|local| {
        debug_assert_eq!(
            at as usize + 1,
            local.frames.len(),
            "a head's edges are stored while it is the top frame"
        );
        let (most, note) = local.note_peak(at as usize);
        let frame = &local.frames[at as usize];
        let (start, owner) =
            (frame.edges_start as usize, (frame.ingredient, Id::from_u32(frame.id)));
        let recorded = (local.edges.len() - start) as u64 + frame.dropped;
        local.dedup_from(start);
        let bound = fold_round(local, at);
        let note = if bound == local::Note::Runaway { bound } else { note };
        let cycle = local.frames[at as usize].cycle;
        let union = local.cycle_union(cycle.start as usize);
        let read = if cycle.untracked {
            Converged { summary: ALL, durability: 0, untracked: UNTRACKED }
        } else {
            Converged {
                summary: or(cycle.summary, storage.summary_of_edges(union)),
                durability: u32::from(cycle.durability.min(Durability::HIGH.0)),
                untracked: 0,
            }
        };
        let reads = counts::Reads { recorded, kept: union.len() as u64, most };
        let span = storage.push_edges(union, owner);
        local.cycle_edges.truncate(cycle.start as usize);
        (span, owner.0, reads, note, read)
    });
    // The head is still on top: a new mark (or its runaway) names it.
    super::noted(storage, note);
    counts::reads(ingredient, reads);
    storage.forget_edges(header.edges.swap(span, Ordering::Relaxed));
    read
}

/// A query on this task's stack (at `at`) asked for again: a cycle through `key`. The asker
/// gets the row's provisional value (its fallback or initial value on the first re-entry).
fn reenter<'db, F: Function>(
    db: &'db F::Db,
    storage: &Storage,
    row: &'db RowOf<F>,
    at: u32,
    key: F::Key,
) -> &'db F::Value {
    let header = &row.header;
    let id = key.as_id();
    if F::CYCLE == CycleKind::Panic {
        let stack = local::with(|local| {
            local.frames[at as usize..]
                .iter()
                .filter(|it| it.kind != FrameKind::Barrier { lifted: true })
                .map(|it| (it.ingredient, it.id))
                .collect::<Vec<_>>() // alloc: the panic's message
        });
        let names: Vec<String> = stack // alloc: the panic's message
            .into_iter()
            .map(|(index, id)| format!("{}({:?})", ingredient::name(index), Id::from_u32(id))) // alloc: as above
            .collect();
        panic!(
            "dependency graph cycle when querying {}({id:?}); give it `cycle_result` or `cycle_initial` to recover. Cycle: {}",
            F::NAME,
            names.join(" -> ")
        );
    }
    let word = header.verified.load(Ordering::Acquire);
    if word & PROVISIONAL == 0 {
        let again =
            local::with(|local| std::mem::replace(&mut local.frames[at as usize].reentered, true));
        assert!(!again, "the cycle recovery of {}({id:?}) asked for {} itself", F::NAME, F::NAME);
        if word & RUNNING == 0
            && header
                .verified
                .compare_exchange(word, word | RUNNING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            panic!("{}({id:?}) changed while this task held its flight", F::NAME);
        }
        let initial = match F::CYCLE {
            CycleKind::Result => blaze_mem::frame::task(F::TAG, |_| F::cycle_result(db, id, key)),
            _ => blaze_mem::frame::task(F::TAG, |_| F::cycle_initial(db, id, key)),
        };
        let table = table_of::<F>(storage);
        if word & VALUE != 0 {
            // SAFETY: the flight holder (this task's frame at `at`), `RUNNING` set, drops a
            // value of an earlier view: a cycle's values are never backdated (rules 1, 3).
            unsafe { drop_value(table, row) };
        } else {
            gained(table, id);
        }
        // SAFETY: this task holds the row's flight with `RUNNING` set, and it holds no value.
        unsafe { put(table, row, initial) };
        header.meta.store(header.meta.load(Ordering::Relaxed) | HEAD, Ordering::Relaxed);
        header.extra.store(u64::from(header.node.load(Ordering::Relaxed)), Ordering::Relaxed);
        header
            .verified
            .store((word & VIEW_MASK) | VALUE | RUNNING | PROVISIONAL, Ordering::Release);
    } else {
        local::with(|local| local.frames[at as usize].reentered = true);
    }
    read_provisional(storage, header, at);
    // SAFETY: the provisional value of this task's own cycle, between its writes (rule 4).
    unsafe { row.value() }
}

/// Counts a value `id`'s row took where it held none, and for a function kept by bytes, notes
/// the key for the index of values held ([`FnTable::fresh`]): once a value is made, never on a
/// hit.
#[inline]
fn gained<F: Function>(table: &FnTable<F>, id: Id) {
    table.values.fetch_add(1, Ordering::Relaxed);
    if F::BUDGETED {
        table.fresh.push(id.as_u32());
    }
}

/// Writes `value` into `row`, counting what it weighs to the table.
///
/// # Safety
///
/// As for [`Row::put`].
#[inline]
unsafe fn put<F: Function>(table: &FnTable<F>, row: &RowOf<F>, value: F::Value) {
    if F::BUDGETED {
        table.bytes.fetch_add(weigh::<F>(&value), Ordering::Relaxed);
    }
    // SAFETY: the caller's contract.
    unsafe { row.put(value) };
}

/// Drops `row`'s value, taking what it weighs now off the table; returns that.
///
/// # Safety
///
/// As for [`Row::drop_value`].
#[inline]
unsafe fn drop_value<F: Function>(table: &FnTable<F>, row: &RowOf<F>) -> u64 {
    let bytes = if F::BUDGETED {
        // SAFETY: the caller's contract: the row holds a value no one else reads or writes.
        let bytes = weigh::<F>(unsafe { row.value() });
        table.bytes.fetch_sub(bytes, Ordering::Relaxed);
        bytes as u64
    } else {
        0
    };
    // SAFETY: the caller's contract.
    unsafe { row.drop_value() };
    bytes
}

/// What `value` weighs, as the table counts it.
#[inline]
fn weigh<F: Function>(value: &F::Value) -> i64 {
    i64::try_from(F::weigh(value)).unwrap_or(i64::MAX)
}

fn table_of<F: Function>(storage: &Storage) -> &FnTable<F> {
    storage
        .table::<FnTable<F>>(F::ingredient().index())
        .expect("a function's table, made before its rows")
}

/// [`Vtable::reset`] of `F`: empties row `id`, never verified by its edges again.
unsafe fn reset_row<F: Function>(storage: &Storage, index: u32, id: Id) {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return };
    let Some(row) = table.pages.row::<RowOf<F>>(id) else { return };
    let header = &row.header;
    let word = header.verified.load(Ordering::Relaxed);
    debug_assert!(
        word & VALUE == 0 || word & PROVISIONAL != 0,
        "a reset of {}({id:?}), whose value is published (word {word:#x}): another task may read it",
        F::NAME,
    );
    if word & VALUE != 0 {
        // SAFETY: the caller is the thread of the row's cycle, whose frames no longer read it
        // (rule 4), or a run that unwinds holding its flight (rule 1).
        unsafe { drop_value(table, row) };
        table.values.fetch_sub(1, Ordering::Relaxed);
    }
    header.meta.store(CYCLE, Ordering::Relaxed);
    header.extra.store(0, Ordering::Relaxed);
    storage.repaint(header.node.load(Ordering::Relaxed), 0);
    // Release: the row reads as never made.
    header.verified.store(0, Ordering::Release);
}

/// Every made row of `F` (ID page 0 is never given), for a caller with `&mut` of the storage.
fn rows<'t, F: Function>(
    storage: &Storage,
    table: &'t FnTable<F>,
) -> impl Iterator<Item = (Id, &'t RowOf<F>)> {
    let pages: Vec<(usize, NonNull<u8>)> = table.pages.made(storage.pages_made()).collect(); // alloc: a sweep's or drop's page list
    pages.into_iter().flat_map(|(page, at)| {
        (0..PAGE_LEN).map(move |slot| {
            // SAFETY: a page made for `RowOf<F>` rows, `slot < PAGE_LEN`; it lives as long as
            // the storage's heap, which outlives the table `'t` borrows.
            (Id::at(page, slot), unsafe { row_at::<RowOf<F>>(at, slot) })
        })
    })
}

/// Drops the value of `row` (with `&mut`), keeping its stamp and edges and, for a run that
/// computes it again, its hash: hashed here, once per eviction, never on a run that keeps it,
/// and never for `NO_EQ`, whose every run is a change. Returns what the value weighed, or
/// `None` when the row held no final value.
fn evict<F: Function>(table: &FnTable<F>, row: &RowOf<F>) -> Option<u64> {
    let header = &row.header;
    let word = header.verified.load(Ordering::Relaxed);
    if word & VALUE == 0 || word & PROVISIONAL != 0 {
        return None;
    }
    if !F::NO_EQ {
        // SAFETY: with `&mut` of the storage no reference to the value lives (rule 3), and it
        // is whole (`VALUE`, not provisional).
        header.extra.store(F::hash(unsafe { row.value() }), Ordering::Relaxed);
        header.meta.store(header.meta.load(Ordering::Relaxed) | HASHED, Ordering::Relaxed);
    }
    // SAFETY: as above.
    let bytes = unsafe { drop_value(table, row) };
    row.header.verified.store(word & !VALUE, Ordering::Relaxed);
    Some(bytes)
}

/// Evicts every value of `F`; returns how many.
pub fn evict_values<F: Function>(db: &mut F::Db) -> usize {
    evict_table::<F>(db.storage_mut(), F::ingredient().index())
}

/// [`Explicit::evict`] of `F`, and [`evict_values`]: every value of its table at `index`.
fn evict_table<F: Function>(storage: &mut Storage, index: u32) -> usize {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return 0 };
    let evicted =
        rows::<F>(storage, table).filter(|(_, row)| evict::<F>(table, row).is_some()).count();
    table.values.fetch_sub(evicted, Ordering::Relaxed);
    counts::add(index, counts::DROPPED, evicted as u64);
    evicted
}

/// [`Vtable::unread`] of `F`: lets go of each value whose row was last verified in a view
/// before `before`. A row is stamped with the view of each read of it (a hit needs
/// `verified == clock`, and the cold path stamps what it verifies or runs), so such a value was
/// read in no view since. A walk of every row made, for a caller with `&mut` of the storage.
fn evict_unread<F: Function>(storage: &Storage, index: u32, before: u32) -> (usize, u64) {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return (0, 0) };
    let (mut evicted, mut freed) = (0usize, 0u64);
    for (_, row) in rows::<F>(storage, table) {
        let word = row.header.verified.load(Ordering::Relaxed);
        if word & VALUE == 0 || word & VIEW_MASK >= before {
            continue;
        }
        if let Some(bytes) = evict::<F>(table, row) {
            evicted += 1;
            freed += bytes;
        }
    }
    table.values.fetch_sub(evicted, Ordering::Relaxed);
    if evicted != 0 {
        counts::add(index, counts::EVICTED, evicted as u64);
    }
    (evicted, freed)
}

/// [`Explicit::values`] of `F`: how many of its rows hold a value, from its counter.
fn explicit_values<F: Function>(storage: &Storage, index: u32) -> usize {
    storage.table::<FnTable<F>>(index).map_or(0, |table| table.values.load(Ordering::Relaxed))
}

/// Evicts the values of `keys` of `F`; returns how many held one.
pub fn evict_values_of<F: Function>(db: &mut F::Db, keys: &[F::Key]) -> usize {
    let storage = db.storage_mut();
    let Some(table) = storage.table::<FnTable<F>>(F::ingredient().index()) else { return 0 };
    let evicted = keys
        .iter()
        .filter_map(|key| table.pages.row::<RowOf<F>>(key.as_id()))
        .filter(|row| evict::<F>(table, row).is_some())
        .count();
    table.values.fetch_sub(evicted, Ordering::Relaxed);
    counts::add(F::ingredient().index(), counts::DROPPED, evicted as u64);
    evicted
}

/// [`Weighed::weight`] of `F`: what its values weigh as counted, after folding the keys that
/// took a value since into its index, which is cut back to the keys holding one when it has
/// grown to twice their count. Costs the values made since, never the values held.
fn weight<F: Function>(storage: &Storage, index: u32) -> u64 {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return 0 };
    let mut held = table.held.lock();
    while let Some(key) = table.fresh.pop() {
        held.push(key); // alloc: the budgeted function's index, a key per value made
    }
    if held.len() > 2 * table.values.load(Ordering::Relaxed) + 1024 {
        cut_to_held(table, &mut held);
    }
    u64::try_from(table.bytes.load(Ordering::Relaxed)).unwrap_or(0)
}

/// Sorts `keys` and cuts them to the keys of `F` whose rows hold a value, each once.
fn cut_to_held<F: Function>(table: &FnTable<F>, keys: &mut Vec<u32>) {
    keys.sort_unstable();
    keys.dedup();
    keys.retain(|&key| {
        table
            .pages
            .row::<RowOf<F>>(Id::from_u32(key))
            .is_some_and(|row| row.header.verified.load(Ordering::Relaxed) & VALUE != 0)
    });
}

/// [`Weighed::held`] of `F`: every final value it holds, weighed now, onto `out`; their sum
/// becomes the table's weight (what grew since it was weighed is counted from here on).
fn held_values<F: Function>(storage: &Storage, index: u32, out: &mut Vec<Held>) -> u64 {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return 0 };
    let mut held = table.held.lock();
    while let Some(key) = table.fresh.pop() {
        held.push(key); // alloc: as in `weight`
    }
    cut_to_held(table, &mut held);
    let mut sum: u64 = 0;
    for &key in held.iter() {
        let Some(row) = table.pages.row::<RowOf<F>>(Id::from_u32(key)) else { continue };
        let word = row.header.verified.load(Ordering::Relaxed);
        if word & PROVISIONAL != 0 {
            continue;
        }
        // SAFETY: with `&mut` of the storage (the view boundary) no reference to the value
        // lives (rule 3), and it is whole (`VALUE`, not provisional).
        let bytes = F::weigh(unsafe { row.value() }) as u64;
        sum += bytes;
        // alloc: the pass's values, once per eighth of the tag's budget made
        out.push(Held { view: word & VIEW_MASK, ingredient: index, key, bytes });
    }
    table.bytes.store(i64::try_from(sum).unwrap_or(i64::MAX), Ordering::Relaxed);
    sum
}

/// Evictions a pass spreads over the pool: below it, one thread evicts them.
const PAR_EVICT: usize = 256;

/// [`Weighed::evict`] of `F`: evicts the values of `gone` (keys of `F`, each once); returns how
/// many held one and what they weighed. An eviction hashes its value ([`evict`]), which for a
/// big one (a macro expansion's parse and span map) is most of a pass, so many go on the pool
/// at once: the rows are distinct, and the caller's `&mut` of the storage keeps every reader
/// out while the loop runs. A span names the function and the count, so a write that evicts
/// much says so.
fn evict_held<F: Function>(storage: &Storage, index: u32, gone: &[Held]) -> (usize, u64) {
    if gone.is_empty() {
        return (0, 0);
    }
    let _span =
        tracing::info_span!(target: "blaze", "lru evict", function = F::NAME, count = gone.len())
            .entered();
    let Some(table) = storage.table::<FnTable<F>>(index) else { return (0, 0) };
    let one = |table: &FnTable<F>, value: &Held| {
        table.pages.row::<RowOf<F>>(Id::from_u32(value.key)).and_then(|row| evict::<F>(table, row))
    };
    let (evicted, freed) = if gone.len() < PAR_EVICT {
        gone.iter().filter_map(|value| one(table, value)).fold((0, 0), |(n, b), it| (n + 1, b + it))
    } else {
        let evicted = AtomicUsize::new(0);
        let freed = AtomicU64::new(0);
        super::pump_barrier(|| {
            blaze_graph::par::for_each(gone, |value| {
                let table = storage.table::<FnTable<F>>(index).expect("the table this pass reads");
                if let Some(bytes) = one(table, value) {
                    evicted.fetch_add(1, Ordering::Relaxed);
                    freed.fetch_add(bytes, Ordering::Relaxed);
                }
            })
        });
        (evicted.into_inner(), freed.into_inner())
    };
    table.values.fetch_sub(evicted, Ordering::Relaxed);
    if evicted != 0 {
        counts::add(index, counts::EVICTED, evicted as u64);
    }
    (evicted, freed)
}

/// [`Vtable::restamp`] of `F`: each row's verified view and `changed`.
fn restamp<F: Function>(storage: &Storage, index: u32, restamp: &mut Restamp<'_>) {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return };
    for (_, row) in rows::<F>(storage, table) {
        let header = &row.header;
        let word = header.verified.load(Ordering::Relaxed);
        debug_assert!(word & (RUNNING | PROVISIONAL) == 0, "a row running across a `&mut`");
        let verified = restamp.apply(word & VIEW_MASK);
        header.verified.store((word & !VIEW_MASK) | verified, Ordering::Relaxed);
        let changed = restamp.apply(header.changed.load(Ordering::Relaxed));
        header.changed.store(changed, Ordering::Relaxed);
    }
}

/// [`Vtable::memory`] of `F`: rows made and their bytes, a value in place included, and what a
/// budgeted function's values hold beside their rows; its rows in use are [`used`]'s, a walk.
/// Two loads a table, no row read.
fn memory<F: Function>(storage: &Storage, index: u32) -> MemoryUsage {
    let (made, weighed) = storage.table::<FnTable<F>>(index).map_or((0, 0), |table| {
        (table.pages.count(), u64::try_from(table.bytes.load(Ordering::Relaxed)).unwrap_or(0))
    });
    MemoryUsage {
        name: F::NAME,
        rows: (made * PAGE_LEN) as u64,
        used: 0,
        row_bytes: (made * PAGE_LEN * size_of::<RowOf<F>>()) as u64,
        // A budgeted function's values say what they hold beside themselves ([`Function::weigh`]).
        value_bytes: weighed,
    }
}

/// [`Vtable::used`] of `F`: its rows that ran, each holding a node from its first run
/// ([`node_of`]) to the storage's end; an evicted value's row still does. A walk of every row
/// made, one load of each header: a page's directory entry is loaded with acquire
/// ([`Pages::made`]), so its zeroes are seen, and a header is atomics, so a run racing the walk
/// is read whole or not yet.
fn used<F: Function>(storage: &Storage, index: u32) -> u64 {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return 0 };
    let ran = |at: NonNull<u8>| {
        (0..PAGE_LEN)
            .filter(|&slot| {
                // SAFETY: a page made for `RowOf<F>` rows, `slot < PAGE_LEN`, read while the
                // storage `storage` borrows lives.
                let header = unsafe { row::header_at(at, slot, size_of::<RowOf<F>>()) };
                header.node.load(Ordering::Relaxed) != 0
            })
            .count() as u64
    };
    table.pages.made(storage.pages_made()).map(|(_, at)| ran(at)).sum()
}

/// [`Vtable::relocate`] of `F`.
fn relocate<F: Function>(storage: &mut Storage, index: u32, moved: &mut Relocate) {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return };
    for (_, row) in rows::<F>(storage, table) {
        let span = row.header.edges.load(Ordering::Relaxed);
        if span != 0 {
            row.header.edges.store(storage.relocate(moved, span), Ordering::Relaxed);
        }
    }
}

/// [`Vtable::drop`] of `F`: drops every value and the table.
fn drop_table<F: Function>(storage: &mut Storage, index: u32) {
    let Some(table) = storage.take_table::<FnTable<F>>(index) else { return };
    for (_, row) in rows::<F>(storage, &table) {
        if row.header.verified.load(Ordering::Relaxed) & VALUE != 0 {
            // SAFETY: the storage is being dropped: nothing reads the value (rule 3).
            unsafe { row.drop_value() };
        }
    }
}

/// The ID of the storage's running query, if any: what a tracked struct's identity names.
pub(crate) fn running(storage: &Storage) -> Option<DatabaseKeyIndex> {
    local::with(|local| {
        local
            .recording(storage)
            .map(|frame| DatabaseKeyIndex::new(frame.ingredient, Id::from_u32(frame.id)))
    })
}

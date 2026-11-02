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

use std::{
    cell::Cell,
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use blaze_mem::Tag;

use super::{
    AsId, Cancelled, Cycle, Database, DatabaseKeyIndex, EventKind, FromIdIn, Id, MemoryUsage,
    counts,
    id::PAGE_LEN,
    ingredient::{self, Ingredient, Kind, Vtable},
    local::{self, Frame, Kind as FrameKind, Member, NONE, Pending},
    provenance::Cause,
    row::{
        self, CYCLE, DURABILITY, HASHED, HEAD, Header, PROVISIONAL, Pages, RUNNING, Row, STALE,
        UNTRACKED, VALUE, VIEW_MASK, row_at, span,
    },
    storage::{
        ALL, Durability, FIELD, Relocate, Restamp, Storage, Table, color, key_parts, node_key, or,
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
    /// The value cap: past it, the oldest-verified values are evicted at a new view. 0: none.
    const LRU: usize;
    /// `lru = N` given (0 too): the values may be evicted, and without `NO_EQ` `hash` is real.
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
}

/// A function's table: its rows, how many hold a value, and its `lru` cap (0: none), read
/// only by the sweep.
pub(crate) struct FnTable<F: Function> {
    pages: Pages,
    values: AtomicUsize,
    cap: AtomicUsize,
    _of: PhantomData<fn() -> F>,
}

impl<F: Function> Table for FnTable<F> {
    fn new(storage: &Storage, index: u32) -> FnTable<F> {
        storage.check_db::<F::Db>(F::NAME);
        if F::EVICTABLE {
            storage.add_lru(index);
        }
        FnTable {
            pages: Pages::new(Tag::Query),
            values: AtomicUsize::new(0),
            cap: AtomicUsize::new(F::LRU),
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
            verify: Some(verify_edge::<F> as unsafe fn(*const (), &Storage, Id) -> Verified),
            reset: Some(reset_row::<F> as unsafe fn(&Storage, u32, Id)),
            sweep: if F::EVICTABLE { Some(sweep::<F> as fn(&mut Storage, u32)) } else { None },
            relocate: Some(relocate::<F> as fn(&mut Storage, u32, &mut Relocate)),
            restamp: Some(restamp::<F> as fn(&Storage, u32, &mut Restamp<'_>)),
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
        // SAFETY: a published directory is committed whole for `MAX_PAGES` entries
        // (`Pages::new`), and `id.page() < MAX_PAGES` (`Id`'s invariant); it lives as long
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

/// Records a read of a row's current value into the running query, if any.
#[inline(always)]
fn read(storage: &Storage, header: &Header) {
    local::with(|local| {
        if local.frames.is_empty() {
            return;
        }
        let node = header.node.load(Ordering::Relaxed);
        local.read(storage, node, header.durability(), header.changed.load(Ordering::Relaxed));
    });
}

/// A read of a provisional row of the cycle headed at stack index `head`.
fn read_provisional(storage: &Storage, header: &Header, head: u32) {
    local::with(|local| {
        let node = header.node.load(Ordering::Relaxed);
        local.read(storage, node, header.durability(), header.changed.load(Ordering::Relaxed));
        if let Some(frame) = local.recording(storage) {
            frame.head = frame.head.min(head);
            frame.unshareable = true;
        }
    });
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
/// sweep or drop can name.
fn row_or_make<F: Function>(storage: &Storage, id: Id) -> (&RowOf<F>, u32) {
    let (table, index) = storage.table_or_make::<FnTable<F>>(F::ingredient());
    if storage.owner(id).is_none() {
        never_given(index, id);
    }
    let (size, align) = row::layout::<RowOf<F>>();
    let page = table.pages.page_or_make(id.page(), &storage.heap, size, align);
    // SAFETY: the page was made for `RowOf<F>` rows; `id.slot() < PAGE_LEN`.
    (unsafe { row_at::<RowOf<F>>(page, id.slot()) }, index)
}

#[cold]
#[inline(never)]
fn never_given(index: u32, id: Id) -> ! {
    panic!(
        "`{}` was asked for the key {id:?}, an ID this database never gave (its page {} has no \
         owner): a key forged, or decoded without checking its page's owner",
        ingredient::name(index),
        id.page(),
    )
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
    loop {
        if storage.stopped() {
            Cancelled::PendingWrite.throw();
        }
        let view = storage.view_bits();
        let word = header.verified.load(Ordering::Acquire);
        if word & (RUNNING | PROVISIONAL) == 0
            && word & VIEW_MASK == view
            && (word & VALUE != 0 || !value)
        {
            return State::Current;
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
        || storage.inputs_unchanged(node, word & VIEW_MASK)
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
            local::with(|local| {
                // alloc: none once warm: the pending list grows to its high-water mark once per thread
                local.pending.push(Pending {
                    header: std::ptr::from_ref(header),
                    word,
                    node,
                    ingredient: index,
                    id: id.as_u32(),
                });
                local.pop(at);
            });
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
    while let Some(pending) = local::with(|local| {
        (local.pending.len() as u32 > from).then(|| local.pending.pop()).flatten()
    }) {
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
        let mut seen = storage.color_of(node);
        let view = storage.view_bits();
        let painted = (seen >> 32) as u32;
        if painted < FIELD && painted != view {
            let (index, id, _) = key_parts(storage.key_of(node));
            let Some(verify) = ingredient::vtable(index).and_then(|it| it.verify) else {
                return Some(node);
            };
            // SAFETY: `db` is this storage's database as every function of it names it
            // (`Storage::check_db`), and `id` is a key of ingredient `index` (its node's key).
            match unsafe { verify(db, storage, id) } {
                Verified::Changed => return Some(node),
                Verified::Assumed => {
                    summary = or(summary, storage.summary_of(node));
                    continue;
                }
                Verified::Current => {}
            }
            seen = storage.color_of(node);
            if (seen >> 32) as u32 != view {
                return Some(node);
            }
        }
        if seen as u32 > verified {
            return Some(node);
        }
        summary = or(summary, storage.summary_of(node));
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
            local.pending.truncate(pending as usize);
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

/// Empties the cycle members from `from` on the member list: a cycle's unwinding.
fn empty_members(storage: &Storage, from: u32) {
    loop {
        let Some(member) = local::with(|local| {
            (local.members.len() as u32 > from).then(|| local.members.pop()).flatten()
        }) else {
            return;
        };
        reset_member(storage, member);
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
    // SAFETY: as in `fetch`: a published directory, committed whole.
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
        let new = blaze_mem::frame::task(F::TAG, |_| F::execute(db, key));
        crate::ran_one();
        let done = ran(at);
        if done.head == NONE {
            finish::<F>(storage, row, &done, at, word, new, keep);
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

/// Stores the run's edges as the row's, forgetting its old ones.
fn store_edges(storage: &Storage, header: &Header, at: u32) {
    let span = local::with(|local| {
        debug_assert_eq!(at as usize + 1, local.frames.len(), "a run's edges are stored while it is the top frame");
        let start = local.frames[at as usize].edges_start as usize;
        local.dedup_from(start);
        storage.push_edges(&local.edges[start..])
    });
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
        unsafe { row.drop_value() };
    } else {
        table.values.fetch_add(1, Ordering::Relaxed);
    }
    let meta = durability | if done.untracked { UNTRACKED } else { 0 };
    // SAFETY: the flight holder, `RUNNING` set; the value was dropped above (rule 1).
    unsafe { row.put(new) };
    store_edges(storage, header, at);
    summarize(storage, done.node, at, done.untracked);
    header.changed.store(changed, Ordering::Relaxed);
    header.meta.store(meta, Ordering::Relaxed);
    storage.paint(done.node, color(view, changed));
    // Release: the value, stamp and edges are published with the word (rule 2).
    header.verified.store(view | VALUE, Ordering::Release);
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
        unsafe { row.drop_value() };
    } else {
        table.values.fetch_add(1, Ordering::Relaxed);
    }
    // SAFETY: as above, the row holds no value now.
    unsafe { row.put(value) };
    store_edges(storage, header, at);
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
    // SAFETY: the head replaces its own provisional value between rounds (rule 4).
    unsafe { row.drop_value() };
    // SAFETY: as above; the row holds no value now.
    unsafe { row.put(value) };
    let view = storage.view_bits();
    if converged {
        let durability = finalize(storage, done.members, durability_of(done));
        store_edges(storage, header, at);
        storage.set_summary(done.node, ALL);
        header.changed.store(view, Ordering::Relaxed);
        header.meta.store(durability | CYCLE, Ordering::Relaxed);
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
    next_round(storage, done.members);
    local::with(|local| {
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
        debug_assert!(edges >= start);
        local.edges.truncate(start);
        local.renew_serial(at);
    });
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
/// durability of the cycle on each, a new stamp, never verified by edges again.
fn finalize(storage: &Storage, from: u32, durability: u32) -> u32 {
    let members = local::with(|local| local.members.split_off(from as usize)); // alloc: a cycle's end, once
    let view = storage.view_bits();
    let mut lowest = durability;
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
        storage.set_summary(node, ALL);
        header.changed.store(view, Ordering::Relaxed);
        header.meta.store(lowest | CYCLE, Ordering::Relaxed);
        header.extra.store(0, Ordering::Relaxed);
        storage.paint(node, color(view, view));
        // Release: the member's value is final in this view (rule 2).
        header.verified.store(view | VALUE, Ordering::Release);
    }
    lowest
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
            unsafe { row.drop_value() };
        } else {
            table.values.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: this task holds the row's flight with `RUNNING` set, and it holds no value.
        unsafe { row.put(initial) };
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
    if header.verified.load(Ordering::Relaxed) & VALUE != 0 {
        // SAFETY: the caller is the thread of the row's cycle, whose frames no longer read it
        // (rule 4), or a run that unwinds holding its flight (rule 1).
        unsafe { row.drop_value() };
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
/// and never for `NO_EQ`, whose every run is a change.
fn evict<F: Function>(row: &RowOf<F>) -> bool {
    let header = &row.header;
    let word = header.verified.load(Ordering::Relaxed);
    if word & VALUE == 0 || word & PROVISIONAL != 0 {
        return false;
    }
    if !F::NO_EQ {
        // SAFETY: with `&mut` of the storage no reference to the value lives (rule 3), and it
        // is whole (`VALUE`, not provisional).
        header.extra.store(F::hash(unsafe { row.value() }), Ordering::Relaxed);
        header.meta.store(header.meta.load(Ordering::Relaxed) | HASHED, Ordering::Relaxed);
    }
    // SAFETY: as above.
    unsafe { row.drop_value() };
    row.header.verified.store(word & !VALUE, Ordering::Relaxed);
    true
}

/// Evicts every value of `F`; returns how many.
pub fn evict_values<F: Function>(db: &mut F::Db) -> usize {
    let storage = db.storage_mut();
    let Some(table) = storage.table::<FnTable<F>>(F::ingredient().index()) else { return 0 };
    let evicted = rows::<F>(storage, table).filter(|(_, row)| evict::<F>(row)).count();
    table.values.fetch_sub(evicted, Ordering::Relaxed);
    evicted
}

/// Evicts the values of `keys` of `F`; returns how many held one.
pub fn evict_values_of<F: Function>(db: &mut F::Db, keys: &[F::Key]) -> usize {
    let storage = db.storage_mut();
    let Some(table) = storage.table::<FnTable<F>>(F::ingredient().index()) else { return 0 };
    let evicted = keys
        .iter()
        .filter_map(|key| table.pages.row::<RowOf<F>>(key.as_id()))
        .filter(|row| evict::<F>(row))
        .count();
    table.values.fetch_sub(evicted, Ordering::Relaxed);
    evicted
}

/// [`Vtable::sweep`] of `F`: past its cap, the values verified longest ago go.
fn sweep<F: Function>(storage: &mut Storage, index: u32) {
    let Some(table) = storage.table::<FnTable<F>>(index) else { return };
    let held = table.values.load(Ordering::Relaxed);
    let cap = table.cap.load(Ordering::Relaxed);
    if cap == 0 || held <= cap {
        return;
    }
    let mut ages: Vec<(u32, Id)> = rows::<F>(storage, table) // alloc: a sweep past the cap, at a view boundary
        .filter_map(|(id, row)| {
            let word = row.header.verified.load(Ordering::Relaxed);
            (word & VALUE != 0 && word & PROVISIONAL == 0).then_some((word & VIEW_MASK, id))
        })
        .collect(); // alloc: as above
    let over = ages.len().saturating_sub(cap);
    if over == 0 {
        return;
    }
    ages.select_nth_unstable(over - 1);
    let mut evicted = 0;
    for &(_, id) in &ages[..over] {
        if let Some(row) = table.pages.row::<RowOf<F>>(id)
            && evict::<F>(row)
        {
            evicted += 1;
        }
    }
    table.values.fetch_sub(evicted, Ordering::Relaxed);
}

/// Replaces `F`'s `lru` cap (0: none), applied at the next view's sweep.
///
/// # Panics
///
/// If `F` has no `lru`.
pub fn set_lru_capacity<F: Function>(db: &mut F::Db, capacity: usize) {
    assert!(F::EVICTABLE, "{} has no `lru`: its cap can't be set", F::NAME);
    let (table, _) = db.storage_mut().table_or_make::<FnTable<F>>(F::ingredient());
    table.cap.store(capacity, Ordering::Relaxed);
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

/// [`Vtable::memory`] of `F`: rows made, and the bytes of rows and of the values in place.
fn memory<F: Function>(storage: &Storage, index: u32) -> MemoryUsage {
    let made = storage
        .table::<FnTable<F>>(index)
        .map_or(0, |table| table.pages.made(storage.pages_made()).count());
    let values =
        storage.table::<FnTable<F>>(index).map_or(0, |table| table.values.load(Ordering::Relaxed));
    MemoryUsage {
        name: F::NAME,
        rows: (made * PAGE_LEN) as u64,
        row_bytes: (made * PAGE_LEN * size_of::<RowOf<F>>()) as u64,
        value_bytes: (values * size_of::<F::Value>()) as u64,
    }
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

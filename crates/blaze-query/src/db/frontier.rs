//! The changed set: after writes, which rows' values changed, found from what was written
//! through the reverse edges with early cutoff, at a cost of the rows the change reaches
//! (docs/v2/plan/design/incremental-walk.md, "The changed set from the engine").
//!
//! This is the driver; the reverse index and the walk's bookkeeping are the core
//! ([`super::reverse`]).
//!
//! What feeds it, each one push where the engine already does the work, never on a hit:
//! - a run that stores its edges says so ([`Frontier::stored`]); the next view boundary folds
//!   them into the reverse index ([`Storage::fold_reverse`], with `&mut`: no reader, and
//!   before the edge array is settled, so the spans are still there);
//! - an input field written says so ([`Storage::note_written`]);
//! - a row whose stamp moves to the view (its run was not backdated; a cycle's rows; a row
//!   emptied), having had a value before, and a tracked field rewritten with another value,
//!   say so ([`Frontier::changed`]);
//! - a run that made an untracked read says so ([`Frontier::untracked`]): such a row runs
//!   again in every view, so every walk asks it.
//!
//! The walk ([`walk`], one at a time under the storage's flight of it): from the fields
//! written since the last walk and every node that said it changed, the readers are verified
//! as any reader's edge is (`refresh` without the value: run again where a read changed,
//! backdated where the value is equal); whatever changes says so and the walk goes on to its
//! readers; a row that verifies unchanged stops it. It ends when nothing more changed.
//!
//! Why no changed row is missed. Write `L` for the view of the last walk. Held at every walk's
//! end, for every row made before it: the row is verified in that view, or a pull of it would
//! find it unchanged (no read of it, transitively, changed since it was verified). A row can
//! only change by running again, and it runs again only for a read that changed after it was
//! verified, an untracked read, or having been emptied. Take a row that changes after `L`
//! and the first changed read `d` of its stored edges: by the invariant `d` changed after `L`,
//! so `d` is a written field or itself said it changed, by induction is in the walk, and the
//! row is among `d`'s readers ([`super::reverse`]'s invariant; a run re-reads its edges up to
//! and including the first changed one, so the edge to `d` survives the run, and an edge is
//! never taken out of the index). So the walk verifies the row, and it says it changed.
//!
//! A row the walk verifies may be one nothing asks for any more (its key names something an
//! edit removed): a pull would never run it. So a verification that unwinds for any reason
//! but the caller's own (a write pending, its fuel) is contained: the engine's guard puts the
//! row's word back, the walk counts it, names it as changed and goes on, and does not ask it
//! again until something else brought it up to date ([`Walked::unwound`]).
//!
//! Persistence: none. The storage is memory (a restart makes another storage), and the index
//! is made from the rows' own edges: empty with an empty storage, it holds the invariant from
//! the first run on.

use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
};

use crossbeam_queue::SegQueue;

use super::{
    Cancelled, FromIdIn, Id, OutOfFuel, OutOfSteps,
    function::{self, Function, Verified},
    ingredient::{self, Kind},
    local,
    reverse::{Plan, Reverse, Seen},
    row::{Header, PROVISIONAL, RUNNING, UNTRACKED, VALUE, VIEW_MASK, span},
    storage::{Database, Storage, View, key_parts},
};

/// The edges one run stored: its row's node, the span it held before (0 for none) and the
/// span it holds now.
struct Stored {
    node: u32,
    old: u64,
    new: u64,
}

/// What one walk found: the view it ran in ([`Storage::view`]'s numbering) and the nodes that
/// changed, sorted.
struct Found {
    at: u64,
    nodes: Vec<u32>,
}

/// The most nodes the walks' findings keep, and the most walks: past either the oldest go,
/// and a host that asks from before them is told so ([`Unknown::Before`]).
const LOG_NODES: usize = 1 << 20;
const LOG_WALKS: usize = 256;

/// The rows one level of a walk verifies before it hands them to the pool.
const PAR_AT: usize = 64;

/// What the walks keep between them. Its vectors are taken out by the walker (the holder of
/// the walk's flight) and put back, so the lock is held for moves only.
#[derive(Default)]
struct Walked {
    /// The view the last walk ended in.
    done: u64,
    /// [`changed_since`] answers a view at or after this.
    floor: u64,
    log: Arc<Vec<Arc<Found>>>,
    /// The field nodes written since the last walk.
    written: Vec<u32>,
    /// The rows whose last run made an untracked read, sorted.
    untracked: Vec<u32>,
    /// Rows whose verification by a walk unwound, each with its `verified` word then: not
    /// asked again by a walk while the word stands, named as changed whenever reached, and
    /// never taken as clean ([`clean`]). Replaced whole by a walk that ended.
    unwound: Arc<Vec<(u32, u32)>>,
    work: ChangedSetWork,
}

/// What the last walk of a storage did: what a host puts on its span, and what a test holds
/// to the edit. A regression shows here first: `visited` or `edges` growing with the program
/// where the edit did not.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct ChangedSetWork {
    /// The view it ran in.
    pub view: u64,
    /// Field nodes written since the walk before.
    pub written: u64,
    /// Rows asked because their last run made an untracked read.
    pub untracked: u64,
    /// Rows it verified.
    pub visited: u64,
    /// Reverse edges it followed.
    pub edges: u64,
    /// Nodes that changed: the changed set's size, fields included.
    pub changed: u64,
    /// Verifications that unwound and were contained.
    pub unwound: u64,
}

/// A bug planted in the walk or its feeds, for the simulation's controls: each is one
/// safeguard taken out, which the oracle must catch (rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Plant {
    #[default]
    None,
    /// The fold drops the reverse edges to every second node.
    DropsAnEdge,
    /// The fold keeps a row's first edges and ignores what a rerun stored.
    KeepsOldEdges,
    /// The walk verifies the readers of what was written and stops: a changed reader does
    /// not pass it on.
    StopsAtChangedReader,
    /// A run that was backdated says it changed: the cutoff is not applied.
    NamesEqualRows,
    /// The walk passes over a row whose value was evicted.
    SkipsEvictedRows,
}

/// The storage's part of the changed set.
pub(crate) struct Frontier {
    stored: SegQueue<Stored>,
    changed: SegQueue<u32>,
    untracked: SegQueue<u32>,
    /// Read by a walk, written at a view boundary (`&mut`).
    reverse: Reverse,
    #[expect(clippy::disallowed_types, reason = "held to move the walks' vectors in and out, never across a call")]
    walked: blaze_core::lock::Lock<Walked>,
    /// The view the last walk ended in ([`Storage::view`]'s number), 0 before any: while it
    /// is the storage's view, a row not verified in it is unchanged ([`clean`]).
    clean_at: AtomicU64,
    /// How many rows [`Walked::unwound`] holds.
    unwound: AtomicU32,
    plant: Plant,
}

impl Frontier {
    #[expect(clippy::disallowed_types, reason = "makes the lock its field allows")]
    pub(crate) fn new() -> Frontier {
        Frontier {
            stored: SegQueue::new(),
            changed: SegQueue::new(),
            untracked: SegQueue::new(),
            reverse: Reverse::default(),
            walked: blaze_core::lock::Lock::new("query.frontier.walked", Walked::default()),
            clean_at: AtomicU64::new(0),
            unwound: AtomicU32::new(0),
            plant: Plant::None,
        }
    }

    /// The row of `node` stored the span `new` where it held `old`: once a run.
    #[inline]
    pub(crate) fn stored(&self, node: u32, old: u64, new: u64) {
        if old != new {
            self.stored.push(Stored { node, old, new });
        }
    }

    /// `node`'s stamp moved to this view. For a row, `word` is its `verified` word before the
    /// run: a row never made before (no view in it) changed no value anyone holds, and is not
    /// named.
    #[inline]
    pub(crate) fn changed_row(&self, node: u32, word: u32) {
        if word & VIEW_MASK != 0 && node != 0 {
            self.changed.push(node);
        }
    }

    /// A tracked field's node was rewritten with another value.
    #[inline]
    pub(crate) fn changed_field(&self, node: u32) {
        self.changed.push(node);
    }

    /// The run of `node` made an untracked read.
    #[inline]
    pub(crate) fn untracked(&self, node: u32) {
        self.untracked.push(node);
    }

    /// Whether a backdated run says it changed all the same ([`Plant::NamesEqualRows`]).
    #[inline]
    pub(crate) fn names_equal_rows(&self) -> bool {
        self.plant == Plant::NamesEqualRows
    }

    /// The bytes the reverse index has reserved.
    pub(crate) fn bytes(&self) -> u64 {
        self.reverse.bytes()
    }

    /// The readers the reverse index holds.
    pub(crate) fn held(&self) -> u64 {
        self.reverse.held()
    }
}

impl Storage {
    /// Plants `plant` in this storage's changed set, for a simulation's control.
    #[cfg(test)]
    pub(crate) fn plant_in_changed_set(&mut self, plant: Plant) {
        self.frontier.plant = plant;
    }

    /// An input field's node was written (with `&mut`: its setter).
    pub(crate) fn note_written(&mut self, node: u32) {
        self.frontier.walked.get_mut().written.push(node); // alloc: the writes since the last walk, a word each
    }

    /// What the last changed-set walk of this storage did.
    pub fn changed_set_work(&self) -> ChangedSetWork {
        self.frontier.walked.lock().work
    }

    /// Folds the edges runs stored since the last view boundary into the reverse index, with
    /// `&mut` (no run in flight) and before [`Storage::settle`] moves the edge array (a span a
    /// run replaced is dead but still in place). Costs the edges of the rows that ran. When
    /// the index holds more than twice the edges the rows name (and a margin of the nodes),
    /// stale readers are most of it, and it is made again from every row: at most the readers
    /// it frees.
    pub(crate) fn fold_reverse(&mut self) {
        if self.frontier.stored.is_empty() {
            return;
        }
        let plant = self.frontier.plant;
        let mut reverse = std::mem::take(&mut self.frontier.reverse);
        // Named, so a view's time folding is its own, never its writer's self time (rule 16).
        let fold = tracing::info_span!(
            target: "blaze",
            "reverse edges fold",
            view = self.view_bits(),
            runs = tracing::field::Empty,
            readers = tracing::field::Empty,
            remade = tracing::field::Empty,
        );
        let _entered = fold.enter();
        let mut seen = Seen::with_hasher(blaze_hash::FixedState);
        let (mut old, mut new) = (Vec::new(), Vec::new()); // alloc: none until a run is folded, then scratch of one row's edges
        let mut runs = 0u64;
        while let Some(stored) = self.frontier.stored.pop() {
            runs += 1;
            if plant == Plant::KeepsOldEdges && stored.old != 0 {
                continue;
            }
            self.copy_edges(stored.old, &mut old);
            self.copy_edges(stored.new, &mut new);
            if plant == Plant::DropsAnEdge {
                new.retain(|dep| dep % 2 == 1);
            }
            reverse.stored(stored.node, &old, &new, &mut seen);
        }
        let nodes = u64::from(self.nodes_made());
        let remade = plant == Plant::None && reverse.held() > 2 * self.live_edges() + nodes + (1 << 16);
        if remade {
            reverse.clear();
            for node in 1..self.nodes_made() {
                let Some((header, _, _)) = row_of(self, node) else { continue };
                self.copy_edges(header.edges.load(Ordering::Relaxed), &mut new);
                reverse.stored(node, &[], &new, &mut seen);
            }
        }
        fold.record("runs", runs);
        fold.record("readers", reverse.held());
        fold.record("remade", remade);
        self.frontier.reverse = reverse;
    }

    /// The nodes of the edge span `span`, into `out` (emptied first).
    fn copy_edges(&self, span: u64, out: &mut Vec<u32>) {
        out.clear();
        let (start, len) = (span::start(span), span::len(span));
        out.extend((start..start + len).map(|at| self.edge(at))); // alloc: the fold's scratch, one row's edges
    }
}

/// The header of the function row `node` names, with its ingredient and key; `None` for a
/// field's node, a node with no key yet, or a row whose page is gone.
fn row_of(storage: &Storage, node: u32) -> Option<(&Header, u32, Id)> {
    let key = storage.key_of(node);
    if key == 0 {
        return None;
    }
    let (index, id, _) = key_parts(key);
    if ingredient::vtable(index)?.kind != Kind::Function {
        return None;
    }
    Some((function::header_of(storage, index, id)?, index, id))
}

/// The keys of `F` whose value changed since a view, or why the storage cannot say.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Changed<K> {
    /// Every key of `F` whose row held a value in the view asked from and whose value now is
    /// another, in key order, each once. It may hold more, and only these: a key whose value
    /// changed and changed back, one a walk could not verify (its verification unwound), and
    /// one that changed in a view at or before the one asked from when no walk ran between
    /// them. Each row named is up to date in this view (or its verification unwound). A row
    /// first made after the view asked from is not named: a host asks a key it never held.
    Keys(Vec<K>),
    Unknown(Unknown),
}

/// Why [`changed_since`] cannot name the changed keys: the host takes every key it holds as
/// changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unknown {
    /// The view asked from is before what the walks' findings keep (they keep the last
    /// [`LOG_WALKS`] walks and [`LOG_NODES`] nodes): `floor` is the earliest view they answer.
    Before { floor: View },
    /// The view asked from is not one of this storage's: it is after its current view.
    NotAView { view: View },
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unknown::Before { floor } => write!(
                f,
                "the view asked from is before view {}, the earliest the changed sets kept answer",
                floor.as_u64()
            ),
            Unknown::NotAView { view } => {
                write!(f, "the view asked from is after the storage's view {}", view.as_u64())
            }
        }
    }
}

/// The keys of `F` whose value changed since view `since` ([`Storage::view`], read when the
/// caller last held `F`'s values): the changed set, brought up to date first by a walk from
/// what was written ([`walk`]) when one is due. For a caller outside any query; records no
/// read.
///
/// The key of what a host keeps by it: the storage (a host keeps `since` with which storage
/// it is of) and `since`. The answer covers every input of `F`'s rows because the engine
/// records every read of a run, and a read it cannot see ([`super::report_untracked_read`])
/// puts its row in every walk.
pub fn changed_since<F: Function>(db: &F::Db, since: View) -> Changed<F::Key> {
    let storage = db.storage();
    match changed_ids_since::<F>(db, since) {
        Changed::Keys(ids) => Changed::Keys(
            // alloc: the caller's answer
            ids.into_iter().filter_map(|id| F::Key::from_id_in(storage, id)).collect(),
        ),
        Changed::Unknown(why) => Changed::Unknown(why),
    }
}

/// [`changed_since`], the keys as their IDs: what a generated query's `changed_since` gives,
/// for its caller to read the arguments of (`arguments`), in ID order.
pub fn changed_ids_since<F: Function>(db: &F::Db, since: View) -> Changed<Id> {
    let storage = db.storage();
    bring_to_date(std::ptr::from_ref(db).cast(), storage);
    let index = F::ingredient().index();
    let (log, floor) = {
        let walked = storage.frontier.walked.lock();
        (Arc::clone(&walked.log), walked.floor) // clone: a count, no allocation under the lock
    };
    if since > storage.view() {
        return Changed::Unknown(Unknown::NotAView { view: storage.view() });
    }
    if since.as_u64() < floor {
        return Changed::Unknown(Unknown::Before { floor: View(floor) });
    }
    let from = log.partition_point(|found| found.at <= since.as_u64());
    let mut ids: Vec<u32> = Vec::new(); // alloc: the caller's answer, the changed keys
    for found in &log[from..] {
        for &node in &found.nodes {
            let (owner, id, _) = key_parts(storage.key_of(node));
            if owner == index {
                ids.push(id.as_u32());
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    Changed::Keys(ids.into_iter().map(Id::from_u32).collect()) // alloc: the caller's answer
}

/// Whether a walk is due: a view opened since the last, or a node said it changed.
fn due(storage: &Storage) -> bool {
    !walked_here(storage)
        || (!storage.frontier.changed.is_empty()
            && storage.frontier.plant != Plant::StopsAtChangedReader)
}

/// Whether a walk ended in this view.
fn walked_here(storage: &Storage) -> bool {
    storage.frontier.walked.lock().done == storage.view().as_u64()
}

/// The most times [`bring_to_date`] asks for the walk's flight: each time a walk ended (this
/// task's or the one it waited for) or unwound in the task that held it.
const ROUNDS: u32 = 64;

/// Walks when a walk is due, one walker at a time: the walk's flight, so a second asker
/// waits under a span naming it (rule 15) and finds the walk done.
fn bring_to_date(db: *const (), storage: &Storage) {
    if !due(storage) {
        return;
    }
    let mut rounds = 0;
    loop {
        if storage.readers_stopped() {
            Cancelled::PendingWrite.throw();
        }
        rounds += 1;
        assert!(
            rounds <= ROUNDS,
            "the changed set of view {} was asked for {ROUNDS} times and no walk of it ended \
             (rule 18)",
            storage.view().as_u64()
        );
        // The barrier stays down while this task walks: a walk verifies from an empty query
        // stack, whatever its caller's holds.
        let barrier = local::barrier();
        let awaited = |it: &mut String| {
            use std::fmt::Write as _;
            _ = write!(it, "the changed set of view {}", storage.view().as_u64());
        };
        // Ingredient 0 is no ingredient's: its flight keys are the engine's own.
        let key = Storage::flight(0, Id::from_u32(1));
        let got = storage.flights.get_or_run_named(key, awaited, || {
            if due(storage) {
                walk(db, storage);
            }
            Ok(())
        });
        drop(barrier);
        if got.is_err() {
            Cancelled::WouldWaitOnItself { asked: None }.throw();
        }
        if walked_here(storage) {
            return;
        }
    }
}

/// The walker's vectors, taken out of [`Walked`] and put back when it ends or unwinds: a walk
/// that unwinds (a write pending) leaves what was written in place, and the next walk starts
/// from it again.
struct Taken<'a> {
    frontier: &'a Frontier,
    written: Vec<u32>,
    untracked: Vec<u32>,
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        let mut walked = self.frontier.walked.lock();
        walked.written = std::mem::take(&mut self.written);
        walked.untracked = std::mem::take(&mut self.untracked);
    }
}

/// What verifying one row for the walk found.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Seen1 {
    /// Never made, emptied, or no function row: nothing to have changed.
    Passed = 0,
    /// Up to date in this view: it said so if it changed.
    Current = 1,
    /// It could not be told (running on a stack, provisional): taken as changed.
    Unsure = 2,
    /// Its verification unwound and was contained: taken as changed.
    Unwound = 3,
}

/// This storage's database, as its functions' vtables take it, for the pool's tasks.
#[derive(Clone, Copy)]
struct Db(*const ());

// SAFETY: the pointer is the `&F::Db` [`changed_since`] was given, which outlives the walk
// (the walk returns before it does, and every task of a level has ended by then:
// `par_for_each`); a database is shared by the pool's workers as it is (`Database: Sync`).
unsafe impl Send for Db {}
// SAFETY: as above.
unsafe impl Sync for Db {}

/// Brings row `node` up to this view as a reader's edge would, containing an unwind that is
/// not the caller's own.
fn verify(db: Db, storage: &Storage, node: u32, plant: Plant) -> Seen1 {
    let Some((header, index, id)) = row_of(storage, node) else { return Seen1::Passed };
    let Some(verify) = ingredient::vtable(index).and_then(|it| it.verify) else {
        return Seen1::Passed;
    };
    let word = header.verified.load(Ordering::Acquire);
    if word & (VIEW_MASK | RUNNING | PROVISIONAL) == 0 {
        return Seen1::Passed;
    }
    if plant == Plant::SkipsEvictedRows && word & VALUE == 0 {
        return Seen1::Passed;
    }
    if word & (RUNNING | PROVISIONAL) == 0 && word & VIEW_MASK == storage.view_bits() {
        return Seen1::Current;
    }
    // SAFETY: `db.0` is this storage's database as every function of it names it
    // (`Storage::check_db`), and `id` is a key of ingredient `index` (its node's key).
    match catch_unwind(AssertUnwindSafe(|| unsafe { verify(db.0, storage, id) })) {
        Ok(Verified::Current) => Seen1::Current,
        Ok(Verified::Changed | Verified::Assumed) => Seen1::Unsure,
        Err(payload) => {
            if payload.is::<Cancelled>() || payload.is::<OutOfFuel>() || payload.is::<OutOfSteps>()
            {
                // The caller's own: a write is pending, its bound ran out, or the wait would
                // close a cycle of tasks (the pool's loop runs that one again).
                resume_unwind(payload);
            }
            tracing::warn!(
                target: "blaze",
                query = ingredient::name(index),
                key = ?id,
                why = %super::unwind_text(&*payload).unwrap_or_default(),
                "the changed-set walk's verification of a row unwound: contained, the row is named as changed"
            );
            Seen1::Unwound
        }
    }
}

/// One walk: see the module. Runs holding the walk's flight.
fn walk(db: *const (), storage: &Storage) {
    let frontier = &storage.frontier;
    let plant = frontier.plant;
    let view = storage.view().as_u64();
    let (mut taken, apart) = {
        let mut walked = frontier.walked.lock();
        let taken = Taken {
            frontier,
            written: std::mem::take(&mut walked.written),
            untracked: std::mem::take(&mut walked.untracked),
        };
        (taken, Arc::clone(&walked.unwound)) // clone: a count
    };
    // Named, with what it did: the size of the changed set, the rows visited and the edges
    // walked for this edit (rule 16).
    let walking = tracing::info_span!(
        target: "blaze",
        "changed set",
        view,
        written = taken.written.len(),
        untracked = tracing::field::Empty,
        visited = tracing::field::Empty,
        edges = tracing::field::Empty,
        changed = tracing::field::Empty,
        unwound = tracing::field::Empty,
    );
    let _entered = walking.enter();
    let mut plan = Plan::new();
    let mut work = ChangedSetWork { view, written: taken.written.len() as u64, ..Default::default() };

    // The rows whose last run made an untracked read: those said since the last walk, and
    // those before that still are. Each runs again in every view, so each is asked.
    while let Some(node) = frontier.untracked.pop() {
        taken.untracked.push(node); // alloc: the untracked rows, a word each
    }
    taken.untracked.sort_unstable();
    taken.untracked.dedup();
    taken.untracked.retain(|&node| {
        row_of(storage, node)
            .is_some_and(|(header, _, _)| header.meta.load(Ordering::Relaxed) & UNTRACKED != 0)
    });
    // A row whose verification unwound stands apart while its word does.
    // alloc: the rows set apart, none in a build whose every row verifies
    let mut unwound: Vec<(u32, u32)> = apart
        .iter()
        .copied()
        .filter(|&(node, word)| {
            row_of(storage, node)
                .is_some_and(|(header, _, _)| header.verified.load(Ordering::Acquire) == word)
        })
        .collect();

    let mut changed: Vec<u32> = taken.written.clone(); // alloc: a walk's lists, what the edit reaches
    let mut found: Vec<u32> = Vec::new(); // alloc: as above
    let mut suspects: Vec<u32> = Vec::new(); // alloc: as above
    for &node in &taken.untracked {
        if plan.ask(node) {
            suspects.push(node);
            work.untracked += 1;
        }
    }
    let mut first = true;
    loop {
        // What said it changed, by this walk's verifications or by any pull since the last.
        if first || plant != Plant::StopsAtChangedReader {
            while let Some(node) = frontier.changed.pop() {
                changed.push(node);
                found.push(node);
            }
        }
        first = false;
        plan.suspects(&frontier.reverse, &mut changed, &mut suspects);
        if suspects.is_empty() {
            if frontier.changed.is_empty() || plant == Plant::StopsAtChangedReader {
                break;
            }
            continue;
        }
        // A row set apart is not verified again: it is named, and its readers are asked.
        suspects.retain(|&node| {
            let apart = unwound.iter().any(|&(it, _)| it == node);
            if apart {
                changed.push(node);
                found.push(node);
            }
            !apart
        });
        let db = Db(db);
        let seen: Vec<AtomicU8> = suspects.iter().map(|_| AtomicU8::new(0)).collect(); // alloc: as above
        let one = |at: usize| {
            seen[at].store(verify(db, storage, suspects[at], plant) as u8, Ordering::Relaxed);
        };
        if suspects.len() < PAR_AT {
            (0..suspects.len()).for_each(one);
        } else {
            super::par_for_each(suspects.len(), one);
        }
        for (at, &node) in suspects.iter().enumerate() {
            let seen = seen[at].load(Ordering::Relaxed);
            if seen == Seen1::Unsure as u8 || seen == Seen1::Unwound as u8 {
                changed.push(node);
                found.push(node);
            }
            if seen == Seen1::Unwound as u8 {
                work.unwound += 1;
                if let Some((header, _, _)) = row_of(storage, node) {
                    unwound.push((node, header.verified.load(Ordering::Acquire))); // alloc: as above
                }
            }
        }
        suspects.clear();
        // Bounded (rule 18): `plan` hands a row out once and gathers a node once, so the
        // loop ends within the nodes there are; this says so if that ever fails.
        assert!(
            plan.visited <= u64::from(storage.nodes_made()),
            "the changed-set walk of view {view} visited {} rows, more than the {} nodes there are",
            plan.visited,
            storage.nodes_made()
        );
    }
    found.sort_unstable();
    found.dedup();
    work.visited = plan.visited;
    work.edges = plan.edges;
    work.changed = found.len() as u64;
    walking.record("untracked", work.untracked);
    walking.record("visited", work.visited);
    walking.record("edges", work.edges);
    walking.record("changed", work.changed);
    walking.record("unwound", work.unwound);

    // The findings: this walk's after the kept ones, the oldest dropped past the bounds.
    let kept = Arc::clone(&frontier.walked.lock().log); // clone: a count
    let mut floor = None;
    let mut log: Vec<Arc<Found>> = kept.iter().cloned().collect(); // alloc: the log's list, at most `LOG_WALKS` pointers
    if !found.is_empty() {
        log.push(Arc::new(Found { at: view, nodes: found })); // alloc: a walk's findings
    }
    let mut nodes: usize = log.iter().map(|it| it.nodes.len()).sum();
    let mut drop_to = 0;
    while log.len() - drop_to > LOG_WALKS || (nodes > LOG_NODES && drop_to < log.len()) {
        nodes -= log[drop_to].nodes.len();
        floor = Some(log[drop_to].at);
        drop_to += 1;
    }
    log.drain(..drop_to);
    let log = Arc::new(log); // alloc: as above
    let apart = unwound.len() as u32;
    let unwound = Arc::new(unwound); // alloc: as above
    taken.written.clear();
    let mut walked = frontier.walked.lock();
    walked.log = log;
    walked.unwound = unwound;
    if let Some(floor) = floor {
        walked.floor = walked.floor.max(floor);
    }
    walked.done = view;
    walked.work = work;
    drop(walked);
    drop(taken);
    // Published last: from here a row not verified in this view is unchanged ([`clean`]).
    frontier.unwound.store(apart, Ordering::Release);
    frontier.clean_at.store(view, Ordering::Release);
}

/// Whether the row of `node`, verified in an earlier view, holds in this one with no look at
/// its edges: a walk ended in this view, and what a walk leaves is that every row made before
/// it is verified in its view or unchanged (the module's argument), but a row whose last run
/// made an untracked read (it runs again in every view) and one whose verification by the
/// walk unwound. So after a walk a pull of an unchanged row costs this, not its edges: what
/// makes a host's pass over every row it holds cost a few loads a row.
///
/// A walk that started again in the same view changes nothing here: only a row set apart
/// can change behind a walk that ended, and its readers are set apart with it (a reader's
/// verification pulls it and unwinds as it does).
#[inline]
pub(crate) fn clean(storage: &Storage, header: &Header, node: u32) -> bool {
    let frontier = &storage.frontier;
    if frontier.clean_at.load(Ordering::Acquire) != storage.view().as_u64() {
        return false;
    }
    if header.meta.load(Ordering::Relaxed) & UNTRACKED != 0 {
        return false;
    }
    frontier.unwound.load(Ordering::Acquire) == 0 || !set_apart(frontier, node)
}

#[cold]
fn set_apart(frontier: &Frontier, node: u32) -> bool {
    let apart = Arc::clone(&frontier.walked.lock().unwound); // clone: a count
    apart.iter().any(|&(it, _)| it == node)
}

/// Walks the changed set of this view if a walk is due: what a host calls once after its
/// writes and before it asks for what it holds, so each row it asks that the edit did not
/// reach answers by [`clean`]. Returns what the last walk did. For a caller outside any
/// query: asked from inside one (a query on this task's stack) it starts no walk, since a
/// walk waits for rows the caller's own stack may hold, and gives the last walk's work.
pub fn walk_changed_set<D: Database>(db: &D) -> ChangedSetWork {
    let storage = db.storage();
    if local::with(|local| local.task_frames()) == 0 {
        bring_to_date(std::ptr::from_ref(db).cast(), storage);
    }
    storage.changed_set_work()
}


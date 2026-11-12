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
//! A reader verified in the view its dependency changed in, or later, is passed over with one
//! load ([`Plan::suspects`]): the change is in its value already. So a walk after a build asks
//! the rows that are behind what the build made again, not every reader of it.
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
//! The readers passed over lose nothing: `d` changed after the row was verified, so the row
//! is behind `d`'s stamp and is asked.
//!
//! A row the walk verifies may be one nothing asks for any more (its key names something an
//! edit removed): a pull would never run it. So a verification that unwinds for any reason
//! but the caller's own (a write pending, its fuel) is contained: the engine's guard puts the
//! row's word back, the walk counts it, names it as changed and goes on, and does not ask it
//! again until something else brought it up to date ([`Walked::apart`]). A walk contains at
//! most [`UNWOUND_MOST`] of them: past that it marks what it reaches, verifying nothing.
//!
//! Persistence: none. The storage is memory (a restart makes another storage), and the index
//! is made from the rows' own edges: empty with an empty storage, it holds the invariant from
//! the first run on.

use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use crossbeam_queue::SegQueue;

use super::{
    Cancelled, FromIdIn, Id, OutOfFuel, OutOfSteps,
    function::{self, Function, Verified},
    ingredient::{self, Kind},
    local,
    reverse::{ASSUMED, ChangedIn, Plan, Reverse, Seen},
    row::{Header, PROVISIONAL, RUNNING, UNTRACKED, VALUE, VIEW_MASK, span},
    storage::{Database, FIELD, Storage, View, key_parts},
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

/// The most walks' findings kept apart, and the most nodes they hold before the oldest are
/// merged ([`merge_oldest`]): past either, the oldest findings become one, of the newest view
/// among them. Nothing is dropped, so a host that asks from any earlier view is answered
/// (there is no "before the log": a host with nothing to start from would have to take
/// everything it holds as changed, which is the sweep the changed set replaces).
const LOG_NODES: usize = 1 << 20;
const LOG_WALKS: usize = 256;

/// Merges the oldest findings of `log` where it is over its bounds: more than [`LOG_WALKS`]
/// walks (the older half becomes one), or more nodes than [`LOG_NODES`] and twice what its
/// oldest finding holds (all become one). The merged finding holds each node once and is of
/// the newest view merged.
///
/// Why an answer stays right: [`changed_ids_since`] names the nodes of every finding whose
/// view is after the one asked from. A finding merged into one of a later view is named
/// whenever it was before (its own view is no later than the merged one), and to a host that
/// asks from between the two it is named where it was not: a name too many, which
/// [`Changed::Keys`] allows and costs the host one ask of a row that is up to date.
///
/// Cost: the node bound is at least twice the oldest finding, so the findings merged into it
/// hold as many nodes as it does: each node is copied a number of times logarithmic in the
/// log's size, never once a walk. Memory: each node once in the oldest finding, so at most 4
/// bytes a node of the storage beside [`LOG_NODES`] of newer ones.
fn merge_oldest(log: &mut Vec<Arc<Found>>) {
    let nodes: usize = log.iter().map(|it| it.nodes.len()).sum();
    let most = LOG_NODES.max(log.first().map_or(0, |it| 2 * it.nodes.len()));
    let merged = if nodes > most {
        log.len()
    } else if log.len() > LOG_WALKS {
        log.len() - LOG_WALKS / 2
    } else {
        return;
    };
    if merged < 2 {
        return;
    }
    let at = log[merged - 1].at;
    let mut all: Vec<u32> = Vec::new(); // alloc: the merged finding, each changed node once
    for found in log.drain(..merged) {
        all.extend_from_slice(&found.nodes);
    }
    all.sort_unstable();
    all.dedup();
    all.shrink_to_fit();
    log.insert(0, Arc::new(Found { at, nodes: all })); // alloc: as above
}

/// The rows of one level past which a walk stops verifying and marks: a level of fewer is
/// verified on the walker's thread, one row after the other, with early cutoff; from the
/// first level of this many on, every row the walk reaches is set apart unverified
/// ([`Walked::apart`]), named as changed, and its readers reached in turn.
///
/// Why a wide level is marked and not verified. Verifying a row pulls what it reads, and
/// the rows of one level read each other and the rows the walk has yet to reach. Until
/// 2026-11-11 a wide level was handed to the pool a job a row: each job waited on runs
/// other jobs of the level held, gave way and asked again, and the walk also ran again
/// every evicted row it reached, asked for or not. After a change the whole program reads
/// (measured: a first `blaze test` after `run`, `changed set` 465 s, 108,508 waits of
/// 23,548 thread-seconds, 84,075 bodies run again) sixteen cores spent eight minutes
/// waiting on each other. A change that reaches a wide level is broad, and a broad change
/// gains little from the walk's cutoff: so the walk follows the reverse edges only (loads,
/// no query run, no wait, nothing given way to) and leaves each marked row to whoever asks
/// for it, verified then as any pull verifies, by the hosts' own tasks on all cores. A row
/// nothing asks for is never run.
///
/// What stays true: a row the walk did not reach is unchanged ([`clean`]); a marked row is
/// never taken as clean; every row whose value changed is named, with marked rows named
/// too, which may be unchanged (a name too many costs its host one ask).
const WIDE: usize = 64;

/// The verifications one walk contains the unwinding of before it stops verifying: from the
/// row after the last of them it marks, as past a wide level ([`WIDE`]), and warns once.
///
/// Why. A row the walk reaches may be keyed by something an edit removed (a function taken
/// out of a file, a module gone from a def map): nothing would ask for it, its run panics,
/// and so does the run of every row that reads it. Each panic costs its hook (a stack
/// walked and written to the daemon's log and the flight recorder), and nothing bounded how
/// many one walk met: `fn main` replaced by another function in a hello-world was 83 in one
/// build (2026-11-11, 54 "Can't find ast id" and 29 "is outside fn bodies"), and the first
/// edit after a restart, whose build rewrites 4,722 input fields that 551,000 rows read, was
/// 40,903 in a request that did not answer in 45 s (30,939 "module ... of an item is not in
/// its crate's def map", 9,356 "ModuleId not found in ModulesMap"). A walk that has met this
/// many is looking at rows of something removed: the rest are left to whoever asks for them,
/// who asks only for rows of things that are still there.
///
/// What stays true is what [`WIDE`] keeps: a marked row is never taken as clean and is named
/// as changed. Bounded progress (rule 18): the unwinds a walk contains are at most this.
const UNWOUND_MOST: u64 = 8;

/// What the walks keep between them. Its vectors are taken out by the walker (the holder of
/// the walk's flight) and put back, so the lock is held for moves only.
#[derive(Default)]
struct Walked {
    /// The view the last walk ended in.
    done: u64,
    log: Arc<Vec<Arc<Found>>>,
    /// The field nodes written since the last walk.
    written: Vec<u32>,
    /// The rows whose last run made an untracked read, sorted.
    untracked: Vec<u32>,
    /// Rows a walk set apart: those of a wide level and past it, marked unverified
    /// ([`WIDE`]), and those whose verification unwound; each with its `verified` word then,
    /// sorted by node, each node once. Not
    /// asked again by a walk while the word stands, named as changed whenever reached, and
    /// never taken as clean ([`clean`]). Replaced whole by a walk that ended.
    apart: Arc<Vec<(u32, u32)>>,
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
    /// Nodes that had said they changed before the walk began (runs of pulls since the walk
    /// before, not backdated): the backlog the walk starts from beside what was written. A
    /// build's pulls leave one per row it made again; an edit's walk should find none.
    pub said: u64,
    /// Rows it verified.
    pub visited: u64,
    /// Reverse edges it followed.
    pub edges: u64,
    /// Readers it passed over with one load each: verified in the view their dependency
    /// changed in or later, so the change is in their value already.
    pub seen_since: u64,
    /// Nodes that changed: the changed set's size, fields included.
    pub changed: u64,
    /// Verifications that unwound and were contained.
    pub unwound: u64,
    /// Rows marked unverified: those of the first wide level ([`WIDE`] rows) and every row
    /// reached past it, or past the walk's most unwinds ([`UNWOUND_MOST`]). 0 for an edit
    /// whose every level is narrow and whose rows verify.
    pub marked: u64,
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
    /// The walk follows no reverse edge: neither the readers of an input field that was
    /// written nor those of a row a pull ran and found changed are asked, so nothing the
    /// walk alone would find is found.
    ForgetsReadersOfWritten,
    /// The walk goes on verifying however many verifications unwound ([`UNWOUND_MOST`]
    /// taken out): a panic a row for everything keyed by what an edit removed.
    ContainsEveryUnwind,
    /// The walk asks every reader of a changed node, those verified since it changed too
    /// ([`Plan::suspects`]'s test taken out): the first walk after a build asks the readers
    /// of every row the build made again.
    AsksReadersThatSawTheChange,
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
    /// How many rows [`Walked::apart`] holds.
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

    /// Plants [`Plant::ForgetsReadersOfWritten`] in this storage's changed set: for the
    /// control of a host's test that an edit reaches what reads it (hir's body check), which
    /// cannot name the engine's own plants. With every reverse edge dropped, a row no pull
    /// ran is taken as unchanged after an edit, and the host's oracle, a build from scratch,
    /// must say so. Never called outside a test.
    #[doc(hidden)]
    pub fn plant_forgotten_readers_of_written(&mut self) {
        self.frontier.plant = Plant::ForgetsReadersOfWritten;
    }

    /// An input field's node was written (with `&mut`: its setter).
    pub(crate) fn note_written(&mut self, node: u32) {
        self.frontier.walked.get_mut().written.push(node); // alloc: the writes since the last walk, a word each
    }

    /// The runs whose edges are not yet folded into the reverse index: what the next fold
    /// ([`Storage::pack_reverse`], or the next view's boundary) costs.
    pub fn runs_unfolded(&self) -> usize {
        self.frontier.stored.len()
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
            packed = tracing::field::Empty,
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
        // The lists hold as many readers as the packed tier: packed, a copy a reader, a
        // bounded number of times a reader however long the storage lives.
        let packed = reverse.due();
        if packed {
            reverse.pack();
        }
        fold.record("runs", runs);
        fold.record("readers", reverse.held());
        fold.record("remade", remade);
        fold.record("packed", packed);
        self.frontier.reverse = reverse;
    }

    /// Packs the reverse index where its lists hold enough readers to pay
    /// ([`Reverse::worth_packing`]): what a host does with time to spare (a build's end), so
    /// what a build's runs added is held at 4 bytes a reader from then on. With `&mut`: no
    /// walk reads the index meanwhile.
    ///
    /// It folds first ([`Storage::fold_reverse`]): the edges of the runs since the last view
    /// boundary are otherwise folded at the next one, which is the next build's first write,
    /// on its path. A build of a new hello-world left 1.49 million runs to it, and the
    /// project's second build spent 0.49 to 0.65 s of its 1.1 s there (`reverse edges fold`,
    /// 2026-11-11). A fold needs only `&mut` and may come at any time before the edge array
    /// settles, which only a view boundary does, after folding itself.
    pub fn pack_reverse(&mut self) {
        self.fold_reverse();
        if !self.frontier.reverse.worth_packing() {
            return;
        }
        let pack = tracing::info_span!(
            target: "blaze",
            "reverse edges pack",
            readers = self.frontier.reverse.held(),
            before = self.frontier.reverse.bytes(),
            after = tracing::field::Empty,
        );
        let _entered = pack.enter();
        self.frontier.reverse.pack();
        pack.record("after", self.frontier.reverse.bytes());
    }

    /// The nodes of the edge span `span`, into `out` (emptied first).
    fn copy_edges(&self, span: u64, out: &mut Vec<u32>) {
        out.clear();
        let (start, len) = (span::start(span), span::len(span));
        out.extend((start..start + len).map(|at| self.edge(at))); // alloc: the fold's scratch, one row's edges
    }
}

/// The view node `node` changed in, for a node that said it changed or was written: a
/// field's stamp, a settled row's `changed`. [`ASSUMED`] where nothing says (a row emptied,
/// running or provisional, a node with no row): every reader of it is asked.
fn changed_in(storage: &Storage, node: u32, plant: Plant) -> ChangedIn {
    if plant == Plant::AsksReadersThatSawTheChange {
        return (node, ASSUMED);
    }
    let painted = storage.color_of(node);
    if (painted >> 32) as u32 >= FIELD {
        return (node, painted as u32);
    }
    let since = row_of(storage, node).map_or(ASSUMED, |(header, _, _)| {
        let word = header.verified.load(Ordering::Acquire);
        if word & (RUNNING | PROVISIONAL) != 0 || word & VIEW_MASK == 0 {
            ASSUMED
        } else {
            header.changed.load(Ordering::Relaxed)
        }
    });
    (node, since)
}

/// The view the row of `node` was last verified in; 0 where that cannot be told (running,
/// provisional, never made, no row): a reader that has seen no change.
fn verified_in(storage: &Storage, node: u32) -> u32 {
    row_of(storage, node).map_or(0, |(header, _, _)| {
        let word = header.verified.load(Ordering::Acquire);
        if word & (RUNNING | PROVISIONAL) != 0 { 0 } else { word & VIEW_MASK }
    })
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
    /// The view asked from is not one of this storage's: it is after its current view.
    NotAView { view: View },
    /// Asked from inside a query while a walk is due: a walk is started only from outside
    /// one.
    InsideQuery,
}

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unknown::NotAView { view } => {
                write!(f, "the view asked from is after the storage's view {}", view.as_u64())
            }
            Unknown::InsideQuery => f.write_str(
                "the changed set was asked for from inside a query while a walk of it is due",
            ),
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
    if local::with(|local| local.task_frames()) != 0 {
        // Asked from inside a query: a walk waits for rows the caller's own stack may hold,
        // so none is started; without one the log may lack this view's changes.
        if due(storage) {
            return Changed::Unknown(Unknown::InsideQuery);
        }
    } else {
        bring_to_date(std::ptr::from_ref(db).cast(), storage);
    }
    let index = F::ingredient().index();
    let log = Arc::clone(&storage.frontier.walked.lock().log); // clone: a count, no allocation under the lock
    if since > storage.view() {
        return Changed::Unknown(Unknown::NotAView { view: storage.view() });
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
        (taken, Arc::clone(&walked.apart)) // clone: a count
    };
    // Named, with what it did: the size of the changed set, the rows visited and the edges
    // walked for this edit (rule 16).
    let walking = tracing::info_span!(
        target: "blaze",
        "changed set",
        view,
        written = taken.written.len(),
        untracked = tracing::field::Empty,
        said = tracing::field::Empty,
        seen_since = tracing::field::Empty,
        visited = tracing::field::Empty,
        edges = tracing::field::Empty,
        changed = tracing::field::Empty,
        unwound = tracing::field::Empty,
        marked = tracing::field::Empty,
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
    // A row set apart (marked past a wide level, or its verification unwound) stands apart
    // while its word does: a pull that brought it up to date since took it out.
    // alloc: the rows set apart, none in a build whose every row verifies
    let mut unwound: Vec<(u32, u32)> = apart
        .iter()
        .copied()
        .filter(|&(node, word)| {
            row_of(storage, node)
                .is_some_and(|(header, _, _)| header.verified.load(Ordering::Acquire) == word)
        })
        .collect();

    // alloc: a walk's lists, what the edit reaches
    let mut changed: Vec<ChangedIn> = match plant {
        Plant::ForgetsReadersOfWritten => Vec::new(),
        // The written fields, each with the view of its write; `taken` keeps them should
        // the walk unwind.
        _ => taken.written.iter().map(|&node| changed_in(storage, node, plant)).collect(),
    };
    let mut found: Vec<u32> = Vec::new(); // alloc: as above
    let mut suspects: Vec<u32> = Vec::new(); // alloc: as above
    for &node in &taken.untracked {
        if plan.ask(node) {
            suspects.push(node);
            work.untracked += 1;
        }
    }
    // The rows set apart, by node: those kept from the walks before and this walk's.
    let mut apart_now = Seen::with_hasher(blaze_hash::FixedState);
    apart_now.extend(unwound.iter().map(|it| it.0)); // alloc: as above
    // Whether a wide level was met, or the walk's most unwinds: from then on it marks and
    // verifies nothing.
    let mut broad = false;
    let unwound_most = match plant {
        Plant::ContainsEveryUnwind => u64::MAX,
        _ => UNWOUND_MOST,
    };
    let mut first = true;
    loop {
        // What said it changed, by this walk's verifications or by any pull since the last.
        if first || plant != Plant::StopsAtChangedReader {
            while let Some(node) = frontier.changed.pop() {
                changed.push(changed_in(storage, node, plant));
                found.push(node);
                work.said += u64::from(first);
            }
        }
        first = false;
        if plant == Plant::ForgetsReadersOfWritten {
            changed.clear();
        }
        plan.suspects(&frontier.reverse, &mut changed, &mut suspects, |reader| verified_in(storage, reader));
        if suspects.is_empty() {
            if frontier.changed.is_empty() || plant == Plant::StopsAtChangedReader {
                break;
            }
            continue;
        }
        // A row set apart is not verified again: it is named, and its readers are asked.
        suspects.retain(|&node| {
            let apart = apart_now.contains(&node);
            if apart {
                changed.push((node, ASSUMED));
                found.push(node);
            }
            !apart
        });
        let db = Db(db);
        // A wide level, and every level after it: marked, not verified ([`WIDE`]). A row
        // never made or emptied has nothing to mark; one up to date in this view said so
        // if it changed.
        broad |= suspects.len() >= WIDE;
        if broad {
            let view_bits = storage.view_bits();
            for &node in &suspects {
                let Some((header, _, _)) = row_of(storage, node) else { continue };
                let word = header.verified.load(Ordering::Acquire);
                if word & (VIEW_MASK | RUNNING | PROVISIONAL) == 0 {
                    continue;
                }
                if word & (RUNNING | PROVISIONAL) == 0 && word & VIEW_MASK == view_bits {
                    continue;
                }
                changed.push((node, ASSUMED));
                found.push(node);
                if apart_now.insert(node) {
                    unwound.push((node, word)); // alloc: as above
                }
                work.marked += 1;
            }
            suspects.clear();
            continue;
        }
        // A narrow level: verified here, one row after the other, until the walk has
        // contained its most unwinds ([`UNWOUND_MOST`]); the rows after that are marked.
        let view_bits = storage.view_bits();
        for &node in &suspects {
            if work.unwound >= unwound_most {
                let Some((header, _, _)) = row_of(storage, node) else { continue };
                let word = header.verified.load(Ordering::Acquire);
                if word & (VIEW_MASK | RUNNING | PROVISIONAL) == 0 {
                    continue;
                }
                if word & (RUNNING | PROVISIONAL) == 0 && word & VIEW_MASK == view_bits {
                    continue;
                }
                changed.push((node, ASSUMED));
                found.push(node);
                if apart_now.insert(node) {
                    unwound.push((node, word)); // alloc: as above
                }
                work.marked += 1;
                continue;
            }
            let seen = verify(db, storage, node, plant);
            if seen == Seen1::Unsure || seen == Seen1::Unwound {
                changed.push((node, ASSUMED));
                found.push(node);
            }
            if seen == Seen1::Unwound {
                work.unwound += 1;
                if let Some((header, _, _)) = row_of(storage, node)
                    && apart_now.insert(node)
                {
                    unwound.push((node, header.verified.load(Ordering::Acquire))); // alloc: as above
                }
                if work.unwound == unwound_most {
                    broad = true;
                    tracing::warn!(
                        target: "blaze",
                        view,
                        unwound = work.unwound,
                        "the changed-set walk contained its most verifications that unwound: the rows it reaches from here are marked, not verified, and left to whoever asks for them"
                    );
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
    work.seen_since = plan.seen_since;
    work.changed = found.len() as u64;
    walking.record("untracked", work.untracked);
    walking.record("said", work.said);
    walking.record("seen_since", work.seen_since);
    walking.record("visited", work.visited);
    walking.record("edges", work.edges);
    walking.record("changed", work.changed);
    walking.record("unwound", work.unwound);
    walking.record("marked", work.marked);
    // The walk's work, kept with the build it ran in (`blaze explain edit_work`): a walk
    // that verified or reached more than the edit did shows here first.
    tracing::info!(
        target: "blaze",
        stage = super::edit_work::CHANGED_SET,
        written = work.written,
        untracked = work.untracked,
        said = work.said,
        seen_since = work.seen_since,
        visited = work.visited,
        edges = work.edges,
        changed = work.changed,
        unwound = work.unwound,
        marked = work.marked,
        "edit work"
    );

    let log = logged(frontier, view, found);
    // Sorted by node, each once ([`set_apart`] searches it).
    unwound.sort_unstable_by_key(|it| it.0);
    let apart = u32::try_from(unwound.len()).unwrap_or(u32::MAX);
    let unwound = Arc::new(unwound); // alloc: as above
    taken.written.clear();
    let mut walked = frontier.walked.lock();
    walked.log = log;
    walked.apart = unwound;
    walked.done = view;
    walked.work = work;
    drop(walked);
    drop(taken);
    // Published last: from here a row not verified in this view is unchanged ([`clean`]).
    frontier.unwound.store(apart, Ordering::Release);
    frontier.clean_at.store(view, Ordering::Release);
}

/// The log with `found` (sorted, each node once) after the kept findings, as view `view`'s,
/// the oldest merged past the bounds. Nothing found leaves the kept log as it is.
fn logged(frontier: &Frontier, view: u64, found: Vec<u32>) -> Arc<Vec<Arc<Found>>> {
    let kept = Arc::clone(&frontier.walked.lock().log); // clone: a count
    if found.is_empty() {
        return kept;
    }
    let mut log: Vec<Arc<Found>> = kept.iter().cloned().collect(); // alloc: the log's list, at most `LOG_WALKS` pointers
    log.push(Arc::new(Found { at: view, nodes: found })); // alloc: a walk's findings
    merge_oldest(&mut log);
    Arc::new(log) // alloc: as above
}

/// What [`Storage::settle_changed_set`] did.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Settled {
    /// Nodes that had said they changed, and written fields, it looked at.
    pub said: u64,
    /// Reverse edges it followed: a load of each reader's word.
    pub edges: u64,
    /// Those of `said` it took out of the next walk's start: every reader has seen the change.
    pub settled: u64,
    /// Those left to the next walk: a reader is behind, or nothing says when the node changed.
    pub left: u64,
}

impl Storage {
    /// Settles what a build's pulls left for the next walk, with `&mut` (no run in flight):
    /// what a host does at a build's end, after its answer. Every row the build made again
    /// said it changed, and every field it wrote is listed; the next walk would start from
    /// all of them and follow each one's reverse edges, on the path of the next edit, whose
    /// own write is one field. Here each is looked at once: where every reader was verified in
    /// the view the node changed in or later, the change is in every reader's value already
    /// ([`Plan::suspects`]'s invariant), so the node is named in the log as changed by this
    /// view and taken out of the next walk's start. A node with a reader that is behind, or
    /// with nothing to say when it changed, stays for the next walk, which verifies.
    ///
    /// It runs no query and verifies no row: it claims nothing about the rows ([`clean`] is
    /// a walk's to say), only that the next walk need not ask these nodes' readers.
    ///
    /// Why no changed row is missed: a row changes by running again for a read that changed
    /// after it was verified. A node taken out has no reader verified before its stamp, so
    /// no reader runs again for it; if the node changes again it says so again.
    ///
    /// Folds first ([`Storage::fold_reverse`]): the readers of this build's runs are in the
    /// index before it is asked.
    pub fn settle_changed_set(&mut self) -> Settled {
        self.fold_reverse();
        let view = self.view().as_u64();
        let plant = self.frontier.plant;
        let span = tracing::info_span!(
            target: "blaze",
            "changed set settle",
            view,
            said = tracing::field::Empty,
            edges = tracing::field::Empty,
            settled = tracing::field::Empty,
            left = tracing::field::Empty,
        );
        let _entered = span.enter();
        let mut done = Settled::default();
        let storage = &*self;
        let frontier = &storage.frontier;
        // Whether every reader of `node` has seen its change; counts the edges looked at.
        let mut seen_by_all = |node: u32| {
            let (_, since) = changed_in(storage, node, plant);
            if since == ASSUMED {
                return false;
            }
            let mut behind = false;
            done.edges += frontier.reverse.readers(node, |reader| {
                // A reader never made, or emptied, has nothing to bring up to date.
                let verified = verified_word(storage, reader);
                behind |= verified & VIEW_MASK != 0 && verified & VIEW_MASK < since
                    || verified & (RUNNING | PROVISIONAL) != 0;
            });
            !behind
        };
        let mut said: Vec<u32> = Vec::new(); // alloc: the build's backlog, a word a node
        while let Some(node) = frontier.changed.pop() {
            said.push(node);
        }
        said.sort_unstable();
        said.dedup();
        let mut found: Vec<u32> = Vec::with_capacity(said.len()); // alloc: as above
        let mut left = 0u64;
        for &node in &said {
            if seen_by_all(node) {
                found.push(node);
            } else {
                frontier.changed.push(node);
                left += 1;
            }
        }
        let mut written = std::mem::take(&mut frontier.walked.lock().written);
        let fields = written.len();
        written.retain(|&node| !seen_by_all(node));
        left += written.len() as u64;
        done.said = said.len() as u64 + fields as u64;
        done.left = left;
        done.settled = done.said - left;
        let log = logged(frontier, view, found);
        let mut walked = frontier.walked.lock();
        walked.written = written;
        walked.log = log;
        drop(walked);
        span.record("said", done.said);
        span.record("edges", done.edges);
        span.record("settled", done.settled);
        span.record("left", done.left);
        done
    }
}

/// The `verified` word of the row of `node`; 0 for a node with no row.
fn verified_word(storage: &Storage, node: u32) -> u32 {
    row_of(storage, node).map_or(0, |(header, _, _)| header.verified.load(Ordering::Acquire))
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
    let apart = Arc::clone(&frontier.walked.lock().apart); // clone: a count
    apart.binary_search_by_key(&node, |it| it.0).is_ok()
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


#[cfg(test)]
mod tests {
    use super::*;

    fn found(at: u64, nodes: &[u32]) -> Arc<Found> {
        Arc::new(Found { at, nodes: nodes.to_vec() })
    }

    /// The nodes the log names to a host that asks from view `since`, each once, in order:
    /// what [`changed_ids_since`] reads of it.
    fn named(log: &[Arc<Found>], since: u64) -> Vec<u32> {
        let from = log.partition_point(|it| it.at <= since);
        let mut nodes: Vec<u32> = log[from..].iter().flat_map(|it| it.nodes.iter().copied()).collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    /// The invariant: merging the log's oldest findings loses no name. A host that asks from
    /// any view, the views before the merged findings too, is named every node it was named
    /// before the merge (it may be named more), so no host is ever told the log cannot
    /// answer and left to sweep everything it holds.
    #[test]
    fn a_merged_log_names_every_node_it_named_before_from_every_view() {
        // More walks than the log keeps apart, each of two nodes, one shared with the next.
        let walks = LOG_WALKS as u64 + 40;
        let whole: Vec<Arc<Found>> =
            (1..=walks).map(|at| found(at * 3, &[at as u32, at as u32 + 1])).collect();
        let mut merged = whole.clone();
        merge_oldest(&mut merged);
        assert!(merged.len() <= LOG_WALKS, "{} findings kept apart", merged.len());
        assert!(merged.windows(2).all(|it| it[0].at < it[1].at), "the log stays in view order");
        assert!(merged[0].nodes.windows(2).all(|it| it[0] < it[1]), "a merged finding holds each node once");
        for since in 0..=walks * 3 + 1 {
            let (before, after) = (named(&whole, since), named(&merged, since));
            assert!(
                before.iter().all(|node| after.binary_search(node).is_ok()),
                "asked from view {since}: the merged log names {after:?}, the whole one {before:?}"
            );
        }
        // At and after the merged finding's view the answer is the same: nothing more is named.
        let at = merged[0].at;
        assert_eq!(named(&whole, at), named(&merged, at));
    }

    /// The control of [`a_merged_log_names_every_node_it_named_before_from_every_view`]: a
    /// log that drops its oldest findings (what the merge replaced) names less to a host that
    /// asks from before them, and the oracle says so.
    #[test]
    fn the_oracle_catches_a_log_that_drops_its_oldest_findings() {
        let walks = LOG_WALKS as u64 + 40;
        let whole: Vec<Arc<Found>> =
            (1..=walks).map(|at| found(at * 3, &[at as u32, at as u32 + 1])).collect();
        let dropped = &whole[40..];
        let (before, after) = (named(&whole, 0), named(dropped, 0));
        assert!(before.iter().any(|node| after.binary_search(node).is_err()));
    }

    /// The node bound merges every finding into one, and then only once the newer findings
    /// hold as many nodes as it does: a walk never copies the whole log.
    #[test]
    fn the_node_bound_merges_only_when_the_newer_findings_match_the_oldest() {
        let big: Vec<u32> = (0..LOG_NODES as u32 + 10).collect();
        // The oldest holds a quarter: the bound is `LOG_NODES`, which the two pass together.
        let mut log = vec![found(1, &big[..LOG_NODES / 4]), found(2, &big[LOG_NODES / 4..])];
        merge_oldest(&mut log);
        assert_eq!(log.len(), 1, "past the node bound the findings are one");
        assert_eq!((log[0].at, log[0].nodes.len()), (2, big.len()));
        log.push(found(3, &[7, 8, 9]));
        merge_oldest(&mut log);
        assert_eq!(log.len(), 2, "a small finding after a large one is kept apart");
    }
}

//! The dense key space's [`Storage`]: one per database, holding every ingredient's table, the
//! ID pages, the dependency graph's nodes and edges, and the view.

use std::{
    any::TypeId,
    fmt,
    ptr::{self, NonNull},
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering},
};

use blaze_graph::{Key, pump::Flights};
use blaze_mem::{Tag, tag};

use super::{
    Event, EventKind, MemoryUsage,
    budget::{self, Held},
    capture::REF,
    counts,
    dense::{Dense, Heap, Zeroed},
    id::{Id, MAX_PAGES, PAGE_BITS, PAGE_LEN},
    ingredient::{self, Ingredient, MAX_INGREDIENTS},
    provenance::{self, Cause, restamp_run},
    row::{Owners, Pages, VALUE, VIEW_MASK, span},
};

/// What a database is to the engine: its storage. `base_db::Db` implements it; the generated
/// code is generic over it, so with one concrete database every query is a static call.
pub trait Database: Sync + 'static {
    fn storage(&self) -> &Storage;
    /// A write: `&mut` ends every reader's borrow, so a new view may begin.
    fn storage_mut(&mut self) -> &mut Storage;
}

/// The storage's view as hosts see it: how many views it has opened, bumped by every write and
/// never reused, so `==` on two of them says no write came between. Rows hold a 28-bit stamp
/// instead, which renumbering at its limit reuses; this count is untouched by that.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct View(pub(crate) u64);

impl View {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// What [`Storage::end_epoch`] let go of.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Unread {
    /// How many values went.
    pub values: usize,
    /// What they weighed: the `weigh` of each value of a function kept by bytes (`lru`); a
    /// value let go only when asked (`lru = 0`) is not weighed and counts 0.
    pub bytes: u64,
}

/// An input's kind, by how often it changes: workspace files (`LOW`), workspace structure
/// (`MEDIUM`), libraries and frozen tables (`HIGH`). A row's durability is the lowest kind it
/// read; a row whose kind did not change since it was verified verifies with one compare.
/// No default: each input states its kind, and a row's comes from what it read.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Durability(pub(crate) u8);

impl Durability {
    pub const LOW: Durability = Durability(0);
    pub const MEDIUM: Durability = Durability(1);
    pub const HIGH: Durability = Durability(2);
    pub const KINDS: usize = 3;

    pub(crate) fn from_bits(bits: u8) -> Durability {
        Durability(bits.min(2))
    }

    pub fn as_u8(self) -> u8 {
        self.0
    }
}

/// What the storage tells a host of its cold paths. Set once at setup.
pub type Hook = Box<dyn Fn(&Storage, Event) + Send + Sync>; // dyn: set once at setup, called on cold paths only

/// A node's color: the view it was verified in (high half) and its `changed` (low half).
/// Input and tracked fields are always verified: their high half is [`FIELD`] with the
/// field's durability in its low bits.
pub(crate) const FIELD: u32 = 0xffff_fff0;

#[inline(always)]
pub(crate) fn color(verified: u32, changed: u32) -> u64 {
    (u64::from(verified) << 32) | u64::from(changed)
}

/// A node's key word: its row's ID, ingredient and, for a field, the field's number.
#[inline]
pub(crate) fn node_key(ingredient: u32, id: Id, field: u32) -> u64 {
    debug_assert!((ingredient as usize) < MAX_INGREDIENTS && field < 1 << 16);
    u64::from(id.as_u32()) | (u64::from(ingredient) << 32) | (u64::from(field) << 44)
}

#[inline]
pub(crate) fn key_parts(key: u64) -> (u32, Id, u32) {
    ((key >> 32) as u32 & 0xfff, Id::from_u32(key as u32), (key >> 44) as u32 & 0xffff)
}

/// A node's input summary: a 128-bit Bloom filter of the input fields it read, transitively.
/// An input field sets two bits ([`input_bits`]); a memo's is the OR of its edges' when it ran,
/// grown by each walk that proves it; a cycle's or an untracked run's is [`ALL`]. A memo whose
/// summary shares no bit with the fields written since it was verified is unchanged, with no
/// walk: a field in its cone sets a bit of its summary, so a false positive costs a walk and a
/// false negative can't happen.
pub(crate) type Summary = [u64; 2];
pub(crate) const ALL: Summary = [u64::MAX; 2];

/// A node of the dependency graph: what a walk reads of it (its color, summary and key) in one
/// place, so a walk checks the node's index once and finds all of it in one cache line.
#[repr(C, align(32))]
pub(crate) struct NodeRow {
    /// Its color ([`color`]).
    color: AtomicU64,
    /// Its input summary ([`Summary`]).
    summary: [AtomicU64; 2],
    /// Its key word ([`node_key`]).
    key: AtomicU64,
}

// SAFETY: every field is an atomic integer, whose zero bytes are the value 0, and `Sync`; the
// row has no padding (four 8-byte fields, 32-byte aligned).
unsafe impl Zeroed for NodeRow {}

impl NodeRow {
    #[inline(always)]
    pub(crate) fn color(&self) -> u64 {
        self.color.load(Ordering::Acquire)
    }

    /// Its color under `&mut` (an input's setter): no reader races it.
    #[inline(always)]
    pub(crate) fn color_mut(&mut self) -> &mut u64 {
        self.color.get_mut()
    }

    /// Paints it: colors only go up, so racing painters of one view end at its color.
    #[inline]
    pub(crate) fn paint(&self, color: u64) {
        self.color.fetch_max(color, Ordering::Release);
    }

    #[inline(always)]
    pub(crate) fn summary(&self) -> Summary {
        let [a, b] = &self.summary;
        // Relaxed: a summary is stored before its row's word or its node's color is published
        // (release), which the reader acquired.
        [a.load(Ordering::Relaxed), b.load(Ordering::Relaxed)]
    }

    /// Adds `summary`'s bits: a walk's, which racing walkers of a view agree on.
    #[inline]
    pub(crate) fn grow_summary(&self, summary: Summary) {
        let [a, b] = &self.summary;
        a.fetch_or(summary[0], Ordering::Relaxed);
        b.fetch_or(summary[1], Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn key(&self) -> u64 {
        self.key.load(Ordering::Relaxed)
    }
}

/// Views of written-field masks kept: a memo verified further back walks.
const HISTORY: usize = 64;

/// The two bits of input field node `node`: two multiplicative hashes by odd constants.
#[inline]
pub(crate) fn input_bits(node: u32) -> Summary {
    let node = u64::from(node);
    let mut bits = [0; 2];
    for k in [0x9e37_79b9_7f4a_7c15_u64, 0xc2b2_ae3d_27d4_eb4f] {
        let bit = (node.wrapping_mul(k) >> 57) as u32;
        bits[(bit >> 6) as usize] |= 1 << (bit & 63);
    }
    bits
}

#[inline(always)]
pub(crate) fn or(a: Summary, b: Summary) -> Summary {
    [a[0] | b[0], a[1] | b[1]]
}

/// Bytes of address space each storage reserves for row pages (committed as they are made).
#[cfg(target_pointer_width = "64")]
const HEAP: usize = 1 << 36;
#[cfg(target_pointer_width = "64")]
const INTERN_HEAP: usize = 1 << 35;
/// Nodes and edges a storage may have; entries and captures one view's [`super::Reads`] may
/// hold.
#[cfg(target_pointer_width = "64")]
const NODES: usize = 1 << 28;
#[cfg(target_pointer_width = "64")]
const EDGES: usize = 1 << 32;
#[cfg(target_pointer_width = "64")]
const CAPTURED: usize = 1 << 32;
#[cfg(target_pointer_width = "64")]
const CAPTURES: usize = 1 << 31;

/// The same on a 32-bit target (wasm32), where a reservation is memory taken whole and the
/// whole address space is 4 GB, so each is a budget: 512 MB and 256 MB of row pages, 2^23
/// nodes (320 MB of node columns), 2^26 edges (256 MB), and 2^24 captured entries and 2^21
/// captures (80 MB). A storage that outgrows one stops with the error naming it, as on 64-bit.
#[cfg(not(target_pointer_width = "64"))]
const HEAP: usize = 512 << 20;
#[cfg(not(target_pointer_width = "64"))]
const INTERN_HEAP: usize = 256 << 20;
#[cfg(not(target_pointer_width = "64"))]
const NODES: usize = 1 << 23;
#[cfg(not(target_pointer_width = "64"))]
const EDGES: usize = 1 << 26;
#[cfg(not(target_pointer_width = "64"))]
const CAPTURED: usize = 1 << 24;
#[cfg(not(target_pointer_width = "64"))]
const CAPTURES: usize = 1 << 21;

// A captured entry is a node or, with [`REF`] set, a capture's number: neither reaches the bit.
const _: () = assert!(NODES <= REF as usize && CAPTURES <= REF as usize);

/// Every ingredient's rows, the dependency graph and the view of one database: `Send + Sync`,
/// read by `&` from any number of tasks, written by `&mut`.
pub struct Storage {
    /// `view | VALUE` (what a hit's `verified` equals) and, at bit 32, the stop flag: a hit
    /// compares one word with one load.
    clock: AtomicU64,
    view: u32,
    /// [`Storage::view`]: views opened, never renumbered.
    opened: u64,
    /// The last view an input of each kind or above changed in.
    last_changed: [u32; Durability::KINDS],
    /// `opened` as the last write of kind `HIGH` left it: never renumbered, so a holder that
    /// kept it across views compares it ([`Storage::durable_view`]).
    high_opened: u64,
    /// `opened` when the views were last renumbered (0: never): the stamps of the views
    /// since are in step with `opened`, those before are not ([`Storage::stamp_of`]).
    renumbered: u64,
    /// `written[i]`: the OR of the [`input_bits`] of every field written in the last `i + 1`
    /// views.
    written: [Summary; HISTORY],
    /// Each ingredient's row-page directory, null until first use.
    dirs: Box<[AtomicPtr<AtomicPtr<u8>>; MAX_INGREDIENTS]>,
    /// Each ingredient's table, null until first use.
    tables: Box<[AtomicPtr<()>; MAX_INGREDIENTS]>,
    #[expect(clippy::disallowed_types, reason = "held only while an ingredient's table or page directory is made, once each")]
    making: blaze_core::lock::Lock<()>,
    pub(crate) heap: Heap,
    pub(crate) intern_heap: Heap,
    owners: Owners,
    next_page: AtomicU32,
    pub(crate) nodes: Dense<NodeRow>,
    /// Each memo's last run: its view and [`Cause`], written by the run's flight holder.
    pub(crate) runs: Dense<AtomicU64>,
    /// What its runs logged for the profile while the run log is on ([`provenance::log_runs`]),
    /// taken by its holder with the database in hand ([`provenance::take_log_in`]).
    pub(crate) run_log: provenance::RunLog,
    next_node: AtomicU32,
    /// The rows' edge spans, one per row (in its header's `edges`): a rerun replaces its row's
    /// span, leaving the old one dead until [`Storage::settle`] moves the
    /// live ones into a fresh array.
    edges: Dense<AtomicU32>,
    edges_top: AtomicU64,
    /// Edges no row names any more: spans a rerun replaced.
    edges_dead: AtomicU64,
    /// The edges the array may hold: [`EDGES`], lower in a test of the error.
    edge_limit: u64,
    /// What this view's captures ([`super::capture_reads`]) read, each a span of entries: a
    /// node read in it, or a capture closed or replayed in it as [`REF`] and its number, so a
    /// read is held once, by the innermost capture that made it. A [`super::Reads`] is used
    /// only in the view it was made in, so the next view frees them all ([`Storage::settle`]).
    captured: Dense<AtomicU32>,
    captured_top: AtomicU64,
    /// Each capture's span in `captured`, by its number; number 0 is the capture of no read.
    captures: Dense<AtomicU64>,
    captures_made: AtomicU32,
    pub(crate) flights: Flights<()>,
    hook: Option<Hook>,
    /// The functions whose values are kept by their bytes (`lru`), weighed at each new view.
    #[expect(clippy::disallowed_types, reason = "held to add a function or take the list at a new view, never across a call")]
    weighed: blaze_core::lock::Lock<Vec<u32>>,
    /// [`Storage::keep_every_value`].
    keep: bool,
    /// The values the last view's passes read, over every tag ([`Storage::swept`]).
    swept: usize,
    /// [`Storage::set_value_budget`] of each tag: `None` reads the tag's own budget.
    budgets: [Option<u64>; Tag::COUNT],
    /// What each tag held live at its last pass ([`Storage::sweep`]): a tag over its budget is
    /// weighed again once it has grown by a [`budget::SPARE`]th of the budget since.
    passed_at: [u64; Tag::COUNT],
    /// The views the last two epochs that wrote ended at, the older first
    /// ([`Storage::end_epoch`]).
    epochs: [u32; 2],
    /// The database type of its first function table ([`Storage::check_db`]).
    #[expect(clippy::disallowed_types, reason = "held once per function table made, to check its database type")]
    db_type: blaze_core::lock::Lock<Option<TypeId>>,
}

// The tables behind the raw pointers are `Send + Sync` (each typed table's fields are atomics,
// locks and values that are `Send + Sync` by `Function::Value`'s bounds); the pointers are
// written once under `making`, read with acquire.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Heap>();
    send_sync::<Flights<()>>();
};

// SAFETY: every field is `Send` but the table pointers, which own `Send + Sync` tables
// (above) and are freed only by `Drop` with `&mut`.
unsafe impl Send for Storage {}
// SAFETY: shared access is atomics, locks and tables whose own access rules
// (`row`'s value-in-place rule) make them `Sync`; plain fields change only under `&mut`.
unsafe impl Sync for Storage {}

impl Storage {
    #[expect(clippy::disallowed_types, reason = "makes the locks its fields allow")]
    pub fn new(hook: Option<Hook>) -> Storage {
        crate::carry_with_fiber_jobs();
        let view = 1;
        // alloc: once per storage, its slot arrays
        let dirs = Box::new([const { AtomicPtr::new(ptr::null_mut()) }; MAX_INGREDIENTS]);
        // alloc: as above
        let tables = Box::new([const { AtomicPtr::new(ptr::null_mut()) }; MAX_INGREDIENTS]);
        Storage {
            clock: AtomicU64::new(u64::from(view | VALUE)),
            view,
            opened: u64::from(view),
            last_changed: [view; Durability::KINDS],
            high_opened: u64::from(view),
            renumbered: 0,
            written: [[0; 2]; HISTORY],
            dirs,
            tables,
            making: blaze_core::lock::Lock::new("query.storage.making", ()),
            heap: Heap::new(HEAP, Tag::Query),
            intern_heap: Heap::new(INTERN_HEAP, Tag::Intern),
            owners: Owners::sparse(Tag::Query, "page owners"),
            next_page: AtomicU32::new(1),
            nodes: Dense::growing(NODES, Tag::Query, "nodes"),
            runs: Dense::growing(NODES, Tag::Query, "node runs"),
            run_log: provenance::RunLog::new(),
            next_node: AtomicU32::new(1),
            edges: Dense::growing(EDGES, Tag::Query, "dependency edges"),
            edges_top: AtomicU64::new(0),
            edges_dead: AtomicU64::new(0),
            edge_limit: EDGES as u64,
            captured: Dense::growing(CAPTURED, Tag::Query, "captured reads"),
            captured_top: AtomicU64::new(0),
            captures: Dense::growing(CAPTURES, Tag::Query, "captures"),
            captures_made: AtomicU32::new(1),
            flights: Flights::new(),
            hook,
            // alloc: none, empty: one entry per `lru` function, pushed once
            weighed: blaze_core::lock::Lock::new("query.storage.weighed", Vec::new()),
            keep: false,
            swept: 0,
            budgets: [None; Tag::COUNT],
            passed_at: [0; Tag::COUNT],
            epochs: [view; 2],
            db_type: blaze_core::lock::Lock::new("query.storage.db_type", None),
        }
    }

    #[inline]
    pub fn view(&self) -> View {
        View(self.opened)
    }

    /// The view the last write of kind `HIGH` opened ([`Storage::view`]'s numbering, never
    /// renumbered): while it is what a holder saw, no library input or frozen table was
    /// written, so every row that was durable then ([`plumbing::durable`](super::plumbing))
    /// still holds its value, with no look at the row. A storage made anew (a restart, an
    /// image loaded) is another storage: a holder keeps the view with which storage it is of.
    #[inline]
    pub fn durable_view(&self) -> View {
        View(self.high_opened)
    }

    /// The stamp rows and nodes hold for the view a host read as `since`
    /// ([`Storage::view`]): this view's stamp less the views opened since. `None` when no
    /// stamp says: the views were renumbered since (a stamp of before is a rank now), or
    /// `since` is no view of this storage.
    #[inline]
    pub(crate) fn stamp_of(&self, since: View) -> Option<u32> {
        if since.0 < self.renumbered || since.0 > self.opened {
            return None;
        }
        u32::try_from(self.opened - since.0).ok().and_then(|back| self.view.checked_sub(back))
    }

    /// What a current row's `verified` equals, or a word no row has once readers are stopped.
    #[inline(always)]
    pub(crate) fn clock(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub(crate) fn view_bits(&self) -> u32 {
        self.view
    }

    /// Whether a row of durability `durability` verified in `verified` holds without looking
    /// at its edges: no input of its kind or above changed since.
    #[inline]
    pub(crate) fn unchanged_since(&self, durability: u8, verified: u32) -> bool {
        self.last_changed[durability.min(2) as usize] <= verified
    }

    /// Whether node `node`, verified in `verified`, read no input field written since: its
    /// summary shares no bit with the fields written in the views after `verified`.
    #[inline]
    pub(crate) fn inputs_unchanged(&self, node: &NodeRow, verified: u32) -> bool {
        let Some(back) = self.view.checked_sub(verified) else { return false };
        let Some(&written) = back.checked_sub(1).and_then(|at| self.written.get(at as usize))
        else {
            return back == 0;
        };
        let summary = node.summary();
        (summary[0] & written[0]) | (summary[1] & written[1]) == 0
    }

    /// Node `node`: one check of its index for all it holds.
    ///
    /// # Panics
    ///
    /// When no node `node` was made, naming it ([`Dense::get`]).
    #[inline(always)]
    #[track_caller]
    pub(crate) fn node(&self, node: u32) -> &NodeRow {
        self.nodes.get(node as usize)
    }

    /// Node `node` under `&mut`, for a writer: [`Storage::node`] with no atomic load of the
    /// bound.
    #[inline(always)]
    #[track_caller]
    pub(crate) fn node_mut(&mut self, node: u32) -> &mut NodeRow {
        self.nodes.get_mut(node as usize)
    }

    #[inline(always)]
    pub(crate) fn summary_of(&self, node: u32) -> Summary {
        self.node(node).summary()
    }

    /// Sets `node`'s summary, for its one writer (the run that holds its flight, an input's
    /// maker, a tracked struct's creator).
    #[inline]
    pub(crate) fn set_summary(&self, node: u32, summary: Summary) {
        let [a, b] = &self.node(node).summary;
        a.store(summary[0], Ordering::Relaxed);
        b.store(summary[1], Ordering::Relaxed);
    }

    /// Adds `summary`'s bits to `node`'s: a walk's, which racing walkers of a view agree on.
    #[inline]
    pub(crate) fn grow_summary(&self, node: u32, summary: Summary) {
        self.node(node).grow_summary(summary);
    }

    /// The OR of the summaries of `edges`' nodes.
    pub(crate) fn summary_of_edges(&self, edges: &[u32]) -> Summary {
        edges.iter().fold([0; 2], |acc, &node| or(acc, self.summary_of(node)))
    }

    /// Has every reader unwind with [`super::Cancelled::PendingWrite`] at its next query call,
    /// until the next write.
    pub fn stop_readers(&self) {
        self.clock.fetch_or(1 << 32, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn stopped(&self) -> bool {
        self.clock() >> 32 != 0
    }

    /// Clears [`Storage::stop_readers`] without a write: with `&mut` no reader is left, so the
    /// stop did its work. Opens no view and re-verifies nothing.
    pub fn resume_readers(&mut self) {
        *self.clock.get_mut() &= !(1 << 32);
    }

    /// Whether [`Storage::stop_readers`] was called since the last write.
    pub fn readers_stopped(&self) -> bool {
        self.stopped()
    }

    /// While `keep`, no value of an `lru` function is let go at a new view.
    pub fn keep_every_value(&mut self, keep: bool) {
        self.keep = keep;
    }

    /// What the values of `tag`'s `lru` functions may weigh in this storage, from the next view
    /// on; `None` (the default) is the tag's own budget ([`tag::budget`]), which the daemon sets
    /// from the machine's memory. `u64::MAX` keeps every value.
    pub fn set_value_budget(&mut self, tag: Tag, bytes: Option<u64>) {
        self.budgets[tag.index()] = bytes;
    }

    /// What the values of `tag`'s `lru` functions may weigh ([`Storage::set_value_budget`]).
    pub fn value_budget(&self, tag: Tag) -> u64 {
        self.budgets[tag.index()].unwrap_or_else(|| tag::budget(tag))
    }

    /// `file!()` where ingredient `ingredient` is defined.
    pub fn ingredient_file(&self, ingredient: u32) -> &'static str {
        ingredient::vtable(ingredient).map_or("<unregistered>", |it| it.file)
    }

    /// The IDs an interned row's fields name, pushed onto `out`; nothing for another row.
    pub fn ids_of(&self, id: Id, out: &mut Vec<Id>) {
        let Some(owner) = self.owner(id) else { return };
        if let Some(ids_of) = ingredient::vtable(owner).and_then(|it| it.ids_of) {
            ids_of(self, owner, id, out);
        }
    }

    /// What each ingredient in use holds, pushed onto `out`, then the dependency graph's own
    /// columns, which no ingredient owns: its nodes, the rows' edges and this view's captured
    /// reads, each with the bytes it has committed. Each function's rows in use are counted by
    /// a walk of its rows made, one header load a row (at worst a read of every row page,
    /// about a tenth of a second a GiB of rows); sound with queries running, as every count is
    /// an atomic load. For a total without the walk, [`Storage::held_bytes`].
    pub fn memory_usage(&self, out: &mut Vec<MemoryUsage>) {
        for index in self.live() {
            if let Some(vtable) = ingredient::vtable(index) {
                let mut usage = (vtable.memory)(self, index);
                if let Some(used) = vtable.used {
                    usage.used = used(self, index);
                }
                // alloc: the caller's report, one entry per ingredient
                out.push(usage);
            }
        }
        // alloc: the caller's report, three entries
        out.extend(self.graph_usage());
        // Process-wide, not this storage's, and no bytes held now (so not in `held_bytes`): the
        // most one run's reads and one task's pending rows reached as their stacks grew, what
        // the query stack bounds are set against (`local::RUN_READS`, `local::PENDING_FLOOR`).
        let super::local::HighWater { run_reads: most_reads, pending: most_pending, .. } =
            super::local::high_water();
        let stacks = [
            ("query stack reads (most one run held)", most_reads),
            ("query stack pending rows (most one task held)", most_pending),
        ];
        // alloc: the caller's report, two entries
        out.extend(stacks.map(|(name, most)| MemoryUsage {
            name,
            rows: most,
            used: most,
            row_bytes: 0,
            value_bytes: 0,
        }));
    }

    /// What the query storage holds ([`MemoryUsage::held`] summed over
    /// [`Storage::memory_usage`]'s report), from counters alone: a load or two an ingredient,
    /// no row read, cheap enough for a periodic line and sound with queries running.
    pub fn held_bytes(&self) -> u64 {
        let tables: u64 = self
            .live()
            .filter_map(|index| {
                ingredient::vtable(index).map(|vtable| (vtable.memory)(self, index).held())
            })
            .sum();
        tables + self.graph_usage().iter().map(MemoryUsage::held).sum::<u64>()
    }

    /// The dependency graph's columns as [`Storage::memory_usage`] reports them.
    fn graph_usage(&self) -> [MemoryUsage; 3] {
        let live = self
            .edges_top
            .load(Ordering::Relaxed)
            .saturating_sub(self.edges_dead.load(Ordering::Relaxed));
        let nodes = u64::from(self.nodes_made());
        let captured = self.captured_top.load(Ordering::Relaxed);
        [
            MemoryUsage {
                name: "dependency nodes",
                rows: nodes,
                used: nodes,
                row_bytes: (self.nodes.committed_bytes() + self.runs.committed_bytes()) as u64,
                value_bytes: 0,
            },
            MemoryUsage {
                name: "dependency edges",
                rows: live,
                used: live,
                row_bytes: self.edges.committed_bytes() as u64,
                value_bytes: 0,
            },
            MemoryUsage {
                name: "captured reads",
                rows: captured,
                used: captured,
                row_bytes: (self.captured.committed_bytes() + self.captures.committed_bytes())
                    as u64,
                value_bytes: 0,
            },
        ]
    }

    /// A write of kind `durability` that changed nothing the storage holds: what an input a
    /// host keeps elsewhere does when it changes. Its mask is all ones: no row of that kind or
    /// below is green by its summary.
    pub fn synthetic_write(&mut self, durability: Durability) {
        self.new_view(durability, ALL);
    }

    /// Opens the next view for a write of kind `durability` to the input fields whose
    /// [`input_bits`] OR to `written`: rows of that kind or below verify again; each tag's
    /// budget for the values of its `lru` functions is applied.
    pub(crate) fn new_view(&mut self, durability: Durability, written: Summary) -> u32 {
        self.next_view(written);
        for kind in &mut self.last_changed[..=durability.0 as usize] {
            *kind = self.view;
        }
        if durability.0 >= Durability::HIGH.0 {
            self.high_opened = self.opened;
        }
        self.settle();
        self.sweep();
        self.view
    }

    /// Frees, at a view boundary (`&mut`: no reader, no running row), what the dependency
    /// records of the views before no longer need:
    /// - every capture: a [`super::Reads`] is read only in the view it was made in
    ///   ([`super::Reads::for_each_read`] and [`super::replay_reads`] refuse any other), so
    ///   the capture arrays are emptied and their pages given back;
    /// - the edge spans reruns replaced, once they outnumber both the live edges and the
    ///   nodes: the live spans move into a fresh array, a walk of every row and a copy of the
    ///   live edges, so its cost is at most the dead edges it frees.
    fn settle(&mut self) {
        if *self.captures_made.get_mut() > 1 {
            self.captured.clear();
            self.captures.clear();
            *self.captured_top.get_mut() = 0;
            *self.captures_made.get_mut() = 1;
        }
        let top = *self.edges_top.get_mut();
        let dead = *self.edges_dead.get_mut();
        let live = top - dead;
        if dead <= live.max(u64::from(self.nodes_made())) {
            return;
        }
        // Named, so a build's time moving edges is its own, never its writer's self time
        // (rule 16).
        let _settle =
            tracing::info_span!(target: "blaze", "edge settle", view = self.view, live, dead)
                .entered();
        let old = std::mem::replace(
            &mut self.edges,
            Dense::growing(EDGES, Tag::Query, "dependency edges"),
        );
        let mut moved = Relocate { old, top: 0 };
        for index in 1..=ingredient::registered() {
            if self.tables[index as usize].load(Ordering::Relaxed).is_null() {
                continue;
            }
            if let Some(relocate) = ingredient::vtable(index).and_then(|it| it.relocate) {
                relocate(self, index, &mut moved);
            }
        }
        // At most `live`: a push the full array refused took its places and stored nothing.
        debug_assert!(moved.top <= live, "{} edges moved, {live} live", moved.top);
        *self.edges_top.get_mut() = moved.top;
        *self.edges_dead.get_mut() = 0;
    }

    /// Bumps the view, the clock (clearing the stop flag) and the written-field history; at
    /// the last view, renumbers first.
    fn next_view(&mut self, written: Summary) {
        if self.view >= VIEW_MASK {
            self.renumber();
        }
        self.view += 1;
        self.opened += 1;
        *self.clock.get_mut() = u64::from(self.view | VALUE);
        for at in (1..HISTORY).rev() {
            self.written[at] = or(self.written[at - 1], written);
        }
        self.written[0] = written;
    }

    /// Renumbers the views at the limit, under `&mut` (no reader, no running row): every view
    /// stamp in use (the view, `last_changed`, each node's color and last run, each row's
    /// verified view and `changed`) becomes its rank among them, so every order between two
    /// stamps is kept, and the view is the largest. The written-field history becomes all
    /// ones: a row verified before it walks once (a [`super::Reads`] of an old view is never
    /// read again: its view is past every new one until the next renumbering).
    /// `O(stamps log stamps)`, once per 2^28 writes.
    #[cold]
    fn renumber(&mut self) {
        // alloc: once per 2^28 views, a stamp per row and node
        let mut stamps: Vec<u32> = Vec::new();
        self.restamp(&mut Restamp::Collect(&mut stamps));
        stamps.sort_unstable();
        stamps.dedup();
        assert!(
            stamps.len() < VIEW_MASK as usize,
            "the query database ran out of views: all {VIEW_MASK} are stamps still in use"
        );
        self.restamp(&mut Restamp::Rank(&stamps));
        self.written = [ALL; HISTORY];
        self.renumbered = self.opened;
    }

    /// Passes every view stamp the storage holds through `restamp`, storing what it gives.
    fn restamp(&mut self, restamp: &mut Restamp<'_>) {
        self.view = restamp.apply(self.view);
        for kind in &mut self.last_changed {
            *kind = restamp.apply(*kind);
        }
        for epoch in &mut self.epochs {
            *epoch = restamp.apply(*epoch);
        }
        for node in 1..self.nodes_made() {
            let painted = self.color_of(node);
            let (verified, changed) = ((painted >> 32) as u32, painted as u32);
            let verified = if verified < FIELD { restamp.apply(verified) } else { verified };
            self.repaint(node, color(verified, restamp.apply(changed)));
            let ran = self.runs.get(node as usize);
            ran.store(restamp_run(ran.load(Ordering::Relaxed), restamp), Ordering::Relaxed);
        }
        for index in self.live() {
            if let Some(rows) = ingredient::vtable(index).and_then(|it| it.restamp) {
                rows(self, index, restamp);
            }
        }
    }

    /// Starts a fresh storage at view `view`, to test the renumbering near the limit.
    #[cfg(test)]
    pub(crate) fn start_at_view(&mut self, view: u32) {
        assert_eq!(self.nodes_made(), 1, "a fresh storage");
        self.view = view;
        self.last_changed = [view; Durability::KINDS];
        *self.clock.get_mut() = u64::from(view | VALUE);
    }

    /// Keeps each tag's `lru` values under its budget ([`super::budget`]): with `&mut`, no
    /// reader holds a value. Weighing costs the values made since the last view; a tag is
    /// passed over (every value of it read and weighed again, the oldest let go past the
    /// budget) when its values weigh more than its budget, or, under the tag's own budget,
    /// when the tag holds more than that live and has grown by a [`budget::SPARE`]th of it
    /// since its last pass: values that grew after they were weighed, such as an outline whose
    /// bodies were walked, are weighed again.
    fn sweep(&mut self) {
        self.sweep_values(&[u64::MAX; Tag::COUNT]);
    }

    /// [`Storage::sweep`] now, between two views: a build that stopped its readers to let go of
    /// what its front end keeps (`blaze-cargo`'s release) sweeps here, so values are held under
    /// their budgets mid-build, not only at the next write. Each tag's budget is the least of its
    /// own ([`Storage::value_budget`]) and `caps` (by [`Tag::index`]; `u64::MAX` for none): under
    /// memory pressure the caller caps each tag at its fair part of the daemon's limit
    /// (`blaze_graph::admission::Squeeze::caps`). Opens no view: an evicted value read again in
    /// this one runs again with its stamp kept (`evict`'s contract).
    pub fn sweep_values(&mut self, caps: &[u64; Tag::COUNT]) {
        self.swept = 0;
        if self.keep {
            return;
        }
        let count = self.weighed.get_mut().len();
        if count == 0 {
            return;
        }
        // Named, so a build's time weighing is its own, never its writer's self time (rule
        // 14): each input write opens a view and weighs here.
        let _sweep = tracing::info_span!(target: "blaze", "lru sweep", view = self.view).entered();
        let mut weight = [0u64; Tag::COUNT];
        let mut has = [false; Tag::COUNT];
        for at in 0..count {
            let index = self.weighed.get_mut()[at];
            let Some(weighed) = ingredient::vtable(index).and_then(|it| it.weighed) else {
                continue;
            };
            weight[weighed.tag.index()] += (weighed.weight)(self, index);
            has[weighed.tag.index()] = true;
        }
        let mut live = None;
        for tag in Tag::ALL {
            let at = tag.index();
            let budget = self.value_budget(tag).min(caps[at]);
            if !has[at] || budget == u64::MAX {
                continue;
            }
            // What the tag holds is the process's, so it is weighed against the tag's own
            // budget only, never against one this storage was given for its values.
            let grew = self.budgets[at].is_none() && {
                let now = live.get_or_insert_with(tag::snapshot).get(tag).live;
                now > budget && now >= self.passed_at[at].saturating_add(budget / budget::SPARE)
            };
            if weight[at] > budget || grew {
                self.swept += self.pass(tag, budget);
                self.passed_at[at] = tag::snapshot().get(tag).live;
            }
        }
    }

    /// Ends an epoch of the host's (a build), with `&mut`: lets go of every value of an
    /// evictable function (`lru` or `lru = 0`) that was read in no view of this epoch or the
    /// one before it, keeping each row's stamp, edges and hash, so a dependent still verifies
    /// against it and a read makes it again and backdates (`evict`'s contract). Returns what
    /// went.
    ///
    /// This is what lets go of the results of old revisions: a value keyed by something an
    /// edit removed (a function deleted, a macro call's old expansion, a type no code names
    /// any more) is never read again, and under a tag with no budget, or within it, nothing
    /// else lets it go. A value that is unread only because what reads it stayed green is let
    /// go too and made again if a run asks for it: the cost is time alone, bounded by what the
    /// next runs read.
    ///
    /// An epoch in which nothing was written (the view is the one the last epoch ended at) is
    /// no epoch: it lets nothing go and ages nothing, so builds that change nothing never empty
    /// the storage. While [`Storage::keep_every_value`] is set nothing goes either.
    pub fn end_epoch(&mut self) -> Unread {
        let [before, last] = self.epochs;
        if self.keep || self.view == last {
            return Unread::default();
        }
        self.epochs = [last, self.view];
        // Named, so a build's time walking rows is its own, never its writer's self time
        // (rule 16).
        let span = tracing::info_span!(
            target: "blaze",
            "unread values",
            view = self.view,
            before,
            values = tracing::field::Empty,
            bytes = tracing::field::Empty,
        );
        let _entered = span.enter();
        // alloc: once an epoch, the live ingredients
        let indices: Vec<u32> = self.live().collect();
        let mut gone = Unread::default();
        for index in indices {
            if let Some(unread) = ingredient::vtable(index).and_then(|it| it.unread) {
                let (values, bytes) = unread(self, index, before);
                gone.values += values;
                gone.bytes += bytes;
            }
        }
        span.record("values", gone.values);
        span.record("bytes", gone.bytes);
        gone
    }

    /// Lets go of every value of every function of `tag` whose values go only when asked
    /// (`lru = 0`), keeping each row's stamp, edges and hash, so a dependent still verifies
    /// against it and a read makes it again and backdates (`evict`'s contract); returns how many
    /// values went. A tag, not a list of functions: a build lets go of every MIR as it ends
    /// (`hir::evict::drop_mir`), and a MIR query added later goes with the rest by its
    /// `mem_tag`, never kept because a list was not told of it (coroutine witnesses and
    /// location literals were, before: tagged MIR, never let go).
    pub fn evict_tag(&mut self, tag: Tag) -> usize {
        // alloc: once a release, the live ingredients
        let indices: Vec<u32> = self.live().collect();
        let mut evicted = 0;
        for index in indices {
            if let Some(explicit) = ingredient::vtable(index).and_then(|it| it.explicit)
                && explicit.tag == tag
            {
                evicted += (explicit.evict)(self, index);
            }
        }
        evicted
    }

    /// How many values the functions of `tag` whose values go only when asked (`lru = 0`)
    /// hold, from their counters: 0 once [`Storage::evict_tag`] let go of them and nothing read
    /// one again.
    pub fn values_of(&self, tag: Tag) -> usize {
        self.live()
            .filter_map(|index| Some((ingredient::vtable(index)?.explicit?, index)))
            .filter(|(explicit, _)| explicit.tag == tag)
            .map(|(explicit, index)| (explicit.values)(self, index))
            .sum()
    }

    /// Weighs every value of `tag`'s `lru` functions and, past `budget`, lets the least
    /// recently verified go ([`budget::plan`]); returns how many values it read.
    #[cold]
    fn pass(&mut self, tag: Tag, budget: u64) -> usize {
        // Named, so a build's time in passes is the pass's, never its writer's self time
        // (rule 16): each input write opens a view, and any view may pass.
        let span = tracing::info_span!(
            target: "blaze",
            "lru pass",
            tag = tag.name(),
            view = self.view,
            budget,
            values = tracing::field::Empty,
            bytes = tracing::field::Empty,
            evicted = tracing::field::Empty,
            freed = tracing::field::Empty,
        );
        let _pass = span.enter();
        let count = self.weighed.get_mut().len();
        let mut values: Vec<Held> = Vec::new(); // alloc: a pass's values, once per eighth of a tag's budget made
        let mut weight = 0;
        for at in 0..count {
            let index = self.weighed.get_mut()[at];
            let Some(weighed) = ingredient::vtable(index).and_then(|it| it.weighed) else {
                continue;
            };
            if weighed.tag == tag {
                weight += (weighed.held)(self, index, &mut values);
            }
        }
        let gone = budget::plan(&mut values, weight, budget);
        let victims = &mut values[..gone];
        victims.sort_unstable_by_key(|it| (it.ingredient, it.key));
        let (mut evicted, mut freed) = (0, 0);
        for one in victims.chunk_by(|a, b| a.ingredient == b.ingredient) {
            let index = one[0].ingredient;
            let Some(weighed) = ingredient::vtable(index).and_then(|it| it.weighed) else {
                continue;
            };
            let (n, bytes) = (weighed.evict)(self, index, one);
            evicted += n;
            freed += bytes;
        }
        span.record("values", values.len());
        span.record("bytes", weight);
        span.record("evicted", evicted);
        span.record("freed", freed);
        values.len()
    }

    /// The values the last view's passes read, over every tag: what it cost, which follows the
    /// values held when a tag passes and is 0 when none does.
    #[cfg(test)]
    pub(crate) fn swept(&self) -> usize {
        self.swept
    }

    pub(crate) fn add_weighed(&self, index: u32) {
        self.weighed.lock().push(index);
    }

    /// The ingredient that owns `id`'s page.
    #[inline]
    pub fn owner(&self, id: Id) -> Option<u32> {
        match self.owners.get(id.page()).load(Ordering::Acquire) {
            0 => None,
            owner => Some(owner),
        }
    }

    /// Whether `id` is an interned value's: its page's owner is an interned ingredient, and its
    /// slot was interned. False for every other `id`: one from outside (a persisted `u32`, a
    /// proc-macro server's answer) is checked with this before `interned_fields` panics on it.
    pub fn made(&self, id: Id) -> bool {
        let Some(owner) = self.owner(id) else { return false };
        ingredient::vtable(owner).and_then(|it| it.made).is_some_and(|made| made(self, owner, id))
    }

    pub fn ingredient_name(&self, ingredient: u32) -> &'static str {
        ingredient::name(ingredient)
    }

    /// The name of field `field` of ingredient `ingredient`, by the number a read of it
    /// carries ([`super::ReadKind::InputField`], [`super::ReadKind::TrackedField`]).
    pub fn field_name(&self, ingredient: u32, field: u32) -> &'static str {
        let vtable = ingredient::vtable(ingredient)
            .unwrap_or_else(|| panic!("no ingredient {ingredient} is registered"));
        vtable.fields.get(field as usize).copied().unwrap_or_else(|| {
            panic!(
                "`{}` has {} named fields, not a field {field}",
                vtable.name,
                vtable.fields.len()
            )
        })
    }

    /// What a read is named by: a field of an input or tracked struct by the field's name,
    /// anything else by its ingredient's.
    pub fn read_name(&self, key: super::DatabaseKeyIndex, kind: super::ReadKind) -> &'static str {
        match kind {
            super::ReadKind::InputField(field) | super::ReadKind::TrackedField(field) => {
                self.field_name(key.ingredient_index(), field)
            }
            super::ReadKind::Function | super::ReadKind::Interned => {
                self.ingredient_name(key.ingredient_index())
            }
        }
    }

    /// A fresh ID page owned by `ingredient`.
    pub(crate) fn new_page(&self, ingredient: u32) -> usize {
        let page = self.next_page.fetch_add(1, Ordering::Relaxed) as usize;
        assert!(page < MAX_PAGES, "the query database ran out of ID pages ({MAX_PAGES})");
        // Release: whoever sees an ID of the page sees its owner.
        self.owners.get_to_write(page).store(ingredient, Ordering::Release);
        page
    }

    /// ID pages made so far: every page index is below it.
    pub(crate) fn pages_made(&self) -> usize {
        self.next_page.load(Ordering::Relaxed) as usize
    }

    /// `count` fresh nodes, consecutive, the first returned; their keys and colors zero.
    pub(crate) fn new_nodes(&self, count: u32) -> u32 {
        let first = self.next_node.fetch_add(count, Ordering::Relaxed);
        let end = first as usize + count as usize;
        assert!(end <= NODES, "the query database ran out of nodes ({NODES})");
        self.nodes.ensure(end);
        self.runs.ensure(end);
        first
    }

    /// Nodes made so far.
    pub(crate) fn nodes_made(&self) -> u32 {
        self.next_node.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub(crate) fn color_of(&self, node: u32) -> u64 {
        self.node(node).color()
    }

    /// Paints `node`: colors only go up, so racing painters of one view end at its color.
    #[inline]
    pub(crate) fn paint(&self, node: u32, color: u64) {
        self.node(node).paint(color);
    }

    /// Sets `node`'s color outright, for its one writer (a field's setter, a cycle's reset).
    #[inline]
    pub(crate) fn repaint(&self, node: u32, color: u64) {
        self.node(node).color.store(color, Ordering::Release);
    }

    #[inline]
    pub(crate) fn key_of(&self, node: u32) -> u64 {
        self.node(node).key()
    }

    pub(crate) fn set_key(&self, node: u32, key: u64) {
        self.node(node).key.store(key, Ordering::Relaxed);
    }

    /// Records that memo `node` runs in this view because of `cause`: its flight holder's one
    /// store before the run, read back by [`Storage::provenance`]. Each run counts under its
    /// cause ([`counts::RAN_NEW`], [`counts::RAN_CHANGED`], [`counts::RAN_UNTRACKED`],
    /// [`counts::RAN_CYCLE`], and [`counts::EVICTED_ASKED`] for an evicted value); a run in the
    /// view it last ran in counts as [`counts::RERAN`] too. While the run log is on, every run
    /// but a first is logged with its cause ([`provenance::RunLog::log`]).
    #[inline]
    pub(crate) fn record_run(&self, node: u32, cause: Cause) {
        let run = self.runs.get(node as usize);
        let was = run.load(Ordering::Relaxed);
        run.store(cause.word(self.view), Ordering::Relaxed);
        let reran = ((was >> 32) as u32) & VIEW_MASK == self.view;
        let key = self.key_of(node);
        let (index, _, _) = key_parts(key);
        if reran {
            counts::count(index, counts::RERAN);
        }
        let (what, read) = match cause {
            Cause::New => (counts::RAN_NEW, None),
            Cause::Changed(read) => (counts::RAN_CHANGED, Some(read)),
            Cause::Evicted => (counts::EVICTED_ASKED, None),
            Cause::Untracked => (counts::RAN_UNTRACKED, None),
            Cause::Cycle => (counts::RAN_CYCLE, None),
        };
        counts::count(index, what);
        if cause != Cause::New && provenance::logging() {
            let read = read
                .filter(|&it| it != 0 && it < self.nodes_made())
                .map_or(0, |it| self.key_of(it));
            self.run_log.log(key, read, self.view, provenance::Logged::Ran(cause));
        }
    }

    /// Copies `edges` into the edge array: the span of the row of `owner`'s run (its ingredient
    /// and key), which an error names.
    pub(crate) fn push_edges(&self, edges: &[u32], owner: (u32, Id)) -> u64 {
        let len = edges.len() as u64;
        if len == 0 {
            return 0;
        }
        if len > span::MAX_LEN {
            self.too_many_reads(len, owner, "the run of");
        }
        let start = self.edges_top.fetch_add(len, Ordering::Relaxed);
        let end = start + len;
        if end > self.edge_limit {
            self.edges_full(start, len, owner);
        }
        self.edges.ensure(end as usize);
        for (place, &node) in self.edges.slice(start as usize, edges.len()).iter().zip(edges) {
            // Relaxed: the span is published by the row's (or capture's) own release store.
            place.store(node, Ordering::Relaxed);
        }
        span::pack(start, len)
    }

    /// Edge `at` of a published span.
    #[inline(always)]
    pub(crate) fn edge(&self, at: u64) -> u32 {
        self.edges.get(at as usize).load(Ordering::Relaxed)
    }

    /// The span's edges are no longer named.
    pub(crate) fn forget_edges(&self, span: u64) {
        self.edges_dead.fetch_add(span::len(span), Ordering::Relaxed);
    }

    #[cold]
    #[inline(never)]
    fn edges_full(&self, start: u64, len: u64, (ingredient, id): (u32, Id)) -> ! {
        let dead = self.edges_dead.load(Ordering::Relaxed);
        panic!(
            "the query database's edge array is full ({} places: {} named by rows, {dead} left by \
             reruns since the last view) storing the {len} reads of `{}`({id:?})",
            self.edge_limit,
            start.saturating_sub(dead),
            self.ingredient_name(ingredient),
        )
    }

    #[cold]
    #[inline(never)]
    fn too_many_reads(&self, len: u64, (ingredient, id): (u32, Id), what: &str) -> ! {
        panic!(
            "{what} `{}`({id:?}) holds {len} distinct reads and captures, more than a span holds ({})",
            self.ingredient_name(ingredient),
            span::MAX_LEN,
        )
    }

    /// Stores the entries of a capture made in the run of `owner` ([`super::capture_reads`]):
    /// its number in this view, 0 for none.
    pub(crate) fn push_capture(&self, entries: &[u32], owner: (u32, Id)) -> u32 {
        let len = entries.len() as u64;
        if len == 0 {
            return 0;
        }
        if len > span::MAX_LEN {
            self.too_many_reads(len, owner, "a capture in");
        }
        let start = self.captured_top.fetch_add(len, Ordering::Relaxed);
        let end = start + len;
        let number = self.captures_made.fetch_add(1, Ordering::Relaxed);
        if end > CAPTURED as u64 || number as usize >= CAPTURES {
            self.captures_full(start, len, number, owner);
        }
        self.captured.ensure(end as usize);
        for (place, &entry) in
            self.captured.slice(start as usize, entries.len()).iter().zip(entries)
        {
            // Relaxed: the capture is published with its `Reads`, by whatever hands that over.
            place.store(entry, Ordering::Relaxed);
        }
        self.captures.ensure(number as usize + 1);
        self.captures.get(number as usize).store(span::pack(start, len), Ordering::Relaxed);
        number
    }

    #[cold]
    #[inline(never)]
    fn captures_full(&self, start: u64, len: u64, number: u32, (ingredient, id): (u32, Id)) -> ! {
        panic!(
            "the query database's captured reads are full in view {} ({start} of {CAPTURED} entries, \
             {number} of {CAPTURES} captures) storing the {len} entries of a capture in `{}`({id:?})",
            self.opened,
            self.ingredient_name(ingredient),
        )
    }

    /// The span in the captured entries of capture `number`, made in this view.
    #[inline]
    pub(crate) fn capture_span(&self, number: u32) -> u64 {
        self.captures.get(number as usize).load(Ordering::Relaxed)
    }

    /// Captured entry `at` of a published capture.
    #[inline(always)]
    pub(crate) fn captured(&self, at: u64) -> u32 {
        self.captured.get(at as usize).load(Ordering::Relaxed)
    }

    /// The edge array's top and the dead edges under it, and the captured entries' top.
    #[cfg(test)]
    pub(crate) fn edge_counts(&self) -> (u64, u64, u64) {
        (
            self.edges_top.load(Ordering::Relaxed),
            self.edges_dead.load(Ordering::Relaxed),
            self.captured_top.load(Ordering::Relaxed),
        )
    }

    /// Lowers the edges the array may hold, to test the error of a full one.
    #[cfg(test)]
    pub(crate) fn limit_edges(&mut self, limit: u64) {
        assert!(limit <= EDGES as u64);
        self.edge_limit = limit;
    }

    /// Moves `span` from the old edge array into the new one ([`Storage::settle`] only).
    pub(crate) fn relocate(&self, moved: &mut Relocate, span: u64) -> u64 {
        let len = span::len(span);
        if len == 0 {
            return 0;
        }
        let start = moved.top;
        moved.top += len;
        self.edges.ensure(moved.top as usize);
        let from = moved.old.slice(span::start(span) as usize, len as usize);
        for (to, from) in self.edges.slice(start as usize, len as usize).iter().zip(from) {
            to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        span::pack(start, len)
    }

    /// Calls the hook, if any, with what `kind` makes: cold paths only.
    #[inline]
    pub(crate) fn event(&self, kind: impl FnOnce() -> EventKind) {
        if let Some(hook) = &self.hook {
            hook(self, Event { kind: kind() });
        }
    }

    pub(crate) fn has_hook(&self) -> bool {
        self.hook.is_some()
    }

    /// The row-page directory of ingredient `index`, null before its first use: once published,
    /// a [`Pages::dir`], valid for every index below `MAX_PAGES`.
    #[inline(always)]
    pub(crate) fn dir(&self, index: u32) -> *const AtomicPtr<u8> {
        self.dirs[index as usize].load(Ordering::Acquire)
    }

    /// The table of ingredient `index` as `T`, if made. The caller names the `T` the
    /// ingredient's kind makes.
    #[inline]
    pub(crate) fn table<T>(&self, index: u32) -> Option<&T> {
        let at = self.tables[index as usize].load(Ordering::Acquire);
        // SAFETY: a table pointer is published once (release, under `making`) from a leaked
        // `Box<T>` of the ingredient's own table type, which every caller names, and freed
        // only by `Drop` with `&mut`.
        unsafe { at.cast::<T>().as_ref() }
    }

    /// The table of `ingredient` as `T`, made by `make` on first use.
    #[inline]
    pub(crate) fn table_or_make<T: Table>(&self, ingredient: &'static Ingredient) -> (&T, u32) {
        let index = ingredient.index();
        match self.table::<T>(index) {
            Some(table) => (table, index),
            None => (self.make_table::<T>(index), index),
        }
    }

    #[cold]
    #[inline(never)]
    fn make_table<T: Table>(&self, index: u32) -> &T {
        let _one = self.making.lock();
        if let Some(table) = self.table::<T>(index) {
            return table;
        }
        // alloc: once per ingredient per storage
        let table = Box::new(T::new(self, index));
        let dir = table.pages().dir();
        let at = Box::into_raw(table);
        self.dirs[index as usize].store(dir.as_ptr(), Ordering::Release);
        self.tables[index as usize].store(at.cast(), Ordering::Release);
        // SAFETY: just leaked, freed only by `Drop`.
        unsafe { &*at }
    }

    /// Every function of this storage shares one database type (its vtables cast to it).
    pub(crate) fn check_db<D: 'static>(&self, name: &str) {
        let first = *self.db_type.lock().get_or_insert(TypeId::of::<D>());
        assert!(
            first == TypeId::of::<D>(),
            "{name} is used with a second database type on one storage ({})",
            std::any::type_name::<D>()
        );
    }

    /// The table pointer of `index` for its own vtable's `drop`, taken out.
    pub(crate) fn take_table<T>(&mut self, index: u32) -> Option<Box<T>> {
        let at = std::mem::replace(self.tables[index as usize].get_mut(), ptr::null_mut());
        *self.dirs[index as usize].get_mut() = ptr::null_mut();
        // SAFETY: as in `table`: a leaked `Box<T>` of this ingredient's type, now ours alone.
        NonNull::new(at.cast::<T>()).map(|at| unsafe { Box::from_raw(at.as_ptr()) })
    }

    /// The flight key of row `id` of ingredient `index`.
    #[inline]
    pub(crate) fn flight(index: u32, id: Id) -> Key {
        let word = u64::from(id.as_u32()) | (u64::from(index) << 32);
        let mixed = word.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let mut key = [0; 32];
        key[..8].copy_from_slice(&mixed.to_le_bytes());
        key[8..16].copy_from_slice(&mixed.rotate_left(29).to_le_bytes());
        key[16..24].copy_from_slice(&word.to_le_bytes());
        Key(key)
    }

    /// The live ingredients' indices.
    pub(crate) fn live(&self) -> impl Iterator<Item = u32> + '_ {
        (1..=ingredient::registered())
            .filter(|&index| !self.tables[index as usize].load(Ordering::Relaxed).is_null())
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        let indices: Vec<u32> = self.live().collect(); // alloc: once, at the end of a storage
        for index in indices {
            if let Some(vtable) = ingredient::vtable(index) {
                (vtable.drop)(self, index);
            }
        }
    }
}

impl fmt::Debug for Storage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Storage")
            .field("view", &self.view)
            .field("pages", &self.pages_made())
            .field("nodes", &self.nodes_made())
            .field("edges", &self.edges_top.load(Ordering::Relaxed))
            .field("dead edges", &self.edges_dead.load(Ordering::Relaxed))
            .field("captured", &self.captured_top.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// An ingredient's table type.
pub(crate) trait Table: Send + Sync + Sized + 'static {
    fn new(storage: &Storage, index: u32) -> Self;
    fn pages(&self) -> &Pages;
}

/// The renumbering's two passes over every view stamp: collect the stamps in use (0, "never",
/// is not one), then replace each with its rank among them, from 1.
pub(crate) enum Restamp<'a> {
    Collect(&'a mut Vec<u32>),
    /// The stamps in use, sorted, unique.
    Rank(&'a [u32]),
}

impl Restamp<'_> {
    pub(crate) fn apply(&mut self, stamp: u32) -> u32 {
        match self {
            _ if stamp == 0 => 0,
            Restamp::Collect(stamps) => {
                stamps.push(stamp); // alloc: the renumbering's stamp list, once per 2^28 views
                stamp
            }
            Restamp::Rank(stamps) => {
                stamps.binary_search(&stamp).expect("a stamp the first pass collected") as u32 + 1
            }
        }
    }
}

/// The old edge array, while [`Storage::settle`] copies the live spans out of it.
pub(crate) struct Relocate {
    old: Dense<AtomicU32>,
    top: u64,
}

/// IDs of one ingredient, dense: its `n`-th value is slot `n % PAGE_LEN` of its
/// `n / PAGE_LEN`-th page.
pub(crate) struct Ids {
    next: AtomicU32,
    pages: Dense<AtomicU32>,
    #[expect(clippy::disallowed_types, reason = "held only while one of an ingredient's ID pages is made")]
    making: blaze_core::lock::Lock<()>,
}

impl Ids {
    #[expect(clippy::disallowed_types, reason = "makes the locks its fields allow")]
    pub(crate) fn new(tag: Tag) -> Ids {
        Ids {
            next: AtomicU32::new(0),
            pages: Dense::growing(MAX_PAGES, tag, "an ingredient's ID pages"),
            making: blaze_core::lock::Lock::new("query.ids.making", ()),
        }
    }

    /// A fresh ID of ingredient `index`, its row page made in `pages`.
    pub(crate) fn next(
        &self,
        storage: &Storage,
        index: u32,
        pages: &Pages,
        heap: &Heap,
        layout: (usize, usize),
    ) -> Id {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let (k, slot) = ((n >> PAGE_BITS) as usize, n as usize & (PAGE_LEN - 1));
        let page = match self.pages.get_checked(k).map(|it| it.load(Ordering::Acquire)) {
            Some(page @ 1..) => page as usize,
            _ => self.page(storage, index, k, pages, heap, layout),
        };
        Id::at(page, slot)
    }

    #[cold]
    fn page(
        &self,
        storage: &Storage,
        index: u32,
        k: usize,
        pages: &Pages,
        heap: &Heap,
        layout: (usize, usize),
    ) -> usize {
        let _one = self.making.lock();
        self.pages.ensure(k + 1);
        if let page @ 1.. = self.pages.get(k).load(Ordering::Acquire) {
            return page as usize;
        }
        let page = storage.new_page(index);
        pages.page_or_make(page, heap, layout.0, layout.1);
        self.pages.get(k).store(page as u32, Ordering::Release);
        page
    }

    /// How many IDs were given.
    pub(crate) fn len(&self) -> u32 {
        self.next.load(Ordering::Relaxed)
    }

    /// Every ID given, in order (`&mut`: drop).
    pub(crate) fn all(&self) -> impl Iterator<Item = Id> + '_ {
        (0..self.len()).map(|n| {
            let page = self.pages.get((n >> PAGE_BITS) as usize).load(Ordering::Acquire);
            Id::at(page as usize, n as usize & (PAGE_LEN - 1))
        })
    }
}

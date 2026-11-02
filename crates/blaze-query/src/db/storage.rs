//! The dense key space's [`Storage`]: one per database, holding every ingredient's table, the
//! ID pages, the dependency graph's nodes and edges, and the view.

use std::{
    any::TypeId,
    fmt,
    ptr::{self, NonNull},
    sync::{
        OnceLock,
        atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering},
    },
};

use blaze_graph::{Key, pump::Flights};
use blaze_mem::Tag;
use parking_lot::Mutex;

use super::{
    Event, EventKind, counts,
    dense::{Dense, Heap},
    id::{Id, MAX_PAGES, PAGE_BITS, PAGE_LEN},
    ingredient::{self, Ingredient, MAX_INGREDIENTS},
    provenance::{Cause, restamp_run},
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

/// An input's kind, by how often it changes: workspace files (`LOW`), workspace structure
/// (`MEDIUM`), libraries and frozen tables (`HIGH`). A row's durability is the lowest kind it
/// read; a row whose kind did not change since it was verified verifies with one compare.
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

impl Default for Durability {
    fn default() -> Durability {
        Durability::LOW
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
const HEAP: usize = 1 << 36;
const INTERN_HEAP: usize = 1 << 35;
/// Nodes and edges a storage may have.
const NODES: usize = 1 << 28;
const EDGES: usize = 1 << 32;

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
    /// `written[i]`: the OR of the [`input_bits`] of every field written in the last `i + 1`
    /// views.
    written: [Summary; HISTORY],
    /// Each ingredient's row-page directory, null until first use.
    dirs: Box<[AtomicPtr<AtomicPtr<u8>>; MAX_INGREDIENTS]>,
    /// Each ingredient's table, null until first use.
    tables: Box<[AtomicPtr<()>; MAX_INGREDIENTS]>,
    making: Mutex<()>,
    pub(crate) heap: Heap,
    pub(crate) intern_heap: Heap,
    owners: Owners,
    next_page: AtomicU32,
    pub(crate) colors: Dense<AtomicU64>,
    pub(crate) summaries: Dense<[AtomicU64; 2]>,
    pub(crate) keys: Dense<AtomicU64>,
    /// Each memo's last run: its view and [`Cause`], written by the run's flight holder.
    pub(crate) runs: Dense<AtomicU64>,
    next_node: AtomicU32,
    edges: Dense<AtomicU32>,
    edges_top: AtomicU64,
    /// Edges no row names any more (replaced spans, captures).
    edges_dead: AtomicU64,
    pub(crate) flights: Flights<()>,
    hook: Option<Hook>,
    /// The ingredients with an `lru` cap, swept at each new view.
    lru: Mutex<Vec<u32>>,
    /// [`Storage::keep_every_value`].
    keep: bool,
    db_type: OnceLock<TypeId>,
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
            written: [[0; 2]; HISTORY],
            dirs,
            tables,
            making: Mutex::new(()),
            heap: Heap::new(HEAP, Tag::Query),
            intern_heap: Heap::new(INTERN_HEAP, Tag::Intern),
            owners: Owners::committed(MAX_PAGES, Tag::Query),
            next_page: AtomicU32::new(1),
            colors: Dense::growing(NODES, Tag::Query),
            summaries: Dense::growing(NODES, Tag::Query),
            keys: Dense::growing(NODES, Tag::Query),
            runs: Dense::growing(NODES, Tag::Query),
            next_node: AtomicU32::new(1),
            edges: Dense::growing(EDGES, Tag::Query),
            edges_top: AtomicU64::new(0),
            edges_dead: AtomicU64::new(0),
            flights: Flights::new(),
            hook,
            // alloc: none, empty: one entry per `lru` function, pushed once
            lru: Mutex::new(Vec::new()),
            keep: false,
            db_type: OnceLock::new(),
        }
    }

    #[inline]
    pub fn view(&self) -> View {
        View(self.opened)
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
    pub(crate) fn inputs_unchanged(&self, node: u32, verified: u32) -> bool {
        let Some(back) = self.view.checked_sub(verified) else { return false };
        let Some(&written) = back.checked_sub(1).and_then(|at| self.written.get(at as usize))
        else {
            return back == 0;
        };
        let summary = self.summary_of(node);
        (summary[0] & written[0]) | (summary[1] & written[1]) == 0
    }

    #[inline(always)]
    pub(crate) fn summary_of(&self, node: u32) -> Summary {
        let [a, b] = self.summaries.get(node as usize);
        // Relaxed: a summary is stored before its row's word or its node's color is published
        // (release), which the reader acquired.
        [a.load(Ordering::Relaxed), b.load(Ordering::Relaxed)]
    }

    /// Sets `node`'s summary, for its one writer (the run that holds its flight, an input's
    /// maker, a tracked struct's creator).
    #[inline]
    pub(crate) fn set_summary(&self, node: u32, summary: Summary) {
        let [a, b] = self.summaries.get(node as usize);
        a.store(summary[0], Ordering::Relaxed);
        b.store(summary[1], Ordering::Relaxed);
    }

    /// Adds `summary`'s bits to `node`'s: a walk's, which racing walkers of a view agree on.
    #[inline]
    pub(crate) fn grow_summary(&self, node: u32, summary: Summary) {
        let [a, b] = self.summaries.get(node as usize);
        a.fetch_or(summary[0], Ordering::Relaxed);
        b.fetch_or(summary[1], Ordering::Relaxed);
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

    /// While `keep`, no `lru` cap evicts at a new view.
    pub fn keep_every_value(&mut self, keep: bool) {
        self.keep = keep;
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

    /// What each ingredient in use holds, pushed onto `out`.
    pub fn memory_usage(&self, out: &mut Vec<super::MemoryUsage>) {
        for index in self.live() {
            if let Some(vtable) = ingredient::vtable(index) {
                // alloc: the caller's report, one entry per ingredient
                out.push((vtable.memory)(self, index));
            }
        }
    }

    /// A write of kind `durability` that changed nothing the storage holds: what an input a
    /// host keeps elsewhere does when it changes. Its mask is all ones: no row of that kind or
    /// below is green by its summary.
    pub fn synthetic_write(&mut self, durability: Durability) {
        self.new_view(durability, ALL);
    }

    /// Opens the next view for a write of kind `durability` to the input fields whose
    /// [`input_bits`] OR to `written`: rows of that kind or below verify again; `lru` caps are
    /// applied.
    pub(crate) fn new_view(&mut self, durability: Durability, written: Summary) -> u32 {
        self.next_view(written);
        for kind in &mut self.last_changed[..=durability.0 as usize] {
            *kind = self.view;
        }
        self.sweep();
        self.view
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
    }

    /// Passes every view stamp the storage holds through `restamp`, storing what it gives.
    fn restamp(&mut self, restamp: &mut Restamp<'_>) {
        self.view = restamp.apply(self.view);
        for kind in &mut self.last_changed {
            *kind = restamp.apply(*kind);
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

    /// Applies every `lru` cap: with `&mut`, no reader holds a value.
    fn sweep(&mut self) {
        if self.keep {
            return;
        }
        let count = self.lru.get_mut().len();
        for at in 0..count {
            let index = self.lru.get_mut()[at];
            if let Some(sweep) = ingredient::vtable(index).and_then(|it| it.sweep) {
                sweep(self, index);
            }
        }
    }

    pub(crate) fn add_lru(&self, index: u32) {
        self.lru.lock().push(index);
    }

    /// The ingredient that owns `id`'s page.
    #[inline]
    pub fn owner(&self, id: Id) -> Option<u32> {
        if id.page() >= MAX_PAGES {
            return None;
        }
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
        self.owners.get(page).store(ingredient, Ordering::Release);
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
        self.colors.ensure(end);
        self.summaries.ensure(end);
        self.keys.ensure(end);
        self.runs.ensure(end);
        first
    }

    /// Nodes made so far.
    pub(crate) fn nodes_made(&self) -> u32 {
        self.next_node.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub(crate) fn color_of(&self, node: u32) -> u64 {
        self.colors.get(node as usize).load(Ordering::Acquire)
    }

    /// Paints `node`: colors only go up, so racing painters of one view end at its color.
    #[inline]
    pub(crate) fn paint(&self, node: u32, color: u64) {
        self.colors.get(node as usize).fetch_max(color, Ordering::Release);
    }

    /// Sets `node`'s color outright, for its one writer (a field's setter, a cycle's reset).
    #[inline]
    pub(crate) fn repaint(&self, node: u32, color: u64) {
        self.colors.get(node as usize).store(color, Ordering::Release);
    }

    #[inline]
    pub(crate) fn key_of(&self, node: u32) -> u64 {
        self.keys.get(node as usize).load(Ordering::Relaxed)
    }

    pub(crate) fn set_key(&self, node: u32, key: u64) {
        self.keys.get(node as usize).store(key, Ordering::Relaxed);
    }

    /// Records that memo `node` runs in this view because of `cause`: its flight holder's one
    /// store before the run, read back by [`Storage::provenance`]. A run in the view it last
    /// ran in counts as [`counts::RERAN`].
    #[inline]
    pub(crate) fn record_run(&self, node: u32, cause: Cause) {
        let run = self.runs.get(node as usize);
        let was = run.load(Ordering::Relaxed);
        run.store(cause.word(self.view), Ordering::Relaxed);
        if ((was >> 32) as u32) & VIEW_MASK == self.view {
            let (index, _, _) = key_parts(self.key_of(node));
            counts::count(index, counts::RERAN);
        }
    }

    /// What each ingredient's rows did since the last call, in every database of this process
    /// (ingredients are numbered process-wide), summed over threads: `each(name, executed,
    /// verified, reran)` per ingredient that did anything, named as its query is. Executed is a
    /// run, verified a row proved unchanged in a view without running, reran a run in a view
    /// the row had already run in.
    pub fn take_counts(&self, each: impl FnMut(&str, u64, u64, u64)) {
        counts::take(each);
    }

    /// Copies `edges` into the edge array: a span for a row or a capture.
    pub(crate) fn push_edges(&self, edges: &[u32]) -> u64 {
        let len = edges.len() as u64;
        if len == 0 {
            return 0;
        }
        let start = self.edges_top.fetch_add(len, Ordering::Relaxed);
        let end = start + len;
        assert!(end as usize <= EDGES, "the query database's edge array is full ({EDGES})");
        self.edges.ensure(end as usize);
        for (at, &node) in edges.iter().enumerate() {
            // Relaxed: the span is published by the row's (or capture's) own release store.
            self.edges.get(start as usize + at).store(node, Ordering::Relaxed);
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

    /// Settles the edge array once it is more than twice what rows name: copies each live
    /// span into a fresh array. Opens a new view (nothing changed: every row verifies by its
    /// durability, one compare), so no [`super::Reads`] of the old array is read again.
    pub fn compact(&mut self) {
        let top = *self.edges_top.get_mut();
        let live = top - *self.edges_dead.get_mut();
        if top <= 2 * live.max(1 << 16) {
            return;
        }
        let old = std::mem::replace(&mut self.edges, Dense::growing(EDGES, Tag::Query));
        let mut moved = Relocate { old, top: 0 };
        for index in 1..=ingredient::registered() {
            if self.tables[index as usize].load(Ordering::Relaxed).is_null() {
                continue;
            }
            if let Some(relocate) = ingredient::vtable(index).and_then(|it| it.relocate) {
                relocate(self, index, &mut moved);
            }
        }
        *self.edges_top.get_mut() = moved.top;
        *self.edges_dead.get_mut() = 0;
        self.next_view([0; 2]);
    }

    /// Moves `span` from the old edge array into the new one (compaction only).
    pub(crate) fn relocate(&self, moved: &mut Relocate, span: u64) -> u64 {
        let len = span::len(span);
        if len == 0 {
            return 0;
        }
        let start = moved.top;
        moved.top += len;
        self.edges.ensure(moved.top as usize);
        let from = span::start(span);
        for at in 0..len {
            let node = moved.old.get((from + at) as usize).load(Ordering::Relaxed);
            self.edges.get((start + at) as usize).store(node, Ordering::Relaxed);
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

    /// The row-page directory of ingredient `index`, null before its first use.
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
        let first = *self.db_type.get_or_init(TypeId::of::<D>);
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

/// The old edge array, while compaction copies spans out of it.
pub(crate) struct Relocate {
    old: Dense<AtomicU32>,
    top: u64,
}

/// IDs of one ingredient, dense: its `n`-th value is slot `n % PAGE_LEN` of its
/// `n / PAGE_LEN`-th page.
pub(crate) struct Ids {
    next: AtomicU32,
    pages: Dense<AtomicU32>,
    making: Mutex<()>,
}

impl Ids {
    pub(crate) fn new(tag: Tag) -> Ids {
        Ids {
            next: AtomicU32::new(0),
            pages: Dense::growing(MAX_PAGES, tag),
            making: Mutex::new(()),
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

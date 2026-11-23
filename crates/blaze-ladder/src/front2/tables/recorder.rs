//! The per-task recorder of every read (engine.md, E2): ONE list a task fills while it runs,
//! of the names cells its walks and rows consulted (lane CELL's [`NameCell`], put through
//! [`CellSink`]; a crate's lang rows are one, [`NameCell::Lang`]), the heads it read and the impl buckets it read. The
//! list is sorted and deduplicated ONCE, when the task ends ([`Recorder::finish`], rule 25):
//! what a task read is then a function of its questions, never of the order it asked them.
//!
//! A recorder is the task's own (made in its frame, dropped with it); the tables it records
//! reads of hold no per-task cache, so a read is listed for EVERY task that asks it, not only
//! for the first (front2 reads.rs, "What is not listed": the old world's cause).
//!
//! A read is not given its digest here: the merge decides each cell's digest from the same
//! tables a later run checks it against (names_cells.rs `seen_in`), so the two cannot disagree.

use core::cell::{Cell, RefCell};

use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_scope::cell_key::BindingKeys;
use blaze_scope::names_cells::{CellSink, NameCell, Section};
use blaze_surface::Namespace;

/// The most reads one task keeps before it is refused (rule 18): a body that reads more is
/// [`ReadsFull`], never a list cut short.
pub const MOST_READS: usize = 1 << 20;

/// One read of a task. Ordered (and so sorted) by its derived `Ord`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TableRead {
    /// A cell of a crate's names, as lane CELL defines it (its `CrateScope` record's cell table).
    Names(NameCell),
    /// Tokens consumed while reading the declaration. The CLI places this on the
    /// signature part of the physical file, or the expansion that supplied it.
    Signature { key: ItemKey },
    /// Tokens consumed while lowering or evaluating the definition, including its body.
    Text { key: ItemKey },
    /// A head read whole: the item by its checker key's 16 bytes (front2 reads.rs `Fact::key`,
    /// big-endian) and which part (the `FactKind` byte: signature, predicates, ADT, ..).
    Head { item: [u8; 16], part: u8 },
    /// One crate's impl bucket (crate: its key's first 16 bytes, all zero for a fact of the
    /// whole graph; bucket: [`bucket_cell`], or [`UNFILED`], or [`INCOHERENT`]).
    Impls { krate: [u8; 16], bucket: [u8; 16] },
}

/// The bucket cell of a crate's list of impls filed under no bucket.
pub const UNFILED: [u8; 16] = [0xff; 16];
/// The bucket cell of the one fact "which crates may write incoherent impls".
pub const INCOHERENT: [u8; 16] = [0xfe; 16];

const BUCKET_DOMAIN: &str = "blaze-ladder front2 tables bucket cell 1";

/// The cell of an impl bucket by its key's bytes (blaze-scope items_record `BucketKey::bytes`).
pub fn bucket_cell(key: &[u8]) -> [u8; 16] {
    let id = blaze_id::ContentId::of(BUCKET_DOMAIN, key);
    let mut cell = [0u8; 16];
    cell.copy_from_slice(&id.0[..16]);
    cell
}

/// Which record a read is of: what the engine names the read by (its `CellId`'s `Name`), with
/// [`TableRead::cell`] as the cell.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ReadOf {
    /// The `CrateScope` record of the crate whose keys begin with these 8 bytes.
    Names { krate: [u8; 8] },
    /// The input text of an item. Its physical source is placed by the plan.
    Source { key: ItemKey },
    /// The head record of the item (its checker key's 16 bytes).
    Head { item: [u8; 16] },
    /// The impl table of the crate (its key's first 16 bytes; zero: the whole graph's fact).
    Impls { krate: [u8; 16] },
}

/// A canonical identity used while recording.  A caller that already knows the engine cell
/// can provide it so aliases that answer the same cell do not occupy separate recorder slots.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PlacedCell {
    pub kind: u8,
    pub subject: [u8; 32],
    pub cell: [u8; 16],
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum ReadKey {
    Raw(TableRead),
    Placed(PlacedCell),
}

impl TableRead {
    /// The record the read is of.
    pub fn of(self) -> ReadOf {
        match self {
            TableRead::Names(cell) => ReadOf::Names { krate: cell.crate_prefix() },
            TableRead::Signature { key } | TableRead::Text { key } => ReadOf::Source { key },
            TableRead::Head { item, .. } => ReadOf::Head { item },
            TableRead::Impls { krate, .. } => ReadOf::Impls { krate },
        }
    }

    /// The read's cell in its record ([`TableRead::of`]): a names cell's key, a head's part, an
    /// impl table's bucket.
    pub fn cell(self) -> [u8; 16] {
        match self {
            TableRead::Names(cell) => cell.key(),
            TableRead::Signature { .. } => *b"signature tokens",
            TableRead::Text { .. } => *b"definition text\0",
            TableRead::Head { part, .. } => {
                let mut cell = [0u8; 16];
                cell[0] = part;
                cell
            }
            TableRead::Impls { bucket, .. } => bucket,
        }
    }

    /// A workspace places this question on its physical outline or expansion producer.
    /// A standalone check asks its `Make` for the item input directly.
    pub fn is_source(self) -> bool {
        matches!(self, Self::Signature { .. } | Self::Text { .. })
    }
}

/// Where the view puts each read: the recorder, or a test's sink.
pub trait ReadSink {
    fn read(&self, read: TableRead);

    /// Which checker question the reads that follow answer (`Some`), or that its answer is in
    /// (`None`): blaze-typeck `World::asking`, handed down by the body's world (front2
    /// world_items.rs). A recorder counts each offer under the question's kind and, for a
    /// [`Asking::repeat`](blaze_typeck::Asking::repeat) under [`Repeats::Quiet`], offers
    /// nothing until the answer is in. A sink that lists every read keeps this default.
    fn asking(&self, ask: Option<blaze_typeck::Asking>) {
        let _ = ask;
    }

    /// The read of `module`'s own binding of `(ns, name)` (blaze-scope names_cells.rs
    /// `CellSink::put_binding`): the cell `NameCell::Binding { module, name: cell_key(ns, name) }`.
    /// This default derives the key per read; a recorder derives it once per DISTINCT read
    /// (blaze-scope cell_key.rs `BindingKeys`), byte-equal (rule 21).
    fn binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        self.read(TableRead::Names(NameCell::Binding { module, name: blaze_scope::cell_key::cell_key(ns, name) }));
    }
}

/// A [`ReadSink`] as the names walk's [`CellSink`]: each cell a [`TableRead::Names`], each
/// binding read handed on as the read itself, so the sink keys it ([`ReadSink::binding`]).
pub struct NamesInto<'s>(pub &'s dyn ReadSink);

impl CellSink for NamesInto<'_> {
    fn put(&self, cell: NameCell) {
        self.0.read(TableRead::Names(cell));
    }

    fn put_binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        self.0.binding(module, ns, name);
    }
}

/// A task's reads were more than [`MOST_READS`] (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadsFull {
    pub most: usize,
}

impl core::fmt::Display for ReadsFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the task read more than {} cells (front2 tables/recorder.rs `MOST_READS`)", self.most)
    }
}

/// What a recorder is made from.
pub struct RecorderOpts<'a> {
    /// The task's frame: the list lives there and leaves with the task's result.
    pub alloc: Alloc<'a>,
}

/// What a recorder was offered and what it kept (the Bodies stage's reads line, blaze-cli
/// engine_bodies.rs): `offered` every read put, repeats too; `kept` the distinct ones, the
/// list [`Recorder::finish`] sorts; `sort_compares` the compares that sort made. A body that
/// resolves hundreds of paths in a handful of scopes offers thousands and keeps tens.
///
/// THE FLOOR (rule 67): `offered == kept`, 1.0 offers per kept read, when every question a
/// body asks is distinct and each answer offers each of its cells once. `memo` are the offers
/// a repeated question did not make ([`Repeats::Quiet`]); `filtered` the repeats the
/// direct-mapped filter answered with one compare (no hash); `covered` the kept fine cells of a
/// crate's names whose whole section the body had already read (counted, still kept: a coarse
/// read covering fine ones changes the list, so it is the list's own change, not this one).
/// `by_kind` splits `offered` by the checker question being answered (blaze-typeck
/// `QUESTION_KINDS`, then [`OUTSIDE_ASKS`]: setup, lowering and the verdict's own fact reads).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ReadTally {
    pub offered: u64,
    pub kept: u64,
    pub sort_compares: u64,
    pub memo: u64,
    pub filtered: u64,
    pub covered: u64,
    pub by_kind: [u64; ASK_SLOTS],
}

/// The [`ReadTally::by_kind`] slot of reads made outside any checker question.
pub const OUTSIDE_ASKS: usize = blaze_typeck::QUESTION_KINDS.len();
/// The slots of [`ReadTally::by_kind`]: each question kind, then [`OUTSIDE_ASKS`].
pub const ASK_SLOTS: usize = OUTSIDE_ASKS + 1;

impl ReadTally {
    /// Both summed, saturating (the stage sums its bodies').
    pub fn add(&mut self, other: ReadTally) {
        self.offered = self.offered.saturating_add(other.offered);
        self.kept = self.kept.saturating_add(other.kept);
        self.sort_compares = self.sort_compares.saturating_add(other.sort_compares);
        self.memo = self.memo.saturating_add(other.memo);
        self.filtered = self.filtered.saturating_add(other.filtered);
        self.covered = self.covered.saturating_add(other.covered);
        for (a, b) in self.by_kind.iter_mut().zip(other.by_kind.iter()) {
            *a = a.saturating_add(*b);
        }
    }

    /// The slot's name: a question kind's, or `outside` ([`OUTSIDE_ASKS`]).
    pub fn kind_name(slot: usize) -> &'static str {
        blaze_typeck::QUESTION_KINDS.get(slot).copied().unwrap_or("outside")
    }

    /// The `most` slots that offered most, largest first, ties by slot (deterministic, rule
    /// 25); empty slots left out.
    pub fn most_offering(&self, most: usize) -> ([(usize, u64); ASK_SLOTS], usize) {
        let mut slots = [(0usize, 0u64); ASK_SLOTS];
        for (i, s) in slots.iter_mut().enumerate() {
            *s = (i, self.by_kind[i]);
        }
        slots.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let shown = slots.iter().take(most).take_while(|s| s.1 > 0).count();
        (slots, shown)
    }
}

/// How a recorder meets a repeated question (blaze-typeck `Asking::repeat`): the seam its
/// oracle runs both sides of (rule 78; tables/task.rs `compare_repeats`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repeats {
    /// Every read of every answer is offered, repeats too (the recorder before the memo).
    Offer,
    /// A repeated question's reads were recorded at its first answer: none is offered.
    Quiet,
}

/// The direct-mapped filter's slots ([`Recorder::keep`]): a power of two.
const FILTER_SLOTS: usize = 64;

/// One task's reads (module docs).
///
/// A read is kept once, when first offered: the common case is the SAME cell read again and
/// again (every path of a body resolved through a handful of scopes, each lookup logging the
/// cell it read: blaze-scope names_cells.rs `Logged`), so a repeat costs one compare against
/// its slot of the direct-mapped filter, or one probe of `seen`, and is never pushed; a
/// repeated QUESTION offers nothing at all ([`Repeats::Quiet`]). Before (tempo, 2026-11-17, a
/// stack sample of `engine-check`): every offer was pushed and the whole tape, repeats and all,
/// sorted at the end: 538 samples in `read`, 265 in the sort's compares.
pub struct Recorder<'a> {
    /// First observations still to be handed to the caller, in insertion order.
    reads: RefCell<AVec<'a, TableRead>>,
    /// Every cell seen by this entry, including earlier items' transferred observations.
    seen: RefCell<blaze_hash::HashSet<ReadKey, Alloc<'a>>>,
    /// Raw questions whose placement succeeded in this entry. Bounded independently of
    /// the proof: evicting this memo can only cause another placement of the same question.
    mapped: RefCell<blaze_hash::HashSet<TableRead, Alloc<'a>>>,
    /// Set while a recorder allocation is in progress, so an unwind identifies its list.
    allocating: Cell<Option<&'static str>>,
    checking: Cell<Option<&'static str>>,
    /// THE DIRECT-MAPPED FILTER in front of `seen`: the key last kept or met at each slot
    /// ([`slot_of`], a byte of the key, no hash). A repeat of a recent cell is one load and one
    /// compare; a miss probes `seen` as before.
    recent: [Cell<Option<ReadKey>>; FILTER_SLOTS],
    /// Direct-mapped filter in front of `mapped` for raw questions placed via [`Recorder::read_mapped`]:
    /// kept in the recorder's scratch allocator (not on the stack) and only populated after
    /// placement succeeds (or on a `mapped` hit), so a placement error is never hidden.
    recent_raw: blaze_own::ABox<'a, [Cell<Option<TableRead>>; FILTER_SLOTS]>,
    /// Reads offered since the last item transferred its new cells, including repeats; a
    /// repeated question's muted offers are not ([`Recorder::memo`]).
    offered: Cell<u64>,
    /// How a repeated question is met ([`Repeats`]).
    repeats: Cell<Repeats>,
    /// The question being answered, as the checker said it ([`ReadSink::asking`]).
    asking: Cell<Option<blaze_typeck::Asking>>,
    /// Offers a repeated question did not make ([`ReadTally::memo`]).
    memo: Cell<u64>,
    /// Repeats the filter answered ([`ReadTally::filtered`]).
    filtered: Cell<u64>,
    /// Kept names cells a kept whole section covers ([`ReadTally::covered`]).
    covered: Cell<u64>,
    /// Offers by question kind ([`ReadTally::by_kind`]).
    by_kind: [Cell<u64>; ASK_SLOTS],
    /// The crates whose names section this recorder kept whole (`NameCell::Whole { section:
    /// Names }`): few, scanned only when a cell is kept for the first time.
    wholes: RefCell<AVec<'a, [u8; 8]>>,
    full: Cell<bool>,
    /// A test's planted bug: reads of this cell are dropped ([`Recorder::plant_drop`]).
    dropped: Cell<Option<NameCell>>,
    /// The binding reads' cell keys, each derived once ([`ReadSink::binding`]): a names lookup
    /// hashes nothing here on a repeat.
    binding_keys: RefCell<BindingKeys<'a>>,
}

impl<'a> Recorder<'a> {
    pub fn new(opts: RecorderOpts<'a>) -> Recorder<'a> {
        Recorder {
            reads: RefCell::new(AVec::new_in(opts.alloc)),
            seen: RefCell::new(blaze_hash::HashSet::new_in(opts.alloc)),
            mapped: RefCell::new(blaze_hash::HashSet::new_in(opts.alloc)),
            allocating: Cell::new(None),
            checking: Cell::new(None),
            recent: [const { Cell::new(None) }; FILTER_SLOTS],
            recent_raw: blaze_own::Box::new_in([const { Cell::new(None) }; FILTER_SLOTS], opts.alloc),
            offered: Cell::new(0),
            repeats: Cell::new(Repeats::Quiet),
            asking: Cell::new(None),
            memo: Cell::new(0),
            filtered: Cell::new(0),
            covered: Cell::new(0),
            by_kind: [const { Cell::new(0) }; ASK_SLOTS],
            wholes: RefCell::new(AVec::new_in(opts.alloc)),
            full: Cell::new(false),
            dropped: Cell::new(None),
            binding_keys: RefCell::new(BindingKeys::new_in(opts.alloc)),
        }
    }

    /// How many binding cell keys this recorder derived: one per distinct binding read,
    /// whatever the number of lookups (the floor's counter).
    pub fn binding_keys_derived(&self) -> u64 {
        self.binding_keys.borrow().derived()
    }

    /// Offers and newly kept cells since the last transfer (all of them for a single task).
    pub fn tally(&self) -> ReadTally {
        ReadTally {
            offered: self.offered.get(),
            kept: self.reads.borrow().len() as u64,
            sort_compares: 0,
            memo: self.memo.get(),
            filtered: self.filtered.get(),
            covered: self.covered.get(),
            by_kind: core::array::from_fn(|i| self.by_kind[i].get()),
        }
    }

    /// How this recorder meets a repeated question from now on (the oracle's two sides,
    /// tables/task.rs `compare_repeats`); a new recorder is [`Repeats::Quiet`].
    pub fn set_repeats(&self, repeats: Repeats) {
        self.repeats.set(repeats);
    }

    /// Capacity bytes of the pending list, the canonical set, the direct-mapped filter (its
    /// fixed slots) and the placement memo (adds only). The floor is one list of distinct cells. The binding-key memo is
    /// not counted (blaze-scope cell_key.rs `BindingKeys` says no size; its rows are a names
    /// lookup's, one per distinct binding read: [`Recorder::binding_keys_derived`]).
    pub fn table_bytes(&self) -> u64 {
        let slot = core::mem::size_of::<ReadKey>() as u64;
        let raw = core::mem::size_of::<TableRead>() as u64;
        let list = self.reads.borrow().capacity() as u64 * raw;
        let seen = self.seen.borrow().capacity() as u64 * (slot + 1);
        let filter = (core::mem::size_of_val(&self.recent) + core::mem::size_of_val(&*self.recent_raw)) as u64;
        list + seen + filter + self.mapped.borrow().capacity() as u64 * (raw + 1)
    }

    pub fn allocating(&self) -> Option<&'static str> {
        self.allocating.get().or(self.checking.get())
    }

    pub(super) fn checking(&self, what: Option<&'static str>) {
        self.checking.set(what);
    }

    /// Transfer just this item's first reads. The entry's uniqueness index and binding
    /// memo live through its last item. A failed item leaves its entire pending prefix here.
    pub(super) fn take_item(&mut self) -> Result<(AVec<'a, TableRead>, ReadTally), ReadsFull> {
        self.check_bound()?;
        let tally = self.tally();
        let pending = self.reads.get_mut();
        let empty = AVec::new_in(*pending.allocator());
        self.offered.set(0);
        self.memo.set(0);
        self.filtered.set(0);
        self.covered.set(0);
        self.by_kind.iter().for_each(|c| c.set(0));
        Ok((core::mem::replace(pending, empty), tally))
    }

    /// Return a transferred item's list after placement, reusing its slots for the next
    /// item's new cells. Dropping a non-top buffer would leave it in the entry's bump frame.
    pub fn reuse_item_buffer(&mut self, mut batch: AVec<'a, TableRead>) {
        let pending = self.reads.get_mut();
        if pending.is_empty() && pending.capacity() < batch.capacity() {
            batch.clear();
            *pending = batch;
        }
    }

    /// PLANTED BUG, for tests only: every read of `cell` is dropped from now on, as a view
    /// that forgot to record one question would leave it out.
    #[doc(hidden)]
    pub fn plant_drop(&self, cell: NameCell) {
        self.dropped.set(Some(cell));
    }

    /// A head read whole ([`TableRead::Head`]).
    pub fn head(&self, item: [u8; 16], part: u8) {
        self.read(TableRead::Head { item, part });
    }

    /// An impl bucket read ([`TableRead::Impls`]).
    pub fn impls(&self, krate: [u8; 16], bucket: [u8; 16]) {
        self.read(TableRead::Impls { krate, bucket });
    }

    /// A crate's lang rows ([`NameCell::Lang`]: a names cell, absent where it declares none).
    pub fn lang(&self, krate: [u8; 8]) {
        self.read(TableRead::Names(NameCell::Lang { krate }));
    }

    /// A section of a crate's names read whole (a question no cell answers).
    pub fn whole(&self, krate: [u8; 8], section: Section) {
        self.read(TableRead::Names(NameCell::Whole { krate, section }));
    }

    /// An items row's cell.
    pub fn item(&self, key: ItemKey) {
        self.read(TableRead::Names(NameCell::Item { key }));
    }

    /// The task's reads, sorted and each once: the one sort of the task (rule 25).
    pub fn finish(self) -> Result<AVec<'a, TableRead>, ReadsFull> {
        self.finish_tallied().map(|(reads, _)| reads)
    }

    /// [`Recorder::finish`] with what was offered, kept and compared ([`ReadTally`]). The list
    /// holds each read once already (`seen`), so the sort is of the distinct reads alone.
    pub fn finish_tallied(self) -> Result<(AVec<'a, TableRead>, ReadTally), ReadsFull> {
        let (mut reads, mut tally) = self.distinct()?;
        let compares = Cell::new(0u64);
        reads.sort_unstable_by(|a, b| {
            compares.set(compares.get() + 1);
            a.cmp(b)
        });
        tally.sort_compares = compares.get();
        Ok((reads, tally))
    }

    /// For consumers that canonicalize the placed IDs. Insertion order, each key once.
    pub fn distinct(self) -> Result<(AVec<'a, TableRead>, ReadTally), ReadsFull> {
        let tally = self.tally();
        self.check_bound()?;
        Ok((self.reads.into_inner(), tally))
    }

    /// The distinct keys retained before the recorder is finished, including a prefix that ended
    /// at [`ReadsFull`].
    pub fn recorded_prefix(&self) -> core::cell::Ref<'_, [TableRead]> {
        core::cell::Ref::map(self.reads.borrow(), |reads| &reads[..])
    }

    /// A growing evaluator's distinct keys. A cursor into this slice remains valid until the
    /// recorder is dropped, since reads only append. An overflow invalidates the entire proof.
    pub fn keys(&self) -> Result<core::cell::Ref<'_, [TableRead]>, ReadsFull> {
        self.check_bound()?;
        Ok(self.recorded_prefix())
    }

    /// Replay a cached value's inputs. An incomplete list makes the receiving
    /// task's own proof incomplete as well.
    pub(crate) fn replay(&self, rows: Result<&[TableRead], ReadsFull>) {
        match rows {
            Ok(rows) => rows.iter().for_each(|&row| self.read(row)),
            Err(_) => self.full.set(true),
        }
    }

    fn check_bound(&self) -> Result<(), ReadsFull> {
        if self.full.get() { Err(ReadsFull { most: MOST_READS }) } else { Ok(()) }
    }
}

impl ReadSink for Recorder<'_> {
    fn read(&self, read: TableRead) {
        if self.offer() {
            self.keep(read, None);
        }
    }

    fn binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        if let Some(read) = self.first_binding(module, ns, name) {
            self.keep(read, None);
        }
    }

    fn asking(&self, ask: Option<blaze_typeck::Asking>) {
        self.asking.set(ask);
    }
}

impl Recorder<'_> {
    /// One offer: `false` when a repeated question is being answered under [`Repeats::Quiet`]
    /// (its reads were recorded at its first answer: the run's memo, blaze-typeck
    /// `Reads::heard`), counted as `memo` and nothing else done; else counted under the
    /// question's kind, and the caller goes on to keep it.
    fn offer(&self) -> bool {
        let ask = self.asking.get();
        if let Some(a) = ask
            && a.repeat
            && self.repeats.get() == Repeats::Quiet
        {
            self.memo.set(self.memo.get().saturating_add(1));
            return false;
        }
        self.offered.set(self.offered.get().saturating_add(1));
        let slot = ask.map_or(OUTSIDE_ASKS, |a| a.kind.min(OUTSIDE_ASKS));
        let c = &self.by_kind[slot];
        c.set(c.get().saturating_add(1));
        true
    }

    pub(super) fn first_binding(&self, module: ItemKey, ns: Namespace, name: &str) -> Option<TableRead> {
        if !self.offer() {
            return None;
        }
        self.allocating.set(Some("the binding-name key table"));
        let binding = self.binding_keys.borrow_mut().key(module, ns, name);
        self.allocating.set(None);
        (binding.fresh || self.dropped.get().is_some()).then_some(TableRead::Names(NameCell::Binding { module, name: binding.key }))
    }

    /// Place each raw question on its first offer. A hit skips deriving the cell digest,
    /// searching the crate list and probing the canonical set. A placement error ends the
    /// check; keep its key out of the memo so a later check cannot hide the error.
    pub(super) fn read_mapped<E>(&self, read: TableRead, place: impl FnOnce() -> Result<Option<PlacedCell>, E>) -> Result<(), E> {
        const MEMO_LIMIT: usize = 1 << 16;
        if !self.offer() {
            return Ok(());
        }
        let recent = self.recent_raw.get(slot_of(ReadKey::Raw(read)));
        if recent.is_some_and(|slot| slot.get() == Some(read)) {
            self.filtered.set(self.filtered.get().saturating_add(1));
            return Ok(());
        }
        // Impl table reads are already deduplicated per body by `BucketReadUnion` and placed in
        // O(1) via `ReadIds::impl_names`; skip `self.mapped` (which would otherwise hold tens of
        // thousands of one-shot impl keys alongside `self.seen`).
        if matches!(read, TableRead::Impls { .. }) {
            return match place() {
                Ok(cell) => {
                    if let Some(slot) = recent {
                        slot.set(Some(read));
                    }
                    self.keep(read, cell);
                    Ok(())
                }
                Err(error) => {
                    self.keep(read, None);
                    Err(error)
                }
            };
        }
        let mut mapped = self.mapped.borrow_mut();
        if mapped.len() == MEMO_LIMIT {
            if mapped.contains(&read) {
                if let Some(slot) = recent {
                    slot.set(Some(read));
                }
                return Ok(());
            }
            mapped.clear();
            for cell in self.recent_raw.iter() {
                cell.set(None);
            }
        }
        let may_grow = mapped.len() == mapped.capacity();
        if may_grow {
            self.allocating.set(Some("the raw read placement memo"));
        }
        let first = mapped.insert(read);
        if may_grow {
            self.allocating.set(None);
        }
        if !first {
            if let Some(slot) = recent {
                slot.set(Some(read));
            }
            return Ok(());
        }
        match place() {
            Ok(cell) => {
                if let Some(slot) = recent {
                    slot.set(Some(read));
                }
                self.keep(read, cell);
            }
            Err(error) => {
                mapped.remove(&read);
                // Early setup or seed refusal can stop the check before its stored
                // placement error is returned. The prefix must still name this question.
                self.keep(read, None);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Record `read` under an already placed engine cell.  This is used by the body checker,
    /// whose scope grain can make many raw questions answer the same cell.
    pub fn read_cell(&self, read: TableRead, cell: PlacedCell) {
        if self.offer() {
            self.keep(read, Some(cell));
        }
    }

    pub(super) fn keep(&self, read: TableRead, cell: Option<PlacedCell>) {
        if let (TableRead::Names(name), Some(dropped)) = (read, self.dropped.get())
            && name == dropped
        {
            return;
        }
        let key = match cell {
            Some(cell) => ReadKey::Placed(cell),
            None => ReadKey::Raw(read),
        };
        // The filter first: a recent repeat is one load and one compare, no hash.
        let recent = &self.recent[slot_of(key)];
        if recent.get() == Some(key) {
            self.filtered.set(self.filtered.get().saturating_add(1));
            return;
        }
        recent.set(Some(key));
        let mut seen = self.seen.borrow_mut();
        if seen.len() == MOST_READS {
            if !seen.contains(&key) {
                self.full.set(true);
            }
            return;
        }
        if seen.len() == seen.capacity() {
            self.allocating.set(Some("the distinct read key table"));
        }
        if seen.insert(key) {
            self.allocating.set(Some("the distinct read list"));
            self.reads.borrow_mut().push(read);
            self.note_coverage(read);
        }
        self.allocating.set(None);
    }

    /// On a cell's first keep: a whole names section is noted; a fine cell of a crate's names
    /// section already read whole is counted `covered` (kept all the same: [`ReadTally`]).
    /// Only the cells whose digest is a function of the names record's bytes alone (blaze-cli
    /// names.rs hands `Section::Names` the names record): a binding, a module's scope, its
    /// textual sequence.
    fn note_coverage(&self, read: TableRead) {
        let TableRead::Names(cell) = read else { return };
        match cell {
            NameCell::Whole { krate, section: Section::Names } => {
                self.allocating.set(Some("the whole sections read"));
                self.wholes.borrow_mut().push(krate);
            }
            NameCell::Binding { .. } | NameCell::Module { .. } | NameCell::Textual { .. } => {
                let wholes = self.wholes.borrow();
                if !wholes.is_empty() && wholes.contains(&cell.crate_prefix()) {
                    self.covered.set(self.covered.get().saturating_add(1));
                }
            }
            _ => {}
        }
    }
}

/// The filter slot of `key`: one byte of what tells two cells of a body apart (an item key's
/// last byte, a cell key's first, a head's part), folded with the variant's tag, so the cells
/// a path walk offers again and again (its module's scope, the prelude's, the crate root) do
/// not collide by construction. No hash: it is the filter's whole cost.
fn slot_of(key: ReadKey) -> usize {
    let item = |k: ItemKey| k.0.0[31];
    let b = match key {
        ReadKey::Placed(p) => p.cell[0] ^ p.subject[31] ^ p.kind,
        ReadKey::Raw(TableRead::Names(cell)) => match cell {
            NameCell::Binding { module, name } => item(module) ^ name.0[0],
            NameCell::Module { module } => item(module) ^ 0x11,
            NameCell::Textual { module } => item(module) ^ 0x22,
            NameCell::Root { krate } => krate[7] ^ 0x33,
            NameCell::Lang { krate } => krate[7] ^ 0x44,
            NameCell::Incoherent { krate } => krate[7] ^ 0x55,
            NameCell::InternalSymbols { krate } => krate[7] ^ 0x45,
            NameCell::Links { krate } => krate[7] ^ 0x46,
            NameCell::ImplRows { krate } => krate[7] ^ 0x47,
            NameCell::Files { krate } => krate[7] ^ 0x48,
            NameCell::Extensions { krate } => krate[7] ^ 0x49,
            NameCell::CfgAtoms { krate } => krate[7] ^ 0x4a,
            NameCell::Whole { krate, section } => krate[7] ^ (section as u8) ^ 0x66,
            NameCell::Variants { owner } => item(owner) ^ 0x77,
            NameCell::Members { owner } => item(owner) ^ 0x88,
            NameCell::Item { key } => item(key) ^ 0x99,
            NameCell::Expansion { call } => item(call) ^ 0xaa,
            NameCell::MemberOf { member } => item(member) ^ 0xbb,
            NameCell::VariantOf { variant } => item(variant) ^ 0xcc,
            NameCell::Beginning { first } => first[15] ^ 0xdd,
        },
        ReadKey::Raw(TableRead::Signature { key }) => item(key) ^ 0x0e,
        ReadKey::Raw(TableRead::Text { key }) => item(key) ^ 0x1e,
        ReadKey::Raw(TableRead::Head { item: k, part }) => k[15] ^ part.rotate_left(4),
        ReadKey::Raw(TableRead::Impls { krate, bucket }) => krate[15] ^ bucket[0] ^ 0x2e,
    };
    usize::from(b) & (FILTER_SLOTS - 1)
}

impl CellSink for Recorder<'_> {
    fn put(&self, cell: NameCell) {
        self.read(TableRead::Names(cell));
    }

    fn put_binding(&self, module: ItemKey, ns: Namespace, name: &str) {
        ReadSink::binding(self, module, ns, name);
    }
}

#[cfg(test)]
mod tests {
    use blaze_id::{ContentId, ItemKey};
    use blaze_scope::names_cells::NameCell;

    use super::{ReadOf, ReadSink, Recorder, RecorderOpts, Repeats, TableRead};

    fn key(n: u8) -> ItemKey {
        ItemKey(ContentId([n; 32]))
    }

    /// The same reads put in two orders, with repeats, finish as one list: sorted, each once.
    #[test]
    fn reads_finish_sorted_and_once_whatever_the_order() {
        let alloc = blaze_own::testing::heap().alloc();
        let reads = [
            TableRead::Names(NameCell::Item { key: key(3) }),
            TableRead::Head { item: [1; 16], part: 3 },
            TableRead::Names(NameCell::Module { module: key(1) }),
            TableRead::Impls { krate: [2; 16], bucket: [9; 16] },
            TableRead::Names(NameCell::Lang { krate: [4; 8] }),
            TableRead::Signature { key: key(3) },
            TableRead::Text { key: key(3) },
        ];
        let forward = Recorder::new(RecorderOpts { alloc });
        for r in reads.iter().chain(reads.iter()) {
            forward.read(*r);
        }
        let backward = Recorder::new(RecorderOpts { alloc });
        for r in reads.iter().rev() {
            backward.read(*r);
        }
        let (forward, backward) = (forward.finish().expect("within the cap"), backward.finish().expect("within the cap"));
        assert_eq!(&forward[..], &backward[..]);
        assert_eq!(forward.len(), reads.len(), "each read once");
        assert!(forward.windows(2).all(|w| w[0] < w[1]), "sorted: {:?}", &forward[..]);
    }

    #[test]
    fn a_body_read_and_a_signature_read_name_distinct_inputs_of_one_item() {
        let key = key(3);
        let signature = TableRead::Signature { key };
        let body = TableRead::Text { key };
        assert_eq!(signature.of(), ReadOf::Source { key });
        assert_eq!(body.of(), signature.of());
        assert_ne!(body.cell(), signature.cell());
        assert_ne!(TableRead::Names(NameCell::Item { key }).of(), body.of());
    }

    #[test]
    fn repeated_items_do_not_allocate_or_place_the_same_questions_again() {
        let frame = blaze_mem::Frame::with_reserve(1 << 20, blaze_mem::tag::Tag::Infer).expect("recorder frame");
        let erased = blaze_own::Erased::new(&frame);
        let mut recorder = Recorder::new(RecorderOpts { alloc: erased.handle() });
        let mut placements = 0;
        let mut offers = 0;
        let mut kept = 0;
        let mut first_peak = 0;
        for item in 0..100 {
            for n in 0..100u8 {
                let read = TableRead::Names(NameCell::Item { key: key(n % 10) });
                recorder.read_mapped(read, || {
                    placements += 1;
                    Ok::<_, core::convert::Infallible>(Some(super::PlacedCell { kind: 1, subject: [1; 32], cell: [n % 2; 16] }))
                }).unwrap();
            }
            let (batch, tally) = recorder.take_item().expect("within the entry bound");
            assert_eq!(batch.len(), if item == 0 { 2 } else { 0 });
            offers += tally.offered;
            kept += tally.kept;
            recorder.reuse_item_buffer(batch);
            if item == 0 {
                first_peak = frame.peak();
            } else {
                assert_eq!(frame.peak(), first_peak);
            }
        }
        assert_eq!((offers, kept, placements), (10_000, 2, 10));
        std::println!("100 items: 10000 offers, 2 kept, 10 placements, no scratch growth after item one");
    }

    /// The run memo at the recorder (`Repeats`): one tape of asks, a question asked again
    /// re-reading what it read the first time, gives one list under `Offer` and `Quiet`; `Quiet`
    /// offers the repeat's reads to nobody (`memo`) and counts the rest under the question's
    /// kind. CONTROL: a repeat that reads a cell its first answer did not (an answer that is no
    /// function of its question) makes the lists differ: the body oracle (tables/task.rs
    /// `compare_repeats`) would see it.
    #[test]
    fn a_repeated_question_offers_nothing_and_keeps_one_list() {
        use blaze_typeck::Asking;
        let alloc = blaze_own::testing::heap().alloc();
        let (a, b, c) = (TableRead::Names(NameCell::Module { module: key(1) }), TableRead::Names(NameCell::Item { key: key(2) }), TableRead::Names(NameCell::Item { key: key(5) }));
        let tape = |r: &Recorder<'_>, second: &[TableRead]| {
            r.asking(Some(Asking { kind: 8, repeat: false }));
            [a, b, a].iter().for_each(|x| r.read(*x));
            r.asking(None);
            r.read(c);
            r.asking(Some(Asking { kind: 8, repeat: true }));
            second.iter().for_each(|x| r.read(*x));
            r.asking(None);
        };
        let run = |repeats: Repeats, second: &[TableRead]| {
            let r = Recorder::new(RecorderOpts { alloc });
            r.set_repeats(repeats);
            tape(&r, second);
            r.finish_tallied().expect("within the cap")
        };
        let (offer, offer_tally) = run(Repeats::Offer, &[a, b, a]);
        let (quiet, quiet_tally) = run(Repeats::Quiet, &[a, b, a]);
        assert_eq!(&offer[..], &quiet[..], "one list either way");
        assert_eq!((offer_tally.offered, offer_tally.memo), (7, 0));
        assert_eq!((quiet_tally.offered, quiet_tally.memo), (4, 3));
        assert_eq!((quiet_tally.by_kind[8], quiet_tally.by_kind[super::OUTSIDE_ASKS]), (3, 1), "offers by question kind");
        // The control: the repeat reads `c2`, which no first answer read.
        let c2 = TableRead::Names(NameCell::Item { key: key(6) });
        let (offer, _) = run(Repeats::Offer, &[a, c2]);
        let (quiet, _) = run(Repeats::Quiet, &[a, c2]);
        assert_ne!(&offer[..], &quiet[..], "an answer that reads more the second time must show");
        std::println!("repeats: 7 offers -> 4 with the memo (3 muted), one list; a repeat that reads more is seen");
    }

    /// The direct-mapped filter answers a recent repeat with no probe and changes no list.
    #[test]
    fn the_filter_answers_recent_repeats_and_keeps_the_list() {
        let alloc = blaze_own::testing::heap().alloc();
        let r = Recorder::new(RecorderOpts { alloc });
        let (m, p) = (TableRead::Names(NameCell::Module { module: key(1) }), TableRead::Names(NameCell::Module { module: key(9) }));
        for _ in 0..10 {
            r.read(m);
            r.read(p);
        }
        let (list, tally) = r.finish_tallied().expect("within the cap");
        assert_eq!(list.len(), 2);
        assert_eq!((tally.offered, tally.filtered), (20, 18), "two modules in two slots: every repeat met by the filter");
    }

    /// `read_mapped` hits the direct-mapped raw filter on repeats without hiding placement errors.
    #[test]
    fn read_mapped_filters_recent_raw_repeats_and_keeps_errors_visible() {
        let alloc = blaze_own::testing::heap().alloc();
        let r = Recorder::new(RecorderOpts { alloc });
        let ok_read = TableRead::Names(NameCell::Root { krate: [1; 8] });
        let err_read = TableRead::Names(NameCell::Root { krate: [2; 8] });
        let mut placements = 0u32;
        for _ in 0..8 {
            r.read_mapped(ok_read, || {
                placements += 1;
                Ok::<_, ()>(Some(super::PlacedCell { kind: 1, subject: [1; 32], cell: [1; 16] }))
            }).expect("ok placement");
        }
        assert_eq!(placements, 1);
        assert_eq!(r.read_mapped(err_read, || Err(())), Err(()));
        assert_eq!(r.read_mapped(err_read, || Err(())), Err(()), "placement error must not be cached in recent_raw");
        let (list, tally) = r.finish_tallied().expect("within the cap");
        assert_eq!(list.len(), 2);
        assert_eq!(tally.filtered, 8, "7 repeats of ok_read in recent_raw + 1 repeat of err_read in keep");
    }

    /// Control: the planted drop removes exactly that cell.
    #[test]
    fn a_planted_drop_removes_its_cell() {
        let alloc = blaze_own::testing::heap().alloc();
        let r = Recorder::new(RecorderOpts { alloc });
        r.plant_drop(NameCell::Item { key: key(3) });
        r.item(key(3));
        r.item(key(4));
        let list = r.finish().expect("within the cap");
        assert_eq!(&list[..], &[TableRead::Names(NameCell::Item { key: key(4) })]);
    }

    /// Impl-table reads bypass the secondary `mapped` hash set while still deduplicating
    /// cleanly in `recent_raw` + `seen` and surfacing placement errors.
    #[test]
    fn read_mapped_skips_mapped_set_for_impls_and_deduplicates() {
        let alloc = blaze_own::testing::heap().alloc();
        let r = Recorder::new(RecorderOpts { alloc });
        let r1 = TableRead::Impls { krate: [1; 16], bucket: [0xaa; 16] };
        let r2 = TableRead::Impls { krate: [2; 16], bucket: [0xaa; 16] };
        let place = |k: u8| super::PlacedCell { kind: 7, subject: [k; 32], cell: [0xaa; 16] };
        assert!(r.read_mapped(r1, || Ok::<_, ()>(Some(place(1)))).is_ok());
        assert!(r.read_mapped(r2, || Ok::<_, ()>(Some(place(2)))).is_ok());
        // Repeat of r1 and r2: `seen` keeps each once and `mapped` stayed empty.
        assert!(r.read_mapped(r1, || Ok::<_, ()>(Some(place(1)))).is_ok());
        assert!(r.read_mapped(r2, || Ok::<_, ()>(Some(place(2)))).is_ok());
        assert_eq!(r.mapped.borrow().len(), 0);
        let list = match r.finish() {
            Ok(list) => list,
            Err(_) => {
                assert!(false, "within the cap");
                return;
            }
        };
        assert_eq!(list.len(), 2);
    }
}

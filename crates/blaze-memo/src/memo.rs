//! A snapshot ([`Memo`]), how the first is built ([`Builder`]) and how the next is made from it
//! ([`Memo::next`]).

use std::{
    alloc::Global,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use blaze_mem::Tag;

use crate::{
    ArenaId, Gid, MAX_ARENAS, MAX_GENERATION,
    arena::Slot,
    flight::Flight,
    trie::Head,
};

/// Why [`Memo::next`] made no next snapshot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NextRefused {
    /// The arena would be dropped, but it is at [`MAX_GENERATION`]: its next making would wrap
    /// its generation to 0, and an id or a cache keyed by (arena, generation) of an earlier
    /// making would name the new one. The session has spent this index.
    GenerationsSpent { arena: ArenaId },
}

impl std::fmt::Display for NextRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NextRefused::GenerationsSpent { arena } => write!(
                f,
                "{arena:?} would be dropped and made again, but its index has had all \
                 {} generations of this session: a wrapped generation would let an id or a cache \
                 keyed by (arena, generation) of an earlier making answer for the new one; \
                 start a new session (`Memo::builder`)",
                MAX_GENERATION + 1
            ),
        }
    }
}

impl std::error::Error for NextRefused {}

/// Each snapshot's epoch: which snapshot may write to a shared arena.
static EPOCHS: AtomicU64 = AtomicU64::new(1);

/// A snapshot's epoch: unique to it among every snapshot this process makes, and greater than
/// every epoch made before it. What tells an answer kept outside the memo (a thread's cache, a
/// check cache) of one snapshot from another's: it replaces blaze-query's
/// `nonce_and_revision`. An answer that depends only on one arena's entries keys on that
/// arena's [`crate::ArenaId`] instead (index and generation), which outlives the snapshot
/// while the arena is carried.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Epoch(u64);

impl Epoch {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Whether a snapshot keeps read lists.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Reads {
    /// No read lists: [`crate::record`] is refused, a hit replays nothing, and an entry's list
    /// is empty. The cost of the recorder when off: one branch per fetch and 16 bytes per entry.
    #[default]
    Off,
    /// Every entry keeps the facts its run read, and a hit replays them into the asker.
    Kept,
}

/// A bug the tests plant to show their oracle catches it; [`Plant::None`] everywhere else.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// [`Memo::next`] carries the arenas the driver said changed.
    CarryChanged,
    /// [`crate::keep_owned`] places its value but does not list it for drop: what [`crate::keep`]
    /// would do with a value that owns memory, were the no-drop rule not there. It leaks.
    KeepOwnedUnlisted,
    /// [`Memo::next`] ignores borrows: a dependent of a changed arena is carried.
    IgnoreBorrows,
    /// A hit does not replay its entry's reads into the asker.
    HitSkipsReplay,
    /// A hit checks its entry again (walks its read list as a revalidation would): the check
    /// decision 18 forbids, which the counters show.
    HitChecks,
    /// An image record found under its key is used without its read list's one check: a
    /// record whose read moved is served stale.
    ImageSkipsVerify,
    /// `mark_borrows` does not refuse a key naming an arena outside its home's declared
    /// closure.
    MarkOutsideClosure,
    /// A boundary query's own read list is replayed into its asker (no firewall): the asker's
    /// list holds the boundary's input rows in place of one [`crate::Read::Query`] row.
    BoundaryLeaks,
    /// A fixpoint member's flight is let go when its run returns, not when its head settles:
    /// another task can run the member a second time in the snapshot.
    MemberLetGo,
    /// The run log writes a run's key as text when the run starts, with no database attached
    /// (`RunLog::push`): a key whose `Debug` needs one panics there.
    LogFormatsEagerly,
}

/// What every snapshot of a session shares.
#[derive(Clone)]
pub struct Config {
    /// The most arena indices the session uses (sizes each arena's borrow set: `arenas / 8`
    /// bytes per arena).
    pub arenas: u32,
    pub reads: Reads,
    /// How a second asker of a running key waits: [`crate::Scheduled`] on the pool,
    /// [`crate::Alone`] on one thread.
    pub flight: Arc<dyn Flight>,
    /// The most bytes the arenas carried into a next snapshot may hold: past it,
    /// [`Memo::next`] drops every arena (they refill lazily from inputs and records). What a
    /// helper process between commands keeps is bounded by this.
    pub cap_bytes: u64,
    #[doc(hidden)]
    pub plant: Plant,
}

/// Arena index to its arena; `None` where the snapshot has none. Dropped downstream first.
///
/// The invariant: an arena this holds is released before every arena of a lower index. A
/// dependency's index is below its dependent's ([`Builder::arena`]'s contract) and an arena
/// reads only arenas in its declared closure, so when the last holder of an arena drops it,
/// every arena its values borrow from is still alive while its drop list runs (an `owns`
/// value's, a [`crate::keep_owned`] value's, an input's fields'). Dropping in index order
/// would free the session and the dependencies first, under the destructors of their readers.
pub(crate) struct Slots(Vec<Option<Arc<Slot>>, Global>);

impl Slots {
    /// `arenas` indices, none held.
    fn new(arenas: u32) -> Slots {
        let mut slots = Vec::new_in(Global);
        slots.resize_with(arenas as usize, || None);
        Slots(slots)
    }
}

impl std::ops::Deref for Slots {
    type Target = [Option<Arc<Slot>>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Slots {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Slots {
    fn drop(&mut self) {
        // From the greatest index down: the invariant above.
        while let Some(slot) = self.0.pop() {
            drop(slot);
        }
    }
}

/// One snapshot: the arenas its queries read and fill. Inputs never change from its first run
/// on (before it, only the driver's [`Memo::input_mut`], with the snapshot held alone).
pub struct Memo {
    epoch: u64,
    /// What this snapshot computes for an arena it shares with a successor. Declared before
    /// `slots`, so dropped first: its values may borrow from any arena of the snapshot.
    spill: Arc<Slot>,
    /// Arena index to its arena; `None` where this snapshot has none.
    pub(crate) slots: Slots,
    config: Config,
    succeeded: AtomicBool,
    /// Whether a query has run on this snapshot ([`Memo::note_run`]): from then on its inputs
    /// may have been read into a value, and [`Memo::input_mut`] refuses.
    ran: AtomicBool,
    /// The image records the crate being built is served from, while [`Memo::serve`] runs.
    pub(crate) image: crate::image::Serving,
    /// Records of imaged entries computed in this snapshot, for the driver to file
    /// ([`Memo::file_records`]).
    pub(crate) filed: crate::image::Outbox,
    log: Option<Arc<crate::testing::RunLog, Global>>,
    /// The fact boundaries ([`Builder::mark_boundary`]), sorted.
    boundaries: Boundaries,
    /// Arena index to the generation its next making gets: one past the last making of the
    /// session, held or not (an index dropped and not made again keeps its count).
    generations: Box<[u32], Global>,
}

/// The query kinds marked fact boundaries: (name hash, name), sorted, shared by every snapshot
/// of the session.
type Boundaries = Arc<Vec<(u64, &'static str), Global>, Global>;

impl Memo {
    /// Whether `kind` is a fact boundary ([`Builder::mark_boundary`]). A binary search of the
    /// session's few boundaries, by the name's hash, asked only when read lists are kept.
    pub(crate) fn is_boundary(&self, kind: &crate::Kind) -> bool {
        self.boundaries
            .binary_search_by(|(id, name)| (*id, *name).cmp(&(kind.id(), kind.name())))
            .is_ok()
    }

    /// The key of the boundary entry `key` names (a [`crate::Read::Query`] row's), when it is
    /// an entry of `kind` in this snapshot. What the generated `key_of` reads.
    pub(crate) fn boundary_key<K: 'static>(&self, kind: &'static crate::Kind, key: Gid) -> Option<&K> {
        let slot = if key.is_spill() { Some(&*self.spill) } else { self.try_slot(key.arena()) }?;
        let head = slot.local(key.local())?;
        // SAFETY: an entry of `kind` is an `Entry<K, V>` (the caller's `K` is `kind`'s key),
        // `#[repr(C)]`, so its key is at the same offset whatever `V` is.
        std::ptr::eq(head.kind, kind).then(|| &unsafe { crate::trie::Entry::<K, ()>::of(head) }.key)
    }

    /// A builder of a session's first snapshot.
    pub fn builder(config: Config) -> Builder {
        assert!(
            config.arenas >= 1 && config.arenas <= MAX_ARENAS,
            "a session has 1 to {MAX_ARENAS} arena indices, not {}",
            config.arenas
        );
        let epoch = EPOCHS.fetch_add(1, Ordering::Relaxed);
        let slots = Slots::new(config.arenas);
        let mut generations = Vec::new_in(Global);
        generations.resize(config.arenas as usize, 0_u32);
        Builder {
            epoch,
            slots,
            first: true,
            generations,
            config,
            log: None,
            boundaries: Arc::new_in(Vec::new_in(Global), Global),
        }
    }

    /// The run log this snapshot appends to ([`Builder::log_runs`]), if any.
    pub(crate) fn log(&self) -> Option<&crate::testing::RunLog> {
        self.log.as_deref()
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// This snapshot's epoch ([`Epoch`]): what replaces `nonce_and_revision`.
    pub fn snapshot_epoch(&self) -> Epoch {
        Epoch(self.epoch)
    }

    /// Whether `arena` may be named changed in this session: `false` for an arena the driver
    /// sealed ([`Inputs::seal`]: a crate not being built, whose inputs never change while the
    /// session lives; the session arena when the driver sealed it). What replaces
    /// `deps::key::sealed(db, krate)`: `!memo.changed(arena)`. Nothing witnesses it: a sealed
    /// arena is refused in [`Memo::next`]'s changed set, and is dropped only by a borrow (its
    /// dependency's or the session's drop).
    ///
    /// # Panics
    ///
    /// Where this snapshot does not hold `arena` (a dead id).
    pub fn changed(&self, arena: ArenaId) -> bool {
        !self.slot(arena).is_sealed()
    }

    /// How many entries of the query, interned struct or input named `name` (its full path,
    /// `module::f` or `module::Type::method`, as [`crate::Kind::name`]) arena `arena`'s own
    /// table holds (not the spill table of a snapshot whose successor took the arena). For tests ("this was dropped", "this was kept"):
    /// a walk of the arena's table, never asked by a query.
    ///
    /// # Panics
    ///
    /// Where this snapshot does not hold `arena`.
    pub fn entries(&self, arena: ArenaId, name: &str) -> usize {
        let mut count = 0;
        let mut each = |head: &crate::trie::Head| {
            if head.kind.name() == name {
                count += 1;
            }
        };
        self.slot(arena).trie.each(&mut each);
        count
    }

    /// Whether this snapshot keeps read lists ([`Reads::Kept`]).
    pub fn keeps_reads(&self) -> bool {
        self.config.reads == Reads::Kept
    }

    pub(crate) fn reads(&self) -> crate::Reads {
        self.config.reads
    }

    pub(crate) fn plant(&self) -> Plant {
        self.config.plant
    }

    pub(crate) fn flight(&self) -> &dyn Flight {
        &*self.config.flight
    }

    pub(crate) fn spill(&self) -> &Slot {
        &self.spill
    }

    /// The arena `id`.
    ///
    /// The generation compare here is the one compare a hit makes, and it is of the id, not of
    /// any entry: an id stays valid across `Memo::next` exactly when its arena is carried, and
    /// values carried in arenas hold ids of earlier snapshots by design, so which ids are dead
    /// is the edit's (dynamic) closure, not a lifetime. A brand per snapshot
    /// (`blaze_keyed::proof::Snapshot`) would refuse every carried id; the compare refuses only
    /// the dead ones, loudly.
    ///
    /// # Panics
    ///
    /// Where this snapshot has no arena at `id`'s index, or has one of another generation: an
    /// id of an arena that was dropped is dead.
    #[inline]
    pub(crate) fn slot(&self, id: ArenaId) -> &Slot {
        match self.slots.get(id.index() as usize) {
            Some(Some(slot)) if slot.id == id => slot,
            Some(Some(slot)) => panic!(
                "{id:?} is dead: this snapshot holds {:?} at its index (an id of a dropped arena \
                 was kept across `Memo::next`)",
                slot.id
            ),
            _ => panic!(
                "{id:?} is not in this snapshot: the driver did not add it (`Builder::arena`)"
            ),
        }
    }

    /// The arena `id`, if this snapshot holds that arena of that generation.
    pub(crate) fn try_slot(&self, id: ArenaId) -> Option<&Slot> {
        match self.slots.get(id.index() as usize) {
            Some(Some(slot)) if slot.id == id => Some(slot),
            _ => None,
        }
    }

    /// The head an id is of, wherever it lives.
    pub(crate) fn head_of(&self, id: Gid) -> &Head {
        let found = if id.is_spill() {
            self.spill.local(id.local())
        } else {
            self.slot(id.arena()).local(id.local())
        };
        found.unwrap_or_else(|| panic!("{id:?} names nothing in its arena"))
    }

    /// The arena indices this snapshot holds, ascending.
    pub fn arenas(&self) -> impl Iterator<Item = ArenaId> + '_ {
        self.slots.iter().flatten().map(|slot| slot.id)
    }

    /// This snapshot's session arena (index 0) at the generation it holds: the home of every
    /// key that names no arena, and of the singletons. One load, on every fetch of such a key.
    ///
    /// # Panics
    ///
    /// Where the driver added no session arena.
    #[inline]
    pub fn session(&self) -> ArenaId {
        match self.slots.get(ArenaId::SESSION_INDEX as usize) {
            Some(Some(slot)) => slot.id,
            _ => panic!("this snapshot has no session arena (index 0): the driver did not add it"),
        }
    }

    /// Whether [`Memo::next`] was called on this snapshot, whatever came of it: then it never
    /// makes another. A driver whose commit failed after `next` (a maker that panicked while
    /// the next snapshot was made) keeps this one, which answers still, and starts a new
    /// session at its next commit.
    pub fn has_successor(&self) -> bool {
        self.succeeded.load(Ordering::Acquire)
    }

    /// Notes that a query runs on this snapshot ([`Memo::input_mut`] refuses from then on). A
    /// load per run, and a store only at the first: no line is written shared once it is set.
    /// Relaxed: `input_mut` takes `&mut self`, and whatever gave its caller that ordered every
    /// run before it.
    #[inline]
    pub(crate) fn note_run(&self) {
        if !self.ran.load(Ordering::Relaxed) {
            self.ran.store(true, Ordering::Relaxed);
        }
    }

    /// Whether a query has run on this snapshot ([`Memo::note_run`]).
    pub fn has_run(&self) -> bool {
        self.ran.load(Ordering::Relaxed)
    }

    /// Bytes the arenas hold, the spill table's included.
    pub fn held(&self) -> usize {
        self.slots.iter().flatten().map(|slot| slot.held()).sum::<usize>() + self.spill.held()
    }

    /// The next snapshot: every arena of this one carried except those in `changed` (arenas
    /// whose inputs' digests moved, by the driver's records) and, to a fixpoint, every arena
    /// that borrowed from a dropped one.
    ///
    /// The invariant: an arena is carried only if no input any of its entries could have read
    /// has changed. An entry of `A` read only `A`'s inputs and the arenas `A` borrowed from
    /// (every cross-arena read sets a borrow bit before the value that read it is published),
    /// so an arena outside the closure of `changed` read nothing that changed.
    ///
    /// This snapshot keeps answering for tasks still in it; from now on what it computes for an
    /// arena it shares goes to its spill table.
    ///
    /// # Errors
    ///
    /// [`NextRefused::GenerationsSpent`] naming the lowest arena to drop that is at
    /// [`MAX_GENERATION`]. The refusal is final for this snapshot (it has a successor no
    /// longer, and never will; it keeps answering): the driver starts a new session.
    ///
    /// # Panics
    ///
    /// Where this snapshot already has a successor.
    pub fn next(&self, changed: &[ArenaId]) -> Result<Next, NextRefused> {
        assert!(
            !self.succeeded.swap(true, Ordering::AcqRel),
            "this snapshot already has a successor: make the next from that one"
        );
        let epoch = EPOCHS.fetch_add(1, Ordering::Relaxed);
        let words = (self.config.arenas as usize).div_ceil(64);
        let mut dropped = Vec::new_in(Global);
        dropped.resize(words, 0_u64);
        fn mark(dropped: &mut [u64], index: u32) {
            dropped[index as usize / 64] |= 1 << (index % 64);
        }
        fn is(dropped: &[u64], index: u32) -> bool {
            dropped[index as usize / 64] & (1 << (index % 64)) != 0
        }
        if self.config.plant != Plant::CarryChanged {
            for id in changed {
                let slot = self.slot(*id);
                assert!(
                    !slot.is_sealed(),
                    "{id:?} is named changed but the driver sealed it (`Inputs::seal`): a sealed \
                     arena's inputs never change in its session"
                );
                mark(&mut dropped, slot.id.index());
            }
        }
        // Freeze every candidate before reading borrows: from here no predecessor writes to it
        // or adds to its borrows, so the closure below reads a final set.
        for slot in self.slots.iter().flatten() {
            // The one check of an arena per snapshot: whether it is carried. What is carried
            // is settled for the next snapshot by being in it; nothing checks it again.
            crate::counts::check();
            if !is(&dropped, slot.id.index()) {
                slot.freeze(epoch);
            }
        }
        if self.config.plant != Plant::IgnoreBorrows {
            loop {
                let mut grew = false;
                for slot in self.slots.iter().flatten() {
                    let index = slot.id.index();
                    if !is(&dropped, index) && slot.borrowed_any(&dropped) {
                        mark(&mut dropped, index);
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
        }
        let carried: usize = self
            .slots
            .iter()
            .flatten()
            .filter(|slot| !is(&dropped, slot.id.index()))
            .map(|slot| slot.held())
            .sum();
        if carried as u64 > self.config.cap_bytes {
            for slot in self.slots.iter().flatten() {
                mark(&mut dropped, slot.id.index());
            }
        }
        // A dropped arena is made again at its index's next generation: none left, and the
        // making would wrap and reuse generation 0. Ascending, so the lowest is named.
        if let Some(spent) = self.slots.iter().flatten().find(|slot| {
            is(&dropped, slot.id.index()) && self.generations[slot.id.index() as usize] > MAX_GENERATION
        }) {
            return Err(NextRefused::GenerationsSpent { arena: spent.id });
        }
        let mut slots = Slots::new(self.config.arenas);
        let mut generations = Vec::new_in(Global);
        generations.extend_from_slice(&self.generations);
        let mut gone = Vec::new_in(Global);
        for slot in self.slots.iter().flatten() {
            let index = slot.id.index();
            if is(&dropped, index) {
                gone.push(index);
            } else {
                slots[index as usize] = Some(Arc::clone(slot));
            }
        }
        Ok(Next {
            builder: Builder {
                epoch,
                slots,
                first: false,
                generations,
                config: self.config.clone(),
                log: self.log.clone(),
                boundaries: Arc::clone(&self.boundaries),
            },
            dropped: gone,
        })
    }
}

/// The id the spill table's slot carries. Never a home and never looked up: a spill id is
/// `Gid { arena: its own arena, local: .. | SPILL }`, found through [`Memo::spill`] by its bit,
/// so this id is only shown in the slot's own panics. Private, so that nothing outside reads it
/// as the session arena it would equal at generation 0.
const SPILL_TABLE_ID: ArenaId = ArenaId::new(ArenaId::SESSION_INDEX, 0);

/// Builds a snapshot: the driver adds each arena and its inputs.
pub struct Builder {
    epoch: u64,
    slots: Slots,
    /// Whether this builds the session's first snapshot (fact boundaries are marked only here).
    first: bool,
    /// The generation each index's next making gets ([`Memo`]'s, carried), so a remade
    /// arena's ids differ from every earlier making's.
    generations: Vec<u32, Global>,
    config: Config,
    log: Option<Arc<crate::testing::RunLog, Global>>,
    boundaries: Boundaries,
}

impl Builder {
    /// Marks the query named `name` (its full path, `module::f`, as [`crate::Kind::name`]) a
    /// fact boundary for the session: an ask of it under a recorder lists one
    /// [`crate::Read::Query`] row in the asker and never its own list (the firewall; blaze
    /// item-hash marks its per-item hashes). Carried by [`Memo::next`]; marked in the first
    /// snapshot's builder only.
    ///
    /// # Panics
    ///
    /// Where this builder is a next snapshot's: the boundaries are the session's, fixed when
    /// it starts, so every snapshot's lists mean the same.
    pub fn mark_boundary(&mut self, name: &'static str) {
        assert!(
            self.first,
            "`mark_boundary({name})` on a next snapshot's builder: mark boundaries when the \
             session starts, so every snapshot's read lists mean the same"
        );
        let id = crate::Kind::new(name, crate::CycleKind::Panic).id();
        let boundaries = Arc::get_mut(&mut self.boundaries).expect("a first builder owns its boundaries");
        if let Err(at) = boundaries.binary_search(&(id, name)) {
            boundaries.insert(at, (id, name));
        }
    }

    /// Appends every run of this snapshot and of each one [`Memo::next`] makes from it to
    /// `log`, in order (`crate::testing::RunLog`): what blaze-query's event log told tests.
    pub fn log_runs(&mut self, log: Arc<crate::testing::RunLog, Global>) {
        self.log = Some(log);
    }

    /// Adds an empty arena at `index`, its memory counted to `tag`, and gives the handle its
    /// inputs are made through.
    ///
    /// The contract on `index` (the driver's, blaze-cargo's): a crate's arena index is
    /// `1 + its position in a topological order of the crate graph` (dependencies first), and
    /// 0 is the session arena. A key's home is the greatest index it names, so that order makes
    /// it the most downstream crate the key names; [`Inputs::borrows`] checks each declared
    /// dependency's index is below its dependent's, and a key naming an arena outside its
    /// home's declared closure panics naming the key.
    ///
    /// # Panics
    ///
    /// Where `index` is past [`Config::arenas`] or this snapshot holds an arena there already
    /// (one carried keeps its inputs).
    pub fn arena(&mut self, index: u32, tag: Tag) -> Inputs<'_> {
        let at = index as usize;
        assert!(
            at < self.slots.len(),
            "arena index {index} is past the session's {} (`Config::arenas`)",
            self.slots.len()
        );
        assert!(
            self.slots[at].is_none(),
            "arena index {index} is in this snapshot already: a carried arena keeps its inputs"
        );
        let generation = self.generations[at];
        // Never wraps: an index at its last generation is held (made) until `Memo::next`
        // would drop it, and `next` refuses that drop (`NextRefused::GenerationsSpent`).
        assert!(
            generation <= MAX_GENERATION,
            "arena index {index} has no generation left: `Memo::next` refuses its drop"
        );
        self.generations[at] = generation + 1;
        let id = ArenaId::new(index, generation);
        let slot = Arc::new_in(Slot::new(id, tag, self.epoch, self.config.arenas), Global);
        self.slots[at] = Some(slot);
        let slot = self.slots[at].as_deref().expect("just added");
        Inputs { slot, epoch: self.epoch }
    }

    /// The handle to add inputs to an arena added to this snapshot.
    ///
    /// # Panics
    ///
    /// Where the arena was carried (its inputs are fixed) or never added.
    pub fn inputs(&self, id: ArenaId) -> Inputs<'_> {
        match self.slots.get(id.index() as usize) {
            Some(Some(slot)) if slot.id == id && slot.writable_by(self.epoch) && self.fresh(slot) => {
                Inputs { slot, epoch: self.epoch }
            }
            _ => panic!("{id:?} takes no inputs here: it was carried, or never added"),
        }
    }

    fn fresh(&self, slot: &Arc<Slot>) -> bool {
        Arc::strong_count(slot) == 1
    }

    /// Whether arena `index` is in this snapshot (carried or added).
    pub fn has(&self, index: u32) -> bool {
        self.slots.get(index as usize).is_some_and(Option::is_some)
    }

    /// The arena at `index`, if this snapshot holds one.
    pub fn id(&self, index: u32) -> Option<ArenaId> {
        self.slots.get(index as usize)?.as_ref().map(|slot| slot.id)
    }

    /// The session arena (index [`ArenaId::SESSION_INDEX`]) of the snapshot this builds, at the
    /// generation this builder made or carried: what a maker of session inputs names it by
    /// (`builder.inputs(builder.session())`), as [`Memo::session`] names a built snapshot's.
    ///
    /// # Panics
    ///
    /// Where the driver has not added a session arena to this builder, nor carried one.
    pub fn session(&self) -> ArenaId {
        self.id(ArenaId::SESSION_INDEX).unwrap_or_else(|| {
            panic!(
                "this builder has no session arena (index {}): the driver adds it \
                 (`Builder::arena({0}, ..)`) before anything names it",
                ArenaId::SESSION_INDEX
            )
        })
    }

    /// The snapshot. Closes each arena added to it over its declared borrows, in index order
    /// (a dependency's index is below its dependent's, so each is closed before its readers).
    ///
    /// # Panics
    ///
    /// Where an arena declared a borrow of an index this snapshot does not hold.
    pub fn build(self) -> Memo {
        for slot in self.slots.iter().flatten() {
            slot.close(|index| match self.slots.get(index as usize) {
                Some(Some(dep)) => dep,
                _ => panic!(
                    "{:?} declared a borrow of arena index {index}, which this snapshot does not \
                     hold: the driver adds a dependency's arena before its dependent's",
                    slot.id
                ),
            });
            // A sealed crate's dependencies are sealed (what `deps::key::sealed` checked per
            // ask, once here): else its arena drops when theirs change, and `changed` lies.
            if slot.is_sealed() {
                for index in slot.declared().filter(|index| *index != 0) {
                    let dep = self.slots[index as usize].as_deref().expect("closed above");
                    assert!(
                        dep.is_sealed(),
                        "{:?} is sealed but its dependency {:?} is not: seal a crate only with \
                         all its dependencies",
                        slot.id,
                        dep.id
                    );
                }
            }
        }
        let spill = Arc::new_in(
            Slot::new(SPILL_TABLE_ID, Tag::Query, self.epoch, self.config.arenas),
            Global,
        );
        Memo {
            epoch: self.epoch,
            spill,
            slots: self.slots,
            config: self.config,
            succeeded: AtomicBool::new(false),
            ran: AtomicBool::new(false),
            image: crate::image::Serving::none(),
            filed: crate::image::Outbox::new(),
            log: self.log,
            boundaries: self.boundaries,
            generations: self.generations.into_boxed_slice(),
        }
    }
}

/// [`Memo::next`]'s result: the builder of the next snapshot, holding every carried arena, and
/// the indices it dropped, which the driver adds again with their inputs.
pub struct Next {
    pub builder: Builder,
    dropped: Vec<u32, Global>,
}

impl Next {
    /// The arena indices dropped, ascending: those named changed and those that borrowed from
    /// a dropped one.
    pub fn dropped(&self) -> &[u32] {
        &self.dropped
    }
}

/// Where a snapshot's inputs are made: one arena of a [`Builder`].
pub struct Inputs<'b> {
    pub(crate) slot: &'b Slot,
    pub(crate) epoch: u64,
}

impl Inputs<'_> {
    pub fn id(&self) -> ArenaId {
        self.slot.id
    }

    /// Declares that this arena borrows from arena `from`: the driver calls it for each direct
    /// dependency of the crate and for the session arena (a settings change drops all). This
    /// arena is dropped whenever `from` is, as if an entry had read it, and the closure of
    /// these declarations (over the dependencies' own) is what a key homed here may name; an
    /// input field holding an id of an arena not declared here is an id that can go dead while
    /// this arena is carried.
    ///
    /// # Panics
    ///
    /// Where `from`'s index is not below this arena's (the index contract of
    /// [`Builder::arena`]).
    pub fn borrows(&self, from: ArenaId) {
        self.slot.declare(from, self.epoch);
    }

    /// Seals this arena: its inputs never change while the session lives (a crate that is
    /// not being built). [`Memo::changed`] is `false` for it from now on, carried or not, and
    /// [`Memo::next`] refuses it in a changed set.
    pub fn seal(&self) {
        self.slot.seal();
    }
}

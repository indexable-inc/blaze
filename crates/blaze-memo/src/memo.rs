//! A snapshot ([`Memo`]), how the first is built ([`Builder`]) and how the next is made from it
//! ([`Memo::next`]).

use std::{
    alloc::Global,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use blaze_core::lock::Lock;
use blaze_mem::Tag;

use crate::{
    ArenaId, Gid, MAX_ARENAS, MAX_GENERATION,
    arena::Slot,
    flight::Flight,
    trie::Head,
};

/// Why [`Memo::next`] made no next snapshot, or [`Memo::next_session`] no new session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NextRefused {
    /// The arena would be made again, but it is its index's last making in its id space
    /// (generation [`MAX_GENERATION`]): another making would have to take a generation an
    /// earlier making had, and an id, or a cache outside the memo keyed by (arena,
    /// generation), of that earlier making would name the new one. The id space has spent
    /// this index: only a new id space ([`Memo::builder`]) makes it again, and a holder of ids
    /// must tell that space from this one (base-db gives the database another nonce).
    GenerationsSpent { arena: ArenaId },
}

impl std::fmt::Display for NextRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NextRefused::GenerationsSpent { arena } => write!(
                f,
                "{arena:?} would be made again, but its index has had all {} generations of its \
                 id space (every session of the database shares one): another making would \
                 take an id an earlier making had, and an id or a cache keyed by (arena, \
                 generation) of that making would answer for the new one; start a new id space \
                 (`Memo::builder`) under another database identity",
                MAX_GENERATION + 1
            ),
        }
    }
}

impl std::error::Error for NextRefused {}

/// The ids a database hands out over its life: each arena index's next generation, one table
/// shared by every snapshot and builder of every session of the database (a new session takes
/// it over, [`Memo::next_session`]).
///
/// The invariant: within one id space an [`ArenaId`] is taken once, so once it was handed out
/// it names that one making for ever; no later making, in this session or a later one, has
/// it, and so no [`Gid`] of a later making equals one of an earlier making. A builder that
/// never builds (a maker that panicked) has taken its ids too, and they are never taken again.
///
/// Locked once per arena made and once per [`Memo::next`] or [`Memo::next_session`]: never on
/// a fetch.
#[derive(Clone)]
struct Ids(Arc<Lock<Vec<u32, Global>>, Global>);

impl Ids {
    /// A new id space: no index made yet.
    fn new() -> Ids {
        Ids(Arc::new_in(Lock::new("memo.ids", Vec::new_in(Global)), Global))
    }

    /// The id of `index`'s next making, taken; `None` where the index has made its last
    /// generation ([`MAX_GENERATION`]) in this space.
    fn take(&self, index: u32) -> Option<ArenaId> {
        let mut next = self.0.lock();
        let at = index as usize;
        if next.len() <= at {
            next.resize(at + 1, 0);
        }
        let generation = next[at];
        if generation > MAX_GENERATION {
            return None;
        }
        next[at] = generation + 1;
        Some(ArenaId::new(index, generation))
    }

    /// The lowest of `indices` (ascending) that has made its last generation in this space.
    fn first_spent(&self, indices: impl IntoIterator<Item = u32>) -> Option<u32> {
        let next = self.0.lock();
        indices
            .into_iter()
            .find(|index| next.get(*index as usize).is_some_and(|it| *it > MAX_GENERATION))
    }
}

/// Each snapshot's epoch: which snapshot may write to a shared arena.
static EPOCHS: AtomicU64 = AtomicU64::new(1);

/// A snapshot's epoch: unique to it among every snapshot this process makes, and greater than
/// every epoch made before it. What tells an answer kept outside the memo (a thread's cache, a
/// check cache) of one snapshot from another's: it replaces blaze-query's
/// `nonce_and_revision`. An answer that depends only on one arena's entries keys on that
/// arena's [`crate::ArenaId`] instead (index and generation), which outlives the snapshot
/// while the arena is carried, together with which id space the id is of (base-db: the
/// database's nonce): within one id space an `ArenaId` names one making of an arena for ever,
/// across every commit and every new session of the database ([`Memo::next_session`]), so a
/// live arena with the id the answer holds is the arena it was computed in. Another id space
/// (another database, or one made again once an index spent its generations) hands out the
/// same ids again.
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
    /// is empty. The cost of the recorder when off: one branch per fetch and a null word per
    /// entry.
    #[default]
    Off,
    /// Every entry keeps what its run read directly (input and boundary rows, and the entries
    /// it asked), and an ask, hit or run, adds one row naming the entry to the asker; an
    /// entry's flat list is made when asked for (`record::Lists`).
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
    /// [`Memo::next_session`] gives its session a new id space, as `Memo::builder` does: every
    /// index starts again at generation 0, and the new session's arenas take the ids of the
    /// old one's (the bug base-db's fresh session met, when a sealed crate's write started one).
    SessionRestartsIds,
    /// A flat list's walk stops at the first entry it reaches a second time (`break` for
    /// `continue`): every entry still to visit is skipped (`record::flatten_in`).
    FlattenStopsAtRevisit,
    /// An ask copies the asked entry's whole flat list into its asker (the eager lists the
    /// lazy ones replaced): right lists, at N×M rows for N asks of M-row lists.
    AskCopiesFlatList,
    /// A settled cycle's members are published with no lists, not their head's: a member's
    /// flat list misses what its value depends on through the head.
    MemberListsNothing,
    /// A borrow of a sealed arena is never noted: the shortcut "a sealed arena never goes
    /// away", which nothing may take since [`Memo::next`] drops a sealed arena as any other.
    /// Planted where [`Memo::next`] reads the borrows (a bit naming a sealed arena reads as
    /// unset), not in `Slot::borrow`: that is given the borrowed arena's id alone, neither its
    /// seal nor the session's plant.
    SealedSkipsBorrow,
}

/// What every snapshot of a session shares.
#[derive(Clone)]
pub struct Config {
    /// The most arena indices the session uses (sizes each arena's borrow set: `arenas / 8`
    /// bytes per arena).
    pub arenas: u32,
    pub reads: Reads,
    /// How a second asker of a running key waits: [`crate::Scheduled`] on the pool,
    /// [`crate::Alone`] for one task, which refuses another task's ask of a key it holds.
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
    /// The database's id space ([`Ids`]): each index's next generation, shared with every
    /// snapshot and session of the database, held or not (an index dropped and not made again
    /// keeps its count).
    ids: Ids,
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

    /// A builder of the first snapshot of a new database: the first session of a new id space
    /// ([`crate::ArenaId`]'s invariant). Its arenas' ids may equal those of any other id space (another database's,
    /// or this database's before an index spent its generations), so a holder of ids outside
    /// the memo keys by which space they are of (base-db: the database's nonce). A new session
    /// of the same database is [`Memo::next_session`], never this.
    pub fn builder(config: Config) -> Builder {
        Memo::session_in(config, Ids::new())
    }

    /// A builder of a new session of this snapshot's database: no arena carried (the driver
    /// adds every one again, with its inputs, and marks the boundaries again), each made at its
    /// index's next generation in this snapshot's id space, so no arena it makes takes an id
    /// any arena of this session or an earlier one of the database had. What a driver starts
    /// when it cannot carry (the arena capacity outgrown, a failed commit); allowed whether or
    /// not this snapshot has a successor.
    ///
    /// # Errors
    ///
    /// [`NextRefused::GenerationsSpent`] naming the last making of the lowest index below
    /// `config.arenas` that has made its last generation in the id space: the session would
    /// make it again. The driver starts a new id space ([`Memo::builder`]) under another
    /// database identity.
    pub fn next_session(&self, config: Config) -> Result<Builder, NextRefused> {
        if config.plant == Plant::SessionRestartsIds {
            return Ok(Memo::session_in(config, Ids::new()));
        }
        match self.ids.first_spent(0..config.arenas) {
            Some(index) => {
                Err(NextRefused::GenerationsSpent { arena: ArenaId::new(index, MAX_GENERATION) })
            }
            // clone: a reference count of the database's one id space
            None => Ok(Memo::session_in(config, self.ids.clone())),
        }
    }

    /// A builder of a session's first snapshot, its arenas' ids taken from `ids`.
    fn session_in(config: Config, ids: Ids) -> Builder {
        assert!(
            config.arenas >= 1 && config.arenas <= MAX_ARENAS,
            "a session has 1 to {MAX_ARENAS} arena indices, not {}",
            config.arenas
        );
        let epoch = EPOCHS.fetch_add(1, Ordering::Relaxed);
        let slots = Slots::new(config.arenas);
        Builder {
            epoch,
            slots,
            first: true,
            ids,
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

    /// Whether `arena` is not sealed in this snapshot: `false` for an arena the driver sealed
    /// ([`Inputs::seal`]: a crate not being built, whose sources are pinned). What replaces
    /// `deps::key::sealed(db, krate)`: `!memo.changed(arena)`, an answer of this snapshot.
    ///
    /// Sealed arenas. Sealing changes nothing of how an arena is read or carried: every read
    /// of another arena sets a borrow bit, a sealed arena's as any other's, and every declared
    /// borrow ([`Inputs::borrows`]) is one from the start. So [`Memo::next`] takes a sealed
    /// arena in its changed set like any other (2026-11-14; it refused one before, and the
    /// driver started a whole new session instead): the set it drops is closed under
    /// borrowers, so every arena that declared or read the sealed one goes with it, and an
    /// arena it carries read nothing that changed. What is sound to carry is decided by the
    /// borrows alone; what a seal says is where the crate's records are kept
    /// (`deps::key::sealed`), and an arena made again is sealed again by its maker, with a new
    /// id. The one thing a seal still checks is at [`Builder::build`]: a sealed arena's
    /// declared dependencies are sealed.
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

    /// The arena this snapshot holds at `index`, of whatever generation; `None` where it holds
    /// none.
    pub(crate) fn held_at(&self, index: u32) -> Option<ArenaId> {
        self.slots.get(index as usize)?.as_ref().map(|slot| slot.id)
    }

    /// `value` with the proof that this snapshot holds every arena it names at the generation
    /// its ids hold ([`Live`]), or the first arena it names that this snapshot does not
    /// ([`Dead`]): an id of an arena a commit dropped since, or of one a later snapshot made.
    ///
    /// What a value kept outside the memo across [`Memo::next`] (a driver's table keyed by
    /// ids, read by tasks of more than one snapshot) is passed through before any of its ids
    /// is looked up here: a dead id is then a typed answer, never the panic of a lookup
    /// ([`Memo::slot`]). A spill id is refused whatever its arena: it is of the spill table of
    /// the snapshot that made it, which a value kept across snapshots cannot say.
    pub fn live<T: crate::Home>(&self, value: T) -> Result<Live<'_, T>, Dead> {
        match crate::home::first_dead(self, &value) {
            None => Ok(Live { value, _memo: std::marker::PhantomData }),
            Some(named) => Err(Dead { named, held: self.held_at(named.index()) }),
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
    /// [`NextRefused::GenerationsSpent`] naming the lowest arena to drop whose index has made
    /// its last generation in the id space. The refusal is final for this snapshot (it has a
    /// successor no longer, and never will; it keeps answering), and a new session of the
    /// database would be refused too ([`Memo::next_session`]): the driver starts a new id space.
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
        // A sealed arena may be named: it goes with every arena that borrows it, as any arena
        // does (see "Sealed arenas" on `Memo::changed`).
        if self.config.plant != Plant::CarryChanged {
            for id in changed {
                let slot = self.slot(*id);
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
        // Whether `slot` borrowed an arena in `dropped`; under the planted `SealedSkipsBorrow`,
        // as though no borrow of a sealed arena had been noted.
        let borrowed = |slot: &Slot, dropped: &[u64]| {
            if self.config.plant != Plant::SealedSkipsBorrow {
                return slot.borrowed_any(dropped);
            }
            let mut unsealed = Vec::new_in(Global);
            unsealed.extend_from_slice(dropped);
            for sealed in self.slots.iter().flatten().filter(|it| it.is_sealed()) {
                let index = sealed.id.index();
                unsealed[index as usize / 64] &= !(1_u64 << (index % 64));
            }
            slot.borrowed_any(&unsealed)
        };
        if self.config.plant != Plant::IgnoreBorrows {
            loop {
                let mut grew = false;
                for slot in self.slots.iter().flatten() {
                    let index = slot.id.index();
                    if !is(&dropped, index) && borrowed(&**slot, &dropped[..]) {
                        mark(&mut dropped, index);
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
        }
        let mut carried: u64 = self
            .slots
            .iter()
            .flatten()
            .filter(|slot| !is(&dropped, slot.id.index()))
            .map(|slot| slot.held() as u64)
            .sum();
        // Selective unsealed-first eviction: when carried memory exceeds `cap_bytes`, evict
        // unsealed arenas first (largest first, oldest generation, downstream-most index) while
        // maintaining the borrow closure, preserving warm sealed dependency arenas unless still
        // over cap after all unsealed arenas are dropped.
        while carried > self.config.cap_bytes {
            let victim = self
                .slots
                .iter()
                .flatten()
                .filter(|slot| !is(&dropped, slot.id.index()))
                .min_by_key(|slot| {
                    let index = slot.id.index();
                    (
                        index == ArenaId::SESSION_INDEX,
                        slot.is_sealed(),
                        std::cmp::Reverse(slot.held()),
                        slot.id.generation(),
                        std::cmp::Reverse(index),
                    )
                });
            let Some(victim) = victim else {
                break;
            };
            mark(&mut dropped, victim.id.index());
            carried = carried.saturating_sub(victim.held() as u64);
            if self.config.plant != Plant::IgnoreBorrows {
                loop {
                    let mut grew = false;
                    for slot in self.slots.iter().flatten() {
                        let index = slot.id.index();
                        if !is(&dropped, index) && borrowed(&**slot, &dropped[..]) {
                            mark(&mut dropped, index);
                            carried = carried.saturating_sub(slot.held() as u64);
                            grew = true;
                        }
                    }
                    if !grew {
                        break;
                    }
                }
            }
        }
        // A dropped arena is made again at its index's next generation in the id space: none
        // left, and the making would take a generation an earlier making had. Ascending, so the
        // lowest is named.
        let dropped_indices =
            self.slots.iter().flatten().map(|slot| slot.id.index()).filter(|index| is(&dropped, *index));
        if let Some(index) = self.ids.first_spent(dropped_indices) {
            let spent = self.slots[index as usize].as_deref().expect("a held arena's index");
            return Err(NextRefused::GenerationsSpent { arena: spent.id });
        }
        let mut slots = Slots::new(self.config.arenas);
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
                // clone: a reference count of the database's one id space
                ids: self.ids.clone(),
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
    /// The database's id space ([`Ids`], [`Memo`]'s, shared): each index's next generation, so
    /// a remade arena's ids differ from every earlier making's, in any session of the database.
    ids: Ids,
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
    /// The contract on `index` (the driver's, base-db's): 0 is the session arena, the open
    /// arenas ([`Inputs::open`], the source roots' files) come next, and then each crate's in a
    /// topological order of the crate graph (dependencies first). A key's home is the greatest index it names, so that order makes
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
        // Never wraps: an index at its last generation in the id space is held (made) until
        // `Memo::next` would drop it, and `next` refuses that drop, as `Memo::next_session`
        // refuses a session that would make it (`NextRefused::GenerationsSpent`).
        let id = self.ids.take(index).unwrap_or_else(|| {
            panic!(
                "arena index {index} has no generation left in its id space: `Memo::next` \
                 refuses its drop and `Memo::next_session` a session that makes it again"
            )
        });
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
            // An open arena is any arena's to name: what it declared would be in the closure of
            // none of them, so a carried namer could hold its dependency's dead ids.
            if slot.is_open()
                && let Some(index) = slot.declared().find(|index| *index != ArenaId::SESSION_INDEX)
            {
                panic!(
                    "{:?} is open (`Inputs::open`) but declared a borrow of arena index {index}: an \
                     open arena borrows only the session",
                    slot.id
                );
            }
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
            ids: self.ids,
        }
    }
}

/// A value whose every id names an arena the snapshot `'m` borrows holds at that generation
/// ([`Memo::live`]): the proof a reader of a table kept across [`Memo::next`] takes before it
/// looks any of the value's ids up in that snapshot (rule verify-once). Made only by
/// [`Memo::live`]; it lives no longer than the borrow of the snapshot that checked it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Live<'m, T> {
    value: T,
    _memo: std::marker::PhantomData<&'m Memo>,
}

impl<T: Copy> Live<'_, T> {
    /// The value, live in the snapshot that checked it.
    pub fn get(&self) -> T {
        self.value
    }
}

/// Why [`Memo::live`] refused a value: the first arena it names that the snapshot does not hold
/// at that generation, and what the snapshot holds at that index (none, an earlier making, or
/// a later one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dead {
    pub named: ArenaId,
    pub held: Option<ArenaId>,
}

impl Dead {
    /// Whether neither the snapshot nor any made after it can hold the id: it holds a later
    /// making of the named index, or this very making, which then refused a spill id (of the
    /// spill table of an older snapshot, which only that snapshot reads). Generations only rise
    /// within an id space, and a new id space is a new database nonce.
    pub fn superseded(&self) -> bool {
        self.held.is_some_and(|held| held.generation() >= self.named.generation())
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

    /// Seals this arena: a crate that is not being built, whose sources are pinned.
    /// [`Memo::changed`] is `false` for it while it lives, carried or not. [`Memo::next`] takes
    /// it in a changed set like any arena, and drops it with every arena that borrows it.
    pub fn seal(&self) {
        self.slot.seal();
    }

    /// Opens this arena: a key homed in any arena of the snapshot may name its ids, though
    /// no arena declared a borrow of it, and the entry it keys borrows it when written
    /// ([`crate::home::mark_borrows`], at a run's end and at an image record's serving alike),
    /// as a read of it does. For inputs that no crate owns and that a crate is found to read
    /// only once it runs: a source root's files, which a `#[path]` module reaches from another
    /// package. So an edit of them drops the arenas that named or read them, and their
    /// readers, and no arena that only might have.
    ///
    /// The contract: an open arena declares no borrow but the session's, and its index is
    /// below every arena whose keys name it (a key's home is the greatest index it names, so
    /// a key naming an open arena and a crate is homed at the crate). Its own entries are
    /// keyed by its ids alone and read no arena of a crate ([`crate::task`]'s read check).
    ///
    /// # Panics
    ///
    /// Where the snapshot is built already; at [`Builder::build`], where it declared a borrow
    /// of an arena other than the session.
    pub fn open(&self) {
        self.slot.open();
    }
}

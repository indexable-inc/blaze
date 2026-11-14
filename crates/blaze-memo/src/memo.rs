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
    ArenaId, GENERATION_MASK, Gid, MAX_ARENAS,
    arena::Slot,
    flight::Flight,
    trie::Head,
};

/// Each snapshot's epoch: which snapshot may write to a shared arena.
static EPOCHS: AtomicU64 = AtomicU64::new(1);

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
    /// [`Memo::next`] ignores borrows: a dependent of a changed arena is carried.
    IgnoreBorrows,
    /// A hit does not replay its entry's reads into the asker.
    HitSkipsReplay,
    /// A hit checks its entry again (walks its read list as a revalidation would): the check
    /// decision 18 forbids, which the counters show.
    HitChecks,
    /// A fixpoint member's flight is let go when its run returns, not when its head settles:
    /// another task can run the member a second time in the snapshot.
    MemberLetGo,
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

/// One snapshot: the arenas its queries read and fill. Inputs never change while it lives.
pub struct Memo {
    epoch: u64,
    /// Arena index to its arena; `None` where this snapshot has none.
    slots: Box<[Option<Arc<Slot>>], Global>,
    /// What this snapshot computes for an arena it shares with a successor.
    spill: Arc<Slot>,
    config: Config,
    succeeded: AtomicBool,
}

impl Memo {
    /// A builder of a session's first snapshot.
    pub fn builder(config: Config) -> Builder {
        assert!(
            config.arenas >= 1 && config.arenas <= MAX_ARENAS,
            "a session has 1 to {MAX_ARENAS} arena indices, not {}",
            config.arenas
        );
        let epoch = EPOCHS.fetch_add(1, Ordering::Relaxed);
        let mut slots = Vec::new_in(Global);
        slots.resize_with(config.arenas as usize, || None);
        Builder { epoch, slots, generations: None, config }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
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
    /// # Panics
    ///
    /// Where this snapshot already has a successor.
    pub fn next(&self, changed: &[ArenaId]) -> Next {
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
        let mut slots = Vec::new_in(Global);
        slots.resize_with(self.config.arenas as usize, || None);
        let mut generations = Vec::new_in(Global);
        generations.resize(self.config.arenas as usize, 0_u32);
        let mut gone = Vec::new_in(Global);
        for slot in self.slots.iter().flatten() {
            let index = slot.id.index();
            generations[index as usize] = slot.id.generation();
            if is(&dropped, index) {
                gone.push(index);
            } else {
                slots[index as usize] = Some(Arc::clone(slot));
            }
        }
        Next {
            builder: Builder {
                epoch,
                slots,
                generations: Some(generations),
                config: self.config.clone(),
            },
            dropped: gone,
        }
    }
}

/// Builds a snapshot: the driver adds each arena and its inputs.
pub struct Builder {
    epoch: u64,
    slots: Vec<Option<Arc<Slot>>, Global>,
    /// The generation each index last had (a next snapshot's), so a remade arena's ids differ.
    generations: Option<Vec<u32, Global>>,
    config: Config,
}

impl Builder {
    /// Adds an empty arena at `index`, its memory counted to `tag`, and gives the handle its
    /// inputs are made through.
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
        let generation = match &self.generations {
            Some(it) => (it[at] + 1) & GENERATION_MASK,
            None => 0,
        };
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

    pub fn build(self) -> Memo {
        let spill = Arc::new_in(
            Slot::new(ArenaId::SESSION, Tag::Query, self.epoch, self.config.arenas),
            Global,
        );
        Memo {
            epoch: self.epoch,
            slots: self.slots.into_boxed_slice(),
            spill,
            config: self.config,
            succeeded: AtomicBool::new(false),
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

    /// Says this arena's inputs hold ids of arena `from` (a crate's input naming its
    /// dependencies'): it is dropped whenever `from` is, as if an entry had read it. The driver
    /// calls it for each such arena; an input field holding an id of an arena not declared
    /// here is an id that can go dead while this arena is carried.
    pub fn borrows(&self, from: ArenaId) {
        self.slot.borrow(from, self.epoch);
    }
}

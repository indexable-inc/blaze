//! The index table built off the store's thread (docs/v2/plan/design/store.md, "Item sets").
//!
//! With one value an item a crate puts tens of thousands of records, so the table is due
//! every crate or two. Built in a commit ([`Store::index_entries`]) it holds the store's
//! thread for a walk of every row the store has, a sort of them and 64 bytes written a row.
//! Here the thread only hands out an [`IndexBuild`] ([`Store::index_take`]: two counts and a
//! flag and an end a pack) and takes the bytes back ([`Store::index_put`]: a flag a pack
//! compared, one write asked); the walk, the sort and the table are a worker's
//! ([`IndexBuild::run`]). No build waits for it: puts and commits go on while it is out.
//!
//! The bound: on the store's thread a take and a put are each O(packs), whatever the rows.
//! The swap that follows a table written (`Store::try_flip`, and the fold after it) still
//! walks the delta on the store's thread: the records put since the table before, 65,536 and
//! what was put while the build ran, never the rows of the index table.
//!
//! The invariant: a head that names an index table names, for each pack, an `indexed` end
//! such that every record of the pack a key finds is in the table or at or past that end
//! (where an open's scan reads it). A build is taken only while no committed record waits to
//! be pointed at its pack (`Store::pending` is empty), so every record under a pack's
//! committed end then is in the table before, has a pack spot in the delta, or is found by
//! no key; the build holds the first two and its head names those ends, not the ends at the
//! time of the write. A record committed while the build ran is past them. A pack retired
//! while it ran drops the build (the table would name records that are gone): it is taken
//! again. Proved by `tests::rebuild` (crashes at every write and sync; the planted bug is
//! the head naming the ends at the time of the write).

use std::{alloc::Global, sync::Arc};

use blaze_core::Effect;
use blaze_hash::ByHashSet;
use blaze_reclaim::enter;

use crate::{
    INDEX_AFTER, Key, Store,
    disk::Loc,
    index,
    read::{Index, Place, Spot},
    table::{Held, Table},
};

/// Builds lost to their worker in a row before the store stops asking for one and says so
/// (rule bounded-progress: a build that panics every time is not asked for at every turn).
const LOST_MOST: u32 = 3;

/// The next index table of a store, to be built on any thread ([`IndexBuild::run`]) and
/// handed back to [`Store::index_put`].
pub struct IndexBuild {
    index: Option<Arc<Index>>,
    delta: Arc<Table<Spot>>,
    /// Each pack's committed end when the build was taken: the table holds the records under
    /// it, and the head that names the table names it as the pack's indexed end.
    marks: Vec<u64>,
    /// Each pack retired or being retired when the build was taken.
    passed: Vec<bool>,
    /// The slot it is written to: the one the head did not name when it was taken.
    slot: u8,
    /// The head's generation when it was taken.
    generation: u64,
    ran: bool,
    bytes: Vec<u8>,
    count: u64,
    /// Records the table holds that the table before did not, or held elsewhere.
    fresh: u64,
    /// Rows walked: the delta's and the table's before.
    rows: u64,
    problems: Vec<&'static str>,
}

impl IndexBuild {
    /// Rows it walked.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Records its table holds.
    pub fn records(&self) -> u64 {
        self.count
    }

    /// Bytes of its table.
    pub fn bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn covers(&self, loc: Loc) -> bool {
        self.marks.get(loc.pack as usize).is_some_and(|&mark| loc.offset < mark)
            && !self.passed[loc.pack as usize]
    }

    /// Walks the delta and the table before and builds the table. The delta moves under it
    /// (puts, commits, cold moves go on): a spot counts only as a record in a pack under
    /// the pack's end at the take, which no later write changes.
    pub fn run(&mut self) {
        let guard = enter();
        let held = Held::task(&guard);
        // alloc: once an index table built, on its worker: the rows it holds
        let mut entries: Vec<(Key, Loc)> = Vec::new();
        // alloc: once an index table built, on its worker: the keys the delta gave
        let mut taken: ByHashSet<Key, Global> = ByHashSet::new_in(Global);
        let mut rows = 0u64;
        for (_, spot) in self.delta.iter(held) {
            rows += 1;
            if let Place::Pack(loc) = spot.place
                && self.covers(loc)
                && taken.insert(spot.key)
            {
                entries.push((spot.key, loc));
            }
        }
        self.fresh = entries.len() as u64;
        if let Some(before) = &self.index {
            for (_, key, loc) in before.table().iter() {
                rows += 1;
                match loc {
                    Ok(_) if taken.contains(&key) => {}
                    // A key the collector forgot: the pack it was in is retired.
                    Ok(loc) if self.passed.get(loc.pack as usize).copied().unwrap_or(true) => {}
                    Ok(loc) => entries.push((key, loc)),
                    Err(what) => self.problems.push(what),
                }
            }
        }
        self.bytes = index::build_index(&entries);
        self.count = entries.len() as u64;
        self.rows = rows;
        self.ran = true;
    }
}

/// What a store did about its index tables built off its thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IndexCounts {
    /// Builds handed out.
    pub taken: u64,
    /// Tables a commit wrote from a build.
    pub written: u64,
    /// Builds dropped because a pack was retired while they ran.
    pub dropped: u64,
    /// Builds that found too few records committed to be worth a table.
    pub barren: u64,
    /// Builds lost with their worker.
    pub lost: u64,
    /// Rows the store's own thread walked to build a table: 0 for a store whose tables are
    /// built off it.
    pub loop_rows: u64,
}

/// What the store keeps for its index builds.
pub(crate) struct Rebuild {
    /// Tables are built off the store's thread ([`Store::index_off_thread`]); else in the
    /// commit that finds one due.
    pub(crate) off_thread: bool,
    out: bool,
    /// A build that ran, for the next commit to write.
    ready: Option<IndexBuild>,
    /// The head's generation at a build that was barren: none is taken until a commit after.
    barren_at: Option<u64>,
    lost_run: u32,
    pub(crate) counts: IndexCounts,
    /// The planted bug of `tests::rebuild`: the head that names a built table names each
    /// pack's end at the time of the write as indexed, not its end when the build was taken.
    #[cfg(test)]
    pub(crate) plant_indexed_at_write: bool,
}

/// What a commit writes of a build.
pub(crate) struct Built {
    pub(crate) bytes: Vec<u8>,
    pub(crate) count: u64,
    pub(crate) marks: Vec<u64>,
    pub(crate) problems: Vec<&'static str>,
}

impl Rebuild {
    pub(crate) fn new() -> Rebuild {
        Rebuild {
            off_thread: false,
            out: false,
            ready: None,
            barren_at: None,
            lost_run: 0,
            counts: IndexCounts::default(),
            #[cfg(test)]
            plant_indexed_at_write: false,
        }
    }

    /// The build that ran, for a commit to write to `slot`, if it still holds: no pack was
    /// retired since it was taken (`passed`: each pack's flag now). One that does not is
    /// dropped, and taken again.
    pub(crate) fn ready_for(&mut self, passed: impl Iterator<Item = bool>, slot: u8) -> Option<Built> {
        let build = self.ready.take()?;
        let holds = build.slot == slot
            && build.passed.iter().copied().eq(passed.take(build.passed.len()));
        if !holds {
            self.counts.dropped += 1;
            return None;
        }
        self.counts.written += 1;
        Some(Built {
            bytes: build.bytes,
            count: build.count,
            marks: build.marks,
            problems: build.problems,
        })
    }
}

impl Store {
    /// From now on its index tables are built off its thread: its driver takes each
    /// ([`Store::index_take`]), runs it on a worker and hands it back. A store never told so
    /// builds them in the commit that finds one due (a tool's, a test's: no worker).
    pub fn index_off_thread(&mut self) {
        self.rebuild.off_thread = true;
    }

    pub fn index_counts(&self) -> IndexCounts {
        self.rebuild.counts
    }

    /// Whether a build is with its driver, or back and waiting for a commit to write it.
    pub fn index_building(&self) -> bool {
        self.rebuild.out || self.rebuild.ready.is_some()
    }

    /// The next index table to build, when one is due and can be taken: the records not in
    /// the table reach [`INDEX_AFTER`], no build is out or waiting to be written, no table
    /// is being swapped in, and every committed record is pointed at its pack. O(packs):
    /// no row is walked here. `None` otherwise: ask again at a later turn.
    pub fn index_take(&mut self) -> Option<IndexBuild> {
        let rebuild = &self.rebuild;
        if !rebuild.off_thread
            || rebuild.out
            || rebuild.ready.is_some()
            || rebuild.lost_run >= LOST_MOST
            || self.broken.is_some()
            || self.index_mapping
            || self.flip.is_some()
            || self.migrating.is_some()
            || !self.pending.is_empty()
        {
            return None;
        }
        let own = &self.tiers[0];
        if !own.opened
            || own.delta.len() < INDEX_AFTER
            || rebuild.barren_at == Some(own.head.generation)
        {
            return None;
        }
        let build = IndexBuild {
            // clone: an `Arc`, once a build
            index: own.index.clone(),
            delta: Arc::clone(&own.delta), // clone: an `Arc`, once a build
            // alloc: once a build, an end a pack
            marks: own.packs.iter().map(|it| it.committed).collect(),
            // alloc: once a build, a flag a pack
            passed: own.packs.iter().map(|it| it.removed || it.retiring).collect(),
            slot: match own.head.index {
                Some(it) => 1 - it.slot,
                None => 0,
            },
            generation: own.head.generation,
            ran: false,
            bytes: Vec::new(),    // alloc: empty, allocates nothing
            count: 0,
            fresh: 0,
            rows: 0,
            problems: Vec::new(), // alloc: empty, allocates nothing
        };
        self.rebuild.out = true;
        self.rebuild.counts.taken += 1;
        Some(build)
    }

    /// A build that ran: the next commit writes its table, and one begins now when none is
    /// in flight. A build that found fewer than half of [`INDEX_AFTER`] records committed
    /// (the rest of the delta is still in writers' segments) is dropped, and none is taken
    /// until a commit after the one it saw. O(packs).
    pub fn index_put(&mut self, build: IndexBuild, effects: &mut Vec<Effect>) {
        if !build.ran {
            return self.index_lost();
        }
        self.rebuild.out = false;
        self.rebuild.lost_run = 0;
        if (build.fresh as usize) < INDEX_AFTER / 2 {
            self.rebuild.barren_at = Some(build.generation);
            self.rebuild.counts.barren += 1;
            return;
        }
        self.rebuild.ready = Some(build);
        if self.broken.is_some() {
            return;
        }
        if self.commit.is_some() {
            self.flush_wanted = true;
        } else {
            self.start_commit(effects);
        }
    }

    /// A build was lost with its worker (a panic): another is taken at a later turn, up to
    /// [`LOST_MOST`] in a row; past that the store builds no table off its thread and says
    /// so in its problems (the records stay found through the delta, which grows).
    pub fn index_lost(&mut self) {
        self.rebuild.out = false;
        self.rebuild.lost_run += 1;
        self.rebuild.counts.lost += 1;
        if self.rebuild.lost_run == LOST_MOST {
            // alloc: a problem reported, once
            self.problems.push(format!(
                "the index table of {} is not built any more: {LOST_MOST} builds in a row were lost with their worker",
                self.tiers[0].index_file()
            ));
        }
    }
}

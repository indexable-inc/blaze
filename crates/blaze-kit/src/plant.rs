//! Planted bugs for the oracles' controls (rule 53, `the_oracle_catches_..`): a test sets one
//! for the length of a closure on its own thread, and the code it names misbehaves on purpose.
//! The crate's code reads it through [`on`], which is `false` outside tests and folds away.

#[cfg(test)]
use core::cell::Cell;

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// Nothing planted: the code as it is.
    #[cfg(test)]
    Clean,
    /// `Table::get` answers for an id one past the end.
    TableGetPastEnd,
    /// `InlineVec`'s spill to the heap keeps one value fewer than it had.
    SpillDropsLast,
    /// `UnionFind`'s path halving inside a snapshot writes no undo record.
    HalvingUnlogged,
    /// `SparseSet`'s compaction forgets to move the last live element's position.
    CompactKeepsStalePosition,
    /// `IntervalSet::insert` merges a range with its right neighbour only when they overlap,
    /// not when they touch.
    IntervalMissesAdjacent,
    /// `ChunkedBitSet::insert` writes into a shared chunk without copying it first.
    ChunkWritesShared,
    /// `Heap4`'s sift-down compares only the first three children.
    HeapSkipsFourthChild,
    /// `TimerWheel` moves its time to the poll's `now` instead of the slot's start when it
    /// cascades, so a slot between the two is skipped.
    WheelCascadesLate,
    /// The LEB128 writer drops the last byte of a value of ten.
    LebDropsLastByte,
    /// `Dict` gives a new value the code of the one before it.
    DictReusesCode,
    /// `StaticSearch`'s descent takes the child one to the left.
    SearchChildLeft,
}

#[cfg(test)]
std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

/// Whether `plant` is planted on this thread.
#[cfg(test)]
pub(crate) fn on(plant: Plant) -> bool {
    PLANTED.with(Cell::get) == plant
}

/// Nothing is planted outside tests.
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn on(_plant: Plant) -> bool {
    false
}

#[cfg(test)]
/// Runs `body` with `plant` planted on this thread, and puts the clean code back after, a panic
/// out of `body` among the ways.
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

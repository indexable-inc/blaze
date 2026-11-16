//! Planted bugs for the oracles' controls (rule 53, `the_oracle_catches_..`): a test sets one
//! for the length of a closure on its own thread, and the code it names misbehaves on purpose.
//! Compiled into tests only; the crate's code reads it through [`on`], which is `false` outside.

use core::cell::Cell;

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// Nothing planted: the code as it is.
    Clean,
    /// `Vec::truncate` shortens the length but drops nothing it cut off.
    TruncateForgetsDrop,
    /// `Vec::retain` frees no element it removes.
    RetainForgetsDrop,
    /// `Arc::clone` hands out a second owner without raising the count.
    ArcCloneSkipsCount,
    /// `VecDeque::grow` forgets to move the wrapped part of the ring.
    DequeGrowKeepsWrap,
    /// The struct-of-arrays vector's growth copies every column but the last.
    SoaGrowSkipsColumn,
    /// The B-tree's merge of two internal nodes forgets the right one's last child (and the
    /// entry before it); the map's length follows, so only the entries tell.
    BTreeMergeForgetsChild,
    /// An `Alloc` handle's `grow` entry allocates a fresh block instead: the old bytes are lost
    /// and the old block is never freed.
    HandleGrowAllocates,
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
    static LOST: Cell<usize> = const { Cell::new(0) };
}

/// A planted bug lost `n` entries; the code that keeps a count takes them with [`take_lost`].
pub(crate) fn lose(n: usize) {
    LOST.with(|lost| lost.set(lost.get() + n));
}

/// The entries lost since the last call, the count reset.
pub(crate) fn take_lost() -> usize {
    LOST.with(|lost| lost.replace(0))
}

/// Whether `plant` is planted on this thread.
pub(crate) fn on(plant: Plant) -> bool {
    PLANTED.with(Cell::get) == plant
}

/// Runs `body` with `plant` planted on this thread, and puts the clean code back after, a
/// panic out of `body` among the ways.
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

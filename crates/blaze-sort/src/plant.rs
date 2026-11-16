//! Planted bugs for the oracles' controls (rule 53): a test sets one for the length of a
//! closure on its own thread, and the code it names misbehaves on purpose. The crate reads it
//! through [`on`], which is `false` outside tests and folds away.

#[cfg(test)]
use core::cell::Cell;

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// Nothing planted: the code as it is.
    #[cfg(test)]
    Clean,
    /// The radix sorts skip the least significant digit on which the keys differ, as if it
    /// were constant.
    SkipVaryingDigit,
    /// A signed integer's image keeps its sign bit, so negatives order after positives.
    SignNotFlipped,
    /// A float's image of `-0.0` is `+0.0`'s, so the two zeros order as one.
    ZerosMerged,
    /// The vector `position` reports the first hit of the second vector of a block before one
    /// in the first.
    FindSecondVectorFirst,
    /// The vector mismatch skips the first vector of every four-vector block.
    MismatchSkipsVector,
    /// `lower_bound` stops one probe early on slices longer than eight.
    LowerBoundShort,
    /// `sort_by_key_in` sorts its keys with the in-place, unstable radix sort.
    ByKeyUnstable,
}

#[cfg(test)]
std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

/// Whether `plant` is planted on this thread.
#[cfg(test)]
#[inline]
pub(crate) fn on(plant: Plant) -> bool {
    PLANTED.with(Cell::get) == plant
}

/// Nothing is planted outside tests.
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn on(_plant: Plant) -> bool {
    false
}

/// Runs `body` with `plant` planted on this thread, and puts the clean code back after, a panic
/// out of `body` among the ways.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let _back = core::mem::DropGuard::new((), |()| PLANTED.with(|it| it.set(Plant::Clean)));
    PLANTED.with(|it| it.set(plant));
    body()
}

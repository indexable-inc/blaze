//! Planted bugs for the oracles' controls (rule 53): a test sets one for the length of a
//! closure on its own thread, and the code that reads it misbehaves on purpose. Outside tests
//! [`on`] is `false` and folds away.

#[cfg(test)]
use core::cell::Cell;

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// Nothing planted: the code as it is.
    #[cfg(test)]
    Clean,
    /// `bits::pack` keeps one bit fewer than the width of each value.
    PackDropsTopBit,
    /// The vector unpack shifts its last lane one bit short.
    LaneShiftShort,
    /// A delta block's `get` sums one delta fewer than it should.
    DeltaGetSumsShort,
    /// A run-length block's `get` takes the run before the one it found.
    RunSearchLeft,
    /// The dictionary writes each code past the first as the one after it.
    DictCodePlusOne,
    /// FSST's encoder writes the escape code but not the byte it escapes.
    EscapeDropsByte,
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

/// Runs `body` with `plant` planted on this thread, putting the clean code back on every exit.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

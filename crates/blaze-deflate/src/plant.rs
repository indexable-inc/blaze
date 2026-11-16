//! Planted bugs for the oracles' controls (rule 53): a test sets one for the length of a closure
//! on its own thread, and the code it names misbehaves on purpose. Outside tests [`on`] is
//! `false` and folds away.

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// Nothing planted: the code as it is.
    #[cfg(test)]
    Clean,
    /// The inflater never holds its output to the ratio: a bomb gets through.
    RatioUnchecked,
    /// The deflater sends every match one byte farther back than it found it.
    DistanceOffByOne,
    /// The gzip reader takes any CRC-32 in a trailer.
    GzipCrcUnchecked,
    /// The fast loop copies every match from one byte farther back than its distance says.
    FastDistanceOffByOne,
}

#[cfg(test)]
std::thread_local! {
    static PLANTED: core::cell::Cell<Plant> = const { core::cell::Cell::new(Plant::Clean) };
}

/// Whether `plant` is planted on this thread.
#[cfg(test)]
pub(crate) fn on(plant: Plant) -> bool {
    PLANTED.with(core::cell::Cell::get) == plant
}

#[cfg(not(test))]
#[inline(always)]
pub(crate) const fn on(_plant: Plant) -> bool {
    false
}

/// Runs `body` with `plant` planted on this thread, and puts the clean code back after.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

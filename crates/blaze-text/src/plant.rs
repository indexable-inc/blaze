//! The planted bug for the integer oracle's control (rule 53).

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    /// The two-digit table is read one pair off: `r` writes the digits of `r + 1`.
    DigitPairOffByOne,
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

/// The pair index the table path reads for `r`.
#[inline]
pub(crate) fn digit_pair(r: usize) -> usize {
    match PLANTED.with(Cell::get) {
        Plant::Clean => r,
        Plant::DigitPairOffByOne => (r + 1) % 100,
    }
}

pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

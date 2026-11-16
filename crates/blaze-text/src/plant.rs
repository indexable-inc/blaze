//! The planted bug for the integer oracle's control (rule 53).

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    /// The two-digit table is read one pair off: `r` writes the digits of `r + 1`.
    DigitPairOffByOne,
    /// The debug walk writes every named field as `field`.
    DebugFieldNameLost,
}

/// The name the debug walk writes for a field.
#[inline]
pub(crate) fn field_name(name: &'static str) -> &'static str {
    match PLANTED.with(Cell::get) {
        Plant::DebugFieldNameLost => "field",
        _ => name,
    }
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

/// The pair index the table path reads for `r`.
#[inline]
pub(crate) fn digit_pair(r: usize) -> usize {
    match PLANTED.with(Cell::get) {
        Plant::DigitPairOffByOne => (r + 1) % 100,
        _ => r,
    }
}

pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

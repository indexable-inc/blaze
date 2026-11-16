//! The planted bug for the round-trip oracle's control (rule 53).

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    /// A `"` inside a string is written bare.
    QuoteUnescaped,
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

pub(crate) fn escape_quote() -> bool {
    PLANTED.with(Cell::get) == Plant::Clean
}

pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

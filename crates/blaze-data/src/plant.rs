//! The planted bug for the map oracle's control (rule 53).

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    /// Inserting a present key adds a second entry instead of replacing the first.
    DuplicateOnHit,
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

pub(crate) fn replace_on_hit() -> bool {
    PLANTED.with(Cell::get) == Plant::Clean
}

pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

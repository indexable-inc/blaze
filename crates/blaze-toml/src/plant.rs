//! The planted bug for the edit oracle's control (rule 53).

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    /// A section's body end is kept one byte short, before its last newline.
    BodyEndShort,
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

pub(crate) fn body_end(end: usize) -> usize {
    match PLANTED.with(Cell::get) {
        Plant::Clean => end,
        Plant::BodyEndShort => end.saturating_sub(1),
    }
}

pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

//! The planted bug for the precedence oracle's control (rule 53).

use core::cmp::Ordering;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    Clean,
    /// Numeric identifiers compare as text, so `beta.11 < beta.2`.
    NumbersAsText,
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

pub(crate) fn identifier(a: &str, b: &str, order: Ordering) -> Ordering {
    match PLANTED.with(Cell::get) {
        Plant::NumbersAsText => a.cmp(b),
        Plant::Clean => order,
    }
}

pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

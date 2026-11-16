//! The planted bug for the round-trip oracle's control (rule 53): set on one thread for the
//! length of a closure, it makes the encoder misbehave on purpose.

use std::cell::Cell;

/// A bug the control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// The code as it is.
    Clean,
    /// A last quantum of two bytes sets a bit past the second byte in its third symbol.
    TailSetsBit,
}

std::thread_local! {
    static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
}

/// The third symbol's value of a two-byte tail, as the plant has it.
pub(crate) fn tail_bits(v: u8) -> u8 {
    match PLANTED.with(Cell::get) {
        Plant::TailSetsBit => v | 1,
        Plant::Clean => v,
    }
}

/// Runs `body` with `plant` planted on this thread, putting the clean code back on every exit.
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

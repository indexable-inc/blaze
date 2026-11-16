//! The planted bugs of the controls (rule 43, testing skill): each turns one rule wrong while
//! set on this thread, so a control can watch its oracle catch it.

use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    None,
    /// A coinductive cycle's initial result taken as an inductive one (no solution).
    CoinductiveAsInductive,
    /// The prefilter treats an impl's wildcard byte as a mismatch.
    PrefilterDropsWildcard,
}

std::thread_local! {
    static PLANT: Cell<Plant> = const { Cell::new(Plant::None) };
}

pub fn on(p: Plant) -> bool {
    PLANT.with(|c| c.get() == p)
}

/// Runs `f` with `p` planted.
pub fn with<R>(p: Plant, f: impl FnOnce() -> R) -> R {
    let old = PLANT.with(|c| c.replace(p));
    let r = f();
    PLANT.with(|c| c.set(old));
    r
}

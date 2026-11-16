//! Planted bugs for the oracles' controls (rule 53): a test sets one for the length of a closure
//! on its own thread, and the code it names misbehaves on purpose. The crate's code reads it
//! through [`on`], which is `false` outside tests and folds away.

#[cfg(test)]
use core::cell::Cell;

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// Nothing planted: the code as it is.
    #[cfg(test)]
    Clean,
    /// The unique table interns a node whose high edge is complemented as it is, instead of
    /// complementing both edges and the result: a function and its negation stop sharing nodes,
    /// and one function can get two ids.
    UniqueSkipsComplement,
    /// The `ite` cache is keyed by `f` and `g` only: an entry for `ite(f, g, h)` answers
    /// `ite(f, g, h2)`.
    IteCacheSkipsElse,
    /// `restrict(f, x, v)` takes the cofactor of `!v` at a node of `x`.
    RestrictWrongBranch,
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

/// Runs `body` with `plant` planted on this thread, and puts the clean code back after, a panic
/// out of `body` among the ways.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

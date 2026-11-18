//! Planted bugs for the simulation's controls (rule 53): a test sets one for the length of a
//! closure on its own thread, and the driver misbehaves on purpose where it reads [`on`]. Outside
//! tests [`on`] is always `false` and nothing is kept.

/// A bug a control plants: one per law of the driver (lib.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant; the driver reads `on`"))]
pub(crate) enum Plant {
    /// Nothing planted: the driver as it is.
    Clean,
    /// A worker's frame is not reset when its task returns (law e).
    FrameNotReset,
    /// The merge and the commit take tasks in the order they finished (law f).
    CompletionOrder,
    /// A waiting task names its direct input as its root (law b).
    WaitedOnDirect,
    /// A worker that frees up after a stop starts the next ready task (law d).
    StartAfterStop,
    /// The merge is given before the commit has finished (law c).
    VisibleBeforeCommit,
}

#[cfg(test)]
std::thread_local! {
    static PLANTED: core::cell::Cell<Plant> = const { core::cell::Cell::new(Plant::Clean) };
}

/// Whether `plant` is planted on this thread.
pub(crate) fn on(plant: Plant) -> bool {
    #[cfg(test)]
    {
        PLANTED.with(core::cell::Cell::get) == plant
    }
    #[cfg(not(test))]
    {
        let _ = plant;
        false
    }
}

/// Runs `body` with `plant` planted on this thread, and puts the clean driver back after.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

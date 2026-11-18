//! Planted bugs for the simulation's controls (rule 53): a test sets one for the length of a
//! closure on its own thread, and the machine misbehaves on purpose where it reads [`on`]. Outside
//! tests [`on`] is always `false` and nothing is kept.

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant; the machine reads `on`"))]
pub(crate) enum Plant {
    /// Nothing planted: the machine as it is.
    Clean,
    /// A glob's binding wins over an explicit item or import of the same name.
    GlobBeatsExplicit,
    /// The entries an expansion wrote are ignored (the calls are still expanded).
    IgnoreExpanded,
    /// The fixpoint stops after its first pass, settled or not.
    CutEarly,
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

/// Runs `body` with `plant` planted on this thread, and puts the clean machine back after.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

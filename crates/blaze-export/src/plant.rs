//! Planted bugs for the tests' controls (rule 53): a test sets one for the length of a closure on
//! its own thread, and the writer it names misbehaves on purpose, so the test shows its checker
//! catches that bug. Outside tests [`on`] is `false` and nothing here exists.

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code, reason = "`Clean` is set only by the tests' `with`"))]
pub enum Plant {
    /// Nothing planted.
    Clean,
    /// The Mach-O writer computes the code signature's page hashes before it writes the last
    /// chained-fixup slot.
    SignBeforeLastFixup,
}

#[cfg(test)]
std::thread_local! {
    static PLANTED: core::cell::Cell<Plant> = const { core::cell::Cell::new(Plant::Clean) };
}

/// Whether `plant` is planted on this thread.
#[cfg(test)]
pub fn on(plant: Plant) -> bool {
    PLANTED.with(|p| p.get() == plant)
}

/// Whether `plant` is planted: never, outside tests.
#[cfg(not(test))]
pub fn on(_plant: Plant) -> bool {
    false
}

/// Runs `f` with `plant` planted on this thread.
#[cfg(test)]
pub fn with<T>(plant: Plant, f: impl FnOnce() -> T) -> T {
    PLANTED.with(|p| p.set(plant));
    let out = f();
    PLANTED.with(|p| p.set(Plant::Clean));
    out
}

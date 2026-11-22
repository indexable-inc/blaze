//! Planted bugs for the oracles' controls (rule 53): a test sets one for the length of a closure
//! on its own thread, and the code it names misbehaves on purpose. Outside tests [`on`] is
//! `false` and folds away.

/// A bug a control plants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// Nothing planted: the code as it is.
    #[cfg(test)]
    Clean,
    /// The decoder's dynamic table evicts one entry too few when an insertion makes room
    /// (hpack/table.rs `Table::insert`): it is off by one against the encoder's.
    EvictOneTooFew,
    /// A WINDOW_UPDATE received for a stream is applied to the stream opened just before it
    /// (conn.rs `Connection::on_window_update`).
    WindowUpdateWrongStream,
}

#[cfg(test)]
std::thread_local! {
    static PLANTED: core::cell::Cell<Plant> = const { core::cell::Cell::new(Plant::Clean) };
}

/// Whether `plant` is planted on this thread.
#[cfg(test)]
pub(crate) fn on(plant: Plant) -> bool {
    PLANTED.with(core::cell::Cell::get) == plant
}

#[cfg(not(test))]
#[inline(always)]
pub(crate) const fn on(_plant: Plant) -> bool {
    false
}

/// Runs `body` with `plant` planted on this thread, and puts the clean code back after.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|p| p.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|p| p.set(before)));
    body()
}

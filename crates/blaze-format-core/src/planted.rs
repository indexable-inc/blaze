//! The bugs the controls of `verified_tests` plant in [`crate::open`]'s walk (rule 53): each off
//! but on the one thread, for the one closure, of the control that plants it. Outside this
//! crate's tests each is the constant `false`, and the check it guards is always made.

/// A bug a control plants.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// The code as it is.
    Clean,
    /// A reference may reach past its own field, to the end of the buffer: the bound that keeps
    /// every target inside the bytes before its field is skipped.
    Forward,
    /// A [`crate::Text`]'s bytes are not checked to be UTF-8.
    NotUtf8,
}

#[cfg(test)]
std::thread_local! {
    static PLANTED: core::cell::Cell<Plant> = const { core::cell::Cell::new(Plant::Clean) };
}

#[cfg(test)]
fn planted() -> Plant {
    PLANTED.with(core::cell::Cell::get)
}

/// Whether [`Plant::Forward`] is planted.
#[inline(always)]
pub(crate) fn forward() -> bool {
    #[cfg(test)]
    return planted() == Plant::Forward;
    #[cfg(not(test))]
    false
}

/// Whether [`Plant::NotUtf8`] is planted.
#[inline(always)]
pub(crate) fn not_utf8() -> bool {
    #[cfg(test)]
    return planted() == Plant::NotUtf8;
    #[cfg(not(test))]
    false
}

/// Runs `body` with `plant` planted on this thread, the clean code put back on every exit.
#[cfg(test)]
pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
    let before = PLANTED.with(|it| it.replace(plant));
    let _back = core::mem::DropGuard::new(before, |before| PLANTED.with(|it| it.set(before)));
    body()
}

//! The planted bug of the driver's simulation (rule 53): the `Content-Range` check skipped, set
//! by a test for one closure on its own thread. Outside tests [`range_unchecked`] is `false`
//! and folds away.

#[cfg(test)]
std::thread_local! {
    static RANGE_UNCHECKED: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Whether the test on this thread has removed the `Content-Range` check.
#[cfg(test)]
pub(crate) fn range_unchecked() -> bool {
    RANGE_UNCHECKED.with(core::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
pub(crate) const fn range_unchecked() -> bool {
    false
}

/// Runs `body` with the check removed (`planted`) or as it is, and puts it back after.
#[cfg(test)]
pub(crate) fn with<R>(planted: bool, body: impl FnOnce() -> R) -> R {
    let before = RANGE_UNCHECKED.with(|p| p.replace(planted));
    let _back = core::mem::DropGuard::new(before, |before| RANGE_UNCHECKED.with(|p| p.set(before)));
    body()
}

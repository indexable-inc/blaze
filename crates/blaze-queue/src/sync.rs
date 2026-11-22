//! The atomics the queues use: `core`'s, or under `cfg(blaze_check)` the model checker's
//! (docs/v2/plan/design/model-checker.md "The switch"); and the tracking of a slot's value,
//! which a model checks for races and for a read after the block holding it was freed.

#[cfg(not(blaze_check))]
pub(crate) use core::sync::atomic;

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::atomic;

/// A non-atomic write of the value at `at`: a model checks every earlier access of it happens
/// before. Nothing outside a model.
#[inline(always)]
pub(crate) fn wrote<T>(at: *const T) {
    #[cfg(blaze_check)]
    if size_of::<T>() != 0 {
        blaze_check::cell::write(at.cast(), size_of::<T>());
    }
    _ = at;
}

/// A non-atomic read of the value at `at`: a model checks its last write happens before.
#[inline(always)]
pub(crate) fn read<T>(at: *const T) {
    #[cfg(blaze_check)]
    if size_of::<T>() != 0 {
        blaze_check::cell::read(at.cast(), size_of::<T>());
    }
    _ = at;
}

/// The value at `at` is freed with its block: a model reports a later read of it.
#[inline(always)]
pub(crate) fn freed<T>(at: *const T) {
    #[cfg(blaze_check)]
    if size_of::<T>() != 0 {
        blaze_check::cell::free(at.cast(), size_of::<T>());
    }
    _ = at;
}

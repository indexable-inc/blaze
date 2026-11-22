//! How an infallible call stops when memory cannot be had, and the overflow refusal every
//! collection here names itself in: blaze-own's [`AllocFailure`], the one allocation failure of
//! the tree.

use core::alloc::Layout;

use blaze_own::AllocFailure;

/// Stops with `failure`: what an infallible allocation does when memory cannot be had, as
/// blaze-own's owners do. A panic fails the job that asked (rule 20).
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn fail(failure: AllocFailure) -> ! {
    panic!("blaze_kit: {failure}")
}

/// The refusal for `len + additional` elements of `what` that do not fit one allocation.
#[cold]
pub(crate) fn overflow(what: &'static str, len: usize, additional: usize) -> AllocFailure {
    AllocFailure::CapacityOverflow { what, len, additional }
}

/// The refusal for a layout the allocator would not give.
#[cold]
pub(crate) fn refused(what: &'static str, layout: Layout) -> AllocFailure {
    AllocFailure::Refused { what, layout }
}

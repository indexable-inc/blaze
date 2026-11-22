use core::{alloc::Layout, fmt};

/// Why memory for an owning type could not be had. Each names the type it was for, so the
/// message says what wanted the memory (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocFailure {
    /// The size asked for does not fit `isize::MAX` bytes: `len` elements and `additional`
    /// more of `what`.
    CapacityOverflow { what: &'static str, len: usize, additional: usize },
    /// The allocator refused `layout`.
    Refused { what: &'static str, layout: Layout },
}

impl AllocFailure {
    /// The type the memory was for.
    pub fn what(&self) -> &'static str {
        match *self {
            AllocFailure::CapacityOverflow { what, .. } | AllocFailure::Refused { what, .. } => {
                what
            }
        }
    }
}

impl fmt::Display for AllocFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllocFailure::CapacityOverflow { what, len, additional } => write!(
                f,
                "{len} + {additional} elements of `{what}` overflow the largest size an allocation may have (isize::MAX bytes)"
            ),
            AllocFailure::Refused { what, layout } => write!(
                f,
                "the allocator refused {} bytes aligned to {} for `{what}`",
                layout.size(),
                layout.align()
            ),
        }
    }
}

impl core::error::Error for AllocFailure {}

/// Stops with `failure`: what an infallible allocation does when memory cannot be had, as
/// `blaze_mem`'s allocators do. A panic fails the job that asked (rule 20).
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn fail(failure: AllocFailure) -> ! {
    panic!("blaze_own: {failure}")
}

/// Stops the process: a reference count about to pass `isize::MAX`. std aborts here too; a
/// panic would leave the count wrong for every other owner. `core` has no abort, so this
/// panics while a guard's drop panics again, which the runtime turns into an abort.
#[cold]
#[inline(never)]
pub(crate) fn abort_count_overflow(what: &'static str) -> ! {
    // A hand-written guard: it must panic in its drop, which `DropGuard`'s closure could do
    // too, but a type named for what it does reads as what it is.
    struct AbortOnUnwind(&'static str);
    impl Drop for AbortOnUnwind {
        fn drop(&mut self) {
            panic!("blaze_own: the reference count of a `{}` overflowed; aborting", self.0);
        }
    }
    let _abort = AbortOnUnwind(what);
    panic!("blaze_own: the reference count of a `{what}` passed isize::MAX")
}

/// The most owners a count holds before [`abort_count_overflow`]: std's bound, so the count can
/// never wrap even with every thread racing past it at once.
pub(crate) const MOST_COUNT: usize = isize::MAX as usize;

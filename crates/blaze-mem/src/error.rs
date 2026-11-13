use std::{fmt, io};

use crate::Tag;

/// Why memory could not be had. Each names the subsystem and the sizes involved: blaze stops
/// with it rather than taking memory from elsewhere (no-fallback).
#[derive(Debug)]
pub enum MemError {
    /// The operating system refused to reserve address space.
    Reserve { tag: Tag, bytes: usize, os: io::Error },
    /// The operating system refused to back reserved pages with memory.
    Commit { tag: Tag, bytes: usize, os: io::Error },
    /// The memory gate's core owner has no room for a core region's commit
    /// ([`crate::gate::Gate::core_reserve`]): plain numbers, made without allocating, as an
    /// allocator's commit hook may meet it; formatted only where it is printed.
    CoreFull { tag: Tag, bytes: usize, used: u64, core: u64 },
    /// A region's reservation is used up: the allocator would have to move to grow, which it
    /// never does.
    Exhausted { tag: Tag, reserved: usize, wanted: usize },
    /// A size or count that overflows the address space or a `u32` index.
    TooLarge { tag: Tag, what: &'static str, wanted: usize },
    /// The system cannot give a sparse region's pages back ([`crate::Region::evict`]): its
    /// kernel lacks the call, or the call did not drop them.
    Evict { tag: Tag, os: io::Error },
}

impl MemError {
    pub fn tag(&self) -> Tag {
        match *self {
            MemError::Reserve { tag, .. }
            | MemError::Commit { tag, .. }
            | MemError::CoreFull { tag, .. }
            | MemError::Exhausted { tag, .. }
            | MemError::TooLarge { tag, .. }
            | MemError::Evict { tag, .. } => tag,
        }
    }
}

impl fmt::Display for MemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemError::Reserve { tag, bytes, os } => {
                write!(f, "{tag}: reserving {bytes} bytes of address space failed: {os}")
            }
            MemError::Commit { tag, bytes, os } => {
                write!(f, "{tag}: committing {bytes} bytes of reserved memory failed: {os}")
            }
            MemError::CoreFull { tag, bytes, used, core } => write!(
                f,
                "{tag}: the memory gate's core owner holds {used} B of its {core} B, so a core \
                 region cannot commit {bytes} B more (rule one-memory-measure; install a larger \
                 core with blaze_mem::gate::install)"
            ),
            MemError::Exhausted { tag, reserved, wanted } => write!(
                f,
                "{tag}: its region of {reserved} reserved bytes is used up ({wanted} bytes wanted); \
                 regions never move, so reserve more for it"
            ),
            MemError::TooLarge { tag, what, wanted } => {
                write!(f, "{tag}: {what} of {wanted} is too large")
            }
            MemError::Evict { tag, os } => write!(
                f,
                "{tag}: this system cannot give the pages of a sparse region back, so none it \
                 loads would ever leave memory: {os}"
            ),
        }
    }
}

impl std::error::Error for MemError {
    // dyn: std's `Error::source`, on an error path.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MemError::Reserve { os, .. }
            | MemError::Commit { os, .. }
            | MemError::Evict { os, .. } => Some(os),
            MemError::CoreFull { .. } | MemError::Exhausted { .. } | MemError::TooLarge { .. } => {
                None
            }
        }
    }
}

/// Stops with `error`: what an infallible allocation does when memory cannot be had.
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn fail(error: &MemError) -> ! {
    panic!("{error}")
}

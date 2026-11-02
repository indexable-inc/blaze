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
    /// A region's reservation is used up: the allocator would have to move to grow, which it
    /// never does.
    Exhausted { tag: Tag, reserved: usize, wanted: usize },
    /// A size or count that overflows the address space or a `u32` index.
    TooLarge { tag: Tag, what: &'static str, wanted: usize },
}

impl MemError {
    pub fn tag(&self) -> Tag {
        match *self {
            MemError::Reserve { tag, .. }
            | MemError::Commit { tag, .. }
            | MemError::Exhausted { tag, .. }
            | MemError::TooLarge { tag, .. } => tag,
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
            MemError::Exhausted { tag, reserved, wanted } => write!(
                f,
                "{tag}: its region of {reserved} reserved bytes is used up ({wanted} bytes wanted); \
                 regions never move, so reserve more for it"
            ),
            MemError::TooLarge { tag, what, wanted } => {
                write!(f, "{tag}: {what} of {wanted} is too large")
            }
        }
    }
}

impl std::error::Error for MemError {
    // dyn: std's `Error::source`, on an error path.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MemError::Reserve { os, .. } | MemError::Commit { os, .. } => Some(os),
            MemError::Exhausted { .. } | MemError::TooLarge { .. } => None,
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

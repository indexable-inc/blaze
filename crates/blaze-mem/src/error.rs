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
    /// The system cannot give a sparse region's pages back ([`crate::Region::evict`]): its
    /// kernel lacks the call, or the call did not drop them.
    Evict { tag: Tag, os: io::Error },
}

impl MemError {
    pub fn tag(&self) -> Tag {
        match *self {
            MemError::Reserve { tag, .. }
            | MemError::Commit { tag, .. }
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
            MemError::Exhausted { .. } | MemError::TooLarge { .. } => None,
        }
    }
}

impl MemError {
    /// The error as the collections say it: the arena (the tag's name) and the bytes asked.
    /// [`MemError::Evict`] asked for none.
    pub fn out_of_memory(&self) -> OutOfMemory {
        let asked = match *self {
            MemError::Reserve { bytes, .. } | MemError::Commit { bytes, .. } => bytes,
            MemError::Exhausted { wanted, .. } | MemError::TooLarge { wanted, .. } => wanted,
            MemError::Evict { .. } => 0,
        };
        OutOfMemory { arena: self.tag().name(), asked }
    }
}

/// An allocator had no room: `arena` could not hand out `asked` more bytes. What every growth
/// of a collection in an allocator returns ([`crate::Vec`], [`crate::String`]), so running out
/// is an outcome its caller decides about and never `alloc`'s `handle_alloc_error`
/// (docs/v2/plan/design/fallible-bstd.md).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutOfMemory {
    /// The allocator's name ([`Named::name`]): a frame's, an arena's, a tag's.
    pub arena: &'static str,
    /// The bytes the growth asked for beyond what the collection held.
    pub asked: usize,
}

impl fmt::Display for OutOfMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "arena {}: {} more bytes would pass its limit", self.arena, self.asked)
    }
}

impl std::error::Error for OutOfMemory {}

/// An allocator that says whose memory it hands out: the name an [`OutOfMemory`] carries.
pub trait Named {
    fn name(&self) -> &'static str;
}

impl<A: Named + ?Sized> Named for &A {
    #[inline]
    fn name(&self) -> &'static str {
        (**self).name()
    }
}

/// The global heap, for the few collections bstd still keeps there (each with its
/// `// alloc:` line).
impl Named for std::alloc::Global {
    fn name(&self) -> &'static str {
        "the global heap"
    }
}

impl<A: Named + ?Sized> Named for std::rc::Rc<A> {
    #[inline]
    fn name(&self) -> &'static str {
        (**self).name()
    }
}

impl<A: Named + ?Sized> Named for std::sync::Arc<A> {
    #[inline]
    fn name(&self) -> &'static str {
        (**self).name()
    }
}

/// Stops with `error`: what an infallible allocation does when memory cannot be had.
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn fail(error: &MemError) -> ! {
    panic!("{error}")
}

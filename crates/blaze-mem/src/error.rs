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
    /// A range handed to a sparse region ([`crate::Region::reuse`], [`crate::Region::evict`])
    /// is not whole pages of `page` bytes inside its reservation of `reserved` bytes.
    Range { tag: Tag, doing: &'static str, from: usize, len: usize, reserved: usize, page: usize },
    /// A sparse region was counted both by its prefix ([`crate::Region::ensure`]) and by ranges
    /// ([`crate::Region::reuse`]): its owner counts one way.
    Mixed { tag: Tag, doing: &'static str, reserved: usize, committed: usize, held: usize },
    /// Pages evicted from a region that is not sparse: a committed prefix is given back with
    /// [`crate::Region::purge`] or [`crate::Region::decommit_after`].
    NotSparse { tag: Tag, reserved: usize },
    /// More bytes evicted than a sparse region holds: a range given back twice, or never
    /// reused.
    Overgiven { tag: Tag, asked: usize, held: usize, reserved: usize },
    /// Bytes `from..end` reused past the `committed` prefix of a region: only pages once
    /// committed are ever purged.
    PastCommitted { tag: Tag, from: usize, end: usize, committed: usize },
    /// A frame rolled back to a marker it cannot honor ([`crate::Frame::reset_to`]), by `rule`.
    Marker { tag: Tag, rule: &'static str },
    /// An operation the allocator's own rule refuses: `what` names it (a second `AppendVec`
    /// writer, a block of another column, a pooled value of another pool, a broken free list).
    Refused { tag: Tag, what: &'static str },
}

impl MemError {
    pub fn tag(&self) -> Tag {
        match *self {
            MemError::Reserve { tag, .. }
            | MemError::Commit { tag, .. }
            | MemError::Exhausted { tag, .. }
            | MemError::TooLarge { tag, .. }
            | MemError::Evict { tag, .. }
            | MemError::Range { tag, .. }
            | MemError::Mixed { tag, .. }
            | MemError::NotSparse { tag, .. }
            | MemError::Overgiven { tag, .. }
            | MemError::PastCommitted { tag, .. }
            | MemError::Marker { tag, .. }
            | MemError::Refused { tag, .. } => tag,
        }
    }

    /// The same error again, for a cause kept for every later caller (a size class or a chunk
    /// space that could not be made): `io::Error` is not `Clone`, so its OS error is made
    /// again from its code, or its kind when it has none, which allocates nothing.
    pub(crate) fn again(&self) -> MemError {
        fn os_again(os: &io::Error) -> io::Error {
            match os.raw_os_error() {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::from(os.kind()),
            }
        }
        match *self {
            MemError::Reserve { tag, bytes, ref os } => {
                MemError::Reserve { tag, bytes, os: os_again(os) }
            }
            MemError::Commit { tag, bytes, ref os } => {
                MemError::Commit { tag, bytes, os: os_again(os) }
            }
            MemError::Exhausted { tag, reserved, wanted } => {
                MemError::Exhausted { tag, reserved, wanted }
            }
            MemError::TooLarge { tag, what, wanted } => MemError::TooLarge { tag, what, wanted },
            MemError::Evict { tag, ref os } => MemError::Evict { tag, os: os_again(os) },
            MemError::Range { tag, doing, from, len, reserved, page } => {
                MemError::Range { tag, doing, from, len, reserved, page }
            }
            MemError::Mixed { tag, doing, reserved, committed, held } => {
                MemError::Mixed { tag, doing, reserved, committed, held }
            }
            MemError::NotSparse { tag, reserved } => MemError::NotSparse { tag, reserved },
            MemError::Overgiven { tag, asked, held, reserved } => {
                MemError::Overgiven { tag, asked, held, reserved }
            }
            MemError::PastCommitted { tag, from, end, committed } => {
                MemError::PastCommitted { tag, from, end, committed }
            }
            MemError::Marker { tag, rule } => MemError::Marker { tag, rule },
            MemError::Refused { tag, what } => MemError::Refused { tag, what },
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
            MemError::Range { tag, doing, from, len, reserved, page } => write!(
                f,
                "{doing} bytes {from} to {} of a sparse {tag} region of {reserved} reserved bytes: \
                 a range must be whole pages of {page} bytes inside the reservation",
                from.saturating_add(*len)
            ),
            MemError::Mixed { tag, doing, reserved, committed, held } => write!(
                f,
                "{doing} a sparse {tag} region of {reserved} reserved bytes counted both by its \
                 prefix ({committed} committed) and by ranges ({held} held): its owner counts one \
                 way"
            ),
            MemError::NotSparse { tag, reserved } => write!(
                f,
                "evicting pages of a {tag} region of {reserved} reserved bytes that is not sparse: \
                 give a committed prefix back with purge or decommit_after"
            ),
            MemError::Overgiven { tag, asked, held, reserved } => write!(
                f,
                "evicting {asked} bytes of a {tag} region of {reserved} reserved bytes that holds \
                 {held}: a range given back twice, or never reused"
            ),
            MemError::PastCommitted { tag, from, end, committed } => write!(
                f,
                "reusing bytes {from} to {end} of a {tag} region with {committed} committed: past \
                 what it committed"
            ),
            MemError::Marker { tag, rule } => write!(f, "{tag}: a frame's marker refused: {rule}"),
            MemError::Refused { tag, what } => write!(f, "{tag}: refused {what}"),
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
            MemError::Exhausted { .. }
            | MemError::TooLarge { .. }
            | MemError::Range { .. }
            | MemError::Mixed { .. }
            | MemError::NotSparse { .. }
            | MemError::Overgiven { .. }
            | MemError::PastCommitted { .. }
            | MemError::Marker { .. }
            | MemError::Refused { .. } => None,
        }
    }
}

impl MemError {
    /// The error as the collections say it: the arena (the tag's name) and the bytes asked.
    /// [`MemError::Evict`] and the refusals of a misuse (a range, a marker) asked for none.
    pub fn out_of_memory(&self) -> OutOfMemory {
        let asked = match *self {
            MemError::Reserve { bytes, .. } | MemError::Commit { bytes, .. } => bytes,
            MemError::Exhausted { wanted, .. } | MemError::TooLarge { wanted, .. } => wanted,
            MemError::Overgiven { asked, .. } => asked,
            MemError::Evict { .. }
            | MemError::Range { .. }
            | MemError::Mixed { .. }
            | MemError::NotSparse { .. }
            | MemError::PastCommitted { .. }
            | MemError::Marker { .. }
            | MemError::Refused { .. } => 0,
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

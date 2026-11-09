//! Memory is explicit (docs/v2/plan/design/std.md, "Memory is explicit"): what bstd hands back
//! is made in an allocator its caller handed in.
//!
//! | allocator | from | lives |
//! |---|---|---|
//! | [`Frame`] | `cx.frame()` | until the task ends |
//! | [`Arena`] | `mem.arena("name", limit)` | until dropped |
//!
//! Both are blaze-mem's [`Frame`], the bump allocator of blaze's tasks, which already keeps a
//! limit (`Frame::with_reserve`): an arena is a named frame whose reserve is its limit, so
//! nothing here is a new allocator and nothing needs `unsafe`. Running out is an outcome:
//! [`Bytes::try_extend`] returns [`Stopped::OutOfMemory`] naming the arena. A collection the
//! caller makes in an arena itself (`Vec::new_in(arena.alloc())`) must grow through its
//! `try_` forms (`try_reserve`): an infallible growth past the limit is `alloc`'s allocation
//! failure, until bstd's own collections (S4) turn it into the task's failure.

use blaze_mem::Tag;
pub use blaze_mem::Frame;

use crate::{Plant, Stopped, io::Io};

/// The most a frame not given a limit hands out: a task's ([`crate::task`]) and the root's.
pub const TASK_FRAME: usize = 64 << 20;

/// Bytes in an allocator its owner handed in: a frame's or an arena's.
pub struct Bytes<'a> {
    bytes: Vec<u8, &'a Frame>,
    arena: &'static str,
}

impl<'a> Bytes<'a> {
    /// Empty bytes in `frame`, which say they are `arena`'s when they run out of room.
    pub fn new_in(frame: &'a Frame, arena: &'static str) -> Bytes<'a> {
        Bytes { bytes: Vec::new_in(frame), arena }
    }

    /// `more` added at the end, or [`Stopped::OutOfMemory`] with the bytes as they were.
    pub fn try_extend(&mut self, more: &[u8]) -> Result<(), Stopped> {
        self.try_reserve(more.len())?;
        self.bytes.extend_from_slice(more);
        Ok(())
    }

    /// Room for `more` bytes past the end, or [`Stopped::OutOfMemory`].
    pub fn try_reserve(&mut self, more: usize) -> Result<(), Stopped> {
        self.bytes
            .try_reserve(more)
            .map_err(|_| Stopped::OutOfMemory { arena: self.arena, asked: more })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    /// The name its running out of room says.
    pub fn arena(&self) -> &'static str {
        self.arena
    }
}

impl std::fmt::Debug for Bytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({} of {})", self.bytes.len(), self.arena)
    }
}

/// Hands out named arenas, each with a limit: from [`Io::mem`].
#[derive(Clone)]
pub struct Mem {
    io: Io,
}

impl Mem {
    pub(crate) fn new(io: Io) -> Mem {
        Mem { io }
    }

    /// An arena named `name` that hands out at most `limit` bytes, alignment included.
    pub fn arena(&self, name: &'static str, limit: usize) -> Result<Arena, Stopped> {
        let reserve = match self.io.plant() {
            Plant::ArenaUnlimited => TASK_FRAME,
            _ => limit,
        };
        let frame = Frame::with_reserve(reserve, Tag::Other)
            .map_err(|_| Stopped::OutOfMemory { arena: name, asked: limit })?;
        Ok(Arena { name, limit, frame })
    }
}

/// Memory for a result that outlives its task: named, with a limit, freed when dropped.
pub struct Arena {
    name: &'static str,
    limit: usize,
    frame: Frame,
}

impl Arena {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes handed out so far, alignment included: never more than [`Arena::limit`].
    pub fn used(&self) -> usize {
        self.frame.used()
    }

    /// Empty bytes in the arena.
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::new_in(&self.frame, self.name)
    }

    /// The allocator, for a collection of the caller's (`Vec::new_in(arena.alloc())`), grown
    /// through its `try_` forms.
    pub fn alloc(&self) -> &Frame {
        &self.frame
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Arena({}: {} of {})", self.name, self.used(), self.limit)
    }
}

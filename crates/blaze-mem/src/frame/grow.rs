//! What growth leaves behind in a frame, counted, and which collection grows without it.
//!
//! A frame's LAST block grows and shrinks in place (`Frame::grow_top`: the cursor moves, a span
//! commits further): no new block, no copy, nothing left. The three exact points, each in
//! `Frame`'s `Allocator` impl:
//!
//! - **Alignment**: a block grows where it is only when its address already has the new
//!   layout's alignment (`align_offset(new.align()) == 0`); a bump address never changes, so
//!   one that lacks it moves.
//! - **The block's end**: `grow_top` extends the block to `end` only when `end` is within the
//!   bump's end: a chunk's end, a span's committed end (committed further first, up to the
//!   span's reservation), the frame's limit. Past it the block moves to a block of its own (a
//!   span over half a chunk, which then grows in place up to its reservation).
//! - **Zeroing**: `grow_zeroed` writes zeros over `[old.size(), new.size())` on either path:
//!   bytes reused after a reset or a rollback hold what was there before.
//!
//! A block that moves leaves its old bytes taken until the frame is reset or rolled back past
//! them: a bump allocator cannot give them back. That is what filled the command's frame
//! ("effect publish (the settled table) 45776916 114429x": a table that was never the last
//! block when it grew). They are COUNTED per frame ([`Abandoned`], [`Frame::abandoned`]), so a
//! profile says whether any remain: `abandoned by growth: N bytes in M growths`.
//!
//! Which collection for which growth (nothing admits or throttles by memory; these only decide
//! what growth costs):
//!
//! | The collection | Use | Growth costs |
//! |---|---|---|
//! | grows while nothing else is allocated on its frame | `blaze_own::Vec<T, &Frame>` | in place: nothing (one move, at most half a chunk, where it leaves its first chunk for a span) |
//! | grows while other allocations interleave; read by index, never as one slice | `blaze_own::Segs<T, &Frame>` | a new segment: never a copy, never a hole |
//! | grows far while others interleave and is read as ONE slice | [`super::table::FrameTable`] ([`Frame::table`]) | its own reservation, pages committed as it grows; `try_push` refuses with a typed [`super::table::TableRefused`] |
//! | lives past any one frame | [`crate::RegionVec`] | as the table, owned by its holder |
//! | a `blaze_own::Vec<T, &Frame>` under interleaved allocations | avoid | every doubling abandons the old block: this file counts it |

use core::{cell::Cell, fmt};

use super::Frame;

#[cfg(all(test, not(blaze_check)))]
mod tests;

/// What growth left behind in a frame since its last reset: the bytes of the blocks `grow`
/// (or a re-aligning `shrink`) moved. They are still counted in `Frame::used` and come back
/// only at a reset or a rollback past them. Zero for every collection that only ever grew in
/// place.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Abandoned {
    /// Growths (and re-aligning shrinks) that left their old block behind.
    pub growths: u64,
    /// Bytes of those old blocks.
    pub bytes: u64,
}

impl Abandoned {
    /// The sum of two counts.
    #[must_use]
    pub const fn plus(self, other: Abandoned) -> Abandoned {
        Abandoned { growths: self.growths + other.growths, bytes: self.bytes + other.bytes }
    }
}

/// The profile's line: `abandoned by growth: 45776916 bytes in 17 growths`.
impl fmt::Display for Abandoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "abandoned by growth: {} bytes in {} growths", self.bytes, self.growths)
    }
}

thread_local! {
    /// What the frames reset or ended on this thread abandoned, never decreasing.
    static ON_THREAD: Cell<Abandoned> = const { Cell::new(Abandoned { growths: 0, bytes: 0 }) };
}

/// What the frames reset on the calling thread, and the frames of its ended tasks, abandoned
/// by growth so far, never decreasing: a span's part is the difference across it. A live
/// frame's own count is [`Frame::abandoned`].
pub fn abandoned_on_thread() -> Abandoned {
    ON_THREAD.try_with(Cell::get).unwrap_or_default()
}

/// A frame was reset, or its task ended, having abandoned `abandoned`.
pub(super) fn report(abandoned: Abandoned) {
    if abandoned.growths != 0 {
        // A thread being torn down has no sum left to add to.
        _ = ON_THREAD.try_with(|it| it.set(it.get().plus(abandoned)));
    }
}

#[cfg(test)]
thread_local! {
    /// The planted bug of this file's control: growth in place that ignores the block's end.
    static IGNORES_END: Cell<bool> = const { Cell::new(false) };
}

/// Whether the planted bug is on for this thread: `Frame::grow_top` then extends the last
/// block past its chunk's end (or its span's committed end) instead of refusing. Always false
/// in a build that is not a test.
#[inline]
pub(super) fn ignores_end() -> bool {
    #[cfg(test)]
    {
        IGNORES_END.with(Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Turns the planted bug on or off for the calling thread; what it was.
#[cfg(test)]
pub(crate) fn plant_ignores_end(on: bool) -> bool {
    IGNORES_END.with(|it| it.replace(on))
}

impl Frame {
    /// What growth abandoned in this frame since its last reset ([`Abandoned`]).
    pub fn abandoned(&self) -> Abandoned {
        Abandoned { growths: self.abandons.get(), bytes: self.abandoned.get() }
    }

    /// A block of `bytes` bytes was moved by `grow` or `shrink` and its old bytes stay taken.
    #[inline]
    pub(super) fn left_behind(&self, bytes: usize) {
        if bytes != 0 {
            self.abandons.set(self.abandons.get() + 1);
            self.abandoned.set(self.abandoned.get() + bytes as u64);
        }
    }

    /// Whether `[ptr, ptr + size)` lies whole inside ONE of this frame's blocks, below the
    /// block's end (a chunk's end, a span's committed end) or inside the window an outer frame
    /// lent: what a test's oracle asks before it writes a block's bytes, so a block handed out
    /// past its chunk's end is a finding and never a wild write.
    #[cfg(test)]
    pub(crate) fn holds(&self, ptr: core::ptr::NonNull<u8>, size: usize) -> bool {
        use super::{data, header};

        let at = ptr.addr().get();
        let Some(end) = at.checked_add(size) else { return false };
        if self.window.0 <= at && end <= self.window.1 {
            return true;
        }
        let mut block = self.first.get();
        while !block.is_null() {
            let head = header(block);
            let stop = match head.span.as_deref() {
                Some(region) => region.base().addr().get() + region.committed(),
                None => head.end,
            };
            if data(block).addr() <= at && end <= stop {
                return true;
            }
            block = head.next;
        }
        false
    }
}

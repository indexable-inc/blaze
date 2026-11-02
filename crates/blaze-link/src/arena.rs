//! A stage's scratch (docs/v2/plan/design/memory.md, "a task"): a [`Frame`] the image keeps
//! and resets when the next stage begins, so a stage like the last one finds its pages
//! committed and makes no call to the general-purpose allocator for its temporaries.

#![forbid(unsafe_code)]

use blaze_mem::{Frame, MemError, Tag};

/// The address space a stage's scratch may take: the copies of the pieces it rewrites (at
/// most the image's bytes), their fixup lists and the per-piece sets, for an image of up to
/// 16 GiB. Reserved, not committed: pages are committed as a stage first needs them.
pub(crate) const STAGE_RESERVE: usize = 64 << 30;

/// A vector in a stage's scratch, gone when the next stage begins.
pub(crate) type FrameVec<'f, T> = Vec<T, &'f Frame>;

/// The image's stage scratch: once per image.
pub(crate) fn stage_frame() -> Result<Frame, MemError> {
    Frame::with_reserve(STAGE_RESERVE, Tag::Link)
}

/// `items` collected into the frame: [`blaze_mem::frame::collect`], the frame last.
#[inline]
pub(crate) fn collect_in<'f, T>(items: impl IntoIterator<Item = T>, frame: &'f Frame) -> FrameVec<'f, T> {
    blaze_mem::frame::collect(frame, items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A warm frame's stage makes no global allocation, and a reset gives its room to the next.
    #[test]
    fn a_warm_stage_allocates_nothing() {
        let mut frame = stage_frame().unwrap();
        let stage = |frame: &Frame| {
            let mut ids: FrameVec<'_, u32> = Vec::new_in(frame);
            ids.extend(0..100_000);
            let doubled = collect_in(ids.iter().map(|it| u64::from(*it) * 2), frame);
            let bytes = frame.alloc_slice_fill_with(4096, |_| 0xccu8);
            let kept: &[u32] = collect_in(ids.iter().copied().filter(|it| it % 3 == 0), frame).leak();
            (doubled[99_999], bytes[4095], kept.len())
        };
        assert_eq!(stage(&frame), (199_998, 0xcc, 33_334));
        let used = frame.used();
        frame.reset();
        let again = blaze_alloc_count::assert_at_most(0, "a stage on a warm frame", || stage(&frame));
        assert_eq!(again, (199_998, 0xcc, 33_334));
        assert_eq!(frame.used(), used, "the same stage takes the same room");
    }
}

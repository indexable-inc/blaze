//! The run's frame: every temporary of a well-formedness check is in it, and goes in one store
//! when the task that runs the check ends. What a check returns to the database is owned instead.
//! The borrow checker's frame collections are these.

pub use blaze_mem::frame::{Frame, collect};
pub use hir_ty::{FrameMap as FMap, FrameSet as FSet, frame_map as map, frame_set as set};

pub type FVec<'f, T> = Vec<T, &'f Frame>;

/// The items of `iter`, which says its length, as an owned vector of exactly that length: a
/// memoized result built from frame values.
#[inline]
pub fn owned<T>(iter: impl ExactSizeIterator<Item = T>) -> Vec<T> {
    let mut out = Vec::with_capacity(iter.len()); // alloc: the memoized result, owned
    out.extend(iter);
    out
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};

    #[test]
    fn frame_collections_hold_what_global_ones_do() {
        frame::task(Tag::Infer, |frame| {
            let mut map = super::map(frame);
            let mut set = super::set(frame);
            for it in 0..1000u32 {
                map.insert(it, it * 2);
                set.insert(it % 37);
            }
            assert_eq!(map.len(), 1000);
            assert_eq!(map[&999], 1998);
            assert_eq!(set.len(), 37);
            let framed = super::collect(frame, std::iter::repeat_n(7u64, 70));
            assert_eq!((framed.len(), framed.capacity()), (70, 70));
            let owned = super::owned(framed.iter().copied());
            assert_eq!((owned.len(), owned.capacity()), (70, 70));
            assert!(frame.contains(framed.as_ptr().cast()));
            assert!(!frame.contains(owned.as_ptr().cast()));
        });
    }
}

//! The run's frame: every temporary of a borrow check is in it, and goes in one store when the
//! task that runs the check ends. What a check returns to the database is owned instead.

pub(crate) use hir_ty_wfcheck::mem::{FMap, FSet, FVec, Frame, collect, map, owned, set};

pub(crate) type FDeque<'f, T> = std::collections::VecDeque<T, &'f Frame>;

/// `len` copies of `value` in one allocation of exactly `len`.
#[inline]
pub(crate) fn filled<'f, T: Clone>(frame: &'f Frame, len: usize, value: T) -> FVec<'f, T> {
    let mut out = Vec::with_capacity_in(len, frame);
    out.resize(len, value);
    out
}

/// `iter`, of `len` items, as an iterator that says so.
pub(crate) struct ExactLen<I> {
    iter: I,
    len: usize,
}

impl<I> ExactLen<I> {
    pub(crate) fn new(iter: I, len: usize) -> Self {
        ExactLen { iter, len }
    }
}

impl<I: Iterator> Iterator for ExactLen<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        let it = self.iter.next()?;
        self.len -= 1;
        Some(it)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<I: Iterator> ExactSizeIterator for ExactLen<I> {}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};

    #[test]
    fn frame_collections_hold_what_global_ones_do() {
        frame::task(Tag::Mir, |frame| {
            let mut map = super::map(frame);
            let mut set = super::set(frame);
            for it in 0..1000u32 {
                map.insert(it, it * 2);
                set.insert(it % 37);
            }
            assert_eq!(map.len(), 1000);
            assert_eq!(map[&999], 1998);
            assert_eq!(set.len(), 37);
            let filled = super::filled(frame, 70, 7u64);
            assert_eq!((filled.len(), filled.capacity()), (70, 70));
            let owned = super::owned(filled.iter().copied());
            assert_eq!((owned.len(), owned.capacity()), (70, 70));
            assert!(frame.contains(filled.as_ptr().cast()));
            assert!(!frame.contains(owned.as_ptr().cast()));
        });
    }
}

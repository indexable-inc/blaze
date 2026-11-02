//! [`Col`]: a column of a finished store, a slice that either owns its elements alone, as a
//! `Box<[T]>` does, shares one allocation with the store's other columns of `Copy` elements
//! (a slab, written by [`SlabWriter`]), or reads its `Copy` elements where a dependency image
//! holds them ([`Col::borrowed`]).
//!
//! A store's columns are written once, at `finish` or decode, and only read after. Giving each
//! its own box costs an allocation per non-empty column (8 to 20 a body, more than the copy for
//! a body of a few dozen nodes); a slab makes the `Copy` columns (kinds, data words, `extra`, id
//! lists, statement rows) one allocation together. A read is the same as a box's: a pointer and
//! a length, loaded from the column. A borrowed column saves the copy of its elements out of a
//! dependency image that a box would need, once per column per crate read.

use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::MaybeUninit,
    ops::Deref,
    ptr::{self, NonNull},
    slice,
    sync::Arc,
};

use crate::deps::{Held, ImageBuf};

/// The unit a slab is allocated in. Its alignment bounds the alignment of every element type a
/// slab holds ([`SlabWriter::take`] checks it at compile time).
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

const SLAB_ALIGN: usize = align_of::<Block>();

type SlabBytes = Arc<[MaybeUninit<Block>]>;

/// What keeps the elements of a column that does not own them alone.
#[derive(Clone)]
enum Owner {
    /// The slab [`SlabWriter`] wrote them into.
    Slab(SlabBytes),
    /// The image buffer they lie in ([`Col::borrowed`]).
    Image(ImageBuf),
}

/// A column of a finished store.
///
/// Invariant, which every `unsafe` block here relies on: `ptr` is aligned and valid for reads
/// of `len` initialized `T`s for as long as this `Col` lives, and nothing writes them. Either
/// - `owner` is `None` and they are the elements of a `Box<[T]>` this column owns, made by
///   [`Col::boxed`] and freed by `drop`;
/// - or `owner` holds a count of the slab they are in, `T` is `Copy` (so there is nothing to
///   drop), and [`SlabWriter::take`] wrote them before it made this column and writes no byte
///   of them after;
/// - or `owner` holds a count of the image they are in, `T` is `Copy`, and they lie inside its
///   `bytes()`, which neither move nor change while a count lives ([`Held`]). `Held` is a safe
///   trait and needs no more: a borrow of what `bytes()` returns may last as long as any
///   `&self`, so no sound impl frees or writes those bytes while the image is shared, and the
///   count keeps it alive and shared.
pub(crate) struct Col<T> {
    ptr: NonNull<T>,
    len: usize,
    owner: Option<Owner>,
    of: PhantomData<T>,
}

// Both owners are counts of unsized values (a pointer and a length or a vtable), so the owner
// takes a tag word besides: 32 bytes would need a slab that is itself a `dyn Held`, which is a
// second allocation a store.
const _: () = assert!(size_of::<Col<u32>>() == 40);

// SAFETY: a `Col` is a `Box<[T]>` or shares `T`s with other `Col`s as an `Arc<[T]>` does; both
// are `Send` and `Sync` when `T` is `Send + Sync`. An image owner is `Send + Sync` ([`Held`]).
unsafe impl<T: Send + Sync> Send for Col<T> {}
// SAFETY: as for `Send`.
unsafe impl<T: Send + Sync> Sync for Col<T> {}

impl<T> Col<T> {
    /// The column of `elements`, owning them alone.
    pub(crate) fn boxed(elements: Box<[T]>) -> Self {
        let len = elements.len();
        let ptr = NonNull::from(Box::leak(elements)).cast::<T>();
        Col { ptr, len, owner: None, of: PhantomData }
    }

    /// Whether this column shares its allocation with others (for the allocation tests).
    #[cfg(test)]
    pub(crate) fn in_slab(&self) -> bool {
        matches!(self.owner, Some(Owner::Slab(_)))
    }
}

impl<T: Copy> Col<T> {
    /// The column of `elements` where they are, inside `owner`'s bytes, holding a count of it;
    /// `None` unless every byte of `elements` is inside `owner.bytes()`. An empty column holds
    /// nothing.
    pub(crate) fn borrowed(owner: &ImageBuf, elements: &[T]) -> Option<Col<T>> {
        if elements.is_empty() {
            return Some(Col::default());
        }
        let image = owner.bytes().as_ptr_range();
        let column = elements.as_ptr_range();
        let inside = image.start as usize <= column.start as usize
            && column.end as usize <= image.end as usize;
        inside.then(|| Col {
            ptr: NonNull::from(elements).cast::<T>(),
            len: elements.len(),
            // clone: a count of the image, one a column
            owner: Some(Owner::Image(owner.clone())),
            of: PhantomData,
        })
    }

    /// Whether this column reads its elements in an image (for the decode tests).
    #[cfg(test)]
    pub(crate) fn in_image(&self) -> bool {
        matches!(self.owner, Some(Owner::Image(_)))
    }
}

impl<T> Drop for Col<T> {
    fn drop(&mut self) {
        if self.owner.is_some() {
            return;
        }
        // SAFETY: with no owner, `ptr` and `len` are those of the box `Col::boxed` leaked
        // (the invariant on `Col`), and this is the only place that frees it.
        drop(unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(self.ptr.as_ptr(), self.len)) });
    }
}

impl<T> Deref for Col<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: the invariant on `Col`: `len` initialized, aligned `T`s at `ptr`, alive and
        // unwritten while `self` is borrowed.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

/// For tests that corrupt a finished store's columns: a column in a slab is first copied into a
/// box of its own, so its slab's bytes stay unwritten (the invariant on [`Col`]). A store's
/// code never writes a finished column, so this exists only for tests.
#[cfg(test)]
impl<T: Clone> std::ops::DerefMut for Col<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        if self.owner.is_some() {
            *self = Col::boxed(Box::from(&**self));
        }
        // SAFETY: with no owner, this column owns its box alone (the invariant on `Col`), and
        // `&mut self` is the only borrow of it.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> Default for Col<T> {
    fn default() -> Self {
        Col::boxed(Box::default())
    }
}

impl<T: Clone> Clone for Col<T> {
    fn clone(&self) -> Self {
        match &self.owner {
            // alloc: a store's columns are copied only where a whole store is
            None => Col::boxed(Box::from(&**self)),
            // A slab's or an image's elements are never written again, so another count shares
            // them.
            Some(owner) => {
                // clone: a count, not the elements
                let owner = Some(owner.clone());
                Col { ptr: self.ptr, len: self.len, owner, of: PhantomData }
            }
        }
    }
}

impl<T: PartialEq> PartialEq for Col<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for Col<T> {}

impl<T: Hash> Hash for Col<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: fmt::Debug> fmt::Debug for Col<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> From<Box<[T]>> for Col<T> {
    fn from(elements: Box<[T]>) -> Self {
        Col::boxed(elements)
    }
}

impl<T> From<Vec<T>> for Col<T> {
    fn from(elements: Vec<T>) -> Self {
        Col::boxed(elements.into_boxed_slice())
    }
}

impl<T: Clone> From<&[T]> for Col<T> {
    fn from(elements: &[T]) -> Self {
        Col::boxed(Box::from(elements))
    }
}

impl<T> FromIterator<T> for Col<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        // alloc: the column's own box, as a boxed slice collects
        Col::boxed(iter.into_iter().collect())
    }
}

/// The bytes a slab needs for its columns, added in the order [`SlabWriter::take`] writes them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlabLayout {
    bytes: usize,
}

impl SlabLayout {
    /// Room for a column of `len` `T`s after the ones added so far. An empty column takes none.
    #[inline]
    pub(crate) fn add<T: Copy>(&mut self, len: usize) {
        if len != 0 {
            self.bytes = self.bytes.next_multiple_of(align_of::<T>()) + len * size_of::<T>();
        }
    }

    pub(crate) fn bytes(self) -> usize {
        self.bytes
    }
}

/// Writes the `Copy` columns of one store into one allocation, as [`Col`]s that share it.
///
/// Every column is written once, at the first unwritten offset aligned for it, and the [`Col`]
/// for it is made after its bytes are written and before any other column's are: so no `Col`
/// ever sees a byte written, and the columns are disjoint.
pub(crate) struct SlabWriter {
    slab: Option<SlabBytes>,
    /// The slab's first byte, taken while the slab was unique, which every write goes through.
    base: *mut u8,
    room: usize,
    at: usize,
}

impl SlabWriter {
    /// A writer of a slab of `layout`'s bytes; no allocation when it has none.
    pub(crate) fn new(layout: SlabLayout) -> Self {
        let blocks = layout.bytes.div_ceil(SLAB_ALIGN);
        if blocks == 0 {
            return SlabWriter { slab: None, base: ptr::null_mut(), room: 0, at: 0 };
        }
        // alloc: the one allocation of a store's `Copy` columns
        let mut slab: SlabBytes = Arc::new_uninit_slice(blocks);
        let base = Arc::get_mut(&mut slab).expect("a new slab is unique").as_mut_ptr().cast::<u8>();
        SlabWriter { slab: Some(slab), base, room: blocks * SLAB_ALIGN, at: 0 }
    }

    /// The column of `column`'s elements, copied into the slab; `column` is left empty with its
    /// capacity. Panics past the slab's room, which a layout added in a different order or with
    /// other lengths can reach.
    pub(crate) fn take<T: Copy>(&mut self, column: &mut Vec<T>) -> Col<T> {
        let col = self.copy(column);
        column.clear();
        col
    }

    /// The column of a copy of `elements` in the slab, as [`SlabWriter::take`].
    pub(crate) fn copy<T: Copy>(&mut self, elements: &[T]) -> Col<T> {
        const { assert!(align_of::<T>() <= SLAB_ALIGN) };
        if elements.is_empty() {
            return Col::default();
        }
        let at = self.at.next_multiple_of(align_of::<T>());
        let bytes = size_of_val(elements);
        assert!(at.checked_add(bytes).is_some_and(|end| end <= self.room), "a column past its slab");
        let slab = self.slab.as_ref().expect("a slab with room has an allocation");
        // SAFETY: `at + bytes <= room` (asserted), and `base` is the start of the slab's `room`
        // bytes (`new`), so `base + at` stays inside that allocation. Out of it, the offset is UB.
        let dst = unsafe { self.base.add(at) }.cast::<T>();
        // SAFETY: `dst..dst + bytes` is inside the slab (above), whose `base` was taken while the
        // slab was unique (`new`). No `Col` reads these bytes: every column made so far is below
        // `self.at <= at`, and this one is made only below. `base` is `SLAB_ALIGN`-aligned and
        // `at` a multiple of `align_of::<T>()`, which divides `SLAB_ALIGN` (the const assert), so
        // `dst` is aligned for `T`. `elements` is not in the slab (nothing hands out a slab's
        // bytes until its column is made, and then only to read), so the two don't overlap. A
        // column read while written, or overlapping its source, would read torn elements.
        unsafe { ptr::copy_nonoverlapping(elements.as_ptr(), dst, elements.len()) };
        self.at = at + bytes;
        Col {
            ptr: NonNull::new(dst).expect("inside an allocation"),
            len: elements.len(),
            // clone: a count of the slab, one a column
            owner: Some(Owner::Slab(slab.clone())),
            of: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(align(8))]
    struct Wide(u64, u8);

    /// Columns of three alignments written into one slab, in a seeded order and with seeded
    /// lengths (some empty), read back the same as boxed copies, and still the same after the
    /// writer, the sources and every other column are gone.
    #[test]
    fn a_slab_column_reads_as_its_box() {
        for seed in 0..64 {
            let mut rng = SplitMix64::new(seed);
            let mut bytes: Vec<u8> = (0..rng.below(40)).map(|it| it as u8).collect();
            let mut words: Vec<u32> = (0..rng.below(40)).map(|it| (it as u32) << 7).collect();
            let mut wides: Vec<Wide> = (0..rng.below(10)).map(|it| Wide(it, it as u8)).collect();
            let mut more: Vec<u8> = (0..rng.below(5)).map(|it| it as u8 ^ 0x5a).collect();
            let want = (bytes.clone(), words.clone(), wides.clone(), more.clone());

            let mut layout = SlabLayout::default();
            layout.add::<u8>(bytes.len());
            layout.add::<u32>(words.len());
            layout.add::<Wide>(wides.len());
            layout.add::<u8>(more.len());
            let mut writer = SlabWriter::new(layout);
            let (bytes_room, words_room) = (bytes.capacity(), words.capacity());
            let cols = (
                writer.take(&mut bytes),
                writer.take(&mut words),
                writer.take(&mut wides),
                writer.take(&mut more),
            );
            drop(writer);
            assert!(bytes.is_empty() && bytes.capacity() == bytes_room, "the source keeps its room");
            assert!(words.is_empty() && words.capacity() == words_room);
            drop((bytes, words, wides, more));

            assert_eq!(&*cols.0, &want.0[..]);
            assert_eq!(&*cols.1, &want.1[..]);
            assert_eq!(&*cols.2, &want.2[..]);
            assert_eq!(&*cols.3, &want.3[..]);
            assert_eq!(cols.2.as_ptr() as usize % align_of::<Wide>(), 0, "aligned for its type");
            assert_eq!(cols.1.as_ptr() as usize % align_of::<u32>(), 0);
            assert_eq!(cols.0.in_slab(), !want.0.is_empty(), "an empty column takes no slab");

            let boxed = Col::from(want.1.clone());
            assert_eq!(boxed, cols.1, "a boxed and a slab column of the same elements are equal");
            let (copy, (first, second, wide, last)) = (cols.1.clone(), cols);
            drop((first, wide, last));
            assert_eq!(&*copy, &want.1[..], "a clone outlives the other columns of its slab");
            drop(second);
            assert_eq!(&*copy, &want.1[..]);
        }
    }

    #[test]
    fn a_slab_has_the_room_of_its_layout_and_no_allocation_when_empty() {
        let mut layout = SlabLayout::default();
        layout.add::<u8>(0);
        layout.add::<u32>(0);
        assert_eq!(layout.bytes(), 0);
        let mut writer = SlabWriter::new(layout);
        assert!(writer.slab.is_none());
        assert!(writer.take(&mut Vec::<u32>::new()).is_empty());

        let mut layout = SlabLayout::default();
        layout.add::<u8>(3);
        layout.add::<u32>(2);
        assert_eq!(layout.bytes(), 4 + 8, "the words start at the next multiple of 4");
    }

    #[test]
    #[should_panic(expected = "a column past its slab")]
    fn a_column_past_its_layout_is_refused() {
        let mut layout = SlabLayout::default();
        layout.add::<u32>(4);
        let mut writer = SlabWriter::new(layout);
        writer.copy(&[0u32; 5]);
    }

    /// A column borrowed from an image reads its elements where they are, its clones too, after
    /// every other count of the image is gone; one not wholly inside the image is refused.
    #[test]
    fn a_borrowed_column_reads_in_its_image() {
        let words: Vec<u8> = (0..64u32).flat_map(|it| (it << 3).to_ne_bytes()).collect();
        let image: ImageBuf = Arc::new(blaze_format::Aligned::copy_from(&words));
        let (start, len) = (image.bytes().as_ptr(), image.bytes().len());
        assert_eq!(start as usize % align_of::<u32>(), 0, "an image's bytes are aligned");
        // SAFETY: `start` is aligned for `u32` (asserted), the image's bytes hold `len / 4` whole
        // `u32`s, every bit pattern of which is one, and `image` outlives `all`.
        let all: &[u32] = unsafe { slice::from_raw_parts(start.cast::<u32>(), len / 4) };
        let col = Col::borrowed(&image, &all[3..9]).expect("inside the image");
        assert!(col.in_image() && !col.in_slab());
        assert_eq!(col.as_ptr(), all[3..].as_ptr(), "read in place");
        assert_eq!(&*col, &(3..9).map(|it| it << 3).collect::<Vec<u32>>()[..]);

        let outside = [1u32, 2];
        assert!(Col::borrowed(&image, &outside).is_none(), "a column outside its image");
        assert!(!Col::borrowed(&image, &all[..0]).expect("empty").in_image(), "empty holds nothing");

        let copy = col.clone();
        drop((col, image));
        assert_eq!(&*copy, &(3..9).map(|it| it << 3).collect::<Vec<u32>>()[..]);
    }

    /// Boxed columns of an element with a destructor: dropped once each, cloned deeply.
    #[test]
    fn a_boxed_column_owns_its_elements() {
        let col: Col<String> = vec!["a".to_owned(), "b".to_owned()].into();
        let copy = col.clone();
        drop(col);
        assert_eq!(&*copy, ["a", "b"]);
        let collected: Col<String> = copy.iter().cloned().collect();
        assert_eq!(collected, copy);
        assert!(Col::<String>::default().is_empty());
    }
}

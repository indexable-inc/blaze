//! An append-only vector of doubling segments: in place of one `Vec` grown to a gigabyte, which
//! copies every value at each doubling and holds the old and the new buffer at once at the
//! last one (docs/v2/plan/design/std.md, "What std makes easy to get slow").
//!
//! Segment `s` holds `FIRST << s` values; a segment is allocated when the first value lands in
//! it and never moves, so nothing is copied on growth, the peak is the values plus the unfilled
//! part of the newest segment, and a `&T` stays valid while the vector lives. An index is found
//! by a leading-zeros count and a shift: for `j = i + FIRST`, the segment is
//! `log2(j) - log2(FIRST)` and the offset `j - (FIRST << segment)`.
//!
//! [`Segs`] is the owned form, appended through `&mut self`. [`SharedSegs`] is the shared one:
//! one [`Appender`] at a time pushes, and any thread reads below the published length
//! ([`SharedSegs::get`]), with no lock: the appender writes the value (and, at a segment's
//! first value, the segment's pointer) and then publishes the length with a release store; a
//! reader's acquire load of the length makes everything below it visible. Its interleavings
//! are checked under `cfg(blaze_check)` (the `model` tests, with a planted bug: a relaxed
//! publish, which the model catches).

use core::{
    alloc::{Allocator, Layout},
    fmt,
    marker::PhantomData,
    ops::{Index, IndexMut},
    ptr::{self, NonNull},
};

use crate::{
    error::{AllocFailure, fail},
    vec::{Refused, out_of_bounds},
};

/// The atomics of [`SharedSegs`]: `core`'s, or the model checker's in a model build.
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

/// `log2` of the first segment's length.
const FIRST_SHIFT: u32 = 3;
/// The first segment's length: 8 values.
const FIRST: usize = 1 << FIRST_SHIFT;
/// Segments enough for every index below `usize::MAX - FIRST`.
const SEGMENTS: usize = (usize::BITS - FIRST_SHIFT) as usize;

/// The segment and the offset in it of index `i`: a leading-zeros count and a shift.
#[inline(always)]
const fn locate(i: usize) -> (usize, usize) {
    let j = i + FIRST;
    let top = usize::BITS - 1 - j.leading_zeros();
    let segment = (top - FIRST_SHIFT) as usize;
    (segment, j - (1 << top))
}

/// The number of values segment `s` holds.
#[inline(always)]
const fn segment_len(s: usize) -> usize {
    FIRST << s
}

/// The layout of segment `s` of `T`, or the refusal naming the length.
fn segment_layout<T>(s: usize, len: usize) -> Result<Layout, AllocFailure> {
    Layout::array::<T>(segment_len(s)).map_err(|_| AllocFailure::CapacityOverflow {
        what: core::any::type_name::<T>(),
        len,
        additional: segment_len(s),
    })
}

/// Allocates segment `s`; for a zero-sized `T`, no memory, a dangling pointer.
fn allocate_segment<T, A: Allocator>(alloc: &A, s: usize, len: usize) -> Result<NonNull<T>, AllocFailure> {
    if size_of::<T>() == 0 {
        return Ok(NonNull::dangling());
    }
    let layout = segment_layout::<T>(s, len)?;
    match alloc.allocate(layout) {
        Ok(block) => Ok(block.cast()),
        Err(_) => Err(AllocFailure::Refused { what: core::any::type_name::<T>(), layout }),
    }
}

/// Drops the `len` values of the segments in `segs` and frees each segment.
///
/// # Safety
///
/// The first `len` slots across the segments hold values, every segment up to the one of slot
/// `len - 1` is allocated by `alloc` (and any past it either allocated or null), and nothing
/// uses them after.
unsafe fn drop_and_free<T, A: Allocator>(segs: impl Iterator<Item = *mut T>, len: usize, alloc: &A) {
    let mut left = len;
    for (s, seg) in segs.enumerate() {
        if seg.is_null() {
            break;
        }
        let here = left.min(segment_len(s));
        left -= here;
        // SAFETY: the first `here` slots of this segment hold values, dropped once.
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(seg, here)) };
        if size_of::<T>() != 0 {
            // `segment_layout` succeeded when the segment was made.
            let Ok(layout) = segment_layout::<T>(s, 0) else { unreachable!("the segment was made with this layout") };
            // SAFETY: allocated by `alloc` with this layout, freed once.
            unsafe { alloc.deallocate(NonNull::new_unchecked(seg.cast::<u8>()), layout) };
        }
    }
}

/// An append-only vector of doubling segments ([module](self)): a push never moves a value, an index is two shifts and one load.
pub struct Segs<T, A: Allocator> {
    /// Segment `s`'s first slot, or null before a value lands in it.
    segs: [*mut T; SEGMENTS],
    len: usize,
    alloc: A,
    owns: PhantomData<T>,
}

// SAFETY: a `Segs` owns its values as a `Vec` does.
unsafe impl<T: Send, A: Allocator + Send> Send for Segs<T, A> {}
// SAFETY: `&Segs` gives only `&T`.
unsafe impl<T: Sync, A: Allocator + Sync> Sync for Segs<T, A> {}

impl<T, A: Allocator> Segs<T, A> {
    pub const fn new_in(alloc: A) -> Segs<T, A> {
        Segs { segs: [ptr::null_mut(); SEGMENTS], len: 0, alloc, owns: PhantomData }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Appends `value`; its index. Nothing already in moves.
    #[track_caller]
    #[inline]
    pub fn push(&mut self, value: T) -> usize {
        match self.try_push(value) {
            Ok(i) => i,
            Err(refused) => fail(refused.why),
        }
    }

    /// [`Segs::push`], or the value handed back with the [`AllocFailure`] when a new segment
    /// could not be had.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<usize, Refused<T>> {
        let i = self.len;
        let (s, at) = locate(i);
        if at == 0 && self.segs[s].is_null() {
            match allocate_segment::<T, A>(&self.alloc, s, i) {
                Ok(seg) => self.segs[s] = seg.as_ptr(),
                Err(why) => return Err(Refused { value, why }),
            }
        }
        // SAFETY: segment `s` is allocated and `at` is inside it; the slot is past the length,
        // so empty.
        unsafe { self.segs[s].add(at).write(value) };
        self.len = i + 1;
        Ok(i)
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<&T> {
        if i >= self.len {
            return None;
        }
        let (s, at) = locate(i);
        // SAFETY: below the length, so its segment is allocated and the slot holds a value.
        Some(unsafe { &*self.segs[s].add(at) })
    }

    #[inline]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        if i >= self.len {
            return None;
        }
        let (s, at) = locate(i);
        // SAFETY: as `get`, borrowed mutably with the vector.
        Some(unsafe { &mut *self.segs[s].add(at) })
    }

    /// The values as slices, one a segment, in order.
    pub fn segments(&self) -> Segments<'_, T> {
        Segments { segs: &self.segs, next: 0, left: self.len }
    }

    /// Every value, in order.
    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.segments().flatten()
    }

    /// Drops every value; the segments are kept for the next pushes.
    pub fn clear(&mut self) {
        let len = core::mem::replace(&mut self.len, 0);
        let mut left = len;
        for (s, &seg) in self.segs.iter().enumerate() {
            if left == 0 {
                break;
            }
            let here = left.min(segment_len(s));
            left -= here;
            // SAFETY: the first `here` slots hold values; the length is already 0, so a
            // panicking drop leaks the rest rather than dropping one twice.
            unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(seg, here)) };
        }
    }

    /// The bytes of the segments allocated: the values plus the newest segment's unfilled part.
    pub fn heap_bytes(&self) -> usize {
        self.segs.iter().enumerate().filter(|(_, seg)| !seg.is_null()).map(|(s, _)| segment_len(s) * size_of::<T>()).sum()
    }
}

impl<T, A: Allocator> Drop for Segs<T, A> {
    fn drop(&mut self) {
        // SAFETY: the first `len` slots hold values; the segments were made by `alloc`.
        unsafe { drop_and_free(self.segs.iter().copied(), self.len, &self.alloc) };
    }
}

impl<T, A: Allocator> Index<usize> for Segs<T, A> {
    type Output = T;

    #[track_caller]
    #[inline]
    fn index(&self, i: usize) -> &T {
        match self.get(i) {
            Some(value) => value,
            None => out_of_bounds("Segs::index", i, self.len),
        }
    }
}

impl<T, A: Allocator> IndexMut<usize> for Segs<T, A> {
    #[track_caller]
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut T {
        let len = self.len;
        match self.get_mut(i) {
            Some(value) => value,
            None => out_of_bounds("Segs::index_mut", i, len),
        }
    }
}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Segs<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The values of a [`Segs`] as one slice a segment.
pub struct Segments<'a, T> {
    segs: &'a [*mut T; SEGMENTS],
    next: usize,
    left: usize,
}

impl<'a, T> Iterator for Segments<'a, T> {
    type Item = &'a [T];

    fn next(&mut self) -> Option<&'a [T]> {
        if self.left == 0 {
            return None;
        }
        let s = self.next;
        let here = self.left.min(segment_len(s));
        self.next += 1;
        self.left -= here;
        // SAFETY: values below the length: their segment is allocated and holds `here` of them.
        Some(unsafe { core::slice::from_raw_parts(self.segs[s], here) })
    }
}

// ---------------------------------------------------------------------------------------------
// The shared form.

/// An append-only vector of doubling segments one thread appends to while others read
/// ([module](self)).
pub struct SharedSegs<T, A: Allocator> {
    segs: [AtomicPtr<T>; SEGMENTS],
    /// The published length: every slot below it holds a value, visible to an acquire load.
    len: AtomicUsize,
    /// Whether an [`Appender`] is out.
    appending: AtomicBool,
    alloc: A,
    owns: PhantomData<T>,
}

// SAFETY: values move in on the appender's thread and are read as `&T` on any; dropped on the
// thread that drops the vector.
unsafe impl<T: Send, A: Allocator + Send> Send for SharedSegs<T, A> {}
// SAFETY: readers get `&T` (so `T: Sync`), the appender moves values in from its thread (so
// `T: Send`), and every slot below the published length is written before the release store
// that publishes it and never written after.
unsafe impl<T: Send + Sync, A: Allocator + Sync> Sync for SharedSegs<T, A> {}

impl<T, A: Allocator> SharedSegs<T, A> {
    pub fn new_in(alloc: A) -> SharedSegs<T, A> {
        SharedSegs {
            segs: core::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            len: AtomicUsize::new(0),
            appending: AtomicBool::new(false),
            alloc,
            owns: PhantomData,
        }
    }

    /// The published length.
    #[inline]
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The appender, if no other is out: the one way to push.
    pub fn appender(&self) -> Option<Appender<'_, T, A>> {
        match self.appending.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => Some(Appender { segs: self, len: self.len.load(Ordering::Relaxed) }),
            Err(_) => None,
        }
    }

    /// The value at `i`, if `i` is below the published length.
    #[inline]
    pub fn get(&self, i: usize) -> Option<&T> {
        if i >= self.len.load(Ordering::Acquire) {
            return None;
        }
        let (s, at) = locate(i);
        // Relaxed: the segment's pointer was stored before the release store of a length past
        // `i`, which the acquire load above read, so it is visible.
        let seg = self.segs[s].load(Ordering::Relaxed);
        assert!(!seg.is_null(), "SharedSegs: index {i} is published but its segment {s} is not visible");
        // SAFETY: below the published length: the slot holds a value, never written again.
        Some(unsafe { &*seg.add(at) })
    }
}

impl<T, A: Allocator> Drop for SharedSegs<T, A> {
    fn drop(&mut self) {
        let len = self.len.load(Ordering::Relaxed);
        // SAFETY: `&mut self`: no appender or reader is left; the first `len` slots hold values.
        unsafe { drop_and_free(self.segs.iter().map(|seg| seg.load(Ordering::Relaxed)), len, &self.alloc) };
    }
}

/// The one appender of a [`SharedSegs`]; another can be had once it drops.
pub struct Appender<'a, T, A: Allocator> {
    segs: &'a SharedSegs<T, A>,
    /// The length, which only this appender changes.
    len: usize,
}

impl<T, A: Allocator> Appender<'_, T, A> {
    /// Appends and publishes `value`; its index.
    #[track_caller]
    #[inline]
    pub fn push(&mut self, value: T) -> usize {
        match self.push_publishing(value, Ordering::Release) {
            Ok(i) => i,
            Err(refused) => fail(refused.why),
        }
    }

    /// [`Appender::push`], or the value handed back with the [`AllocFailure`].
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<usize, Refused<T>> {
        self.push_publishing(value, Ordering::Release)
    }

    /// The push, publishing the length with `publish`: `Release`, except the model's planted
    /// bug, which publishes `Relaxed`.
    #[inline]
    fn push_publishing(&mut self, value: T, publish: Ordering) -> Result<usize, Refused<T>> {
        let i = self.len;
        let (s, at) = locate(i);
        let mut seg = self.segs.segs[s].load(Ordering::Relaxed);
        if seg.is_null() {
            seg = match allocate_segment::<T, A>(&self.segs.alloc, s, i) {
                Ok(seg) => seg.as_ptr(),
                Err(why) => return Err(Refused { value, why }),
            };
            // Relaxed: published by the release store of the length below.
            self.segs.segs[s].store(seg, Ordering::Relaxed);
        }
        // SAFETY: the slot is inside segment `s`, past the published length: no reader looks
        // at it until the store below.
        unsafe { seg.add(at).write(value) };
        self.len = i + 1;
        self.segs.len.store(i + 1, publish);
        Ok(i)
    }
}

impl<T, A: Allocator> Drop for Appender<'_, T, A> {
    fn drop(&mut self) {
        self.segs.appending.store(false, Ordering::Release);
    }
}

#[cfg(all(test, blaze_check))]
mod model {
    //! `RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-own --release model_segs`.

    extern crate std;

    use std::{alloc::Global, sync::Arc};

    use blaze_check::thread;

    use super::*;

    /// One appender pushes into the first and second segments while a reader reads every
    /// published index; `publish` is the length's store order (`Relaxed` is the planted bug).
    fn reader_sees_every_published_value(publish: Ordering) {
        blaze_check::model(move || {
            let segs: Arc<SharedSegs<u64, Global>> = Arc::new(SharedSegs::new_in(Global));
            // Fill the first segment but one, so the race reaches its last slot and the second
            // segment's first (a new segment's pointer).
            {
                let mut appender = segs.appender().expect("the first appender");
                for v in 0..(FIRST as u64 - 1) {
                    appender.push(v);
                }
            }
            let reader = {
                let segs = segs.clone();
                thread::spawn(move || {
                    let len = segs.len();
                    for i in 0..len {
                        assert_eq!(segs.get(i).copied(), Some(i as u64), "a published value is read whole");
                    }
                })
            };
            {
                let mut appender = segs.appender().expect("the appender, the first dropped");
                for v in (FIRST as u64 - 1)..(FIRST as u64 + 1) {
                    if appender.push_publishing(v, publish).is_err() {
                        panic!("no memory in the model");
                    }
                }
            }
            reader.join().unwrap();
            assert_eq!(segs.len(), FIRST + 1);
        });
    }

    #[test]
    fn model_segs_reader_sees_every_published_value() {
        reader_sees_every_published_value(Ordering::Release);
    }

    /// The control: a relaxed publish lets the reader see the length before the new segment.
    #[test]
    #[should_panic(expected = "is not visible")]
    fn model_segs_relaxed_publish_is_caught() {
        reader_sees_every_published_value(Ordering::Relaxed);
    }
}

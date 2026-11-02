//! Per-worker bump segments (docs/v2/plan/design/concurrency.md "Memory"): what a task made
//! (its memo's edges, its value, a key's bytes, a slot's memo list) is copied once into a
//! segment of the thread that made it, and never moved or freed one by one.
//!
//! - Each thread holds a lane index of its own while it lives ([`lane`]); a [`Segments`] keeps
//!   one open segment a lane, written only by that lane's thread, so copying in is a bump and
//!   a copy with no shared write: the lane counts the holds it hands out and adds them to the
//!   segment's count once, when it seals it.
//! - A [`Span`] is bytes in a segment and one hold on it; dropping the span lets go of the
//!   hold. A span is dropped only once no reader can reach its bytes (its owner's row is past
//!   its grace, or it was never published).
//! - A segment is sealed when its lane opens another, and freed by [`Segments::sweep`] once
//!   it is sealed and holds nothing: freeing happens at the database's collection or with
//!   `&mut`, a segment at a time, never per memo.

use std::{
    alloc::{self, Layout},
    cell::UnsafeCell,
    mem::ManuallyDrop,
    ptr::{self, NonNull},
};

use crate::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicI64, Ordering},
        freed_bytes, read_bytes, thread_local, wrote_bytes,
    },
    table::ShortLock,
};

struct LaneIds {
    next: u32,
    free: Vec<u32>,
}

/// The lane indices of the process: a thread takes one when it first copies in and gives it
/// back when it ends, so the lanes in use stay as many as the threads.
// alloc: none: an empty `Vec` in a static; it grows by one a thread.
static LANE_IDS: ShortLock<LaneIds> = ShortLock::new(LaneIds { next: 0, free: Vec::new() });

struct Lane(u32);

impl Lane {
    fn claim() -> Lane {
        let mut ids = LANE_IDS.lock();
        Lane(ids.free.pop().unwrap_or_else(|| {
            let id = ids.next;
            ids.next += 1;
            id
        }))
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        LANE_IDS.lock().free.push(self.0);
    }
}

thread_local! {
    static LANE: Lane = Lane::claim();
}

/// This thread's lane: no other thread holds it while this one lives. Each caller uses it for
/// one write and makes no query call (so no wait) before it is done with the lane; not inlined,
/// so no caller keeps a thread's address of it across a later wait.
#[inline(never)]
pub(crate) fn lane() -> u32 {
    LANE.with(|lane| lane.0)
}

/// Chunk `k` of a [`Lanes`] holds `LANE_FIRST << k` lanes.
const LANE_FIRST: usize = 8;
const LANE_CHUNKS: usize = 29;

fn lane_place(lane: u32) -> (usize, usize) {
    let biased = lane as usize / LANE_FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, lane as usize - LANE_FIRST * ((1 << chunk) - 1))
}

/// A `T` a lane, made when a lane first asks; chunks never move.
pub(crate) struct Lanes<T> {
    chunks: [OnceLock<Box<[T]>>; LANE_CHUNKS],
}

impl<T: Default> Lanes<T> {
    pub(crate) fn new() -> Lanes<T> {
        Lanes { chunks: std::array::from_fn(|_| OnceLock::new()) }
    }

    pub(crate) fn get(&self, lane: u32) -> &T {
        let (chunk, at) = lane_place(lane);
        // alloc: once a chunk of lanes, made when its first lane asks.
        &self.chunks[chunk].get_or_init(|| (0..LANE_FIRST << chunk).map(|_| T::default()).collect())[at]
    }

    /// Every lane's `T` made so far.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.chunks.iter_mut().filter_map(OnceLock::get_mut).flat_map(|chunk| chunk.iter_mut())
    }
}

/// Segments are allocated at this alignment, the format's ([`blaze_format::ALIGN`]).
const SEGMENT_ALIGN: usize = blaze_format::ALIGN;

pub(crate) struct Segment {
    base: NonNull<u8>,
    cap: usize,
    /// How many spans (and rows that took a span's hold) are in it, plus [`OPEN`] while its
    /// lane copies in: that lane counts its copies in [`Open::taken`] and settles them at
    /// [`Open::seal`], so a copy does no read-modify-write on a shared line.
    holds: AtomicI64,
    /// Its lane opened another: no more is copied in.
    sealed: AtomicBool,
    /// The segment its [`Segments`] made before it: their list, read and written only under
    /// [`Segments::all`]'s lock.
    next: UnsafeCell<*mut Segment>,
}

/// A segment is one allocation: this header, then its bytes at [`SEGMENT_ALIGN`].
const HEADER: usize = size_of::<Segment>().next_multiple_of(SEGMENT_ALIGN);
const _: () = assert!(align_of::<Segment>() <= SEGMENT_ALIGN);

/// Added to an open segment's hold count, so it reads as held whatever its lane has copied
/// in; far above any count of real holds.
const OPEN: i64 = 1 << 48;

impl Segment {
    fn layout(cap: usize) -> Layout {
        Layout::from_size_align(HEADER + cap, SEGMENT_ALIGN).expect("a segment's layout")
    }

    /// A segment of `cap` bytes, its header in the same allocation.
    fn new(cap: usize, holds: i64, sealed: bool) -> NonNull<Segment> {
        assert!(cap > 0);
        let layout = Segment::layout(cap);
        // SAFETY: `layout` has a nonzero size.
        let raw = unsafe { alloc::alloc(layout) };
        let raw = NonNull::new(raw).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        // SAFETY: `HEADER + cap` bytes were allocated, so `HEADER` is inside them.
        let base = unsafe { raw.add(HEADER) };
        let segment = raw.cast::<Segment>();
        let header = Segment {
            base,
            cap,
            holds: AtomicI64::new(holds),
            sealed: AtomicBool::new(sealed),
            next: UnsafeCell::new(ptr::null_mut()),
        };
        // SAFETY: `raw` is aligned to `SEGMENT_ALIGN`, at least `Segment`'s alignment, and its
        // first `HEADER` bytes (at least a `Segment`) are this header's alone.
        unsafe { segment.write(header) };
        segment
    }

    /// # Safety
    ///
    /// `segment` was made by [`Segment::new`] and is freed once: no span, row or list names it.
    unsafe fn free(segment: NonNull<Segment>) {
        // SAFETY: the caller's: made by `Segment::new` and not freed yet.
        let (base, cap) = unsafe { (segment.as_ref().base, segment.as_ref().cap) };
        freed_bytes(base.as_ptr(), cap);
        // SAFETY: as above; the header is dropped once, then its allocation, of the layout it
        // was made with, is freed.
        unsafe {
            ptr::drop_in_place(segment.as_ptr());
            alloc::dealloc(segment.as_ptr().cast(), Segment::layout(cap));
        }
    }
}

/// Lets go of one hold on `segment` (null: bytes no segment holds).
///
/// # Safety
///
/// The caller owns one hold on `segment`, taken when bytes were copied in or shared, and lets
/// go of it once: the segment is freed only when no hold is left, so it is alive here.
pub(crate) unsafe fn release(segment: *const Segment) {
    if !segment.is_null() {
        // SAFETY: the caller's hold keeps the segment alive. Release: every read of its bytes
        // by this hold's owner happens before `Segments::sweep` sees no hold and frees it.
        unsafe { (*segment).holds.fetch_sub(1, Ordering::Release) };
    }
}

/// Bytes in a segment, with one hold on it (or bytes in a mapping the database keeps, with
/// none). 24 bytes.
pub(crate) struct Span {
    ptr: NonNull<u8>,
    segment: *const Segment,
    len: usize,
}

// SAFETY: a span names bytes that never change once copied in and stay until its hold is let
// go of; the hold count is atomic.
unsafe impl Send for Span {}
// SAFETY: as above.
unsafe impl Sync for Span {}

impl Span {
    /// Bytes in a mapping the database keeps (the image, a cold pack).
    ///
    /// # Safety
    ///
    /// `bytes` stay where they are, unchanged, until the span and every span shared from it
    /// are dropped: the database keeps the mapping they are in for as long as it lives.
    pub(crate) unsafe fn mapped(bytes: &[u8]) -> Span {
        Span { ptr: NonNull::from(bytes).cast(), segment: ptr::null(), len: bytes.len() }
    }

    fn empty(align: usize) -> Span {
        let ptr = NonNull::new(ptr::without_provenance_mut::<u8>(align)).expect("a nonzero alignment");
        Span { ptr, segment: ptr::null(), len: 0 }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        read_bytes(self.ptr.as_ptr(), self.len);
        // SAFETY: `len` bytes copied in at `ptr` before the span was made, which never change
        // and stay while its hold does (or while the database keeps the mapping they are in).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Another span of the same bytes, with a hold of its own.
    pub(crate) fn share(&self) -> Span {
        if !self.segment.is_null() {
            // SAFETY: this span's hold keeps the segment alive.
            unsafe { (*self.segment).holds.fetch_add(1, Ordering::Relaxed) };
        }
        Span { ptr: self.ptr, segment: self.segment, len: self.len }
    }

    /// The span's address, length and segment; its hold is the caller's now, to let go of
    /// with [`release`].
    pub(crate) fn into_raw(self) -> (NonNull<u8>, usize, *const Segment) {
        let span = ManuallyDrop::new(self);
        (span.ptr, span.len, span.segment)
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        // SAFETY: the span owns one hold, let go of once, here.
        unsafe { release(self.segment) };
    }
}

/// A lane's open segment: written only by the lane's thread.
struct Open {
    current: *mut Segment,
    used: usize,
    next_cap: usize,
    /// Spans copied into `current`, not yet added to its hold count.
    taken: i64,
}

impl Open {
    /// Seals the open segment: its copies' holds go into its count in place of [`OPEN`].
    fn seal(&mut self) {
        if self.current.is_null() {
            return;
        }
        // SAFETY: the lane's open segment holds [`OPEN`] until here, so it is not freed.
        let segment = unsafe { &*self.current };
        segment.holds.fetch_add(self.taken - OPEN, Ordering::Relaxed);
        // Release: the settled count happens before a collector that sees the seal reads it.
        segment.sealed.store(true, Ordering::Release);
        self.current = ptr::null_mut();
        self.taken = 0;
    }
}

#[repr(align(128))]
struct OpenCell(UnsafeCell<Open>);

impl Default for OpenCell {
    fn default() -> OpenCell {
        OpenCell(UnsafeCell::new(Open { current: ptr::null_mut(), used: 0, next_cap: 0, taken: 0 }))
    }
}

/// One kind of bytes' segments: a lane's grow from `first` bytes to `max`, doubling; a copy
/// larger than a quarter of `max` gets a segment of its own, sealed at once.
///
/// Per copy: its bytes, rounded up to `align`. A copy in: a thread-local load, a bump, a
/// `memcpy` and an add to a lane-local count; no atomic read-modify-write.
pub(crate) struct Segments {
    lanes: Lanes<OpenCell>,
    /// Every segment made and not freed, for [`Segments::sweep`] and drop.
    all: ShortLock<All>,
    first: usize,
    max: usize,
    align: usize,
}

/// A [`Segments`]' segments, the newest first, linked through their headers' `next`: making
/// one allocates nothing past the segment.
struct All {
    head: *mut Segment,
}

impl Default for All {
    fn default() -> All {
        All { head: ptr::null_mut() }
    }
}

// SAFETY: a lane's `Open` is read and written only by the thread holding that lane (`lane()`),
// and segments are reached through spans' holds or under `all`'s lock.
unsafe impl Send for Segments {}
// SAFETY: as above.
unsafe impl Sync for Segments {}

impl Segments {
    pub(crate) fn new(first: usize, max: usize, align: usize) -> Segments {
        debug_assert!(align.is_power_of_two() && align <= SEGMENT_ALIGN && first <= max);
        assert!(first > 0, "segments whose first is 0 never grow to fit a copy");
        Segments { lanes: Lanes::new(), all: ShortLock::default(), first, max, align }
    }

    fn make(&self, cap: usize, holds: i64, sealed: bool) -> *mut Segment {
        let segment = Segment::new(cap, holds, sealed).as_ptr();
        let mut all = self.all.lock();
        // SAFETY: just made, so no other thread reaches it; `next` is written under the lock.
        unsafe { *(*segment).next.get() = all.head };
        all.head = segment;
        segment
    }

    /// `bytes`, copied once into this thread's open segment at a multiple of `align` (at
    /// least the segments' own), with one hold.
    pub(crate) fn copy(&self, bytes: &[u8], align: usize) -> Span {
        let align = align.max(self.align);
        if bytes.is_empty() {
            return Span::empty(align);
        }
        let len = bytes.len();
        if len > self.max / 4 {
            let segment = self.make(len.next_multiple_of(SEGMENT_ALIGN), 1, true);
            // SAFETY: just made with the one hold this copy's span takes, owned by `all`.
            let segment = unsafe { &*segment };
            return Self::put(segment, 0, bytes);
        }
        let cell = self.lanes.get(lane());
        // SAFETY: only this thread holds its lane, and nothing below calls back into a copy.
        let open = unsafe { &mut *cell.0.get() };
        if !open.current.is_null() {
            // SAFETY: the lane's open segment is never sealed by another, so never freed.
            let segment = unsafe { &*open.current };
            let at = open.used.next_multiple_of(align);
            if at + len <= segment.cap {
                open.used = at + len;
                open.taken += 1;
                return Self::put(segment, at, bytes);
            }
            open.seal();
        }
        // A copy up to a quarter of `max` comes here, which can be more than the doubling's next
        // size; grow until it fits, as `put` writes `len` bytes into it unchecked in release.
        let mut cap = open.next_cap.max(self.first);
        while cap < len {
            cap = (cap * 2).min(self.max);
        }
        open.next_cap = (cap * 2).min(self.max);
        let segment = self.make(cap, OPEN, false);
        open.current = segment;
        open.used = len;
        open.taken = 1;
        // SAFETY: just made and open in this lane alone.
        Self::put(unsafe { &*segment }, 0, bytes)
    }

    /// Copies `bytes` in at `at`; the caller has counted the span's hold.
    ///
    /// # Panics
    ///
    /// If `bytes` don't fit at `at`: a copy past the segment is a write out of bounds, so it
    /// is checked in release too.
    fn put(segment: &Segment, at: usize, bytes: &[u8]) -> Span {
        assert!(
            at + bytes.len() <= segment.cap,
            "a copy of {} bytes at {at} into a segment of {}",
            bytes.len(),
            segment.cap
        );
        // SAFETY: `at..at + len` is inside the segment, and past what its lane copied in
        // before, so no reader has these bytes.
        let to = unsafe { segment.base.as_ptr().add(at) };
        wrote_bytes(to, bytes.len());
        // SAFETY: as above; `bytes` is another allocation.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len()) };
        Span {
            ptr: NonNull::new(to).expect("inside a segment"),
            segment: ptr::from_ref(segment),
            len: bytes.len(),
        }
    }

    /// Frees each segment that is sealed and holds nothing.
    pub(crate) fn sweep(&self) {
        let mut guard = self.all.lock();
        let all: &mut All = &mut guard;
        let mut link: *mut *mut Segment = &raw mut all.head;
        // SAFETY: `link` is the list's head or the `next` of a segment in it, both owned by
        // `all`'s lock, which this holds; a segment in the list is not freed yet.
        while let Some(segment) = NonNull::new(unsafe { *link }) {
            // SAFETY: in the list, so not freed yet.
            let it = unsafe { segment.as_ref() };
            let next = it.next.get();
            // Acquire: the sealing lane's copies and every releaser's reads happen before.
            let empty = it.sealed.load(Ordering::Acquire) && it.holds.load(Ordering::Acquire) == 0;
            if empty {
                // SAFETY: unlinked under the lock before it is freed. Sealed, so no lane
                // copies in; no hold, so no span or row names it, and none can be made (a hold
                // is shared only from a hold).
                unsafe {
                    *link = *next;
                    Segment::free(segment);
                }
            } else {
                link = next;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        let all = self.all.lock();
        let mut count = 0;
        let mut at = all.head;
        while let Some(segment) = NonNull::new(at) {
            count += 1;
            // SAFETY: in the list, which the lock held here keeps from changing.
            at = unsafe { *segment.as_ref().next.get() };
        }
        count
    }
}

impl Drop for Segments {
    fn drop(&mut self) {
        let mut at = self.all.get_mut().head;
        while let Some(segment) = NonNull::new(at) {
            // SAFETY: in the list, each freed once, after its `next` is read; `&mut` excludes
            // every reader, and the database drops what holds spans before its segments.
            unsafe {
                at = *segment.as_ref().next.get();
                Segment::free(segment);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hegel::{TestCase, generators as gs};

    use super::{Segments, Span};

    #[test]
    fn a_copy_bigger_than_the_lanes_next_segment_still_fits_in_it() {
        let segments = Segments::new(64, 1024, 8);
        for (len, fill) in [(100, 1), (256, 2), (40, 3), (250, 4)] {
            let bytes = vec![fill; len];
            let span = segments.copy(&bytes, 8);
            assert_eq!(span.bytes(), &bytes[..]);
        }
    }

    /// Copies of any sizes and alignments, in any order (past a lane's next segment, past a
    /// quarter of the largest), each keep their own bytes at their alignment: none was written
    /// past its segment or over another.
    #[hegel::test]
    fn every_copy_keeps_its_bytes_whatever_the_sizes(tc: TestCase) {
        let segments = Segments::new(64, 1024, 8);
        let count: usize = tc.draw(gs::integers::<usize>().min_value(1).max_value(40));
        let spans: Vec<(Span, Vec<u8>, usize)> = (0..count)
            .map(|at| {
                let len: usize = tc.draw(gs::integers::<usize>().max_value(400));
                let align = 1 << tc.draw(gs::integers::<u32>().max_value(4));
                let bytes: Vec<u8> = (0..len).map(|i| (at * 31 + i) as u8).collect();
                (segments.copy(&bytes, align), bytes, align)
            })
            .collect();
        for (span, bytes, align) in &spans {
            assert_eq!(span.bytes(), &bytes[..]);
            assert_eq!(span.bytes().as_ptr() as usize % align, 0, "aligned to {align}");
        }
    }
}

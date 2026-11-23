//! Fixed capacity, after Zig's bounded array and fixed-buffer allocator (rule 18: a table a
//! cap; rule 3: no fallback).
//!
//! - [`FixedVec<T, N>`]: up to `N` values stored inline, no allocator at all. A push past `N`
//!   is refused with [`Full`], which hands the value back and names the capacity; nothing grows.
//! - [`FixedString<N>`]: up to `N` bytes of UTF-8 inline, refused past `N` alike; a
//!   `fmt::Write` sink that fails rather than allocates. Its refusal is sticky (every later
//!   write is refused too) and it counts the bytes all writes wanted ([`Overflow`]), so a chain
//!   of writes never reads as a whole text when it was cut. [`FixedVec`]'s is not: a push
//!   hands its value back and the caller holds it, so nothing is lost unseen.
//! - [`Inline<BYTES>`]: an allocator over a buffer of `BYTES` inside itself, so any collection
//!   here can live on the stack: `Vec::new_in(&inline)`. It bumps a top and refuses when the
//!   buffer is full; it never asks another allocator. A free of the newest block, and a growth
//!   or shrink of it, happen in place (a stack discipline, which a growing vector's buffer and
//!   a scratch string follow); a free of an older block keeps its bytes until
//!   [`Inline::reset`].
//!
//! `Inline` is an allocator only by shared reference (`&Inline<BYTES>`): a block points into the
//! value, which must not move while a block is live, and a borrow is what keeps it still. It
//! is `!Sync` (its top is a `Cell`), so one thread uses it.

use core::{
    alloc::{AllocError, Allocator, Layout},
    cell::{Cell, UnsafeCell},
    fmt,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut, Range},
    ptr::{self, NonNull},
    slice,
};

use crate::vec::out_of_bounds;

/// A value [`FixedVec`] had no room for, handed back with its capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full<T> {
    pub value: T,
    pub cap: usize,
}

impl<T> fmt::Display for Full<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a fixed vector of `{}` is full at {} values", core::any::type_name::<T>(), self.cap)
    }
}

impl<T: fmt::Debug> core::error::Error for Full<T> {}

/// Up to `N` values of `T`, inline.
pub struct FixedVec<T, const N: usize> {
    /// Slots `..len` hold values; the rest are empty.
    len: usize,
    slots: [MaybeUninit<T>; N],
}

impl<T, const N: usize> FixedVec<T, N> {
    pub const fn new() -> FixedVec<T, N> {
        FixedVec { len: 0, slots: [const { MaybeUninit::uninit() }; N] }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// `N`.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    #[inline]
    fn first(&self) -> *const T {
        self.slots.as_ptr().cast()
    }

    #[inline]
    fn first_mut(&mut self) -> *mut T {
        self.slots.as_mut_ptr().cast()
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first `len` slots hold values.
        unsafe { slice::from_raw_parts(self.first(), self.len) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: the first `len` slots hold values.
        unsafe { slice::from_raw_parts_mut(self.first_mut(), self.len) }
    }

    /// `value` at the back, or handed back when all `N` slots are used.
    #[inline]
    pub fn push(&mut self, value: T) -> Result<(), Full<T>> {
        if self.len == N {
            return Err(Full { value, cap: N });
        }
        self.slots[self.len].write(value);
        self.len += 1;
        Ok(())
    }

    /// Each of `values` pushed in order, stopping at the first with no room, which is handed
    /// back; the values before it stay pushed and the rest of `values` is not read.
    pub fn try_extend(&mut self, values: impl IntoIterator<Item = T>) -> Result<(), Full<T>> {
        for value in values {
            self.push(value)?;
        }
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: slot `len` held a value the length no longer counts; it is read once.
        Some(unsafe { self.slots[self.len].assume_init_read() })
    }

    /// `value` at `index`, the values from there on moved one slot back; handed back when full.
    ///
    /// # Panics
    ///
    /// When `index > len`.
    #[track_caller]
    pub fn insert_shift(&mut self, index: usize, value: T) -> Result<(), Full<T>> {
        if index > self.len {
            out_of_bounds("FixedVec::insert_shift", index, self.len);
        }
        if self.len == N {
            return Err(Full { value, cap: N });
        }
        // SAFETY: `index <= len < N`: slots `index..len` move to `index + 1..len + 1`, inside the
        // array; slot `index` is then written.
        unsafe {
            let at = self.first_mut().add(index);
            ptr::copy(at, at.add(1), self.len - index);
            at.write(value);
        }
        self.len += 1;
        Ok(())
    }

    /// The value at `index`, the values after it moved one slot forward.
    ///
    /// # Panics
    ///
    /// When `index >= len`.
    #[track_caller]
    pub fn remove_shift(&mut self, index: usize) -> T {
        if index >= self.len {
            out_of_bounds("FixedVec::remove_shift", index, self.len);
        }
        self.len -= 1;
        // SAFETY: `index` held a value, read once; slots `index + 1..=len` move down over it.
        unsafe {
            let at = self.first_mut().add(index);
            let value = at.read();
            ptr::copy(at.add(1), at, self.len - index);
            value
        }
    }

    /// The value at `index`, the last value moved into its slot.
    ///
    /// # Panics
    ///
    /// When `index >= len`.
    #[track_caller]
    pub fn remove_swap(&mut self, index: usize) -> T {
        if index >= self.len {
            out_of_bounds("FixedVec::remove_swap", index, self.len);
        }
        let last = self.len - 1;
        self.as_mut_slice().swap(index, last);
        let Some(value) = self.pop() else { unreachable!("`index < len`, so it held a value") };
        value
    }

    /// Drops the values from `new_len` on; nothing when it is no longer. A drop that panics
    /// leaves the rest dropped too (the slice's drop goes on), and the length already cut.
    pub fn truncate(&mut self, new_len: usize) {
        if new_len >= self.len {
            return;
        }
        let tail = ptr::slice_from_raw_parts_mut(
            // SAFETY: `new_len < len <= N`.
            unsafe { self.first_mut().add(new_len) },
            self.len - new_len,
        );
        self.len = new_len;
        // SAFETY: the tail held values the length no longer counts, dropped once.
        unsafe { ptr::drop_in_place(tail) };
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Keeps the values `keep` says yes to, in order. A panic in `keep` or in a drop keeps the
    /// values not yet seen, after the ones kept.
    pub fn retain(&mut self, mut keep: impl FnMut(&mut T) -> bool) {
        /// Slots `..write` kept, `write..read` empty, `read..total` not yet seen. Every slot is
        /// reached from `base`, taken once: a second borrow of the slots (`first_mut`) would
        /// end the pointers taken from the first under the aliasing model, as Miri found.
        struct Compact<'a, T> {
            base: *mut T,
            len: &'a mut usize,
            read: usize,
            write: usize,
            total: usize,
        }
        impl<T> Drop for Compact<'_, T> {
            fn drop(&mut self) {
                let unseen = self.total - self.read;
                // SAFETY: the unseen slots move down to right after the kept ones; `copy`
                // allows the overlap.
                unsafe { ptr::copy(self.base.add(self.read), self.base.add(self.write), unseen) };
                *self.len = self.write + unseen;
            }
        }
        let total = self.len;
        // Nothing counts as held while the slots are in motion. The base and the length are
        // borrowed apart: two fields.
        self.len = 0;
        let base = self.slots.as_mut_ptr().cast::<T>();
        let mut at = Compact { base, len: &mut self.len, read: 0, write: 0, total };
        while at.read < at.total {
            // SAFETY: `read < total <= N` holds a value not yet seen.
            let slot = unsafe { at.base.add(at.read) };
            // SAFETY: as above; no other reference to it is live.
            if keep(unsafe { &mut *slot }) {
                if at.read != at.write {
                    // SAFETY: `write < read`: an empty slot; the value moves.
                    unsafe { ptr::copy_nonoverlapping(slot, at.base.add(at.write), 1) };
                }
                at.write += 1;
                at.read += 1;
            } else {
                // Seen before the drop, so a panicking drop does not drop it again.
                at.read += 1;
                // SAFETY: the value is dropped once; its slot is now in the empty run.
                unsafe { ptr::drop_in_place(slot) };
            }
        }
    }
}

/// The old names, whose cost the name did not say; each forwards to the one that says it.
impl<T, const N: usize> FixedVec<T, N> {
    #[deprecated(note = "linear: use `insert_shift`, which says so")]
    #[track_caller]
    #[inline]
    pub fn insert(&mut self, index: usize, value: T) -> Result<(), Full<T>> {
        self.insert_shift(index, value)
    }

    #[deprecated(note = "linear: use `remove_shift`, or `remove_swap` when order does not matter")]
    #[track_caller]
    #[inline]
    pub fn remove(&mut self, index: usize) -> T {
        self.remove_shift(index)
    }

    #[deprecated(note = "renamed `remove_swap`, beside `remove_shift`")]
    #[track_caller]
    #[inline]
    pub fn swap_remove(&mut self, index: usize) -> T {
        self.remove_swap(index)
    }
}

impl<T, const N: usize> Default for FixedVec<T, N> {
    fn default() -> FixedVec<T, N> {
        FixedVec::new()
    }
}

impl<T, const N: usize> Drop for FixedVec<T, N> {
    fn drop(&mut self) {
        // SAFETY: the first `len` slots hold values, dropped once.
        unsafe { ptr::drop_in_place(self.as_mut_slice()) };
    }
}

impl<T, const N: usize> Deref for FixedVec<T, N> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, const N: usize> DerefMut for FixedVec<T, N> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: Clone, const N: usize> Clone for FixedVec<T, N> {
    fn clone(&self) -> FixedVec<T, N> {
        let mut out = FixedVec::new();
        for value in self.as_slice() {
            // The same length fits the same capacity.
            if out.push(value.clone()).is_err() {
                unreachable!("a clone has the capacity of its source");
            }
        }
        out
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for FixedVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl<T: PartialEq, const N: usize, const M: usize> PartialEq<FixedVec<T, M>> for FixedVec<T, N> {
    fn eq(&self, other: &FixedVec<T, M>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, const N: usize> Eq for FixedVec<T, N> {}

impl<T: Hash, const N: usize> Hash for FixedVec<T, N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a FixedVec<T, N> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;
    fn into_iter(self) -> slice::Iter<'a, T> {
        self.as_slice().iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a mut FixedVec<T, N> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;
    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.as_mut_slice().iter_mut()
    }
}

impl<T, const N: usize> IntoIterator for FixedVec<T, N> {
    type Item = T;
    type IntoIter = IntoIter<T, N>;
    fn into_iter(self) -> IntoIter<T, N> {
        let me = ManuallyDrop::new(self);
        // SAFETY: `me` is never dropped, so the slots move out once.
        let slots = unsafe { ptr::read(&me.slots) };
        IntoIter { slots, left: 0..me.len }
    }
}

/// A [`FixedVec`]'s values by value.
pub struct IntoIter<T, const N: usize> {
    slots: [MaybeUninit<T>; N],
    /// The slots still holding values.
    left: Range<usize>,
}

impl<T, const N: usize> Iterator for IntoIter<T, N> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        let at = self.left.next()?;
        // SAFETY: slot `at` held a value no longer in `left`, read once.
        Some(unsafe { self.slots[at].assume_init_read() })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.left.size_hint()
    }
}

impl<T, const N: usize> DoubleEndedIterator for IntoIter<T, N> {
    fn next_back(&mut self) -> Option<T> {
        let at = self.left.next_back()?;
        // SAFETY: as `next`.
        Some(unsafe { self.slots[at].assume_init_read() })
    }
}

impl<T, const N: usize> ExactSizeIterator for IntoIter<T, N> {}

impl<T, const N: usize> FusedIterator for IntoIter<T, N> {}

impl<T, const N: usize> Drop for IntoIter<T, N> {
    fn drop(&mut self) {
        let left = ptr::slice_from_raw_parts_mut(
            // SAFETY: `left.start <= N`.
            unsafe { self.slots.as_mut_ptr().cast::<T>().add(self.left.start) },
            self.left.len(),
        );
        // SAFETY: the slots in `left` hold values, dropped once.
        unsafe { ptr::drop_in_place(left) };
    }
}

/// What a [`FixedString`] that refused a write says: its room, and the bytes all its writes
/// wanted (the refused ones too), to size the next buffer exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Overflow {
    pub capacity: usize,
    pub needed: usize,
}

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a fixed string of {} bytes was asked for {}", self.capacity, self.needed)
    }
}

impl core::error::Error for Overflow {}

/// Up to `N` bytes of UTF-8, inline (see the module).
///
/// A refusal is sticky: once a write does not fit, every later write is refused too, until
/// [`FixedString::clear`], so a chain of writes never leaves a cut text that reads as whole;
/// [`FixedString::text`] says which it is, and how many bytes the writes wanted.
#[derive(Clone, Copy)]
pub struct FixedString<const N: usize> {
    /// Bytes `..len` are UTF-8, cut only at a character's boundary; the rest are zero.
    len: usize,
    bytes: [u8; N],
    /// The bytes every write since the last clear asked for, the refused ones too.
    needed: usize,
    /// A write was refused since the last clear.
    refused: bool,
}

impl<const N: usize> FixedString<N> {
    pub const fn new() -> FixedString<N> {
        FixedString { len: 0, bytes: [0; N], needed: 0, refused: false }
    }

    /// The text, or the [`Overflow`] when any write since the last clear was refused.
    pub fn text(&self) -> Result<&str, Overflow> {
        if self.refused {
            return Err(Overflow { capacity: N, needed: self.needed });
        }
        Ok(self.as_str())
    }

    /// Whether a write since the last clear was refused (and so every later one).
    #[inline]
    pub const fn is_refused(&self) -> bool {
        self.refused
    }

    /// The bytes every write since the last clear asked for, the refused ones too.
    #[inline]
    pub const fn needed(&self) -> usize {
        self.needed
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: bytes `..len` are only ever written from whole `str`s (`try_push_str`), so
        // they are UTF-8 and end at a character's boundary.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `N`.
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// `text` at the end, or handed back whole (nothing written) when it does not fit or an
    /// earlier write was refused (the refusal is sticky).
    pub fn try_push_str<'t>(&mut self, text: &'t str) -> Result<(), Full<&'t str>> {
        self.needed = self.needed.saturating_add(text.len());
        let end = self.len + text.len();
        if self.refused || end > N {
            self.refused = true;
            return Err(Full { value: text, cap: N });
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }

    /// `char` at the end, or handed back when its UTF-8 does not fit.
    pub fn try_push(&mut self, char: char) -> Result<(), Full<char>> {
        let mut buf = [0; 4];
        self.try_push_str(char.encode_utf8(&mut buf)).map_err(|_| Full { value: char, cap: N })
    }

    /// Empties it, the refusal and the count too.
    pub fn clear(&mut self) {
        self.len = 0;
        self.needed = 0;
        self.refused = false;
    }
}

impl<const N: usize> Default for FixedString<N> {
    fn default() -> FixedString<N> {
        FixedString::new()
    }
}

impl<const N: usize> Deref for FixedString<N> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

/// A write that does not fit fails with `fmt::Error` and writes nothing of that piece.
impl<const N: usize> fmt::Write for FixedString<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.try_push_str(text).map_err(|_| fmt::Error)
    }
}

impl<const N: usize> fmt::Debug for FixedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for FixedString<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl<const N: usize> PartialEq for FixedString<N> {
    fn eq(&self, other: &FixedString<N>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const N: usize> Eq for FixedString<N> {}

impl<const N: usize> Hash for FixedString<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

/// An allocator over `BYTES` bytes inside itself (see the module).
pub struct Inline<const BYTES: usize> {
    /// Bytes `..top` are handed out or wasted; `top..` are free.
    top: Cell<usize>,
    buf: UnsafeCell<[MaybeUninit<u8>; BYTES]>,
}

impl<const BYTES: usize> Inline<BYTES> {
    pub const fn new() -> Inline<BYTES> {
        Inline { top: Cell::new(0), buf: UnsafeCell::new([MaybeUninit::uninit(); BYTES]) }
    }

    /// Bytes handed out, padding and freed older blocks included.
    pub fn used(&self) -> usize {
        self.top.get()
    }

    pub fn remaining(&self) -> usize {
        BYTES - self.top.get()
    }

    /// Frees every block at once: `&mut self` shows none is live.
    pub fn reset(&mut self) {
        self.top.set(0);
    }

    #[inline]
    fn base(&self) -> *mut u8 {
        self.buf.get().cast()
    }

    /// The offset of a block of this buffer.
    #[inline]
    fn offset(&self, ptr: NonNull<u8>) -> usize {
        ptr.addr().get() - self.base().addr()
    }

    /// Whether the block at `start` of `size` bytes is the newest, ending at the top.
    #[inline]
    fn is_newest(&self, start: usize, size: usize) -> bool {
        start + size == self.top.get()
    }

    /// The block of `size` bytes at offset `start`.
    #[inline]
    fn block(&self, start: usize, size: usize) -> NonNull<[u8]> {
        // SAFETY: `start <= BYTES`, inside the buffer or one past it, which is not null.
        let ptr = unsafe { NonNull::new_unchecked(self.base().add(start)) };
        NonNull::slice_from_raw_parts(ptr, size)
    }
}

impl<const BYTES: usize> Default for Inline<BYTES> {
    fn default() -> Inline<BYTES> {
        Inline::new()
    }
}

impl<const BYTES: usize> fmt::Debug for Inline<BYTES> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inline").field("bytes", &BYTES).field("used", &self.used()).finish()
    }
}

// SAFETY: `&Inline<BYTES>` allocates from a fixed stack buffer reclaimed in bulk when `Inline`
// drops; forgetting a handle never leaks memory past the `Inline`'s lifetime.
unsafe impl<const BYTES: usize> crate::BumpAlloc for &Inline<BYTES> {}

// SAFETY: a block is a range of the buffer below the top, handed out once; the top only moves
// down over the newest block when that block is freed or shrunk, so no two live blocks overlap.
// The buffer does not move while a block is live, because the allocator is this shared
// reference and the value cannot move while it is borrowed. Growing in place keeps the bytes;
// growing elsewhere copies them; nothing is asked of any other allocator.
unsafe impl<const BYTES: usize> Allocator for &Inline<BYTES> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let top = self.top.get();
        let addr = self.base().addr() + top;
        let aligned = addr.checked_next_multiple_of(layout.align()).ok_or(AllocError)?;
        let start = top + (aligned - addr);
        let end = start.checked_add(layout.size()).ok_or(AllocError)?;
        if end > BYTES {
            return Err(AllocError);
        }
        self.top.set(end);
        Ok(self.block(start, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let start = self.offset(ptr);
        if self.is_newest(start, layout.size()) {
            self.top.set(start);
        }
    }

    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let start = self.offset(ptr);
        if self.is_newest(start, old.size()) && ptr.addr().get().is_multiple_of(new.align()) {
            let end = start.checked_add(new.size()).ok_or(AllocError)?;
            if end > BYTES {
                return Err(AllocError);
            }
            self.top.set(end);
            return Ok(self.block(start, new.size()));
        }
        let block = self.allocate(new)?;
        // SAFETY: the old block holds `old.size()` bytes; the new one, just taken above the
        // top, is at least as large and does not overlap it.
        unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), block.cast::<u8>().as_ptr(), old.size()) };
        // An older block keeps its bytes until `reset`; there is nothing to free.
        Ok(block)
    }

    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let start = self.offset(ptr);
        if !ptr.addr().get().is_multiple_of(new.align()) {
            let block = self.allocate(new)?;
            // SAFETY: the new block, above the top, holds `new.size() <= old.size()` bytes and
            // does not overlap the old one.
            unsafe { ptr::copy_nonoverlapping(ptr.as_ptr(), block.cast::<u8>().as_ptr(), new.size()) };
            return Ok(block);
        }
        if self.is_newest(start, old.size()) {
            self.top.set(start + new.size());
        }
        Ok(self.block(start, new.size()))
    }
}

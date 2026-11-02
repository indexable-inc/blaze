//! The memory of the running image that the platforms read and write for the switch: a
//! patch's bytes put where they go ([`Writes`]), and views of the descriptor, the table cell
//! and the loader's table. Only the platform modules call the unsafe functions here, after
//! checking the addresses against the image's mappings; the switch itself has no unsafe.
//! Also the typed rows of the address space the switch's columns reserve ([`rows`]), whose
//! safety rests on the platform's [`Reserved`].

#![allow(
    unsafe_code,
    reason = "atomic views of the image's words at the addresses the linker placed them"
)]

use std::{
    ops::Range,
    sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering},
};

use super::{
    os::Reserved,
    switch::{Writes, le64},
};

/// What a reserved column holds: a type every pattern of its bytes is a value of, zeroes
/// included, with no padding, and an alignment no more than a page's. Implementing it takes
/// `unsafe impl`, which only this module allows, so its list is the one below.
///
/// # Safety
///
/// Every `size_of::<T>()` bytes are a valid `T`, and `T`'s alignment divides 4096.
pub(super) unsafe trait Plain: Copy {}

// SAFETY: any byte is a `u8`.
unsafe impl Plain for u8 {}
// SAFETY: any 4 bytes are a `u32`, aligned to 4.
unsafe impl Plain for u32 {}
// SAFETY: any 8 bytes are a `u64`, aligned to 8.
unsafe impl Plain for u64 {}

/// The first `len` rows of `T` of `reserved`, which has them committed (asserted).
pub(super) fn rows<T: Plain>(reserved: &Reserved, len: usize) -> &[T] {
    let bytes = len.checked_mul(size_of::<T>());
    assert!(bytes.is_some_and(|it| it <= reserved.committed()), "{len} rows past the committed");
    // SAFETY: the first `committed` bytes of the reservation are committed, readable, and its
    // alone (`Reserved`'s invariant), and `len` rows are within them (asserted). Its base is
    // page-aligned, so aligned for `T`, and any bytes there, written or zero, are a `T`
    // (`Plain`). The borrow of `reserved` keeps it from being written or unmapped meanwhile.
    unsafe { std::slice::from_raw_parts(reserved.base() as *const T, len) }
}

/// [`rows`], to write.
pub(super) fn rows_mut<T: Plain>(reserved: &mut Reserved, len: usize) -> &mut [T] {
    let bytes = len.checked_mul(size_of::<T>());
    assert!(bytes.is_some_and(|it| it <= reserved.committed()), "{len} rows past the committed");
    // SAFETY: as in `rows`, and committed writable; the mutable borrow of `reserved` makes
    // this the only view of its rows.
    unsafe { std::slice::from_raw_parts_mut(reserved.base() as *mut T, len) }
}

/// How many bytes [`Writes::store`] makes on the stack before storing them.
const CHUNK: usize = 256;

impl Writes<'_> {
    /// Writes what the spans have within `range` to `to`, which stands for `range.start`,
    /// with the slide added to each field to rebase: the bytes go straight from the message
    /// to where they run, once, by plain copies (a rebased field is written twice).
    ///
    /// # Safety
    ///
    /// `to` is valid for writes of `range.end - range.start` bytes, which nothing else reads
    /// or writes while this runs.
    pub(crate) unsafe fn put(&self, range: &Range<u64>, to: *mut u8) {
        let first = self.spans.partition_point(|it| it.end() <= range.start);
        for span in &self.spans[first..] {
            if span.at >= range.end {
                break;
            }
            let bytes = self.bytes(span);
            let (from, until) = (span.at.max(range.start), span.end().min(range.end));
            let part = &bytes[(from - span.at) as usize..(until - span.at) as usize];
            let into = to.wrapping_add((from - range.start) as usize);
            // SAFETY: `from..until` is within `range`, writable at `into` (the caller's); the
            // message is not the memory written.
            unsafe { std::ptr::copy_nonoverlapping(part.as_ptr(), into, part.len()) };
            let lowest = span.at.max(from.saturating_sub(7));
            let first = self.rebase.partition_point(|it| *it < lowest);
            for &field in self.rebase[first..].iter().take_while(|it| **it < until) {
                let value = le64(&bytes[(field - span.at) as usize..])
                    .wrapping_add(self.slide)
                    .to_le_bytes();
                let (start, end) = (field.max(from), (field + 8).min(until));
                let part = &value[(start - field) as usize..(end - field) as usize];
                let into = to.wrapping_add((start - range.start) as usize);
                // SAFETY: as above: `start..end` is within `range`; `value` is a local.
                unsafe { std::ptr::copy_nonoverlapping(part.as_ptr(), into, part.len()) };
            }
        }
    }

    /// [`Writes::put`] into `to`, which stands for `range`.
    pub(crate) fn fill(&self, range: &Range<u64>, to: &mut [u8]) {
        assert_eq!(to.len() as u64, range.end - range.start, "a buffer the length of {range:x?}");
        // SAFETY: `to` is `range`'s length (asserted) and borrowed mutably for this call.
        unsafe { self.put(range, to.as_mut_ptr()) }
    }

    /// [`Writes::put`] for memory other threads may read while it is written: the bytes,
    /// rebased, are made [`CHUNK`] at a time in a buffer on the stack (by [`Writes::fill`]),
    /// then stored with one relaxed atomic store per naturally aligned piece of 8, 4, 2 or 1
    /// bytes. A thread that loads such a piece as it is written gets all of its old value or
    /// all of its new one, never an address not yet rebased. Chunks end on 8-byte boundaries
    /// of the memory written, so no aligned field is cut. It costs a copy through the stack,
    /// a few nanoseconds per chunk, beside the plain copy.
    ///
    /// # Safety
    ///
    /// `to` is valid for writes of `range.end - range.start` bytes while this runs. Other
    /// threads may load or store those bytes meanwhile; the stores here are atomic, and on
    /// x86-64 and AArch64 an aligned load or store of at most 8 bytes, which is what the
    /// program's own code makes of a static, is single-copy atomic against them.
    pub(crate) unsafe fn store(&self, range: &Range<u64>, to: *mut u8) {
        let mut buffer = [0u8; CHUNK];
        let first = self.spans.partition_point(|it| it.end() <= range.start);
        for span in &self.spans[first..] {
            if span.at >= range.end {
                break;
            }
            let (mut from, until) = (span.at.max(range.start), span.end().min(range.end));
            while from < until {
                let into = to.wrapping_add((from - range.start) as usize);
                let room = CHUNK - (into as usize & 7);
                let len = (until - from).min(room as u64) as usize;
                let part = &mut buffer[..len];
                self.fill(&(from..from + len as u64), part);
                // SAFETY: `into..into + len` is within `range`, which `to` stands for (the
                // caller's); `part` is this function's buffer.
                unsafe { store_atomic(part, into) };
                from += len as u64;
            }
        }
    }

    /// [`Writes::store`] of every span where it goes.
    ///
    /// # Safety
    ///
    /// As [`Writes::store`], for every span's memory.
    #[cfg(test)]
    pub(crate) unsafe fn store_in_place(&self) {
        for span in self.spans {
            // SAFETY: the caller's.
            unsafe { self.store(&span.range(), span.at as *mut u8) };
        }
    }
}

/// Stores `bytes` at `to`, one relaxed atomic store per naturally aligned piece of 8, 4, 2
/// or 1 bytes, in memory order.
///
/// # Safety
///
/// `to..to + bytes.len()` is valid for writes while this runs, and accessed meanwhile by
/// other threads only as [`Writes::store`] allows.
unsafe fn store_atomic(bytes: &[u8], to: *mut u8) {
    let mut at = 0;
    while at < bytes.len() {
        let into = to.wrapping_add(at);
        let (address, left) = (into as usize, bytes.len() - at);
        if address.is_multiple_of(8) && left >= 8 {
            let value = u64::from_ne_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
            // SAFETY: 8 writable bytes (the caller's) at the 8-aligned `into`; an `AtomicU64`
            // has a `u64`'s size and alignment.
            unsafe { AtomicU64::from_ptr(into.cast()) }.store(value, Ordering::Relaxed);
            at += 8;
        } else if address.is_multiple_of(4) && left >= 4 {
            let value = u32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
            // SAFETY: as above, 4 bytes at the 4-aligned `into`.
            unsafe { AtomicU32::from_ptr(into.cast()) }.store(value, Ordering::Relaxed);
            at += 4;
        } else if address.is_multiple_of(2) && left >= 2 {
            let value = u16::from_ne_bytes(bytes[at..at + 2].try_into().expect("2 bytes"));
            // SAFETY: as above, 2 bytes at the 2-aligned `into`.
            unsafe { AtomicU16::from_ptr(into.cast()) }.store(value, Ordering::Relaxed);
            at += 2;
        } else {
            // SAFETY: as above, 1 byte, which any address aligns.
            unsafe { AtomicU8::from_ptr(into) }.store(bytes[at], Ordering::Relaxed);
            at += 1;
        }
    }
}

/// `len` bytes of the image at `at`.
///
/// # Safety
///
/// `at..at + len` is mapped readable for the life of the program (the platform checked it
/// is in the image), and nothing writes it while the slice is used.
pub(super) unsafe fn view(at: u64, len: usize) -> &'static [u8] {
    // SAFETY: the caller's; any bytes are valid `u8`s.
    unsafe { std::slice::from_raw_parts(at as *const u8, len) }
}

/// `len` 8-byte words of the image at `at`, which [`crate::slots::publish`] may store to while
/// they are read.
///
/// # Safety
///
/// `at..at + len * 8` is mapped readable and writable for the life of the program (the
/// platform checked it is in the image's data), `at` is 8-aligned, and the words are accessed
/// only atomically.
pub(super) unsafe fn words(at: u64, len: usize) -> &'static [AtomicU64] {
    // SAFETY: the caller's; an `AtomicU64` has a `u64`'s size and alignment, and any bits are
    // a valid one.
    unsafe { std::slice::from_raw_parts(at as *const AtomicU64, len) }
}

/// The byte at `at`, which log stubs and other processes sharing its pages may store to.
///
/// # Safety
///
/// `at` is mapped readable and writable for the life of the program, and accessed only
/// atomically or by the log stubs' single-byte stores.
pub(super) unsafe fn byte(at: u64) -> &'static AtomicU8 {
    // SAFETY: the caller's; an `AtomicU8` has a `u8`'s size and alignment.
    unsafe { AtomicU8::from_ptr(at as *mut u8) }
}

/// The 8-byte table cell at `at`.
///
/// # Safety
///
/// `at` is 8-aligned, in the image's data, mapped for the life of the program, and accessed
/// only atomically: the switch's stores and the stubs' loads.
pub(super) unsafe fn cell(at: u64) -> &'static AtomicU64 {
    // SAFETY: the caller's; an `AtomicU64` has a `u64`'s size and alignment.
    unsafe { AtomicU64::from_ptr(at as *mut u64) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::switch::Span;

    /// `store`, the atomic path, gives the bytes `fill`, the plain safe one, does: for every
    /// misalignment of the memory written, for ranges that cut spans and rebased fields, over
    /// more than one chunk.
    #[test]
    fn atomic_stores_give_the_bytes_plain_copies_do() {
        let message: Vec<u8> = (0..2048u32).map(|it| (it * 31 + 7) as u8).collect();
        for misaligned in 0..8u64 {
            let base = 0x10_000 + misaligned;
            let spans = [
                Span::new(base + 3, 0, 1),
                Span::new(base + 5, 1, 13),
                Span::new(base + 40, 100, 700),
                Span::new(base + 800, 900, 9),
                Span::new(base + 811, 1000, 600),
            ];
            let rebase = [base + 6, base + 48, base + 401, base + 731, base + 811, base + 1403];
            let writes = Writes::new(&message, &spans, &rebase, 0x1234_5678_9abc);
            for range in
                [base..base + 1500, base + 7..base + 803, base + 50..base + 51, base..base + 3]
            {
                let len = (range.end - range.start) as usize;
                let mut plain = vec![0xeeu8; len];
                writes.fill(&range, &mut plain);
                // Backed by `u64`s so the stores see `misaligned` as the buffer's own.
                let mut words = vec![0xeeee_eeee_eeee_eeeeu64; len / 8 + 2];
                let start = words.as_mut_ptr().cast::<u8>().wrapping_add(misaligned as usize);
                // SAFETY: `start` has `len` bytes of `words` after it (`len / 8 + 2` words from
                // at most 7 bytes in), which this test alone uses.
                unsafe { writes.store(&range, start) };
                // SAFETY: as above, read after the stores.
                let stored = unsafe { std::slice::from_raw_parts(start, len) };
                assert_eq!(stored, &plain[..], "{range:x?} at {misaligned} bytes off 8");
            }
        }
    }

    /// The pieces `store_atomic` makes: the largest naturally aligned one at each address.
    #[test]
    fn pieces_are_naturally_aligned() {
        let bytes: Vec<u8> = (1..=40).collect();
        for misaligned in 0..8 {
            let mut words = [0u64; 7];
            let start = words.as_mut_ptr().cast::<u8>().wrapping_add(misaligned);
            // SAFETY: 40 bytes after at most 7 into 56, which this test alone uses.
            unsafe { store_atomic(&bytes, start) };
            // SAFETY: as above.
            let stored = unsafe { std::slice::from_raw_parts(start, bytes.len()) };
            assert_eq!(stored, &bytes[..], "{misaligned} bytes off 8");
        }
    }
}

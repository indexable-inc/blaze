//! Copies over memory that another thread, or the kernel, may change while the copy runs: the
//! store's buffer pool reads a page optimistically (docs/v2/plan/design/store-pool.md "Optimistic"
//! reads) while the evictor may give it back (`MADV_DONTNEED`, `MADV_FREE_REUSABLE`) and a loader
//! may read a file into it again. A plain `&[u8]` read there would be a data race, which Rust
//! leaves undefined; every access here is a relaxed atomic, a word at a time where the address is
//! aligned to one and a byte at a time at the ends, so the copy is defined and only its bytes may
//! be torn. The caller says whether they are good, as a seqlock does: it reads its page's state
//! word before (Acquire), copies, fences (Acquire) and reads the word again, and keeps the copy
//! only when the word did not move.
//!
//! What writes such memory in Rust while a reader may copy it writes through [`zero`] and
//! [`copy_in`], so both sides are atomic: the loader's zeroing of what a short read left of a run.
//! Writes by the kernel (a read completing into the pages, a `madvise` dropping them) are outside
//! the program, and atomic loads observe them as some value.
//!
//! Relaxed loads on x86-64 and arm64 are plain moves, so a copy costs what `copy_from_slice` of
//! the same length does a word at a time; the store's optimistic reads are 16 to 96 bytes.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

const WORD: usize = size_of::<u64>();

/// Bytes from `at` to the next 8-byte boundary, at most `len`.
#[inline]
fn head(at: *const u8, len: usize) -> usize {
    ((WORD - at.addr() % WORD) % WORD).min(len)
}

/// Copies `out.len()` bytes from `from` into `out`, each read with a relaxed atomic load. The
/// bytes are what the memory held at some moment of the copy, each word on its own: torn across
/// words when a writer or the kernel changed it meanwhile, which the caller's check finds.
///
/// # Safety
///
/// `[from, from + out.len())` is mapped readable and writable for the whole call (any page of a
/// sparse [`crate::Region`], given back or not), and every write that may race the copy is atomic
/// ([`zero`], [`copy_in`]) or the kernel's.
#[inline]
pub unsafe fn copy_out(from: *const u8, out: &mut [u8]) {
    let len = out.len();
    let head = head(from, len);
    let mut at = 0;
    while at < head {
        // SAFETY: `from + at` is inside the range the caller says is mapped readable and
        // writable; every racing write to it is atomic or the kernel's.
        out[at] = unsafe { AtomicU8::from_ptr(from.add(at).cast_mut()) }.load(Ordering::Relaxed);
        at += 1;
    }
    while at + WORD <= len {
        // SAFETY: as above, and `from + at` is 8-byte aligned (past `head`), so it is a valid
        // place for an `AtomicU64`.
        let word = unsafe { AtomicU64::from_ptr(from.add(at).cast_mut().cast::<u64>()) }
            .load(Ordering::Relaxed);
        out[at..at + WORD].copy_from_slice(&word.to_ne_bytes());
        at += WORD;
    }
    while at < len {
        // SAFETY: as for the head.
        out[at] = unsafe { AtomicU8::from_ptr(from.add(at).cast_mut()) }.load(Ordering::Relaxed);
        at += 1;
    }
}

/// Writes `bytes` at `to` with relaxed atomic stores, so a [`copy_out`] racing it is defined: a
/// loader's write of a page's bytes that it does itself, rather than by a read into the pages.
///
/// # Safety
///
/// `[to, to + bytes.len())` is mapped readable and writable for the whole call, and nothing holds
/// a plain reference to it (a pinned `&[u8]`) meanwhile.
#[inline]
pub unsafe fn copy_in(to: *mut u8, bytes: &[u8]) {
    let len = bytes.len();
    let head = head(to, len);
    let mut at = 0;
    while at < head {
        // SAFETY: `to + at` is inside the range the caller says is mapped and unreferenced.
        unsafe { AtomicU8::from_ptr(to.add(at)) }.store(bytes[at], Ordering::Relaxed);
        at += 1;
    }
    while at + WORD <= len {
        let mut word = [0; WORD];
        word.copy_from_slice(&bytes[at..at + WORD]);
        // SAFETY: as above, and `to + at` is 8-byte aligned (past `head`).
        unsafe { AtomicU64::from_ptr(to.add(at).cast::<u64>()) }
            .store(u64::from_ne_bytes(word), Ordering::Relaxed);
        at += WORD;
    }
    while at < len {
        // SAFETY: as for the head.
        unsafe { AtomicU8::from_ptr(to.add(at)) }.store(bytes[at], Ordering::Relaxed);
        at += 1;
    }
}

/// Zeroes `[to, to + len)` with relaxed atomic stores: what a loader does to the part of a run a
/// short read left, since pages given back on macOS (`MADV_FREE_REUSABLE`) may hold anything.
///
/// # Safety
///
/// As for [`copy_in`].
#[inline]
pub unsafe fn zero(to: *mut u8, len: usize) {
    let head = head(to, len);
    let mut at = 0;
    while at < head {
        // SAFETY: `to + at` is inside the range the caller says is mapped and unreferenced.
        unsafe { AtomicU8::from_ptr(to.add(at)) }.store(0, Ordering::Relaxed);
        at += 1;
    }
    while at + WORD <= len {
        // SAFETY: as above, and `to + at` is 8-byte aligned (past `head`).
        unsafe { AtomicU64::from_ptr(to.add(at).cast::<u64>()) }.store(0, Ordering::Relaxed);
        at += WORD;
    }
    while at < len {
        // SAFETY: as for the head.
        unsafe { AtomicU8::from_ptr(to.add(at)) }.store(0, Ordering::Relaxed);
        at += 1;
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use hegel::{TestCase, generators as gs};

    use super::*;

    /// Bytes written with [`copy_in`] at any offset and length come back whole through
    /// [`copy_out`], the head, words and tail each where the address puts them, and nothing
    /// outside the range moves; [`zero`] clears exactly its range.
    #[hegel::test]
    fn copies_round_trip_at_any_alignment(tc: TestCase) {
        let offset = tc.draw(gs::integers::<usize>().max_value(15));
        let bytes: Vec<u8> = tc.draw(gs::vecs(gs::integers::<u8>()).max_size(80));
        let mut buffer = [0xEEu8; 112];
        let at = buffer.as_mut_ptr().wrapping_add(offset);
        // SAFETY: `[at, at + bytes.len())` is inside `buffer` (16 + 80 <= 112), which only this
        // test reaches, through `at` alone until the copies end.
        unsafe { copy_in(at, &bytes) };
        let mut out = vec![0u8; bytes.len()]; // alloc: a test's copy
        // SAFETY: as above.
        unsafe { copy_out(at, &mut out) };
        assert_eq!(out, bytes);
        let end = offset + bytes.len();
        assert!(buffer[..offset].iter().chain(&buffer[end..]).all(|&it| it == 0xEE), "outside moved");
        let at = buffer.as_mut_ptr().wrapping_add(offset);
        // SAFETY: as above.
        unsafe { zero(at, bytes.len()) };
        assert!(buffer[offset..end].iter().all(|&it| it == 0), "zeroed");
        assert!(buffer[..offset].iter().chain(&buffer[end..]).all(|&it| it == 0xEE), "outside moved");
    }
}

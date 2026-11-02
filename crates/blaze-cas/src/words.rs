//! Atomics laid end to end, made a stretch at a time and all zero when made: the slots and flag
//! words of the store's in-memory tables (`table.rs`, `flags.rs`).
//!
//! Outside a model they sit on a [`Region`] reserved for the most the table can ever hold and
//! tagged [`Tag::Store`]. Making a stretch commits its pages, which the kernel hands out zeroed,
//! so growing calls no allocator, copies nothing and moves nothing: a `&A` taken before stays
//! valid. Under `cfg(blaze_check)` the model checker's atomics are not plain zero bytes, so each
//! stretch is a boxed slice of them instead, made once.

use blaze_mem::MemError;
#[cfg(not(blaze_check))]
use blaze_mem::{Region, Tag};

use crate::sync::atomic::{AtomicPtr, AtomicU64};

/// The stretches one [`Words`] can have: a table's levels.
pub(crate) const STRETCHES: usize = 40;

/// Stops on memory the store cannot have: it names the tag and the sizes (no-fallback).
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn stop(error: MemError) -> ! {
    panic!("{error}")
}

/// An atomic that starts at zero (null for a pointer).
///
/// # Safety
///
/// Outside a model, its all-zero bytes are a valid value of it, equal to [`Zeroed::zero`].
pub(crate) unsafe trait Zeroed: Sized {
    fn zero() -> Self;
}

// SAFETY: `AtomicPtr<T>` has the in-memory representation of `*mut T`, whose all-zero bytes are
// the null pointer.
unsafe impl<T> Zeroed for AtomicPtr<T> {
    fn zero() -> Self {
        AtomicPtr::new(std::ptr::null_mut())
    }
}

// SAFETY: `AtomicU64` has the in-memory representation of `u64`, whose all-zero bytes are 0.
unsafe impl Zeroed for AtomicU64 {
    fn zero() -> Self {
        AtomicU64::new(0)
    }
}

/// Room for up to `len` atomics. A stretch `[at, at + len)` is made once, with [`Words::make`],
/// and published by the caller (a `OnceLock`) before any [`Words::get`] of it.
#[cfg(not(blaze_check))]
pub(crate) struct Words<A> {
    region: Region,
    len: usize,
    _of: std::marker::PhantomData<A>,
}

#[cfg(not(blaze_check))]
impl<A: Zeroed> Words<A> {
    const SIZE: usize = {
        assert!(std::mem::size_of::<A>() != 0 && std::mem::align_of::<A>() <= std::mem::size_of::<A>());
        std::mem::size_of::<A>()
    };

    /// Reserves room for `len` atomics, committing none.
    pub(crate) fn new(len: usize) -> Words<A> {
        let bytes = len.checked_mul(Self::SIZE).unwrap_or_else(|| {
            stop(MemError::TooLarge { tag: Tag::Store, what: "a table's words", wanted: len })
        });
        match Region::reserve_huge(bytes, Tag::Store) {
            Ok(region) => Words { region, len, _of: std::marker::PhantomData },
            Err(error) => stop(error),
        }
    }

    /// Makes `[at, at + len)` readable, every atomic zero; `stretch` names it under a model.
    pub(crate) fn make(&self, _stretch: usize, at: usize, len: usize) {
        let end = at.checked_add(len).filter(|&end| end <= self.len).unwrap_or_else(|| {
            stop(MemError::Exhausted {
                tag: Tag::Store,
                reserved: self.region.reserved(),
                wanted: at.saturating_add(len).saturating_mul(Self::SIZE),
            })
        });
        if let Err(error) = self.region.ensure(end * Self::SIZE) {
            stop(error)
        }
    }

    /// The stretch `[at, at + len)`, made before.
    #[inline]
    pub(crate) fn get(&self, _stretch: usize, at: usize, len: usize) -> &[A] {
        debug_assert!(at + len <= self.len && (at + len) * Self::SIZE <= self.region.committed());
        let start = self.region.at(at * Self::SIZE).cast::<A>();
        // SAFETY: `make` committed `[at, at + len)` before the caller published that it did, so
        // this load of the stretch is ordered after it; committed pages are zeroed by the kernel
        // and stay committed for the region's life (nothing here decommits); all-zero bytes are a
        // valid `A` (`Zeroed`); the address is aligned (a page-aligned base plus a multiple of
        // `size_of::<A>()`, which `align_of::<A>()` divides); and the atomics are reached only
        // through shared references, as atomics are meant to be.
        unsafe { std::slice::from_raw_parts(start.as_ptr(), len) }
    }
}

/// Under a model: each stretch a boxed slice of the checker's atomics.
#[cfg(blaze_check)]
pub(crate) struct Words<A> {
    stretches: [crate::sync::OnceLock<Box<[A]>>; STRETCHES],
}

#[cfg(blaze_check)]
impl<A: Zeroed> Words<A> {
    pub(crate) fn new(_len: usize) -> Words<A> {
        Words { stretches: [const { crate::sync::OnceLock::new() }; STRETCHES] }
    }

    pub(crate) fn make(&self, stretch: usize, _at: usize, len: usize) {
        // alloc: a model's stretch, made once
        _ = self.stretches[stretch].set((0..len).map(|_| A::zero()).collect());
    }

    pub(crate) fn get(&self, stretch: usize, _at: usize, len: usize) -> &[A] {
        let words = self.stretches[stretch].get().expect("a stretch is made before it is read");
        debug_assert_eq!(words.len(), len);
        words
    }
}

#[cfg(test)]
#[cfg(not(blaze_check))]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    /// Stretches made one after another past several commit steps read as a `Vec` of the same
    /// words does, and a word set in an early stretch keeps its value as later ones are made.
    #[test]
    fn stretches_read_as_a_vec_across_growth() {
        let words: Words<AtomicU64> = Words::new(1 << 20);
        let mut model: Vec<u64> = Vec::new();
        let (mut at, mut len) = (0, 64);
        for stretch in 0..14 {
            words.make(stretch, at, len);
            model.resize(at + len, 0);
            let got = words.get(stretch, at, len);
            assert!(got.iter().all(|it| it.load(Ordering::Relaxed) == 0), "a new stretch is zero");
            for (n, word) in got.iter().enumerate().step_by(7) {
                word.store((at + n) as u64 * 3 + 1, Ordering::Relaxed);
                model[at + n] = (at + n) as u64 * 3 + 1;
            }
            at += len;
            len *= 2;
        }
        let (mut at, mut len) = (0, 64);
        for stretch in 0..14 {
            let got: Vec<u64> = words.get(stretch, at, len).iter().map(|it| it.load(Ordering::Relaxed)).collect();
            assert_eq!(got, model[at..at + len], "stretch {stretch}");
            at += len;
            len *= 2;
        }
    }

    #[test]
    fn pointers_start_null() {
        let words: Words<AtomicPtr<u64>> = Words::new(4096);
        words.make(0, 0, 4096);
        assert!(words.get(0, 0, 4096).iter().all(|it| it.load(Ordering::Relaxed).is_null()));
    }

    #[test]
    #[should_panic(expected = "store")]
    fn a_stretch_past_the_reservation_stops_naming_the_store() {
        let words: Words<AtomicU64> = Words::new(64);
        words.make(0, 0, 128);
    }
}

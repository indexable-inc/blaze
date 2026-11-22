//! [`Packed<T, A, K>`]: a sequence of values of different sizes packed end to end in one byte
//! buffer, each taking its own size (after Goose's variable-mode enums). It can be iterated but
//! not indexed: for token streams and instruction lists, where an enum's widest variant would
//! otherwise set every element's size.
//!
//! A value goes in through [`Pack`]: its size, how it is written, and how it is read back with
//! the bytes it took. With `K > 0` a side index holds the byte offset of every `K`-th value, so
//! [`Packed::seek`] starts at the checkpoint below and reads at most `K - 1` values to reach the
//! one asked; the index costs one word per `K` values. With `K = 0` (the default) there is no
//! index, and a seek reads from the front.
//!
//! No `unsafe`: values are written into bytes the buffer already holds and read back by
//! `Pack::unpack`, a safe function. A `Pack` impl that reads back another length than it
//! wrote is a bug that stops with an error naming the type, never a read out of bounds.

use core::{alloc::Allocator, any::type_name, fmt, iter::FusedIterator, marker::PhantomData};

use crate::{
    error::{AllocFailure, fail},
    vec::Vec,
};

/// A value that packs into bytes of its own length.
pub trait Pack: Sized {
    /// The bytes [`Pack::pack`] writes for this value.
    fn packed_len(&self) -> usize;
    /// Writes the value into `out`, exactly [`Pack::packed_len`] bytes.
    fn pack(&self, out: &mut [u8]);
    /// Reads a value from the front of `bytes`, which start with what [`Pack::pack`] wrote
    /// (and may go on past it): the value and how many bytes it took, its `packed_len`.
    fn unpack(bytes: &[u8]) -> (Self, usize);
}

macro_rules! pack_int {
    ($($t:ty),+) => {$(
        /// Little-endian, its full width.
        impl Pack for $t {
            #[inline]
            fn packed_len(&self) -> usize {
                size_of::<$t>()
            }
            #[inline]
            fn pack(&self, out: &mut [u8]) {
                out.copy_from_slice(&self.to_le_bytes());
            }
            #[inline]
            fn unpack(bytes: &[u8]) -> ($t, usize) {
                let mut word = [0; size_of::<$t>()];
                word.copy_from_slice(&bytes[..size_of::<$t>()]);
                (<$t>::from_le_bytes(word), size_of::<$t>())
            }
        }
    )+};
}

pack_int!(u8, u16, u32, u64, i8, i16, i32, i64);

/// Values of `T` packed end to end in `A`, with a seek checkpoint every `K` values (none when
/// `K` is 0).
pub struct Packed<T: Pack, A: Allocator, const K: usize = 0> {
    bytes: Vec<u8, A>,
    /// The byte offset of values `0, K, 2K, ...`; empty when `K` is 0.
    index: Vec<usize, A>,
    len: usize,
    _values: PhantomData<fn() -> T>,
}

impl<T: Pack, A: Allocator + Clone, const K: usize> Packed<T, A, K> {
    /// Empty in `alloc`; nothing is allocated until a value is pushed.
    pub fn new_in(alloc: A) -> Packed<T, A, K> {
        Packed { bytes: Vec::new_in(alloc.clone()), index: Vec::new_in(alloc), len: 0, _values: PhantomData }
    }

    /// Empty in `alloc`, with room for `bytes` bytes of values (and their checkpoints).
    pub fn try_with_capacity_in(bytes: usize, values: usize, alloc: A) -> Result<Packed<T, A, K>, AllocFailure> {
        let mut out = Packed::new_in(alloc);
        out.bytes.try_reserve_exact(bytes)?;
        if K > 0 {
            out.index.try_reserve_exact(values.div_ceil(K))?;
        }
        Ok(out)
    }
}

impl<T: Pack, A: Allocator, const K: usize> Packed<T, A, K> {
    /// The values it holds.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes the values take, checkpoints not counted.
    #[inline]
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// The packed bytes, values end to end.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// [`Packed::try_push`], panicking with the [`AllocFailure`].
    pub fn push(&mut self, value: &T) {
        if let Err(failure) = self.try_push(value) {
            fail(failure);
        }
    }

    /// `value` at the back, or why there is no room; on a refusal nothing changed.
    pub fn try_push(&mut self, value: &T) -> Result<(), AllocFailure> {
        let size = value.packed_len();
        let checkpoint = K > 0 && self.len.is_multiple_of(K);
        // Both reservations before any change, so a refusal leaves it as it was.
        self.bytes.try_reserve(size)?;
        if checkpoint {
            self.index.try_reserve(1)?;
        }
        let start = self.bytes.len();
        if checkpoint {
            self.index.push(start);
        }
        // Within the reserved capacity: no allocation.
        self.bytes.resize(start + size, 0);
        value.pack(&mut self.bytes.as_mut_slice()[start..]);
        self.len += 1;
        Ok(())
    }

    /// Empties it, keeping its capacity.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.index.clear();
        self.len = 0;
    }

    /// The values from the front.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter { rest: self.bytes.as_slice(), left: self.len, _values: PhantomData }
    }

    /// The values from the one at `at` on: from the checkpoint below it, `at % K` values read
    /// and passed over (all `at` of them when `K` is 0). `None` when `at > len`.
    pub fn seek(&self, at: usize) -> Option<Iter<'_, T>> {
        if at > self.len {
            return None;
        }
        let (start, skip) = match K {
            0 => (0, at),
            _ => match self.index.as_slice().get(at / K) {
                Some(&start) => (start, at % K),
                // `at == len` on a checkpoint not yet made: the end.
                None => (self.bytes.len(), 0),
            },
        };
        let first = at - skip;
        let mut iter = Iter { rest: &self.bytes.as_slice()[start..], left: self.len - first, _values: PhantomData };
        for _ in 0..skip {
            let _passed = iter.next();
        }
        Some(iter)
    }

    /// A copy in another allocator: the bytes and the checkpoints, each in a block of exactly
    /// its length in `alloc`. Packed values own no memory, so nothing else is copied.
    pub fn try_clone_in<B: Allocator + Clone>(&self, alloc: B) -> Result<Packed<T, B, K>, AllocFailure> {
        let bytes = self.bytes.try_clone_in(alloc.clone())?;
        let index = self.index.try_clone_in(alloc)?;
        Ok(Packed { bytes, index, len: self.len, _values: PhantomData })
    }

    /// [`Packed::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator + Clone>(&self, alloc: B) -> Packed<T, B, K> {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<T: Pack + fmt::Debug, A: Allocator, const K: usize> fmt::Debug for Packed<T, A, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a, T: Pack, A: Allocator, const K: usize> IntoIterator for &'a Packed<T, A, K> {
    type Item = T;
    type IntoIter = Iter<'a, T>;
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

/// The values of a [`Packed`], read one by one.
pub struct Iter<'a, T: Pack> {
    rest: &'a [u8],
    left: usize,
    _values: PhantomData<fn() -> T>,
}

impl<T: Pack> Clone for Iter<'_, T> {
    fn clone(&self) -> Self {
        Iter { rest: self.rest, left: self.left, _values: PhantomData }
    }
}

impl<T: Pack> Iterator for Iter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.left == 0 {
            return None;
        }
        let (value, taken) = T::unpack(self.rest);
        let Some(rest) = self.rest.get(taken..) else {
            panic!(
                "blaze_own::Packed: `<{} as Pack>::unpack` took {taken} bytes of the {} left: it reads back another length than `pack` wrote",
                type_name::<T>(),
                self.rest.len(),
            );
        };
        self.rest = rest;
        self.left -= 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl<T: Pack> ExactSizeIterator for Iter<'_, T> {}

impl<T: Pack> FusedIterator for Iter<'_, T> {}

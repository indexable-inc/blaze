//! The two things the portable vector code needs beyond `core::simd`'s operators: a 16-entry
//! table lookup per lane, and the previous one, two or three bytes of a stream as a vector.

use core::simd::{Simd, Swizzle};

/// A table lookup of every lane: lane `i` of the result is `table[idx[i]]`, where `table` holds
/// one 16-entry table repeated in every 16 lanes and every index is below 16.
///
/// A strategy type (rule 78): [`Portable`] is `swizzle_dyn`, the reference; [`Avx2`] is the
/// one form `core::simd` cannot say inside a `#[target_feature]` function.
pub(crate) trait Lookup<const N: usize>: Copy {
    fn lookup(self, table: Simd<u8, N>, idx: Simd<u8, N>) -> Simd<u8, N>;
}

/// `swizzle_dyn`: `tbl` on aarch64, `pshufb` where the crate is built with SSSE3, a lane at a
/// time elsewhere.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Portable;

impl<const N: usize> Lookup<N> for Portable {
    #[inline(always)]
    fn lookup(self, table: Simd<u8, N>, idx: Simd<u8, N>) -> Simd<u8, N> {
        table.swizzle_dyn(idx)
    }
}

/// The proof that the CPU has AVX2: made only by [`Avx2::new_unchecked`].
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Avx2(());

#[cfg(target_arch = "x86_64")]
impl Avx2 {
    /// # Safety
    ///
    /// The CPU every use of the value runs on has AVX2.
    pub(crate) const unsafe fn new_unchecked() -> Avx2 {
        Avx2(())
    }
}

#[cfg(target_arch = "x86_64")]
impl Lookup<32> for Avx2 {
    #[inline(always)]
    fn lookup(self, table: Simd<u8, 32>, idx: Simd<u8, 32>) -> Simd<u8, 32> {
        use core::arch::x86_64::{__m256i, _mm256_shuffle_epi8};
        let table: __m256i = table.into();
        let idx: __m256i = idx.into();
        // SAFETY: `vpshufb` needs AVX2, and `self` is an `Avx2`, which exists only where its
        // maker promised the CPU has it. Within each 128-bit half it is `table[idx]` for indices
        // below 16, and `table` repeats its 16 entries in both halves, so it is the lookup.
        let found = unsafe { _mm256_shuffle_epi8(table, idx) };
        Simd::from(found)
    }
}

/// A 16-entry table repeated across `N` lanes, for [`Lookup`].
#[inline(always)]
pub(crate) fn table<const N: usize>(entries: &[u8; 16]) -> Simd<u8, N> {
    Simd::from_array(core::array::from_fn(|lane| entries[lane % 16]))
}

/// The stream shifted by `K` lanes: lane `i` of `concat_swizzle(prev, cur)` is `cur[i - K]`,
/// and `prev`'s last `K` lanes for the first `K`.
pub(crate) struct Back<const K: usize>;

impl<const N: usize, const K: usize> Swizzle<N> for Back<K> {
    const INDEX: [usize; N] = {
        let mut index = [0; N];
        let mut lane = 0;
        while lane < N {
            index[lane] = N - K + lane;
            lane += 1;
        }
        index
    };
}

/// `cur` with the last `K` bytes of `prev` in front: each lane holds the byte `K` before it.
#[inline(always)]
pub(crate) fn back<const K: usize, const N: usize>(prev: Simd<u8, N>, cur: Simd<u8, N>) -> Simd<u8, N> {
    <Back<K> as Swizzle<N>>::concat_swizzle(prev, cur)
}

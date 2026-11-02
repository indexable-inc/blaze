//! The newline bitmap of a text: bit `i % 64` of word `i / 64` is set when byte `i` is `\n`,
//! made 64 bytes at a time with the widest vector unit the CPU has (AVX-512BW, AVX2, SSE2 or
//! NEON), chosen at run time. Every [`Kernel`] gives the words [`Kernel::Scalar`] gives, so the
//! choice is dispatch: `every_kernel_gives_the_scalar_bits` checks each against it.
//!
//! The crate's unsafe module for its line index (docs/v2/plan/design/unsafe.md): the vector
//! loads and the calls of `target_feature` functions are unsafe. The safe version is
//! [`fill_scalar`], a compare and shift per byte; a vector path does 64 bytes in one to four
//! compares and a mask move, so building is bound by memory bandwidth.

/// A way to make the bitmap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kernel {
    /// A compare and shift per byte: the reference every other kernel is checked against.
    Scalar,
    /// Four 16-byte compares per 64 bytes (x86-64's baseline).
    Sse2,
    /// Two 32-byte compares per 64 bytes.
    Avx2,
    /// One 64-byte compare into a mask register.
    Avx512,
    /// Four 16-byte compares, folded to 64 bits with three pairwise adds (aarch64).
    Neon,
}

impl Kernel {
    pub const ALL: [Kernel; 5] =
        [Kernel::Scalar, Kernel::Sse2, Kernel::Avx2, Kernel::Avx512, Kernel::Neon];

    /// Whether this CPU runs it.
    pub fn available(self) -> bool {
        match self {
            Kernel::Scalar => true,
            #[cfg(target_arch = "x86_64")]
            Kernel::Sse2 => is_x86_feature_detected!("sse2"),
            #[cfg(target_arch = "x86_64")]
            Kernel::Avx2 => is_x86_feature_detected!("avx2"),
            #[cfg(target_arch = "x86_64")]
            Kernel::Avx512 => is_x86_feature_detected!("avx512bw"),
            #[cfg(target_arch = "aarch64")]
            Kernel::Neon => std::arch::is_aarch64_feature_detected!("neon"),
            _ => false,
        }
    }

    /// The widest kernel this CPU runs. Feature detection is a load std caches.
    pub fn best() -> Kernel {
        [Kernel::Avx512, Kernel::Avx2, Kernel::Sse2, Kernel::Neon]
            .into_iter()
            .find(|it| it.available())
            .unwrap_or(Kernel::Scalar)
    }
}

/// Writes the bitmap of `bytes` into `bits`, which has a word per 64 bytes or part of them.
/// Panics when this CPU can't run `kernel`.
pub(super) fn fill(kernel: Kernel, bytes: &[u8], bits: &mut [u64]) {
    assert!(kernel.available(), "the {kernel:?} kernel on a CPU without it");
    assert_eq!(bits.len(), bytes.len().div_ceil(64), "a word per 64 bytes");
    let (chunks, tail) = bytes.as_chunks::<64>();
    let (whole, last) = bits.split_at_mut(chunks.len());
    match kernel {
        Kernel::Scalar => fill_scalar(chunks, whole),
        #[cfg(target_arch = "x86_64")]
        Kernel::Sse2 => {
            // SAFETY: `kernel.available()` was asserted above, so the CPU has `sse2`.
            unsafe { fill_sse2(chunks, whole) }
        }
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx2 => {
            // SAFETY: `kernel.available()` was asserted above, so the CPU has `avx2`.
            unsafe { fill_avx2(chunks, whole) }
        }
        #[cfg(target_arch = "x86_64")]
        Kernel::Avx512 => {
            // SAFETY: `kernel.available()` was asserted above, so the CPU has `avx512bw`.
            unsafe { fill_avx512(chunks, whole) }
        }
        #[cfg(target_arch = "aarch64")]
        Kernel::Neon => {
            // SAFETY: `kernel.available()` was asserted above, so the CPU has `neon`.
            unsafe { fill_neon(chunks, whole) }
        }
        _ => unreachable!("{kernel:?} is available only where it is compiled"),
    }
    if let Some(word) = last.first_mut() {
        *word = mask_scalar(tail);
    }
}

/// The bits of 64 bytes or fewer, one byte at a time.
fn mask_scalar(bytes: &[u8]) -> u64 {
    debug_assert!(bytes.len() <= 64);
    let mut mask = 0u64;
    for (at, &byte) in bytes.iter().enumerate() {
        mask |= u64::from(byte == b'\n') << at;
    }
    mask
}

fn fill_scalar(chunks: &[[u8; 64]], bits: &mut [u64]) {
    for (word, chunk) in bits.iter_mut().zip(chunks) {
        *word = mask_scalar(chunk);
    }
}

/// [`fill_scalar`] on SSE2, 16 bytes per compare.
///
/// # Safety
///
/// The CPU has `sse2` (the caller checked [`Kernel::available`]); without it the instructions
/// fault.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn fill_sse2(chunks: &[[u8; 64]], bits: &mut [u64]) {
    use std::arch::x86_64::*;
    let newline = _mm_set1_epi8(b'\n' as i8);
    for (word, chunk) in bits.iter_mut().zip(chunks) {
        let mut mask = 0u64;
        for (at, lane) in chunk.as_chunks::<16>().0.iter().enumerate() {
            // SAFETY: `lane` is 16 readable bytes, the load's width, and `loadu` has no
            // alignment requirement.
            let v = unsafe { _mm_loadu_si128(lane.as_ptr().cast()) };
            let hits = _mm_movemask_epi8(_mm_cmpeq_epi8(v, newline)) as u16;
            mask |= u64::from(hits) << (16 * at);
        }
        *word = mask;
    }
}

/// [`fill_scalar`] on AVX2, 32 bytes per compare.
///
/// # Safety
///
/// The CPU has `avx2` (the caller checked [`Kernel::available`]); without it the instructions
/// fault.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn fill_avx2(chunks: &[[u8; 64]], bits: &mut [u64]) {
    use std::arch::x86_64::*;
    let newline = _mm256_set1_epi8(b'\n' as i8);
    for (word, chunk) in bits.iter_mut().zip(chunks) {
        let (low, high) = chunk.split_at(32);
        // SAFETY: `low` is 32 readable bytes (the chunk's first half), the load's width, and
        // `loadu` has no alignment requirement.
        let low = unsafe { _mm256_loadu_si256(low.as_ptr().cast()) };
        // SAFETY: as for `low`: `high` is the chunk's other 32 bytes.
        let high = unsafe { _mm256_loadu_si256(high.as_ptr().cast()) };
        let low = _mm256_movemask_epi8(_mm256_cmpeq_epi8(low, newline)) as u32;
        let high = _mm256_movemask_epi8(_mm256_cmpeq_epi8(high, newline)) as u32;
        *word = u64::from(low) | u64::from(high) << 32;
    }
}

/// [`fill_scalar`] on AVX-512BW, 64 bytes per compare.
///
/// # Safety
///
/// The CPU has `avx512bw` (the caller checked [`Kernel::available`]); without it the
/// instructions fault.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw")]
unsafe fn fill_avx512(chunks: &[[u8; 64]], bits: &mut [u64]) {
    use std::arch::x86_64::*;
    let newline = _mm512_set1_epi8(b'\n' as i8);
    for (word, chunk) in bits.iter_mut().zip(chunks) {
        // SAFETY: `chunk` is 64 readable bytes, exactly the load's width, and `loadu` has no
        // alignment requirement.
        let v = unsafe { _mm512_loadu_si512(chunk.as_ptr().cast()) };
        *word = _mm512_cmpeq_epi8_mask(v, newline);
    }
}

/// 16 bytes compared with `\n`, each hit as its bit of `weights` (bit `i % 8` for byte `i`).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
fn hits_neon(
    lane: &[u8; 16],
    newline: std::arch::aarch64::uint8x16_t,
    weights: std::arch::aarch64::uint8x16_t,
) -> std::arch::aarch64::uint8x16_t {
    use std::arch::aarch64::*;
    // SAFETY: `lane` is 16 readable bytes, the load's width, and `vld1q_u8` has no alignment
    // requirement.
    let v = unsafe { vld1q_u8(lane.as_ptr()) };
    vandq_u8(vceqq_u8(v, newline), weights)
}

/// [`fill_scalar`] on NEON, 16 bytes per compare; three pairwise adds fold the four weighted
/// masks to one byte per 8 bytes of text, byte `i` of the 64 as bit `i`.
///
/// # Safety
///
/// The CPU has `neon` (the caller checked [`Kernel::available`]); without it the instructions
/// fault.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn fill_neon(chunks: &[[u8; 64]], bits: &mut [u64]) {
    use std::arch::aarch64::*;
    const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    // SAFETY: `WEIGHTS` is 16 readable bytes, the load's width, with no alignment requirement.
    let weights = unsafe { vld1q_u8(WEIGHTS.as_ptr()) };
    let newline = vdupq_n_u8(b'\n');
    for (word, chunk) in bits.iter_mut().zip(chunks) {
        let lanes = chunk.as_chunks::<16>().0;
        let a = hits_neon(&lanes[0], newline, weights);
        let b = hits_neon(&lanes[1], newline, weights);
        let c = hits_neon(&lanes[2], newline, weights);
        let d = hits_neon(&lanes[3], newline, weights);
        let abcd = vpaddq_u8(vpaddq_u8(a, b), vpaddq_u8(c, d));
        let abcd = vpaddq_u8(abcd, abcd);
        *word = vgetq_lane_u64::<0>(vreinterpretq_u64_u8(abcd));
    }
}

//! Byte comparison for the oracle (docs/v2/plan/design/hardware.md): where two outputs first
//! differ, 64 bytes at a time with AVX-512BW, 32 with AVX2, 16 with SSE2 or NEON, four vectors
//! per loop and one flag test per four. The path is chosen at run time by CPU feature; every
//! path gives the same answer as [`Simd::Scalar`], so the choice is dispatch, never a different
//! result.
//!
//! Files are compared mapped, in place: no copy into a buffer, no read per megabyte. A short
//! tail is compared by one last vector that overlaps the bytes already found equal, so no path
//! drops to a byte loop except on inputs shorter than one vector.
//!
//! Unsafe is the vector loads (and the map of [`Mapped`]): a load reads the bytes at a pointer
//! the loop bound keeps in both slices; the offsets themselves are safe `wrapping_add`s. The
//! safe version, `==` over fixed pieces (libc's `bcmp`) and then [`scalar`] in the piece that
//! differs, reads that piece twice and pays a call and a byte loop per difference: one pass
//! more over up to a piece, and on short outputs, most of what the oracle compares, the call
//! and the byte loop are the whole cost. `scalar` is the reference every path is tested against
//! (`tests.rs`, and the tests below).

#![allow(unsafe_code)]

use std::{io, path::Path, sync::OnceLock};

/// A way to compare, in the order [`fastest`] prefers them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Simd {
    Avx512,
    Avx2,
    Sse2,
    Neon,
    Scalar,
}

/// The paths this CPU can run, fastest first; [`Simd::Scalar`] is always last.
pub fn available() -> Vec<Simd> {
    // Once per process through `fastest`, and in the tests that run every path.
    let mut paths = Vec::with_capacity(5);
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512bw") {
            paths.push(Simd::Avx512);
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            paths.push(Simd::Avx2);
        }
        if std::arch::is_x86_feature_detected!("sse2") {
            paths.push(Simd::Sse2);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if std::arch::is_aarch64_feature_detected!("neon") {
            paths.push(Simd::Neon);
        }
    }
    paths.push(Simd::Scalar);
    paths
}

/// The first of [`available`], detected once per process.
pub fn fastest() -> Simd {
    static FASTEST: OnceLock<Simd> = OnceLock::new();
    *FASTEST.get_or_init(|| available()[0])
}

/// The first offset at which `a` and `b` differ, or where the shorter ends; `None` when they
/// are equal.
pub fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    first_difference_with(fastest(), a, b)
}

/// [`first_difference`] by `path`, which must be one of [`available`]'s.
pub fn first_difference_with(path: Simd, a: &[u8], b: &[u8]) -> Option<usize> {
    let n = a.len().min(b.len());
    let longer = a.len() != b.len();
    let (a, b) = (&a[..n], &b[..n]);
    let found = match path {
        Simd::Scalar => scalar(a, b, 0),
        #[cfg(target_arch = "x86_64")]
        Simd::Avx512 => {
            assert!(std::arch::is_x86_feature_detected!("avx512bw"), "this CPU has no AVX-512BW");
            // SAFETY: the CPU has AVX-512BW, checked above.
            unsafe { avx512(a, b) }
        }
        #[cfg(target_arch = "x86_64")]
        Simd::Avx2 => {
            assert!(std::arch::is_x86_feature_detected!("avx2"), "this CPU has no AVX2");
            // SAFETY: the CPU has AVX2, checked above.
            unsafe { avx2(a, b) }
        }
        #[cfg(target_arch = "x86_64")]
        Simd::Sse2 => {
            assert!(std::arch::is_x86_feature_detected!("sse2"), "this CPU has no SSE2");
            // SAFETY: the CPU has SSE2, checked above.
            unsafe { sse2(a, b) }
        }
        #[cfg(target_arch = "aarch64")]
        Simd::Neon => {
            assert!(std::arch::is_aarch64_feature_detected!("neon"), "this CPU has no NEON");
            // SAFETY: the CPU has NEON, checked above.
            unsafe { neon(a, b) }
        }
        #[allow(unreachable_patterns)]
        other => panic!("{other:?} is not a path of this architecture"),
    };
    found.or(longer.then_some(n))
}

/// The reference every vector path is tested against.
fn scalar(a: &[u8], b: &[u8], from: usize) -> Option<usize> {
    (from..a.len()).find(|&i| a[i] != b[i])
}

// Each vector path below takes `a` and `b` of one length `n`. It compares four vectors per
// iteration while `4 * W` bytes remain, then one per iteration, then the last `W` bytes of
// the input (overlapping bytes already found equal, so the first difference in it is the
// first of all); inputs shorter than `W` go to `scalar`, or a masked load on AVX-512.

/// # Safety
///
/// The CPU has AVX-512BW, which `first_difference_with` asserts, and `b` is at least as long as
/// `a`: every read is below `a.len()` in both.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw")]
unsafe fn avx512(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{__m512i, _mm512_cmpneq_epi8_mask, _mm512_loadu_si512, _mm512_maskz_loadu_epi8};
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    /// # Safety
    ///
    /// The CPU has AVX-512BW, and `pa` and `pb` each point at 64 readable bytes.
    #[inline]
    #[target_feature(enable = "avx512bw")]
    unsafe fn differ(pa: *const u8, pb: *const u8) -> u64 {
        // SAFETY: the caller has 64 readable bytes at `pa`; the load is unaligned.
        let x = unsafe { _mm512_loadu_si512(pa.cast::<__m512i>()) };
        // SAFETY: as above, at `pb`.
        let y = unsafe { _mm512_loadu_si512(pb.cast::<__m512i>()) };
        _mm512_cmpneq_epi8_mask(x, y)
    }
    if n < 64 {
        let keep = (1u64 << n) - 1;
        // SAFETY: the masked load touches only the lanes of `keep`, the first `n` bytes of `a`,
        // and reads zero in the others.
        let x = unsafe { _mm512_maskz_loadu_epi8(keep, pa.cast()) };
        // SAFETY: as above, for `b`, as long as `a`.
        let y = unsafe { _mm512_maskz_loadu_epi8(keep, pb.cast()) };
        let differ = _mm512_cmpneq_epi8_mask(x, y);
        return (differ != 0).then(|| differ.trailing_zeros() as usize);
    }
    let mut at = 0;
    while at + 256 <= n {
        // `at + 256 <= n`, the length of both slices, so `at` is inside both.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 64 bytes from each end at or before `at + 256 <= n`.
        let m0 = unsafe { differ(a0, b0) };
        let (a1, b1) = (pa.wrapping_add(at + 64), pb.wrapping_add(at + 64));
        // SAFETY: as above.
        let m1 = unsafe { differ(a1, b1) };
        let (a2, b2) = (pa.wrapping_add(at + 128), pb.wrapping_add(at + 128));
        // SAFETY: as above.
        let m2 = unsafe { differ(a2, b2) };
        let (a3, b3) = (pa.wrapping_add(at + 192), pb.wrapping_add(at + 192));
        // SAFETY: as above.
        let m3 = unsafe { differ(a3, b3) };
        let m = [m0, m1, m2, m3];
        if m[0] | m[1] | m[2] | m[3] != 0 {
            let k = m.iter().position(|&w| w != 0).expect("one of the four differs");
            return Some(at + 64 * k + m[k].trailing_zeros() as usize);
        }
        at += 256;
    }
    while at + 64 <= n {
        // `at + 64 <= n`, so `at` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 64 bytes from each end at or before `n`.
        let m = unsafe { differ(a0, b0) };
        if m != 0 {
            return Some(at + m.trailing_zeros() as usize);
        }
        at += 64;
    }
    if at < n {
        let last = n - 64;
        // `n >= 64`, so `last + 64 == n` and `last` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(last), pb.wrapping_add(last));
        // SAFETY: the 64 bytes from each end at `n`.
        let m = unsafe { differ(a0, b0) };
        if m != 0 {
            return Some(last + m.trailing_zeros() as usize);
        }
    }
    None
}

/// # Safety
///
/// The CPU has AVX2, which `first_difference_with` asserts, and `b` is at least as long as
/// `a`: every read is below `a.len()` in both.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{__m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8};
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    /// A bit per byte, set where the bytes are equal.
    ///
    /// # Safety
    ///
    /// The CPU has AVX2, and `pa` and `pb` each point at 32 readable bytes.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn same(pa: *const u8, pb: *const u8) -> u32 {
        // SAFETY: the caller has 32 readable bytes at `pa`; the load is unaligned.
        let x = unsafe { _mm256_loadu_si256(pa.cast::<__m256i>()) };
        // SAFETY: as above, at `pb`.
        let y = unsafe { _mm256_loadu_si256(pb.cast::<__m256i>()) };
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(x, y)) as u32
    }
    if n < 32 {
        return scalar(a, b, 0);
    }
    let mut at = 0;
    while at + 128 <= n {
        // `at + 128 <= n`, the length of both slices, so `at` is inside both.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 32 bytes from each end at or before `at + 128 <= n`.
        let m0 = unsafe { same(a0, b0) };
        let (a1, b1) = (pa.wrapping_add(at + 32), pb.wrapping_add(at + 32));
        // SAFETY: as above.
        let m1 = unsafe { same(a1, b1) };
        let (a2, b2) = (pa.wrapping_add(at + 64), pb.wrapping_add(at + 64));
        // SAFETY: as above.
        let m2 = unsafe { same(a2, b2) };
        let (a3, b3) = (pa.wrapping_add(at + 96), pb.wrapping_add(at + 96));
        // SAFETY: as above.
        let m3 = unsafe { same(a3, b3) };
        let m = [m0, m1, m2, m3];
        if m[0] & m[1] & m[2] & m[3] != u32::MAX {
            let all = u128::from(m[0]) | (u128::from(m[1]) << 32) | (u128::from(m[2]) << 64) | (u128::from(m[3]) << 96);
            return Some(at + (!all).trailing_zeros() as usize);
        }
        at += 128;
    }
    while at + 32 <= n {
        // `at + 32 <= n`, so `at` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 32 bytes from each end at or before `n`.
        let m = unsafe { same(a0, b0) };
        if m != u32::MAX {
            return Some(at + (!m).trailing_zeros() as usize);
        }
        at += 32;
    }
    if at < n {
        let last = n - 32;
        // `n >= 32`, so `last + 32 == n` and `last` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(last), pb.wrapping_add(last));
        // SAFETY: the 32 bytes from each end at `n`.
        let m = unsafe { same(a0, b0) };
        if m != u32::MAX {
            return Some(last + (!m).trailing_zeros() as usize);
        }
    }
    None
}

/// # Safety
///
/// The CPU has SSE2, which `first_difference_with` asserts, and `b` is at least as long as
/// `a`: every read is below `a.len()` in both.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn sse2(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::x86_64::{__m128i, _mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    /// A bit per byte in the low 16, set where the bytes are equal.
    ///
    /// # Safety
    ///
    /// The CPU has SSE2, and `pa` and `pb` each point at 16 readable bytes.
    #[inline]
    #[target_feature(enable = "sse2")]
    unsafe fn same(pa: *const u8, pb: *const u8) -> u16 {
        // SAFETY: the caller has 16 readable bytes at `pa`; the load is unaligned.
        let x = unsafe { _mm_loadu_si128(pa.cast::<__m128i>()) };
        // SAFETY: as above, at `pb`.
        let y = unsafe { _mm_loadu_si128(pb.cast::<__m128i>()) };
        _mm_movemask_epi8(_mm_cmpeq_epi8(x, y)) as u16
    }
    if n < 16 {
        return scalar(a, b, 0);
    }
    let mut at = 0;
    while at + 64 <= n {
        // `at + 64 <= n`, the length of both slices, so `at` is inside both.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 16 bytes from each end at or before `at + 64 <= n`.
        let m0 = unsafe { same(a0, b0) };
        let (a1, b1) = (pa.wrapping_add(at + 16), pb.wrapping_add(at + 16));
        // SAFETY: as above.
        let m1 = unsafe { same(a1, b1) };
        let (a2, b2) = (pa.wrapping_add(at + 32), pb.wrapping_add(at + 32));
        // SAFETY: as above.
        let m2 = unsafe { same(a2, b2) };
        let (a3, b3) = (pa.wrapping_add(at + 48), pb.wrapping_add(at + 48));
        // SAFETY: as above.
        let m3 = unsafe { same(a3, b3) };
        let m = [m0, m1, m2, m3];
        if m[0] & m[1] & m[2] & m[3] != u16::MAX {
            let all = u64::from(m[0]) | (u64::from(m[1]) << 16) | (u64::from(m[2]) << 32) | (u64::from(m[3]) << 48);
            return Some(at + (!all).trailing_zeros() as usize);
        }
        at += 64;
    }
    while at + 16 <= n {
        // `at + 16 <= n`, so `at` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 16 bytes from each end at or before `n`.
        let m = unsafe { same(a0, b0) };
        if m != u16::MAX {
            return Some(at + (!m).trailing_zeros() as usize);
        }
        at += 16;
    }
    if at < n {
        let last = n - 16;
        // `n >= 16`, so `last + 16 == n` and `last` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(last), pb.wrapping_add(last));
        // SAFETY: the 16 bytes from each end at `n`.
        let m = unsafe { same(a0, b0) };
        if m != u16::MAX {
            return Some(last + (!m).trailing_zeros() as usize);
        }
    }
    None
}

/// # Safety
///
/// The CPU has NEON, which `first_difference_with` asserts, and `b` is at least as long as
/// `a`: every read is below `a.len()` in both.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn neon(a: &[u8], b: &[u8]) -> Option<usize> {
    use std::arch::aarch64::{
        uint8x16_t, vandq_u8, vceqq_u8, vget_lane_u64, vld1q_u8, vreinterpret_u64_u8, vreinterpretq_u16_u8, vshrn_n_u16,
    };
    let (n, pa, pb) = (a.len(), a.as_ptr(), b.as_ptr());
    /// # Safety
    ///
    /// The CPU has NEON, and `pa` and `pb` each point at 16 readable bytes.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn eq(pa: *const u8, pb: *const u8) -> uint8x16_t {
        // SAFETY: the caller has 16 readable bytes at `pa`; `vld1q_u8` has no alignment
        // requirement.
        let x = unsafe { vld1q_u8(pa) };
        // SAFETY: as above, at `pb`.
        let y = unsafe { vld1q_u8(pb) };
        vceqq_u8(x, y)
    }
    /// Four bits per byte (shift right and narrow), all set where the bytes are equal: the
    /// movemask NEON lacks, one instruction.
    #[inline]
    #[target_feature(enable = "neon")]
    fn same(eq: uint8x16_t) -> u64 {
        vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(vreinterpretq_u16_u8(eq))))
    }
    if n < 16 {
        return scalar(a, b, 0);
    }
    let mut at = 0;
    while at + 64 <= n {
        // `at + 64 <= n`, the length of both slices, so `at` is inside both.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 16 bytes from each end at or before `at + 64 <= n`.
        let e0 = unsafe { eq(a0, b0) };
        let (a1, b1) = (pa.wrapping_add(at + 16), pb.wrapping_add(at + 16));
        // SAFETY: as above.
        let e1 = unsafe { eq(a1, b1) };
        let (a2, b2) = (pa.wrapping_add(at + 32), pb.wrapping_add(at + 32));
        // SAFETY: as above.
        let e2 = unsafe { eq(a2, b2) };
        let (a3, b3) = (pa.wrapping_add(at + 48), pb.wrapping_add(at + 48));
        // SAFETY: as above.
        let e3 = unsafe { eq(a3, b3) };
        let e = [e0, e1, e2, e3];
        if same(vandq_u8(vandq_u8(e[0], e[1]), vandq_u8(e[2], e[3]))) != u64::MAX {
            for (k, &e) in e.iter().enumerate() {
                let m = same(e);
                if m != u64::MAX {
                    return Some(at + 16 * k + (!m).trailing_zeros() as usize / 4);
                }
            }
            unreachable!("one of the four vectors differs");
        }
        at += 64;
    }
    while at + 16 <= n {
        // `at + 16 <= n`, so `at` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(at), pb.wrapping_add(at));
        // SAFETY: the 16 bytes from each end at or before `n`.
        let m = same(unsafe { eq(a0, b0) });
        if m != u64::MAX {
            return Some(at + (!m).trailing_zeros() as usize / 4);
        }
        at += 16;
    }
    if at < n {
        let last = n - 16;
        // `n >= 16`, so `last + 16 == n` and `last` is inside both slices.
        let (a0, b0) = (pa.wrapping_add(last), pb.wrapping_add(last));
        // SAFETY: the 16 bytes from each end at `n`.
        let m = same(unsafe { eq(a0, b0) });
        if m != u64::MAX {
            return Some(last + (!m).trailing_zeros() as usize / 4);
        }
    }
    None
}

/// Inputs at least this long are compared across cores: below it one core streams them in
/// under 3 ms, about what starting the threads and faulting the pages in from each costs.
pub const ACROSS_CORES: usize = 64 << 20;

/// What a piece compares between looks at whether an earlier piece has found a difference.
const BLOCK: usize = 4 << 20;

/// [`first_difference`] with the bytes cut into one piece per core, each compared by
/// [`fastest`] on a thread of its own: memory-bound on every core at once rather than one. A
/// piece stops once an earlier one has found a difference; the earliest piece with one never
/// stops, so the answer is the same offset as on one core. The pieces are consecutive, so the
/// least offset any piece finds is the answer: one atomic minimum, nothing collected.
pub fn first_difference_across_cores(a: &[u8], b: &[u8]) -> Option<usize> {
    first_difference_in(fastest(), a, b, crate::join::cores(), BLOCK)
}

fn first_difference_in(path: Simd, a: &[u8], b: &[u8], pieces: usize, block: usize) -> Option<usize> {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    let n = a.len().min(b.len());
    let pieces = pieces.clamp(1, n.div_ceil(block).max(1));
    if pieces == 1 {
        return first_difference_with(path, a, b);
    }
    let longer = a.len() != b.len();
    let first = AtomicUsize::new(usize::MAX);
    let piece = |p: usize| {
        let (start, end) = (p * n / pieces, (p + 1) * n / pieces);
        let mut at = start;
        while at < end && first.load(Relaxed) >= start {
            let to = (at + block).min(end);
            if let Some(found) = first_difference_with(path, &a[at..to], &b[at..to]) {
                first.fetch_min(at + found, Relaxed);
                return;
            }
            at = to;
        }
    };
    std::thread::scope(|scope| {
        let piece = &piece;
        for p in 1..pieces {
            scope.spawn(move || piece(p));
        }
        piece(0);
    });
    let first = first.into_inner();
    (first != usize::MAX).then_some(first).or(longer.then_some(n))
}

/// A file's bytes, mapped read-only; an empty file maps nothing.
///
/// The file must not change while mapped: what is mapped here are the outputs and traces of
/// builds that have finished, which nothing writes while they are compared.
pub struct Mapped(blaze_core::fs::Mapped);

impl Mapped {
    /// Mapped and faulted in at once by the kernel (`MAP_POPULATE` on Linux), for bytes one
    /// thread reads.
    pub fn open(path: &Path) -> io::Result<Mapped> {
        Mapped::map(path, true)
    }

    /// Mapped with no page faulted in, for bytes many threads read: each faults in the pages of
    /// its own piece, so the faults run on every core rather than serially in `mmap`.
    pub fn open_lazily(path: &Path) -> io::Result<Mapped> {
        Mapped::map(path, false)
    }

    fn map(path: &Path, populate: bool) -> io::Result<Mapped> {
        let options = blaze_core::fs::MapOptions::new().populate(populate);
        // SAFETY: a read-only map of a file nothing writes while it is mapped (the type's
        // contract above).
        Ok(Mapped(unsafe { options.open(path) }?))
    }

    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }
}

/// Where two files first differ, or where the shorter ends; `None` when their bytes are the
/// same. Both are mapped and compared in place by [`fastest`], across cores from
/// [`ACROSS_CORES`] bytes on.
pub fn first_file_difference(a: &Path, b: &Path) -> io::Result<Option<u64>> {
    let long = |path: &Path| std::fs::metadata(path).map(|m| m.len() >= ACROSS_CORES as u64);
    if long(a)? && long(b)? {
        let (a, b) = (Mapped::open_lazily(a)?, Mapped::open_lazily(b)?);
        return Ok(first_difference_across_cores(a.bytes(), b.bytes()).map(|at| at as u64));
    }
    let (a, b) = (Mapped::open(a)?, Mapped::open(b)?);
    Ok(first_difference(a.bytes(), b.bytes()).map(|at| at as u64))
}

#[cfg(test)]
mod tests {
    use super::{Simd, first_difference_in, first_difference_with};
    use crate::Rng;

    #[test]
    fn pieces_across_cores_find_the_first_difference_one_core_does() {
        let mut rng = Rng::new(4242);
        const BLOCK: usize = 4096;
        let len = 3 * BLOCK + 1_234;
        let a: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        let mut wants = vec![None, Some(0), Some(len - 1), Some(BLOCK), Some(BLOCK - 1)];
        wants.extend((0..12).map(|_| Some(rng.below(len as u64) as usize)));
        for want in wants {
            let mut b = a.clone();
            if let Some(at) = want {
                b[at] ^= 1;
                // A later difference in every later piece: the earliest still wins.
                for later in (at + 1..len).step_by(BLOCK / 3) {
                    b[later] ^= 0x80;
                }
            }
            let serial = first_difference_with(Simd::Scalar, &a, &b);
            assert_eq!(serial, want);
            for pieces in [1, 2, 3, 4, 5, 8, 64] {
                assert_eq!(first_difference_in(Simd::Scalar, &a, &b, pieces, BLOCK), want, "{pieces} pieces, {want:?}");
                assert_eq!(first_difference_in(super::fastest(), &a, &b, pieces, BLOCK), want, "{pieces} pieces, {want:?}");
            }
        }
        let mut longer = a.clone();
        longer.push(7);
        for pieces in [1, 3, 8] {
            assert_eq!(first_difference_in(Simd::Scalar, &a, &longer, pieces, BLOCK), Some(len));
            assert_eq!(first_difference_in(Simd::Scalar, &longer, &a, pieces, BLOCK), Some(len));
        }
    }

    /// The first difference on one core, by every path, reads the two slices in place and
    /// allocates nothing once the path is detected.
    #[test]
    fn a_first_difference_allocates_nothing() {
        let mut rng = Rng::new(31);
        let len = 3 * 4096 + 77;
        let a: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        let mut b = a.clone();
        b[len - 3] ^= 1;
        let fastest = super::fastest();
        for path in super::available() {
            let found =
                blaze_alloc_count::assert_at_most(0, "a first difference", || first_difference_with(path, &a, &b));
            assert_eq!(found, Some(len - 3), "{path:?}");
            let short = blaze_alloc_count::assert_at_most(0, "a first difference of a short input", || {
                first_difference_with(path, &a[..9], &b[..10])
            });
            assert_eq!(short, Some(9), "{path:?}");
        }
        let same = blaze_alloc_count::assert_at_most(0, "equal bytes by the fastest path", || {
            super::first_difference(&a, &a)
        });
        assert_eq!(same, None);
        let one = blaze_alloc_count::assert_at_most(0, "one piece, on this thread", || {
            first_difference_in(fastest, &a, &b, 1, 4096)
        });
        assert_eq!(one, Some(len - 3));
    }
}

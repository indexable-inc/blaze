//! SHA-256 (FIPS 180-4), with the processor's SHA-256 instructions where it has them (SHA-NI on
//! x86-64, the ARMv8 SHA-256 instructions on aarch64) and a portable compression where it has
//! neither. Every path computes the same function, checked against each other in the tests:
//! which one runs is dispatch, not a fallback.
//!
//! What hashes with it: the pages a Mach-O code signature covers (tens of megabytes of a large
//! debug build, on every link and splice), and two build scripts (`blaze-engine`'s hash of the
//! workspace's sources, `blaze-cargo`'s name of the sandbox library). It has no dependencies so
//! that those build scripts take it without `sha2`, whose `cpufeatures` needs `libc` on Apple's
//! aarch64 and a build dependency is built for the machine that builds.

#![deny(unsafe_code)]

const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The SHA-256 of `bytes`. Whole blocks are compressed in place, never copied.
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finish()
}

/// Whether this processor has the SHA-256 instructions, so that pages hash on one thread about
/// as fast as memory feeds them (a few GB/s) and splitting them over threads gains little.
pub fn fast() -> bool {
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("sha2") {
        return true;
    }
    #[cfg(target_arch = "x86_64")]
    if x86::detected() {
        return true;
    }
    false
}

/// A SHA-256 of bytes given in parts: the same as [`digest`] of them joined. Holds at most one
/// block, inline: it never allocates.
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    pending: [u8; 64],
    pending_len: usize,
    len: u64,
}

impl Default for Sha256 {
    fn default() -> Sha256 {
        Sha256::new()
    }
}

impl Sha256 {
    pub const fn new() -> Sha256 {
        Sha256 { state: INITIAL, pending: [0; 64], pending_len: 0, len: 0 }
    }

    pub fn update(&mut self, mut bytes: &[u8]) -> &mut Sha256 {
        self.len += bytes.len() as u64;
        if self.pending_len > 0 {
            let take = (64 - self.pending_len).min(bytes.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&bytes[..take]);
            self.pending_len += take;
            bytes = &bytes[take..];
            if self.pending_len < 64 {
                return self;
            }
            let block = self.pending;
            compress(&mut self.state, &block);
            self.pending_len = 0;
        }
        let whole = bytes.len() / 64 * 64;
        compress(&mut self.state, &bytes[..whole]);
        let rest = &bytes[whole..];
        self.pending[..rest.len()].copy_from_slice(rest);
        self.pending_len = rest.len();
        self
    }

    pub fn finish(&self) -> [u8; 32] {
        let mut state = self.state;
        // The rest, the bit 1, zeros and the length in bits, in one or two blocks.
        let rest = &self.pending[..self.pending_len];
        let mut tail = [0u8; 128];
        tail[..rest.len()].copy_from_slice(rest);
        tail[rest.len()] = 0x80;
        let len = if rest.len() < 56 { 64 } else { 128 };
        tail[len - 8..len].copy_from_slice(&(self.len * 8).to_be_bytes());
        compress(&mut state, &tail[..len]);
        let mut out = [0u8; 32];
        for (word, at) in state.iter().zip(out.chunks_exact_mut(4)) {
            at.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// `state` after the whole 64-byte blocks of `blocks`.
#[allow(unsafe_code)]
fn compress(state: &mut [u32; 8], blocks: &[u8]) {
    debug_assert_eq!(blocks.len() % 64, 0, "whole blocks");
    if blocks.is_empty() {
        return;
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("sha2") {
        // SAFETY: the processor has the SHA-256 instructions, checked just above.
        unsafe { aarch64::compress(state, blocks) };
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if x86::detected() {
        // SAFETY: `detected` checked SHA-NI, SSSE3 and SSE4.1.
        unsafe { x86::compress(state, blocks) };
        return;
    }
    portable(state, blocks);
}

fn portable(state: &mut [u32; 8], blocks: &[u8]) {
    for block in blocks.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for t in 16..64 {
            let s0 = w[t - 15].rotate_right(7) ^ w[t - 15].rotate_right(18) ^ (w[t - 15] >> 3);
            let s1 = w[t - 2].rotate_right(17) ^ w[t - 2].rotate_right(19) ^ (w[t - 2] >> 10);
            w[t] = w[t - 16].wrapping_add(s0).wrapping_add(w[t - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for t in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = h.wrapping_add(s1).wrapping_add(choice).wrapping_add(K[t]).wrapping_add(w[t]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            (h, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (word, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(add);
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(unsafe_code)]
mod aarch64 {
    use std::arch::aarch64::{
        uint8x16_t, uint32x4_t, vaddq_u32, vld1q_u8, vld1q_u32, vreinterpretq_u32_u8, vrev32q_u8,
        vsha256h2q_u32, vsha256hq_u32, vsha256su0q_u32, vsha256su1q_u32, vst1q_u32,
    };

    use super::K;

    /// The four words at the start of `words`, as one vector.
    #[inline(always)]
    fn words(words: &[u32]) -> uint32x4_t {
        let words: &[u32; 4] = words[..4].try_into().expect("four words");
        // SAFETY: `words` is 4 readable `u32`s, the 16 bytes `vld1q_u32` reads; NEON is in
        // every aarch64 target, so the load needs no detection.
        unsafe { vld1q_u32(words.as_ptr()) }
    }

    #[inline(always)]
    fn bytes(bytes: &[u8]) -> uint8x16_t {
        let bytes: &[u8; 16] = bytes[..16].try_into().expect("16 bytes");
        // SAFETY: `bytes` is 16 readable bytes, what `vld1q_u8` reads, at any alignment.
        unsafe { vld1q_u8(bytes.as_ptr()) }
    }

    #[inline(always)]
    fn store(out: &mut [u32], value: uint32x4_t) {
        let out: &mut [u32; 4] = (&mut out[..4]).try_into().expect("four words");
        // SAFETY: `out` is 4 writable `u32`s, the 16 bytes `vst1q_u32` writes.
        unsafe { vst1q_u32(out.as_mut_ptr(), value) }
    }

    /// # Safety
    ///
    /// The processor has the SHA-256 instructions.
    #[target_feature(enable = "sha2")]
    pub(super) unsafe fn compress(state: &mut [u32; 8], blocks: &[u8]) {
        let mut abcd = words(&state[..4]);
        let mut efgh = words(&state[4..]);
        for block in blocks.chunks_exact(64) {
            let (abcd_was, efgh_was) = (abcd, efgh);
            let mut words: [uint32x4_t; 4] = std::array::from_fn(|nth| {
                vreinterpretq_u32_u8(vrev32q_u8(bytes(&block[nth * 16..])))
            });
            for group in 0..16 {
                let at = group % 4;
                if group >= 4 {
                    let early = vsha256su0q_u32(words[at], words[(at + 1) % 4]);
                    words[at] = vsha256su1q_u32(early, words[(at + 2) % 4], words[(at + 3) % 4]);
                }
                let mixed = vaddq_u32(words[at], self::words(&K[group * 4..]));
                let was = abcd;
                abcd = vsha256hq_u32(abcd, efgh, mixed);
                efgh = vsha256h2q_u32(efgh, was, mixed);
            }
            abcd = vaddq_u32(abcd, abcd_was);
            efgh = vaddq_u32(efgh, efgh_was);
        }
        store(&mut state[..4], abcd);
        store(&mut state[4..], efgh);
    }
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod x86 {
    use std::arch::x86_64::{
        __m128i, _mm_add_epi32, _mm_alignr_epi8, _mm_blend_epi16, _mm_loadu_si128, _mm_set_epi64x,
        _mm_sha256msg1_epu32, _mm_sha256msg2_epu32, _mm_sha256rnds2_epu32, _mm_shuffle_epi8,
        _mm_shuffle_epi32, _mm_storeu_si128,
    };

    use super::K;

    pub(super) fn detected() -> bool {
        std::arch::is_x86_feature_detected!("sha")
            && std::arch::is_x86_feature_detected!("ssse3")
            && std::arch::is_x86_feature_detected!("sse4.1")
    }

    /// The 16 bytes at the start of `bytes`, as one vector.
    #[inline(always)]
    fn load<T>(bytes: &[T]) -> __m128i {
        assert!(size_of_val(bytes) >= 16, "16 bytes to load");
        // SAFETY: `bytes` has at least the 16 readable bytes `_mm_loadu_si128` reads, at any
        // alignment; SSE2 is in every x86-64 target, so the load needs no detection.
        unsafe { _mm_loadu_si128(bytes.as_ptr().cast()) }
    }

    #[inline(always)]
    fn store(out: &mut [u32], value: __m128i) {
        let out: &mut [u32; 4] = (&mut out[..4]).try_into().expect("four words");
        // SAFETY: `out` is the 16 writable bytes `_mm_storeu_si128` writes, at any alignment.
        unsafe { _mm_storeu_si128(out.as_mut_ptr().cast(), value) }
    }

    /// Intel's SHA extensions: the state as ABEF and CDGH, two rounds per `sha256rnds2`.
    ///
    /// # Safety
    ///
    /// The processor has SHA-NI, SSSE3 and SSE4.1 ([`detected`]).
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub(super) unsafe fn compress(state: &mut [u32; 8], blocks: &[u8]) {
        let swap = _mm_set_epi64x(0x0c0d_0e0f_0809_0a0b, 0x0405_0607_0001_0203);
        let dcba = _mm_shuffle_epi32(load(&state[..4]), 0xb1);
        let efgh = _mm_shuffle_epi32(load(&state[4..]), 0x1b);
        let mut abef = _mm_alignr_epi8(dcba, efgh, 8);
        let mut cdgh = _mm_blend_epi16(efgh, dcba, 0xf0);
        for block in blocks.chunks_exact(64) {
            let (abef_was, cdgh_was) = (abef, cdgh);
            let mut words: [__m128i; 4] = [
                _mm_shuffle_epi8(load(&block[..16]), swap),
                _mm_shuffle_epi8(load(&block[16..32]), swap),
                _mm_shuffle_epi8(load(&block[32..48]), swap),
                _mm_shuffle_epi8(load(&block[48..]), swap),
            ];
            for group in 0..16 {
                let at = group % 4;
                if group >= 4 {
                    let early = _mm_add_epi32(
                        _mm_sha256msg1_epu32(words[at], words[(at + 1) % 4]),
                        _mm_alignr_epi8(words[(at + 3) % 4], words[(at + 2) % 4], 4),
                    );
                    words[at] = _mm_sha256msg2_epu32(early, words[(at + 3) % 4]);
                }
                let mixed = _mm_add_epi32(words[at], load(&K[group * 4..group * 4 + 4]));
                cdgh = _mm_sha256rnds2_epu32(cdgh, abef, mixed);
                abef = _mm_sha256rnds2_epu32(abef, cdgh, _mm_shuffle_epi32(mixed, 0x0e));
            }
            abef = _mm_add_epi32(abef, abef_was);
            cdgh = _mm_add_epi32(cdgh, cdgh_was);
        }
        let feba = _mm_shuffle_epi32(abef, 0x1b);
        let dchg = _mm_shuffle_epi32(cdgh, 0xb1);
        store(&mut state[..4], _mm_blend_epi16(feba, dchg, 0xf0));
        store(&mut state[4..], _mm_alignr_epi8(dchg, feba, 8));
    }
}

#[cfg(test)]
mod tests;

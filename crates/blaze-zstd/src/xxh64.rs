//! Pure `#![no_std]`, zero-unsafe 64-bit xxHash (`XXH64`), used by RFC 8878 Zstandard content
//! checksums and re-exported by `bstd::digest`.

use blaze_own::Int;

#[inline]
fn read_u32_le(bytes: &[u8]) -> u32 {
    match bytes.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) {
        Some(arr) => u32::from_le_bytes(arr),
        None => 0,
    }
}

#[inline]
fn read_u64_le(bytes: &[u8]) -> u64 {
    match bytes.get(..8).and_then(|s| <[u8; 8]>::try_from(s).ok()) {
        Some(arr) => u64::from_le_bytes(arr),
        None => 0,
    }
}

const XXH64_P1: u64 = 0x9E37_79B1_85EB_CA87;
const XXH64_P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const XXH64_P3: u64 = 0x1656_67B1_9E37_79F9;
const XXH64_P4: u64 = 0x85EB_CA77_C2B2_AE63;
const XXH64_P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline]
const fn xxh64_round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(XXH64_P2))
        .rotate_left(31)
        .wrapping_mul(XXH64_P1)
}

#[inline]
const fn xxh64_merge_round(acc: u64, val: u64) -> u64 {
    let v = xxh64_round(0, val);
    (acc ^ v).wrapping_mul(XXH64_P1).wrapping_add(XXH64_P4)
}

/// Streaming 64-bit xxHash (`XXH64`, used by RFC 8878 Zstandard content checksums).
#[derive(Clone, Copy, Debug)]
pub struct Xxh64 {
    seed: u64,
    total_len: u64,
    v: [u64; 4],
    buf: [u8; 32],
    buf_len: usize,
}

impl Xxh64 {
    /// Initialize a streaming `XXH64` hasher with `seed`.
    #[inline]
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            seed,
            total_len: 0,
            v: [
                seed.wrapping_add(XXH64_P1).wrapping_add(XXH64_P2),
                seed.wrapping_add(XXH64_P2),
                seed,
                seed.wrapping_sub(XXH64_P1),
            ],
            buf: [0u8; 32],
            buf_len: 0,
        }
    }

    /// Feed `bytes` into the hasher.
    pub fn update(&mut self, mut bytes: &[u8]) -> &mut Self {
        self.total_len = self.total_len.wrapping_add(bytes.len().widen_to::<u64>());

        if self.buf_len > 0 {
            let need = 32usize.saturating_sub(self.buf_len);
            if bytes.len() < need {
                if let Some(dst) = self.buf.get_mut(self.buf_len..self.buf_len + bytes.len()) {
                    dst.copy_from_slice(bytes);
                }
                self.buf_len += bytes.len();
                return self;
            }
            if let Some((head, tail)) = bytes.split_at_checked(need) {
                if let Some(dst) = self.buf.get_mut(self.buf_len..32) {
                    dst.copy_from_slice(head);
                }
                let stripe = self.buf;
                let _ = blaze_own::xxhash64_stripes(&mut self.v, &stripe);
                self.buf_len = 0;
                bytes = tail;
            }
        }

        let consumed = blaze_own::xxhash64_stripes(&mut self.v, bytes);
        let rem = bytes.get(consumed..).unwrap_or(&[]);
        if !rem.is_empty() {
            if let Some(dst) = self.buf.get_mut(..rem.len()) {
                dst.copy_from_slice(rem);
            }
            self.buf_len = rem.len();
        }
        self
    }

    /// Finalize and return the 64-bit `XXH64` hash.
    #[must_use]
    pub fn finish(&self) -> u64 {
        let [v1, v2, v3, v4] = self.v;
        let mut h = if self.total_len >= 32 {
            let acc = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            let acc = xxh64_merge_round(acc, v1);
            let acc = xxh64_merge_round(acc, v2);
            let acc = xxh64_merge_round(acc, v3);
            xxh64_merge_round(acc, v4)
        } else {
            self.seed.wrapping_add(XXH64_P5)
        };
        h = h.wrapping_add(self.total_len);

        let mut rem = self.buf.get(..self.buf_len).unwrap_or(&[]);
        while let Some((head, tail)) = rem.split_at_checked(8) {
            let k1 = xxh64_round(0, read_u64_le(head));
            h = (h ^ k1)
                .rotate_left(27)
                .wrapping_mul(XXH64_P1)
                .wrapping_add(XXH64_P4);
            rem = tail;
        }
        if let Some((head, tail)) = rem.split_at_checked(4) {
            h = (h ^ u64::from(read_u32_le(head)).wrapping_mul(XXH64_P1))
                .rotate_left(23)
                .wrapping_mul(XXH64_P2)
                .wrapping_add(XXH64_P3);
            rem = tail;
        }
        for &b in rem {
            h = (h ^ u64::from(b).wrapping_mul(XXH64_P5))
                .rotate_left(11)
                .wrapping_mul(XXH64_P1);
        }

        h ^= h >> 33;
        h = h.wrapping_mul(XXH64_P2);
        h ^= h >> 29;
        h = h.wrapping_mul(XXH64_P3);
        h ^= h >> 32;
        h
    }
}

/// One-shot 64-bit xxHash (`XXH64`) of `data` with `seed`.
#[inline]
#[must_use]
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    blaze_own::xxhash64_slice(data, seed)
}

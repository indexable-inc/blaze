//! Cryptographic digests, MACs, and checksums with zero allocations, zero third-party deps,
//! zero panicking slice indexes, and zero unsafe:
//!
//! - **BLAKE3**: [`Blake3`], [`BLAKE3_LEN`], [`blake3`] (from `blaze-ids`).
//! - **SHA-256**: [`Sha256`], [`SHA256_LEN`], [`sha256`] (from `blaze-sha256`, hardware SHA-NI /
//!   ARMv8 SHA2 when enabled by the target).
//! - **SHA-512**: [`sha512`] (from `blaze-ed25519`).
//! - **HMAC-SHA256** (RFC 2104): [`HmacSha256`], [`hmac_sha256`], and constant-time comparison
//!   [`ct_eq`].
//! - **Checksums**: [`Crc32`] (IEEE / ISO 3309), [`Adler32`] (from `blaze-deflate`), hardware
//!   accelerated [`crc32c`] / [`crc32c_extend`] (from `blaze-ids`), and fast non-cryptographic
//!   [`Xxh32`] / [`xxh32`] and [`Xxh64`] / [`xxh64`].

pub use blaze_deflate::{Adler32, Crc32};
pub use blaze_ed25519::sha512;
pub use blaze_ids::b3::{Hasher as Blake3, OUT_LEN as BLAKE3_LEN, hash as blake3};
pub use blaze_ids::crc32c::{crc32c, extend as crc32c_extend};
pub use blaze_sha256::{Sha256, digest as sha256};

/// Output length in bytes of SHA-256 and HMAC-SHA256.
pub const SHA256_LEN: usize = 32;

const SHA256_BLOCK: usize = 64;

/// Streaming RFC 2104 HMAC-SHA256 with zero heap allocation.
#[derive(Clone)]
pub struct HmacSha256 {
    inner: Sha256,
    outer_key: [u8; SHA256_BLOCK],
}

impl HmacSha256 {
    /// Initialize HMAC-SHA256 with `key`.
    #[must_use]
    pub fn new(key: &[u8]) -> Self {
        let mut block = [0u8; SHA256_BLOCK];
        if key.len() > SHA256_BLOCK {
            let hashed = sha256(key);
            if let Some(dst) = block.get_mut(..SHA256_LEN) {
                dst.copy_from_slice(&hashed);
            }
        } else if let Some(dst) = block.get_mut(..key.len()) {
            dst.copy_from_slice(key);
        }

        let mut ipad = [0u8; SHA256_BLOCK];
        let mut opad = [0u8; SHA256_BLOCK];
        for ((ip, op), &b) in ipad.iter_mut().zip(opad.iter_mut()).zip(block.iter()) {
            *ip = b ^ 0x36;
            *op = b ^ 0x5c;
        }

        let mut inner = Sha256::new();
        inner.update(&ipad);
        Self { inner, outer_key: opad }
    }

    /// Feed additional message bytes into the HMAC.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.inner.update(bytes);
        self
    }

    /// Finalize and return the 32-byte authentication tag.
    #[must_use]
    pub fn finish(self) -> [u8; SHA256_LEN] {
        let inner_hash = self.inner.finish();
        let mut outer = Sha256::new();
        outer.update(&self.outer_key);
        outer.update(&inner_hash);
        outer.finish()
    }

    /// Finalize and verify against `expected` in constant time.
    #[must_use]
    pub fn verify(self, expected: &[u8; SHA256_LEN]) -> bool {
        let tag = self.finish();
        ct_eq(&tag, expected)
    }
}

/// One-shot RFC 2104 HMAC-SHA256 of `data` under `key`.
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; SHA256_LEN] {
    let mut mac = HmacSha256::new(key);
    mac.update(data);
    mac.finish()
}

/// Constant-time equality comparison for equal-length byte slices (e.g. MACs and digests).
#[inline(never)]
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    core::hint::black_box(diff) == 0
}

// ---------------------------------------------------------------------------
// XXH32 & XXH64 (Yann Collet's xxHash specification)
// ---------------------------------------------------------------------------

const XXH32_P1: u32 = 0x9E37_79B1;
const XXH32_P2: u32 = 0x85EB_CA77;
const XXH32_P3: u32 = 0xC2B2_AE3D;
const XXH32_P4: u32 = 0x27D4_EB2F;
const XXH32_P5: u32 = 0x1656_67B1;

#[inline]
const fn xxh32_round(acc: u32, lane: u32) -> u32 {
    acc.wrapping_add(lane.wrapping_mul(XXH32_P2))
        .rotate_left(13)
        .wrapping_mul(XXH32_P1)
}

#[inline]
fn read_u32_le(bytes: &[u8]) -> u32 {
    let Ok(arr): Result<[u8; 4], _> = bytes.try_into() else {
        return 0;
    };
    u32::from_le_bytes(arr)
}

#[inline]
fn read_u64_le(bytes: &[u8]) -> u64 {
    let Ok(arr): Result<[u8; 8], _> = bytes.try_into() else {
        return 0;
    };
    u64::from_le_bytes(arr)
}

/// Streaming 32-bit xxHash (`XXH32`, used by LZ4 Frame checksums).
#[derive(Clone, Copy, Debug)]
pub struct Xxh32 {
    seed: u32,
    total_len: u64,
    v1: u32,
    v2: u32,
    v3: u32,
    v4: u32,
    buf: [u8; 16],
    buf_len: usize,
}

impl Xxh32 {
    /// Initialize a streaming `XXH32` hasher with `seed`.
    #[inline]
    #[must_use]
    pub const fn new(seed: u32) -> Self {
        Self {
            seed,
            total_len: 0,
            v1: seed.wrapping_add(XXH32_P1).wrapping_add(XXH32_P2),
            v2: seed.wrapping_add(XXH32_P2),
            v3: seed,
            v4: seed.wrapping_sub(XXH32_P1),
            buf: [0u8; 16],
            buf_len: 0,
        }
    }

    #[inline]
    fn consume_stripe(&mut self, stripe: &[u8]) {
        if let (Some(a), Some(b), Some(c), Some(d)) = (
            stripe.get(0..4),
            stripe.get(4..8),
            stripe.get(8..12),
            stripe.get(12..16),
        ) {
            self.v1 = xxh32_round(self.v1, read_u32_le(a));
            self.v2 = xxh32_round(self.v2, read_u32_le(b));
            self.v3 = xxh32_round(self.v3, read_u32_le(c));
            self.v4 = xxh32_round(self.v4, read_u32_le(d));
        }
    }

    /// Feed `bytes` into the hasher.
    pub fn update(&mut self, mut bytes: &[u8]) -> &mut Self {
        self.total_len = self.total_len.wrapping_add(u64::try_from(bytes.len()).unwrap_or(0));

        if self.buf_len > 0 {
            let need = 16usize.saturating_sub(self.buf_len);
            if bytes.len() < need {
                if let Some(dst) = self.buf.get_mut(self.buf_len..self.buf_len + bytes.len()) {
                    dst.copy_from_slice(bytes);
                }
                self.buf_len += bytes.len();
                return self;
            }
            if let Some((head, tail)) = bytes.split_at_checked(need) {
                if let Some(dst) = self.buf.get_mut(self.buf_len..16) {
                    dst.copy_from_slice(head);
                }
                let stripe = self.buf;
                self.consume_stripe(&stripe);
                self.buf_len = 0;
                bytes = tail;
            }
        }

        let mut chunks = bytes.chunks_exact(16);
        for stripe in &mut chunks {
            self.consume_stripe(stripe);
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            if let Some(dst) = self.buf.get_mut(..rem.len()) {
                dst.copy_from_slice(rem);
            }
            self.buf_len = rem.len();
        }
        self
    }

    /// Finalize and return the 32-bit `XXH32` hash.
    #[must_use]
    pub fn finish(&self) -> u32 {
        let mut h = if self.total_len >= 16 {
            self.v1
                .rotate_left(1)
                .wrapping_add(self.v2.rotate_left(7))
                .wrapping_add(self.v3.rotate_left(12))
                .wrapping_add(self.v4.rotate_left(18))
        } else {
            self.seed.wrapping_add(XXH32_P5)
        };
        let [l0, l1, l2, l3, ..] = self.total_len.to_le_bytes();
        h = h.wrapping_add(u32::from_le_bytes([l0, l1, l2, l3]));

        let mut rem = self.buf.get(..self.buf_len).unwrap_or(&[]);
        while let Some((head, tail)) = rem.split_at_checked(4) {
            h = h
                .wrapping_add(read_u32_le(head).wrapping_mul(XXH32_P3))
                .rotate_left(17)
                .wrapping_mul(XXH32_P4);
            rem = tail;
        }
        for &b in rem {
            h = h
                .wrapping_add(u32::from(b).wrapping_mul(XXH32_P5))
                .rotate_left(11)
                .wrapping_mul(XXH32_P1);
        }

        h ^= h >> 15;
        h = h.wrapping_mul(XXH32_P2);
        h ^= h >> 13;
        h = h.wrapping_mul(XXH32_P3);
        h ^= h >> 16;
        h
    }
}

/// One-shot 32-bit xxHash (`XXH32`) of `data` with `seed`.
#[inline]
#[must_use]
pub fn xxh32(data: &[u8], seed: u32) -> u32 {
    let mut h = Xxh32::new(seed);
    h.update(data);
    h.finish()
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
    v1: u64,
    v2: u64,
    v3: u64,
    v4: u64,
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
            v1: seed.wrapping_add(XXH64_P1).wrapping_add(XXH64_P2),
            v2: seed.wrapping_add(XXH64_P2),
            v3: seed,
            v4: seed.wrapping_sub(XXH64_P1),
            buf: [0u8; 32],
            buf_len: 0,
        }
    }

    #[inline]
    fn consume_stripe(&mut self, stripe: &[u8]) {
        if let (Some(a), Some(b), Some(c), Some(d)) = (
            stripe.get(0..8),
            stripe.get(8..16),
            stripe.get(16..24),
            stripe.get(24..32),
        ) {
            self.v1 = xxh64_round(self.v1, read_u64_le(a));
            self.v2 = xxh64_round(self.v2, read_u64_le(b));
            self.v3 = xxh64_round(self.v3, read_u64_le(c));
            self.v4 = xxh64_round(self.v4, read_u64_le(d));
        }
    }

    /// Feed `bytes` into the hasher.
    pub fn update(&mut self, mut bytes: &[u8]) -> &mut Self {
        self.total_len = self.total_len.wrapping_add(u64::try_from(bytes.len()).unwrap_or(0));

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
                self.consume_stripe(&stripe);
                self.buf_len = 0;
                bytes = tail;
            }
        }

        let mut chunks = bytes.chunks_exact(32);
        for stripe in &mut chunks {
            self.consume_stripe(stripe);
        }
        let rem = chunks.remainder();
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
        let mut h = if self.total_len >= 32 {
            let acc = self
                .v1
                .rotate_left(1)
                .wrapping_add(self.v2.rotate_left(7))
                .wrapping_add(self.v3.rotate_left(12))
                .wrapping_add(self.v4.rotate_left(18));
            let acc = xxh64_merge_round(acc, self.v1);
            let acc = xxh64_merge_round(acc, self.v2);
            let acc = xxh64_merge_round(acc, self.v3);
            xxh64_merge_round(acc, self.v4)
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
    let mut h = Xxh64::new(seed);
    h.update(data);
    h.finish()
}

#[cfg(test)]
mod tests {
    use blaze_own::{Vec as OwnVec, testing::heap};
    use blaze_sim::Config;

    use super::*;

    #[test]
    fn rfc4231_hmac_sha256_vectors() {
        let k1 = [0x0bu8; 20];
        let t1 = hmac_sha256(&k1, b"Hi There");
        let e1 = crate::hex::decode_array::<32>(
            b"b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        )
        .expect("hex");
        assert!(ct_eq(&t1, &e1));

        let t2 = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let e2 = crate::hex::decode_array::<32>(
            b"5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        )
        .expect("hex");
        assert!(ct_eq(&t2, &e2));

        let k6 = [0xaau8; 131];
        let t6 = hmac_sha256(&k6, b"Test Using Larger Than Block-Size Key - Hash Key First");
        let e6 = crate::hex::decode_array::<32>(
            b"60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
        )
        .expect("hex");
        assert!(ct_eq(&t6, &e6));

        let mut mac = HmacSha256::new(b"Jefe");
        mac.update(b"what do ya ");
        mac.update(b"want for nothing?");
        assert!(mac.verify(&e2));

        let mut flipped = e2;
        flipped[15] ^= 1;
        assert!(!ct_eq(&t2, &flipped));
    }

    #[test]
    fn property_streaming_hmac_matches_oneshot() {
        let h = heap();
        blaze_sim::check(Config::cases(128), |tape| {
            let klen = usize::try_from(tape.int_in(0..=96, "klen")).unwrap_or(0);
            let mlen = usize::try_from(tape.int_in(0..=128, "mlen")).unwrap_or(0);
            let mut key = OwnVec::new_in(h.alloc());
            for _ in 0..klen {
                key.push(u8::try_from(tape.below(256, "k")).unwrap_or(0));
            }
            let mut msg = OwnVec::new_in(h.alloc());
            for _ in 0..mlen {
                msg.push(u8::try_from(tape.below(256, "m")).unwrap_or(0));
            }
            let split = if mlen == 0 {
                0
            } else {
                usize::try_from(tape.below(u64::try_from(mlen).unwrap_or(0) + 1, "split"))
                    .unwrap_or(0)
            };
            let oneshot = hmac_sha256(key.as_slice(), msg.as_slice());
            let mut stream = HmacSha256::new(key.as_slice());
            stream.update(&msg.as_slice()[..split]);
            stream.update(&msg.as_slice()[split..]);
            assert!(stream.verify(&oneshot));
        });
    }

    #[test]
    fn xxh32_xxh64_and_crc32c_reference_vectors() {
        assert_eq!(xxh32(b"", 0), 0x02CC_5D05);
        assert_eq!(xxh32(b"abc", 0), 0x32D1_53FF);
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"hello zstd world!", 0) & 0xFFFF_FFFF, 0xB60C_1CA5);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);

        // Verify streaming matches one-shot across multi-stripe (>32B) input.
        let sample = b"The quick brown fox jumps over the lazy dog 0123456789!@#$%^&*()";
        let mut h32 = Xxh32::new(0x1234_5678);
        let mut h64 = Xxh64::new(0x1234_5678_9ABC_DEF0);
        for chunk in sample.chunks(7) {
            h32.update(chunk);
            h64.update(chunk);
        }
        assert_eq!(h32.finish(), xxh32(sample, 0x1234_5678));
        assert_eq!(h64.finish(), xxh64(sample, 0x1234_5678_9ABC_DEF0));
    }
}

//! Cryptographic digests, MACs, and checksums with zero allocations, zero third-party deps,
//! zero panicking slice indexes, and zero unsafe:
//!
//! - **BLAKE3**: [`Blake3`], [`BLAKE3_LEN`], [`blake3`] (from `blaze-ids`).
//! - **SHA-256**: [`Sha256`], [`SHA256_LEN`], [`sha256`] (from `blaze-sha256`, hardware SHA-NI /
//!   ARMv8 SHA2 when enabled by the target).
//! - **HMAC-SHA256** (RFC 2104): [`HmacSha256`], [`hmac_sha256`], and constant-time comparison
//!   [`ct_eq`].
//! - **Checksums**: [`Crc32`] (IEEE / ISO 3309) and [`Adler32`] (from `blaze-deflate`).

pub use blaze_deflate::{Adler32, Crc32};
pub use blaze_ids::b3::{Hasher as Blake3, OUT_LEN as BLAKE3_LEN, hash as blake3};
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
}

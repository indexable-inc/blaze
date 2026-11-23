//! Pure `#![no_std]` modern authenticated encryption, key derivation, and Ed25519 verification:
//!
//! - **Ed25519** signature verification: re-exported from [`blaze_ed25519`] as [`ed25519`].
//! - **ChaCha20-Poly1305 AEAD** (RFC 8439): [`ChaCha20Poly1305`], [`KEY_LEN`], [`NONCE_LEN`],
//!   [`TAG_LEN`], [`AuthError`], [`CryptoError`].
//! - **HKDF-SHA256** (RFC 5869): [`HkdfSha256`], [`hkdf_sha256`], [`HkdfError`].
//! - **PBKDF2-HMAC-SHA256** (RFC 2898): [`pbkdf2_hmac_sha256`], [`Pbkdf2Error`].
//!
//! Zero `unsafe`, zero integer `as` casts, zero panicking slice indexes, zero recursion, and
//! constant-time tag verification via [`crate::digest::ct_eq`].

use core::alloc::Allocator;
use core::fmt;

pub use blaze_ed25519 as ed25519;
use blaze_own::{AllocFailure, Vec};

use crate::digest::{HmacSha256, SHA256_LEN, ct_eq, hmac_sha256, sha256};

/// ChaCha20-Poly1305 key length in bytes (256 bits).
pub const KEY_LEN: usize = 32;

/// ChaCha20-Poly1305 nonce length in bytes (96 bits).
pub const NONCE_LEN: usize = 12;

/// ChaCha20-Poly1305 Poly1305 authentication tag length in bytes (128 bits).
pub const TAG_LEN: usize = 16;

/// Maximum output length of RFC 5869 HKDF-SHA256 (`255 * 32 = 8160` bytes).
pub const HKDF_MAX_OUTPUT_LEN: usize = 255 * SHA256_LEN;

/// Returned when AEAD authentication tag verification fails.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AuthError;

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bstd::crypto: AEAD authentication tag mismatch")
    }
}

impl core::error::Error for AuthError {}

/// Failure returned by allocating AEAD decryption ([`ChaCha20Poly1305::decrypt_in`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CryptoError {
    /// Authentication tag verification failed.
    Auth(AuthError),
    /// Input was shorter than the 16-byte authentication tag.
    CiphertextTooShort { len: usize },
    /// The caller's allocator refused an allocation.
    Alloc(AllocFailure),
}

impl From<AuthError> for CryptoError {
    #[inline]
    fn from(err: AuthError) -> Self {
        Self::Auth(err)
    }
}

impl From<AllocFailure> for CryptoError {
    #[inline]
    fn from(err: AllocFailure) -> Self {
        Self::Alloc(err)
    }
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CryptoError::Auth(err) => fmt::Display::fmt(&err, f),
            CryptoError::CiphertextTooShort { len } => write!(
                f,
                "bstd::crypto: ciphertext shorter than {TAG_LEN}-byte tag ({len} bytes)"
            ),
            CryptoError::Alloc(err) => write!(f, "bstd::crypto: allocation failed ({err})"),
        }
    }
}

impl core::error::Error for CryptoError {}

/// Failure returned by HKDF-SHA256 key expansion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HkdfError {
    /// Requested output length exceeded `255 * 32 = 8160` bytes (RFC 5869 §2.3).
    OutputTooLong { requested: usize, max: usize },
}

impl fmt::Display for HkdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HkdfError::OutputTooLong { requested, max } => write!(
                f,
                "bstd::crypto::hkdf: requested {requested} bytes exceeds RFC 5869 maximum of {max} bytes"
            ),
        }
    }
}

impl core::error::Error for HkdfError {}

/// Failure returned by PBKDF2-HMAC-SHA256 key derivation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pbkdf2Error {
    /// Iteration count was `0` (RFC 2898 requires `iterations >= 1`).
    ZeroIterations,
    /// Output slice was empty.
    EmptyOutput,
    /// Requested output length exceeded `(2^32 - 1) * 32` bytes.
    OutputTooLong { requested: usize },
}

impl fmt::Display for Pbkdf2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Pbkdf2Error::ZeroIterations => {
                f.write_str("bstd::crypto::pbkdf2: iteration count must be at least 1")
            }
            Pbkdf2Error::EmptyOutput => {
                f.write_str("bstd::crypto::pbkdf2: output buffer must not be empty")
            }
            Pbkdf2Error::OutputTooLong { requested } => {
                write!(f, "bstd::crypto::pbkdf2: requested {requested} bytes exceeds RFC 2898 limit")
            }
        }
    }
}

impl core::error::Error for Pbkdf2Error {}

// ---------------------------------------------------------------------------
// ChaCha20 (RFC 8439 §2.1 - §2.4)
// ---------------------------------------------------------------------------

#[inline]
const fn quarter_round(mut a: u32, mut b: u32, mut c: u32, mut d: u32) -> (u32, u32, u32, u32) {
    a = a.wrapping_add(b);
    d ^= a;
    d = d.rotate_left(16);

    c = c.wrapping_add(d);
    b ^= c;
    b = b.rotate_left(12);

    a = a.wrapping_add(b);
    d ^= a;
    d = d.rotate_left(8);

    c = c.wrapping_add(d);
    b ^= c;
    b = b.rotate_left(7);

    (a, b, c, d)
}

#[inline]
const fn chacha20_block(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; NONCE_LEN]) -> [u8; 64] {
    let [
        k0, k1, k2, k3, k4, k5, k6, k7, k8, k9, k10, k11, k12, k13, k14, k15, k16, k17, k18, k19,
        k20, k21, k22, k23, k24, k25, k26, k27, k28, k29, k30, k31,
    ] = *key;
    let [n0, n1, n2, n3, n4, n5, n6, n7, n8, n9, n10, n11] = *nonce;

    let s0: u32 = 0x6170_7865;
    let s1: u32 = 0x3320_646e;
    let s2: u32 = 0x7962_2d32;
    let s3: u32 = 0x6b20_6574;
    let s4 = u32::from_le_bytes([k0, k1, k2, k3]);
    let s5 = u32::from_le_bytes([k4, k5, k6, k7]);
    let s6 = u32::from_le_bytes([k8, k9, k10, k11]);
    let s7 = u32::from_le_bytes([k12, k13, k14, k15]);
    let s8 = u32::from_le_bytes([k16, k17, k18, k19]);
    let s9 = u32::from_le_bytes([k20, k21, k22, k23]);
    let s10 = u32::from_le_bytes([k24, k25, k26, k27]);
    let s11 = u32::from_le_bytes([k28, k29, k30, k31]);
    let s12 = counter;
    let s13 = u32::from_le_bytes([n0, n1, n2, n3]);
    let s14 = u32::from_le_bytes([n4, n5, n6, n7]);
    let s15 = u32::from_le_bytes([n8, n9, n10, n11]);

    let mut x0 = s0;
    let mut x1 = s1;
    let mut x2 = s2;
    let mut x3 = s3;
    let mut x4 = s4;
    let mut x5 = s5;
    let mut x6 = s6;
    let mut x7 = s7;
    let mut x8 = s8;
    let mut x9 = s9;
    let mut x10 = s10;
    let mut x11 = s11;
    let mut x12 = s12;
    let mut x13 = s13;
    let mut x14 = s14;
    let mut x15 = s15;

    let mut round = 0u8;
    while round < 10 {
        (x0, x4, x8, x12) = quarter_round(x0, x4, x8, x12);
        (x1, x5, x9, x13) = quarter_round(x1, x5, x9, x13);
        (x2, x6, x10, x14) = quarter_round(x2, x6, x10, x14);
        (x3, x7, x11, x15) = quarter_round(x3, x7, x11, x15);

        (x0, x5, x10, x15) = quarter_round(x0, x5, x10, x15);
        (x1, x6, x11, x12) = quarter_round(x1, x6, x11, x12);
        (x2, x7, x8, x13) = quarter_round(x2, x7, x8, x13);
        (x3, x4, x9, x14) = quarter_round(x3, x4, x9, x14);
        round += 1;
    }

    let [w0_0, w0_1, w0_2, w0_3] = x0.wrapping_add(s0).to_le_bytes();
    let [w1_0, w1_1, w1_2, w1_3] = x1.wrapping_add(s1).to_le_bytes();
    let [w2_0, w2_1, w2_2, w2_3] = x2.wrapping_add(s2).to_le_bytes();
    let [w3_0, w3_1, w3_2, w3_3] = x3.wrapping_add(s3).to_le_bytes();
    let [w4_0, w4_1, w4_2, w4_3] = x4.wrapping_add(s4).to_le_bytes();
    let [w5_0, w5_1, w5_2, w5_3] = x5.wrapping_add(s5).to_le_bytes();
    let [w6_0, w6_1, w6_2, w6_3] = x6.wrapping_add(s6).to_le_bytes();
    let [w7_0, w7_1, w7_2, w7_3] = x7.wrapping_add(s7).to_le_bytes();
    let [w8_0, w8_1, w8_2, w8_3] = x8.wrapping_add(s8).to_le_bytes();
    let [w9_0, w9_1, w9_2, w9_3] = x9.wrapping_add(s9).to_le_bytes();
    let [w10_0, w10_1, w10_2, w10_3] = x10.wrapping_add(s10).to_le_bytes();
    let [w11_0, w11_1, w11_2, w11_3] = x11.wrapping_add(s11).to_le_bytes();
    let [w12_0, w12_1, w12_2, w12_3] = x12.wrapping_add(s12).to_le_bytes();
    let [w13_0, w13_1, w13_2, w13_3] = x13.wrapping_add(s13).to_le_bytes();
    let [w14_0, w14_1, w14_2, w14_3] = x14.wrapping_add(s14).to_le_bytes();
    let [w15_0, w15_1, w15_2, w15_3] = x15.wrapping_add(s15).to_le_bytes();

    [
        w0_0, w0_1, w0_2, w0_3, w1_0, w1_1, w1_2, w1_3, w2_0, w2_1, w2_2, w2_3, w3_0, w3_1, w3_2,
        w3_3, w4_0, w4_1, w4_2, w4_3, w5_0, w5_1, w5_2, w5_3, w6_0, w6_1, w6_2, w6_3, w7_0, w7_1,
        w7_2, w7_3, w8_0, w8_1, w8_2, w8_3, w9_0, w9_1, w9_2, w9_3, w10_0, w10_1, w10_2, w10_3,
        w11_0, w11_1, w11_2, w11_3, w12_0, w12_1, w12_2, w12_3, w13_0, w13_1, w13_2, w13_3, w14_0,
        w14_1, w14_2, w14_3, w15_0, w15_1, w15_2, w15_3,
    ]
}

fn chacha20_xor_from_one(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], buf: &mut [u8]) {
    let mut counter = 1u32;
    for chunk in buf.chunks_mut(64) {
        let block = chacha20_block(key, counter, nonce);
        for (dst, &ks) in chunk.iter_mut().zip(block.iter()) {
            *dst ^= ks;
        }
        counter = counter.wrapping_add(1);
    }
}

// ---------------------------------------------------------------------------
// Poly1305 (RFC 8439 §2.5), constant-time 3-limb radix-2^44 over GF(2^130 - 5)
// ---------------------------------------------------------------------------

const M44: u64 = (1u64 << 44) - 1;
const M42: u64 = (1u64 << 42) - 1;
const M44_128: u128 = (1u128 << 44) - 1;
const M42_128: u128 = (1u128 << 42) - 1;

#[inline]
const fn low_u64(x: u128) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = x.to_le_bytes();
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

struct Poly1305 {
    r0: u64,
    r1: u64,
    r2: u64,
    s1: u64,
    s2: u64,
    pad: [u8; 16],
    h0: u64,
    h1: u64,
    h2: u64,
}

impl Poly1305 {
    const fn new(otk: [u8; 32]) -> Self {
        let [
            b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15, p0, p1, p2, p3,
            p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15,
        ] = otk;
        let lo = u64::from_le_bytes([b0, b1, b2, b3 & 0x0f, b4 & 0xfc, b5, b6, b7 & 0x0f]);
        let hi = u64::from_le_bytes([b8 & 0xfc, b9, b10, b11 & 0x0f, b12 & 0xfc, b13, b14, b15 & 0x0f]);
        let r0 = lo & M44;
        let r1 = ((lo >> 44) | (hi << 20)) & M44;
        let r2 = hi >> 24;
        Self {
            r0,
            r1,
            r2,
            s1: r1 * 20,
            s2: r2 * 20,
            pad: [p0, p1, p2, p3, p4, p5, p6, p7, p8, p9, p10, p11, p12, p13, p14, p15],
            h0: 0,
            h1: 0,
            h2: 0,
        }
    }

    fn update_full_block(&mut self, block: [u8; 16]) {
        let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15] = block;
        let blo = u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
        let bhi = u64::from_le_bytes([b8, b9, b10, b11, b12, b13, b14, b15]);

        let h0 = u128::from(self.h0 + (blo & M44));
        let h1 = u128::from(self.h1 + (((blo >> 44) | (bhi << 20)) & M44));
        let h2 = u128::from(self.h2 + ((bhi >> 24) | (1u64 << 40)));

        let r0 = u128::from(self.r0);
        let r1 = u128::from(self.r1);
        let r2 = u128::from(self.r2);
        let s1 = u128::from(self.s1);
        let s2 = u128::from(self.s2);

        let d0 = h0 * r0 + h1 * s2 + h2 * s1;
        let d1 = h0 * r1 + h1 * r0 + h2 * s2;
        let d2 = h0 * r2 + h1 * r1 + h2 * r0;

        let next_h0_raw = d0 & M44_128;
        let carry0 = d0 >> 44;

        let t1 = d1 + carry0;
        let next_h1_base = t1 & M44_128;
        let carry1 = t1 >> 44;

        let t2 = d2 + carry1;
        let next_h2 = t2 & M42_128;
        let carry2 = t2 >> 42;

        let t0 = next_h0_raw + carry2 * 5;
        let next_h0 = t0 & M44_128;
        let carry3 = t0 >> 44;

        self.h0 = low_u64(next_h0);
        self.h1 = low_u64(next_h1_base + carry3);
        self.h2 = low_u64(next_h2);
    }

    fn update_padded(&mut self, data: &[u8]) {
        let mut chunks = data.chunks_exact(16);
        for chunk in &mut chunks {
            if let Ok(block) = <[u8; 16]>::try_from(chunk) {
                self.update_full_block(block);
            }
        }
        let rem = chunks.remainder();
        if !rem.is_empty() {
            let mut padded = [0u8; 16];
            if let Some(dst) = padded.get_mut(..rem.len()) {
                dst.copy_from_slice(rem);
            }
            self.update_full_block(padded);
        }
    }

    const fn finish(self) -> [u8; TAG_LEN] {
        let mut h0 = self.h0;
        let mut h1 = self.h1;
        let mut h2 = self.h2;

        let c1 = h1 >> 44;
        h1 &= M44;
        h2 += c1;

        let c2 = h2 >> 42;
        h2 &= M42;
        h0 += c2 * 5;

        let c3 = h0 >> 44;
        h0 &= M44;
        h1 += c3;

        let c4 = h1 >> 44;
        h1 &= M44;
        h2 += c4;

        // Constant-time conditional subtraction of p = 2^130 - 5.
        let mut g0 = h0.wrapping_add(5);
        let gc0 = g0 >> 44;
        g0 &= M44;

        let mut g1 = h1.wrapping_add(gc0);
        let gc1 = g1 >> 44;
        g1 &= M44;

        let g2 = h2.wrapping_add(gc1);
        let mask = (g2 >> 42).wrapping_neg();

        let f0 = (h0 & !mask) | (g0 & mask);
        let f1 = (h1 & !mask) | (g1 & mask);
        let f2 = (h2 & !mask) | ((g2 & M42) & mask);

        let h_lo = f0 | (f1 << 44);
        let h_hi = (f1 >> 20) | (f2 << 24);
        let [l0, l1, l2, l3, l4, l5, l6, l7] = h_lo.to_le_bytes();
        let [u0, u1, u2, u3, u4, u5, u6, u7] = h_hi.to_le_bytes();
        let h128 = u128::from_le_bytes([
            l0, l1, l2, l3, l4, l5, l6, l7, u0, u1, u2, u3, u4, u5, u6, u7,
        ]);
        let s128 = u128::from_le_bytes(self.pad);
        h128.wrapping_add(s128).to_le_bytes()
    }
}

#[inline]
fn usize_to_le64(len: usize) -> [u8; 8] {
    match u64::try_from(len) {
        Ok(v) => v.to_le_bytes(),
        Err(_) => u64::MAX.to_le_bytes(),
    }
}

#[inline]
const fn poly1305_key_gen(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN]) -> [u8; 32] {
    let [
        k0, k1, k2, k3, k4, k5, k6, k7, k8, k9, k10, k11, k12, k13, k14, k15, k16, k17, k18, k19,
        k20, k21, k22, k23, k24, k25, k26, k27, k28, k29, k30, k31, ..,
    ] = chacha20_block(key, 0, nonce);
    [
        k0, k1, k2, k3, k4, k5, k6, k7, k8, k9, k10, k11, k12, k13, k14, k15, k16, k17, k18, k19,
        k20, k21, k22, k23, k24, k25, k26, k27, k28, k29, k30, k31,
    ]
}

fn compute_aead_tag(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
    plant_skip_aad: bool,
) -> [u8; TAG_LEN] {
    let otk = poly1305_key_gen(key, nonce);
    let mut poly = Poly1305::new(otk);
    if !plant_skip_aad {
        poly.update_padded(aad);
    }
    poly.update_padded(ciphertext);
    let aad_len = if plant_skip_aad { 0 } else { aad.len() };
    let [a0, a1, a2, a3, a4, a5, a6, a7] = usize_to_le64(aad_len);
    let [c0, c1, c2, c3, c4, c5, c6, c7] = usize_to_le64(ciphertext.len());
    poly.update_full_block([a0, a1, a2, a3, a4, a5, a6, a7, c0, c1, c2, c3, c4, c5, c6, c7]);
    poly.finish()
}

/// RFC 8439 ChaCha20-Poly1305 Authenticated Encryption with Associated Data (AEAD).
pub struct ChaCha20Poly1305 {
    key: [u8; KEY_LEN],
}

impl fmt::Debug for ChaCha20Poly1305 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChaCha20Poly1305 { .. }")
    }
}

impl ChaCha20Poly1305 {
    /// Initialize a ChaCha20-Poly1305 AEAD instance with a 32-byte key.
    #[inline]
    #[must_use]
    pub const fn new(key: [u8; KEY_LEN]) -> Self {
        Self { key }
    }

    /// Encrypt `buf` in place under `(nonce, aad)` and return the 16-byte Poly1305 tag.
    #[must_use]
    pub fn seal_in_place(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        buf: &mut [u8],
    ) -> [u8; TAG_LEN] {
        chacha20_xor_from_one(&self.key, &nonce, buf);
        compute_aead_tag(&self.key, &nonce, aad, buf, false)
    }

    /// Authenticate `buf` and `aad` against `tag` in constant time; decrypt `buf` in place on
    /// success, or wipe `buf` to zeros and return [`AuthError`] on tag mismatch.
    pub fn open_in_place(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), AuthError> {
        self.open_in_place_planted(nonce, aad, buf, tag, false)
    }

    fn open_in_place_planted(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8; TAG_LEN],
        plant_skip_aad: bool,
    ) -> Result<(), AuthError> {
        let expected = compute_aead_tag(&self.key, &nonce, aad, buf, plant_skip_aad);
        if !ct_eq(&expected, tag) {
            for slot in buf.iter_mut() {
                *slot = 0;
            }
            core::hint::black_box(buf);
            return Err(AuthError);
        }
        chacha20_xor_from_one(&self.key, &nonce, buf);
        Ok(())
    }

    /// Encrypt `plaintext` under `(nonce, aad)` into a new [`Vec<u8, A>`] in `alloc` with the
    /// 16-byte authentication tag appended.
    pub fn encrypt_in<A: Allocator>(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        plaintext: &[u8],
        alloc: A,
    ) -> Result<Vec<u8, A>, AllocFailure> {
        let total = plaintext.len().saturating_add(TAG_LEN);
        let mut out = Vec::try_with_capacity_in(total, alloc)?;
        out.try_extend_from_slice(plaintext)?;
        let tag = self.seal_in_place(nonce, aad, out.as_mut_slice());
        out.try_extend_from_slice(&tag)?;
        Ok(out)
    }

    /// Authenticate and decrypt `ciphertext_and_tag` (ciphertext followed by its 16-byte tag)
    /// into a new [`Vec<u8, A>`] in `alloc`.
    pub fn decrypt_in<A: Allocator>(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        ciphertext_and_tag: &[u8],
        alloc: A,
    ) -> Result<Vec<u8, A>, CryptoError> {
        let Some(ct_len) = ciphertext_and_tag.len().checked_sub(TAG_LEN) else {
            return Err(CryptoError::CiphertextTooShort { len: ciphertext_and_tag.len() });
        };
        let Some((ct, tag_slice)) = ciphertext_and_tag.split_at_checked(ct_len) else {
            return Err(CryptoError::CiphertextTooShort { len: ciphertext_and_tag.len() });
        };
        let Ok(tag): Result<[u8; TAG_LEN], _> = tag_slice.try_into() else {
            return Err(CryptoError::CiphertextTooShort { len: ciphertext_and_tag.len() });
        };
        let mut out = Vec::try_with_capacity_in(ct.len(), alloc).map_err(CryptoError::Alloc)?;
        out.try_extend_from_slice(ct).map_err(CryptoError::Alloc)?;
        self.open_in_place(nonce, aad, out.as_mut_slice(), &tag).map_err(CryptoError::Auth)?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// HKDF-SHA256 (RFC 5869)
// ---------------------------------------------------------------------------

/// RFC 5869 HMAC-based Extract-and-Expand Key Derivation Function over SHA-256.
pub struct HkdfSha256 {
    prk: [u8; SHA256_LEN],
}

impl fmt::Debug for HkdfSha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HkdfSha256 { .. }")
    }
}

impl HkdfSha256 {
    /// RFC 5869 §2.2 HKDF-Extract: derive a pseudorandom key from optional `salt` and input keying
    /// material `ikm`. An empty `salt` is treated as 32 zero bytes per the specification.
    #[must_use]
    pub fn extract(salt: &[u8], ikm: &[u8]) -> Self {
        let zero_salt = [0u8; SHA256_LEN];
        let effective_salt = if salt.is_empty() { zero_salt.as_slice() } else { salt };
        Self { prk: hmac_sha256(effective_salt, ikm) }
    }

    /// Construct directly from an already-extracted 32-byte pseudorandom key (`PRK`).
    #[inline]
    #[must_use]
    pub const fn from_prk(prk: [u8; SHA256_LEN]) -> Self {
        Self { prk }
    }

    /// Access the 32-byte pseudorandom key (`PRK`).
    #[inline]
    #[must_use]
    pub const fn prk(&self) -> &[u8; SHA256_LEN] {
        &self.prk
    }

    /// RFC 5869 §2.3 HKDF-Expand: fill `out` with output keying material bound to `info`.
    ///
    /// Refuses `out.len() > 255 * 32` with [`HkdfError::OutputTooLong`].
    pub fn expand(&self, info: &[u8], out: &mut [u8]) -> Result<(), HkdfError> {
        let out_len = out.len();
        if out_len > HKDF_MAX_OUTPUT_LEN {
            return Err(HkdfError::OutputTooLong {
                requested: out_len,
                max: HKDF_MAX_OUTPUT_LEN,
            });
        }
        if out.is_empty() {
            return Ok(());
        }
        let mut prev = [0u8; SHA256_LEN];
        for (idx, chunk) in out.chunks_mut(SHA256_LEN).enumerate() {
            let counter = u8::try_from(idx + 1).map_err(|_| HkdfError::OutputTooLong {
                requested: out_len,
                max: HKDF_MAX_OUTPUT_LEN,
            })?;
            let mut mac = HmacSha256::new(&self.prk);
            if idx > 0 {
                mac.update(&prev);
            }
            mac.update(info);
            mac.update(&[counter]);
            prev = mac.finish();
            if let Some(src) = prev.get(..chunk.len()) {
                chunk.copy_from_slice(src);
            }
        }
        Ok(())
    }
}

/// One-shot RFC 5869 HKDF-SHA256 extract + expand into `out`.
pub fn hkdf_sha256(
    salt: &[u8],
    ikm: &[u8],
    info: &[u8],
    out: &mut [u8],
) -> Result<(), HkdfError> {
    HkdfSha256::extract(salt, ikm).expand(info, out)
}

// ---------------------------------------------------------------------------
// PBKDF2-HMAC-SHA256 (RFC 2898 §5.2)
// ---------------------------------------------------------------------------

/// RFC 2898 PBKDF2-HMAC-SHA256 password-based key derivation.
///
/// Refuses `iterations == 0` ([`Pbkdf2Error::ZeroIterations`]) and `out.is_empty()`
/// ([`Pbkdf2Error::EmptyOutput`]).
pub fn pbkdf2_hmac_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), Pbkdf2Error> {
    if iterations == 0 {
        return Err(Pbkdf2Error::ZeroIterations);
    }
    if out.is_empty() {
        return Err(Pbkdf2Error::EmptyOutput);
    }

    // Pre-hash keys longer than the 64-byte SHA-256 block once so inner iterations avoid rehashing.
    let hashed_pw;
    let key_bytes: &[u8] = if password.len() > 64 {
        hashed_pw = sha256(password);
        &hashed_pw
    } else {
        password
    };

    let out_len = out.len();
    for (idx, chunk) in out.chunks_mut(SHA256_LEN).enumerate() {
        let block_num = u32::try_from(idx + 1)
            .map_err(|_| Pbkdf2Error::OutputTooLong { requested: out_len })?;
        let mut mac = HmacSha256::new(key_bytes);
        mac.update(salt);
        mac.update(&block_num.to_be_bytes());
        let mut u = mac.finish();
        let mut acc = u;

        for _ in 1..iterations {
            u = hmac_sha256(key_bytes, &u);
            for (dst, &src) in acc.iter_mut().zip(u.iter()) {
                *dst ^= src;
            }
        }

        if let Some(src) = acc.get(..chunk.len()) {
            chunk.copy_from_slice(src);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_own::{Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::*;
    use crate::hex::{decode_array, decode_in};

    #[test]
    fn rfc8439_section_2_8_2_chacha20_poly1305_vector() {
        let h = heap();
        let key = decode_array::<32>(
            b"808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f",
        )
        .expect("key");
        let nonce = decode_array::<12>(b"070000004041424344454647").expect("nonce");
        let aad = decode_array::<12>(b"50515253c0c1c2c3c4c5c6c7").expect("aad");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";

        let expected_ct = decode_in(
            b"d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
              3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
              92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
              3ff4def08e4b7a9de576d26586cec64b6116",
            h.alloc(),
        )
        .expect("expected ct");
        let expected_tag = decode_array::<16>(b"1ae10b594f09e26a7e902ecbd0600691").expect("tag");

        let aead = ChaCha20Poly1305::new(key);
        let mut buf = OwnVec::new_in(h.alloc());
        buf.extend_from_slice(plaintext);

        let tag = aead.seal_in_place(nonce, &aad, buf.as_mut_slice());
        assert_eq!(buf.as_slice(), expected_ct.as_slice());
        assert_eq!(tag, expected_tag);

        aead.open_in_place(nonce, &aad, buf.as_mut_slice(), &tag).expect("open");
        assert_eq!(buf.as_slice(), plaintext);

        // Encrypt/decrypt allocating API.
        let sealed = aead.encrypt_in(nonce, &aad, plaintext, h.alloc()).expect("encrypt_in");
        assert_eq!(sealed.len(), plaintext.len() + TAG_LEN);
        assert_eq!(&sealed.as_slice()[..plaintext.len()], expected_ct.as_slice());
        assert_eq!(&sealed.as_slice()[plaintext.len()..], &expected_tag);
        let opened = aead.decrypt_in(nonce, &aad, sealed.as_slice(), h.alloc()).expect("decrypt_in");
        assert_eq!(opened.as_slice(), plaintext);

        // Tampered tag must fail and wipe the buffer to zeros.
        let mut bad_tag = tag;
        bad_tag[0] ^= 0x01;
        let mut wiped = OwnVec::new_in(h.alloc());
        wiped.extend_from_slice(expected_ct.as_slice());
        assert_eq!(
            aead.open_in_place(nonce, &aad, wiped.as_mut_slice(), &bad_tag),
            Err(AuthError)
        );
        assert!(wiped.as_slice().iter().all(|&b| b == 0));

        // Short ciphertext must fail with CiphertextTooShort.
        assert_eq!(
            aead.decrypt_in(nonce, &aad, &[0u8; 15], h.alloc()),
            Err(CryptoError::CiphertextTooShort { len: 15 })
        );
    }

    #[test]
    fn rfc5869_hkdf_sha256_cases_1_2_3() {
        // Test Case 1
        let ikm1 = [0x0bu8; 22];
        let salt1 = decode_array::<13>(b"000102030405060708090a0b0c").expect("salt1");
        let info1 = decode_array::<10>(b"f0f1f2f3f4f5f6f7f8f9").expect("info1");
        let prk1_exp = decode_array::<32>(
            b"077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
        )
        .expect("prk1");
        let okm1_exp = decode_array::<42>(
            b"3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
              34007208d5b887185865",
        )
        .expect("okm1");
        let hkdf1 = HkdfSha256::extract(&salt1, &ikm1);
        assert_eq!(hkdf1.prk(), &prk1_exp);
        let mut okm1 = [0u8; 42];
        hkdf1.expand(&info1, &mut okm1).expect("expand1");
        assert_eq!(okm1, okm1_exp);

        // Test Case 2 (longer inputs/outputs)
        let mut ikm2 = [0u8; 80];
        let mut salt2 = [0u8; 80];
        let mut info2 = [0u8; 80];
        for i in 0..80usize {
            let b = u8::try_from(i).unwrap_or(0);
            ikm2[i] = b;
            salt2[i] = 0x60 + b;
            info2[i] = 0xb0 + b;
        }
        let prk2_exp = decode_array::<32>(
            b"06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244",
        )
        .expect("prk2");
        let okm2_exp = decode_array::<82>(
            b"b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c\
              59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71\
              cc30c58179ec3e87c14c01d5c1f3434f1d87",
        )
        .expect("okm2");
        let hkdf2 = HkdfSha256::extract(&salt2, &ikm2);
        assert_eq!(hkdf2.prk(), &prk2_exp);
        let mut okm2 = [0u8; 82];
        hkdf2.expand(&info2, &mut okm2).expect("expand2");
        assert_eq!(okm2, okm2_exp);

        // Test Case 3 (zero-length salt and info)
        let prk3_exp = decode_array::<32>(
            b"19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
        )
        .expect("prk3");
        let okm3_exp = decode_array::<42>(
            b"8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d\
              9d201395faa4b61a96c8",
        )
        .expect("okm3");
        let hkdf3 = HkdfSha256::extract(b"", &ikm1);
        assert_eq!(hkdf3.prk(), &prk3_exp);
        let mut okm3 = [0u8; 42];
        hkdf_sha256(b"", &ikm1, b"", &mut okm3).expect("oneshot3");
        assert_eq!(okm3, okm3_exp);

        // from_prk matches extract
        let mut okm3_from_prk = [0u8; 42];
        HkdfSha256::from_prk(prk3_exp).expand(b"", &mut okm3_from_prk).expect("from_prk");
        assert_eq!(okm3_from_prk, okm3_exp);

        // Refuses output > 255 * 32
        let mut too_big = [0u8; HKDF_MAX_OUTPUT_LEN + 1];
        assert_eq!(
            hkdf1.expand(b"", &mut too_big),
            Err(HkdfError::OutputTooLong {
                requested: HKDF_MAX_OUTPUT_LEN + 1,
                max: HKDF_MAX_OUTPUT_LEN,
            })
        );
    }

    #[test]
    fn rfc6070_pbkdf2_hmac_sha256_vectors() {
        let mut dk = [0u8; 32];

        pbkdf2_hmac_sha256(b"password", b"salt", 1, &mut dk).expect("c=1");
        assert_eq!(
            dk,
            decode_array::<32>(
                b"120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
            )
            .expect("v1")
        );

        pbkdf2_hmac_sha256(b"password", b"salt", 2, &mut dk).expect("c=2");
        assert_eq!(
            dk,
            decode_array::<32>(
                b"ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
            )
            .expect("v2")
        );

        pbkdf2_hmac_sha256(b"password", b"salt", 4096, &mut dk).expect("c=4096");
        assert_eq!(
            dk,
            decode_array::<32>(
                b"c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
            )
            .expect("v4096")
        );

        let mut dk40 = [0u8; 40];
        pbkdf2_hmac_sha256(
            b"passwordPASSWORDpassword",
            b"saltSALTsaltSALTsaltSALTsaltSALTsalt",
            4096,
            &mut dk40,
        )
        .expect("long");
        assert_eq!(
            dk40,
            decode_array::<40>(
                b"348c89dbcbd32b2f32d814b8116e84cf2b17347ebc1800181c4e2a1fb8dd53e1\
                  c635518c7dac47e9"
            )
            .expect("vlong")
        );

        assert_eq!(
            pbkdf2_hmac_sha256(b"password", b"salt", 0, &mut dk),
            Err(Pbkdf2Error::ZeroIterations)
        );
        assert_eq!(
            pbkdf2_hmac_sha256(b"password", b"salt", 1, &mut []),
            Err(Pbkdf2Error::EmptyOutput)
        );
    }

    #[test]
    fn ed25519_reexport_smoke() {
        // Re-export of `blaze-ed25519` refuses invalid/small-order keys and signatures.
        assert!(ed25519::verify(&[0u8; 32], b"blaze", &[0u8; 64]).is_err());
    }

    #[test]
    fn property_aead_round_trip_and_single_bit_tamper_rejection() {
        let h = heap();
        blaze_sim::check(Config::cases(128), |tape| {
            let mut key = [0u8; KEY_LEN];
            let mut nonce = [0u8; NONCE_LEN];
            tape.fill(&mut key, "key");
            tape.fill(&mut nonce, "nonce");

            let aad_len = usize::try_from(tape.int_in(0..=48, "aad_len")).unwrap_or(0);
            let pt_len = usize::try_from(tape.int_in(0..=160, "pt_len")).unwrap_or(0);
            let mut aad = OwnVec::new_in(h.alloc());
            for _ in 0..aad_len {
                aad.push(u8::try_from(tape.below(256, "aad")).unwrap_or(0));
            }
            let mut pt = OwnVec::new_in(h.alloc());
            for _ in 0..pt_len {
                pt.push(u8::try_from(tape.below(256, "pt")).unwrap_or(0));
            }

            let aead = ChaCha20Poly1305::new(key);
            let sealed = aead
                .encrypt_in(nonce, aad.as_slice(), pt.as_slice(), h.alloc())
                .expect("encrypt");
            let opened = aead
                .decrypt_in(nonce, aad.as_slice(), sealed.as_slice(), h.alloc())
                .expect("decrypt");
            assert_eq!(opened.as_slice(), pt.as_slice());

            // Flip 1 bit in tag -> must be refused.
            let mut bad_tag_ct = OwnVec::new_in(h.alloc());
            bad_tag_ct.extend_from_slice(sealed.as_slice());
            let tag_pos = pt_len + tape.index(TAG_LEN, "tag_pos");
            let tag_bit = 1u8 << u8::try_from(tape.below(8, "tag_bit")).unwrap_or(0);
            bad_tag_ct.as_mut_slice()[tag_pos] ^= tag_bit;
            assert_eq!(
                aead.decrypt_in(nonce, aad.as_slice(), bad_tag_ct.as_slice(), h.alloc()),
                Err(CryptoError::Auth(AuthError))
            );

            // Flip 1 bit in ciphertext (when non-empty) -> must be refused and wiped.
            if pt_len > 0 {
                let mut ct_buf = OwnVec::new_in(h.alloc());
                ct_buf.extend_from_slice(&sealed.as_slice()[..pt_len]);
                let tag: [u8; TAG_LEN] = sealed.as_slice()[pt_len..].try_into().expect("tag");
                let ct_pos = tape.index(pt_len, "ct_pos");
                let ct_bit = 1u8 << u8::try_from(tape.below(8, "ct_bit")).unwrap_or(0);
                ct_buf.as_mut_slice()[ct_pos] ^= ct_bit;
                assert_eq!(
                    aead.open_in_place(nonce, aad.as_slice(), ct_buf.as_mut_slice(), &tag),
                    Err(AuthError)
                );
                assert!(ct_buf.as_slice().iter().all(|&b| b == 0));
            }

            // Flip 1 bit in AAD (when non-empty) -> must be refused.
            if aad_len > 0 {
                let mut bad_aad = OwnVec::new_in(h.alloc());
                bad_aad.extend_from_slice(aad.as_slice());
                let aad_pos = tape.index(aad_len, "aad_pos");
                let aad_bit = 1u8 << u8::try_from(tape.below(8, "aad_bit")).unwrap_or(0);
                bad_aad.as_mut_slice()[aad_pos] ^= aad_bit;
                assert_eq!(
                    aead.decrypt_in(nonce, bad_aad.as_slice(), sealed.as_slice(), h.alloc()),
                    Err(CryptoError::Auth(AuthError))
                );
            }
        });
    }

    #[test]
    fn the_oracle_catches_unauthenticated_aad() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let mut key = [0u8; KEY_LEN];
            let mut nonce = [0u8; NONCE_LEN];
            tape.fill(&mut key, "key");
            tape.fill(&mut nonce, "nonce");

            let aad_len = usize::try_from(tape.int_in(1..=32, "aad_len")).unwrap_or(1);
            let pt_len = usize::try_from(tape.int_in(1..=48, "pt_len")).unwrap_or(1);
            let mut aad = OwnVec::new_in(h.alloc());
            for _ in 0..aad_len {
                aad.push(u8::try_from(tape.below(256, "aad")).unwrap_or(0));
            }
            let mut pt = OwnVec::new_in(h.alloc());
            for _ in 0..pt_len {
                pt.push(u8::try_from(tape.below(256, "pt")).unwrap_or(0));
            }

            let aead = ChaCha20Poly1305::new(key);
            let mut ct = OwnVec::new_in(h.alloc());
            ct.extend_from_slice(pt.as_slice());
            let tag = compute_aead_tag(&key, &nonce, aad.as_slice(), ct.as_slice(), plant);
            let mut check_buf = OwnVec::new_in(h.alloc());
            check_buf.extend_from_slice(ct.as_slice());
            let mut tampered_aad = OwnVec::new_in(h.alloc());
            tampered_aad.extend_from_slice(aad.as_slice());
            tampered_aad.as_mut_slice()[0] ^= 0x80;

            let verdict = aead.open_in_place_planted(
                nonce,
                tampered_aad.as_slice(),
                check_buf.as_mut_slice(),
                &tag,
                plant,
            );
            if verdict.is_ok() {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "accepted tampered AAD of length {aad_len}")
                        .as_str()
                        .into(),
                ));
            }
            Ok(())
        });
    }
}

//! Pure `#![no_std]` modern authenticated encryption, key exchange, key derivation, and Ed25519 verification:
//!
//! - **Ed25519** signature verification: re-exported from [`blaze_ed25519`] as [`ed25519`].
//! - **ChaCha20-Poly1305 AEAD** (RFC 8439): [`ChaCha20Poly1305`], [`KEY_LEN`], [`NONCE_LEN`],
//!   [`TAG_LEN`], [`AuthError`], [`CryptoError`].
//! - **XChaCha20-Poly1305 AEAD** (draft-irtf-cfrg-xchacha): [`hchacha20`], [`XChaCha20Poly1305`],
//!   [`XNONCE_LEN`] (192-bit random-safe nonces).
//! - **X25519** Diffie-Hellman key exchange (RFC 7748): [`x25519`], [`x25519_public_key`],
//!   [`x25519_checked`], [`X25519_BASEPOINT`], [`WeakSharedSecret`].
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

/// XChaCha20-Poly1305 extended nonce length in bytes (192 bits, safe for random generation).
pub const XNONCE_LEN: usize = 24;

/// ChaCha20-Poly1305 Poly1305 authentication tag length in bytes (128 bits).
pub const TAG_LEN: usize = 16;

/// Canonical RFC 7748 X25519 basepoint (`u = 9`).
pub const X25519_BASEPOINT: [u8; 32] = [
    9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Returned by [`x25519_checked`] when the peer public key produces the all-zero low-order shared secret.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WeakSharedSecret;

impl fmt::Display for WeakSharedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bstd::crypto::x25519: peer public key produced an all-zero shared secret")
    }
}

impl core::error::Error for WeakSharedSecret {}

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
// HChaCha20 & XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha §2.2–§2.3)
// ---------------------------------------------------------------------------

/// Compute the 32-byte HChaCha20 subkey from a 32-byte key and 16-byte nonce prefix
/// (draft-irtf-cfrg-xchacha §2.2).
#[must_use]
pub const fn hchacha20(key: &[u8; KEY_LEN], nonce16: &[u8; 16]) -> [u8; KEY_LEN] {
    let [
        k0, k1, k2, k3, k4, k5, k6, k7, k8, k9, k10, k11, k12, k13, k14, k15, k16, k17, k18, k19,
        k20, k21, k22, k23, k24, k25, k26, k27, k28, k29, k30, k31,
    ] = *key;
    let [
        n0, n1, n2, n3, n4, n5, n6, n7, n8, n9, n10, n11, n12, n13, n14, n15,
    ] = *nonce16;

    let mut x0: u32 = 0x6170_7865;
    let mut x1: u32 = 0x3320_646e;
    let mut x2: u32 = 0x7962_2d32;
    let mut x3: u32 = 0x6b20_6574;
    let mut x4 = u32::from_le_bytes([k0, k1, k2, k3]);
    let mut x5 = u32::from_le_bytes([k4, k5, k6, k7]);
    let mut x6 = u32::from_le_bytes([k8, k9, k10, k11]);
    let mut x7 = u32::from_le_bytes([k12, k13, k14, k15]);
    let mut x8 = u32::from_le_bytes([k16, k17, k18, k19]);
    let mut x9 = u32::from_le_bytes([k20, k21, k22, k23]);
    let mut x10 = u32::from_le_bytes([k24, k25, k26, k27]);
    let mut x11 = u32::from_le_bytes([k28, k29, k30, k31]);
    let mut x12 = u32::from_le_bytes([n0, n1, n2, n3]);
    let mut x13 = u32::from_le_bytes([n4, n5, n6, n7]);
    let mut x14 = u32::from_le_bytes([n8, n9, n10, n11]);
    let mut x15 = u32::from_le_bytes([n12, n13, n14, n15]);

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

    let [a0, a1, a2, a3] = x0.to_le_bytes();
    let [b0, b1, b2, b3] = x1.to_le_bytes();
    let [c0, c1, c2, c3] = x2.to_le_bytes();
    let [d0, d1, d2, d3] = x3.to_le_bytes();
    let [e0, e1, e2, e3] = x12.to_le_bytes();
    let [f0, f1, f2, f3] = x13.to_le_bytes();
    let [g0, g1, g2, g3] = x14.to_le_bytes();
    let [h0, h1, h2, h3] = x15.to_le_bytes();

    [
        a0, a1, a2, a3, b0, b1, b2, b3, c0, c1, c2, c3, d0, d1, d2, d3, e0, e1, e2, e3, f0, f1, f2,
        f3, g0, g1, g2, g3, h0, h1, h2, h3,
    ]
}

/// XChaCha20-Poly1305 AEAD cipher (draft-irtf-cfrg-xchacha §2.3) with 192-bit nonces safe for
/// random generation.
pub struct XChaCha20Poly1305 {
    key: [u8; KEY_LEN],
}

impl fmt::Debug for XChaCha20Poly1305 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("XChaCha20Poly1305([REDACTED])")
    }
}

impl Drop for XChaCha20Poly1305 {
    fn drop(&mut self) {
        for slot in self.key.iter_mut() {
            *slot = 0;
        }
        core::hint::black_box(&self.key);
    }
}

impl XChaCha20Poly1305 {
    /// Initialize an XChaCha20-Poly1305 AEAD instance with a 32-byte key.
    #[inline]
    #[must_use]
    pub const fn new(key: [u8; KEY_LEN]) -> Self {
        Self { key }
    }

    #[inline]
    const fn derive(&self, nonce: [u8; XNONCE_LEN]) -> (ChaCha20Poly1305, [u8; NONCE_LEN]) {
        let [
            n0, n1, n2, n3, n4, n5, n6, n7, n8, n9, n10, n11, n12, n13, n14, n15, n16, n17, n18,
            n19, n20, n21, n22, n23,
        ] = nonce;
        let subkey = hchacha20(
            &self.key,
            &[n0, n1, n2, n3, n4, n5, n6, n7, n8, n9, n10, n11, n12, n13, n14, n15],
        );
        let subnonce = [0, 0, 0, 0, n16, n17, n18, n19, n20, n21, n22, n23];
        (ChaCha20Poly1305::new(subkey), subnonce)
    }

    /// Encrypt `buf` in place under `(nonce, aad)` and return the 16-byte Poly1305 tag.
    #[must_use]
    pub fn seal_in_place(
        &self,
        nonce: [u8; XNONCE_LEN],
        aad: &[u8],
        buf: &mut [u8],
    ) -> [u8; TAG_LEN] {
        let (aead, subnonce) = self.derive(nonce);
        aead.seal_in_place(subnonce, aad, buf)
    }

    /// Authenticate and decrypt `buf` in place; wipes `buf` on authentication failure.
    pub fn open_in_place(
        &self,
        nonce: [u8; XNONCE_LEN],
        aad: &[u8],
        buf: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> Result<(), AuthError> {
        let (aead, subnonce) = self.derive(nonce);
        aead.open_in_place(subnonce, aad, buf, tag)
    }

    /// Encrypt `plaintext` under `(nonce, aad)` into a new [`Vec<u8, A>`] with the 16-byte tag appended.
    pub fn encrypt_in<A: Allocator>(
        &self,
        nonce: [u8; XNONCE_LEN],
        aad: &[u8],
        plaintext: &[u8],
        alloc: A,
    ) -> Result<Vec<u8, A>, AllocFailure> {
        let (aead, subnonce) = self.derive(nonce);
        aead.encrypt_in(subnonce, aad, plaintext, alloc)
    }

    /// Authenticate and decrypt `ciphertext_and_tag` into a new [`Vec<u8, A>`].
    pub fn decrypt_in<A: Allocator>(
        &self,
        nonce: [u8; XNONCE_LEN],
        aad: &[u8],
        ciphertext_and_tag: &[u8],
        alloc: A,
    ) -> Result<Vec<u8, A>, CryptoError> {
        let (aead, subnonce) = self.derive(nonce);
        aead.decrypt_in(subnonce, aad, ciphertext_and_tag, alloc)
    }
}

// ---------------------------------------------------------------------------
// X25519 Diffie-Hellman (RFC 7748) over GF(2^255 - 19) in 5x51-bit limbs
// ---------------------------------------------------------------------------

const MASK51: u64 = (1u64 << 51) - 1;
const MASK51_128: u128 = (1u128 << 51) - 1;

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

#[inline]
const fn u128_low51(x: u128) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = (x & MASK51_128).to_le_bytes();
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

#[inline]
const fn u128_low64(x: u128) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = x.to_le_bytes();
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

impl Fe {
    const ZERO: Self = Self([0, 0, 0, 0, 0]);
    const ONE: Self = Self([1, 0, 0, 0, 0]);

    const fn from_bytes(bytes: &[u8; 32]) -> Self {
        let [
            b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15, b16, b17, b18,
            b19, b20, b21, b22, b23, b24, b25, b26, b27, b28, b29, b30, b31,
        ] = *bytes;
        let l0 = u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
        let l1 = u64::from_le_bytes([b6, b7, b8, b9, b10, b11, b12, b13]);
        let l2 = u64::from_le_bytes([b12, b13, b14, b15, b16, b17, b18, b19]);
        let l3 = u64::from_le_bytes([b19, b20, b21, b22, b23, b24, b25, b26]);
        let l4 = u64::from_le_bytes([b24, b25, b26, b27, b28, b29, b30, b31]);
        // Mask top bit (bit 255) per RFC 7748 §5.
        Self([
            l0 & MASK51,
            (l1 >> 3) & MASK51,
            (l2 >> 6) & MASK51,
            (l3 >> 1) & MASK51,
            (l4 >> 12) & MASK51,
        ])
    }

    const fn add(self, rhs: Self) -> Self {
        let [a0, a1, a2, a3, a4] = self.0;
        let [b0, b1, b2, b3, b4] = rhs.0;
        Self([a0 + b0, a1 + b1, a2 + b2, a3 + b3, a4 + b4])
    }

    const fn sub(self, rhs: Self) -> Self {
        // Add 2*p to prevent underflow (inputs bounded < 2^52).
        const TWO_P0: u64 = 0x000f_ffff_ffff_ffda;
        const TWO_P1234: u64 = 0x000f_ffff_ffff_fffe;
        let [a0, a1, a2, a3, a4] = self.0;
        let [b0, b1, b2, b3, b4] = rhs.0;
        Self([
            (a0 + TWO_P0) - b0,
            (a1 + TWO_P1234) - b1,
            (a2 + TWO_P1234) - b2,
            (a3 + TWO_P1234) - b3,
            (a4 + TWO_P1234) - b4,
        ])
    }

    fn mul(self, rhs: Self) -> Self {
        let [s0, s1, s2, s3, s4] = self.0;
        let [o0, o1, o2, o3, o4] = rhs.0;
        let a0 = u128::from(s0);
        let a1 = u128::from(s1);
        let a2 = u128::from(s2);
        let a3 = u128::from(s3);
        let a4 = u128::from(s4);

        let b0 = u128::from(o0);
        let b1 = u128::from(o1);
        let b2 = u128::from(o2);
        let b3 = u128::from(o3);
        let b4 = u128::from(o4);

        let b1_19 = b1 * 19;
        let b2_19 = b2 * 19;
        let b3_19 = b3 * 19;
        let b4_19 = b4 * 19;

        let t0 = a0 * b0 + a1 * b4_19 + a2 * b3_19 + a3 * b2_19 + a4 * b1_19;
        let mut t1 = a0 * b1 + a1 * b0 + a2 * b4_19 + a3 * b3_19 + a4 * b2_19;
        let mut t2 = a0 * b2 + a1 * b1 + a2 * b0 + a3 * b4_19 + a4 * b3_19;
        let mut t3 = a0 * b3 + a1 * b2 + a2 * b1 + a3 * b0 + a4 * b4_19;
        let mut t4 = a0 * b4 + a1 * b3 + a2 * b2 + a3 * b1 + a4 * b0;

        t1 += t0 >> 51;
        let mut r0 = u128_low51(t0);
        t2 += t1 >> 51;
        let mut r1 = u128_low51(t1);
        t3 += t2 >> 51;
        let r2 = u128_low51(t2);
        t4 += t3 >> 51;
        let r3 = u128_low51(t3);
        let carry = u128_low64(t4 >> 51);
        let r4 = u128_low51(t4);

        r0 += carry * 19;
        r1 += r0 >> 51;
        r0 &= MASK51;

        Self([r0, r1, r2, r3, r4])
    }

    fn sq(self) -> Self {
        self.mul(self)
    }

    fn mul_a24(self) -> Self {
        const A24: u128 = 121_665;
        let [s0, s1, s2, s3, s4] = self.0;
        let t0 = u128::from(s0) * A24;
        let t1 = u128::from(s1) * A24 + (t0 >> 51);
        let t2 = u128::from(s2) * A24 + (t1 >> 51);
        let t3 = u128::from(s3) * A24 + (t2 >> 51);
        let t4 = u128::from(s4) * A24 + (t3 >> 51);

        let mut r0 = u128_low51(t0) + u128_low64(t4 >> 51) * 19;
        let r1 = u128_low51(t1) + (r0 >> 51);
        r0 &= MASK51;
        Self([r0, r1, u128_low51(t2), u128_low51(t3), u128_low51(t4)])
    }

    fn pow2k(mut self, mut k: u32) -> Self {
        while k > 0 {
            self = self.sq();
            k -= 1;
        }
        self
    }

    /// Compute `self^(p - 2) mod p` (multiplicative inverse in `GF(2^255 - 19)`).
    fn invert(self) -> Self {
        let t0 = self.sq();
        let t1 = t0.sq().sq();
        let t2 = self.mul(t1);
        let t3 = t0.mul(t2);
        let t4 = t3.sq();
        let t5 = t2.mul(t4);
        let t6 = t5.pow2k(5).mul(t5);
        let t7 = t6.pow2k(10).mul(t6);
        let t8 = t7.pow2k(20).mul(t7);
        let t9 = t8.pow2k(10).mul(t6);
        let t10 = t9.pow2k(50).mul(t9);
        let t11 = t10.pow2k(100).mul(t10);
        let t12 = t11.pow2k(50).mul(t9);
        t12.pow2k(5).mul(t3)
    }

    /// Canonical little-endian encoding modulo `2^255 - 19`.
    const fn to_bytes(self) -> [u8; 32] {
        let [mut h0, mut h1, mut h2, mut h3, mut h4] = self.0;

        // Two full carry passes reduce h into [0, 2^255 + 19*2).
        let mut pass = 0u8;
        while pass < 2 {
            h1 += h0 >> 51;
            h0 &= MASK51;
            h2 += h1 >> 51;
            h1 &= MASK51;
            h3 += h2 >> 51;
            h2 &= MASK51;
            h4 += h3 >> 51;
            h3 &= MASK51;
            h0 += (h4 >> 51) * 19;
            h4 &= MASK51;
            pass += 1;
        }

        // Subtract p if h >= p (constant-time).
        let mut q = (h0 + 19) >> 51;
        q = (h1 + q) >> 51;
        q = (h2 + q) >> 51;
        q = (h3 + q) >> 51;
        q = (h4 + q) >> 51;

        h0 += 19 * q;
        h1 += h0 >> 51;
        h0 &= MASK51;
        h2 += h1 >> 51;
        h1 &= MASK51;
        h3 += h2 >> 51;
        h2 &= MASK51;
        h4 += h3 >> 51;
        h3 &= MASK51;
        h4 &= MASK51;

        let [w0_0, w0_1, w0_2, w0_3, w0_4, w0_5, w0_6, w0_7] = (h0 | (h1 << 51)).to_le_bytes();
        let [w1_0, w1_1, w1_2, w1_3, w1_4, w1_5, w1_6, w1_7] =
            ((h1 >> 13) | (h2 << 38)).to_le_bytes();
        let [w2_0, w2_1, w2_2, w2_3, w2_4, w2_5, w2_6, w2_7] =
            ((h2 >> 26) | (h3 << 25)).to_le_bytes();
        let [w3_0, w3_1, w3_2, w3_3, w3_4, w3_5, w3_6, w3_7] =
            ((h3 >> 39) | (h4 << 12)).to_le_bytes();
        [
            w0_0, w0_1, w0_2, w0_3, w0_4, w0_5, w0_6, w0_7, w1_0, w1_1, w1_2, w1_3, w1_4, w1_5,
            w1_6, w1_7, w2_0, w2_1, w2_2, w2_3, w2_4, w2_5, w2_6, w2_7, w3_0, w3_1, w3_2, w3_3,
            w3_4, w3_5, w3_6, w3_7,
        ]
    }
}

#[inline]
const fn cswap(swap: u64, a: &mut Fe, b: &mut Fe) {
    let mask = 0u64.wrapping_sub(swap);
    let [mut a0, mut a1, mut a2, mut a3, mut a4] = a.0;
    let [mut b0, mut b1, mut b2, mut b3, mut b4] = b.0;
    let x0 = mask & (a0 ^ b0);
    a0 ^= x0;
    b0 ^= x0;
    let x1 = mask & (a1 ^ b1);
    a1 ^= x1;
    b1 ^= x1;
    let x2 = mask & (a2 ^ b2);
    a2 ^= x2;
    b2 ^= x2;
    let x3 = mask & (a3 ^ b3);
    a3 ^= x3;
    b3 ^= x3;
    let x4 = mask & (a4 ^ b4);
    a4 ^= x4;
    b4 ^= x4;
    a.0 = [a0, a1, a2, a3, a4];
    b.0 = [b0, b1, b2, b3, b4];
}

/// RFC 7748 X25519 scalar multiplication (`scalar * u_coord` on Curve25519) in constant time.
///
/// Applies RFC 7748 §5 clamping to `scalar` and clears bit 255 of `u_coord`. For authenticated
/// key exchange where low-order peer keys must be refused, prefer [`x25519_checked`].
#[must_use]
pub fn x25519(mut scalar: [u8; 32], u_coord: [u8; 32]) -> [u8; 32] {
    if let Some(first) = scalar.first_mut() {
        *first &= 248;
    }
    if let Some(last) = scalar.last_mut() {
        *last &= 127;
        *last |= 64;
    }

    let x_1 = Fe::from_bytes(&u_coord);
    let mut x_2 = Fe::ONE;
    let mut z_2 = Fe::ZERO;
    let mut x_3 = x_1;
    let mut z_3 = Fe::ONE;
    let mut swap = 0u64;

    let mut t = 255usize;
    while t > 0 {
        t -= 1;
        let byte = scalar.get(t >> 3).copied().unwrap_or(0);
        let k_t = u64::from((byte >> (t & 7)) & 1);
        swap ^= k_t;
        cswap(swap, &mut x_2, &mut x_3);
        cswap(swap, &mut z_2, &mut z_3);
        swap = k_t;

        let a = x_2.add(z_2);
        let aa = a.sq();
        let b = x_2.sub(z_2);
        let bb = b.sq();
        let e = aa.sub(bb);
        let c = x_3.add(z_3);
        let d = x_3.sub(z_3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x_3 = da.add(cb).sq();
        z_3 = x_1.mul(da.sub(cb).sq());
        x_2 = aa.mul(bb);
        z_2 = e.mul(aa.add(e.mul_a24()));
    }

    cswap(swap, &mut x_2, &mut x_3);
    cswap(swap, &mut z_2, &mut z_3);

    x_2.mul(z_2.invert()).to_bytes()
}

/// Derive the 32-byte X25519 public key from a 32-byte secret key (`x25519(secret, X25519_BASEPOINT)`).
#[inline]
#[must_use]
pub fn x25519_public_key(secret: [u8; 32]) -> [u8; 32] {
    x25519(secret, X25519_BASEPOINT)
}

/// RFC 7748 X25519 Diffie-Hellman key agreement that refuses the all-zero low-order shared secret
/// in constant time (RFC 7748 §6.1).
pub fn x25519_checked(
    secret: [u8; 32],
    peer_public: [u8; 32],
) -> Result<[u8; 32], WeakSharedSecret> {
    let shared = x25519(secret, peer_public);
    if ct_eq(&shared, &[0u8; 32]) {
        return Err(WeakSharedSecret);
    }
    Ok(shared)
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

    /// The invariant: AEAD seal/open round-trips arbitrary plaintext and rejects any single-bit tamper in ciphertext or AAD.
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

    #[test]
    fn xchacha20_poly1305_and_hchacha20_draft_vectors() {
        let h = heap();
        // draft-irtf-cfrg-xchacha-03 §2.2.1 HChaCha20 test vector
        let key = crate::hex::decode_array::<32>(
            b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
        .expect("key");
        let nonce16 = crate::hex::decode_array::<16>(b"000000090000004a0000000031415927")
            .expect("nonce16");
        let expected_subkey = crate::hex::decode_array::<32>(
            b"82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc",
        )
        .expect("subkey");
        assert_eq!(hchacha20(&key, &nonce16), expected_subkey);

        // draft-irtf-cfrg-xchacha-03 §A.3.1 XChaCha20-Poly1305 AEAD vector
        let aead_key = crate::hex::decode_array::<32>(
            b"808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f",
        )
        .expect("aead_key");
        let nonce24 = crate::hex::decode_array::<24>(
            b"404142434445464748494a4b4c4d4e4f5051525354555657",
        )
        .expect("nonce24");
        let aad = crate::hex::decode_array::<12>(b"50515253c0c1c2c3c4c5c6c7").expect("aad");
        let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let expected_ct = crate::hex::decode_in(
            b"bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b4522f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff921f9664c97637da9768812f615c68b13b52e",
            h.alloc(),
        )
        .expect("ct");
        let expected_tag = crate::hex::decode_array::<16>(b"c0875924c1c7987947deafd8780acf49")
            .expect("tag");

        let x = XChaCha20Poly1305::new(aead_key);
        let sealed = x.encrypt_in(nonce24, &aad, pt, h.alloc()).expect("seal");
        assert_eq!(&sealed.as_slice()[..pt.len()], expected_ct.as_slice());
        assert_eq!(&sealed.as_slice()[pt.len()..], &expected_tag);
        let opened = x.decrypt_in(nonce24, &aad, sealed.as_slice(), h.alloc()).expect("open");
        assert_eq!(opened.as_slice(), pt);
    }

    #[test]
    fn rfc7748_x25519_vectors_and_commutativity() {
        // RFC 7748 §5.2 Vector 1
        let s1 = crate::hex::decode_array::<32>(
            b"a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4",
        )
        .expect("s1");
        let u1 = crate::hex::decode_array::<32>(
            b"e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c",
        )
        .expect("u1");
        let out1 = crate::hex::decode_array::<32>(
            b"c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552",
        )
        .expect("out1");
        assert_eq!(x25519(s1, u1), out1);

        // RFC 7748 §5.2 Vector 2
        let s2 = crate::hex::decode_array::<32>(
            b"4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d",
        )
        .expect("s2");
        let u2 = crate::hex::decode_array::<32>(
            b"e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493",
        )
        .expect("u2");
        let out2 = crate::hex::decode_array::<32>(
            b"95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957",
        )
        .expect("out2");
        assert_eq!(x25519(s2, u2), out2);

        // RFC 7748 §5.2 1,000-iteration test
        let mut k = X25519_BASEPOINT;
        let mut u = X25519_BASEPOINT;
        for _ in 0..1000 {
            let next = x25519(k, u);
            u = k;
            k = next;
        }
        let expected_1000 = crate::hex::decode_array::<32>(
            b"684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51",
        )
        .expect("1000");
        assert_eq!(k, expected_1000);

        // RFC 7748 §6.1 Alice/Bob DH key agreement
        let alice_priv = crate::hex::decode_array::<32>(
            b"77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
        )
        .expect("a_priv");
        let alice_pub_expected = crate::hex::decode_array::<32>(
            b"8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a",
        )
        .expect("a_pub");
        let bob_priv = crate::hex::decode_array::<32>(
            b"5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
        )
        .expect("b_priv");
        let bob_pub_expected = crate::hex::decode_array::<32>(
            b"de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
        )
        .expect("b_pub");
        let shared_expected = crate::hex::decode_array::<32>(
            b"4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742",
        )
        .expect("shared");

        let alice_pub = x25519_public_key(alice_priv);
        let bob_pub = x25519_public_key(bob_priv);
        assert_eq!(alice_pub, alice_pub_expected);
        assert_eq!(bob_pub, bob_pub_expected);
        assert_eq!(x25519_checked(alice_priv, bob_pub), Ok(shared_expected));
        assert_eq!(x25519_checked(bob_priv, alice_pub), Ok(shared_expected));

        // Low-order point (all-zero) must be refused by x25519_checked.
        assert_eq!(x25519_checked(alice_priv, [0u8; 32]), Err(WeakSharedSecret));

        // Property: random Alice/Bob keypairs always agree on the same shared secret.
        blaze_sim::check(Config::cases(32), |tape| {
            let mut a = [0u8; 32];
            let mut b = [0u8; 32];
            tape.fill(&mut a, "alice");
            tape.fill(&mut b, "bob");
            let a_pub = x25519_public_key(a);
            let b_pub = x25519_public_key(b);
            assert_eq!(x25519(a, b_pub), x25519(b, a_pub));
        });
    }
}

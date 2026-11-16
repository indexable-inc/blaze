//! The field of integers modulo p = 2^255 - 19.
//!
//! The representation is five unsigned 51-bit limbs, value = l0 + l1 2^51 + l2 2^102 + l3 2^153
//! + l4 2^204, with products formed in 128 bits: the usual fast form on 64-bit machines (one
//! 64 x 64 -> 128 multiply is `mul` and `umulh` on AArch64, one `mul` on x86-64), with room in
//! each limb for the carries of an addition. A product reduces by 2^255 = 19 (mod p), so the
//! high limbs fold back into the low ones multiplied by 19.
//!
//! Every operation but `add` returns limbs below 2^52 ("weakly reduced"); `add` skips its carry
//! chain and returns their sum, and every operation accepts limbs below 2^54, the sum of up to
//! three weakly reduced elements. The value is canonical (below p) only in [`Fe::to_bytes`], so two elements are
//! compared by their bytes ([`Fe::equals`]), never by their limbs.
//!
//! Every function is `const`, so the base point's table is computed by the compiler from the
//! base point's encoding (`point.rs`), not typed in. None is constant-time where it matters:
//! verification handles only public values (the crate's doc).

use blaze_int::Cast;

const MASK: u64 = (1 << 51) - 1;

/// An element of GF(2^255 - 19): each limb below 2^52, or below 2^54 for a sum (`add`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fe(pub(crate) [u64; 5]);

const fn wide(x: u64) -> u128 {
    Cast::<u64, u128>::widen(x)
}

const fn low(x: u128) -> u64 {
    Cast::<u128, u64>::truncate(x)
}

/// The 128-bit product of two limbs.
const fn m(x: u64, y: u64) -> u128 {
    wide(x) * wide(y)
}

/// The eight bytes at `at`, little-endian.
const fn load8(bytes: &[u8; 32], at: usize) -> u64 {
    let mut word = 0u64;
    let mut i = 0;
    while i < 8 {
        word |= Cast::<u8, u64>::widen(bytes[at + i]) << (8 * i);
        i += 1;
    }
    word
}

/// Whether two byte strings are equal (`==` on arrays is not `const`).
pub(crate) const fn bytes_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut i = 0;
    while i < 32 {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Limbs from five 128-bit sums: carries each into the next and the last, times 19, into the
/// first (2^255 = 19 mod p). Accepts sums below 2^121.
const fn carry_wide(c: [u128; 5]) -> Fe {
    let mask = wide(MASK);
    let c1 = c[1] + (c[0] >> 51);
    let c2 = c[2] + (c1 >> 51);
    let c3 = c[3] + (c2 >> 51);
    let c4 = c[4] + (c3 >> 51);
    let l0 = (c[0] & mask) + (c4 >> 51) * 19;
    let l1 = (c1 & mask) + (l0 >> 51);
    Fe([low(l0 & mask), low(l1), low(c2 & mask), low(c3 & mask), low(c4 & mask)])
}

impl Fe {
    pub(crate) const ZERO: Fe = Fe([0; 5]);
    pub(crate) const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// The element whose little-endian encoding is `bytes`, with bit 255 ignored (RFC 8032,
    /// section 5.1.3, step 1, takes it as x's sign). A value of p or above is read as itself
    /// modulo p: whether `bytes` was canonical is the caller's to check (`to_bytes` of the
    /// result equals `bytes` with bit 255 cleared exactly when it was).
    pub(crate) const fn from_bytes(bytes: &[u8; 32]) -> Fe {
        Fe([
            load8(bytes, 0) & MASK,
            (load8(bytes, 6) >> 3) & MASK,
            (load8(bytes, 12) >> 6) & MASK,
            (load8(bytes, 19) >> 1) & MASK,
            (load8(bytes, 24) >> 12) & MASK,
        ])
    }

    /// The canonical encoding: the value reduced below p, 32 bytes little-endian, bit 255 clear.
    pub(crate) const fn to_bytes(&self) -> [u8; 32] {
        let mut l = self.carried().0;
        // l is below 2^255 + 2^218 < 2p: subtract p once if l >= p, which is when l + 19
        // carries out of bit 255.
        let mut q = (l[0] + 19) >> 51;
        q = (l[1] + q) >> 51;
        q = (l[2] + q) >> 51;
        q = (l[3] + q) >> 51;
        q = (l[4] + q) >> 51;
        l[0] += 19 * q;
        l[1] += l[0] >> 51;
        l[0] &= MASK;
        l[2] += l[1] >> 51;
        l[1] &= MASK;
        l[3] += l[2] >> 51;
        l[2] &= MASK;
        l[4] += l[3] >> 51;
        l[3] &= MASK;
        // Dropping the carry out of l4 subtracts the 2^255 that q * 19 added p to reach.
        l[4] &= MASK;
        let words = [l[0] | (l[1] << 51), (l[1] >> 13) | (l[2] << 38), (l[2] >> 26) | (l[3] << 25), (l[3] >> 39) | (l[4] << 12)];
        let mut out = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            out[i] = Cast::<u64, u8>::truncate(words[i / 8] >> (8 * (i % 8)));
            i += 1;
        }
        out
    }

    /// The same value with each limb below 2^51 plus the carry it takes from the limb below it
    /// (`l[i - 1] >> 51`; the first takes 19 times the top limb's): carries each limb into the
    /// next and the last, times 19, into the first, once, so limbs below 2^56 (`sub`'s sums
    /// are below 2^55.6) come out below 2^51 + 2^5 (the first below 2^51 + 19 * 2^5).
    const fn carried(&self) -> Fe {
        let l = self.0;
        let c0 = l[0] >> 51;
        let c1 = l[1] >> 51;
        let c2 = l[2] >> 51;
        let c3 = l[3] >> 51;
        let c4 = l[4] >> 51;
        Fe([(l[0] & MASK) + c4 * 19, (l[1] & MASK) + c0, (l[2] & MASK) + c1, (l[3] & MASK) + c2, (l[4] & MASK) + c3])
    }

    /// The sum, limb by limb with no carry (dropping the carry chain took a verification from
    /// 17.1 to 16.4 us in `examples/cost.rs`, one run each, 2026-11-16): two weakly reduced inputs give limbs below 2^53, a third added below
    /// 2^54, and `mul`, `square`, `sub`, `to_bytes` and `equals` all accept limbs below 2^54
    /// (their comments give the bounds). The formulas in `point.rs` add at most three.
    pub(crate) const fn add(&self, other: &Fe) -> Fe {
        let (a, b) = (self.0, other.0);
        Fe([a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3], a[4] + b[4]])
    }

    /// `self - other`, computed as `self + 16p - other` so no limb goes below zero (each limb of
    /// 16p is above 2^54.9, each of `other` below 2^54).
    pub(crate) const fn sub(&self, other: &Fe) -> Fe {
        const P16_0: u64 = 16 * ((1 << 51) - 19);
        const P16: u64 = 16 * ((1 << 51) - 1);
        let (a, b) = (self.0, other.0);
        Fe([a[0] + P16_0 - b[0], a[1] + P16 - b[1], a[2] + P16 - b[2], a[3] + P16 - b[3], a[4] + P16 - b[4]]).carried()
    }

    pub(crate) const fn neg(&self) -> Fe {
        Fe::ZERO.sub(self)
    }

    /// The product: 25 limb products, the ones past 2^255 folded back times 19. Limbs below
    /// 2^54 keep each column below 5 * 2^54 * 19 * 2^54 < 2^116, inside `carry_wide`'s 2^121.
    pub(crate) const fn mul(&self, other: &Fe) -> Fe {
        let (a, b) = (self.0, other.0);
        let (b1, b2, b3, b4) = (b[1] * 19, b[2] * 19, b[3] * 19, b[4] * 19);
        carry_wide([
            m(a[0], b[0]) + m(a[4], b1) + m(a[3], b2) + m(a[2], b3) + m(a[1], b4),
            m(a[1], b[0]) + m(a[0], b[1]) + m(a[4], b2) + m(a[3], b3) + m(a[2], b4),
            m(a[2], b[0]) + m(a[1], b[1]) + m(a[0], b[2]) + m(a[4], b3) + m(a[3], b4),
            m(a[3], b[0]) + m(a[2], b[1]) + m(a[1], b[2]) + m(a[0], b[3]) + m(a[4], b4),
            m(a[4], b[0]) + m(a[3], b[1]) + m(a[2], b[2]) + m(a[1], b[3]) + m(a[0], b[4]),
        ])
    }

    /// The square: 15 limb products, the symmetric ones doubled. Takes what `mul` takes, limbs
    /// below 2^54: each column is at most 2^108 + 2 * (2^55 * 19 * 2^54) < 2^114.3, inside
    /// `carry_wide`'s 2^121. On the chains `point.rs` makes, the review of 2026-11-16 bounded the
    /// worst seen: a limb times 19 at most 2^56.83, the largest column 2^111.44, and `sub`'s
    /// subtrahend at most 2^52 against its limit of 2^55.
    pub(crate) const fn square(&self) -> Fe {
        let a = self.0;
        let (a3_19, a4_19) = (a[3] * 19, a[4] * 19);
        let (d0, d1, d2) = (a[0] * 2, a[1] * 2, a[2] * 2);
        carry_wide([
            m(a[0], a[0]) + m(d1, a4_19) + m(d2, a3_19),
            m(a[3], a3_19) + m(d0, a[1]) + m(d2, a4_19),
            m(a[1], a[1]) + m(d0, a[2]) + m(a[4] * 2, a3_19),
            m(a[4], a4_19) + m(d0, a[3]) + m(d1, a[2]),
            m(a[2], a[2]) + m(d0, a[4]) + m(d1, a[3]),
        ])
    }

    /// `self` squared `k` times: `self^(2^k)`.
    pub(crate) const fn pow2k(&self, k: u32) -> Fe {
        let mut x = *self;
        let mut i = 0;
        while i < k {
            x = x.square();
            i += 1;
        }
        x
    }

    /// `(self^(2^250 - 1), self^11)`: the shared head of inversion and the square root, by the
    /// usual chain of 250 squarings and 11 multiplications.
    const fn pow22501(&self) -> (Fe, Fe) {
        let t0 = self.square(); // 2
        let t1 = t0.pow2k(2).mul(self); // 9
        let t0 = t0.mul(&t1); // 11
        let t2 = t0.square().mul(&t1); // 2^5 - 1
        let t3 = t2.pow2k(5).mul(&t2); // 2^10 - 1
        let t4 = t3.pow2k(10).mul(&t3); // 2^20 - 1
        let t5 = t4.pow2k(20).mul(&t4); // 2^40 - 1
        let t6 = t5.pow2k(10).mul(&t3); // 2^50 - 1
        let t7 = t6.pow2k(50).mul(&t6); // 2^100 - 1
        let t8 = t7.pow2k(100).mul(&t7); // 2^200 - 1
        let t9 = t8.pow2k(50).mul(&t6); // 2^250 - 1
        (t9, t0)
    }

    /// The inverse, `self^(p - 2)` (Fermat); zero for zero.
    #[cfg(test)]
    pub(crate) const fn invert(&self) -> Fe {
        let (t250, t11) = self.pow22501();
        // (2^250 - 1) 2^5 + 11 = 2^255 - 21 = p - 2
        t250.pow2k(5).mul(&t11)
    }

    /// `self^((p - 5) / 8)` = `self^(2^252 - 3)`, the power the square root of a ratio takes
    /// (RFC 8032, section 5.1.3, step 3).
    pub(crate) const fn pow_p58(&self) -> Fe {
        let (t250, _) = self.pow22501();
        t250.pow2k(2).mul(self)
    }

    /// Whether the canonical value is odd: RFC 8032's "negative" x.
    pub(crate) const fn is_negative(&self) -> bool {
        self.to_bytes()[0] & 1 == 1
    }

    pub(crate) const fn is_zero(&self) -> bool {
        bytes_eq(&self.to_bytes(), &[0; 32])
    }

    /// Equality of the values, by their canonical encodings.
    pub(crate) const fn equals(&self, other: &Fe) -> bool {
        bytes_eq(&self.to_bytes(), &other.to_bytes())
    }
}

/// d = -121665 / 121666, the curve's constant (RFC 8032, section 5.1), little-endian. Held to
/// its definition by a test (`tests/algebra.rs`).
pub(crate) const D: Fe = Fe::from_bytes(&[
    0xa3, 0x78, 0x59, 0x13, 0xca, 0x4d, 0xeb, 0x75, 0xab, 0xd8, 0x41, 0x41, 0x4d, 0x0a, 0x70, 0x00, 0x98, 0xe8, 0x79, 0x77,
    0x79, 0x40, 0xc7, 0x8c, 0x73, 0xfe, 0x6f, 0x2b, 0xee, 0x6c, 0x03, 0x52,
]);

/// 2d, which the addition formula takes.
pub(crate) const D2: Fe = D.add(&D);

/// A square root of -1, 2^((p - 1) / 4), little-endian. Held to its definition by a test.
pub(crate) const SQRT_M1: Fe = Fe::from_bytes(&[
    0xb0, 0xa0, 0x0e, 0x4a, 0x27, 0x1b, 0xee, 0xc4, 0x78, 0xe4, 0x2f, 0xad, 0x06, 0x18, 0x43, 0x2f, 0xa7, 0xd7, 0xfb, 0x3d,
    0x99, 0x00, 0x4d, 0x2b, 0x0b, 0xdf, 0xc1, 0x4f, 0x80, 0x24, 0x83, 0x2b,
]);

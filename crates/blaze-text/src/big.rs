//! [`Big`]: a fixed 1152-bit unsigned integer, `const` throughout. It computes the float
//! tables at compile time (powers of five and their inverses, the digits of `5^s`) and carries
//! the rare exact paths: fixed-place floats past `u128`'s reach.

use blaze_int::{Cast, idx};

pub(crate) const LIMBS: usize = 18;

#[derive(Clone, Copy)]
pub(crate) struct Big {
    /// Little-endian 64-bit limbs.
    limbs: [u64; LIMBS],
}

const fn wide(x: u64) -> u128 {
    Cast::<u64, u128>::widen(x)
}

const fn low(x: u128) -> u64 {
    Cast::<u128, u64>::truncate(x)
}

impl Big {
    pub(crate) const ZERO: Big = Big { limbs: [0; LIMBS] };

    pub(crate) const fn from_u64(v: u64) -> Big {
        let mut out = Big::ZERO;
        out.limbs[0] = v;
        out
    }

    /// `m << shift`.
    ///
    /// # Panics
    ///
    /// When the result has more than 1152 bits.
    pub(crate) const fn from_u64_shl(m: u64, shift: u32) -> Big {
        let mut out = Big::ZERO;
        let limb = idx(shift / 64);
        let s = shift % 64;
        assert!(limb < LIMBS, "blaze_text::big: a shift past 1152 bits");
        out.limbs[limb] = m << s;
        if s > 0 && limb + 1 < LIMBS {
            out.limbs[limb + 1] = m >> (64 - s);
        }
        out
    }

    /// `2^bit`.
    pub(crate) const fn pow2(bit: u32) -> Big {
        Big::from_u64_shl(1, bit)
    }

    /// The limbs below the highest nonzero one, plus it: the work bound of each operation.
    const fn used(&self) -> usize {
        let mut n = LIMBS;
        while n > 0 && self.limbs[n - 1] == 0 {
            n -= 1;
        }
        n
    }

    /// `self *= m`.
    ///
    /// # Panics
    ///
    /// When the product has more than 1152 bits: a table or a path sized wrong.
    pub(crate) const fn mul_small(&mut self, m: u64) {
        let used = self.used();
        let mut carry = 0u64;
        let mut i = 0;
        while i < used {
            let product = wide(self.limbs[i]) * wide(m) + wide(carry);
            self.limbs[i] = low(product);
            carry = low(product >> 64);
            i += 1;
        }
        if carry != 0 {
            assert!(used < LIMBS, "blaze_text::big: a product past 1152 bits");
            self.limbs[used] = carry;
        }
    }

    /// `self /= d`, returning the remainder.
    pub(crate) const fn div_small(&mut self, d: u64) -> u64 {
        let mut rem = 0u64;
        let mut i = self.used();
        while i > 0 {
            i -= 1;
            let cur = (wide(rem) << 64) | wide(self.limbs[i]);
            self.limbs[i] = low(cur / wide(d));
            rem = low(cur % wide(d));
        }
        rem
    }

    /// The number of bits up to the highest set one; 0 for zero.
    pub(crate) const fn bit_len(&self) -> u32 {
        let used = self.used();
        if used == 0 {
            return 0;
        }
        Cast::<usize, u32>::truncate(used) * 64 - self.limbs[used - 1].leading_zeros()
    }

    const fn limb(&self, i: usize) -> u64 {
        if i < LIMBS { self.limbs[i] } else { 0 }
    }

    /// The 128 bits from bit `start` up.
    pub(crate) const fn bits_at(&self, start: u32) -> u128 {
        let limb = idx(start / 64);
        let s = start % 64;
        let (l0, l1, l2) = (self.limb(limb), self.limb(limb + 1), self.limb(limb + 2));
        if s == 0 {
            (wide(l1) << 64) | wide(l0)
        } else {
            let lo = (l0 >> s) | (l1 << (64 - s));
            let hi = (l1 >> s) | (l2 << (64 - s));
            (wide(hi) << 64) | wide(lo)
        }
    }

    /// Whether bit `bit` is set.
    pub(crate) const fn bit(&self, bit: u32) -> bool {
        (self.limb(idx(bit / 64)) >> (bit % 64)) & 1 == 1
    }

    /// Clears every bit at `bit` and above.
    pub(crate) const fn truncate_bits(&mut self, bit: u32) {
        let limb = idx(bit / 64);
        let s = bit % 64;
        let mut i = limb;
        while i < LIMBS {
            if i == limb && s > 0 {
                self.limbs[i] &= (1u64 << s) - 1;
            } else {
                self.limbs[i] = 0;
            }
            i += 1;
        }
    }

    /// Whether every bit below `bit` is clear.
    pub(crate) const fn zero_below(&self, bit: u32) -> bool {
        let limb = idx(bit / 64);
        let s = bit % 64;
        let mut i = 0;
        while i < limb && i < LIMBS {
            if self.limbs[i] != 0 {
                return false;
            }
            i += 1;
        }
        s == 0 || limb >= LIMBS || self.limbs[limb] & ((1u64 << s) - 1) == 0
    }

    pub(crate) const fn is_zero(&self) -> bool {
        self.used() == 0
    }
}

/// `5^n` for `n <= 27`, which fits a `u64`.
pub(crate) const fn pow5_u64(n: u32) -> u64 {
    let mut out = 1u64;
    let mut i = 0;
    while i < n {
        out *= 5;
        i += 1;
    }
    out
}

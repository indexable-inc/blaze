//! foldhash's fast hasher under its fixed seed (foldhash 0.2.0, `foldhash::fast::FixedState`,
//! by Orson Peters, Zlib licensed), written here so this crate depends on no crate from outside
//! the workspace (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15"). Bit for bit
//! foldhash's on every 64-bit target with a widening multiply (all of blaze's hosts): the same
//! constants, the same sponge for integers, the same short and long byte paths, so a table's
//! layout and every hash a test pinned is unchanged. `tests::fold_is_foldhash` holds it to the
//! crate it replaces.
//!
//! The one change is the loads: eight bytes are read with `from_ne_bytes` on a checked slice,
//! not an unchecked unaligned read, so the file has no unsafe code. The bytes and their order
//! are the same.

/// Constants of high entropy: hexadecimal digits of pi, as foldhash takes them.
const ARBITRARY3: u64 = 0x082e_fa98_ec4e_6c89;
const ARBITRARY6: u64 = 0xc0ac_29b7_c97c_50dd;
const ARBITRARY7: u64 = 0x3f84_d5b5_b547_0917;
const ARBITRARY8: u64 = 0x9216_d5d9_8979_fb1b;
const ARBITRARY9: u64 = 0xd131_0ba6_98df_b5ac;
const ARBITRARY10: u64 = 0x2ffd_72db_d01a_dfb7;
const ARBITRARY11: u64 = 0xb8e1_afed_6a26_7e96;

/// foldhash's fixed shared seed (`SharedSeed::global_fixed`).
const SEEDS: [u64; 6] = [ARBITRARY6, ARBITRARY7, ARBITRARY8, ARBITRARY9, ARBITRARY10, ARBITRARY11];

/// foldhash's `fast::FoldHasher` on [`SEEDS`].
#[derive(Clone)]
pub(crate) struct Fold {
    accumulator: u64,
    sponge: u128,
    sponge_len: u8,
}

impl Fold {
    /// `foldhash::fast::FixedState::with_seed(seed).build_hasher()`: `seed` 0 is the default.
    #[inline(always)]
    pub(crate) const fn with_seed(seed: u64) -> Fold {
        Fold { accumulator: seed ^ ARBITRARY3, sponge: 0, sponge_len: 0 }
    }

    /// An integer of `bits` bits into the 128-bit sponge, folded into the accumulator first
    /// when it would overflow.
    #[inline(always)]
    fn write_num(&mut self, x: u128, bits: u8) {
        if u32::from(self.sponge_len) + u32::from(bits) > 128 {
            let (lo, hi) = (self.sponge as u64, (self.sponge >> 64) as u64);
            self.accumulator = folded_multiply(lo ^ self.accumulator, hi ^ SEEDS[0]);
            self.sponge = x;
            self.sponge_len = bits;
        } else {
            self.sponge |= x << self.sponge_len;
            self.sponge_len += bits;
        }
    }

    #[inline(always)]
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        // A length-dependent rotation of the accumulator, which defeats length extension
        // through the overlapping reads below.
        let len = bytes.len();
        self.accumulator = self.accumulator.rotate_right(len as u32);
        self.accumulator = if len <= 16 {
            hash_bytes_short(bytes, self.accumulator)
        } else {
            hash_bytes_long(bytes, self.accumulator)
        };
    }

    #[inline(always)]
    pub(crate) fn write_u8(&mut self, it: u8) {
        self.write_num(u128::from(it), 8);
    }

    #[inline(always)]
    pub(crate) fn write_u16(&mut self, it: u16) {
        self.write_num(u128::from(it), 16);
    }

    #[inline(always)]
    pub(crate) fn write_u32(&mut self, it: u32) {
        self.write_num(u128::from(it), 32);
    }

    #[inline(always)]
    pub(crate) fn write_u64(&mut self, it: u64) {
        self.write_num(u128::from(it), 64);
    }

    #[inline(always)]
    pub(crate) fn write_u128(&mut self, it: u128) {
        let (lo, hi) = (it as u64, (it >> 64) as u64);
        self.accumulator = folded_multiply(lo ^ self.accumulator, hi ^ SEEDS[0]);
    }

    #[inline(always)]
    pub(crate) fn write_usize(&mut self, it: usize) {
        #[cfg(target_pointer_width = "32")]
        self.write_num(u128::from(it as u32), 32);
        #[cfg(target_pointer_width = "64")]
        self.write_num(u128::from(it as u64), 64);
    }

    #[inline(always)]
    pub(crate) fn finish(&self) -> u64 {
        if self.sponge_len > 0 {
            let (lo, hi) = (self.sponge as u64, (self.sponge >> 64) as u64);
            folded_multiply(lo ^ self.accumulator, hi ^ SEEDS[0])
        } else {
            self.accumulator
        }
    }
}

/// The full 64 by 64 to 128-bit product, its halves XORed: the middle bits of a product move
/// most with its inputs, and they are the top of the low half and the bottom of the high.
#[inline(always)]
const fn folded_multiply(x: u64, y: u64) -> u64 {
    let full = (x as u128).wrapping_mul(y as u128);
    (full as u64) ^ ((full >> 64) as u64)
}

/// Eight bytes of `bytes` from `at`, in the machine's order (foldhash's unaligned read).
#[inline(always)]
fn load(bytes: &[u8], at: usize) -> u64 {
    let (word, _) = bytes[at..].split_first_chunk::<8>().expect("eight bytes at a checked offset");
    u64::from_ne_bytes(*word)
}

/// At most 16 bytes.
#[inline(always)]
fn hash_bytes_short(bytes: &[u8], accumulator: u64) -> u64 {
    let len = bytes.len();
    let mut s0 = accumulator;
    let mut s1 = SEEDS[1];
    if len >= 8 {
        s0 ^= load(bytes, 0);
        s1 ^= load(bytes, len - 8);
    } else if len >= 4 {
        let word = |at: usize| {
            let (word, _) = bytes[at..].split_first_chunk::<4>().expect("four bytes");
            u64::from(u32::from_ne_bytes(*word))
        };
        s0 ^= word(0);
        s1 ^= word(len - 4);
    } else if len > 0 {
        let (lo, mid, hi) = (bytes[0], bytes[len / 2], bytes[len - 1]);
        s0 ^= u64::from(lo);
        s1 ^= (u64::from(hi) << 8) | u64::from(mid);
    }
    folded_multiply(s0, s1)
}

/// More than 16 bytes: six lanes past 256, four past 128, then up to eight words from each end.
#[cold]
#[inline(never)]
fn hash_bytes_long(mut v: &[u8], accumulator: u64) -> u64 {
    let seeds = &SEEDS;
    let mut s0 = accumulator;
    let mut s1 = s0.wrapping_add(seeds[1]);

    if v.len() > 128 {
        let mut s2 = s0.wrapping_add(seeds[2]);
        let mut s3 = s0.wrapping_add(seeds[3]);

        if v.len() > 256 {
            let mut s4 = s0.wrapping_add(seeds[4]);
            let mut s5 = s0.wrapping_add(seeds[5]);
            loop {
                s0 = folded_multiply(load(v, 0) ^ s0, load(v, 48) ^ seeds[0]);
                s1 = folded_multiply(load(v, 8) ^ s1, load(v, 56) ^ seeds[0]);
                s2 = folded_multiply(load(v, 16) ^ s2, load(v, 64) ^ seeds[0]);
                s3 = folded_multiply(load(v, 24) ^ s3, load(v, 72) ^ seeds[0]);
                s4 = folded_multiply(load(v, 32) ^ s4, load(v, 80) ^ seeds[0]);
                s5 = folded_multiply(load(v, 40) ^ s5, load(v, 88) ^ seeds[0]);
                v = &v[96..];
                if v.len() <= 256 {
                    break;
                }
            }
            s0 ^= s4;
            s1 ^= s5;
        }

        loop {
            s0 = folded_multiply(load(v, 0) ^ s0, load(v, 32) ^ seeds[0]);
            s1 = folded_multiply(load(v, 8) ^ s1, load(v, 40) ^ seeds[0]);
            s2 = folded_multiply(load(v, 16) ^ s2, load(v, 48) ^ seeds[0]);
            s3 = folded_multiply(load(v, 24) ^ s3, load(v, 56) ^ seeds[0]);
            v = &v[64..];
            if v.len() <= 128 {
                break;
            }
        }
        s0 ^= s2;
        s1 ^= s3;
    }

    // At least 16 bytes are left: the loops above stop past 128.
    let len = v.len();
    s0 = folded_multiply(load(v, 0) ^ s0, load(v, len - 16) ^ seeds[0]);
    s1 = folded_multiply(load(v, 8) ^ s1, load(v, len - 8) ^ seeds[0]);
    if len >= 32 {
        s0 = folded_multiply(load(v, 16) ^ s0, load(v, len - 32) ^ seeds[0]);
        s1 = folded_multiply(load(v, 24) ^ s1, load(v, len - 24) ^ seeds[0]);
        if len >= 64 {
            s0 = folded_multiply(load(v, 32) ^ s0, load(v, len - 48) ^ seeds[0]);
            s1 = folded_multiply(load(v, 40) ^ s1, load(v, len - 40) ^ seeds[0]);
            if len >= 96 {
                s0 = folded_multiply(load(v, 48) ^ s0, load(v, len - 64) ^ seeds[0]);
                s1 = folded_multiply(load(v, 56) ^ s1, load(v, len - 56) ^ seeds[0]);
            }
        }
    }
    s0 ^ s1
}

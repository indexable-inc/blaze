//! The arena's hashes: one widening multiply per row, folded (foldhash's `folded_multiply`,
//! which blaze-hash's `fold` ports). Written here, not taken from blaze-hash, because a row is
//! three words and blaze-hash's plain-key path hashes 12 bytes with its full sponge; the floor
//! is one multiply.

use blaze_int::Int;

const K0: u64 = 0x243f_6a88_85a3_08d3;
const K1: u64 = 0x1319_8a2e_0370_7344;
const K2: u64 = 0xa409_3822_299f_31d0;
const K3: u64 = 0x082e_fa98_ec4e_6c89;

#[inline(always)]
pub(crate) fn fold_mul(x: u64, y: u64) -> u64 {
    let p = u128::from(x) * u128::from(y);
    p.truncate_to::<u64>() ^ (p >> 64).truncate_to::<u64>()
}

/// The hash of a row `(kind, a, b)`.
#[inline(always)]
pub(crate) fn row(kind: u8, a: u32, b: u32) -> u64 {
    let x = u64::from(a) | (u64::from(b) << 32);
    fold_mul(x ^ K0, (u64::from(kind) << 1 | 1) ^ K1)
}

/// The hash of a sequence of words.
#[inline]
pub(crate) fn words(ws: impl ExactSizeIterator<Item = u32>) -> u64 {
    let mut h = fold_mul(ws.len().widen_to::<u64>() ^ K2, K3);
    let mut pair: u64 = 0;
    let mut odd = false;
    for w in ws {
        if odd {
            h = fold_mul(h ^ pair ^ (u64::from(w) << 32), K1);
        } else {
            pair = u64::from(w);
        }
        odd = !odd;
    }
    if odd {
        h = fold_mul(h ^ pair, K1);
    }
    h
}

/// The hash of a 64-bit key (memo tables).
#[inline(always)]
pub(crate) fn key(k: u64) -> u64 {
    fold_mul(k ^ K2, K3)
}

/// The hash of a 128-bit value.
#[inline(always)]
pub(crate) fn wide(v: u128) -> u64 {
    fold_mul(v.truncate_to::<u64>() ^ K0, (v >> 64).truncate_to::<u64>() ^ K3)
}

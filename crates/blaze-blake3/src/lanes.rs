//! BLAKE3's compression over `N` independent lanes at once, transposed: `cv[w]` and
//! `message[w]` are word `w` of every lane, so each step of a round is one operation over the
//! lanes once LLVM vectorizes the loops (one AVX-512 register a word at 16 lanes, one AVX2
//! register at 8, two NEON registers at 8). Safe code, so there is no SIMD of our own to prove:
//! a caller compiled with more vector features (a `#[target_feature]` function it is inlined
//! into) vectorizes it with them.
//!
//! What uses it: [`crate::Hasher`]'s whole chunks side by side, and `blaze_ids`' many short
//! inputs (`ContentId::of_many`), whose last blocks have a length each.

use std::simd::Simd;

use crate::{BLOCK_LEN, IV, SCHEDULE};

#[inline(always)]
fn add<const N: usize>(a: [u32; N], b: [u32; N]) -> [u32; N] {
    let mut out = a;
    for lane in 0..N {
        out[lane] = out[lane].wrapping_add(b[lane]);
    }
    out
}

#[inline(always)]
fn xor_rotate<const N: usize>(a: [u32; N], b: [u32; N], by: u32) -> [u32; N] {
    let mut out = a;
    for lane in 0..N {
        out[lane] = (out[lane] ^ b[lane]).rotate_right(by);
    }
    out
}

#[inline(always)]
fn g<const N: usize>(v: &mut [[u32; N]; 16], [a, b, c, d]: [usize; 4], x: [u32; N], y: [u32; N]) {
    v[a] = add(add(v[a], v[b]), x);
    v[d] = xor_rotate(v[d], v[a], 16);
    v[c] = add(v[c], v[d]);
    v[b] = xor_rotate(v[b], v[c], 12);
    v[a] = add(add(v[a], v[b]), y);
    v[d] = xor_rotate(v[d], v[a], 8);
    v[c] = add(v[c], v[d]);
    v[b] = xor_rotate(v[b], v[c], 7);
}

/// BLAKE3's compression of one block in each of `N` lanes: each lane its own chaining value,
/// message, counter (`counter_low`, `counter_high`: the low and high words of the 64-bit
/// counter) and block length, all under one `flags`. Gives each lane's next chaining value, the
/// first 32 bytes of the output when `flags` has [`ROOT`](crate::ROOT). Always inlined, so it
/// is vectorized for the features of the function it is inlined into.
#[inline(always)]
pub fn compress<const N: usize>(
    cv: &[[u32; N]; 8],
    message: &[[u32; N]; 16],
    counter_low: [u32; N],
    counter_high: [u32; N],
    block_len: [u32; N],
    flags: u32,
) -> [[u32; N]; 8] {
    let mut v = [[0; N]; 16];
    v[..8].copy_from_slice(cv);
    for word in 0..4 {
        v[8 + word] = [IV[word]; N];
    }
    v[12] = counter_low;
    v[13] = counter_high;
    v[14] = block_len;
    v[15] = [flags; N];
    let mut m = *message;
    for round in 0..7 {
        g(&mut v, [0, 4, 8, 12], m[0], m[1]);
        g(&mut v, [1, 5, 9, 13], m[2], m[3]);
        g(&mut v, [2, 6, 10, 14], m[4], m[5]);
        g(&mut v, [3, 7, 11, 15], m[6], m[7]);
        g(&mut v, [0, 5, 10, 15], m[8], m[9]);
        g(&mut v, [1, 6, 11, 12], m[10], m[11]);
        g(&mut v, [2, 7, 8, 13], m[12], m[13]);
        g(&mut v, [3, 4, 9, 14], m[14], m[15]);
        if round < 6 {
            m = SCHEDULE.map(|word| m[word]);
        }
    }
    let mut out = [[0; N]; 8];
    for (word, lanes) in out.iter_mut().enumerate() {
        for lane in 0..N {
            lanes[lane] = v[word][lane] ^ v[word + 8][lane];
        }
    }
    out
}

/// A block's sixteen little-endian words.
#[inline(always)]
pub fn block_words(block: &[u8; BLOCK_LEN]) -> [u32; 16] {
    std::array::from_fn(|word| {
        u32::from_le_bytes([
            block[4 * word],
            block[4 * word + 1],
            block[4 * word + 2],
            block[4 * word + 3],
        ])
    })
}

/// Each lane's sixteen message words (`rows[lane]`) as sixteen columns of `S` lanes, the form
/// [`compress`] takes: `16 / S` square transposes in registers, so a block goes from its bytes
/// to the compression with no scattered store and reload.
#[inline(always)]
pub fn columns<const S: usize>(rows: &[[u32; 16]; S]) -> [[u32; S]; 16] {
    const { assert!(S.is_power_of_two() && S <= 16) };
    let mut out = [[0; S]; 16];
    for from in (0..16).step_by(S) {
        let square = transpose(std::array::from_fn(|lane| {
            Simd::from_array(std::array::from_fn(|word| rows[lane][from + word]))
        }));
        for (word, column) in square.iter().enumerate() {
            out[from + word] = column.to_array();
        }
    }
    out
}

/// Eight words of `S` lanes (a compression's output) as each lane's eight words: [`columns`]'
/// transpose the other way, in `S`-square registers (zero rows filling a square past the eight).
#[inline(always)]
pub fn rows_of<const S: usize>(columns: &[[u32; S]; 8]) -> [[u32; 8]; S] {
    const { assert!(S.is_power_of_two() && S <= 16) };
    let mut out = [[0; 8]; S];
    for from in (0..8).step_by(S) {
        let square = transpose(std::array::from_fn(|row| {
            Simd::from_array(columns.get(from + row).copied().unwrap_or([0; S]))
        }));
        for (lane, row) in square.iter().enumerate() {
            let row = row.to_array();
            for word in 0..S.min(8) {
                out[lane][from + word] = row[word];
            }
        }
    }
    out
}

/// An `S` by `S` transpose: interleaving row `i` with row `i + S / 2` into rows `2i` and `2i + 1`
/// rotates each element's address (row bits, then column bits) left by one bit, so
/// `log2(S)` rounds swap rows and columns. Each interleave is two shuffles (`vpermt2d` on
/// AVX-512, `zip1`/`zip2` on NEON).
#[inline(always)]
fn transpose<const S: usize>(mut v: [Simd<u32, S>; S]) -> [Simd<u32, S>; S] {
    for _ in 0..S.trailing_zeros() {
        let mut next = v;
        for i in 0..S / 2 {
            (next[2 * i], next[2 * i + 1]) = v[i].interleave(v[i + S / 2]);
        }
        v = next;
    }
    v
}

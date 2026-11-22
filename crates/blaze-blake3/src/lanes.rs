//! BLAKE3's compression over `N` independent lanes at once, transposed: `cv[w]` and
//! `message[w]` are word `w` of every lane, each a `core::simd::Simd<u32, N>`, so each step of a
//! round is one vector operation over the lanes (one NEON register a word at 4 lanes, one AVX2
//! register at 8, one AVX-512 register at 16), from one body. Safe code, so there is no SIMD of
//! our own to prove: a caller compiled with more vector features (a `#[target_feature]` function
//! it is inlined into) gets them.
//!
//! What uses it: [`crate::Hasher`]'s whole chunks side by side, [`crate::hash_many`]'s
//! independent short inputs, and `blaze_ids`' (`ContentId::of_many`).

use core::simd::{Simd, Swizzle, ToBytes};

use blaze_int::{Cast, Int};

use crate::{BLOCK_LEN, IV, MESSAGE};

/// `x` rotated right by `by` in each lane: a shift and a shift-insert on NEON (`ushr`, `sli`),
/// `vprord` on AVX-512.
#[inline(always)]
pub(crate) fn rotate<const N: usize>(x: Simd<u32, N>, by: u32) -> Simd<u32, N> {
    (x >> Simd::splat(by)) | (x << Simd::splat(32 - by))
}

/// The byte shuffle that rotates each 32-bit word right by `BY` bytes: byte `i` of a word takes
/// byte `(i + BY) % 4`.
struct ByBytes<const BY: usize>;

impl<const BY: usize, const M: usize> Swizzle<M> for ByBytes<BY> {
    const INDEX: [usize; M] = {
        let mut index = [0; M];
        let mut at = 0;
        while at < M {
            index[at] = at / 4 * 4 + (at % 4 + BY) % 4;
            at += 1;
        }
        index
    };
}

/// `x` rotated right by `BY` whole bytes as one byte shuffle (`tbl` on NEON, `vpshufb` on
/// x86), where shifts take two instructions: at 8 lanes on the owner's M5 Max, 16.0 ns a block
/// a lane against 17.1 with shifts (blaze-int-1112-notes/nightly-probes/blake3_lanes.rs). The
/// lane counts the crate runs (2, 4, 8, 16, and 4 for the rows of `crate::rows`) go through
/// their bytes; any other by shifts. The `from_slice` round trips are of `N` words into `N`
/// words, so LLVM keeps them in registers.
#[inline(always)]
pub(crate) fn rotate_bytes<const N: usize, const BY: usize>(x: Simd<u32, N>) -> Simd<u32, N> {
    let words = x.as_array().as_slice();
    match N {
        2 => {
            let bytes = Simd::<u32, 2>::from_slice(words).to_ne_bytes();
            Simd::from_slice(Simd::<u32, 2>::from_ne_bytes(ByBytes::<BY>::swizzle(bytes)).as_array())
        }
        4 => {
            let bytes = Simd::<u32, 4>::from_slice(words).to_ne_bytes();
            Simd::from_slice(Simd::<u32, 4>::from_ne_bytes(ByBytes::<BY>::swizzle(bytes)).as_array())
        }
        8 => {
            let bytes = Simd::<u32, 8>::from_slice(words).to_ne_bytes();
            Simd::from_slice(Simd::<u32, 8>::from_ne_bytes(ByBytes::<BY>::swizzle(bytes)).as_array())
        }
        16 => {
            let bytes = Simd::<u32, 16>::from_slice(words).to_ne_bytes();
            Simd::from_slice(
                Simd::<u32, 16>::from_ne_bytes(ByBytes::<BY>::swizzle(bytes)).as_array(),
            )
        }
        _ => rotate(x, const { Cast::<usize, u32>::narrow_or_stop(8 * BY) }),
    }
}

#[inline(always)]
fn g<const N: usize>(
    v: &mut [Simd<u32, N>; 16],
    [a, b, c, d]: [usize; 4],
    x: Simd<u32, N>,
    y: Simd<u32, N>,
) {
    v[a] = v[a] + v[b] + x;
    v[d] = rotate_bytes::<N, 2>(v[d] ^ v[a]);
    v[c] = v[c] + v[d];
    v[b] = rotate(v[b] ^ v[c], 12);
    v[a] = v[a] + v[b] + y;
    v[d] = rotate_bytes::<N, 1>(v[d] ^ v[a]);
    v[c] = v[c] + v[d];
    v[b] = rotate(v[b] ^ v[c], 7);
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
    compress_each(cv, message, counter_low, counter_high, block_len, [flags; N])
}

/// [`compress`] with flags of each lane's own: a lane starting its chunk beside one ending its
/// own, as [`crate::hash_many`] runs them. Each word of the state is one `Simd<u32, N>`, so 4
/// lanes are one NEON register a word, 8 one AVX2 register, 16 one AVX-512 register, from this
/// one body.
#[inline(always)]
pub fn compress_each<const N: usize>(
    cv: &[[u32; N]; 8],
    message: &[[u32; N]; 16],
    counter_low: [u32; N],
    counter_high: [u32; N],
    block_len: [u32; N],
    flags: [u32; N],
) -> [[u32; N]; 8] {
    let next = compress_in_registers(
        (*cv).map(Simd::from_array),
        &(*message).map(Simd::from_array),
        Simd::from_array(counter_low),
        Simd::from_array(counter_high),
        Simd::from_array(block_len),
        Simd::from_array(flags),
    );
    next.map(Simd::to_array)
}

/// [`compress_each`] on vectors, the form a loop over a chunk's blocks keeps its chaining value
/// in: taken and given by value, so nothing goes through memory between blocks. The array forms
/// put each block's chaining value on the stack and back (eight `stp q` and eight `ldp q` a
/// block at four lanes), and a `[64; L]` block length was built by a `memset_pattern16` call
/// each block (the coordinator's 2026-11-20 build of `examples/cost`, `chunks_lanes::<4>`).
#[inline(always)]
pub(crate) fn compress_in_registers<const N: usize>(
    cv: [Simd<u32, N>; 8],
    m: &[Simd<u32, N>; 16],
    counter_low: Simd<u32, N>,
    counter_high: Simd<u32, N>,
    block_len: Simd<u32, N>,
    flags: Simd<u32, N>,
) -> [Simd<u32, N>; 8] {
    let mut v: [Simd<u32, N>; 16] = [
        cv[0],
        cv[1],
        cv[2],
        cv[3],
        cv[4],
        cv[5],
        cv[6],
        cv[7],
        Simd::splat(IV[0]),
        Simd::splat(IV[1]),
        Simd::splat(IV[2]),
        Simd::splat(IV[3]),
        counter_low,
        counter_high,
        block_len,
        flags,
    ];
    round(&mut v, m, &MESSAGE[0]);
    round(&mut v, m, &MESSAGE[1]);
    round(&mut v, m, &MESSAGE[2]);
    round(&mut v, m, &MESSAGE[3]);
    round(&mut v, m, &MESSAGE[4]);
    round(&mut v, m, &MESSAGE[5]);
    round(&mut v, m, &MESSAGE[6]);
    core::array::from_fn(|word| v[word] ^ v[word + 8])
}

/// One round, its message words read where `t` says: written out seven times by the caller,
/// so every index is a constant and no round moves the message.
#[inline(always)]
fn round<const N: usize>(v: &mut [Simd<u32, N>; 16], m: &[Simd<u32, N>; 16], t: &[usize; 16]) {
    g(v, [0, 4, 8, 12], m[t[0]], m[t[1]]);
    g(v, [1, 5, 9, 13], m[t[2]], m[t[3]]);
    g(v, [2, 6, 10, 14], m[t[4]], m[t[5]]);
    g(v, [3, 7, 11, 15], m[t[6]], m[t[7]]);
    g(v, [0, 5, 10, 15], m[t[8]], m[t[9]]);
    g(v, [1, 6, 11, 12], m[t[10]], m[t[11]]);
    g(v, [2, 7, 8, 13], m[t[12]], m[t[13]]);
    g(v, [3, 4, 9, 14], m[t[14]], m[t[15]]);
}

/// The sixteen words of a block's last bytes (at most [`BLOCK_LEN`]), zeros after them: read
/// eight bytes at a time, a short tail by overlapping loads, so a last block costs a load a
/// word with no byte loop and no copy into a zeroed buffer.
#[inline(always)]
pub fn padded_words(bytes: &[u8]) -> [u32; 16] {
    assert!(bytes.len() <= BLOCK_LEN, "a last block of {} bytes", bytes.len());
    let mut words = [0u64; BLOCK_LEN / 8];
    let (whole, rest) = bytes.as_chunks::<8>();
    for (word, chunk) in words.iter_mut().zip(whole) {
        *word = u64::from_le_bytes(*chunk);
    }
    if !rest.is_empty() {
        words[whole.len()] = tail_word(bytes, rest.len());
    }
    core::array::from_fn(|word| {
        // The low half, then the high half, of each 64-bit word: the truncation is the point.
        let pair = words[word / 2] >> (32 * (word % 2));
        pair.truncate_to::<u32>()
    })
}

/// The last `rest` bytes of `bytes` (1 to 7), little-endian in the low bytes of a word: one
/// load where eight bytes are there to read, else two four-byte loads that overlap, else three
/// single bytes that cover one to three.
#[inline(always)]
fn tail_word(bytes: &[u8], rest: usize) -> u64 {
    let len = bytes.len();
    if let Some(eight) = bytes.last_chunk::<8>() {
        return u64::from_le_bytes(*eight) >> (8 * (8 - rest));
    }
    // `bytes` is `rest` long here: fewer than eight bytes in all.
    if rest >= 4 {
        let low = u32::from_le_bytes(bytes[..4].try_into().expect("four bytes"));
        let high = u32::from_le_bytes(bytes[len - 4..].try_into().expect("four bytes"));
        return u64::from(low) | u64::from(high) << (8 * (rest - 4));
    }
    let (first, middle, last) = (bytes[0], bytes[rest / 2], bytes[rest - 1]);
    u64::from(first) | u64::from(middle) << (8 * (rest / 2)) | u64::from(last) << (8 * (rest - 1))
}

/// A block's sixteen little-endian words.
#[inline(always)]
pub fn block_words(block: &[u8; BLOCK_LEN]) -> [u32; 16] {
    core::array::from_fn(|word| {
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
        // Each lane's `S` words in one vector load from where they lie.
        let square = transpose(core::array::from_fn(|lane| Simd::from_slice(&rows[lane][from..])));
        for (word, column) in square.iter().enumerate() {
            out[from + word] = column.to_array();
        }
    }
    out
}

/// [`columns`] of each lane's [`block_words`], read from the blocks' bytes: a lane's `S` words
/// in one vector load (`ldr q` at four lanes on NEON), then the same square transposes. What a
/// loop over whole blocks takes; through `block_words` and `columns`, LLVM built each lane's
/// words from 64-bit loads and lane inserts (`ldr d`, `mov.d`) before the transposes, twice
/// the instructions of the official crate's `transpose_msg_vecs4` (sixteen `vld1q_u8` and four
/// `vtrnq` and `vcombine` transposes).
#[inline(always)]
pub(crate) fn message_columns<const S: usize>(blocks: &[&[u8; BLOCK_LEN]; S]) -> [Simd<u32, S>; 16] {
    const { assert!(S.is_power_of_two() && S <= 16) };
    let mut out = [Simd::splat(0); 16];
    for from in (0..16).step_by(S) {
        let square = transpose(core::array::from_fn(|lane| le_words::<S>(&blocks[lane][4 * from..])));
        out[from..from + S].copy_from_slice(&square);
    }
    out
}

/// The first `S` little-endian words of `bytes` (at least `4 * S` long) as one vector: one
/// load of `4 * S` bytes at the widths the crate runs, the byte order fixed by `from_le_bytes`
/// (nothing on a little-endian target). The `from_slice` round trip is of `S` words into `S`
/// words, as in [`rotate_bytes`].
#[inline(always)]
fn le_words<const S: usize>(bytes: &[u8]) -> Simd<u32, S> {
    match S {
        2 => Simd::from_slice(Simd::<u32, 2>::from_le_bytes(Simd::<u8, 8>::from_slice(bytes)).as_array()),
        4 => Simd::from_slice(Simd::<u32, 4>::from_le_bytes(Simd::<u8, 16>::from_slice(bytes)).as_array()),
        8 => Simd::from_slice(Simd::<u32, 8>::from_le_bytes(Simd::<u8, 32>::from_slice(bytes)).as_array()),
        16 => Simd::from_slice(Simd::<u32, 16>::from_le_bytes(Simd::<u8, 64>::from_slice(bytes)).as_array()),
        _ => Simd::from_array(core::array::from_fn(|word| {
            u32::from_le_bytes(*bytes[4 * word..].first_chunk::<4>().expect("four bytes a word"))
        })),
    }
}

/// Eight words of `S` lanes (a compression's output) as each lane's eight words: [`columns`]'
/// transpose the other way, in `S`-square registers (zero rows filling a square past the eight).
#[inline(always)]
pub fn rows_of<const S: usize>(columns: &[[u32; S]; 8]) -> [[u32; 8]; S] {
    const { assert!(S.is_power_of_two() && S <= 16) };
    let mut out = [[0; 8]; S];
    for from in (0..8).step_by(S) {
        let square = transpose(core::array::from_fn(|row| {
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

#[cfg(test)]
mod tests {
    extern crate std;

    use core::simd::Simd;

    use blaze_sim::{Config, Tape, check};

    use super::{block_words, columns, message_columns};
    use crate::BLOCK_LEN;

    /// `S` drawn blocks read by [`message_columns`] are the columns of their scalar words
    /// ([`block_words`], then [`columns`]): the twin, at every width the crate runs.
    fn at<const S: usize>(tape: &Tape) {
        let blocks: [[u8; BLOCK_LEN]; S] = core::array::from_fn(|_| {
            let mut block = [0; BLOCK_LEN];
            tape.fill(&mut block, "block");
            block
        });
        let got = message_columns::<S>(&core::array::from_fn(|lane| &blocks[lane]));
        let want = columns::<S>(&core::array::from_fn(|lane| block_words(&blocks[lane]))).map(Simd::from_array);
        assert_eq!(got, want, "{S} lanes");
    }

    #[test]
    fn message_columns_are_the_columns_of_block_words() {
        check(Config::cases(500), |tape: &Tape| {
            at::<2>(tape);
            at::<4>(tape);
            at::<8>(tape);
            at::<16>(tape);
        });
    }
}

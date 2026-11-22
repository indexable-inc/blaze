//! Two compressions side by side in general registers, step for step: each line of G done for
//! the first then the second, so the core has eight independent chains a half-round where one
//! compression has four. [`crate::Pair::Interleaved`]'s form of a batch two wide (two chunks,
//! two parents), against two lanes of `core::simd` ([`crate::Pair::Lanes`]): two lanes fill half
//! a NEON register and pay two instructions a rotation by 12 or 7 (`shl`, `usra`), where a
//! general register rotates in one (`ror`) on more units (on the owner's M5 Max a block a lane
//! took 58.9 ns at two lanes, 56.2 ns alone in general registers: coordinator, 2026-11-20).
//!
//! The scalar twin is [`crate::compress`] run twice; `tests` holds each lane to it.

use blaze_int::Int;

use crate::{IV, MESSAGE};

/// Two independent compressions' state, `[pair][word]`.
type States = [[u32; 16]; 2];

#[inline(always)]
fn g(s: &mut States, [a, b, c, d]: [usize; 4], x: [u32; 2], y: [u32; 2]) {
    for p in 0..2 {
        s[p][a] = s[p][a].wrapping_add(s[p][b]).wrapping_add(x[p]);
    }
    for p in 0..2 {
        s[p][d] = (s[p][d] ^ s[p][a]).rotate_right(16);
    }
    for p in 0..2 {
        s[p][c] = s[p][c].wrapping_add(s[p][d]);
    }
    for p in 0..2 {
        s[p][b] = (s[p][b] ^ s[p][c]).rotate_right(12);
    }
    for p in 0..2 {
        s[p][a] = s[p][a].wrapping_add(s[p][b]).wrapping_add(y[p]);
    }
    for p in 0..2 {
        s[p][d] = (s[p][d] ^ s[p][a]).rotate_right(8);
    }
    for p in 0..2 {
        s[p][c] = s[p][c].wrapping_add(s[p][d]);
    }
    for p in 0..2 {
        s[p][b] = (s[p][b] ^ s[p][c]).rotate_right(7);
    }
}

/// One round of both, the message words read where `t` says (written out seven times by the
/// caller, every index a constant, as [`crate::compress`]).
#[inline(always)]
fn round(s: &mut States, m: [&[u32; 16]; 2], t: &[usize; 16]) {
    let w = |at: usize| [m[0][t[at]], m[1][t[at]]];
    g(s, [0, 4, 8, 12], w(0), w(1));
    g(s, [1, 5, 9, 13], w(2), w(3));
    g(s, [2, 6, 10, 14], w(4), w(5));
    g(s, [3, 7, 11, 15], w(6), w(7));
    g(s, [0, 5, 10, 15], w(8), w(9));
    g(s, [1, 6, 11, 12], w(10), w(11));
    g(s, [2, 7, 8, 13], w(12), w(13));
    g(s, [3, 4, 9, 14], w(14), w(15));
}

/// The next chaining values of two blocks compressed side by side, each its own chaining value,
/// message and counter, both of one block length and one `flags`: what [`crate::compress`]
/// gives in its first eight words, for each. Never the root (its last eight words are not
/// made).
#[inline(always)]
pub(crate) fn compress(
    cv: [&[u32; 8]; 2],
    message: [&[u32; 16]; 2],
    counter: [u64; 2],
    block_len: u32,
    flags: u32,
) -> [[u32; 8]; 2] {
    let state = |p: usize| -> [u32; 16] {
        [
            cv[p][0],
            cv[p][1],
            cv[p][2],
            cv[p][3],
            cv[p][4],
            cv[p][5],
            cv[p][6],
            cv[p][7],
            IV[0],
            IV[1],
            IV[2],
            IV[3],
            // The counter's low word, then its high word: dropping the other half is the point.
            counter[p].truncate_to::<u32>(),
            (counter[p] >> 32).truncate_to::<u32>(),
            block_len,
            flags,
        ]
    };
    let mut s: States = [state(0), state(1)];
    round(&mut s, message, &MESSAGE[0]);
    round(&mut s, message, &MESSAGE[1]);
    round(&mut s, message, &MESSAGE[2]);
    round(&mut s, message, &MESSAGE[3]);
    round(&mut s, message, &MESSAGE[4]);
    round(&mut s, message, &MESSAGE[5]);
    round(&mut s, message, &MESSAGE[6]);
    [0, 1].map(|p| core::array::from_fn(|word| s[p][word] ^ s[p][word + 8]))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use blaze_sim::{Config, Tape, check};

    use crate::{CHUNK_END, CHUNK_START, KEYED_HASH, PARENT, first_8};

    /// Each side of the pair is the scalar compression of its own block: drawn chaining values,
    /// messages, counters across the 32-bit edge, every block length and flag set a tree uses.
    #[test]
    fn each_side_is_the_scalar_compression() {
        check(Config::cases(2_000), |tape: &Tape| {
            let words = |label: &'static str| -> [u32; 16] { core::array::from_fn(|_| tape.next_u32(label)) };
            let cv: [[u32; 8]; 2] = [first_8(&words("cv")), first_8(&words("cv"))];
            let message = [words("message"), words("message")];
            let near_edge = || -> u64 {
                if tape.coin("any counter") {
                    tape.next_u64("counter")
                } else {
                    (1u64 << 32) - 2 + tape.below(4, "counter at the 32-bit edge")
                }
            };
            let counter = [near_edge(), near_edge()];
            let block_len = u32::try_from(tape.range(0..65, "block length")).expect("at most 64");
            let flags = *tape
                .pick(&[0, CHUNK_START, CHUNK_END, CHUNK_START | CHUNK_END, PARENT, KEYED_HASH | PARENT], "flags")
                .expect("six flag sets");
            let got = super::compress([&cv[0], &cv[1]], [&message[0], &message[1]], counter, block_len, flags);
            for p in 0..2 {
                let want = first_8(&crate::compress(&cv[p], &message[p], counter[p], block_len, flags));
                assert_eq!(got[p], want, "side {p} of the pair");
            }
        });
    }
}

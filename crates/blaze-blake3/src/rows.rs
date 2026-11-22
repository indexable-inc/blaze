//! BLAKE3's compression of one block with the state as four rows of four `u32` lanes
//! (`core::simd::u32x4`, one NEON or SSE register a row): [`crate::Single::Rows`].
//!
//! Row 0 is state words 0 to 3, row 1 words 4 to 7, row 2 words 8 to 11, row 3 words 12 to
//! 15, so the four column G functions of a half-round are one G on whole rows. Rotating row 1
//! left by one lane, row 2 by two and row 3 by three puts each diagonal (0, 5, 10, 15), (1, 6,
//! 11, 12), (2, 7, 8, 13), (3, 4, 9, 14) in a column, so the diagonal step is the same G; the
//! rotations by three, two and one undo it. The rotations of words by 16 and 8 bits are byte
//! shuffles and by 12 and 7 a shift and a shift-insert ([`crate::lanes`]' `rotate_bytes` and
//! `rotate`, the one home of both). Each round's two message vectors of each step are gathered
//! from the block by the constant indices of [`crate::MESSAGE`], so no round moves the message.
//!
//! What to expect, and why it is not the default: the scalar form runs each half-round's four G
//! functions side by side on the integer units, a chain of fourteen one-cycle operations a
//! half-round. This form runs them in one register, a chain of about sixteen vector operations
//! a half-round plus the three lane rotations between half-rounds; on Apple's cores a NEON
//! add, xor, shift or table lookup has two cycles of latency where an integer one has one, so
//! the chain is about twice as long and the form is expected SLOWER there. On x86-64 a vector
//! add or shuffle has one cycle of latency and the integer core has fewer ports, which is why
//! the official crate's SSE4.1 and AVX-512 compressions take this form. `examples/cost.rs`
//! prints both columns; the default changes only on its numbers.

use core::simd::Simd;

use blaze_int::Int;

use crate::lanes::{rotate, rotate_bytes};
use crate::{IV, MESSAGE};

type Row = Simd<u32, 4>;

/// One G on whole rows: the four columns (or, turned, the four diagonals) at once.
#[inline(always)]
fn g(a: &mut Row, b: &mut Row, c: &mut Row, d: &mut Row, x: Row, y: Row) {
    *a = *a + *b + x;
    *d = rotate_bytes::<4, 2>(*d ^ *a);
    *c = *c + *d;
    *b = rotate(*b ^ *c, 12);
    *a = *a + *b + y;
    *d = rotate_bytes::<4, 1>(*d ^ *a);
    *c = *c + *d;
    *b = rotate(*b ^ *c, 7);
}

/// The four message words of `block` that slots `at` of a round read, where `t` says.
#[inline(always)]
fn gather(block: &[u32; 16], t: &[usize; 16], at: [usize; 4]) -> Row {
    Row::from_array([block[t[at[0]]], block[t[at[1]]], block[t[at[2]]], block[t[at[3]]]])
}

/// One round: the column step, the diagonals turned into columns, the diagonal step, the turn
/// undone. Called seven times with a constant `t`, so every gather index is a constant.
#[inline(always)]
fn round(a: &mut Row, b: &mut Row, c: &mut Row, d: &mut Row, block: &[u32; 16], t: &[usize; 16]) {
    g(a, b, c, d, gather(block, t, [0, 2, 4, 6]), gather(block, t, [1, 3, 5, 7]));
    *b = b.rotate_elements_left::<1>();
    *c = c.rotate_elements_left::<2>();
    *d = d.rotate_elements_left::<3>();
    g(a, b, c, d, gather(block, t, [8, 10, 12, 14]), gather(block, t, [9, 11, 13, 15]));
    *b = b.rotate_elements_right::<1>();
    *c = c.rotate_elements_right::<2>();
    *d = d.rotate_elements_right::<3>();
}

/// [`crate::compress`] in rows: the same sixteen output words for every input.
#[inline(always)]
pub(crate) fn compress(cv: &[u32; 8], block: &[u32; 16], counter: u64, block_len: u32, flags: u32) -> [u32; 16] {
    let low = Row::from_array([cv[0], cv[1], cv[2], cv[3]]);
    let high = Row::from_array([cv[4], cv[5], cv[6], cv[7]]);
    let mut a = low;
    let mut b = high;
    let mut c = Row::from_array([IV[0], IV[1], IV[2], IV[3]]);
    // The counter's low word, then its high word: dropping the other half is the point.
    let mut d = Row::from_array([counter.truncate_to::<u32>(), (counter >> 32).truncate_to::<u32>(), block_len, flags]);
    round(&mut a, &mut b, &mut c, &mut d, block, &MESSAGE[0]);
    round(&mut a, &mut b, &mut c, &mut d, block, &MESSAGE[1]);
    round(&mut a, &mut b, &mut c, &mut d, block, &MESSAGE[2]);
    round(&mut a, &mut b, &mut c, &mut d, block, &MESSAGE[3]);
    round(&mut a, &mut b, &mut c, &mut d, block, &MESSAGE[4]);
    round(&mut a, &mut b, &mut c, &mut d, block, &MESSAGE[5]);
    round(&mut a, &mut b, &mut c, &mut d, block, &MESSAGE[6]);
    // Words 0 to 7 take the xor of words 8 to 15, which take the xor of the chaining value:
    // the scalar form's final loop, a row at a time.
    let rows = [(a ^ c).to_array(), (b ^ d).to_array(), (c ^ low).to_array(), (d ^ high).to_array()];
    core::array::from_fn(|word| rows[word / 4][word % 4])
}

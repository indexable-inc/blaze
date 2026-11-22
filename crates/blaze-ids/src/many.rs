//! [`ContentId::of_many`]: many short inputs hashed a SIMD lane each.
//!
//! A one-chunk input's blocks before its last go through [`leading_lanes`], and each input's
//! last block, of any length, through the same lane-wise compression, 16 at a time on AVX-512
//! and 8 elsewhere: `blaze_blake3::lanes::compress`, the one that also hashes the hasher's
//! whole chunks side by side. `of_many_is_of_for_each` holds every path to the hasher at every
//! length and grouping, and the tests here hold one lane from the key to the `blake3` crate,
//! so a change in any fails there and not in an ID.

use std::cell::RefCell;

#[cfg(target_arch = "x86_64")]
use blaze_blake3::cpu::{self, Feature};
use blaze_blake3::{
    CHUNK_END, CHUNK_START, DERIVE_KEY_MATERIAL, ROOT,
    lanes::{block_words, columns, rows_of},
};

use crate::{
    ContentId,
    b3::{self, BLOCK_LEN, CHUNK_LEN, ContextKey},
    radix,
};

/// How a backend hashes what [`compress_lanes`] doesn't: a one-chunk input's blocks before its
/// last, and an input longer than a chunk.
pub(crate) trait Leading {
    /// The chaining value after the first `blocks` blocks (1 to 15) of each of `members`, a
    /// SIMD lane each, pushed onto `cvs` in their order.
    fn leading_blocks(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    );

    /// The derived key of `input`, longer than a chunk, under the context key `key`.
    fn long(key: &ContextKey, input: &[u8]) -> [u8; 32];
}

/// A thread's buffers for [`of_many_into`], kept warm across calls: the inputs grouped by
/// leading blocks, the grouping's cursors and one group's chaining values.
struct Kept {
    groups: radix::Csr<u32, std::alloc::Global>,
    cursors: Vec<u32>,
    cvs: Vec<[u32; 8]>,
}

thread_local! {
    static KEPT: RefCell<Kept> = const {
        RefCell::new(Kept {
            groups: radix::Csr::new(),
            cursors: Vec::new(), // alloc: empty until a thread's first call, then reused
            cvs: Vec::new(),     // alloc: empty until a thread's first call, then reused
        })
    };
}

/// Blocks a one-chunk input has before its last: 0 for one of at most a block.
fn leading(len: usize) -> usize {
    len.saturating_sub(1) / BLOCK_LEN
}

pub(crate) fn of_many(key: &ContextKey, inputs: &[&[u8]]) -> Vec<ContentId> {
    // alloc: the result, which the caller keeps; `of_many_into` fills one of the caller's
    let mut out = Vec::with_capacity(inputs.len());
    of_many_into::<b3::Many>(key, inputs, &mut out);
    out
}

/// [`of_many`] by the backend `L`, into `out`, cleared first: a warm call with room allocates
/// nothing.
pub(crate) fn of_many_into<L: Leading>(
    key: &ContextKey,
    inputs: &[&[u8]],
    out: &mut Vec<ContentId>,
) {
    out.clear();
    out.resize(inputs.len(), ContentId::default());
    KEPT.with_borrow_mut(|kept| hash_into::<L>(key, inputs, out, kept));
}

/// `bytes`' eight little-endian words.
fn words_of(bytes: &[u8; 32]) -> [u32; 8] {
    std::array::from_fn(|word| {
        u32::from_le_bytes([
            bytes[4 * word],
            bytes[4 * word + 1],
            bytes[4 * word + 2],
            bytes[4 * word + 3],
        ])
    })
}

fn hash_into<L: Leading>(
    key: &ContextKey,
    inputs: &[&[u8]],
    out: &mut [ContentId],
    kept: &mut Kept,
) {
    let words = words_of(key);
    let Kept { groups, cursors, cvs } = kept;
    groups.rebuild(
        CHUNK_LEN / BLOCK_LEN,
        inputs
            .iter()
            .enumerate()
            .filter(|(_, input)| input.len() <= CHUNK_LEN)
            .map(|(at, input)| (leading(input.len()) as u32, at as u32)),
        cursors,
    );
    for (at, input) in inputs.iter().enumerate() {
        if input.len() > CHUNK_LEN {
            out[at] = ContentId(L::long(key, input));
        }
    }
    for group in 0..groups.rows() {
        let members = groups.row(group);
        if members.is_empty() {
            continue;
        }
        let start = if group == 0 {
            Start::Key(&words)
        } else {
            cvs.clear();
            L::leading_blocks(&words, inputs, members, group, cvs);
            Start::After(cvs.as_slice())
        };
        last_blocks(inputs, members, start, group, out);
    }
}

/// What a group's last blocks are compressed from: the context key itself, for inputs of at
/// most one block, else each member's chaining value after its leading blocks.
#[derive(Clone, Copy)]
enum Start<'a> {
    Key(&'a [u32; 8]),
    After(&'a [[u32; 8]]),
}

/// Each of `members`' last block, the bytes from block `group` on, compressed from `start` as
/// its chunk's end and the root, its content ID written to `out`: 16 lanes in AVX-512 registers
/// and 8 in AVX2 ones where the CPU has them, else [`LANES`] at the target's baseline.
fn last_blocks(
    inputs: &[&[u8]],
    members: &[u32],
    start: Start<'_>,
    group: usize,
    out: &mut [ContentId],
) {
    #[cfg(target_arch = "x86_64")]
    {
        if cpu::has(Feature::Avx512f) {
            return x86::last_blocks_avx512(inputs, members, start, group, out);
        }
        if cpu::has(Feature::Avx2) {
            return x86::last_blocks_avx2(inputs, members, start, group, out);
        }
    }
    last_blocks_of::<LANES>(inputs, members, start, group, out);
}

/// [`last_blocks`] `N` members at a time. Always inlined, so it is vectorized for the features
/// of the function it is inlined into.
#[inline(always)]
fn last_blocks_of<const N: usize>(
    inputs: &[&[u8]],
    members: &[u32],
    start: Start<'_>,
    group: usize,
    out: &mut [ContentId],
) {
    let first = if group == 0 { CHUNK_START } else { 0 };
    let flags = DERIVE_KEY_MATERIAL | first | CHUNK_END | ROOT;
    for (n, batch) in members.chunks(N).enumerate() {
        let cv: [[u32; N]; 8] = match start {
            Start::Key(key) => key.map(|word| [word; N]),
            Start::After(cvs) => {
                let mut cv = [[0; N]; 8];
                for (lane, words) in cvs[n * N..n * N + batch.len()].iter().enumerate() {
                    for (word, &value) in words.iter().enumerate() {
                        cv[word][lane] = value;
                    }
                }
                cv
            }
        };
        let mut rows = [[0; 16]; N];
        let mut block_len = [0; N];
        for (lane, &at) in batch.iter().enumerate() {
            let last = &inputs[at as usize][group * BLOCK_LEN..];
            block_len[lane] = last.len() as u32;
            rows[lane] = padded_words(last);
        }
        let ids = rows_of(&compress_lanes(&cv, &columns(&rows), block_len, flags));
        for (lane, &at) in batch.iter().enumerate() {
            out[at as usize] = ContentId(std::array::from_fn(|byte| {
                (ids[lane][byte / 4] >> (8 * (byte % 4))) as u8
            }));
        }
    }
}

/// The sixteen words of a last block, `bytes` (at most 64 of them) and then zeros: read eight
/// bytes at a time, the last word read overlapping the one before it and shifted, so a block
/// costs a load a word, with no byte loop and no `memcpy` call into a zeroed block.
#[inline(always)]
fn padded_words(bytes: &[u8]) -> [u32; 16] {
    assert!(bytes.len() <= BLOCK_LEN, "a last block of {} bytes", bytes.len());
    let mut words = [0u64; BLOCK_LEN / 8];
    for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
        *word = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
    }
    let rest = bytes.len() % 8;
    if rest > 0 {
        words[bytes.len() / 8] = last_bytes(bytes, rest);
    }
    std::array::from_fn(|word| (words[word / 2] >> (32 * (word % 2))) as u32)
}

/// The last `rest` bytes of `bytes` (1 to 7 of them), little-endian in the low bytes of a word:
/// one load where eight bytes are there to read, else two that overlap, or three bytes.
#[inline(always)]
fn last_bytes(bytes: &[u8], rest: usize) -> u64 {
    let len = bytes.len();
    if len >= 8 {
        return u64::from_le_bytes(bytes[len - 8..].try_into().expect("8 bytes"))
            >> (8 * (8 - rest));
    }
    if rest >= 4 {
        let low = u64::from(u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")));
        let high =
            u64::from(u32::from_le_bytes(bytes[rest - 4..rest].try_into().expect("4 bytes")));
        return low | high << (8 * (rest - 4));
    }
    let (first, middle, last) =
        (u64::from(bytes[0]), u64::from(bytes[rest / 2]), u64::from(bytes[rest - 1]));
    first | middle << (8 * (rest / 2)) | last << (8 * (rest - 1))
}

/// Last blocks compressed together at the baseline, a lane each: one AVX2 register of `u32`s,
/// two NEON ones.
const LANES: usize = 8;

/// BLAKE3's compression of one block of chunk 0 in each of `N` lanes, each with its own block
/// length: `blaze_blake3::lanes::compress` at counter 0. Gives the chaining value, the first 32
/// bytes of the output when `flags` has `ROOT`. Always inlined, so a caller compiled with more
/// vector features (`x86::leading_lanes_avx512`) vectorizes it with them.
#[inline(always)]
fn compress_lanes<const N: usize>(
    cv: &[[u32; N]; 8],
    message: &[[u32; N]; 16],
    block_len: [u32; N],
    flags: u32,
) -> [[u32; N]; 8] {
    blaze_blake3::lanes::compress(cv, message, [0; N], [0; N], block_len, flags)
}

/// [`Leading::leading_blocks`] by [`compress_lanes`]: 16 members at a time in AVX-512 registers
/// where the CPU has them, 8 in AVX2 ones, else [`LANES`].
pub(crate) fn leading_lanes(
    key: &[u32; 8],
    inputs: &[&[u8]],
    members: &[u32],
    blocks: usize,
    cvs: &mut Vec<[u32; 8]>,
) {
    #[cfg(target_arch = "x86_64")]
    {
        if cpu::has(Feature::Avx512f) {
            return x86::leading_lanes_avx512(key, inputs, members, blocks, cvs);
        }
        if cpu::has(Feature::Avx2) {
            return x86::leading_lanes_avx2(key, inputs, members, blocks, cvs);
        }
    }
    leading_lanes_of::<LANES>(key, inputs, members, blocks, cvs);
}

/// [`leading_lanes`] `N` members at a time, a block of each per compression. Always inlined, so
/// it is vectorized for the features of the function it is inlined into.
#[inline(always)]
fn leading_lanes_of<const N: usize>(
    key: &[u32; 8],
    inputs: &[&[u8]],
    members: &[u32],
    blocks: usize,
    cvs: &mut Vec<[u32; 8]>,
) {
    let flags = DERIVE_KEY_MATERIAL;
    for batch in members.chunks(N) {
        // Lanes past the batch hash zeros; only `..batch.len()` is kept.
        let mut cv: [[u32; N]; 8] = key.map(|word| [word; N]);
        for block in 0..blocks {
            let mut rows = [[0; 16]; N];
            for (lane, &at) in batch.iter().enumerate() {
                let bytes = &inputs[at as usize][block * BLOCK_LEN..(block + 1) * BLOCK_LEN];
                rows[lane] = block_words(bytes.try_into().expect("a whole block"));
            }
            let first = if block == 0 { CHUNK_START } else { 0 };
            cv = compress_lanes(&cv, &columns(&rows), [BLOCK_LEN as u32; N], flags | first);
        }
        cvs.extend((0..batch.len()).map(|lane| std::array::from_fn(|word| cv[word][lane])));
    }
}

/// [`leading_lanes_of`] and [`last_blocks_of`] compiled for AVX-512F at 16 lanes (each word of
/// the transposed state one 512-bit register, the rotates `vprord`) and for AVX2 at 8 (one
/// 256-bit register a word), where the baseline would give them SSE2's two 128-bit registers
/// a word and three instructions a rotate.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod x86 {
    use blaze_blake3::cpu::{self, Feature};

    use super::Start;
    use crate::ContentId;

    pub(super) fn leading_lanes_avx512(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    ) {
        assert!(
            cpu::has(Feature::Avx512f),
            "AVX-512F lanes on a CPU without it"
        );
        // SAFETY: the CPU has AVX-512F, asserted just above.
        unsafe { leading_wide(key, inputs, members, blocks, cvs) }
    }

    pub(super) fn leading_lanes_avx2(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    ) {
        assert!(cpu::has(Feature::Avx2), "AVX2 lanes on a CPU without it");
        // SAFETY: the CPU has AVX2, asserted just above.
        unsafe { leading_narrow(key, inputs, members, blocks, cvs) }
    }

    pub(super) fn last_blocks_avx512(
        inputs: &[&[u8]],
        members: &[u32],
        start: Start<'_>,
        group: usize,
        out: &mut [ContentId],
    ) {
        assert!(
            cpu::has(Feature::Avx512f),
            "AVX-512F lanes on a CPU without it"
        );
        // SAFETY: the CPU has AVX-512F, asserted just above.
        unsafe { last_wide(inputs, members, start, group, out) }
    }

    pub(super) fn last_blocks_avx2(
        inputs: &[&[u8]],
        members: &[u32],
        start: Start<'_>,
        group: usize,
        out: &mut [ContentId],
    ) {
        assert!(cpu::has(Feature::Avx2), "AVX2 lanes on a CPU without it");
        // SAFETY: the CPU has AVX2, asserted just above.
        unsafe { last_narrow(inputs, members, start, group, out) }
    }

    #[target_feature(enable = "avx512f")]
    fn leading_wide(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    ) {
        super::leading_lanes_of::<16>(key, inputs, members, blocks, cvs);
    }

    #[target_feature(enable = "avx2")]
    fn leading_narrow(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    ) {
        super::leading_lanes_of::<8>(key, inputs, members, blocks, cvs);
    }

    #[target_feature(enable = "avx512f")]
    fn last_wide(
        inputs: &[&[u8]],
        members: &[u32],
        start: Start<'_>,
        group: usize,
        out: &mut [ContentId],
    ) {
        super::last_blocks_of::<16>(inputs, members, start, group, out);
    }

    #[target_feature(enable = "avx2")]
    fn last_narrow(
        inputs: &[&[u8]],
        members: &[u32],
        start: Start<'_>,
        group: usize,
        out: &mut [ContentId],
    ) {
        super::last_blocks_of::<8>(inputs, members, start, group, out);
    }
}

#[cfg(test)]
mod tests;

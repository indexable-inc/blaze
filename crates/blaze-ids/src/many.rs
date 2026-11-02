//! [`ContentId::of_many`]: many short inputs hashed a SIMD lane each.
//!
//! A one-chunk input's blocks before its last go through the backend's [`Leading`] (official
//! BLAKE3's `hash_many`, or [`leading_lanes`] under `blake3-servil`, which exports no kernel
//! that stops short of a root); each input's last block, of any length, through
//! [`compress_lanes`], 16 at a time on AVX-512 and 8 elsewhere. `compress_lanes` is ours: `of_many_is_of_for_each` and
//! the backends' differential test hold every path to the hasher at every length and grouping,
//! so a change in any fails there and not in an ID.

use std::{cell::RefCell, simd::Simd};

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
    fn leading_blocks(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>);

    /// The derived key of `input`, longer than a chunk, under the context key `key`.
    fn long(key: &ContextKey, input: &[u8]) -> [u8; 32];
}

/// A thread's buffers for [`of_many_into`], kept warm across calls: the inputs grouped by
/// leading blocks, the grouping's cursors and one group's chaining values.
struct Kept {
    groups: radix::Csr<u32>,
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

/// BLAKE3's block flags.
const CHUNK_START: u8 = 1;
const CHUNK_END: u8 = 2;
const ROOT: u8 = 8;
const DERIVE_KEY_MATERIAL: u8 = 64;

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
pub(crate) fn of_many_into<L: Leading>(key: &ContextKey, inputs: &[&[u8]], out: &mut Vec<ContentId>) {
    out.clear();
    out.resize(inputs.len(), ContentId::default());
    KEPT.with_borrow_mut(|kept| hash_into::<L>(key, inputs, out, kept));
}

/// `bytes`' eight little-endian words.
fn words_of(bytes: &[u8; 32]) -> [u32; 8] {
    std::array::from_fn(|word| {
        u32::from_le_bytes([bytes[4 * word], bytes[4 * word + 1], bytes[4 * word + 2], bytes[4 * word + 3]])
    })
}

fn hash_into<L: Leading>(key: &ContextKey, inputs: &[&[u8]], out: &mut [ContentId], kept: &mut Kept) {
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
fn last_blocks(inputs: &[&[u8]], members: &[u32], start: Start<'_>, group: usize, out: &mut [ContentId]) {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512f") {
            return x86::last_blocks_avx512(inputs, members, start, group, out);
        }
        if std::arch::is_x86_feature_detected!("avx2") {
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
    let flags = u32::from(DERIVE_KEY_MATERIAL | first | CHUNK_END | ROOT);
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
            out[at as usize] = ContentId(std::array::from_fn(|byte| (ids[lane][byte / 4] >> (8 * (byte % 4))) as u8));
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
        return u64::from_le_bytes(bytes[len - 8..].try_into().expect("8 bytes")) >> (8 * (8 - rest));
    }
    if rest >= 4 {
        let low = u64::from(u32::from_le_bytes(bytes[..4].try_into().expect("4 bytes")));
        let high = u64::from(u32::from_le_bytes(bytes[rest - 4..rest].try_into().expect("4 bytes")));
        return low | high << (8 * (rest - 4));
    }
    let (first, middle, last) = (u64::from(bytes[0]), u64::from(bytes[rest / 2]), u64::from(bytes[rest - 1]));
    first | middle << (8 * (rest / 2)) | last << (8 * (rest - 1))
}

/// Eight words of `S` lanes (a compression's output) as each lane's eight words: [`columns`]'
/// transpose the other way, in `S`-square registers (zero rows filling a square past the eight).
#[inline(always)]
fn rows_of<const S: usize>(columns: &[[u32; S]; 8]) -> [[u32; 8]; S] {
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

/// A block's sixteen little-endian words.
#[inline(always)]
fn block_words(block: &[u8; BLOCK_LEN]) -> [u32; 16] {
    std::array::from_fn(|word| {
        u32::from_le_bytes([block[4 * word], block[4 * word + 1], block[4 * word + 2], block[4 * word + 3]])
    })
}

/// Each lane's sixteen message words (`rows[lane]`) as sixteen columns of `S` lanes, the form
/// [`compress_lanes`] takes: `16 / S` square transposes in registers, so a block goes from its
/// bytes to the compression with no scattered store and reload.
#[inline(always)]
fn columns<const S: usize>(rows: &[[u32; 16]; S]) -> [[u32; S]; 16] {
    const { assert!(S.is_power_of_two() && S <= 16) };
    let mut out = [[0; S]; 16];
    for from in (0..16).step_by(S) {
        let square =
            transpose(std::array::from_fn(|lane| Simd::from_array(std::array::from_fn(|word| rows[lane][from + word]))));
        for (word, column) in square.iter().enumerate() {
            out[from + word] = column.to_array();
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

/// Last blocks compressed together at the baseline, a lane each: one AVX2 register of `u32`s,
/// two NEON ones.
const LANES: usize = 8;

const IV: [u32; 4] = [0x6A09_E667, 0xBB67_AE85, 0x3C6E_F372, 0xA54F_F53A];
const SCHEDULE: [usize; 16] = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8];

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

/// BLAKE3's compression of one block of chunk 0 in each of `N` lanes, each with its own
/// block length: what `hash_many` can't do, since it gives every lane full blocks. Transposed
/// (`message[w]` is word `w` of every lane), so each step is one instruction over the lanes
/// once LLVM vectorizes the loops; safe code, so no SIMD of our own to prove. Gives the
/// chaining value, the first 32 bytes of the output when `flags` has `ROOT`. Always inlined, so
/// a caller compiled with more vector features (`x86::leading_lanes_avx512`) vectorizes it
/// with them.
#[inline(always)]
fn compress_lanes<const N: usize>(
    cv: &[[u32; N]; 8],
    message: &[[u32; N]; 16],
    block_len: [u32; N],
    flags: u32,
) -> [[u32; N]; 8] {
    let mut v = [[0; N]; 16];
    v[..8].copy_from_slice(cv);
    for (word, iv) in IV.iter().enumerate() {
        v[8 + word] = [*iv; N];
    }
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

/// [`Leading::leading_blocks`] by [`compress_lanes`], for a backend with no kernel that stops
/// short of a chunk's root: 16 members at a time in AVX-512 registers where the CPU has them
/// (x86-64 Linux, the fork's one x86 target), else [`LANES`].
#[cfg_attr(
    not(all(
        target_endian = "little",
        any(
            all(target_arch = "x86_64", target_os = "linux"),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
        ),
    )),
    expect(dead_code, reason = "blake3-servil's way alone, and it doesn't build for this target")
)]
pub(crate) fn leading_lanes(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>) {
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        if std::arch::is_x86_feature_detected!("avx512f") {
            return x86::leading_lanes_avx512(key, inputs, members, blocks, cvs);
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            return x86::leading_lanes_avx2(key, inputs, members, blocks, cvs);
        }
    }
    leading_lanes_of::<LANES>(key, inputs, members, blocks, cvs);
}

/// [`leading_lanes`] `N` members at a time, a block of each per compression. Always inlined, so
/// it is vectorized for the features of the function it is inlined into.
#[cfg_attr(
    not(all(
        target_endian = "little",
        any(
            all(target_arch = "x86_64", target_os = "linux"),
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")),
        ),
    )),
    expect(dead_code, reason = "blake3-servil's way alone, and it doesn't build for this target")
)]
#[inline(always)]
fn leading_lanes_of<const N: usize>(
    key: &[u32; 8],
    inputs: &[&[u8]],
    members: &[u32],
    blocks: usize,
    cvs: &mut Vec<[u32; 8]>,
) {
    let flags = u32::from(DERIVE_KEY_MATERIAL);
    for batch in members.chunks(N) {
        // Lanes past the batch hash zeros; only `..batch.len()` is kept.
        let mut cv: [[u32; N]; 8] = key.map(|word| [word; N]);
        for block in 0..blocks {
            let mut rows = [[0; 16]; N];
            for (lane, &at) in batch.iter().enumerate() {
                let bytes = &inputs[at as usize][block * BLOCK_LEN..(block + 1) * BLOCK_LEN];
                rows[lane] = block_words(bytes.try_into().expect("a whole block"));
            }
            let first = if block == 0 { u32::from(CHUNK_START) } else { 0 };
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
    use super::Start;
    use crate::ContentId;

    #[cfg(target_os = "linux")]
    pub(super) fn leading_lanes_avx512(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    ) {
        assert!(std::arch::is_x86_feature_detected!("avx512f"), "AVX-512F lanes on a CPU without it");
        // SAFETY: the CPU has AVX-512F, asserted just above.
        unsafe { leading_wide(key, inputs, members, blocks, cvs) }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn leading_lanes_avx2(
        key: &[u32; 8],
        inputs: &[&[u8]],
        members: &[u32],
        blocks: usize,
        cvs: &mut Vec<[u32; 8]>,
    ) {
        assert!(std::arch::is_x86_feature_detected!("avx2"), "AVX2 lanes on a CPU without it");
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
        assert!(std::arch::is_x86_feature_detected!("avx512f"), "AVX-512F lanes on a CPU without it");
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
        assert!(std::arch::is_x86_feature_detected!("avx2"), "AVX2 lanes on a CPU without it");
        // SAFETY: the CPU has AVX2, asserted just above.
        unsafe { last_narrow(inputs, members, start, group, out) }
    }

    #[cfg(target_os = "linux")]
    #[target_feature(enable = "avx512f")]
    fn leading_wide(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>) {
        super::leading_lanes_of::<16>(key, inputs, members, blocks, cvs);
    }

    #[cfg(target_os = "linux")]
    #[target_feature(enable = "avx2")]
    fn leading_narrow(key: &[u32; 8], inputs: &[&[u8]], members: &[u32], blocks: usize, cvs: &mut Vec<[u32; 8]>) {
        super::leading_lanes_of::<8>(key, inputs, members, blocks, cvs);
    }

    #[target_feature(enable = "avx512f")]
    fn last_wide(inputs: &[&[u8]], members: &[u32], start: Start<'_>, group: usize, out: &mut [ContentId]) {
        super::last_blocks_of::<16>(inputs, members, start, group, out);
    }

    #[target_feature(enable = "avx2")]
    fn last_narrow(inputs: &[&[u8]], members: &[u32], start: Start<'_>, group: usize, out: &mut [ContentId]) {
        super::last_blocks_of::<8>(inputs, members, start, group, out);
    }
}

#[cfg(test)]
mod tests;

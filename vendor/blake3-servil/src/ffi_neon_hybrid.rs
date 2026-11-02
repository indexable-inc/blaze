//! `hash_many` for AArch64, built from the kernels in
//! c/blake3_neon_hybrid_aarch64.S: with the SHA-3 extension the table
//! below, without it the scalar and quad kernels alone (`PLAIN_PLANS`).
//!
//! Each kernel hashes a fixed number of inputs given as a pointer table. Chunk
//! kernels (`k<n>`) take whole 1024-byte chunks with a counter that
//! increments per chunk; parent kernels (`p<n>`) take 64-byte blocks with
//! one shared counter. This wrapper splits an input count into kernel calls
//! that keep the integer and vector units busy together:
//!
//! | inputs | chunk kernels | parent kernels |
//! |--------|---------------|----------------|
//! | 1      | k1            | k1             |
//! | 2      | k2            | p2             |
//! | 3      | k3            | p3             |
//! | 4      | k4            | p4             |
//! | 5      | k5            | p5             |
//! | 6      | k6            | p3 + p3        |
//! | 7      | k7            | p7             |
//! | 8      | k8            | p8             |
//! | 9      | k9            | p9             |
//! | 10     | k10           | p7 + p3        |
//! | 11     | k8 + k3       | p9 + p2        |
//! | 12     | k9 + k3       | p9 + p3        |
//! | 13     | k10 + k3      | p9 + p4        |
//! | 14     | k10 + k4      | p9 + p5        |
//! | 15     | k10 + k5      | p8 + p7        |
//! | 16     | k10 + k6      | p8 + p8        |
//!
//! Longer input lists are hashed sixteen at a time. k10 is the fastest
//! kernel per chunk (two scalar chunks beside eight NEON ones: 0.21 ns/B
//! on a two-CPU SME2 VM against k8's 0.26 and k4's 0.29), so counts
//! from 13 up lead with it, and `Platform::NEON` reports a degree of 16 so
//! the tree walk hands over sixteen chunks at a time (0.23 ns/B in bulk,
//! against 0.29 at a degree of four).
//!
//! Plans are judged on an M4 Max P-core, its E-core, and the VM together.
//! Seven chunks on k7 and twelve on k9 + k3 beat k4 + k3 and k8 + k4 on all
//! three (cycles per byte P / E: 1.00 / 1.51 against 1.19 / 2.08, and
//! 1.02 / 1.52 against 1.10 / 1.89). E-cores have fewer integer units, so
//! kernels with two scalar chunks (k4, k6, k10) run twice the P-core's
//! cycles there, the others 1.1 to 1.6 times. k8 is two scalar chunks
//! beside a quad and a pair, the user's choice over two quads: 18% fewer
//! cycles on a P-core (7180 against 8718 for eight chunks), 6.5% more on an
//! E-core (14170 against 13310).
//!
//! p4 is two scalar blocks beside a pair, as k4 is for chunks: four
//! one-block messages 20.3 ns/msg against two pairs' 23.2 on an M4 Max
//! P-core (348 cycles per call against 395; jobs 382-389), 14% faster on
//! the VM; 30% more cycles on an E-core (720 per call against 555; jobs
//! 396-399), a cost Zooko accepted for top speed where most work runs.
//! One quad was 14% slower on the VM.
//!
//! The scalar kernel also serves every single-chunk job: `hash_chunk` runs
//! a whole input of one chunk or less, root compression included, in one
//! call; `compress_blocks` is `ChunkState::update`'s inner loop; and
//! `compress_in_place` is one compression of any block length, which the
//! incremental `Hasher` uses for chunk finalization, parent nodes, and the
//! root. Every AArch64 core runs the scalar kernel, so those need no
//! feature check.

use crate::{BLOCK_LEN, CHUNK_LEN, CVWords, IncrementCounter, OUT_LEN};

/// The kernels live in c/blake3_neon_hybrid_aarch64.S. Every one has the
/// signature `(base, blocks, key, counter, packed_flags, out)`; the contract
/// is in that file's header. `packed_flags` is
/// `flags | flags_start << 8 | flags_end << 16 | last_len << 24`.
mod asm {
    unsafe extern "C" {
        /// k1 entered with the input pointer itself in place of a table,
        /// and the address of the last block as a seventh argument: blocks
        /// 0..blocks-1 come from `input`, the last from `last`.
        pub fn blake3_servil_hybrid_c1(input: *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, last: *const u8);
        pub fn blake3_servil_hybrid_k1(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k2(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k3(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k4(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k5(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k6(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k7(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k8(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k9(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_k10(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        /// q<n>: n whole chunks and a partial chunk, `n + 1` inputs. The
        /// partial chunk is the last input, read as `blocks` whole blocks
        /// (zero-padded), its final block recording `last_len` bytes;
        /// `partial` is `blocks | last_len << 8`.
        pub fn blake3_servil_hybrid_q1(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q2(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q3(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q4(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q7(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q5(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q6(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q8(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_q9(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8, partial: u64);
        pub fn blake3_servil_hybrid_p2(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_p3(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_p5(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_p7(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_p9(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_p4(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
        pub fn blake3_servil_hybrid_p8(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
    }
    /// The kernels on scalars and quads alone, which need no SHA-3.
    macro_rules! plain {
        ($($name:ident),*) => {
            unsafe extern "C" {
                $(pub fn $name(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);)*
            }
        };
    }
    plain!(blake3_servil_hybrid_kn2, blake3_servil_hybrid_kn4, blake3_servil_hybrid_kn5, blake3_servil_hybrid_kn6, blake3_servil_hybrid_kn8);
    plain!(blake3_servil_hybrid_pn2, blake3_servil_hybrid_pn4, blake3_servil_hybrid_pn5, blake3_servil_hybrid_pn6, blake3_servil_hybrid_pn10);
    // Extended output, one, four and eight blocks: (table, 1, cv, counter,
    // packed, out), 64 bytes a block.
    plain!(blake3_servil_hybrid_x1, blake3_servil_hybrid_x4, blake3_servil_hybrid_x8);
}

/// Fill `out`'s whole groups of four 64-byte blocks with extended output
/// (block i: `block` compressed from `cv` at `counter + i` with `flags` and
/// `block_len`, both halves of the state), eight blocks a call where they
/// fit (x8), then four (x4); the bytes filled, a multiple of 256. Every
/// AArch64 CPU runs them (quads, which need no SHA-3).
pub fn xof_many(cv: &CVWords, block: &[u8; BLOCK_LEN], block_len: u8, mut counter: u64, flags: u8, out: &mut [u8]) -> usize {
    let table = [block.as_ptr(); 8];
    let packed = flags as u64 | (block_len as u64) << 24;
    let mut done = 0;
    while out.len() - done >= 4 * BLOCK_LEN {
        let (kernel, blocks): (Kernel, usize) =
            if out.len() - done >= 8 * BLOCK_LEN { (asm::blake3_servil_hybrid_x8, 8) } else { (asm::blake3_servil_hybrid_x4, 4) };
        crate::ran_kernel(kernel as *const ());
        // Sound: every table entry points at the 64-byte block; the kernel
        // writes 64 bytes a block within `out[done..]`.
        unsafe { kernel(table.as_ptr(), 1, cv.as_ptr(), counter, packed, out[done..].as_mut_ptr()) };
        counter = counter.wrapping_add(blocks as u64);
        done += blocks * BLOCK_LEN;
    }
    done
}

/// One block of extended output, block `counter`, on the integer units
/// (x1). The four-lane x4 costs about two compressions on the M4's cores
/// and the VM's: one block of output through it took 2.1 times the
/// portable compression's time, and through x1 0.97 (VM, BLAKE3 #190).
pub fn xof_block(cv: &CVWords, block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8) -> [u8; BLOCK_LEN] {
    let mut out = [0u8; BLOCK_LEN];
    let table = [block.as_ptr()];
    let packed = flags as u64 | (block_len as u64) << 24;
    crate::ran_kernel(asm::blake3_servil_hybrid_x1 as *const ());
    // Sound: the table's one entry points at the 64-byte block; the kernel
    // writes the block's 64 bytes of output to `out`.
    unsafe { asm::blake3_servil_hybrid_x1(table.as_ptr(), 1, cv.as_ptr(), counter, packed, out.as_mut_ptr()) };
    out
}

/// The last one to three blocks of extended output into `out`, from block
/// `counter`: one or two blocks on x1, three on x4 (its fourth discarded),
/// which takes about the time of two x1 calls.
pub fn xof_tail(cv: &CVWords, block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8, out: &mut [u8]) {
    assert!(out.len() < 4 * BLOCK_LEN && out.len() % BLOCK_LEN == 0, "a tail of one to three blocks");
    if out.len() == 3 * BLOCK_LEN {
        let mut four = [0u8; 4 * BLOCK_LEN];
        xof_many(cv, block, block_len, counter, flags, &mut four);
        out.copy_from_slice(&four[..out.len()]);
        return;
    }
    for (i, out) in out.chunks_exact_mut(BLOCK_LEN).enumerate() {
        out.copy_from_slice(&xof_block(cv, block, block_len, counter.wrapping_add(i as u64), flags));
    }
}

/// Every kernel shares one signature: `(inputs, blocks, key, counter,
/// packed_flags, out)`. See the contract in that file.
type Kernel = unsafe extern "C" fn(*const *const u8, u64, *const u32, u64, u64, *mut u8);

/// Most inputs one `hash_many` call hands to the kernels at once.
const GROUP: usize = 16;

/// Chunk kernel per exact input count.
const CHUNK_KERNELS: [Option<Kernel>; 11] = [
    None,
    Some(asm::blake3_servil_hybrid_k1),
    Some(asm::blake3_servil_hybrid_k2),
    Some(asm::blake3_servil_hybrid_k3),
    Some(asm::blake3_servil_hybrid_k4),
    Some(asm::blake3_servil_hybrid_k5),
    Some(asm::blake3_servil_hybrid_k6),
    Some(asm::blake3_servil_hybrid_k7),
    Some(asm::blake3_servil_hybrid_k8),
    Some(asm::blake3_servil_hybrid_k9),
    Some(asm::blake3_servil_hybrid_k10),
];

/// Kernel sizes per chunk count 1..=16, largest first. Each count's plan is
/// the one with the smallest worst ratio to the best plan across a P-core,
/// an E-core and the VM, except 4 and 8 chunks, where Zooko chose P-core
/// speed (k4, k8, with two scalar lanes). Ten whole chunks keep one k10 call
/// and the partial chunk after it (every split cost more); one whole chunk
/// with a partial one uses q1 from five partial blocks (below that P-cores
/// lost up to 14%).
const CHUNK_PLANS: [&[usize]; 17] =
    [&[], &[1], &[2], &[3], &[4], &[5], &[6], &[7], &[8], &[9], &[10], &[8, 3], &[9, 3], &[10, 3], &[10, 4], &[10, 5], &[10, 6]];

/// Parent kernel per exact input count.
const PARENT_KERNELS: [Option<Kernel>; 10] = [
    None,
    Some(asm::blake3_servil_hybrid_k1),
    Some(asm::blake3_servil_hybrid_p2),
    Some(asm::blake3_servil_hybrid_p3),
    Some(asm::blake3_servil_hybrid_p4),
    Some(asm::blake3_servil_hybrid_p5),
    None,
    Some(asm::blake3_servil_hybrid_p7),
    Some(asm::blake3_servil_hybrid_p8),
    Some(asm::blake3_servil_hybrid_p9),
];

/// Kernel sizes per parent count 1..=16. p3, p5, p7, and p9 hash one
/// block on the integer units beside the NEON ones, as k3, k5, k7, and k9
/// do for chunks. Against the binary decomposition over p2, p4, p8, and k1
/// run one after another, cycles per call on an M4 Max P-core / E-core
/// (probe/mixed-parents, jobs 121-122): 3 messages -37% / -38%, 5 -27 /
/// -27, 6 (p3 + p3) -25 / -11, 7 -31 / -34, 9 -18 / -22, 10 (p7 + p3;
/// p9 + k1 -10 / +1, p5 + p5 -8 / -2) -11 / -8, 11 -13 / -18, 12 -15 / -9,
/// 13 -12 / -15, 14 -17 / -12, 15 -18 / -19; on the VM 6-39% faster. 2,
/// 4, 8, and 16 keep their kernels; 13-16 were level with the other
/// plans tried on the VM. Two scalar lanes stay out: E-cores pay for a
/// second one what P-cores save. The exception is p4 itself (Zooko,
/// September 27, 2026): hash_many is built for top speed, P-cores do most
/// of this machine's work, and four messages were the last batch where
/// BLAKE3 official led (P -12%, E +30% in cycles per call).
const PARENT_PLANS: [&[usize]; 17] =
    [&[], &[1], &[2], &[3], &[4], &[5], &[3, 3], &[7], &[8], &[9], &[7, 3], &[9, 2], &[9, 3], &[9, 4], &[9, 5], &[8, 7], &[8, 8]];

/// The kernels without SHA-3 per exact input count: scalars and quads
/// alone (k1, k9, k10, p8, and p9 among them).
const PLAIN_CHUNK_KERNELS: [Option<Kernel>; 11] = [
    None,
    Some(asm::blake3_servil_hybrid_k1),
    Some(asm::blake3_servil_hybrid_kn2),
    None,
    Some(asm::blake3_servil_hybrid_kn4),
    Some(asm::blake3_servil_hybrid_kn5),
    Some(asm::blake3_servil_hybrid_kn6),
    None,
    Some(asm::blake3_servil_hybrid_kn8),
    Some(asm::blake3_servil_hybrid_k9),
    Some(asm::blake3_servil_hybrid_k10),
];

const PLAIN_PARENT_KERNELS: [Option<Kernel>; 11] = [
    None,
    Some(asm::blake3_servil_hybrid_k1),
    Some(asm::blake3_servil_hybrid_pn2),
    None,
    Some(asm::blake3_servil_hybrid_pn4),
    Some(asm::blake3_servil_hybrid_pn5),
    Some(asm::blake3_servil_hybrid_pn6),
    None,
    Some(asm::blake3_servil_hybrid_p8),
    Some(asm::blake3_servil_hybrid_p9),
    Some(asm::blake3_servil_hybrid_pn10),
];

/// Kernel sizes per count 1..=16 without SHA-3, chunks and parents alike:
/// 3 and 7 run a kernel and k1 after it.
const PLAIN_PLANS: [&[usize]; 17] =
    [&[], &[1], &[2], &[2, 1], &[4], &[5], &[6], &[6, 1], &[8], &[9], &[10], &[10, 1], &[10, 2], &[9, 4], &[10, 4], &[10, 5], &[10, 6]];

/// A set of kernels and the plans that compose them, for chunks and for
/// parents: one with SHA-3, one without.
struct Family {
    chunk_plans: &'static [&'static [usize]; 17],
    chunk_kernels: &'static [Option<Kernel>],
    parent_plans: &'static [&'static [usize]; 17],
    parent_kernels: &'static [Option<Kernel>],
}

static WITH_SHA3: Family = Family { chunk_plans: &CHUNK_PLANS, chunk_kernels: &CHUNK_KERNELS, parent_plans: &PARENT_PLANS, parent_kernels: &PARENT_KERNELS };
static PLAIN: Family =
    Family { chunk_plans: &PLAIN_PLANS, chunk_kernels: &PLAIN_CHUNK_KERNELS, parent_plans: &PLAIN_PLANS, parent_kernels: &PLAIN_PARENT_KERNELS };

/// This CPU's family.
fn family() -> &'static Family {
    if sha3_detected() { &WITH_SHA3 } else { &PLAIN }
}

#[cfg(feature = "std")]
/// Where each kernel's code ends (labels the generator puts after each).
mod ends {
    unsafe extern "C" {
        pub static blake3_servil_hybrid_p2_end: u8;
        pub static blake3_servil_hybrid_p3_end: u8;
        pub static blake3_servil_hybrid_p4_end: u8;
        pub static blake3_servil_hybrid_p5_end: u8;
        pub static blake3_servil_hybrid_p7_end: u8;
        pub static blake3_servil_hybrid_p8_end: u8;
        pub static blake3_servil_hybrid_p9_end: u8;
        pub static blake3_servil_hybrid_k1_end: u8;
        pub static blake3_servil_hybrid_k2_end: u8;
        pub static blake3_servil_hybrid_k3_end: u8;
        pub static blake3_servil_hybrid_k4_end: u8;
        pub static blake3_servil_hybrid_k5_end: u8;
        pub static blake3_servil_hybrid_k6_end: u8;
        pub static blake3_servil_hybrid_k7_end: u8;
        pub static blake3_servil_hybrid_k8_end: u8;
        pub static blake3_servil_hybrid_k9_end: u8;
        pub static blake3_servil_hybrid_k10_end: u8;
        pub static blake3_servil_hybrid_q1_end: u8;
        pub static blake3_servil_hybrid_q2_end: u8;
        pub static blake3_servil_hybrid_q3_end: u8;
        pub static blake3_servil_hybrid_q4_end: u8;
        pub static blake3_servil_hybrid_q5_end: u8;
        pub static blake3_servil_hybrid_q6_end: u8;
        pub static blake3_servil_hybrid_q7_end: u8;
        pub static blake3_servil_hybrid_q8_end: u8;
        pub static blake3_servil_hybrid_q9_end: u8;
    }
}

#[cfg(feature = "std")]
/// The code of the chunk kernel for `n` whole chunks (1 to 10), and of
/// the q kernel for `n` whole chunks and a partial one (1 to 9): start
/// and end addresses.
fn chunk_code(n: usize, partial: bool) -> (usize, usize) {
    macro_rules! code {
        ($kernel:ident, $end:ident) => {
            (asm::$kernel as *const () as usize, &raw const ends::$end as usize)
        };
    }
    match (n, partial) {
        (1, false) => code!(blake3_servil_hybrid_k1, blake3_servil_hybrid_k1_end),
        (2, false) => code!(blake3_servil_hybrid_k2, blake3_servil_hybrid_k2_end),
        (3, false) => code!(blake3_servil_hybrid_k3, blake3_servil_hybrid_k3_end),
        (4, false) => code!(blake3_servil_hybrid_k4, blake3_servil_hybrid_k4_end),
        (5, false) => code!(blake3_servil_hybrid_k5, blake3_servil_hybrid_k5_end),
        (6, false) => code!(blake3_servil_hybrid_k6, blake3_servil_hybrid_k6_end),
        (7, false) => code!(blake3_servil_hybrid_k7, blake3_servil_hybrid_k7_end),
        (8, false) => code!(blake3_servil_hybrid_k8, blake3_servil_hybrid_k8_end),
        (9, false) => code!(blake3_servil_hybrid_k9, blake3_servil_hybrid_k9_end),
        (10, false) => code!(blake3_servil_hybrid_k10, blake3_servil_hybrid_k10_end),
        (1, true) => code!(blake3_servil_hybrid_q1, blake3_servil_hybrid_q1_end),
        (2, true) => code!(blake3_servil_hybrid_q2, blake3_servil_hybrid_q2_end),
        (3, true) => code!(blake3_servil_hybrid_q3, blake3_servil_hybrid_q3_end),
        (4, true) => code!(blake3_servil_hybrid_q4, blake3_servil_hybrid_q4_end),
        (5, true) => code!(blake3_servil_hybrid_q5, blake3_servil_hybrid_q5_end),
        (6, true) => code!(blake3_servil_hybrid_q6, blake3_servil_hybrid_q6_end),
        (7, true) => code!(blake3_servil_hybrid_q7, blake3_servil_hybrid_q7_end),
        (8, true) => code!(blake3_servil_hybrid_q8, blake3_servil_hybrid_q8_end),
        (9, true) => code!(blake3_servil_hybrid_q9, blake3_servil_hybrid_q9_end),
        _ => unreachable!("no kernel for {n} whole chunks (partial: {partial})"),
    }
}

#[cfg(feature = "std")]
/// The code of the parent kernel for `n` parents (PARENT_KERNELS).
fn parent_code(n: usize) -> (usize, usize) {
    macro_rules! code {
        ($kernel:ident, $end:ident) => {
            (asm::$kernel as *const () as usize, &raw const ends::$end as usize)
        };
    }
    match n {
        1 => code!(blake3_servil_hybrid_k1, blake3_servil_hybrid_k1_end),
        2 => code!(blake3_servil_hybrid_p2, blake3_servil_hybrid_p2_end),
        3 => code!(blake3_servil_hybrid_p3, blake3_servil_hybrid_p3_end),
        4 => code!(blake3_servil_hybrid_p4, blake3_servil_hybrid_p4_end),
        5 => code!(blake3_servil_hybrid_p5, blake3_servil_hybrid_p5_end),
        7 => code!(blake3_servil_hybrid_p7, blake3_servil_hybrid_p7_end),
        8 => code!(blake3_servil_hybrid_p8, blake3_servil_hybrid_p8_end),
        9 => code!(blake3_servil_hybrid_p9, blake3_servil_hybrid_p9_end),
        _ => unreachable!("no parent kernel for {n} parents"),
    }
}

#[cfg(feature = "std")]
/// Prefetch the code a tree of `whole` chunks and then `partial` bytes
/// (under a chunk) runs on the hybrids: the chunk kernels
/// (`compress_chunks_parallel` and [`hash_chunks_with_partial`] choose
/// them from the same plans), then [`prefetch_parent_code`]. Requires 1 to
/// 16 whole chunks.
pub fn prefetch_tree_code(whole: usize, partial: usize) {
    if partial > 0 && partial_pays(whole, partial) {
        let lead = whole.saturating_sub(9);
        for &n in CHUNK_PLANS[lead] {
            crate::platform::prefetch_code(chunk_code(n, false));
        }
        crate::platform::prefetch_code(chunk_code(whole - lead, true));
    } else {
        for &n in CHUNK_PLANS[whole] {
            crate::platform::prefetch_code(chunk_code(n, false));
        }
    }
    prefetch_parent_code(whole + usize::from(partial > 0));
}

#[cfg(feature = "std")]
/// Prefetch the code a batch of `count` one-block messages runs on the
/// hybrids: the parent plans, a group of sixteen at a time (hash_many on
/// one-block inputs). Requires 1 or more.
pub fn prefetch_message_code(count: usize) {
    let plan = |n: usize| PARENT_PLANS[n];
    for &n in plan(count.min(16)) {
        crate::platform::prefetch_code(parent_code(n));
    }
    if count > 16 && count % 16 > 0 {
        for &n in plan(count % 16) {
            crate::platform::prefetch_code(parent_code(n));
        }
    }
}

#[cfg(feature = "std")]
/// Prefetch the code that merges `values` chaining values to the root:
/// each level's parent plan (`compress_parents_parallel`), then the scalar
/// kernel that compresses the root. Requires 2 to 32 values.
pub fn prefetch_parent_code(mut values: usize) {
    while values > 2 {
        for &n in PARENT_PLANS[values / 2] {
            crate::platform::prefetch_code(parent_code(n));
        }
        values = values / 2 + values % 2;
    }
    crate::platform::prefetch_code(parent_code(1));
}

/// True when the CPU has the SHA-3 extension that provides `xar`. Every
/// Apple M-series core and every Armv8.2+ core with `FEAT_SHA3` does. The
/// pair kernels rotate with `xar`; the scalar and quad kernels need no
/// extension.
pub fn sha3_detected() -> bool {
    #[cfg(feature = "std")]
    {
        std::arch::is_aarch64_feature_detected!("sha3")
    }
    #[cfg(not(feature = "std"))]
    {
        cfg!(target_feature = "sha3")
    }
}

/// Compress `count` whole 64-byte blocks of one chunk into `cv`, all with
/// the same counter: `flags | flags_start` on the first block, `flags` on
/// the rest. This is `ChunkState::update`'s inner loop as one kernel call;
/// the scalar kernel keeps the whole state in registers and runs at 13.3
/// cycles per G-step against the portable compressor's 14.
///
/// The kernel uses only integer instructions. Unsafe because `blocks` must
/// hold `count * 64` readable bytes and `count` must be in 1..=16.
pub unsafe fn compress_blocks(cv: &mut CVWords, blocks: *const u8, count: usize, counter: u64, flags: u8, flags_start: u8) {
    debug_assert!((1..=16).contains(&count));
    let packed = flags as u64 | (flags_start as u64) << 8 | (BLOCK_LEN as u64) << 24;
    // The kernel reads the whole key in its prologue and stores the output
    // at its end, so `cv` serves as both: the output words land in place,
    // in the little-endian layout CVWords has on this target.
    let cv_ptr = cv.as_mut_ptr();
    unsafe {
        let last = blocks.add((count - 1) * BLOCK_LEN);
        crate::ran_kernel(asm::blake3_servil_hybrid_c1 as *const ());
        asm::blake3_servil_hybrid_c1(blocks, count as u64, cv_ptr, counter, packed, cv_ptr as *mut u8, last);
    }
}

/// One compression of `block` into `cv`, with every flag in `flags`
/// (start, end, root, and so on are the caller's business) and the given
/// `block_len` recorded in the state. This is `Platform::compress_in_place`
/// on the scalar kernel: the chunk finalizer, the root compression, and
/// every parent node of the incremental `Hasher` go through it. Registers
/// hold the whole state, so it saves the portable compressor's spills.
///
/// The kernel uses only integer instructions and runs on every AArch64
/// core. `block_len` is at most 64; the block is 64 bytes, zero-padded
/// past `block_len` by the caller as the portable compressor requires.
pub fn compress_in_place(cv: &mut CVWords, block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8) {
    debug_assert!(block_len as usize <= BLOCK_LEN);
    // The one block is both first and last: the kernel ORs flags_start
    // into the first block and flags_end into the last, so the flags go
    // in as the base and the two per-position fields stay zero.
    let packed = flags as u64 | (block_len as u64) << 24;
    // `cv` is key and output at once; see compress_blocks.
    let cv_ptr = cv.as_mut_ptr();
    // Safe: the block is 64 readable bytes and the count is 1.
    unsafe {
        let block = block.as_ptr();
        crate::ran_kernel(asm::blake3_servil_hybrid_c1 as *const ());
        asm::blake3_servil_hybrid_c1(block, 1, cv_ptr, counter, packed, cv_ptr as *mut u8, block);
    }
}

/// Hash a whole chunk of `1..=16` blocks in one kernel call and write its
/// 32-byte chaining value (or root hash) to `out`. Blocks `0..blocks - 1`
/// are the first `(blocks - 1) * 64` bytes of `input`; the last block is
/// `last`, zero-padded past `last_len`, so a short final block costs one
/// 64-byte copy. The first block carries `flags | flags_start`, the last
/// `flags | flags_end`, and every block `flags` and `counter`. This is the
/// one-chunk `hash()`: CHUNK_START on the first block, CHUNK_END | ROOT on
/// the last, one kernel call, no chaining value travelling through memory
/// between calls.
///
/// Unsafe because `input` must hold `(blocks - 1) * 64` readable bytes and
/// `blocks` must be in 1..=16.
pub unsafe fn hash_chunk(
    input: *const u8,
    blocks: usize,
    last: &[u8; BLOCK_LEN],
    last_len: u8,
    key: &CVWords,
    counter: u64,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    out: &mut [u8; OUT_LEN],
) {
    debug_assert!((1..=16).contains(&blocks));
    debug_assert!(last_len as usize <= BLOCK_LEN);
    let packed = flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | (last_len as u64) << 24;
    unsafe {
        crate::ran_kernel(asm::blake3_servil_hybrid_c1 as *const ());
        asm::blake3_servil_hybrid_c1(input, blocks as u64, key.as_ptr(), counter, packed, out.as_mut_ptr(), last.as_ptr());
    }
}

/// A kernel for whole chunks plus a partial chunk: the arguments of
/// [`Kernel`] and the partial chunk's `blocks | last_len << 8`.
type PartialKernel = unsafe extern "C" fn(*const *const u8, u64, *const u32, u64, u64, *mut u8, u64);

/// Every assembly entry point of the hybrids, named: from the kernel
/// tables, the q kernels, and the integer kernel (for the self-test's
/// test).
#[cfg(all(test, feature = "std"))]
pub(crate) fn entries() -> std::vec::Vec<(std::string::String, *const ())> {
    let mut out = std::vec::Vec::new();
    let family = family();
    for (size, k) in family.chunk_kernels.iter().enumerate() {
        if let Some(k) = k {
            out.push((std::format!("k{size}"), *k as *const ()));
        }
    }
    for (size, k) in family.parent_kernels.iter().enumerate() {
        if let Some(k) = k {
            out.push((std::format!("p{size}"), *k as *const ()));
        }
    }
    if sha3_detected() {
        for n in 1..=9 {
            out.push((std::format!("q{n}"), partial_kernel(n) as *const ()));
        }
    }
    out.push(("c1".into(), asm::blake3_servil_hybrid_c1 as *const ()));
    out.push(("x1".into(), asm::blake3_servil_hybrid_x1 as *const ()));
    out.push(("x4".into(), asm::blake3_servil_hybrid_x4 as *const ()));
    out.push(("x8".into(), asm::blake3_servil_hybrid_x8 as *const ()));
    out
}

/// The q kernel for `n` whole chunks plus a partial chunk, `n` in 1..=9.
fn partial_kernel(n: usize) -> PartialKernel {
    match n {
        1 => asm::blake3_servil_hybrid_q1,
        2 => asm::blake3_servil_hybrid_q2,
        3 => asm::blake3_servil_hybrid_q3,
        4 => asm::blake3_servil_hybrid_q4,
        5 => asm::blake3_servil_hybrid_q5,
        6 => asm::blake3_servil_hybrid_q6,
        7 => asm::blake3_servil_hybrid_q7,
        8 => asm::blake3_servil_hybrid_q8,
        9 => asm::blake3_servil_hybrid_q9,
        _ => panic!("no q kernel for {n} whole chunks"),
    }
}

/// Whether [`hash_chunks_with_partial`] takes `n` whole chunks: 1 through
/// 15 but 10 (from 16 the SME2 kernels or a second call take the whole
/// chunks).
fn partial_covers(n: usize) -> bool {
    (1..GROUP).contains(&n) && n != 10
}

/// Whether `n` whole chunks and a partial chunk of `len` bytes hash
/// faster through [`hash_chunks_with_partial`] than the whole chunks and
/// then the partial one. Ten whole chunks are one k10 call, the kernel with
/// the fewest cycles per byte; every split into a lead plan and a q kernel
/// costs more than the partial chunk after it (VM, 10 KiB + 500 B: 3097 ns
/// against 2790), where 11 to 15 gain 1-11%. One whole chunk moves from the
/// scalar kernel to q1's NEON pair, whose time does not grow with the
/// partial chunk: from five blocks on, the M4 Max's P-cores are level or
/// faster and its E-cores faster (1300 B: P level, E -14%; 2000 B: P -32%,
/// E -36%); at two blocks (1100 B) P-cores paid 14%.
pub fn partial_pays(n: usize, len: usize) -> bool {
    partial_covers(n) && (n > 1 || len > 4 * BLOCK_LEN)
}

/// The chaining values of `chunks` (whole, at `counter` on) and of
/// `partial` (1 to 1023 bytes, the chunk after them) into `out`: the last
/// up to nine whole chunks and the partial chunk in one q kernel call, the
/// partial chunk on the scalar units beside them; any whole chunks before
/// those through the usual plans. Requires 1 to 15 whole chunks but 10,
/// room in `out` for `chunks.len() + 1` values, and the SHA-3 extension.
pub unsafe fn hash_chunks_with_partial(
    chunks: &[&[u8; CHUNK_LEN]],
    partial: &[u8],
    key: &CVWords,
    counter: u64,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    out: &mut [u8],
) {
    assert!(partial_covers(chunks.len()), "1 to 15 whole chunks but 10");
    assert!(!partial.is_empty() && partial.len() < CHUNK_LEN, "a partial chunk holds 1 to 1023 bytes");
    assert!(out.len() >= (chunks.len() + 1) * OUT_LEN);
    // Ten whole chunks and more: the first ones through the usual plans,
    // so the q kernel takes the last nine or fewer.
    let lead = chunks.len().saturating_sub(9);
    if lead > 0 {
        let packed = flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | (BLOCK_LEN as u64) << 24;
        unsafe {
            run_plan(CHUNK_PLANS[lead], &CHUNK_KERNELS, chunks.as_ptr() as *const *const u8, CHUNK_LEN / BLOCK_LEN, key, counter, 1, packed, out);
        }
    }
    let (chunks, counter, out) = (&chunks[lead..], counter + lead as u64, &mut out[lead * OUT_LEN..]);
    let n = chunks.len();
    let kernel = partial_kernel(n);
    crate::ran_kernel(kernel as *const ());
    if n == 1 {
        // q1's pair takes the whole chunk twice (slots 0 and 2); the
        // partial chunk is slot 1; the duplicate's value is dropped.
        let blocks = partial.len().div_ceil(BLOCK_LEN);
        let last_len = partial.len() - (blocks - 1) * BLOCK_LEN;
        let mut padded = [0u8; CHUNK_LEN];
        padded[..partial.len()].copy_from_slice(partial);
        let table = [chunks[0].as_ptr(), padded.as_ptr(), chunks[0].as_ptr()];
        let mut cvs = [0u8; 3 * OUT_LEN];
        let packed = flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | (BLOCK_LEN as u64) << 24;
        unsafe {
            kernel(table.as_ptr(), (CHUNK_LEN / BLOCK_LEN) as u64, key.as_ptr(), counter, packed, cvs.as_mut_ptr(), blocks as u64 | (last_len as u64) << 8);
        }
        out[..2 * OUT_LEN].copy_from_slice(&cvs[..2 * OUT_LEN]);
        return;
    }
    let blocks = partial.len().div_ceil(BLOCK_LEN);
    let last_len = partial.len() - (blocks - 1) * BLOCK_LEN;
    // The partial chunk as whole blocks: its bytes, then zeros to the end
    // of its last block.
    let mut padded = [0u8; CHUNK_LEN];
    padded[..partial.len()].copy_from_slice(partial);
    let mut table = [core::ptr::null::<u8>(); 10];
    for (slot, chunk) in table.iter_mut().zip(chunks) {
        *slot = chunk.as_ptr();
    }
    table[n] = padded.as_ptr();
    let packed = flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | (BLOCK_LEN as u64) << 24;
    unsafe {
        kernel(table.as_ptr(), (CHUNK_LEN / BLOCK_LEN) as u64, key.as_ptr(), counter, packed, out.as_mut_ptr(), blocks as u64 | (last_len as u64) << 8);
    }
}

#[cfg(feature = "std")]
/// Separate one-block messages, two at a time on the pair kernel alone
/// (2.9 KB of code, where the plans for 3 to 16 run up to 17 KB: after a
/// pause the code comes from memory), the last of an odd count paired with
/// itself; each message's last block `last_len` bytes (1 to 64, zero past
/// it). Unsafe because the CPU must have NEON and the SHA-3 extension.
pub unsafe fn hash_pairs(inputs: &[&[u8; BLOCK_LEN]], key: &CVWords, flags: u8, flags_start: u8, flags_end: u8, last_len: usize, out: &mut [u8]) {
    assert!(out.len() >= inputs.len() * OUT_LEN && (1..=BLOCK_LEN).contains(&last_len), "a digest per input, a last block of 1 to 64 bytes");
    let packed = flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | (last_len as u64) << 24;
    let pair = PARENT_KERNELS[2].expect("the pair kernel");
    crate::ran_kernel(pair as *const ());
    let table = inputs.as_ptr() as *const *const u8;
    let whole = inputs.len() / 2 * 2;
    for p in (0..whole).step_by(2) {
        unsafe { pair(table.add(p), 1, key.as_ptr(), 0, packed, out[p * OUT_LEN..].as_mut_ptr()) };
    }
    if whole < inputs.len() {
        let last = [inputs[whole].as_ptr(), inputs[whole].as_ptr()];
        let mut two = [0u8; 2 * OUT_LEN];
        unsafe { pair(last.as_ptr(), 1, key.as_ptr(), 0, packed, two.as_mut_ptr()) };
        out[whole * OUT_LEN..(whole + 1) * OUT_LEN].copy_from_slice(&two[..OUT_LEN]);
    }
}

#[cfg(feature = "std")]
/// Prefetch the pair kernel's code ([`hash_pairs`]).
pub fn prefetch_pair_code() {
    crate::platform::prefetch_code(parent_code(2));
}

/// Run the kernels of `plan` over `inputs`.
unsafe fn run_plan(
    plan: &[usize],
    kernels: &[Option<Kernel>],
    inputs: *const *const u8,
    blocks: usize,
    key: &CVWords,
    counter: u64,
    counter_step: u64,
    packed_flags: u64,
    out: &mut [u8],
) {
    let mut done = 0;
    for &size in plan {
        let kernel = kernels[size].expect("plan names an existing kernel");
        crate::ran_kernel(kernel as *const ());
        unsafe {
            kernel(inputs.add(done), blocks as u64, key.as_ptr(), counter + done as u64 * counter_step, packed_flags, out[done * OUT_LEN..].as_mut_ptr());
        }
        done += size;
    }
}

/// `hash_many` for whole chunks (`N == CHUNK_LEN`, counter incrementing)
/// on the chunk plans, and for inputs of 1 to 16 whole blocks at one
/// counter (parents, separate messages) on the parent plans: the shapes
/// `Platform::hash_many` takes. Another N cannot compile, and incrementing
/// counters on less than a chunk stop.
///
/// Unsafe because the CPU must have NEON.
pub unsafe fn hash_many<const N: usize>(
    inputs: &[&[u8; N]],
    key: &CVWords,
    counter: u64,
    increment_counter: IncrementCounter,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    out: &mut [u8],
) {
    unsafe { hash_many_last_len(inputs, key, counter, increment_counter, flags, flags_start, flags_end, BLOCK_LEN, out) }
}

/// [`hash_many`] with each input's last block `last_len` bytes long (1 to
/// 64; its bytes past that zero): separate messages in slots of whole
/// blocks. Unsafe because the CPU must have NEON.
#[allow(clippy::too_many_arguments)]
pub unsafe fn hash_many_last_len<const N: usize>(
    inputs: &[&[u8; N]],
    key: &CVWords,
    counter: u64,
    increment_counter: IncrementCounter,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    last_len: usize,
    out: &mut [u8],
) {
    unsafe { hash_many_in(family(), inputs, key, counter, increment_counter, flags, flags_start, flags_end, last_len, out) }
}

/// [`hash_many_last_len`] on the kernels of `family`.
#[allow(clippy::too_many_arguments)]
unsafe fn hash_many_in<const N: usize>(
    family: &Family,
    inputs: &[&[u8; N]],
    key: &CVWords,
    counter: u64,
    increment_counter: IncrementCounter,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    last_len: usize,
    out: &mut [u8],
) {
    assert!(out.len() >= inputs.len() * OUT_LEN);
    assert!((1..=BLOCK_LEN).contains(&last_len), "a last block of 1 to 64 bytes");
    const { assert!(N > 0 && N % BLOCK_LEN == 0 && N <= CHUNK_LEN, "1 to 16 whole blocks") };
    let (plans, kernels, counter_step): (&[&[usize]; 17], &[Option<Kernel>], u64) = if increment_counter.yes() {
        assert_eq!(N, CHUNK_LEN, "incrementing counters hash whole chunks");
        (family.chunk_plans, family.chunk_kernels, 1)
    } else {
        (family.parent_plans, family.parent_kernels, 0)
    };
    let packed = flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | (last_len as u64) << 24;
    let blocks = N / BLOCK_LEN;
    // `&[&[u8; N]]` is a table of pointers, which is what the kernels take.
    let table = inputs.as_ptr() as *const *const u8;

    let mut done = 0;
    while done < inputs.len() {
        let count = core::cmp::min(GROUP, inputs.len() - done);
        unsafe {
            run_plan(
                plans[count],
                kernels,
                table.add(done),
                blocks,
                key,
                counter + done as u64 * counter_step,
                counter_step,
                packed,
                &mut out[done * OUT_LEN..],
            );
        }
        done += count;
    }
}

/// Separate inputs of `blocks` whole blocks (1 to 16) at one `counter`, the
/// last block `last_len` bytes (1 to 64, zero past it), from a table of
/// `count` pointers: the parent plans, as [`hash_many_last_len`] runs them
/// for inputs of a const length, `flags` on every block. Unsafe because the CPU must have NEON and
/// every pointer must reach `blocks` blocks.
#[allow(clippy::too_many_arguments)]
pub unsafe fn hash_messages_raw(
    table: *const *const u8,
    count: usize,
    blocks: usize,
    key: &CVWords,
    counter: u64,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    last_len: usize,
    out: &mut [u8],
) {
    assert!(out.len() >= count * OUT_LEN, "room for every digest");
    assert!((1..=16).contains(&blocks) && (1..=BLOCK_LEN).contains(&last_len), "1 to 16 blocks, the last of 1 to 64 bytes");
    let packed = flags as u64 | (flags_start as u64) << 8 | (flags_end as u64) << 16 | (last_len as u64) << 24;
    let mut done = 0;
    while done < count {
        let n = core::cmp::min(GROUP, count - done);
        let family = family();
        unsafe { run_plan(family.parent_plans[n], family.parent_kernels, table.add(done), blocks, key, counter, 0, packed, &mut out[done * OUT_LEN..]) };
        done += n;
    }
}

#[cfg(all(test, feature = "std"))]
mod test {
    use super::*;

    #[test]
    fn test_kernel_code_ranges() {
        // Each kernel's code runs from its symbol to its end label: present,
        // in order, and under a kernel's size (the largest, q6, is 20 KB).
        for partial in [false, true] {
            for n in 1..=if partial { 9 } else { 10 } {
                let (start, end) = chunk_code(n, partial);
                assert!(start < end && end - start < 32 * 1024, "kernel {n} (partial {partial}): {start:#x}..{end:#x}");
            }
        }
        for n in [1, 2, 3, 4, 5, 7, 8, 9] {
            let (start, end) = parent_code(n);
            assert!(start < end && end - start < 32 * 1024, "parent kernel {n}: {start:#x}..{end:#x}");
        }
        // The plans' mirror takes every shape compress_chunks_parallel does.
        for whole in 1..=16 {
            for partial in [0, 1, 63, 64, 65, 500, 1023] {
                prefetch_tree_code(whole, if whole == 16 { 0 } else { partial });
            }
        }
    }

    /// The groups of four against fixed values (crate::test::xof_groups_sweep).
    #[test]
    fn test_xof_many_groups() {
        crate::test::xof_groups_sweep(&mut |cv, block, block_len, counter, flags, out| xof_many(cv, block, block_len, counter, flags, out));
    }
}

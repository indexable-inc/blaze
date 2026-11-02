//! The startup self-test: before a process first hashes, one pass over
//! inputs chosen so that each goes through a different kernel and code
//! path on this machine, every result compared with the reference
//! implementation's (checked in below, verified by a unit test). A
//! miscompiled kernel, a faulty vector or matrix unit, or a broken build
//! then stops the program with a message naming the path, instead of
//! producing wrong digests. Its purpose is the build and the machine
//! (a miscompilation, a faulty unit, a binary on a CPU it was not built
//! for, tampering that breaks the digests); what the code computes is the
//! tests' and the proofs' to establish. So each kernel runs once, on a
//! small input. On AArch64 it takes 0.1 to 0.2 ms, once (Apple M4 Max:
//! 93 µs warm, 140-200 µs in a fresh process; the VM, October 25:
//! 160-280 µs): most of it is running every assembly kernel, 500 KB of
//! unrolled code, whose
//! first fetch dominates the fresh-process cost (Zooko chose this budget
//! over a smaller one that leaves kernels out, September 26, 2026).
//! Elsewhere nine cases reach every x86 kernel: about 28 µs in a fresh
//! process on an i7-12700K's performance core, 16 µs warm (47 on an
//! efficiency core), from 109 and 90 (210) with the AArch64 list.
//!
//! As Niels Ferguson suggested, the cases chain: each writes the first 32
//! bytes of its output over the start of the next case's input (and keys
//! the keyed case), so a fault anywhere changes every value after it; and
//! they use unusual alignments, lengths, and padding, where platform,
//! compiler, and hardware faults hide from ordinary tests. The first case
//! that differs names the failing path: the cases before it agreed.
//!
//! `initialize()`, which every program calls before any other call, runs
//! it through `ensure`, in every build (std or not); other threads calling
//! it wait, spinning (once per process, for 0.1 to 0.2 ms). The cases call
//! the entry points' bodies from the undetected platform
//! (`hash_serial_from` and its like), just past Platform::detect(), which
//! panics until `initialize` has recorded the platform. It allocates nothing: its buffers (about 44 KiB) live on the
//! calling thread's stack while it runs.

use crate::platform::Platform;

/// One case: an input shape and the entry point that hashes it. Every
/// case reads its input from the start of the shared buffer, `offset`
/// bytes in where it has one.
#[derive(Clone, Copy)]
pub(crate) enum Case {
    /// hash() of `len` bytes starting `offset` bytes into the input.
    Hash { offset: usize, len: usize },
    /// hash_many() of `count` messages of `len` bytes, in slots of whole
    /// blocks, zero between messages, the batch's base `offset` bytes past
    /// an allocation's start.
    Many { offset: usize, len: usize, count: usize },
    /// The chaining values of `chunks` whole chunks at counter 1, keyed by
    /// the previous case's output: on SME2, one call of the kernel with
    /// integer lanes beside the matrix unit (18 chunks a group), which
    /// hash() reaches only from 256 KiB; elsewhere the platform's chunk
    /// kernels, with the same values. Only the AArch64 list has one.
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    ChunkValues { chunks: usize },
    /// keyed_hash() of `len` bytes, keyed by the previous case's output.
    Keyed { len: usize },
    /// derive_key() over `len` bytes.
    Derive { len: usize },
    /// A Hasher fed `len` bytes in pieces of `piece` bytes.
    // AArch64's cases alone feed a Hasher in pieces; elsewhere the
    // extended-output case runs the Hasher.
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    Incremental { len: usize, piece: usize },
    /// `out` bytes of extended output from hashing `len` bytes, read
    /// from output position `skip`.
    Xof { len: usize, skip: u64, out: usize },
}

use Case::*;

const KIB: usize = 1024;

/// The cases, each with the path it is there for, assembly first (on
/// Apple M4 and later; other machines take their own paths through the
/// same entry points). Every assembly entry of the AArch64 kernels runs:
/// the hybrids' k1-k10, q1-q9, p2-p9, c1, x1, x4 and x8, and SME2's chunk, message,
/// parent, counter-per-group, and integer-lane kernels
/// (test_self_test_runs_every_kernel checks it). The hybrids' k kernels hash whole chunks at
/// rising counters, so only whole inputs reach them; batches of one-chunk
/// messages, all at counter 0, take the parent plans (p).
#[cfg(target_arch = "aarch64")]
pub(crate) const CASES: &[(Case, &str)] = &[
    (Hash { offset: 0, len: 0 }, "the empty input"),
    (Hash { offset: 1, len: 63 }, "one partial block, unaligned"),
    (Hash { offset: 3, len: 65 }, "a block and a byte, unaligned"),
    (Hash { offset: 5, len: KIB }, "one whole chunk, unaligned"),
    (Hash { offset: 0, len: KIB + 300 }, "a chunk and a partial one: q1"),
    (Hash { offset: 7, len: 2 * KIB }, "two chunks, unaligned: k2"),
    (Derive { len: 3 * KIB - 1 }, "key derivation, two chunks and a partial one: q2"),
    (Hash { offset: 0, len: 3 * KIB + 100 }, "three chunks and a partial one: q3"),
    (Hash { offset: 1, len: 4 * KIB + 65 }, "four chunks and a partial one, unaligned: q4"),
    (Hash { offset: 0, len: 5 * KIB + 1000 }, "five chunks and a partial one: q5"),
    (Hash { offset: 3, len: 6 * KIB + 129 }, "six chunks and a partial one, unaligned: q6"),
    (Hash { offset: 0, len: 8 * KIB - 1 }, "seven chunks and a partial one: q7"),
    (Hash { offset: 5, len: 8 * KIB + 200 }, "eight chunks and a partial one, unaligned: q8"),
    (Hash { offset: 0, len: 10 * KIB }, "ten chunks: k10"),
    (Hash { offset: 1, len: 16 * KIB - 1 }, "fifteen chunks and a partial one, unaligned: k6 and q9"),
    (Hash { offset: 2, len: 3 * KIB }, "three chunks, unaligned: k3 (on SME2 also the remainder below)"),
    (Hash { offset: 0, len: 4 * KIB }, "four chunks: k4"),
    (Hash { offset: 3, len: 5 * KIB }, "five chunks, unaligned: k5"),
    (Hash { offset: 0, len: 7 * KIB }, "seven chunks: k7"),
    (Hash { offset: 1, len: 8 * KIB }, "eight chunks, unaligned: k8"),
    (Hash { offset: 0, len: 9 * KIB }, "nine chunks: k9"),
    (Many { offset: 0, len: KIB, count: 10 }, "ten one-chunk messages: the SME2 message kernel"),
    (Hash { offset: 0, len: 16 * KIB }, "one SME2 group of sixteen chunks"),
    (Hash { offset: 7, len: 19 * KIB + 5 }, "an SME2 group, a remainder on NEON, unaligned: k3"),
    (Hash { offset: 0, len: 32 * KIB }, "two SME2 groups: the SME2 parent kernel"),
    (ChunkValues { chunks: 18 }, "keyed chaining values: the SME2 kernel with integer lanes"),
    (ChunkValues { chunks: 16 }, "keyed chaining values from a group's counter: the SME2 chunk kernel at a counter per group"),
    (Many { offset: 1, len: 64, count: 2 }, "a pair of 64-byte messages: the parent plans"),
    (Many { offset: 0, len: 64, count: 11 }, "eleven 64-byte messages: a padded SME2 group"),
    (Many { offset: 3, len: 64, count: 21 }, "an SME2 group and five more in a padded group"),
    (Many { offset: 0, len: 1, count: 12 }, "one-byte messages, each in a block of padding"),
    (Many { offset: 7, len: 100, count: 13 }, "messages of 100 bytes: partial blocks in slots"),
    (Many { offset: 0, len: 256, count: 3 }, "three 256-byte messages on the parent plans"),
    (Many { offset: 1, len: 128, count: 9 }, "nine two-block messages: p9"),
    (Many { offset: 1, len: 255, count: 17 }, "an SME2 group of 255-byte messages and one more"),
    (Many { offset: 5, len: 2000, count: 3 }, "two-chunk messages side by side on NEON"),
    (Many { offset: 0, len: 1100, count: 7 }, "two-chunk messages side by side on SME2"),
    (Keyed { len: 2 * KIB + 1 }, "keyed hashing"),
    (Incremental { len: 5 * KIB + 7, piece: 700 }, "a Hasher fed in uneven pieces"),
    (Incremental { len: 2 * KIB, piece: 63 }, "a Hasher fed less than a block at a time"),
    (Xof { len: 100, skip: 37, out: 300 }, "extended output of several blocks, from an unaligned position"),
    (Xof { len: 1500, skip: 64, out: 1600 }, "extended output of twenty-five blocks: SME2's sixteen, then x8 and x1"),
];

/// The cases elsewhere (x86-64 and portable builds), each kernel
/// and each of the Rust paths around them once, on a small input: x86's
/// assembly has one-block, extended-output, and many-input kernels
/// (compress_in_place, compress_xof, hash_many, and on AVX-512 xof_many),
/// which these reach on every x86 platform (AVX2: all 3 of its entries,
/// counted with gdb in October 2026), with the generated passes of whole
/// chunks and a partial one (ffi_partial: four lanes and eight), the tree,
/// the Hasher, a batch and its tail, and the three modes. The logic around
/// the kernels is the tests' and the proofs' to establish; this checks the
/// build and the machine running it.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const CASES: &[(Case, &str)] = &[
    (Hash { offset: 1, len: 65 }, "a block and a byte, unaligned: the one-block kernel"),
    (
        Keyed { len: KIB + 65 },
        "keyed, a chunk and a block and a byte (the shortest the one-chunk pass takes): the four-lane pass with a partial chunk (AVX2, AVX-512)",
    ),
    (Hash { offset: 3, len: 4 * KIB + 65 }, "four chunks and a block, unaligned: the chunk kernel over chunks, their parents, the tree"),
    (Hash { offset: 0, len: 3 * KIB + 100 }, "three chunks and a partial one: the four-lane pass with a partial chunk"),
    (
        Hash { offset: 5, len: 5 * KIB + 321 },
        "five chunks and a partial one of six blocks, unaligned (the fewest the eight-lane pass takes): the eight-lane pass with a partial chunk",
    ),
    (Many { offset: 1, len: 64, count: 17 }, "seventeen 64-byte messages: the batch kernel, full groups and a tail"),
    (Derive { len: 64 }, "key derivation over a block holding all of the previous output"),
    (
        Xof { len: 100, skip: 37, out: 1100 },
        "seventeen blocks and more of extended output, from an unaligned position, through a Hasher: the extended-output kernels",
    ),
];

/// The largest input any case reads, offset included.
#[cfg(target_arch = "aarch64")]
pub(crate) const INPUT_LEN: usize = 32 * KIB + 8;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const INPUT_LEN: usize = 5 * KIB + 512;

/// The most output any case writes (an extended output's).
pub(crate) const OUTPUT_LEN: usize = 1600;

/// The largest batch a Many case lays out, offset included (AArch64: ten
/// one-chunk messages; elsewhere seventeen of 64 bytes).
#[cfg(target_arch = "aarch64")]
const BATCH_LEN: usize = 10 * KIB;
#[cfg(not(target_arch = "aarch64"))]
const BATCH_LEN: usize = 17 * 64 + 1;

/// The most messages a Many case holds.
#[cfg(target_arch = "aarch64")]
const BATCH_COUNT: usize = 21;
#[cfg(not(target_arch = "aarch64"))]
const BATCH_COUNT: usize = 17;

/// Fill `input` with the starting bytes: little-endian words i * 0x9E3779B97F4A7C15.
pub(crate) fn fill(input: &mut [u8]) {
    for (i, word) in input.chunks_mut(8).enumerate() {
        let bytes = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes();
        word.copy_from_slice(&bytes[..word.len()]);
    }
}

/// The context derive_key uses.
pub(crate) const CONTEXT: &str = "blake3-servil self-test";

/// A case's outputs as one 64-bit word: FNV-1a over their bytes. It needs
/// no hash of its own; a wrong output changes it but for a 2^-64 chance.
pub(crate) fn fold(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Run the cases in order, chained: before each, the first 32 bytes of the
/// previous case's output (zeros before the first) overwrite the start of
/// its input (of every message, in a batch), as much as it reads, and are
/// the key a keyed case takes; so every output depends on every earlier
/// one. `outputs` hashes one case into its buffer and returns how many
/// bytes it wrote; `check` sees each case's fold as it comes. Returns the
/// final chaining seed for the platform-specific checks that follow.
pub(crate) fn chain(mut outputs: impl FnMut(Case, &[u8], &[u8; 32], &mut [u8]) -> usize, mut check: impl FnMut(usize, u64)) -> [u8; 32] {
    let mut input = [0u8; INPUT_LEN];
    fill(&mut input);
    let mut out = [0u8; OUTPUT_LEN];
    let mut previous = [0u8; 32];
    for (index, &(case, _)) in CASES.iter().enumerate() {
        let (start, len, count) = match case {
            Hash { offset, len } => (offset, len, 1),
            Many { len, count, .. } => (0, len, count),
            ChunkValues { chunks } => (0, chunks * crate::CHUNK_LEN, 1),
            Keyed { len } | Derive { len } | Incremental { len, .. } | Xof { len, .. } => (0, len, 1),
        };
        let n = len.min(32);
        for message in 0..count {
            input[start + message * len..][..n].copy_from_slice(&previous[..n]);
        }
        let written = outputs(case, &input, &previous, &mut out);
        let output = &out[..written];
        check(index, fold(output));
        previous.copy_from_slice(&output[..32]);
    }
    previous
}

/// Each case's fold, from official BLAKE3 1.8.7 (the unit test
/// `self_test_matches_official_blake3` checks them, and prints this table
/// when it differs).
#[cfg(target_arch = "aarch64")]
const EXPECTED: [u64; 42] = [
    0x90e4e563714f7c48,
    0x80003c83df0cf1a6,
    0xf4ffcff9e99d31ba,
    0xb4bb0ea2c72a1ab1,
    0xa7903018757d10ab,
    0xc9d2c00ed1c20580,
    0xed99f187435619b2,
    0xf0bb0553f78db276,
    0xaf5ff9ff4790a2fb,
    0xab47f312598807b6,
    0xa20d5c093d025c29,
    0xddefca2f181fba05,
    0xb50d992295f65767,
    0x03f7aba2333521ce,
    0xcf1b84d7e1e8f634,
    0x3ed268cc757c3ad5,
    0xfb8a29ffb491beb7,
    0x6d86aff7642ffaaf,
    0x5a02620092be4471,
    0xefc04b50144043d0,
    0x2c6d2745a22caa46,
    0xce56bd0962c822c2,
    0x2ec64bd6f51ba6ff,
    0x442eaa2a0db562ae,
    0x2c5f4d952b6bcfde,
    0x1f69235edf56b6ba,
    0x478371756c405e64,
    0xc870366158dbb83d,
    0x4dc41e003b5dd2c2,
    0x005533a47661dc59,
    0x197a243f90b14025,
    0x64478380cced1fca,
    0xe908cf50ea1fd8cd,
    0x9b9871fc28adae08,
    0x516640462b6e73dc,
    0x6ea50293e266aaf8,
    0xbd0882b3b5d35fe7,
    0x4bfd08492dfa496a,
    0x7b0a14d718f0ed3c,
    0x86659376d6731ca2,
    0xeb7d3b584a6ab00f,
    0x4ec390a6848cb5f5,
];

#[cfg(not(target_arch = "aarch64"))]
const EXPECTED: [u64; 8] = [
    0x64e806087be9bc85,
    0x7d406241656b8c26,
    0xe80762bc37755db3,
    0xdf7d1f1dcd33ceb4,
    0x47bc1a41d7c475f5,
    0x9e68560dd612af97,
    0xe1a4c5967b2225ff,
    0xdb630ec123f1460e,
];

/// A case's outputs from this build, into `out`, through the entry points'
/// bodies from `platform`; returns their length.
pub(crate) fn outputs(platform: Platform, case: Case, input: &[u8], previous: &[u8; 32], out: &mut [u8]) -> usize {
    let words = |key: &[u8; 32]| crate::platform::words_from_le_bytes_32(key);
    let mut digest = |hash: crate::Hash| {
        out[..32].copy_from_slice(hash.as_bytes());
        32
    };
    match case {
        Hash { offset, len } => digest(crate::hash_serial_from(platform, &input[offset..][..len], crate::IV, 0)),
        Many { offset, len, count } => many(platform, &input[..len * count], offset, len, &mut out[..count * crate::OUT_LEN]),
        ChunkValues { chunks } => chunk_values(platform, &input[..chunks * crate::CHUNK_LEN], previous, out),
        Keyed { len } => digest(crate::hash_serial_from(platform, &input[..len], &words(previous), crate::KEYED_HASH)),
        Derive { len } => {
            let context = crate::hash_serial_from(platform, CONTEXT.as_bytes(), crate::IV, crate::DERIVE_KEY_CONTEXT);
            digest(crate::hash_serial_from(platform, &input[..len], &words(context.as_bytes()), crate::DERIVE_KEY_MATERIAL))
        }
        Incremental { len, piece } => digest(incremental(platform, &input[..len], piece)),
        Xof { len, skip, out: n } => xof(platform, &input[..len], skip, &mut out[..n]),
    }
}

/// A Hasher on `platform` fed `input` in pieces of `piece` bytes (out of
/// line, as `xof` and `many`: their buffers take the stack only in turn).
#[inline(never)]
fn incremental(platform: Platform, input: &[u8], piece: usize) -> crate::Hash {
    let mut hasher = crate::Hasher::from_core(crate::HasherCore::new_from(crate::IV, 0, platform));
    for part in input.chunks(piece) {
        hasher.update(part);
    }
    hasher.finalize()
}

/// Extended output of `input` from position `skip`, into `out`.
#[inline(never)]
fn xof(platform: Platform, input: &[u8], skip: u64, out: &mut [u8]) -> usize {
    let mut hasher = crate::Hasher::from_core(crate::HasherCore::new_from(crate::IV, 0, platform));
    let mut reader = hasher.update(input).finalize_xof();
    reader.set_position(skip);
    reader.fill(out);
    out.len()
}

/// hash_many() of `messages`, `len` bytes each, laid out in slots `offset`
/// bytes into a buffer of its own; the digests in `out`.
#[inline(never)]
fn many(platform: Platform, messages: &[u8], offset: usize, len: usize, out: &mut [u8]) -> usize {
    let slot = crate::many::slot_len(len);
    let count = out.len() / crate::OUT_LEN;
    let mut buffer = [0u8; BATCH_LEN];
    let batch = &mut buffer[offset..][..slot * count];
    for (message, bytes) in batch.chunks_mut(slot).zip(messages.chunks(len.max(1))) {
        message[..len].copy_from_slice(&bytes[..len]);
    }
    let mut digests = [[0u8; crate::OUT_LEN]; BATCH_COUNT];
    crate::hash_many_serial_from(platform, batch, len, crate::IV, 0, &mut digests[..count]);
    for (to, digest) in out.chunks_mut(crate::OUT_LEN).zip(&digests) {
        to.copy_from_slice(digest);
    }
    out.len()
}

/// The chaining values of the whole chunks in `input`, at counter 1 on,
/// keyed by `key` (see Case::ChunkValues), into `out`.
fn chunk_values(platform: Platform, input: &[u8], key: &[u8; 32], out: &mut [u8]) -> usize {
    let key = crate::platform::words_from_le_bytes_32(key);
    let mut chunks = arrayvec::ArrayVec::<&[u8; crate::CHUNK_LEN], 18>::new();
    chunks.extend(input.chunks_exact(crate::CHUNK_LEN).map(|c| c.try_into().expect("whole chunks")));
    let out = &mut out[..chunks.len() * crate::OUT_LEN];
    #[cfg(blake3_sme2)]
    if chunks.len() % crate::sme2::HYBRID_GROUP == 0 {
        let turn = crate::platform::Sme2Turn::take(platform, true);
        if matches!(turn.platform(), Platform::SME2) {
            let flags = crate::KEYED_HASH as u32 | (crate::CHUNK_START as u32) << 8 | (crate::CHUNK_END as u32) << 16;
            crate::ran_kernel(crate::sme2::ffi::blake3_servil_sme2x2_hash_chunks_512 as *const ());
            // Sound: SME2 was detected, and the table holds 18 chunk pointers per group.
            let lanes = unsafe {
                crate::sme2::ffi::blake3_servil_sme2x2_hash_chunks_512(
                    chunks.as_ptr() as *const *const u8,
                    key.as_ptr(),
                    1,
                    flags,
                    out.as_mut_ptr(),
                    (chunks.len() / crate::sme2::HYBRID_GROUP) as u64,
                )
            };
            assert_eq!(lanes, 16, "SME2 streaming vector length changed under us");
            return out.len();
        }
    }
    // A group of sixteen: on SME2 the kernel with a counter per group (as
    // Hasher::update_each hashes held groups).
    #[cfg(all(blake3_sme2, feature = "std"))]
    if chunks.len() == crate::sme2::GROUP {
        let turn = crate::platform::Sme2Turn::take(platform, true);
        if matches!(turn.platform(), Platform::SME2) {
            // Sound: SME2 was detected.
            unsafe { crate::sme2::hash_groups_at(&chunks, &[1], &key, crate::KEYED_HASH, crate::CHUNK_START, crate::CHUNK_END, out) };
            return out.len();
        }
    }
    platform.hash_many(&chunks, &key, 1, crate::IncrementCounter::Yes, crate::KEYED_HASH, crate::CHUNK_START, crate::CHUNK_END, out);
    out.len()
}

/// Run every case and panic on the first that differs from the reference;
/// how many cases ran (CASES and the selected platform's extra entries).
fn run() -> usize {
    // A kernel's panic, as a test plants it: the guard in ensure_slow
    // stops the process for it.
    #[cfg(test)]
    if PLANTED_FAILURE.load(core::sync::atomic::Ordering::Relaxed) {
        panic!("a kernel's panic, planted by a test");
    }
    compiled_features_present();
    let platform = Platform::detect_unchecked();
    let mut checked = 0;
    let previous = chain(
        |case, input, previous, out| outputs(platform, case, input, previous, out),
        |index, folded| {
            checked += 1;
            if folded != EXPECTED[index] {
                stop(format_args!(
                    "blake3-servil startup self-test failed: case {index}, {} (platform {}): this build or this CPU \
                     computes wrong BLAKE3 digests on that path",
                    CASES[index].1,
                    platform.name(),
                ));
            }
        },
    );
    #[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
    if crate::platform::wide_avx512() {
        // Sound: the detected platform has the wide kernels' ISA, and
        // hash_many dispatches to them (only then does startup check them). Their
        // wrapper checks its bounds, and builds all arguments on the stack.
        checked += unsafe { crate::avx512_32::self_test(&previous) };
    }
    let _ = previous;
    checked
}

/// Stop when this binary was compiled to assume an instruction that this
/// CPU runs as another one instead of trapping: x86's LZCNT and TZCNT
/// (BMI1) run as BSR and BSF on CPUs without them (Intel before Haswell),
/// silently giving other values (QEMU's Ivy Bridge: leading_zeros(1 << 20)
/// is 20, not 43). Every other instruction the compiler could assume traps
/// on a CPU without it (SIGILL), which already stops the program. The
/// check reads the CPU (CPUID) only in a build that assumes them.
fn compiled_features_present() {
    #[cfg(target_arch = "x86_64")]
    {
        #[cfg(target_arch = "x86_64")]
        use core::arch::x86_64::__cpuid_count;
        let cpuid = |leaf, sub| __cpuid_count(leaf, sub);
        let lacks = |feature: &str| -> ! {
            stop(format_args!(
                "blake3-servil: this binary was compiled for CPUs with {feature}, which this CPU lacks, and here it would compute wrong digests; build it for this CPU"
            ))
        };
        if cfg!(target_feature = "lzcnt") && cpuid(0x8000_0001, 0).ecx & (1 << 5) == 0 {
            lacks("LZCNT");
        }
        if cfg!(target_feature = "bmi1") && cpuid(7, 0).ebx & (1 << 3) == 0 {
            lacks("BMI1 (TZCNT)");
        }
    }
}

/// Stop the process: this build or this CPU computes wrong digests, and
/// nothing may hash on. No caller can catch it; a panic can be caught
/// (catch_unwind, a thread's join, an FFI boundary), and the program
/// would go on hashing wrong digests (Zooko, October 24). With std, the
/// reason to stderr, then abort (SIGABRT). Without std, the CPU's trap
/// instruction (an illegal-instruction fault, SIGILL where there are
/// signals): stable Rust, no library. On an architecture with no trap
/// named here, a panic, to the program's panic handler.
/// The stop for a CPU this crate does not support: the same as a failed
/// self-test's, uncatchable, the reason first.
#[cfg(target_arch = "x86_64")]
#[cold]
pub(crate) fn unsupported(why: core::fmt::Arguments) -> ! {
    stop(why)
}

#[cold]
fn stop(why: core::fmt::Arguments) -> ! {
    #[cfg(feature = "std")]
    {
        std::eprintln!("{why}");
        std::process::abort()
    }
    #[cfg(not(feature = "std"))]
    {
        let _ = why;
        trap()
    }
}

/// The CPU's own stop: an instruction defined to fault.
#[cfg(not(feature = "std"))]
#[inline(always)]
fn trap() -> ! {
    // Sound: each instruction faults where it is executed; nothing after
    // it runs.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("ud2", options(noreturn, nomem, nostack))
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("udf #0", options(noreturn, nomem, nostack))
    }
}

/// The self-test's progress in this process. A failed one stops the
/// process (stop), so no state says so.
const UNTESTED: u8 = 0;
const RUNNING: u8 = 1;
const PASSED: u8 = 2;
static STATE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(UNTESTED);

/// The threads that have reached the wait for another's verdict (test
/// builds only): the test of that wait waits for one to arrive, not for a
/// time, before it gives the verdict.
#[cfg(all(test, feature = "std"))]
static WAITING: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// A failure a test plants in the self-test's run (test builds only), as
/// a CPU or build that computes wrong digests would fail it.
#[cfg(test)]
static PLANTED_FAILURE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Run the self-test unless it has passed: once per process, the first
/// caller running it and any other waiting. It publishes no data, so a
/// relaxed load suffices on the fast path.
#[inline]
pub(crate) fn ensure() {
    if STATE.load(core::sync::atomic::Ordering::Relaxed) != PASSED {
        ensure_slow();
    }
}

#[cold]
#[inline(never)]
fn ensure_slow() {
    use core::sync::atomic::Ordering::Relaxed;
    if STATE.compare_exchange(UNTESTED, RUNNING, Relaxed, Relaxed).is_ok() {
        /// A kernel that panics during the test stops the process too:
        /// its unwind passes here.
        struct Failed;
        impl Drop for Failed {
            fn drop(&mut self) {
                stop(format_args!("blake3-servil startup self-test failed: a kernel panicked on this build or this CPU"));
            }
        }
        let failed = Failed;
        run();
        core::mem::forget(failed);
        STATE.store(PASSED, Relaxed);
        return;
    }
    #[cfg(all(test, feature = "std"))]
    WAITING.fetch_add(1, Relaxed);
    // Wait for the verdict: a test that fails stops the process, so the
    // one a thread sees leave RUNNING is PASSED. The loop spins while the
    // test runs and leaves on any other state, so no change below can
    // make a thread spin forever (the mutation check's clock was the only
    // thing stopping two such changes).
    while STATE.load(Relaxed) == RUNNING {
        core::hint::spin_loop();
    }
    let state = STATE.load(Relaxed);
    assert!(state == PASSED, "the self-test's state is {state} after it ran");
}

#[cfg(all(test, feature = "std"))]
mod test {
    use super::*;

    /// The startup run checks every case, on every platform (a run that
    /// skipped them would pass by doing nothing).
    #[test]
    fn test_self_test_run_checks_every_case() {
        crate::RAN_ENTRIES.with(|r| r.borrow_mut().clear());
        let checked = run();
        let expected = CASES.len();
        #[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
        let expected = if crate::platform::wide_avx512() {
            for (name, entry) in crate::avx512_32::entries() {
                assert!(crate::RAN_ENTRIES.with(|r| r.borrow().contains(&(entry as usize))), "startup left out {name}");
            }
            expected + crate::avx512_32::entries().len()
        } else {
            expected
        };
        assert_eq!(checked, expected);
    }

    /// The first call runs the self-test, which passes (each test runs in
    /// a fresh process, so it has not run yet). Then a thread that arrives
    /// while another runs the self-test waits for its verdict: it returns
    /// once the self-test has passed, and stops once it has failed. The
    /// test stands in for the running thread by setting the state itself,
    /// so the arrival always finds the self-test running, and gives the
    /// verdict once the thread is waiting (WAITING), not after a sleep that a
    /// loaded machine could outlast. A waiting thread that missed the
    /// verdict would spin, and nextest's one clock would end it: a time-out
    /// (tools/mutants/timeouts-<arch>.txt), not a failure.
    #[test]
    fn test_a_waiting_thread_takes_the_verdict() {
        use core::sync::atomic::Ordering::Relaxed;
        crate::initialize();
        assert_eq!(STATE.load(Relaxed), PASSED, "initialize runs the self-test");
        STATE.store(RUNNING, Relaxed);
        let arrived = WAITING.load(Relaxed);
        let waiter = std::thread::spawn(ensure_slow);
        while WAITING.load(Relaxed) == arrived {
            std::thread::yield_now();
        }
        STATE.store(PASSED, Relaxed);
        assert!(waiter.join().is_ok(), "the waiting thread returns after the verdict");
    }

    /// The self-test runs every assembly entry point this machine has:
    /// the hybrids' (with SHA-3 or without), and SME2's with SME2. It runs again when another test held the SME2 turn.
    #[test]
    #[cfg(blake3_neon)]
    fn test_self_test_runs_every_kernel() {
        use crate::{RAN, RAN_ENTRIES, Ran};
        let mut want: std::vec::Vec<(std::string::String, *const ())> = std::vec::Vec::new();
        assert!(!crate::neon_hybrid::entries().is_empty(), "the hybrids' entries");
        want.extend(crate::neon_hybrid::entries());
        #[cfg(blake3_sme2)]
        if crate::platform::sme2_detected() {
            assert!(!crate::sme2::entries().is_empty(), "SME2's entries");
            want.extend(crate::sme2::entries().into_iter().map(|(n, e)| (n.into(), e)));
        }
        let ran = loop {
            RAN.with(|r| r.set(0));
            RAN_ENTRIES.with(|r| r.borrow_mut().clear());
            run();
            if RAN.with(|r| r.get()) & Ran::TurnTaken as u32 == 0 {
                break RAN_ENTRIES.with(|r| r.borrow().clone());
            }
        };
        let missing: std::vec::Vec<&str> = want.iter().filter(|(_, e)| !ran.contains(&(*e as usize))).map(|(n, _)| n.as_str()).collect();
        assert!(missing.is_empty(), "the self-test leaves out {missing:?}");
    }

    /// `outputs` as `chain` takes it: the bytes written into its buffer.
    fn into(mut outputs: impl FnMut(Case, &[u8], &[u8; 32]) -> std::vec::Vec<u8>) -> impl FnMut(Case, &[u8], &[u8; 32], &mut [u8]) -> usize {
        move |case, input, previous, out| {
            let output = outputs(case, input, previous);
            out[..output.len()].copy_from_slice(&output);
            output.len()
        }
    }

    /// A case's outputs from official BLAKE3 1.8.7, which shares no code
    /// with this crate's assembly: the generator of EXPECTED.
    fn reference(case: Case, input: &[u8], previous: &[u8; 32]) -> std::vec::Vec<u8> {
        use crate::Mode;
        use crate::test::official;
        match case {
            Hash { offset, len } => official::hash(Mode::Plain, &input[offset..][..len]).to_vec(),
            Many { len, count, .. } => (0..count).flat_map(|i| official::hash(Mode::Plain, &input[i * len..][..len])).collect(),
            ChunkValues { chunks } => {
                let key = crate::platform::words_from_le_bytes_32(previous);
                let table: std::vec::Vec<&[u8; crate::CHUNK_LEN]> =
                    input[..chunks * crate::CHUNK_LEN].chunks_exact(crate::CHUNK_LEN).map(|c| c.try_into().unwrap()).collect();
                let mut out = std::vec![0u8; chunks * crate::OUT_LEN];
                official::hash_many(&table, &key, 1, true, crate::KEYED_HASH, crate::CHUNK_START, crate::CHUNK_END, &mut out);
                out
            }
            Keyed { len } => official::hash(Mode::Keyed(previous), &input[..len]).to_vec(),
            Derive { len } => official::hash(Mode::DeriveKey(CONTEXT), &input[..len]).to_vec(),
            Incremental { len, .. } => official::hash(Mode::Plain, &input[..len]).to_vec(),
            Xof { len, skip, out } => official::xof(Mode::Plain, &input[..len], skip as u64, out),
        }
    }

    #[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
    /// The chain's outputs from this crate's own kernels (the production
    /// self-test's), for the tests that need a correct chain to start from:
    /// every process checks those kernels against EXPECTED at startup.
    fn ours(case: Case, input: &[u8], previous: &[u8; 32], out: &mut [u8]) -> usize {
        outputs(Platform::detect_unchecked(), case, input, previous, out)
    }

    #[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
    fn wide_reference(inputs: &[&[u8; 192]], key: &crate::CVWords, counter: u64, increment: crate::IncrementCounter, out: &mut [u8]) {
        for (i, &input) in inputs.iter().enumerate() {
            // hash_many's increment is not wrapping; exercise the
            // explicitly wrapping raw ABI with individual calls.
            let c = counter.wrapping_add(if increment.yes() { i as u64 } else { 0 });
            // SSE4.1's kernel, which every AVX-512 CPU has (and the
            // production self-test checks).
            crate::platform::Platform::SSE41.hash_many(
                &[input],
                key,
                c,
                crate::IncrementCounter::No,
                crate::KEYED_HASH,
                crate::CHUNK_START,
                crate::CHUNK_END,
                &mut out[i * crate::OUT_LEN..][..crate::OUT_LEN],
            );
        }
    }

    /// The wide entries' constants come from the chain's seed and SSE4.1's
    /// kernel, not the new assembly. A changed table is printed for review,
    /// never accepted automatically.
    #[test]
    #[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
    fn wide_self_test_matches_reference() {
        let previous = chain(ours, |i, folded| assert_eq!(folded, EXPECTED[i], "shared chain changed"));
        let mut folds = std::vec::Vec::new();
        crate::avx512_32::self_test_chain(&previous, wide_reference, |_, folded| folds.push(folded));
        let table: std::string::String = folds.iter().map(|f| std::format!("    0x{f:016x},\n")).collect();
        assert!(folds == crate::avx512_32::SELF_TEST_EXPECTED, "wide EXPECTED differs from SSE4.1's; its table:\n{table}");
    }

    /// Each new case detects corruption in its last output byte, not
    /// merely the first CV used as the next seed, and stops before any
    /// later case. Use the production checker with SSE4.1's callback.
    #[test]
    #[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
    fn wide_self_test_rejects_corruption() {
        let previous = chain(ours, |i, folded| assert_eq!(folded, EXPECTED[i]));
        for bad in 0..crate::avx512_32::SELF_TEST_EXPECTED.len() {
            let mut calls = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::avx512_32::self_test_chain(
                    &previous,
                    |inputs, key, counter, increment, out| {
                        wide_reference(inputs, key, counter, increment, out);
                        let index = calls;
                        calls += 1;
                        if index == bad {
                            *out.last_mut().expect("a nonempty self-test output") ^= 1;
                        }
                    },
                    crate::avx512_32::check_self_test,
                );
            }));
            let failure = result.expect_err("corruption went unnoticed");
            let message = failure
                .downcast_ref::<std::string::String>()
                .map(std::string::String::as_str)
                .or_else(|| failure.downcast_ref::<&str>().copied())
                .unwrap_or("");
            assert!(message.contains("blake3-servil startup self-test failed"), "wrong failure for case {bad}: {message}");
            assert_eq!(calls, bad + 1, "a case ran after the failed one");
        }
    }

    /// EXPECTED is official BLAKE3's answer for every case. On a
    /// difference the test prints the table official BLAKE3 gives; pasting
    /// it in is a reviewed change.
    #[test]
    fn self_test_matches_official_blake3() {
        let mut folds = std::vec::Vec::new();
        chain(into(reference), |_, folded| folds.push(folded));
        let table: std::string::String = folds.iter().map(|f| std::format!("    0x{f:016x},\n")).collect();
        assert!(folds == EXPECTED, "EXPECTED differs from official BLAKE3's; its table:\n{table}");
    }
}

/// The self-test's stop, in every test build (std or not: the tests use
/// std in every build); a child process.
#[cfg(all(test, unix))]
mod stops {
    use super::*;

    /// A self-test that fails stops the process: no caller can catch it
    /// and go on hashing wrong digests (with std an abort, SIGABRT; without
    /// it the CPU's trap, SIGILL). Run in a child process of
    /// this test binary, which plants a kernel's panic and calls the
    /// self-test inside catch_unwind, as a careless caller might: the
    /// child must die of SIGABRT, saying why. With the guard broken it
    /// would catch the panic and exit 0 (the mutation check had the guard
    /// OPEN: no test made the self-test fail).
    #[test]
    fn test_a_failing_self_test_stops_the_process() {
        use core::sync::atomic::Ordering::Relaxed;
        const CHILD: &str = "BLAKE3_SERVIL_TEST_PLANTED_FAILURE";
        if std::env::var_os(CHILD).is_some() {
            STATE.store(UNTESTED, Relaxed);
            PLANTED_FAILURE.store(true, Relaxed);
            let _ = std::panic::catch_unwind(ensure_slow);
            std::process::exit(0);
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "self_test::stops::test_a_failing_self_test_stops_the_process", "--nocapture", "--test-threads", "1"])
            .env(CHILD, "1")
            .output()
            .expect("the test binary runs again");
        use std::os::unix::process::ExitStatusExt as _;
        let stderr = std::string::String::from_utf8_lossy(&out.stderr);
        if cfg!(feature = "std") {
            assert_eq!(out.status.signal(), Some(6), "the child aborts (SIGABRT): {:?}\n{stderr}", out.status);
            assert!(stderr.contains("startup self-test failed"), "it says why: {stderr}");
        } else {
            assert_eq!(out.status.signal(), Some(4), "the child traps (SIGILL): {:?}\n{stderr}", out.status);
        }
    }
}

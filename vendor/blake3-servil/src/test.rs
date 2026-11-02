use crate::{BLOCK_LEN, CHUNK_LEN, CVBytes};

include!("../tools/require_nextest.rs");
use arrayvec::ArrayVec;

/// Every symbol the assembly defines carries the crate's prefix,
/// `blake3_servil_`, so a program that links official BLAKE3 as well (whose
/// kernels are `blake3_*`) binds each crate's calls to its own kernels
/// (#151: before, the linker kept one of each name, and servil ran
/// official's AVX2 and AVX-512 batch kernels). A new kernel without the
/// prefix fails here, on every machine, before it can meet official's.
#[test]
#[cfg(feature = "std")]
fn test_every_assembly_symbol_carries_the_crate_prefix() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("c");
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("S") {
            continue;
        }
        files += 1;
        for (n, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
            let mut words = line.split_whitespace();
            // .globl / .global: the names the linker sees.
            if let (Some(".globl" | ".global"), Some(name)) = (words.next(), words.next()) {
                assert!(name.trim_start_matches('_').starts_with("blake3_servil_"), "{}:{}: {name} lacks the prefix blake3_servil_", path.display(), n + 1);
            }
        }
    }
    assert!(files >= 8, "the assembly files in {}: {files}", dir.display());
}

/// The kernels servil shares a name with official BLAKE3 1.8.7, with
/// official linked into this binary (a dev-dependency): each of servil's
/// bindings reaches an entry of its own, and both crates hash alike.
#[test]
#[cfg(all(feature = "std", target_arch = "x86_64", blake3_sse41_ffi))]
fn test_official_kernels_and_ours_stay_apart() {
    // Official's names, as its own assembly exports them.
    unsafe extern "C" {
        fn blake3_compress_in_place_sse41();
        fn blake3_compress_xof_sse41();
        fn blake3_hash_many_sse41();
        fn blake3_hash_many_avx2();
    }
    #[cfg(blake3_avx512_ffi)]
    unsafe extern "C" {
        fn blake3_compress_in_place_avx512();
        fn blake3_compress_xof_avx512();
        fn blake3_hash_many_avx512();
    }
    crate::initialize();
    let input = core::hint::black_box([42u8; 4096]);
    assert_eq!(crate::hash_serial(&input, crate::IV, 0).as_bytes(), blake3::hash(&input).as_bytes());
    #[allow(unused_mut)]
    let mut pairs: Vec<(&str, *const (), *const ())> = vec![
        ("compress_in_place_sse41", crate::sse41::ffi::blake3_servil_compress_in_place_sse41 as *const (), blake3_compress_in_place_sse41 as *const ()),
        ("compress_xof_sse41", crate::sse41::ffi::blake3_servil_compress_xof_sse41 as *const (), blake3_compress_xof_sse41 as *const ()),
        ("hash_many_sse41", crate::sse41::ffi::blake3_servil_hash_many_sse41 as *const (), blake3_hash_many_sse41 as *const ()),
        ("hash_many_avx2", crate::avx2::ffi::blake3_servil_hash_many_avx2 as *const (), blake3_hash_many_avx2 as *const ()),
    ];
    #[cfg(blake3_avx512_ffi)]
    pairs.extend([
        ("compress_in_place_avx512", crate::avx512::ffi::blake3_servil_compress_in_place_avx512 as *const (), blake3_compress_in_place_avx512 as *const ()),
        ("compress_xof_avx512", crate::avx512::ffi::blake3_servil_compress_xof_avx512 as *const (), blake3_compress_xof_avx512 as *const ()),
        ("hash_many_avx512", crate::avx512::ffi::blake3_servil_hash_many_avx512 as *const (), blake3_hash_many_avx512 as *const ()),
    ]);
    for (name, ours, official) in pairs {
        assert_ne!(ours, official, "servil's {name} is bound to official's kernel");
    }
}

/// `bytes` copied into whole blocks, zero after them: a message as the
/// performance calls take one.
#[cfg(feature = "std")]
pub(crate) fn message(bytes: &[u8]) -> std::vec::Vec<crate::performance::Block> {
    let mut blocks = std::vec![crate::performance::Block::ZERO; bytes.len().div_ceil(crate::BLOCK_LEN).max(1)];
    crate::performance::Block::bytes_mut(&mut blocks)[..bytes.len()].copy_from_slice(bytes);
    blocks
}

/// Each of `items` copied into blocks (`message`).
#[cfg(feature = "std")]
pub(crate) fn messages(items: &[&[u8]]) -> std::vec::Vec<std::vec::Vec<crate::performance::Block>> {
    items.iter().map(|i| message(i)).collect()
}

/// `owned`'s messages (from `message`) as hash_each's items, `items`'
/// lengths.
#[cfg(feature = "std")]
pub(crate) fn items<'a>(owned: &'a [std::vec::Vec<crate::performance::Block>], items: &[&[u8]]) -> std::vec::Vec<(&'a [crate::performance::Block], usize)> {
    owned.iter().zip(items).map(|(b, i)| (&b[..], i.len())).collect()
}

/// `bytes` (whole blocks) copied into blocks, as the batch calls take them.
pub(crate) fn blocks(bytes: &[u8]) -> std::vec::Vec<crate::performance::Block> {
    assert_eq!(bytes.len() % crate::BLOCK_LEN, 0, "whole blocks");
    bytes.chunks_exact(crate::BLOCK_LEN).map(|b| crate::performance::Block(b.try_into().unwrap())).collect()
}

// Interesting input lengths to run tests on.
pub const TEST_CASES: &[usize] = &[
    0,
    1,
    2,
    3,
    4,
    5,
    6,
    7,
    8,
    BLOCK_LEN - 1,
    BLOCK_LEN,
    BLOCK_LEN + 1,
    2 * BLOCK_LEN - 1,
    2 * BLOCK_LEN,
    2 * BLOCK_LEN + 1,
    CHUNK_LEN - 1,
    CHUNK_LEN,
    CHUNK_LEN + 1,
    2 * CHUNK_LEN,
    2 * CHUNK_LEN + 1,
    3 * CHUNK_LEN,
    3 * CHUNK_LEN + 1,
    4 * CHUNK_LEN,
    4 * CHUNK_LEN + 1,
    5 * CHUNK_LEN,
    5 * CHUNK_LEN + 1,
    6 * CHUNK_LEN,
    6 * CHUNK_LEN + 1,
    7 * CHUNK_LEN,
    7 * CHUNK_LEN + 1,
    8 * CHUNK_LEN,
    8 * CHUNK_LEN + 1,
    16 * CHUNK_LEN - 1,
    16 * CHUNK_LEN, // AVX512's bandwidth
    16 * CHUNK_LEN + 1,
    31 * CHUNK_LEN - 1,
    31 * CHUNK_LEN, // 16 + 8 + 4 + 2 + 1
    31 * CHUNK_LEN + 1,
    100 * CHUNK_LEN, // subtrees larger than MAX_SIMD_DEGREE chunks
];

pub const TEST_CASES_MAX: usize = 100 * CHUNK_LEN;

// Paint the input with a repeating byte pattern. We use a cycle length of 251,
// because that's the largest prime number less than 256. This makes it
// unlikely to swapping any two adjacent input blocks or chunks will give the
// same answer.

// Golden, the fixed values' check, shared with the integration tests.
#[cfg(feature = "std")]
#[path = "../tests/common/golden.rs"]
mod golden;
#[cfg(feature = "std")]
pub(crate) use golden::Golden;

/// Official BLAKE3 1.8.7, the generator of the fixed values: its public
/// calls, and its hidden portable kernels (`blake3::platform`) for the
/// compression-level sweeps.
#[cfg(feature = "std")]
pub(crate) mod official {
    use std::vec::Vec;
    fn portable() -> blake3::platform::Platform {
        blake3::platform::Platform::portable()
    }
    pub(crate) fn hash(mode: crate::Mode, input: &[u8]) -> [u8; 32] {
        match mode {
            crate::Mode::Plain => *blake3::hash(input).as_bytes(),
            crate::Mode::Keyed(key) => *blake3::keyed_hash(key, input).as_bytes(),
            crate::Mode::DeriveKey(context) => blake3::derive_key(context, input),
            crate::Mode::DeriveKeyWith(_) => unreachable!("a context key's own mode is DeriveKey"),
        }
    }
    /// `len` bytes of extended output from byte `skip`.
    pub(crate) fn xof(mode: crate::Mode, input: &[u8], skip: u64, len: usize) -> Vec<u8> {
        let mut hasher = match mode {
            crate::Mode::Plain => blake3::Hasher::new(),
            crate::Mode::Keyed(key) => blake3::Hasher::new_keyed(key),
            crate::Mode::DeriveKey(context) => blake3::Hasher::new_derive_key(context),
            crate::Mode::DeriveKeyWith(_) => unreachable!("a context key's own mode is DeriveKey"),
        };
        hasher.update(input);
        let mut reader = hasher.finalize_xof();
        reader.set_position(skip);
        let mut out = std::vec![0u8; len];
        reader.fill(&mut out);
        out
    }
    pub(crate) fn compress_xof(cv: &[u32; 8], block: &[u8; 64], block_len: u8, counter: u64, flags: u8) -> [u8; 64] {
        portable().compress_xof(cv, block, block_len, counter, flags)
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn hash_many<const N: usize>(inputs: &[&[u8; N]], key: &[u32; 8], counter: u64, increment: bool, flags: u8, start: u8, end: u8, out: &mut [u8]) {
        let increment = if increment { blake3::IncrementCounter::Yes } else { blake3::IncrementCounter::No };
        portable().hash_many::<N>(inputs, key, counter, increment, flags, start, end, out)
    }
    /// Each chunk's chaining value (a chunk of the plain hash, not the root).
    pub(crate) fn chunk_cvs(input: &[u8], counter: u64) -> Vec<u8> {
        input
            .chunks(crate::CHUNK_LEN)
            .enumerate()
            .flat_map(|(i, c)| {
                use blake3::hazmat::HasherExt;
                blake3::Hasher::new().set_input_offset((counter + i as u64) * crate::CHUNK_LEN as u64).update(c).finalize_non_root()
            })
            .collect()
    }
}

/// Every platform this CPU can run, not only detect()'s:
/// the code paths of a platform narrower than detect()'s (SSE4.1's and
/// AVX2's on an AVX-512 machine) run on the machines that lack the wider
/// one, and a test on detect()'s alone never reaches them (an EPYC 9255,
/// October 22: spare_lane's partial chunk and xof_many's per-block loop
/// went untested there).
#[cfg(feature = "std")]
pub(crate) fn every_platform() -> Vec<crate::platform::Platform> {
    use crate::platform::Platform;
    #[allow(unused_mut)]
    let mut all = std::vec::Vec::new();
    #[cfg(target_arch = "x86_64")]
    all.extend([Platform::sse41(), Platform::avx2()].into_iter().flatten());
    #[cfg(blake3_avx512_ffi)]
    all.extend(Platform::avx512());
    #[cfg(blake3_neon)]
    all.extend(Platform::neon());
    #[cfg(blake3_sme2)]
    all.extend(Platform::sme2());
    all
}

/// compress_chunks_parallel on every platform the CPU runs, against fixed
/// values (official BLAKE3's chunk chaining values): every count of whole
/// chunks it takes, with no partial chunk and with partial ones on each
/// side of the thresholds spare_lane and pair choose by.
#[test]
#[cfg(feature = "std")]
fn test_compress_chunks_parallel_every_cpu_platform() {
    use crate::platform::MAX_SIMD_DEGREE;
    let mut input = vec![0u8; MAX_SIMD_DEGREE * CHUNK_LEN];
    paint_test_input(&mut input);
    let platforms = every_platform();
    let mut golden = Golden::new("compress_chunks_parallel");
    for whole in 0..=MAX_SIMD_DEGREE {
        for partial in [0, 1, BLOCK_LEN, BLOCK_LEN + 1, 5 * BLOCK_LEN, 5 * BLOCK_LEN + 1, CHUNK_LEN - 1] {
            let len = whole * CHUNK_LEN + partial;
            if len == 0 || len > input.len() {
                continue;
            }
            for counter in [0, u32::MAX as u64] {
                let run = |platform| {
                    let mut out = vec![0u8; (MAX_SIMD_DEGREE + 1) * crate::OUT_LEN];
                    let n = crate::compress_chunks_parallel(&input[..len], crate::IV, counter, 0, platform, &mut out);
                    out.truncate(n * crate::OUT_LEN);
                    out
                };
                let first = run(platforms[0]);
                for &platform in &platforms[1..] {
                    assert!(run(platform) == first, "{platform:?}: {whole} whole chunks and {partial} bytes, counter {counter}");
                }
                golden.add(&first, || official::chunk_cvs(&input[..len], counter));
            }
        }
    }
    // The counts end at MAX_SIMD_DEGREE, so each degree's sweep has its own
    // value: 32 on x86-64, 128 on AArch64 with SME2, 16 without.
    golden.check(match MAX_SIMD_DEGREE {
        16 => 0x3c82c44666778d0f,
        32 => 0x17d8ef758412b936,
        128 => 0x3b1aa2a01ca807a4,
        d => panic!("no fixed value for MAX_SIMD_DEGREE {d}"),
    });
}

/// Platform::xof_many on every platform the CPU runs, against fixed values
/// (official BLAKE3's compress_xof, block by block): the platforms without a kernel of their
/// own take its loop over compress_xof (SSE4.1 and AVX2 on x86), which a
/// test of detect()'s platform alone leaves out on an AVX-512 machine.
#[test]
#[cfg(feature = "std")]
fn test_xof_many_every_cpu_platform() {
    let mut block = [0; BLOCK_LEN];
    let block_len = 42;
    paint_test_input(&mut block[..block_len]);
    let cv = [40, 41, 42, 43, 44, 45, 46, 47];
    let flags = crate::KEYED_HASH;
    let platforms = every_platform();
    let mut golden = Golden::new("xof_many_every_cpu_platform");
    for counter in [0, u32::MAX as u64, i32::MAX as u64] {
        // Past 32, the AVX-512 kernel's proved count, the calls split.
        for blocks in [1, 2, 3, 16, 17, 31, 32, 33, 64, 65, 100] {
            let run = |platform: crate::platform::Platform| {
                let mut got = vec![0u8; blocks * BLOCK_LEN];
                platform.xof_many(&cv, &block, block_len as u8, counter, flags, &mut got);
                got
            };
            let first = run(platforms[0]);
            for &platform in &platforms[1..] {
                assert!(run(platform) == first, "{platform:?}: {blocks} blocks from counter {counter}");
            }
            golden.add(&first, || (0..blocks).flat_map(|i| official::compress_xof(&cv, &block, block_len as u8, counter + i as u64, flags)).collect());
        }
    }
    golden.check(0xce209f7b4d1fa413);
}

/// The extended-output sweep of NEON's groups of four (ffi_neon_hybrid's
/// test) against its fixed fold: counters whose low word wraps inside a
/// group, flag sets every mode uses, short and full blocks, lengths that
/// leave blocks past the groups (which `xof` leaves untouched). `xof` fills
/// `out`'s whole groups of four blocks and returns the bytes filled. Every
/// machine runs it (x86-64 on its own kernels), so the constant is
/// generated and checked on any of them.
#[cfg(feature = "std")]
pub(crate) fn xof_groups_sweep(xof: &mut dyn FnMut(&crate::CVWords, &[u8; BLOCK_LEN], u8, u64, u8, &mut [u8]) -> usize) {
    let cv: crate::CVWords = core::array::from_fn(|i| 0x9E37_79B9u32.wrapping_mul(i as u32 + 3));
    let mut block = [0u8; BLOCK_LEN];
    paint_test_input(&mut block);
    let mut golden = Golden::new("xof_groups");
    for counter in [0u64, 7, 0xFFFF_FFFE, 0x1_FFFF_FFFE] {
        for (block_len, flags) in [(64u8, crate::ROOT), (21, crate::ROOT | crate::CHUNK_END), (0, crate::ROOT | crate::KEYED_HASH | crate::PARENT)] {
            for blocks in [4usize, 5, 8, 11, 12, 16, 20] {
                let mut out = std::vec![0u8; blocks * BLOCK_LEN];
                let done = xof(&cv, &block, block_len, counter, flags, &mut out);
                assert_eq!(done, blocks / 4 * 4 * BLOCK_LEN);
                assert!(out[done..].iter().all(|&b| b == 0), "nothing past the groups");
                golden.add(&out[..done], || {
                    (0..done / BLOCK_LEN).flat_map(|i| official::compress_xof(&cv, &block, block_len, counter.wrapping_add(i as u64), flags)).collect()
                });
            }
        }
    }
    golden.check(0x7fb1a8bd9825109c);
}

/// The groups' sweep on x86-64's kernels, so the constant is checked here.
#[test]
#[cfg(all(feature = "std", target_arch = "x86_64"))]
fn test_xof_groups_sweep_on_this_cpu() {
    xof_groups_sweep(&mut |cv, block, block_len, counter, flags, out| {
        let done = out.len() / (4 * BLOCK_LEN) * 4 * BLOCK_LEN;
        crate::platform::Platform::detect().xof_many(cv, block, block_len, counter, flags, &mut out[..done]);
        done
    });
}

pub fn paint_test_input(buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
}

#[cfg(blake3_avx512_ffi)]
type XofManyFunction = unsafe fn(cv: &crate::CVWords, block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8, out: &mut [u8]);

/// An extended-output kernel (AVX-512's) against SSE4.1's.
#[cfg(blake3_avx512_ffi)]
pub fn test_xof_many_fn(xof_many_function: XofManyFunction) {
    let mut block = [0; BLOCK_LEN];
    let block_len = 42;
    crate::test::paint_test_input(&mut block[..block_len]);
    let cv = [40, 41, 42, 43, 44, 45, 46, 47];
    let flags = crate::KEYED_HASH;

    // Test a few different initial counter values.
    // - 0: The base case.
    // - u32::MAX: The low word of the counter overflows for all inputs except the first.
    // - i32::MAX: *No* overflow. But carry bugs in tricky SIMD code can screw this up, if you XOR
    //   when you're supposed to ANDNOT...
    // Against SSE4.1's extended output (every x86-64 CPU this runs on has
    // it; the sweeps above hold SSE4.1 to fixed values).
    let initial_counters = [0, u32::MAX as u64, i32::MAX as u64];
    for counter in initial_counters {
        // 31 (16 + 8 + 4 + 2 + 1) outputs
        const OUTPUT_SIZE: usize = 31 * BLOCK_LEN;
        let mut want = [0u8; OUTPUT_SIZE];
        crate::platform::Platform::SSE41.xof_many(&cv, &block, block_len as u8, counter, flags, &mut want);
        let mut test_out = [0u8; OUTPUT_SIZE];
        unsafe {
            xof_many_function(&cv, &block, block_len as u8, counter, flags, &mut test_out);
        }
        assert_eq!(want, test_out, "counter {counter}");
    }

    // Test that xof_many doesn't write more blocks than requested. Note that the current assembly
    // implementation always outputs at least one block, so we don't test the zero case.
    for block_count in 1..=32 {
        let mut array = [0; BLOCK_LEN * 33];
        let output_start = 17;
        let output_len = block_count * BLOCK_LEN;
        let output_end = output_start + output_len;
        let output = &mut array[output_start..output_end];
        unsafe {
            xof_many_function(&cv, &block, block_len as u8, 0, flags, output);
        }
        for i in 0..array.len() {
            if i < output_start || output_end <= i {
                assert_eq!(0, array[i], "index {i}");
            }
        }
    }
}

#[test]
fn test_xof_seek() {
    let mut out = [0; 533];
    let mut hasher = crate::Hasher::new(crate::Mode::Plain);
    hasher.update(b"foo");
    hasher.finalize_xof().fill(&mut out);
    assert_eq!(hasher.finalize().as_bytes(), &out[0..32]);

    let mut reader = hasher.finalize_xof();
    reader.set_position(303);
    let mut out2 = [0; 102];
    reader.fill(&mut out2);
    assert_eq!(&out[303..][..102], &out2[..]);

    #[cfg(feature = "std")]
    {
        use std::io::prelude::*;
        let mut reader = hasher.finalize_xof();
        reader.seek(std::io::SeekFrom::Start(303)).unwrap();
        let mut out3 = Vec::new();
        reader.by_ref().take(102).read_to_end(&mut out3).unwrap();
        assert_eq!(&out[303..][..102], &out3[..]);

        assert_eq!(reader.seek(std::io::SeekFrom::Current(0)).unwrap(), 303 + 102);
        reader.seek(std::io::SeekFrom::Current(-5)).unwrap();
        assert_eq!(reader.seek(std::io::SeekFrom::Current(0)).unwrap(), 303 + 102 - 5);
        let mut out4 = [0; 17];
        assert_eq!(reader.read(&mut out4).unwrap(), 17);
        assert_eq!(&out[303 + 102 - 5..][..17], &out4[..]);
        assert_eq!(reader.seek(std::io::SeekFrom::Current(0)).unwrap(), 303 + 102 - 5 + 17);
        assert!(reader.seek(std::io::SeekFrom::End(0)).is_err());
        assert!(reader.seek(std::io::SeekFrom::Current(-1000)).is_err());
        // Back to the start exactly, and one byte before it; past the end
        // of u64 stops at its end.
        let at = reader.position() as i64;
        assert_eq!(reader.seek(std::io::SeekFrom::Current(-at)).unwrap(), 0);
        assert!(reader.seek(std::io::SeekFrom::Current(-1)).is_err());
        reader.seek(std::io::SeekFrom::Start(u64::MAX - 1)).unwrap();
        assert_eq!(reader.seek(std::io::SeekFrom::Current(2)).unwrap(), u64::MAX);
        assert_eq!(std::format!("{reader:?}"), "OutputReader { position: 18446744073709551615 }");
    }
}

#[test]
fn test_reset() {
    let mut hasher = crate::Hasher::new(crate::Mode::Plain);
    hasher.update(&[42; 3 * CHUNK_LEN + 7]);
    hasher.reset();
    hasher.update(&[42; CHUNK_LEN + 3]);
    assert_eq!(hasher.finalize(), crate::convenience::hash(&[42; CHUNK_LEN + 3]));

    let key = &[99; crate::KEY_LEN];
    let mut keyed_hasher = crate::Hasher::new(crate::Mode::Keyed(key));
    keyed_hasher.update(&[42; 3 * CHUNK_LEN + 7]);
    keyed_hasher.reset();
    keyed_hasher.update(&[42; CHUNK_LEN + 3]);
    assert_eq!(keyed_hasher.finalize(), crate::convenience::hash_with(crate::Mode::Keyed(key), &[42; CHUNK_LEN + 3]),);

    let context = "BLAKE3 2020-02-12 10:20:58 reset test";
    let mut kdf = crate::Hasher::new(crate::Mode::DeriveKey(context));
    kdf.update(&[42; 3 * CHUNK_LEN + 7]);
    kdf.reset();
    kdf.update(&[42; CHUNK_LEN + 3]);
    let expected = *crate::convenience::hash_with(crate::Mode::DeriveKey(context), &[42; CHUNK_LEN + 3]).as_bytes();
    assert_eq!(kdf.finalize(), expected);

    // A reset after the stack holds values (past the stage, which keeps
    // short updates from the core): the stack empty again.
    let long = std::vec![42u8; 5 * crate::STAGE_LEN + 7];
    let mut hasher = crate::Hasher::new(crate::Mode::Plain);
    hasher.update(&long);
    hasher.reset();
    hasher.update(&long[..CHUNK_LEN + 3]);
    assert_eq!(hasher.finalize(), crate::convenience::hash(&[42; CHUNK_LEN + 3]));
}

#[test]
fn test_hex_encoding_decoding() {
    let digest_str = "04e0bb39f30b1a3feb89f536c93be15055482df748674b00d26e5a75777702e9";
    let mut hasher = crate::Hasher::new(crate::Mode::Plain);
    hasher.update(b"foo");
    let digest = hasher.finalize();
    assert_eq!(digest.to_hex().as_str(), digest_str);
    #[cfg(feature = "std")]
    assert_eq!(digest.to_string(), digest_str);

    // Test round trip
    let digest = crate::Hash::from_hex(digest_str).unwrap();
    assert_eq!(digest.to_hex().as_str(), digest_str);

    // Test uppercase
    let digest = crate::Hash::from_hex(digest_str.to_uppercase()).unwrap();
    assert_eq!(digest.to_hex().as_str(), digest_str);

    // Test string parsing via FromStr
    let digest: crate::Hash = digest_str.parse().unwrap();
    assert_eq!(digest.to_hex().as_str(), digest_str);

    // Test errors
    let bad_len = "04e0bb39f30b1";
    let _result = crate::Hash::from_hex(bad_len).unwrap_err();
    #[cfg(feature = "std")]
    assert_eq!(_result.to_string(), "expected 64 hex bytes, received 13");

    let bad_char = "Z4e0bb39f30b1a3feb89f536c93be15055482df748674b00d26e5a75777702e9";
    let _result = crate::Hash::from_hex(bad_char).unwrap_err();
    #[cfg(feature = "std")]
    assert_eq!(_result.to_string(), "invalid hex character: 'Z'");

    let _result = crate::Hash::from_hex([128; 64]).unwrap_err();
    #[cfg(feature = "std")]
    assert_eq!(_result.to_string(), "invalid hex character: 0x80");
}

#[test]
fn test_hash_conversions() {
    let bytes1 = [42; 32];
    let hash1: crate::Hash = bytes1.into();
    let bytes2: [u8; 32] = hash1.into();
    assert_eq!(bytes1, bytes2);

    let bytes3 = *hash1.as_bytes();
    assert_eq!(bytes1, bytes3);

    let hash2 = crate::Hash::from_bytes(bytes1);
    assert_eq!(hash1, hash2);

    let hex = hash1.to_hex();
    let hash3 = crate::Hash::from_hex(hex.as_bytes()).unwrap();
    assert_eq!(hash1, hash3);

    let slice1: &[u8] = bytes1.as_slice();
    let hash4 = crate::Hash::from_slice(slice1).expect("correct length");
    assert_eq!(hash1, hash4);

    let slice2 = hash1.as_slice();
    assert_eq!(slice1, slice2);

    assert!(crate::Hash::from_slice(&[]).is_err());
    assert!(crate::Hash::from_slice(&[42]).is_err());
    assert!(crate::Hash::from_slice([42; 31].as_slice()).is_err());
    assert!(crate::Hash::from_slice([42; 33].as_slice()).is_err());
    assert!(crate::Hash::from_slice([42; 100].as_slice()).is_err());
}

#[test]
const fn test_hash_const_conversions() {
    let bytes = [42; 32];
    let hash = crate::Hash::from_bytes(bytes);
    _ = hash.as_bytes();
}

/// ChunkState.buf and Output.block are 64-byte aligned, so that wide vector
/// stores into them (memcpy's among them) forward to the loads after them
/// whatever the stack's layout: a measured speed property (the comment on
/// Output in hasher.rs; https://github.com/BLAKE3-team/BLAKE3/pull/582)
/// that no mutant models and the speed gate, AArch64's alone, does not see.
#[test]
fn test_block_buffer_alignment() {
    assert_eq!(64, core::mem::align_of::<crate::Output>());
    assert_eq!(0, core::mem::offset_of!(crate::Output, block));
    assert_eq!(64, core::mem::align_of::<crate::ChunkState>());
    assert_eq!(0, core::mem::offset_of!(crate::ChunkState, buf));
}

#[cfg(feature = "zeroize")]
#[test]
fn test_zeroize() {
    use zeroize::Zeroize;

    let mut hash = crate::Hash([42; 32]);
    hash.zeroize();
    assert_eq!(hash.0, [0u8; 32]);

    // A Hasher: its core, and the bytes it has staged.
    let mut hasher = crate::Hasher::new(crate::Mode::Keyed(&[42; 32]));
    hasher.update(&[42; 3000]);
    assert!(hasher.staged > 0);
    hasher.zeroize();
    assert_eq!(hasher.staged, 0);
    assert!(hasher.stage.iter().all(|b| unsafe { b.assume_init() } == 0));
    assert_eq!(hasher.core.key, [0; 8]);

    let mut hasher = crate::HasherCore {
        chunk_state: crate::hasher::ChunkState {
            cv: [42; 8],
            chunk_counter: 42,
            buf: [42; 64],
            buf_len: 42,
            blocks_compressed: 42,
            flags: 42,
            platform: crate::Platform::detect_unchecked(),
        },
        initial_chunk_counter: 42,
        key: [42; 8],
        cv_stack: crate::hasher::CvStack::new(),
    };
    for _ in 0..crate::MAX_DEPTH + 1 {
        hasher.cv_stack.push([42; 32]);
    }
    hasher.zeroize();
    assert_eq!(hasher.chunk_state.cv, [0; 8]);
    assert_eq!(hasher.chunk_state.chunk_counter, 0);
    assert_eq!(hasher.chunk_state.buf, [0; 64]);
    assert_eq!(hasher.chunk_state.buf_len, 0);
    assert_eq!(hasher.chunk_state.blocks_compressed, 0);
    assert_eq!(hasher.chunk_state.flags, 0);
    assert_eq!(hasher.chunk_state.platform, crate::Platform::detect_unchecked(), "zeroize leaves the platform");
    assert_eq!(hasher.initial_chunk_counter, 0);
    assert_eq!(hasher.key, [0; 8]);
    assert_eq!(hasher.cv_stack.len(), 0);
    assert_eq!(hasher.cv_stack.array(), Some(&[[0u8; 32]; crate::MAX_DEPTH + 1]));

    let mut output_reader = crate::OutputReader {
        inner: crate::Output {
            input_chaining_value: [42; 8],
            block: [42; 64],
            counter: 42,
            block_len: 42,
            flags: 42,
            platform: crate::Platform::detect_unchecked(),
        },
        position_within_block: 42,
    };

    output_reader.zeroize();
    assert_eq!(output_reader.inner.input_chaining_value, [0; 8]);
    assert_eq!(output_reader.inner.block, [0; 64]);
    assert_eq!(output_reader.inner.counter, 0);
    assert_eq!(output_reader.inner.block_len, 0);
    assert_eq!(output_reader.inner.flags, 0);
    assert_eq!(output_reader.inner.platform, crate::Platform::detect_unchecked(), "zeroize leaves the platform");
    assert_eq!(output_reader.position_within_block, 0);
}

#[test]
#[cfg(feature = "std")]
fn test_update_reader_keeps_every_byte_across_errors() {
    // A reader that fails (WouldBlock) after given offsets, in short reads,
    // and a caller that retries until the end: every byte read before each
    // error is hashed, inside the first 64 KiB and inside the 1 MiB buffer.
    use std::io;
    struct Failing<'a> {
        slice: &'a [u8],
        at: usize,
        fail_at: Vec<usize>,
    }
    impl io::Read for Failing<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_at.first() == Some(&self.at) {
                self.fail_at.remove(0);
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let next = self.fail_at.first().copied().unwrap_or(usize::MAX);
            let take = buf.len().min(self.slice.len()).min(next - self.at).min(50_000);
            buf[..take].copy_from_slice(&self.slice[..take]);
            self.slice = &self.slice[take..];
            self.at += take;
            Ok(take)
        }
    }
    let mut input = vec![0; (3 << 20) + 12345];
    paint_test_input(&mut input);
    for fail_at in [vec![], vec![1000], vec![65536], vec![70_000, 1 << 20], vec![10, 2_000_000, 3_000_000]] {
        let mut reader = Failing { slice: &input, at: 0, fail_at: fail_at.clone() };
        let mut hasher = crate::Hasher::new(crate::Mode::Plain);
        let mut errors = 0;
        while let Err(e) = hasher.update_reader(&mut reader) {
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
            errors += 1;
        }
        assert_eq!(errors, fail_at.len());
        assert_eq!(hasher.finalize(), crate::convenience::hash(&input), "failing at {fail_at:?}");
    }
}

#[test]
#[cfg(feature = "std")]
fn test_update_reader_interrupted() -> std::io::Result<()> {
    use std::io;
    struct InterruptingReader<'a> {
        already_interrupted: bool,
        slice: &'a [u8],
    }
    impl<'a> InterruptingReader<'a> {
        fn new(slice: &'a [u8]) -> Self {
            Self { already_interrupted: false, slice }
        }
    }
    impl<'a> io::Read for InterruptingReader<'a> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.already_interrupted {
                self.already_interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            let take = std::cmp::min(self.slice.len(), buf.len());
            buf[..take].copy_from_slice(&self.slice[..take]);
            self.slice = &self.slice[take..];
            Ok(take)
        }
    }

    let input = b"hello world";
    let mut reader = InterruptingReader::new(input);
    let mut hasher = crate::Hasher::new(crate::Mode::Plain);
    hasher.update_reader(&mut reader)?;
    assert_eq!(hasher.finalize(), crate::convenience::hash(input));
    Ok(())
}

/// Hash's wire format: an array of 32 bytes in JSON and CBOR, and a CBOR
/// bytestring still read. The format comes from a derive no mutant touches,
/// so this test is its one check.
#[test]
#[cfg(feature = "std")]
#[cfg(feature = "serde")]
fn test_serde() {
    // Henrik suggested that we use 0xfe / 254 for byte test data instead of 0xff / 255, due to the
    // fact that 0xfe is not a well formed CBOR item.
    let hash: crate::Hash = [0xfe; 32].into();

    let json = serde_json::to_string(&hash).unwrap();
    assert_eq!(json, "[254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254,254]",);
    let hash2: crate::Hash = serde_json::from_str(&json).unwrap();
    assert_eq!(hash, hash2);

    let mut cbor = Vec::<u8>::new();
    ciborium::into_writer(&hash, &mut cbor).unwrap();
    assert_eq!(
        cbor,
        [
            0x98, 0x20, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe,
            0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe,
            0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe, 0x18, 0xfe,
        ]
    );
    let hash_from_cbor: crate::Hash = ciborium::from_reader(&cbor[..]).unwrap();
    assert_eq!(hash_from_cbor, hash);

    // Version 1.5.2 of this crate changed the default serialization format to a bytestring
    // (instead of an array/list) to save bytes on the wire. That was a backwards compatibility
    // mistake for non-self-describing formats, and it's been reverted. Since some small number of
    // serialized bytestrings will probably exist forever in the wild, we should test that we can
    // still deserialize these from self-describing formats.
    let bytestring_cbor: &[u8] = &[
        0x58, 0x20, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe,
        0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe,
    ];
    let hash_from_bytestring_cbor: crate::Hash = ciborium::from_reader(bytestring_cbor).unwrap();
    assert_eq!(hash_from_bytestring_cbor, hash);
}

// The fork's unsafe paths on small inputs: the batch tables and padded
// lanes of many.rs, and the pool's jobs from concurrent callers. Each
// against crate::convenience::hash().
#[cfg(feature = "std")]
mod unsafe_paths {
    use crate::*;

    fn input(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u32).wrapping_mul(0x9E37_79B1).to_le_bytes()[3] ^ seed).collect()
    }

    #[test]
    fn test_batches_small() {
        for len in [0, 1, 64, 100, 128, 192, 1024, 1088, 2048, 3000] {
            for count in [0, 1, 2, 3, 4, 5, 9, 17] {
                let slot = crate::many::slot_len(len);
                let mut data = input(slot * count, len as u8);
                for message in data.chunks_exact_mut(slot) {
                    message[len..].fill(0);
                }
                let mut out = vec![[0u8; OUT_LEN]; count];
                crate::performance::hash_many(Mode::Plain, &crate::performance::Batch::in_place(&crate::test::blocks(&data), len), &mut out);
                for (i, digest) in out.iter().enumerate() {
                    assert_eq!(*digest, *crate::convenience::hash(&data[i * slot..][..len]).as_bytes(), "{count} x {len} B, message {i}");
                }
            }
        }
    }

    /// A batch and an input, from two threads at once through the
    /// multithreaded full forms.
    #[test]
    fn test_pool_from_two_callers() {
        let batch = input(1024 * 64, 1);
        let tree = input(65 * 1024 + 3, 2);
        let mut expected = vec![[0u8; OUT_LEN]; 1024];
        crate::performance::hash_many(Mode::Plain, &crate::performance::Batch::in_place(&crate::test::blocks(&batch), 64), &mut expected);
        let expected_tree = crate::convenience::hash(&tree);
        std::thread::scope(|scope| {
            for caller in 0..2 {
                let (batch, tree, expected) = (&batch, &tree, &expected);
                scope.spawn(move || {
                    let mut out = vec![[0u8; OUT_LEN]; 1024];
                    crate::performance::hash_many_multithreaded(Mode::Plain, &crate::performance::Batch::in_place(&crate::test::blocks(batch), 64), &mut out);
                    assert_eq!(&out, expected, "batch, caller {caller}");
                    assert_eq!(
                        crate::performance::hash_multithreaded(crate::Mode::Plain, &crate::test::message(tree), tree.len()),
                        expected_tree,
                        "tree, caller {caller}"
                    );
                });
            }
        });
    }
}

// Every kernel's reads and writes against guard pages: inputs placed to end
// exactly where an inaccessible page begins (and, again, to start right
// after one), outputs likewise, so a kernel that reads or writes one byte
// outside its buffers stops the test with a fault. The assembly kernels'
// accesses are invisible to the sanitizers; these are not.
#[cfg(all(feature = "std", any(target_arch = "aarch64", target_arch = "x86_64"), any(target_vendor = "apple", target_os = "linux")))]
mod guard_pages {
    use crate::*;

    /// `len` bytes with an inaccessible page before and after them, the
    /// bytes flush against the page after (`at_end`) or before.
    struct Guarded {
        base: *mut u8,
        total: usize,
        start: usize,
        len: usize,
    }

    impl Guarded {
        fn new(len: usize, at_end: bool) -> Self {
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let body = len.div_ceil(page).max(1) * page;
            let total = body + 2 * page;
            let base = unsafe { libc::mmap(core::ptr::null_mut(), total, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0) };
            assert_ne!(base, libc::MAP_FAILED, "mmap");
            let base = base as *mut u8;
            unsafe {
                assert_eq!(libc::mprotect(base as *mut _, page, libc::PROT_NONE), 0);
                assert_eq!(libc::mprotect(base.add(page + body) as *mut _, page, libc::PROT_NONE), 0);
            }
            let start = if at_end { page + body - len } else { page };
            Guarded { base, total, start, len }
        }

        fn bytes(&mut self) -> &mut [u8] {
            unsafe { core::slice::from_raw_parts_mut(self.base.add(self.start), self.len) }
        }
    }

    impl Drop for Guarded {
        fn drop(&mut self) {
            unsafe { libc::munmap(self.base as *mut _, self.total) };
        }
    }

    fn filled(len: usize, at_end: bool) -> Guarded {
        let mut g = Guarded::new(len, at_end);
        crate::test::paint_test_input(g.bytes());
        g
    }

    /// The one-chunk root kernels with the input flush against an
    /// inaccessible page, after it and before it: whole blocks are read in
    /// place, so a kernel that read a byte outside them would fault here.
    #[test]
    #[cfg(all(target_arch = "x86_64", blake3_sse41_ffi))]
    fn test_chunk_root_against_a_guard_page() {
        let mut variants = Vec::new();
        if platform::Platform::sse41().is_some() {
            variants.push(false);
        }
        #[cfg(blake3_avx512_ffi)]
        if platform::Platform::avx512().is_some() {
            variants.push(true);
        }
        let mut golden = crate::test::Golden::new("chunk_root_against_a_guard_page");
        for at_end in [true, false] {
            for len in [0, 1, 63, 64, 65, 127, 128, 960, 1023, 1024] {
                let mut g = filled(len, at_end);
                let input = g.bytes();
                let first = unsafe { crate::chunk_root::chunk_root(variants[0], input, IV, 0) };
                for &avx512 in &variants[1..] {
                    assert_eq!(unsafe { crate::chunk_root::chunk_root(avx512, input, IV, 0) }, first, "avx512={avx512} len={len} at_end={at_end}");
                }
                golden.add(&first, || crate::test::official::hash(crate::Mode::Plain, input).to_vec());
            }
        }
        golden.check(0x5372bd18346a3525);
    }

    /// Every platform's `hash_many` with each input in its own pages,
    /// flush against an inaccessible page: a kernel that reads past an
    /// input's end (into what, in a contiguous buffer, would be the next
    /// input) faults here: upstream's AVX2 and AVX-512 kernels did, at two
    /// to seven inputs past the groups of eight or sixteen (they loaded a
    /// whole vector and kept its first 16 bytes). 1 to 17 inputs of 1, 2
    /// and 16 blocks, at one counter and at incrementing ones, on every
    /// kernel this CPU runs.
    #[test]
    fn test_hash_many_each_input_against_a_guard_page() {
        fn check<const N: usize>(platforms: &[platform::Platform], increment: IncrementCounter, name: &'static str, want: u64) {
            let mut golden = crate::test::Golden::new(name);
            for count in 1..=17 {
                let mut pages: Vec<Guarded> = (0..count).map(|_| filled(N, true)).collect();
                let inputs: Vec<&[u8; N]> = pages.iter_mut().map(|g| -> &[u8; N] { unsafe { &*(g.bytes().as_ptr() as *const [u8; N]) } }).collect();
                let run = |platform: platform::Platform| {
                    let mut got = vec![0u8; count * OUT_LEN];
                    platform.hash_many::<N>(&inputs, IV, 0, increment, 0, CHUNK_START, CHUNK_END, &mut got);
                    got
                };
                let first = run(platforms[0]);
                for &platform in &platforms[1..] {
                    assert!(run(platform) == first, "{platform:?}, {count} inputs of {N} bytes");
                }
                golden.add(&first, || {
                    let mut theirs = vec![0u8; count * OUT_LEN];
                    crate::test::official::hash_many::<N>(&inputs, IV, 0, matches!(increment, IncrementCounter::Yes), 0, CHUNK_START, CHUNK_END, &mut theirs);
                    theirs
                });
            }
            golden.check(want);
        }
        #[allow(unused_mut)]
        let mut platforms: Vec<platform::Platform> = Vec::new();
        #[cfg(target_arch = "x86_64")]
        platforms.extend([platform::Platform::sse41(), platform::Platform::avx2()].into_iter().flatten());
        #[cfg(blake3_avx512_ffi)]
        platforms.extend(platform::Platform::avx512());
        #[cfg(blake3_neon)]
        platforms.extend(platform::Platform::neon());
        #[cfg(blake3_sme2)]
        platforms.extend(platform::Platform::sme2());
        // The kernels this CPU has are the ones checked: none left out.
        #[cfg(blake3_sme2)]
        assert_eq!(
            platforms.iter().any(|p| matches!(p, platform::Platform::SME2)),
            platform::sme2_detected(),
            "SME2 checked where the build and the CPU have it: {platforms:?}"
        );
        #[cfg(blake3_avx2_ffi)]
        {
            assert_eq!(
                platforms.iter().any(|p| matches!(p, platform::Platform::AVX2)),
                // The standard library's detection, not avx2_detected(): the
                // list comes from Platform::avx2(), that is avx2_detected(),
                // so comparing with it again would hold whatever it answered.
                std::is_x86_feature_detected!("avx2"),
                "AVX2 checked where the build and the CPU have it: {platforms:?}"
            );
            #[cfg(blake3_avx512_ffi)]
            assert_eq!(
                platforms.iter().any(|p| matches!(p, platform::Platform::AVX512)),
                std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512vl"),
                "AVX-512 checked where the build and the CPU have it: {platforms:?}"
            );
        }
        check::<BLOCK_LEN>(&platforms, IncrementCounter::No, "hash_many_guard_64", 0x6127895af4d255cf);
        check::<{ 2 * BLOCK_LEN }>(&platforms, IncrementCounter::No, "hash_many_guard_128", 0xebfbfdda86c4864b);
        check::<CHUNK_LEN>(&platforms, IncrementCounter::No, "hash_many_guard_1024", 0xe3228fd868305079);
        check::<CHUNK_LEN>(&platforms, IncrementCounter::Yes, "hash_many_guard_1024_inc", 0x5fc8b871ccbb233c);
    }

    #[test]
    fn test_hash_inside_guard_pages() {
        let mut lens: Vec<usize> = (0..=2 * CHUNK_LEN + 70).collect();
        for chunks in [3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 64, 100, 128, 256, 1024, 1025] {
            for delta in [-65i64, -64, -1, 0, 1, 63, 64, 65] {
                lens.push(((chunks * CHUNK_LEN) as i64 + delta) as usize);
            }
        }
        let mut golden = crate::test::Golden::new("hash_inside_guard_pages");
        for len in lens {
            for at_end in [true, false] {
                let mut g = filled(len, at_end);
                let data = g.bytes().to_vec();
                golden.add(crate::convenience::hash(g.bytes()).as_bytes(), || crate::test::official::hash(crate::Mode::Plain, &data).to_vec());
                if len >= 64 * CHUNK_LEN && len % 7 == 0 {
                    // The message's whole blocks flush against the page: the
                    // performance call may read to their end, no further.
                    let mut blocks = filled(len.div_ceil(64) * 64, at_end);
                    let message = blocks.bytes()[..len].to_vec();
                    let input = crate::performance::Block::from_bytes(blocks.bytes()).expect("a page-end region of whole blocks starts aligned");
                    assert_eq!(crate::performance::hash_multithreaded(crate::Mode::Plain, input, len), crate::convenience::hash(&message), "mt, {len} bytes");
                }
            }
        }
        golden.check(0x477cad8a62ac89d1);
    }

    #[test]
    fn test_hash_many_inside_guard_pages() {
        for len in [1, 63, 64, 100, 128, 192, 200, 256, 640, 1000, 1024, 1088, 2000, 2048, 3072, 4096, 4100, 8192, 15360, 15359, 16384] {
            for count in [1, 2, 3, 4, 5, 6, 7, 9, 10, 12, 15, 16, 17, 21, 24, 31, 33, 127, 128, 129] {
                for at_end in [true, false] {
                    let slot = crate::many::slot_len(len);
                    let mut input = filled(slot * count, at_end);
                    for message in input.bytes().chunks_exact_mut(slot) {
                        message[len..].fill(0);
                    }
                    let mut out = Guarded::new(count * OUT_LEN, !at_end);
                    let outputs: &mut [[u8; OUT_LEN]] = unsafe { core::slice::from_raw_parts_mut(out.bytes().as_mut_ptr() as *mut [u8; OUT_LEN], count) };
                    crate::performance::hash_many(
                        Mode::Plain,
                        &crate::performance::Batch::in_place(
                            crate::performance::Block::from_bytes(input.bytes()).expect("a page-end batch starts aligned"),
                            len,
                        ),
                        outputs,
                    );
                    let data = input.bytes().to_vec();
                    for (i, digest) in outputs.iter().enumerate() {
                        assert_eq!(*digest, *crate::convenience::hash(&data[i * slot..][..len]).as_bytes(), "{count} x {len} B, message {i}, at end {at_end}");
                    }
                    if slot * count >= 64 * CHUNK_LEN {
                        let (_page, mt) = digests(count, at_end);
                        crate::performance::hash_many_multithreaded(
                            Mode::Plain,
                            &crate::performance::Batch::in_place(
                                crate::performance::Block::from_bytes(input.bytes()).expect("a page-end batch starts aligned"),
                                len,
                            ),
                            mt,
                        );
                        assert_eq!(&mt[..], &outputs[..], "mt, {count} x {len} B");
                    }
                }
            }
        }
    }

    /// The collection and many-message entry points, each item in its own
    /// pages flush against a guard page (then flush after one): the calls
    /// that gather items from separate places into lanes read no byte
    /// outside any item.
    #[test]
    fn test_each_item_against_a_guard_page() {
        use crate::Mode;
        let mut lens: Vec<usize> = (0..=130).collect();
        lens.extend([255, 256, 1000, 1023, 1024, 1025, 1089, 1500, 2047, 2048, 3072, 3839, 4096, 7935, 8191, 16383, 16384, 16385, 65536 + 7]);
        let mut golden = crate::test::Golden::new("each_item_against_a_guard_page");
        for count in [1, 2, 3, 4, 5, 7, 8, 9, 16, 17, 33] {
            for &len in &lens {
                for at_end in [true, false] {
                    // Each item's whole blocks flush against a page of its own:
                    // a performance call may read to their end, no further.
                    let lens: Vec<usize> = (0..count).map(|i| len + (i % 3)).collect();
                    let guarded: Vec<Guarded> = lens.iter().map(|&l| filled(l.div_ceil(64).max(1) * 64, at_end)).collect();
                    let regions: Vec<&[u8]> = guarded.iter().map(|g| unsafe { core::slice::from_raw_parts(g.base.add(g.start), g.len) }).collect();
                    let items: Vec<&[u8]> = regions.iter().zip(&lens).map(|(r, &l)| &r[..l]).collect();
                    let in_blocks: Vec<(&[crate::performance::Block], usize)> = regions
                        .iter()
                        .zip(&lens)
                        .map(|(r, &l)| (crate::performance::Block::from_bytes(r).expect("a page-end region of whole blocks starts aligned"), l))
                        .collect();
                    let want: Vec<[u8; 32]> = items.iter().map(|m| *crate::convenience::hash(m).as_bytes()).collect();
                    golden.add(want.as_flattened(), || items.iter().flat_map(|m| crate::test::official::hash(Mode::Plain, m)).collect());
                    let (_page, got) = digests(count, !at_end);
                    crate::performance::hash_each(Mode::Plain, &in_blocks, got);
                    assert_eq!(got, &want[..], "hash_each_with, {count} x {len} B, at end {at_end}");
                    let (_page, got) = digests(count, at_end);
                    crate::performance::hash_each_multithreaded(Mode::Plain, &in_blocks, got);
                    assert_eq!(got, &want[..], "hash_each_multithreaded_with, {count} x {len} B, at end {at_end}");
                    let mut hashers: Vec<crate::Hasher> = (0..count).map(|_| crate::Hasher::new(crate::Mode::Plain)).collect();
                    let pieces: Vec<(usize, &[u8])> = items.iter().copied().enumerate().collect();
                    crate::performance::update_each(&mut hashers, &pieces);
                    for (i, h) in hashers.iter().enumerate() {
                        assert_eq!(*h.finalize().as_bytes(), want[i], "update_each, {count} x {len} B, at end {at_end}");
                    }
                    if count == 1 {
                        let mut h = crate::Hasher::new(crate::Mode::Plain);
                        for piece in items[0].chunks(1000) {
                            h.update(piece);
                        }
                        assert_eq!(*h.finalize().as_bytes(), want[0], "update in 1000 B pieces, {len} B");
                    }
                }
            }
        }
        golden.check(0x4cef5685667ec43d);
    }

    #[test]
    fn test_xof_inside_guard_pages() {
        // Extended output written flush against a guard page after the
        // buffer, then before it: the wide xof_many (SME2: groups of
        // sixteen blocks) writes no byte outside it, from aligned and
        // unaligned positions.
        let mut reference = vec![0u8; 5 * 1024 + 200];
        crate::Hasher::new(crate::Mode::Plain).update(b"guarded").finalize_xof().fill(&mut reference);
        for len in [1, 64, 1023, 1024, 1025, 1088, 2048, 3000, 4096, 5 * 1024] {
            for skip in [0usize, 64, 37] {
                for at_end in [true, false] {
                    let mut out = Guarded::new(len, at_end);
                    let mut reader = crate::Hasher::new(crate::Mode::Plain).update(b"guarded").finalize_xof();
                    reader.set_position(skip as u64);
                    reader.fill(out.bytes());
                    assert_eq!(out.bytes(), &reference[skip..][..len], "{len} B from {skip}, at end {at_end}");
                }
            }
        }
    }

    /// `count` digests' places flush against a guard page (after them, or
    /// before): a call writes no byte outside its outputs.
    fn places(count: usize, at_end: bool) -> (Guarded, &'static mut [Hash]) {
        let mut g = Guarded::new(count * OUT_LEN, at_end);
        // Sound: Hash is repr(transparent) over [u8; 32], align 1, and the
        // pages live until the Guarded drops, after every use here.
        let out = unsafe { core::slice::from_raw_parts_mut(g.bytes().as_mut_ptr() as *mut Hash, count) };
        (g, out)
    }

    /// The same places as digests' bytes, for the calls that write those.
    fn digests(count: usize, at_end: bool) -> (Guarded, &'static mut [[u8; OUT_LEN]]) {
        let mut g = Guarded::new(count * OUT_LEN, at_end);
        let out = unsafe { core::slice::from_raw_parts_mut(g.bytes().as_mut_ptr() as *mut [u8; OUT_LEN], count) };
        (g, out)
    }

    /// Every public call that reads a caller's bytes or writes into a
    /// caller's places, in every mode, its inputs and outputs each flush
    /// against a guard page, after them and then before: the convenience
    /// calls, the single-message performance calls, the Hasher's
    /// multithreaded update, finalize_each, the stream's three ways in
    /// with its digests, and the key and context themselves. With the
    /// batch and collection tests above, no call is left out.
    #[test]
    fn test_every_call_inside_guard_pages() {
        let lens =
            [0, 1, 63, 64, 65, 127, 128, 1023, 1024, 1025, 2047, 2048, 2049, 3839, 4470, 7935, 8192, 16383, 16384, 16385, 65543, 131073, 524291, (1 << 20) + 5];
        let mut golden = crate::test::Golden::new("every_call_inside_guard_pages");
        for at_end in [true, false] {
            let mut key_page = filled(KEY_LEN, at_end);
            // Sound: the page holds KEY_LEN bytes and outlives the modes.
            let key: &[u8; KEY_LEN] = unsafe { &*(key_page.bytes().as_ptr() as *const [u8; KEY_LEN]) };
            let context_text = "guard pages 2026-10-25 every call";
            let mut context_page = Guarded::new(context_text.len(), at_end);
            context_page.bytes().copy_from_slice(context_text.as_bytes());
            let context = core::str::from_utf8(context_page.bytes()).unwrap();
            let context_key = crate::performance::context_key(context);
            for (name, mode) in [("hash", Mode::Plain), ("keyed", Mode::Keyed(key)), ("derive key", Mode::DeriveKey(context))] {
                for len in lens {
                    let mut flush = filled(len, at_end);
                    let message = flush.bytes().to_vec();
                    let what = format!("{name}, {len} B, at end {at_end}");
                    // hash_with's digest and hash_xof's first 100 bytes, against
                    // the fixed values; every other call against these.
                    let want = *crate::convenience::hash_with(mode, flush.bytes()).as_bytes();
                    let mut xof = Guarded::new(100, !at_end);
                    crate::convenience::hash_xof(mode, flush.bytes()).fill(xof.bytes());
                    golden.add(&[&want[..], xof.bytes()].concat(), || {
                        [crate::test::official::hash(mode, &message).to_vec(), crate::test::official::xof(mode, &message, 0, 100)].concat()
                    });
                    if let Mode::DeriveKey(_) = mode {
                        let material = Mode::DeriveKeyWith(&context_key);
                        assert_eq!(*crate::convenience::hash_with(material, flush.bytes()).as_bytes(), want, "hash_with, its context hashed once, {what}");
                    }
                    // The message's whole blocks flush against the page.
                    let mut region = filled(len.div_ceil(64).max(1) * 64, at_end);
                    region.bytes()[..len].copy_from_slice(&message);
                    let blocks = crate::performance::Block::from_bytes(unsafe { core::slice::from_raw_parts(region.base.add(region.start), region.len) })
                        .expect("a page-end region of whole blocks starts aligned");
                    assert_eq!(*crate::performance::hash(mode, blocks, len).as_bytes(), want, "performance::hash, {what}");
                    assert_eq!(*crate::performance::hash_multithreaded(mode, blocks, len).as_bytes(), want, "hash_multithreaded, {what}");
                    let mut hasher = crate::Hasher::new(mode);
                    hasher.update_multithreaded(flush.bytes());
                    assert_eq!(*hasher.finalize().as_bytes(), want, "update_multithreaded, {what}");
                    let (_out_page, out) = digests(1, !at_end);
                    crate::performance::finalize_each(core::slice::from_ref(&hasher), &[0], out);
                    assert_eq!(out[0], want, "finalize_each, {what}");
                    // A stream: the message where it is, the message written into
                    // its rooms, and a batch of three, every digest into places
                    // flush against a page.
                    let count = 3;
                    let slot = crate::many::slot_len(len);
                    let mut batch = filled(slot * count, at_end);
                    for i in 0..count {
                        let m = &mut batch.bytes()[i * slot..][..slot];
                        m[..len].copy_from_slice(&message);
                        m[len..].fill(0);
                    }
                    let batch_blocks = crate::performance::Block::from_bytes(unsafe { core::slice::from_raw_parts(batch.base.add(batch.start), batch.len) })
                        .expect("a page-end batch starts aligned");
                    let batch_in_place = crate::performance::Batch::in_place(batch_blocks, len);
                    let (_page_one, one) = places(1, !at_end);
                    let (_page_with, with) = places(1, at_end);
                    let (_page_batch, batched) = places(count, !at_end);
                    let input: &[u8] = unsafe { core::slice::from_raw_parts(flush.base.add(flush.start), flush.len) };
                    crate::performance::stream(
                        mode,
                        |_| {},
                        |s| {
                            s.submit(blocks, len, &mut one[0]);
                            s.submit_with(
                                len,
                                |at, room| {
                                    room.copy_from_slice(&input[at..][..room.len()]);
                                    room.len()
                                },
                                &mut with[0],
                            );
                            s.submit_batch(&batch_in_place, batched);
                            s.finish();
                        },
                    );
                    assert_eq!(*one[0].as_bytes(), want, "stream submit, {what}");
                    assert_eq!(*with[0].as_bytes(), want, "stream submit_with, {what}");
                    assert!(batched.iter().all(|h| *h.as_bytes() == want), "stream submit_batch, {what}");
                }
            }
        }
        golden.check(0x9bfe90db92a75d95);
    }
}

/// The Hasher's stage: pieces of many lengths across 16 KiB boundaries,
/// update and update_multithreaded mixed, finalize midway and after more
/// input, and subtrees at input offsets, against one-shot hashes.
#[test]
#[cfg(feature = "std")]
fn test_hasher_gathers() {
    let mut input = vec![0u8; 300_000];
    paint_test_input(&mut input);
    let pieces = [1, 1448, 4096, 1448, 16384, 31, 16383, 65536, 16385, 7, 1000];
    for start in 0..pieces.len() {
        let mut hasher = crate::Hasher::new(crate::Mode::Plain);
        let mut done = 0;
        let mut k = start;
        while done < input.len() {
            let n = pieces[k % pieces.len()].min(input.len() - done);
            if k % 3 == 0 {
                hasher.update_multithreaded(&input[done..done + n]);
            } else {
                hasher.update(&input[done..done + n]);
            }
            done += n;
            k += 1;
            if k % 5 == 0 {
                assert_eq!(hasher.finalize(), crate::convenience::hash(&input[..done]), "after {done} bytes");
                assert_eq!(hasher.count(), done as u64);
            }
        }
        assert_eq!(hasher.finalize(), crate::convenience::hash(&input), "pieces from {start}");
    }
    // A subtree of 64 chunks at offset 64 KiB, in small pieces, against one update.
    let sub = &input[..64 * CHUNK_LEN];
    let mut whole = crate::Hasher::new(crate::Mode::Plain);
    whole.set_input_offset(64 * CHUNK_LEN as u64).update(sub);
    let mut pieced = crate::Hasher::new(crate::Mode::Plain);
    pieced.set_input_offset(64 * CHUNK_LEN as u64);
    for piece in sub.chunks(1448) {
        pieced.update(piece);
    }
    assert_eq!(pieced.finalize_non_root(), whole.finalize_non_root());
}

/// Hash's comparisons (constant time) and formats: equal and unequal
/// against a Hash, a 32-byte array, and a slice, and a slice of another
/// length; Display as the hex digest, Debug as Hash("hex").
#[test]
fn test_hash_comparisons_and_formats() {
    let a = crate::convenience::hash(b"a");
    let b = crate::convenience::hash(b"b");
    assert!(a == a && a != b);
    assert!(a == *a.as_bytes() && a != *b.as_bytes());
    assert!(a == a.as_bytes()[..] && a != b.as_bytes()[..] && a != a.as_bytes()[..31]);
    let hex = a.to_hex();
    assert_eq!(hex.as_str(), "17762fddd969a453925d65717ac3eea21320b66b54342fde15128d6caf21215f");
    #[cfg(feature = "std")]
    {
        assert_eq!(std::format!("{a}"), hex.as_str());
        assert_eq!(std::format!("{a:?}"), std::format!("Hash({:?})", hex.as_str()));
    }
}

/// A Hasher's Debug form names its flags and platform, and leaves out
/// its bytes and chaining values (they may be secret).
#[test]
#[cfg(feature = "std")]
fn test_debug_leaves_out_the_bytes() {
    let mut hasher = crate::Hasher::new(crate::Mode::Keyed(&[0xab; 32]));
    hasher.update(&[0xab; 70]);
    let shown = std::format!("{hasher:?}");
    assert!(shown.starts_with("Hasher {") && shown.contains("flags: 16") && shown.contains("platform"), "{shown}");
    assert!(!shown.contains("171"), "{shown}");
}

/// A Hasher as an io::Write: write takes the whole input, and the digest
/// is update's.
#[test]
#[cfg(feature = "std")]
fn test_hasher_io_write() {
    use std::io::Write;
    let mut input = [0u8; 3000];
    paint_test_input(&mut input);
    let mut hasher = crate::Hasher::new(crate::Mode::Plain);
    assert_eq!(hasher.write(&input[..1000]).unwrap(), 1000);
    hasher.write_all(&input[1000..]).unwrap();
    hasher.flush().unwrap();
    assert_eq!(hasher.finalize(), crate::convenience::hash(&input));
}

/// The dispatch, pinned by the kernels each call runs: for a one-shot
/// call and for batches, the set of assembly entry points a call records
/// changes exactly at the thresholds the code names. Each threshold is a
/// speed choice between correct paths, which the digest checks cannot
/// see, so this is the test that holds them where measurement put them.
/// On a CPU with SME2 (every kernel present); the recorded entry points
/// are the tests' (`ran_kernel`).
#[cfg(all(blake3_sme2, feature = "std"))]
mod dispatch {
    use crate::{CHUNK_LEN, RAN, RAN_ENTRIES, Ran};
    use std::collections::BTreeSet;

    /// The names of the entry points one call ran (again when another
    /// test held the SME2 turn or the call went after a pause).
    fn ran(call: &dyn Fn()) -> BTreeSet<String> {
        let names: Vec<(String, usize)> = crate::neon_hybrid::entries()
            .into_iter()
            .map(|(n, e)| (n, e as usize))
            .chain(crate::sme2::entries().into_iter().map(|(n, e)| (n.to_string(), e as usize)))
            .collect();
        // The startup self-test runs every kernel: before the record.
        crate::initialize();
        loop {
            RAN.with(|r| r.set(0));
            RAN_ENTRIES.with(|r| r.borrow_mut().clear());
            call();
            if RAN.with(|r| r.get()) & Ran::TurnTaken as u32 == 0 {
                let ran = RAN_ENTRIES.with(|r| r.borrow().clone());
                return names.iter().filter(|(_, e)| ran.contains(e)).map(|(n, _)| n.clone()).collect();
            }
        }
    }

    /// A call on NEON runs no SME2 kernel, however long its input: the
    /// turn hands SME2 out only to a call made on SME2 (on a CPU without
    /// SME2, handing it out would be an illegal instruction).
    #[test]
    fn test_neon_runs_no_sme2() {
        if !crate::platform::sme2_detected() {
            return;
        }
        let sme2: BTreeSet<usize> = crate::sme2::entries().into_iter().map(|(_, e)| e as usize).collect();
        let neon = crate::platform::Platform::neon().expect("NEON on AArch64");
        crate::initialize();
        RAN_ENTRIES.with(|r| r.borrow_mut().clear());
        let mut out = vec![0u8; 64 * crate::BLOCK_LEN];
        neon.xof_many(&[0; 8], &[0; crate::BLOCK_LEN], 64, 0, 0, &mut out);
        let ran = RAN_ENTRIES.with(|r| r.borrow().clone());
        assert!(ran.is_disjoint(&sme2), "NEON's extended output ran an SME2 kernel");
    }

    /// Extended output on NEON, by block count: eight blocks a call on
    /// x8, four on x4, and the tail of one or two blocks on x1, of three
    /// on x4 (neon_hybrid::xof_tail).
    #[test]
    fn test_xof_dispatch() {
        if !crate::platform::sme2_detected() {
            return;
        }
        let neon = crate::platform::Platform::neon().expect("NEON on AArch64");
        let at = |blocks: usize| {
            ran(&|| {
                let mut out = vec![0u8; blocks * crate::BLOCK_LEN];
                neon.xof_many(&[0; 8], &[0; crate::BLOCK_LEN], 64, 0, 0, &mut out);
            })
        };
        let cases: [(usize, &[&str]); 8] =
            [(1, &["x1"]), (2, &["x1"]), (3, &["x4"]), (4, &["x4"]), (5, &["x1", "x4"]), (7, &["x4"]), (8, &["x8"]), (11, &["x4", "x8"])];
        for (blocks, want) in cases {
            assert_eq!(at(blocks), names(want), "extended output of {blocks} blocks");
        }
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// One message, by length (a call after a pause may prefetch its code
    /// on the pair kernel, so each is made twice and the second counts):
    /// the integer compress kernel alone up to a chunk; the hybrid chunk and parent kernels above it; SME2's chunks
    /// from sixteen chunks (its parents too once a tree has them); the
    /// SME2 kernel with an integer lane from LANE_MIN_CHUNKS chunks.
    #[test]
    fn test_one_message_dispatch() {
        if !crate::platform::sme2_detected() {
            return;
        }
        let input = vec![7u8; 2 * crate::sme2::LANE_MIN_CHUNKS * CHUNK_LEN];
        let at = |len: usize| {
            let _ = crate::convenience::hash(&input[..len]);
            ran(&|| {
                let _ = crate::convenience::hash(&input[..len]);
            })
        };
        let lane = crate::sme2::LANE_MIN_CHUNKS * CHUNK_LEN;
        let cases: [(usize, &[&str]); 8] = [
            (CHUNK_LEN, &["c1"]),
            (CHUNK_LEN + 1, &["c1", "k1", "p1"]),
            (2 * CHUNK_LEN, &["c1", "k2"]),
            (16 * CHUNK_LEN - 1, &["c1", "k6", "p2", "p4", "p8", "q9"]),
            (16 * CHUNK_LEN, &["c1", "hash16_chunks", "p2", "p4", "p8"]),
            (lane - CHUNK_LEN, &["c1", "hash16_chunks", "hash16_parents", "k10", "k5", "p2", "p4", "p8"]),
            (lane, &["c1", "hash16_chunks", "hash16_parents", "sme2x2_chunks"]),
            (2 * lane, &["c1", "hash16_chunks", "hash16_parents", "sme2x2_chunks"]),
        ];
        for (len, want) in cases {
            assert_eq!(at(len), names(want), "hash of {len} bytes");
        }
    }

    /// Batches, by message length and count: one-block messages on the
    /// parent kernel of their count up to nine, two kernels at ten, SME2's
    /// message kernel from ONE_BLOCK_PAD_MIN (11) and a whole group's
    /// parents at sixteen; two-chunk messages on the plans up to six and
    /// SME2 from seven (sme2_chunked_min); 256-byte messages on the parent
    /// kernels up to nine and SME2 from ten (ONE_BLOCK_PAD_MIN's cousin,
    /// SME2_TAIL_MIN).
    #[test]
    fn test_batch_dispatch() {
        if !crate::platform::sme2_detected() {
            return;
        }
        let input = crate::test::blocks(&vec![3u8; 2 * CHUNK_LEN * 32]);
        let at = |len: usize, count: usize| {
            ran(&|| {
                let mut out = vec![[0u8; 32]; count];
                let batch = crate::performance::Batch::from_blocks(&input[..crate::many::slot_len(len) * count / crate::BLOCK_LEN], len);
                crate::performance::hash_many(crate::Mode::Plain, &batch, &mut out);
                crate::RAN_ENTRIES.with(|r| r.borrow_mut().clear());
                crate::performance::hash_many(crate::Mode::Plain, &batch, &mut out);
            })
        };
        assert_eq!(crate::many::ONE_BLOCK_PAD_MIN, 11);
        assert_eq!(crate::many::sme2_chunked_min(2 * CHUNK_LEN), 7);
        assert_eq!(crate::many::SME2_TAIL_MIN, 10);
        let cases: [(usize, usize, &[&str]); 15] = [
            (64, 2, &["p2"]),
            (64, 4, &["p4"]),
            (64, 9, &["p9"]),
            (64, 10, &["p3", "p7"]),
            (64, 11, &["hash16_messages"]),
            (64, 16, &["hash16_parents"]),
            (64, 17, &["hash16_parents", "k1", "p1"]),
            (64, 20, &["hash16_parents", "p4"]),
            (64, 21, &["hash16_messages"]),
            (2 * CHUNK_LEN, 6, &["p3"]),
            (2 * CHUNK_LEN, 7, &["hash16_messages", "hash16_parents"]),
            (2 * CHUNK_LEN, 17, &["c1", "hash16_messages", "hash16_parents", "k2"]),
            (256, 2, &["p2"]),
            (256, 9, &["p9"]),
            (256, 10, &["hash16_messages"]),
        ];
        for (len, count, want) in cases {
            assert_eq!(at(len, count), names(want), "{count} messages of {len} bytes");
        }
    }
}

/// Subtrees hashed apart and merged, the independent computation the
/// pool's and the Hasher's tests check themselves against: a Hasher
/// positioned at a subtree's offset, its chaining value, and the parent of
/// two chaining values (`hasher::parent_output` under a mode).
pub(crate) trait SubtreeHasher {
    /// Position the hasher at `offset` bytes into the whole input, before
    /// any input (a subtree's offset bounds its length: `max_subtree_len`).
    fn set_input_offset(&mut self, offset: u64) -> &mut Self;
    /// The chaining value of the chunk or subtree hashed so far.
    fn finalize_non_root(&self) -> CVBytes;
}

impl SubtreeHasher for crate::Hasher {
    fn set_input_offset(&mut self, offset: u64) -> &mut Self {
        assert_eq!(self.count(), 0, "hasher has already accepted input");
        self.core.set_input_offset(offset);
        self
    }

    fn finalize_non_root(&self) -> CVBytes {
        assert_ne!(self.count(), 0, "empty subtrees are never valid");
        self.final_output().chaining_value()
    }
}

/// The parent node of two chaining values under `mode`.
pub(crate) fn parent(left: &CVBytes, right: &CVBytes, mode: crate::Mode) -> crate::Output {
    let (key, flags) = mode.key_and_flags();
    crate::hasher::parent_output(left, right, &key, flags, crate::Platform::detect())
}

#[test]
#[should_panic]
fn test_empty_subtree_should_panic() {
    crate::Hasher::new(crate::Mode::Plain).finalize_non_root();
}

#[test]
#[should_panic]
fn test_unaligned_offset_should_panic() {
    crate::Hasher::new(crate::Mode::Plain).set_input_offset(1);
}

#[test]
#[should_panic]
fn test_hasher_already_accepted_input_should_panic() {
    crate::Hasher::new(crate::Mode::Plain).update(b"x").set_input_offset(0);
}

#[test]
#[should_panic]
fn test_too_much_input_should_panic() {
    crate::Hasher::new(crate::Mode::Plain).set_input_offset(CHUNK_LEN as u64).update(&[0; CHUNK_LEN + 1]);
}

#[test]
#[should_panic]
fn test_set_input_offset_cant_finalize() {
    crate::Hasher::new(crate::Mode::Plain).set_input_offset(CHUNK_LEN as u64).finalize();
}

#[test]
#[should_panic]
fn test_set_input_offset_cant_finalize_xof() {
    crate::Hasher::new(crate::Mode::Plain).set_input_offset(CHUNK_LEN as u64).finalize_xof();
}

/// Every test case hashed as subtrees of 1 to 32 chunks, merged layer by
/// layer, against the one-shot hash.
#[test]
fn test_grouped_hash() {
    const MAX_CHUNKS: usize = (TEST_CASES_MAX + 1) / CHUNK_LEN;
    let mut input_buf = [0; TEST_CASES_MAX];
    paint_test_input(&mut input_buf);
    for subtree_chunks in [1, 2, 4, 8, 16, 32] {
        let subtree_len = subtree_chunks * CHUNK_LEN;
        for &case in TEST_CASES {
            if case <= subtree_len {
                continue;
            }
            let input = &input_buf[..case];
            let expected_hash = crate::convenience::hash(input);
            let mut chaining_values = ArrayVec::<CVBytes, MAX_CHUNKS>::new();
            let mut subtree_offset = 0;
            while subtree_offset < input.len() {
                let take = core::cmp::min(subtree_len, input.len() - subtree_offset);
                let subtree_cv =
                    crate::Hasher::new(crate::Mode::Plain).set_input_offset(subtree_offset as u64).update(&input[subtree_offset..][..take]).finalize_non_root();
                chaining_values.push(subtree_cv);
                subtree_offset += take;
            }
            assert!(chaining_values.len() >= 2);
            while chaining_values.len() > 2 {
                let n = chaining_values.len();
                for i in 0..(n / 2) {
                    chaining_values[i] = parent(&chaining_values[2 * i], &chaining_values[2 * i + 1], crate::Mode::Plain).chaining_value();
                }
                if n % 2 == 1 {
                    chaining_values[n / 2] = chaining_values[n - 1];
                }
                chaining_values.truncate(n / 2 + n % 2);
            }
            assert_eq!(chaining_values.len(), 2);
            assert_eq!(
                expected_hash,
                parent(&chaining_values[0], &chaining_values[1], crate::Mode::Plain).root_hash(),
                "{case} bytes in subtrees of {subtree_chunks} chunks"
            );
        }
    }
}

/// The prefetches a call makes after a pause, at every length below the
/// prefetch limit and every batch count, on every platform: they look up
/// the kernels' plans by length and count, and a lookup out of range would
/// end the program. Called directly, since a pause (100 µs since this
/// thread's last call) is a timing the other tests leave to chance.
#[test]
#[cfg(all(blake3_neon, feature = "std"))]
fn test_prefetches_after_a_pause_every_length() {
    for platform in every_platform() {
        for len in (0..crate::PREFETCH_BELOW).step_by(17) {
            crate::prefetch_kernels(len, platform);
            crate::prefetch_update_kernels(len, platform);
        }
    }
    for count in 0..=4 * crate::PREFETCH_BATCH_BELOW {
        crate::neon_hybrid::prefetch_message_code(count);
    }
}

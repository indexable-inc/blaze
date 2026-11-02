//! FFI for wide AVX-512 kernels (including the private p16 experiment). Arithmetic, transposition,
//! counter setup and output stores are all in generated assembly.
//! The ABI matches the AArch64 kernel family; see the generator's contract.
use crate::{BLOCK_LEN, CVWords, IncrementCounter, OUT_LEN};

unsafe extern "C" {
    fn blake3_servil_avx512_k32(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
    fn blake3_servil_avx512_p32(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
    fn blake3_servil_avx512_p16(inputs: *const *const u8, blocks: u64, key: *const u32, counter: u64, packed_flags: u64, out: *mut u8);
}

/// Exactly 32 inputs, each 1..=16 whole blocks, and space for their CVs.
/// Caller has detected AVX-512F. Initial counter increments wrap as u64.
pub(crate) unsafe fn hash32<const N: usize>(
    inputs: &[&[u8; N]],
    key: &CVWords,
    counter: u64,
    increment: IncrementCounter,
    flags: u8,
    start: u8,
    end: u8,
    out: &mut [u8],
) {
    const {
        assert!(N > 0 && N <= 1024 && N % BLOCK_LEN == 0);
    }
    assert_eq!(inputs.len(), 32);
    assert!(out.len() >= 32 * OUT_LEN);
    type Kernel = unsafe extern "C" fn(*const *const u8, u64, *const u32, u64, u64, *mut u8);
    let kernel: Kernel = if increment.yes() { blake3_servil_avx512_k32 } else { blake3_servil_avx512_p32 };
    let packed = flags as u64 | (start as u64) << 8 | (end as u64) << 16 | (BLOCK_LEN as u64) << 24;
    crate::ran_kernel(kernel as *const ());
    // Sound: every input points at N readable bytes, the key has 8 words,
    // and the bounds above provide 32 outputs. The caller detected AVX-512.
    unsafe { kernel(inputs.as_ptr().cast(), (N / BLOCK_LEN) as u64, key.as_ptr(), counter, packed, out.as_mut_ptr()) };
}

/// Experimental fixed-counter entry. Same bounds and raw ABI as hash32,
/// but exactly sixteen inputs. Production experimentation is parents only.
/// Caller detected AVX-512F. Startup uses the same chained control as
/// the other wide entries, with a sixteen-output final case.
pub(crate) unsafe fn hash16<const N: usize>(inputs: &[&[u8; N]], key: &CVWords, counter: u64, flags: u8, start: u8, end: u8, out: &mut [u8]) {
    const {
        assert!(N > 0 && N <= 1024 && N % BLOCK_LEN == 0);
    }
    assert_eq!(inputs.len(), 16);
    assert!(out.len() >= 16 * OUT_LEN);
    let packed = flags as u64 | (start as u64) << 8 | (end as u64) << 16 | (BLOCK_LEN as u64) << 24;
    crate::ran_kernel(blake3_servil_avx512_p16 as *const ());
    // Sound: exactly sixteen readable inputs and room for sixteen CVs;
    // the caller supplies the ISA and the key's eight words.
    unsafe { blake3_servil_avx512_p16(inputs.as_ptr().cast(), (N / BLOCK_LEN) as u64, key.as_ptr(), counter, packed, out.as_mut_ptr()) };
}

// Filled from the independent portable compression callback in the
// reference-table test, never from these assembly kernels' output.
pub(crate) const SELF_TEST_EXPECTED: [u64; 3] = [0x8418db51e3b4a019, 0x675dde19182e7dd6, 0xc1b6cd25e25d91f8];

pub(crate) fn check_self_test(index: usize, folded: u64) {
    let entry = ["AVX-512 k32", "AVX-512 p32", "AVX-512 p16"][index];
    assert_eq!(folded, SELF_TEST_EXPECTED[index], "blake3-servil startup self-test failed: {entry}: this build or CPU computes wrong digests");
}

/// Three chained startup cases, without a 32 KiB input allocation. Distinct,
/// unaligned, overlapping windows fit in 512 bytes; all are keyed by the
/// previous case's output. Three blocks exercise first, middle and last.
pub(crate) fn self_test_chain(
    previous: &[u8; 32],
    mut hash: impl FnMut(&[&[u8; 192]], &CVWords, u64, IncrementCounter, &mut [u8]),
    mut check: impl FnMut(usize, u64),
) {
    let mut input = [0u8; 512];
    crate::self_test::fill(&mut input);
    input[..32].copy_from_slice(previous);
    let mut key = crate::platform::words_from_le_bytes_32(previous);
    let mut out = [0u8; 32 * OUT_LEN];
    for (index, (width, increment)) in [(32, IncrementCounter::Yes), (32, IncrementCounter::No), (16, IncrementCounter::No)].into_iter().enumerate() {
        let inputs: [&[u8; 192]; 32] = core::array::from_fn(|i| input[1 + 8 * i..][..192].try_into().unwrap());
        let out = &mut out[..width * OUT_LEN];
        hash(&inputs[..width], &key, u64::MAX - 15, increment, out);
        check(index, crate::self_test::fold(out));
        input[..32].copy_from_slice(&out[..32]);
        key = crate::platform::words_from_le_bytes_32(out[..32].try_into().unwrap());
    }
}

/// Caller detected AVX-512. Keep the additional startup workspace out of
/// the shared startup frame and off machines that do not run these entries.
#[inline(never)]
pub(crate) unsafe fn self_test(previous: &[u8; 32]) -> usize {
    let mut checked = 0;
    self_test_chain(
        previous,
        |inputs, key, counter, increment, out| unsafe {
            if inputs.len() == 16 {
                assert!(!increment.yes(), "the p16 self-test uses a fixed counter");
                hash16(inputs, key, counter, crate::KEYED_HASH, crate::CHUNK_START, crate::CHUNK_END, out);
            } else {
                hash32(inputs, key, counter, increment, crate::KEYED_HASH, crate::CHUNK_START, crate::CHUNK_END, out);
            }
        },
        |index, folded| {
            check_self_test(index, folded);
            checked += 1;
        },
    );
    checked
}

#[cfg(all(test, feature = "std"))]
pub(crate) fn parent_entry() -> *const () {
    blake3_servil_avx512_p16 as *const ()
}

#[cfg(all(test, feature = "std"))]
pub(crate) fn entries() -> [(&'static str, *const ()); 3] {
    [
        ("AVX-512 k32", blake3_servil_avx512_k32 as *const ()),
        ("AVX-512 p32", blake3_servil_avx512_p32 as *const ()),
        ("AVX-512 p16", blake3_servil_avx512_p16 as *const ()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn check<const N: usize>() {
        let mut data = vec![[0u8; N]; 32];
        let mut seed = 0xfedcba9876543210u64;
        for row in &mut data {
            for b in row {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *b = seed as u8;
            }
        }
        let refs: Vec<_> = data.iter().collect();
        for counter in [0, u32::MAX as u64 - 15, 1u64 << 32, u64::MAX - 15] {
            for inc in [IncrementCounter::No, IncrementCounter::Yes] {
                for (flags, start, end) in [(0, 1, 2), (16, 1, 10), (4, 0, 8), (32, 1, 2), (64, 1, 10), (0xa5, 0x5a, 0xff)] {
                    let key = [0x12345678, 0x87654321, 2, 3, 4, 5, 6, 0xfedcba98];
                    let mut got = [0xa5u8; 32 * OUT_LEN + 1];
                    unsafe {
                        hash32(&refs, &key, counter, inc, flags, start, end, &mut got);
                    }
                    if !inc.yes() {
                        let mut got16 = [0xa5u8; 16 * OUT_LEN + 1];
                        unsafe { hash16(&refs[..16], &key, counter, flags, start, end, &mut got16) };
                        assert_eq!(&got16[..16 * OUT_LEN], &got[..16 * OUT_LEN], "p16 N={N} counter={counter} flags={flags}");
                        assert_eq!(got16[16 * OUT_LEN], 0xa5, "p16 output canary");
                    }
                    for (i, input) in refs.iter().enumerate() {
                        let mut cv = key;
                        let c = counter.wrapping_add(if inc.yes() { i as u64 } else { 0 });
                        for (b, block) in input.chunks_exact(BLOCK_LEN).enumerate() {
                            crate::platform::Platform::SSE41.compress_in_place(
                                &mut cv,
                                block.try_into().unwrap(),
                                BLOCK_LEN as u8,
                                c,
                                flags | if b == 0 { start } else { 0 } | if b + 1 == N / BLOCK_LEN { end } else { 0 },
                            );
                        }
                        for w in 0..8 {
                            assert_eq!(&got[i * OUT_LEN + 4 * w..][..4], &cv[w].to_le_bytes(), "N={N} counter={c} flags={flags}");
                        }
                    }
                    assert_eq!(got[32 * OUT_LEN], 0xa5, "output canary");
                }
            }
        }
    }

    #[test]
    fn randomized_counters_flags_and_block_counts() {
        if !crate::platform::avx512_detected() {
            return;
        }
        check::<64>();
        check::<128>();
        check::<192>();
        check::<256>();
        check::<320>();
        check::<384>();
        check::<448>();
        check::<512>();
        check::<576>();
        check::<640>();
        check::<704>();
        check::<768>();
        check::<832>();
        check::<896>();
        check::<960>();
        check::<1024>();
    }

    #[test]
    fn parent16_unaligned_overlapping_output() {
        if !crate::platform::avx512_detected() {
            return;
        }
        for blocks in [1, 16] {
            let n = blocks * BLOCK_LEN;
            let mut input = vec![0u8; 1 + 16 * n];
            crate::test::paint_test_input(&mut input[1..]);
            let original = input.clone();
            let key = *crate::IV;
            let counter = u64::MAX;
            let mut want = [0u8; 16 * OUT_LEN];
            for i in 0..16 {
                let mut cv = key;
                for b in 0..blocks {
                    let at = 1 + i * n + b * BLOCK_LEN;
                    crate::platform::Platform::SSE41.compress_in_place(
                        &mut cv,
                        input[at..at + BLOCK_LEN].try_into().unwrap(),
                        BLOCK_LEN as u8,
                        counter,
                        16 | if b == 0 { 1 } else { 0 } | if b + 1 == blocks { 2 } else { 0 },
                    );
                }
                for w in 0..8 {
                    want[i * OUT_LEN + 4 * w..][..4].copy_from_slice(&cv[w].to_le_bytes());
                }
            }
            let pointers: [*const u8; 16] = core::array::from_fn(|i| input[1 + i * n..].as_ptr());
            unsafe {
                blake3_servil_avx512_p16(
                    pointers.as_ptr(),
                    blocks as u64,
                    key.as_ptr(),
                    counter,
                    16 | (1 << 8) | (2 << 16) | (64 << 24),
                    input.as_mut_ptr().add(1),
                )
            };
            assert_eq!(&input[1..1 + 16 * OUT_LEN], &want);
            assert_eq!(input[0], original[0]);
            assert_eq!(&input[1 + 16 * OUT_LEN..], &original[1 + 16 * OUT_LEN..]);
        }
    }

    #[test]
    fn short_last_blocks_unaligned_and_overlapping_output() {
        if !crate::platform::avx512_detected() {
            return;
        }
        for blocks in [1, 2, 16] {
            let n = blocks * BLOCK_LEN;
            for last_len in [0, 1, 31, 63, 64] {
                // Every input starts one byte past alignment. Compute the
                // reference first, then overwrite the input with the digests.
                let mut data = vec![0u8; 1 + 32 * n];
                for (i, b) in data[1..].iter_mut().enumerate() {
                    *b = ((i * 13 + i / 251) % 251) as u8;
                }
                for i in 0..32 {
                    data[1 + i * n + (blocks - 1) * BLOCK_LEN + last_len..1 + (i + 1) * n].fill(0);
                }
                for inc in [false, true] {
                    let mut input = data.clone();
                    let key = [11, 22, 33, 44, 55, 66, 77, 88];
                    let counter = u32::MAX as u64 - 15;
                    let mut want = [0u8; 32 * OUT_LEN];
                    for i in 0..32 {
                        let mut cv = key;
                        for b in 0..blocks {
                            let at = 1 + i * n + b * BLOCK_LEN;
                            crate::platform::Platform::SSE41.compress_in_place(
                                &mut cv,
                                input[at..at + BLOCK_LEN].try_into().unwrap(),
                                if b + 1 == blocks { last_len as u8 } else { BLOCK_LEN as u8 },
                                counter + if inc { i as u64 } else { 0 },
                                16 | if b == 0 { 1 } else { 0 } | if b + 1 == blocks { 10 } else { 0 },
                            );
                        }
                        for w in 0..8 {
                            want[i * OUT_LEN + 4 * w..][..4].copy_from_slice(&cv[w].to_le_bytes());
                        }
                    }
                    let pointers: [*const u8; 32] = core::array::from_fn(|i| input[1 + i * n..].as_ptr());
                    let packed = 16 | (1 << 8) | (10 << 16) | ((last_len as u64) << 24);
                    unsafe {
                        let kernel = if inc { blake3_servil_avx512_k32 } else { blake3_servil_avx512_p32 };
                        kernel(pointers.as_ptr(), blocks as u64, key.as_ptr(), counter, packed, input.as_mut_ptr().add(1));
                    }
                    assert_eq!(&input[1..1 + 32 * OUT_LEN], &want, "blocks={blocks} last_len={last_len} increment={inc}");
                    assert_eq!(input[0], data[0], "leading canary");
                    assert_eq!(&input[1 + 32 * OUT_LEN..], &data[1 + 32 * OUT_LEN..], "trailing input untouched");
                }
            }
        }
    }

    include!("ffi_avx512_32_guard_tests.rs");

    #[test]
    fn generated_assembly_matches() {
        let output = std::process::Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/gen_avx512_32.py"))
            .output()
            .expect("Python runs the assembly generator");
        assert!(output.status.success());
        assert_eq!(output.stdout, include_bytes!("../c/blake3_avx512_32.S"));
    }
}

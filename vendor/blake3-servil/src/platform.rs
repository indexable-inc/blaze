use crate::{BLOCK_LEN, CHUNK_LEN, CVWords, IncrementCounter};

// The most chunks one call hashes: AVX-512's 32 (16 off Linux), SME2's,
// NEON's 16. Every platform hashes at least four, so the walk's arrays
// (src/tree.rs) hold two values or more.
cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        pub const MAX_SIMD_DEGREE: usize = if cfg!(target_os = "linux") { 32 } else { 16 };
    } else if #[cfg(blake3_sme2)] {
        // See sme2::DEGREE.
        pub const MAX_SIMD_DEGREE: usize = crate::sme2::DEGREE;
    } else {
        // See neon_hybrid::GROUP.
        pub const MAX_SIMD_DEGREE: usize = 16;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
/// Each platform's discriminant is fixed, the same in every build (0 means
/// "not yet recorded" to [`Platform::detect`]); tools/verify's whole-call
/// proofs start from the value `initialize` records.
pub enum Platform {
    #[cfg(blake3_sse41_ffi)]
    SSE41 = 3,
    #[cfg(blake3_avx2_ffi)]
    AVX2 = 4,
    #[cfg(blake3_avx512_ffi)]
    #[cfg(target_arch = "x86_64")]
    AVX512 = 5,
    #[cfg(blake3_neon)]
    NEON = 6,
    #[cfg(blake3_sme2)]
    SME2 = 7,
}

/// One SME2 call at a time per process (all but always: two calls taking
/// the turn at the same instant can both get it): the turn a call takes
/// before it runs SME2 kernels, and the platform it runs on. A call finding
/// another SME2 call running takes NEON instead. Two SME2 threads of a process can
/// share one SME unit (macOS keeps a thread group on one P-cluster), each
/// at half speed or less, slower than NEON (n threads each hashing 1 MiB:
/// 0.170, 0.176, 0.322, 0.47 ns/B a thread on SME2 for n = 1, 2, 4, 8,
/// NEON 0.25 throughout); and SME2 calls running together sent the
/// process's other threads to E-cores on an M4 Max (4-10% of a sleeping
/// thread's following work; the turn took the benchmark's small cells'
/// E-core samples from 5.1% to none). Calls too small for the SME2 kernels
/// (`sme2_sized` false) leave the turn alone. The flag is a plain load and
/// store, no read-modify-write: two calls taking it at one instant can both
/// get it, which costs only what running SME2 at once always cost. Its
/// accepted costs (Zooko, September 2026): two programs sharing the machine
/// lose the cells where each held an SME unit, and their small batches
/// 6-16%.
pub(crate) struct Sme2Turn {
    platform: Platform,
    #[cfg(blake3_sme2)]
    held: bool,
}

/// The turn's flag, on a 128-byte line of its own (Apple's cache line), so
/// no other state's writes invalidate it.
#[cfg(blake3_sme2)]
#[repr(align(128))]
struct Busy(core::sync::atomic::AtomicBool);

#[cfg(blake3_sme2)]
static SME2_BUSY: Busy = Busy(core::sync::atomic::AtomicBool::new(false));

impl Sme2Turn {
    #[inline]
    pub(crate) fn take(platform: Platform, sme2_sized: bool) -> Self {
        #[cfg(blake3_sme2)]
        if sme2_sized && matches!(platform, Platform::SME2) {
            use core::sync::atomic::Ordering;
            // A load, and a store when the turn is free: two calls that
            // look at the same instant can both take it, which costs what
            // running SME2 at once always did, and nothing else depends on
            // the flag. An atomic swap cost 3-9% for a batch of 24
            // messages (on the VM, 282 ns against 273, and 9-10% in the
            // regression check); acquire and release ordering about 1 µs
            // per call beside the SME2 kernels (256 one-block messages:
            // 3.43 µs against 2.50).
            let held = !SME2_BUSY.0.load(Ordering::Relaxed);
            if held {
                SME2_BUSY.0.store(true, Ordering::Relaxed);
            } else {
                crate::ran(crate::Ran::TurnTaken);
            }
            return Sme2Turn { platform: if held { Platform::SME2 } else { Platform::NEON }, held };
        }
        let _ = sme2_sized;
        Sme2Turn {
            platform,
            #[cfg(blake3_sme2)]
            held: false,
        }
    }

    #[inline]
    pub(crate) fn platform(&self) -> Platform {
        self.platform
    }
}

impl Drop for Sme2Turn {
    #[inline]
    fn drop(&mut self) {
        #[cfg(blake3_sme2)]
        if self.held {
            SME2_BUSY.0.store(false, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The platform `initialize` detected, as its discriminant (1 and up), or 0
/// before `initialize` has run.
static DETECTED: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// A call before `initialize`: the caller broke the contract. The crate's
/// own unit tests, many small programs in one, initialize here instead.
#[cold]
#[inline(never)]
fn not_initialized() -> u8 {
    #[cfg(test)]
    {
        crate::initialize();
        DETECTED.load(core::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(test))]
    panic!("blake3-servil: call blake3_servil::initialize() once, before any other call: it runs the startup self-test and detects the CPU")
}

/// x86-64's kernels for a CPU with the given features: the widest it
/// runs, or None for a CPU without SSE4.1, which is not supported.
#[cfg(all(target_arch = "x86_64", blake3_sse41_ffi, blake3_avx2_ffi, blake3_avx512_ffi))]
const fn x86_platform(avx512: bool, avx2: bool, sse41: bool) -> Option<Platform> {
    if avx512 {
        Some(Platform::AVX512)
    } else if avx2 {
        Some(Platform::AVX2)
    } else if sse41 {
        Some(Platform::SSE41)
    } else {
        None
    }
}

impl Platform {
    /// The platform [`initialize`](crate::initialize) detected for this
    /// process. The caller must have called `initialize` (or
    /// `initialize_multithreaded`) before: a call before it panics, in every
    /// build. The check costs nothing extra: it is the load of the platform,
    /// which every call makes anyway.
    #[inline]
    pub fn detect() -> Self {
        let mut code = DETECTED.load(core::sync::atomic::Ordering::Relaxed);
        if code == 0 {
            code = not_initialized();
        }
        // Sound: DETECTED holds 0 or a Platform's discriminant (`record`).
        unsafe { core::mem::transmute::<u8, Platform>(code) }
    }

    /// Make `self` what [`Platform::detect`] returns from now on, in every
    /// thread: `initialize` does, once its self-test has passed.
    pub(crate) fn record(self) {
        DETECTED.store(self as u8, core::sync::atomic::Ordering::Relaxed);
    }

    /// [`Platform::detect`] without the startup self-test: an assembly
    /// platform on every supported machine, or the process stops.
    #[allow(unreachable_code)]
    pub(crate) fn detect_unchecked() -> Self {
        // Every x86-64 build assembles every kernel (build.rs), so a CPU
        // runs the widest it has; one without SSE4.1 is not supported, and
        // the process stops here, in the startup self-test, as a build for
        // an unsupported target stops (official BLAKE3 runs there).
        #[cfg(all(target_arch = "x86_64", blake3_sse41_ffi, blake3_avx2_ffi, blake3_avx512_ffi))]
        match x86_platform(avx512_detected(), avx2_detected(), sse41_detected()) {
            Some(platform) => return platform,
            None => crate::self_test::unsupported(format_args!(
                "blake3-servil runs on x86-64 CPUs with SSE4.1 (Intel since 2008, AMD since 2011); this CPU lacks it: use official BLAKE3 (https://github.com/BLAKE3-team/BLAKE3)"
            )),
        }
        // SME2 is detected at runtime; the kernels also require a 512-bit
        // streaming vector length, which sme2_detected() checks. Without
        // SME2 the NEON backend (integer + vector hybrid kernels) is the
        // selection, and it is a real result in its own right; a benchmark
        // that cares which one ran reads it back from detect().
        #[cfg(blake3_sme2)]
        {
            if sme2_detected() {
                return Platform::SME2;
            }
        }
        // We don't use dynamic feature detection for NEON. If the "neon"
        // feature is on, NEON is assumed to be supported.
        #[cfg(blake3_neon)]
        {
            return Platform::NEON;
        }
        unreachable!("build.rs builds for x86-64 and AArch64 alone")
    }

    /// The selection's name, for reports.
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(blake3_sse41_ffi)]
            Platform::SSE41 => "SSE4.1",
            #[cfg(blake3_avx2_ffi)]
            Platform::AVX2 => "AVX2",
            #[cfg(blake3_avx512_ffi)]
            #[cfg(target_arch = "x86_64")]
            Platform::AVX512 => "AVX-512",
            #[cfg(blake3_neon)]
            Platform::NEON => "NEON",
            #[cfg(blake3_sme2)]
            Platform::SME2 => "SME2",
        }
    }

    pub fn simd_degree(&self) -> usize {
        let degree = match self {
            #[cfg(blake3_sse41_ffi)]
            Platform::SSE41 => 4,
            #[cfg(blake3_avx2_ffi)]
            Platform::AVX2 => 8,
            #[cfg(blake3_avx512_ffi)]
            #[cfg(target_arch = "x86_64")]
            Platform::AVX512 => {
                if cfg!(target_os = "linux") {
                    32
                } else {
                    16
                }
            }
            // Sixteen chunks per call reach the k10 kernel (see
            // neon_hybrid), in both its families.
            #[cfg(blake3_neon)]
            Platform::NEON => 16,
            #[cfg(blake3_sme2)]
            Platform::SME2 => crate::sme2::DEGREE,
        };
        debug_assert!((2..=MAX_SIMD_DEGREE).contains(&degree), "the walk (src/tree.rs) takes 2 to MAX_SIMD_DEGREE chunks a call");
        degree
    }

    pub fn compress_in_place(&self, cv: &mut CVWords, block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8) {
        match self {
            // Safe because detect() checked for platform support.
            #[cfg(blake3_sse41_ffi)]
            Platform::SSE41 | Platform::AVX2 => unsafe { crate::sse41::compress_in_place(cv, block, block_len, counter, flags) },
            // Safe because detect() checked for platform support.
            #[cfg(blake3_avx512_ffi)]
            #[cfg(target_arch = "x86_64")]
            Platform::AVX512 => unsafe { crate::avx512::compress_in_place(cv, block, block_len, counter, flags) },
            // The scalar kernel k1 keeps the state in registers; it runs on
            // every AArch64 core, so the NEON and SME2 platforms share it.
            #[cfg(blake3_neon)]
            Platform::NEON => crate::neon_hybrid::compress_in_place(cv, block, block_len, counter, flags),
            #[cfg(blake3_sme2)]
            Platform::SME2 => crate::neon_hybrid::compress_in_place(cv, block, block_len, counter, flags),
        }
    }

    /// Compress `count` whole blocks of one chunk into `cv`: the first block
    /// with `flags | flags_start`, the rest with `flags`, all with `counter`.
    /// `blocks` holds at least `count * BLOCK_LEN` bytes; `count` is 1..=16.
    pub fn compress_blocks(&self, cv: &mut CVWords, blocks: &[u8], count: usize, counter: u64, flags: u8, flags_start: u8) {
        assert!((1..=16).contains(&count));
        assert!(blocks.len() >= count * BLOCK_LEN);
        // The integer-only kernel runs on every AArch64 core.
        #[cfg(blake3_neon)]
        {
            unsafe {
                crate::neon_hybrid::compress_blocks(cv, blocks.as_ptr(), count, counter, flags, flags_start);
            }
            return;
        }
        #[allow(unreachable_code)]
        {
            let mut block_flags = flags | flags_start;
            for block in blocks[..count * BLOCK_LEN].chunks_exact(BLOCK_LEN) {
                self.compress_in_place(cv, block.try_into().unwrap(), BLOCK_LEN as u8, counter, block_flags);
                block_flags = flags;
            }
        }
    }

    pub fn compress_xof(&self, cv: &CVWords, block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8) -> [u8; 64] {
        match self {
            // Safe because detect() checked for platform support.
            #[cfg(blake3_sse41_ffi)]
            Platform::SSE41 | Platform::AVX2 => unsafe { crate::sse41::compress_xof(cv, block, block_len, counter, flags) },
            // Safe because detect() checked for platform support.
            #[cfg(blake3_avx512_ffi)]
            #[cfg(target_arch = "x86_64")]
            Platform::AVX512 => unsafe { crate::avx512::compress_xof(cv, block, block_len, counter, flags) },
            // The one-block extended-output kernel (neon_hybrid::xof_block).
            #[cfg(blake3_neon)]
            Platform::NEON => crate::neon_hybrid::xof_block(cv, block, block_len, counter, flags),
            #[cfg(blake3_sme2)]
            Platform::SME2 => crate::neon_hybrid::xof_block(cv, block, block_len, counter, flags),
        }
    }

    // IMPLEMENTATION NOTE
    // ===================
    // hash_many() applies two optimizations. The critically important
    // optimization is the high-performance parallel SIMD hashing mode,
    // described in detail in the spec. This more than doubles throughput per
    // thread. Another optimization is keeping the state vectors transposed
    // from block to block within a chunk. When state vectors are transposed
    // after every block, there's a small but measurable performance loss.
    // Compressing chunks with a dedicated loop avoids this.

    pub fn hash_many<const N: usize>(
        &self,
        inputs: &[&[u8; N]],
        key: &CVWords,
        counter: u64,
        increment_counter: IncrementCounter,
        flags: u8,
        flags_start: u8,
        flags_end: u8,
        out: &mut [u8],
    ) {
        // Every kernel hashes whole blocks: an input of another length
        // would lose its tail, so it cannot compile.
        const { assert!(N > 0 && N % BLOCK_LEN == 0, "hash_many inputs are whole blocks") };
        // The shapes the crate makes, which every kernel takes: whole chunks
        // at counters counting up, or 1 to 16 whole blocks at one counter.
        assert!(
            if increment_counter.yes() { N == CHUNK_LEN } else { N <= CHUNK_LEN },
            "hash_many takes whole chunks at incrementing counters, or 1 to 16 blocks at one counter"
        );
        match self {
            // Safe because detect() checked for platform support.
            #[cfg(blake3_sse41_ffi)]
            Platform::SSE41 => unsafe { crate::sse41::hash_many(inputs, key, counter, increment_counter, flags, flags_start, flags_end, out) },
            // Safe because detect() checked for platform support.
            #[cfg(blake3_avx2_ffi)]
            Platform::AVX2 => unsafe { crate::avx2::hash_many(inputs, key, counter, increment_counter, flags, flags_start, flags_end, out) },
            // Safe because detect() checked for platform support.
            #[cfg(blake3_avx512_ffi)]
            #[cfg(target_arch = "x86_64")]
            Platform::AVX512 => unsafe { crate::avx512::hash_many(inputs, key, counter, increment_counter, flags, flags_start, flags_end, out) },
            // Assumed to be safe if the "neon" feature is on.
            #[cfg(blake3_neon)]
            Platform::NEON => unsafe { crate::neon_hybrid::hash_many(inputs, key, counter, increment_counter, flags, flags_start, flags_end, out) },
            // Safe because detect() checked for SME2 with a 512-bit vector length.
            #[cfg(blake3_sme2)]
            Platform::SME2 => unsafe { crate::sme2::hash_many(inputs, key, counter, increment_counter, flags, flags_start, flags_end, out) },
        }
    }

    pub fn xof_many(&self, cv: &CVWords, block: &[u8; BLOCK_LEN], block_len: u8, mut counter: u64, flags: u8, out: &mut [u8]) {
        debug_assert_eq!(0, out.len() % BLOCK_LEN, "whole blocks only");
        if out.is_empty() {
            // The current assembly implementation always outputs at least 1 block.
            return;
        }
        match self {
            // Safe because detect() checked for platform support.
            #[cfg(blake3_avx512_ffi)]
            #[cfg(unix)]
            #[cfg(target_arch = "x86_64")]
            Platform::AVX512 => unsafe { crate::avx512::xof_many(cv, block, block_len, counter, flags, out) },
            _ => {
                // On SME2, whole groups of sixteen blocks on the extended-
                // output kernel, under the process's turn; the rest below.
                #[cfg(blake3_sme2)]
                let out = {
                    let turn = Sme2Turn::take(*self, out.len() >= 16 * BLOCK_LEN);
                    let done = if matches!(turn.platform(), Platform::SME2) {
                        // Safe: the turn's platform is SME2 only where detect() found it.
                        unsafe { crate::sme2::xof_many(cv, block, block_len, counter, flags, out) }
                    } else {
                        0
                    };
                    counter += (done / BLOCK_LEN) as u64;
                    &mut out[done..]
                };
                // Then four or eight at a time on NEON (AArch64): CPUs
                // without SME2, and SME2's leftovers; the last one to three
                // blocks after them (neon_hybrid::xof_tail).
                #[cfg(blake3_neon)]
                {
                    let done = crate::neon_hybrid::xof_many(cv, block, block_len, counter, flags, out);
                    if done < out.len() {
                        crate::neon_hybrid::xof_tail(cv, block, block_len, counter.wrapping_add((done / BLOCK_LEN) as u64), flags, &mut out[done..]);
                    }
                }
                // SSE4.1 and AVX2, without an xof_many kernel: block by block.
                #[cfg(target_arch = "x86_64")]
                for out_block in out.chunks_exact_mut(BLOCK_LEN) {
                    // TODO: Use array_chunks_mut here once that's stable.
                    let out_array: &mut [u8; BLOCK_LEN] = out_block.try_into().unwrap();
                    *out_array = self.compress_xof(cv, block, block_len, counter, flags);
                    counter += 1;
                }
            }
        }
    }

    // Explicit platform constructors, for the tests (std).

    /// The SSE41 platform where this build has its kernels and the CPU its
    /// instructions.
    #[cfg(test)]
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    pub fn sse41() -> Option<Self> {
        #[cfg(blake3_sse41_ffi)]
        if sse41_detected() {
            return Some(Self::SSE41);
        }
        None
    }

    /// The AVX2 platform where this build has its kernels and the CPU its
    /// instructions.
    #[cfg(test)]
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    pub fn avx2() -> Option<Self> {
        #[cfg(blake3_avx2_ffi)]
        if avx2_detected() {
            return Some(Self::AVX2);
        }
        None
    }

    #[cfg(blake3_avx512_ffi)]
    #[cfg(target_arch = "x86_64")]
    #[cfg(test)]
    #[cfg(feature = "std")]
    pub fn avx512() -> Option<Self> {
        if avx512_detected() { Some(Self::AVX512) } else { None }
    }

    #[cfg(blake3_neon)]
    #[cfg(test)]
    pub fn neon() -> Option<Self> {
        // Assumed to be safe if the "neon" feature is on.
        Some(Self::NEON)
    }

    #[cfg(blake3_sme2)]
    #[cfg(all(test, feature = "std"))]
    pub fn sme2() -> Option<Self> {
        if sme2_detected() { Some(Self::SME2) } else { None }
    }
}

/// The x86-64 features the platforms check, as bits: [`SSE41`], [`AVX2`],
/// [`AVX512`] (F and VL, the two AVX-512 featuresets the kernels use), and
/// [`READ`] once the bits are read. Every check reads them from CPUID once
/// per process: CPUID costs about 640 ns (i7-12700K), and cpufeatures read
/// three leaves per feature checked, six on an AVX2 machine's first
/// detection where two serve. A feature the build assumes needs none.
#[cfg(target_arch = "x86_64")]
mod x86_bits {
    pub const SSE41: u8 = 2;
    pub const AVX2: u8 = 4;
    pub const AVX512: u8 = 8;
    /// AVX-512 on a CPU where the 32-input kernels pay ([`super::wide_cpu`]).
    pub const WIDE: u8 = 16;
    pub const READ: u8 = 128;
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
// Read by the x86 kernels' dispatch and the tests.
#[cfg_attr(not(blake3_sse41_ffi), allow(dead_code))]
fn x86_features() -> u8 {
    use core::sync::atomic::{AtomicU8, Ordering};
    static FEATURES: AtomicU8 = AtomicU8::new(0);
    let features = FEATURES.load(Ordering::Relaxed);
    if features != 0 {
        return features;
    }
    let features = read_x86_features();
    FEATURES.store(features, Ordering::Relaxed);
    features
}

/// [`x86_features`] from CPUID: leaf 1, and, where the CPU has AVX and the
/// OS saves its registers (XSAVE, OSXSAVE, AVX, then XCR0), leaf 7. Every
/// CPU with AVX has leaf 7.
#[cfg(target_arch = "x86_64")]
#[cold]
// Read by the x86 kernels' dispatch and the tests.
#[cfg_attr(not(blake3_sse41_ffi), allow(dead_code))]
fn read_x86_features() -> u8 {
    use core::arch::x86_64 as arch;
    use x86_bits::*;
    // Sound: every x86-64 CPU has CPUID. (Safe since Rust 1.94; unsafe
    // before.)
    #[allow(unused_unsafe)]
    let leaf1 = unsafe { arch::__cpuid(1) };
    let mut features = READ;
    if leaf1.ecx & (1 << 19) != 0 {
        features |= SSE41;
    }
    let avx = (1 << 26) | (1 << 27) | (1 << 28);
    if leaf1.ecx & avx == avx {
        // Sound: OSXSAVE (bit 27) says the OS enabled XGETBV.
        let xcr0 = unsafe { arch::_xgetbv(0) };
        // Sound: as leaf 1; leaf 7 exists where AVX does.
        #[allow(unused_unsafe)]
        let leaf7 = unsafe { arch::__cpuid_count(7, 0) };
        let (ymm, zmm) = (0b110, 0b1110_0110);
        if xcr0 & ymm == ymm && leaf7.ebx & (1 << 5) != 0 {
            features |= AVX2;
        }
        let f_vl = (1 << 16) | (1 << 31);
        if xcr0 & zmm == zmm && leaf7.ebx & f_vl == f_vl {
            features |= AVX512;
            // Sound: as leaf 1; leaf 0 names the vendor.
            #[allow(unused_unsafe)]
            let leaf0 = unsafe { arch::__cpuid(0) };
            let amd = (leaf0.ebx, leaf0.edx, leaf0.ecx) == (0x6874_7541, 0x6974_6e65, 0x444d_4163); // "AuthenticAMD"
            if wide_cpu(amd, family(leaf1.eax)) {
                features |= WIDE;
            }
        }
    }
    features
}

/// The display family of CPUID leaf 1's EAX: the base family, plus the
/// extended family where the base is 0xF.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(blake3_sse41_ffi), allow(dead_code))]
fn family(eax: u32) -> u32 {
    let base = (eax >> 8) & 0xF;
    if base == 0xF { base + ((eax >> 20) & 0xFF) } else { base }
}

/// Whether the 32-input AVX-512 kernels pay on this CPU: AMD from family
/// 0x1A (Zen 5). Measured against the 16-input kernel for one thread on
/// inputs of 32 KiB and more (BLAKE3 #125): about 20% less time on Zen 5
/// (EPYC 9255), but on Zen 4 (EPYC 9124, family 0x19) 4-5% more from
/// memory, and on Intel Ice Lake (Xeon Gold 6330) 3-4.5% more. Other
/// CPUs join the list when measured.
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(blake3_sse41_ffi), allow(dead_code))]
fn wide_cpu(amd: bool, family: u32) -> bool {
    amd && family >= 0x1A
}

/// Whether `hash_many` dispatches to the 32-input AVX-512 kernels
/// (Linux): AVX-512 on a CPU where they pay ([`wide_cpu`]).
#[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn wide_avx512() -> bool {
    // The bit comes only with AVX-512 (read_x86_features).
    x86_features() & x86_bits::WIDE != 0
}

#[cfg(blake3_avx512_ffi)]
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn avx512_detected() -> bool {
    // A build for CPUs that have it (`-C target-cpu=...` or
    // `-C target-feature=...`) needs no check: run elsewhere, it stops
    // at its first such instruction (SIGILL), in the startup self-test.
    if cfg!(all(target_feature = "avx512f", target_feature = "avx512vl")) {
        return true;
    }

    x86_features() & x86_bits::AVX512 != 0
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
// Read by the x86 kernels' dispatch and the tests.
#[cfg_attr(not(blake3_sse41_ffi), allow(dead_code))]
pub fn avx2_detected() -> bool {
    // A build for CPUs that have it (`-C target-cpu=...` or
    // `-C target-feature=...`) needs no check: run elsewhere, it stops
    // at its first such instruction (SIGILL), in the startup self-test.
    if cfg!(target_feature = "avx2") {
        return true;
    }

    x86_features() & x86_bits::AVX2 != 0
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
// Read by the x86 kernels' dispatch and the tests.
#[cfg_attr(not(blake3_sse41_ffi), allow(dead_code))]
pub fn sse41_detected() -> bool {
    // A build for CPUs that have it (`-C target-cpu=...` or
    // `-C target-feature=...`) needs no check: run elsewhere, it stops
    // at its first such instruction (SIGILL), in the startup self-test.
    if cfg!(target_feature = "sse4.1") {
        return true;
    }

    x86_features() & x86_bits::SSE41 != 0
}

/// The core's virtual counter (CNTVCT_EL0, which user code reads
/// directly): control, not measurement.
#[cfg(all(blake3_neon, feature = "std"))]
#[inline]
pub(crate) fn counter() -> u64 {
    let now: u64;
    // Sound: readable at EL0 on every AArch64 OS this crate builds the
    // hybrids for (Linux, macOS).
    unsafe { core::arch::asm!("mrs {0}, cntvct_el0", out(reg) now, options(nomem, nostack, preserves_flags)) };
    now
}

/// The counter's ticks in 100 us (CNTFRQ_EL0 / 10,000: 2400 on Apple's
/// cores), read once: reading the frequency costs about 10 cycles on an M4
/// Max (probe/stamp-cost, job 1015), the counter itself next to nothing.
#[cfg(all(blake3_neon, feature = "std"))]
#[inline]
pub(crate) fn ticks_in_100_us() -> u64 {
    static TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let ticks = TICKS.load(core::sync::atomic::Ordering::Relaxed);
    if ticks != 0 {
        return ticks;
    }
    let per_second: u64;
    // Sound: as counter().
    unsafe { core::arch::asm!("mrs {0}, cntfrq_el0", out(reg) per_second, options(nomem, nostack, preserves_flags)) };
    let ticks = (per_second / 10_000).max(1);
    TICKS.store(ticks, core::sync::atomic::Ordering::Relaxed);
    ticks
}

/// Prefetch code from `start` to `end` into the L2 cache, a line (128
/// bytes on Apple's cores) at a time. A call after other work otherwise
/// fetches its kernel's lines from DRAM one miss after another: hash() of
/// 8 KiB took 4.8 us so on an M4 Max, and 2.8 with this prefetch (job 971),
/// where code already near costs it a few cycles a line.
#[cfg(all(blake3_neon, feature = "std"))]
#[inline]
pub(crate) fn prefetch_code((start, end): (usize, usize)) {
    debug_assert!(start < end && end - start < 64 * 1024, "a kernel's code");
    // Four lines a step (past `end` by up to three: harmless).
    let mut line = start;
    while line < end {
        // Sound: a prefetch never faults, whatever the address.
        unsafe {
            core::arch::asm!(
                "prfm pldl2keep, [{0}]",
                "prfm pldl2keep, [{0}, #128]",
                "prfm pldl2keep, [{0}, #256]",
                "prfm pldl2keep, [{0}, #384]",
                in(reg) line,
                options(nostack, readonly, preserves_flags),
            )
        };
        line += 512;
    }
}

// The little-endian words of a block or a key, and back: 4-byte chunks
// (as_chunks), with no index arithmetic to get wrong (each unrolled form's
// 16 lines of `bytes[i * 4..]` were 16 places for a mutant; October 26).
#[inline(always)]
pub fn words_from_le_bytes_32(bytes: &[u8; 32]) -> [u32; 8] {
    let mut out = [0; 8];
    for (word, chunk) in out.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    out
}

/// True when the CPU reports SME2 and the streaming vector length is 512
/// bits, the only length the SME2 kernels support. Detection is per
/// platform: `hw.optional.arm.FEAT_SME2` on Apple systems and
/// `HWCAP2_SME2` on Linux.
///
/// The answer is a property of the machine, so it is computed once and
/// cached. The vector-length check enters and leaves streaming mode, which
/// costs about as much as hashing 64 bytes; doing it on every call would
/// slow small hashes by a third.
#[cfg(blake3_sme2)]
#[inline(always)]
pub fn sme2_detected() -> bool {
    use core::sync::atomic::{AtomicU8, Ordering};

    // 0 = unknown, 1 = no, 2 = yes.
    static CACHE: AtomicU8 = AtomicU8::new(0);

    match CACHE.load(Ordering::Relaxed) {
        2 => return true,
        1 => return false,
        _ => {}
    }

    let detected = sme2_detect_uncached();
    CACHE.store(if detected { 2 } else { 1 }, Ordering::Relaxed);
    detected
}

#[cfg(blake3_sme2)]
fn sme2_detect_uncached() -> bool {
    if !sme2_reported() {
        return false;
    }

    // Vector length check: the kernel reports it without doing any work
    // when asked for zero groups.
    let lanes = unsafe { crate::sme2::ffi::blake3_servil_sme2_hash16_chunks_512(core::ptr::null(), core::ptr::null(), 0, 0, core::ptr::null_mut(), 0) };
    lanes == crate::sme2::GROUP as u64
}

#[cfg(all(blake3_sme2, target_vendor = "apple"))]
fn sme2_reported() -> bool {
    let mut value: u32 = 0;
    let mut size = core::mem::size_of::<u32>();
    let rc =
        unsafe { libc::sysctlbyname(c"hw.optional.arm.FEAT_SME2".as_ptr(), &mut value as *mut u32 as *mut libc::c_void, &mut size, core::ptr::null_mut(), 0) };
    rc == 0 && value != 0
}

#[cfg(all(blake3_sme2, target_os = "linux"))]
fn sme2_reported() -> bool {
    // AT_HWCAP2 from <elf.h> and HWCAP2_SME2 from <asm/hwcap.h> (Linux 6.4+).
    // The libc crate doesn't expose either for glibc targets yet.
    const AT_HWCAP2: libc::c_ulong = 26;
    const HWCAP2_SME2: libc::c_ulong = 1 << 37;
    let hwcap2 = unsafe { libc::getauxval(AT_HWCAP2) };
    hwcap2 & HWCAP2_SME2 != 0
}

#[cfg(all(test, target_arch = "x86_64"))]
mod x86_degree_test {
    use super::*;

    /// The detected platform hashes as many chunks at once as the widest
    /// kernel this build and CPU have (the standard library's detection, an
    /// independent reading, says which the CPU has): a narrower one would
    /// hash correctly, more slowly.
    #[test]
    fn test_detected_degree_is_the_widest_kernel() {
        let expected = if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512vl") {
            if cfg!(target_os = "linux") { 32 } else { 16 }
        } else if std::is_x86_feature_detected!("avx2") {
            8
        } else {
            4
        };
        assert_eq!(Platform::detect().simd_degree(), expected);
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod x86_features_test {
    /// The CPUID read finds what the standard library's detection, an
    /// independent reading, finds.
    #[test]
    fn test_x86_features_match_std() {
        use super::x86_bits::*;
        let features = super::read_x86_features();
        assert_eq!(features & READ, READ);
        assert_eq!(features & SSE41 != 0, std::is_x86_feature_detected!("sse4.1"));
        assert_eq!(features & AVX2 != 0, std::is_x86_feature_detected!("avx2"));
        assert_eq!(features & AVX512 != 0, std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512vl"));
    }

    /// The wide kernels' CPUs: AMD from family 0x1A (Zen 5), from leaf 1's
    /// family fields as CPUID packs them.
    #[test]
    fn test_wide_cpu_and_family() {
        let eax = |base: u32, ext: u32| (ext << 20) | (base << 8);
        assert_eq!(super::family(eax(0xF, 0x0B)), 0x1A, "Zen 5");
        assert_eq!(super::family(eax(0xF, 0x0A)), 0x19, "Zen 4");
        assert_eq!(super::family(eax(0x6, 0x0F)), 0x6, "Intel: the extended family counts only with base 0xF");
        for (amd, family, wide) in [(true, 0x1A, true), (true, 0x1B, true), (true, 0x19, false), (true, 0x17, false), (false, 0x1A, false), (false, 0x6, false)]
        {
            assert_eq!(super::wide_cpu(amd, family), wide, "amd {amd}, family {family:#x}");
        }
    }

    /// The wide bit agrees with the kernel's own reading of this CPU
    /// (/proc/cpuinfo's vendor and family), and comes only with AVX-512.
    #[test]
    #[cfg(all(target_os = "linux", feature = "std"))]
    fn test_wide_bit_matches_cpuinfo() {
        use super::x86_bits::*;
        let features = super::read_x86_features();
        let info = std::fs::read_to_string("/proc/cpuinfo").expect("/proc/cpuinfo");
        let field = |name: &str| info.lines().find(|l| l.starts_with(name)).and_then(|l| l.split(':').nth(1)).map(|v| v.trim().to_string()).expect(name);
        let amd = field("vendor_id") == "AuthenticAMD";
        let family: u32 = field("cpu family").parse().expect("cpu family");
        let want = features & AVX512 != 0 && amd && family >= 0x1A;
        assert_eq!(features & WIDE != 0, want, "vendor {amd}, family {family:#x}");
        // What the dispatch reads, against the same independent reading.
        #[cfg(blake3_avx512_ffi)]
        assert_eq!(super::wide_avx512(), want, "wide_avx512 on vendor {amd}, family {family:#x}");
    }

    /// The detection functions answer as the standard library does, each
    /// one on its own: detect() asks for the widest kernel first, so it
    /// never consults the narrower ones' answers on a CPU that has a wider
    /// one (AVX2's on an AVX-512 machine, SSE4.1's on an AVX2 one), and the
    /// guard-page tests' platform lists come from these same functions.
    #[test]
    fn test_x86_detection_matches_std() {
        assert_eq!(super::sse41_detected(), std::is_x86_feature_detected!("sse4.1"), "sse41_detected");
        assert_eq!(super::avx2_detected(), std::is_x86_feature_detected!("avx2"), "avx2_detected");
        #[cfg(blake3_avx512_ffi)]
        assert_eq!(super::avx512_detected(), std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512vl"), "avx512_detected");
    }
    /// Each CPU runs the widest kernel it has; a CPU without SSE4.1 gets
    /// none, and the process stops (the stop itself: unsupported).
    #[test]
    fn test_a_cpu_runs_its_widest_kernel_and_one_without_sse41_none() {
        use super::{Platform, x86_platform};
        for (avx512, avx2, sse41, want) in [
            (true, true, true, Some(Platform::AVX512)),
            (true, false, false, Some(Platform::AVX512)),
            (false, true, true, Some(Platform::AVX2)),
            (false, true, false, Some(Platform::AVX2)),
            (false, false, true, Some(Platform::SSE41)),
            (false, false, false, None),
        ] {
            assert_eq!(x86_platform(avx512, avx2, sse41), want, "avx512 {avx512}, avx2 {avx2}, sse41 {sse41}");
        }
    }
}

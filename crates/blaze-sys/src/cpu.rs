//! The CPU's features, as one value: what the hashes (blaze-sha256, blaze-blake3), CRC-32C
//! (blaze-ids) and any other kernel of the bottom dispatch on. Found by [`CpuFeatures::detect`]
//! once, by a driver at its entry, and passed down to what dispatches: there is no static cache
//! here, and nothing reads the machine but that one call (rule 75: nothing ambient). On `core`
//! alone; no `std::arch` detection macro.
//!
//! - aarch64 Linux: the kernel's `AT_HWCAP` and `AT_HWCAP2` (asm/hwcap.h), from the process's
//!   facts handed in: `CpuFeatures::detect(&aux)` takes the driver's `crate::facts::Aux`
//!   (made once at its entry by the one source its kind of process has) and reads nothing.
//! - macOS (aarch64): `sysctlbyname("hw.optional.arm.FEAT_*")`, one name a feature; a name the
//!   chip predates is absent, not 0, and reads as absent.
//! - x86-64: CPUID leaves 1 and 7, and for the AVX and AVX-512 bits also that the OS saves
//!   their registers (OSXSAVE, then XCR0 by `xgetbv`), as std checks. On macOS the kernel
//!   enables AVX-512's state on a thread's first AVX-512 instruction, so XCR0 says no until
//!   then and the AVX-512 bits read absent: correct, the lanes run AVX2.
//!
//! The bit-by-bit mappings ([`CpuFeatures::from_hwcap`], [`CpuFeatures::from_cpuid`]) are pure
//! and built on every target, so their tests run on every host.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use core::ffi::CStr;

use crate::Errno;

flag_set! {
    /// The CPU features the bottom dispatches on. One type for both architectures: a bit of the
    /// other one is never set.
    CpuFeatures: u64 {
        /// x86-64 `SSSE3`.
        SSSE3 = 1 << 0;
        /// x86-64 `SSE4.1`.
        SSE41 = 1 << 1;
        /// x86-64 `SSE4.2`: its `CRC32` instruction is CRC-32C.
        SSE42 = 1 << 2;
        /// x86-64 `PCLMULQDQ`.
        PCLMULQDQ = 1 << 3;
        /// x86-64 `AES-NI`.
        AES_NI = 1 << 4;
        /// x86-64 `POPCNT`.
        POPCNT = 1 << 5;
        /// x86-64 `AVX`, its registers saved by the OS.
        AVX = 1 << 6;
        /// x86-64 `AVX2`, its registers saved by the OS.
        AVX2 = 1 << 7;
        /// x86-64 `FMA`, its registers saved by the OS.
        FMA = 1 << 8;
        /// x86-64 `BMI1`.
        BMI1 = 1 << 9;
        /// x86-64 `BMI2`.
        BMI2 = 1 << 10;
        /// x86-64 SHA extensions (`SHA-NI`).
        SHA_NI = 1 << 11;
        /// x86-64 `AVX512F`, its registers saved by the OS.
        AVX512F = 1 << 12;
        /// x86-64 `AVX512VL`, its registers saved by the OS: blake3's 512-bit lanes need it
        /// beside `AVX512F`.
        AVX512VL = 1 << 13;
        /// x86-64 `AVX512BW`.
        AVX512BW = 1 << 14;
        /// x86-64 `AVX512DQ`.
        AVX512DQ = 1 << 15;
        /// x86-64 `VAES`.
        VAES = 1 << 16;
        /// x86-64 `VPCLMULQDQ`.
        VPCLMULQDQ = 1 << 17;

        /// aarch64 Advanced SIMD (NEON): mandatory in Armv8-A, always set on aarch64.
        NEON = 1 << 32;
        /// aarch64 `FEAT_AES`.
        AES = 1 << 33;
        /// aarch64 `FEAT_PMULL`.
        PMULL = 1 << 34;
        /// aarch64 `FEAT_SHA1`.
        SHA1 = 1 << 35;
        /// aarch64 `FEAT_SHA256` (`sha2` in Rust's names).
        SHA2 = 1 << 36;
        /// aarch64 `FEAT_SHA3`.
        SHA3 = 1 << 37;
        /// aarch64 `FEAT_SHA512`.
        SHA512 = 1 << 38;
        /// aarch64 `FEAT_CRC32`.
        CRC32 = 1 << 39;
        /// aarch64 `FEAT_LSE` (the atomics).
        LSE = 1 << 40;
        /// aarch64 `FEAT_SVE`.
        SVE = 1 << 41;
        /// aarch64 `FEAT_SVE2`.
        SVE2 = 1 << 42;
        /// aarch64 `FEAT_SVE_AES`.
        SVE_AES = 1 << 43;
        /// aarch64 `FEAT_SVE_PMULL128`.
        SVE_PMULL = 1 << 44;
        /// aarch64 `FEAT_SVE_SHA3`.
        SVE_SHA3 = 1 << 45;
        /// aarch64 `FEAT_SVE_BITPerm`.
        SVE_BITPERM = 1 << 46;
    }
}

/// Why [`CpuFeatures::detect`] could not say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CpuError {
    /// A `sysctlbyname` refused with other than `ENOENT`, naming the name.
    Sysctl { name: &'static core::ffi::CStr, errno: Errno },
    /// No detection is built for this target (or under Miri, which has no CPUID).
    Unbuilt(crate::Unbuilt),
}

impl core::fmt::Display for CpuError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CpuError::Sysctl { name, errno } => write!(f, "the CPU's features: sysctl {name:?}: {errno:?}"),
            CpuError::Unbuilt(unbuilt) => {
                write!(f, "the CPU's features: no detection on {} ({})", unbuilt.target, unbuilt.operation)
            }
        }
    }
}

/// One bit of a machine word that sets one feature.
type Bit = (u32, CpuFeatures);

/// arm64 `HWCAP_*` (asm/hwcap.h) bits this type has.
const HWCAP: [Bit; 10] = [
    (1, CpuFeatures::NEON), // HWCAP_ASIMD
    (3, CpuFeatures::AES),
    (4, CpuFeatures::PMULL),
    (5, CpuFeatures::SHA1),
    (6, CpuFeatures::SHA2),
    (7, CpuFeatures::CRC32),
    (8, CpuFeatures::LSE), // HWCAP_ATOMICS
    (17, CpuFeatures::SHA3),
    (21, CpuFeatures::SHA512),
    (22, CpuFeatures::SVE),
];

/// arm64 `HWCAP2_*` bits this type has.
const HWCAP2: [Bit; 5] = [
    (1, CpuFeatures::SVE2),
    (2, CpuFeatures::SVE_AES),
    (3, CpuFeatures::SVE_PMULL),
    (4, CpuFeatures::SVE_BITPERM),
    (5, CpuFeatures::SVE_SHA3),
];

/// CPUID leaf 1 `ecx` bits needing no saved register state.
const LEAF1_ECX: [Bit; 6] = [
    (1, CpuFeatures::PCLMULQDQ),
    (9, CpuFeatures::SSSE3),
    (19, CpuFeatures::SSE41),
    (20, CpuFeatures::SSE42),
    (23, CpuFeatures::POPCNT),
    (25, CpuFeatures::AES_NI),
];
/// CPUID leaf 1 `ecx` bits needing the AVX state saved.
const LEAF1_ECX_AVX: [Bit; 2] = [(12, CpuFeatures::FMA), (28, CpuFeatures::AVX)];
/// CPUID leaf 7 `ebx` bits needing no saved state.
const LEAF7_EBX: [Bit; 3] = [(3, CpuFeatures::BMI1), (8, CpuFeatures::BMI2), (29, CpuFeatures::SHA_NI)];
/// CPUID leaf 7 `ebx` bits needing the AVX state saved.
const LEAF7_EBX_AVX: [Bit; 1] = [(5, CpuFeatures::AVX2)];
/// CPUID leaf 7 `ebx` bits needing the AVX-512 state saved.
const LEAF7_EBX_AVX512: [Bit; 4] = [
    (16, CpuFeatures::AVX512F),
    (17, CpuFeatures::AVX512DQ),
    (30, CpuFeatures::AVX512BW),
    (31, CpuFeatures::AVX512VL),
];
/// CPUID leaf 7 `ecx` bits needing the AVX state saved.
const LEAF7_ECX_AVX: [Bit; 2] = [(9, CpuFeatures::VAES), (10, CpuFeatures::VPCLMULQDQ)];

/// CPUID leaf 1 `ecx` bit 27: the OS has set `CR4.OSXSAVE`, so `xgetbv` runs.
pub const OSXSAVE: u32 = 1 << 27;

/// The words of CPUID [`CpuFeatures::from_cpuid`] reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cpuid {
    /// Leaf 1, `ecx`.
    pub leaf1_ecx: u32,
    /// Leaf 7 subleaf 0, `ebx`; 0 where leaf 0 says the highest leaf is below 7.
    pub leaf7_ebx: u32,
    /// Leaf 7 subleaf 0, `ecx`; 0 likewise.
    pub leaf7_ecx: u32,
    /// XCR0, read only where `leaf1_ecx` has [`OSXSAVE`]: `None` otherwise.
    pub xcr0: Option<u64>,
}

/// The features of `bits` whose bit is set in `word`.
#[blaze::noalloc]
#[blaze::nopanic]
const fn pick(word: u64, bits: &[Bit]) -> CpuFeatures {
    let mut found = CpuFeatures::NONE;
    let mut left = bits;
    while let [(bit, feature), rest @ ..] = left {
        if word & 1u64.wrapping_shl(*bit) != 0 {
            found = found.with(*feature);
        }
        left = rest;
    }
    found
}

impl CpuFeatures {
    /// Every feature this type names: bits 0 to 17 (x86-64) and 32 to 46 (aarch64).
    pub const ALL: CpuFeatures = CpuFeatures::from_bits(((1 << 18) - 1) | (((1 << 15) - 1) << 32));

    /// The features of an arm64 kernel's `AT_HWCAP` and `AT_HWCAP2` words.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_hwcap(hwcap: u64, hwcap2: u64) -> CpuFeatures {
        pick(hwcap, &HWCAP).with(pick(hwcap2, &HWCAP2))
    }

    /// The features of CPUID's words: the AVX bits only where XCR0 says the OS saves the
    /// SSE and AVX registers (bits 1 and 2), the AVX-512 bits only where it also saves the
    /// opmask and upper ZMM registers (bits 5 to 7).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_cpuid(words: Cpuid) -> CpuFeatures {
        let (ecx1, ebx7, ecx7) = (words.leaf1_ecx as u64, words.leaf7_ebx as u64, words.leaf7_ecx as u64);
        let mut found = pick(ecx1, &LEAF1_ECX).with(pick(ebx7, &LEAF7_EBX));
        let xcr0 = match words.xcr0 {
            Some(xcr0) if words.leaf1_ecx & OSXSAVE != 0 => xcr0,
            _ => 0,
        };
        if xcr0 & 0b110 == 0b110 {
            found = found.with(pick(ecx1, &LEAF1_ECX_AVX)).with(pick(ebx7, &LEAF7_EBX_AVX)).with(pick(ecx7, &LEAF7_ECX_AVX));
            if xcr0 & 0b1110_0000 == 0b1110_0000 {
                found = found.with(pick(ebx7, &LEAF7_EBX_AVX512));
            }
        }
        found
    }

    /// The features the build enables for every CPU it runs on (`-C target-feature`): what a
    /// dispatch may take as a constant, with no detection. Always within what
    /// [`CpuFeatures::detect`] finds.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn built() -> CpuFeatures {
        let all: [(bool, CpuFeatures); 30] = [
            (cfg!(target_feature = "ssse3"), CpuFeatures::SSSE3),
            (cfg!(target_feature = "sse4.1"), CpuFeatures::SSE41),
            (cfg!(target_feature = "sse4.2"), CpuFeatures::SSE42),
            (cfg!(target_feature = "pclmulqdq"), CpuFeatures::PCLMULQDQ),
            (cfg!(all(target_arch = "x86_64", target_feature = "aes")), CpuFeatures::AES_NI),
            (cfg!(target_feature = "popcnt"), CpuFeatures::POPCNT),
            (cfg!(target_feature = "avx"), CpuFeatures::AVX),
            (cfg!(target_feature = "avx2"), CpuFeatures::AVX2),
            (cfg!(target_feature = "fma"), CpuFeatures::FMA),
            (cfg!(target_feature = "bmi1"), CpuFeatures::BMI1),
            (cfg!(target_feature = "bmi2"), CpuFeatures::BMI2),
            (cfg!(target_feature = "sha"), CpuFeatures::SHA_NI),
            (cfg!(target_feature = "avx512f"), CpuFeatures::AVX512F),
            (cfg!(target_feature = "avx512vl"), CpuFeatures::AVX512VL),
            (cfg!(target_feature = "avx512bw"), CpuFeatures::AVX512BW),
            (cfg!(target_feature = "avx512dq"), CpuFeatures::AVX512DQ),
            (cfg!(target_feature = "vaes"), CpuFeatures::VAES),
            (cfg!(target_feature = "vpclmulqdq"), CpuFeatures::VPCLMULQDQ),
            (cfg!(target_feature = "neon"), CpuFeatures::NEON),
            (cfg!(all(target_arch = "aarch64", target_feature = "aes")), CpuFeatures::AES),
            // Rust's `aes` on aarch64 is FEAT_AES and FEAT_PMULL together.
            (cfg!(all(target_arch = "aarch64", target_feature = "aes")), CpuFeatures::PMULL),
            // Rust's `sha2` is FEAT_SHA1 and FEAT_SHA256 together.
            (cfg!(target_feature = "sha2"), CpuFeatures::SHA1),
            (cfg!(target_feature = "sha2"), CpuFeatures::SHA2),
            // Rust's `sha3` is FEAT_SHA3 and FEAT_SHA512 together.
            (cfg!(target_feature = "sha3"), CpuFeatures::SHA3),
            (cfg!(target_feature = "sha3"), CpuFeatures::SHA512),
            (cfg!(target_feature = "crc"), CpuFeatures::CRC32),
            (cfg!(target_feature = "lse"), CpuFeatures::LSE),
            (cfg!(target_feature = "sve"), CpuFeatures::SVE),
            (cfg!(target_feature = "sve2"), CpuFeatures::SVE2),
            (cfg!(target_feature = "sve2-bitperm"), CpuFeatures::SVE_BITPERM),
        ];
        let mut found = CpuFeatures::NONE;
        let mut left: &[(bool, CpuFeatures)] = &all;
        while let [(on, feature), rest @ ..] = left {
            if *on {
                found = found.with(*feature);
            }
            left = rest;
        }
        found
    }

    /// This CPU's features, read from the machine: the one call that does. A driver calls it
    /// once at its entry and passes the value down.
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn detect() -> Result<CpuFeatures, CpuError> {
        Ok(CpuFeatures::from_cpuid(x86::cpuid()))
    }

    /// This CPU's features, from the kernel's `AT_HWCAP` and `AT_HWCAP2` in the process's facts
    /// `aux`, handed in (made once at the driver's entry): nothing is read here. A driver calls
    /// it once and passes the value down. Never fails; `Result` for the one signature shape.
    #[cfg(all(target_arch = "aarch64", target_os = "linux", not(miri)))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn detect(aux: &crate::facts::Aux) -> Result<CpuFeatures, CpuError> {
        Ok(CpuFeatures::from_hwcap(aux.hwcap(), aux.hwcap2()))
    }

    /// This CPU's features, read from the machine: the one call that does. A driver calls it
    /// once at its entry and passes the value down.
    #[cfg(all(target_arch = "aarch64", target_os = "macos", not(miri)))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn detect() -> Result<CpuFeatures, CpuError> {
        /// The `hw.optional` names of each feature (sys/sysctl.h's `hw.optional.arm.*`).
        const NAMES: [(&CStr, CpuFeatures); 8] = [
            (c"hw.optional.arm.FEAT_AES", CpuFeatures::AES),
            (c"hw.optional.arm.FEAT_PMULL", CpuFeatures::PMULL),
            (c"hw.optional.arm.FEAT_SHA1", CpuFeatures::SHA1),
            (c"hw.optional.arm.FEAT_SHA256", CpuFeatures::SHA2),
            (c"hw.optional.arm.FEAT_SHA512", CpuFeatures::SHA512),
            (c"hw.optional.arm.FEAT_SHA3", CpuFeatures::SHA3),
            (c"hw.optional.arm.FEAT_LSE", CpuFeatures::LSE),
            (c"hw.optional.armv8_crc32", CpuFeatures::CRC32),
        ];
        let mut found = CpuFeatures::NEON;
        for (name, feature) in NAMES {
            match crate::macos::sysctl_u32(name) {
                Ok(Some(0) | None) => {}
                Ok(Some(_)) => found = found.with(feature),
                Err(errno) => return Err(CpuError::Sysctl { name, errno }),
            }
        }
        Ok(found)
    }

    /// No detection is built for this target: the refusal names it.
    #[cfg(any(
        miri,
        not(any(
            target_arch = "x86_64",
            all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
        ))
    ))]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn detect() -> Result<CpuFeatures, CpuError> {
        Err(CpuError::Unbuilt(crate::Unbuilt {
            operation: "cpu_features",
            target: if cfg!(miri) { "miri" } else { "this target" },
        }))
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[allow(unsafe_code, reason = "CPUID and XGETBV: `xgetbv` runs only once CPUID says the OS enabled it")]
mod x86 {
    use super::{Cpuid, OSXSAVE};
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    /// CPUID's leaves 1 and 7, and XCR0 where OSXSAVE is set.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub(super) fn cpuid() -> Cpuid {
        // SAFETY: CPUID exists on every x86-64 CPU, and leaf 0 says the highest leaf; leaf 7 is
        // read only at or below it. (`allow`: newer `core`s mark CPUID safe.)
        #[allow(unused_unsafe)]
        let (leaf1, leaf7) = unsafe {
            let highest = __cpuid(0).eax;
            (__cpuid(1), if highest >= 7 { Some(__cpuid_count(7, 0)) } else { None })
        };
        let xcr0 = if leaf1.ecx & OSXSAVE != 0 {
            // SAFETY: OSXSAVE is set, so XGETBV is enabled and XCR0 (register 0) readable.
            Some(unsafe { xcr0() })
        } else {
            None
        };
        Cpuid {
            leaf1_ecx: leaf1.ecx,
            leaf7_ebx: leaf7.map_or(0, |leaf| leaf.ebx),
            leaf7_ecx: leaf7.map_or(0, |leaf| leaf.ecx),
            xcr0,
        }
    }

    /// XCR0 by `xgetbv`, which needs the `xsave` target feature enabled for the call.
    ///
    /// # Safety
    ///
    /// The CPU's OSXSAVE bit is set.
    #[target_feature(enable = "xsave")]
    unsafe fn xcr0() -> u64 {
        // SAFETY: the caller checked OSXSAVE, and `xsave` is enabled in this function.
        unsafe { core::arch::x86_64::_xgetbv(0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each HWCAP bit is the kernel's (libc's constants on the target that has them) and maps
    /// to its one feature; no other bit sets anything.
    #[test]
    fn hwcap_bits_map_one_feature_each() {
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            let libcs: [(u64, u32); 10] = [
                (libc::HWCAP_ASIMD, 1),
                (libc::HWCAP_AES, 3),
                (libc::HWCAP_PMULL, 4),
                (libc::HWCAP_SHA1, 5),
                (libc::HWCAP_SHA2, 6),
                (libc::HWCAP_CRC32, 7),
                (libc::HWCAP_ATOMICS, 8),
                (libc::HWCAP_SHA3, 17),
                (libc::HWCAP_SHA512, 21),
                (libc::HWCAP_SVE, 22),
            ];
            for (libcs, bit) in libcs {
                assert_eq!(libcs, 1 << bit);
            }
        }
        let mut seen = CpuFeatures::NONE;
        for (bit, feature) in HWCAP {
            assert_eq!(CpuFeatures::from_hwcap(1 << bit, 0), feature, "HWCAP bit {bit}");
            seen = seen.with(feature);
        }
        for (bit, feature) in HWCAP2 {
            assert_eq!(CpuFeatures::from_hwcap(0, 1 << bit), feature, "HWCAP2 bit {bit}");
            seen = seen.with(feature);
        }
        let unnamed = !HWCAP.iter().fold(0u64, |word, (bit, _)| word | 1 << bit);
        assert_eq!(CpuFeatures::from_hwcap(unnamed, 0), CpuFeatures::NONE);
        assert_eq!(CpuFeatures::from_hwcap(0, 1), CpuFeatures::NONE, "HWCAP2_DCPODP is not named");
        assert!(CpuFeatures::ALL.contains(seen));
    }

    /// The AVX bits need XCR0's SSE and AVX state, the AVX-512 bits its opmask and ZMM state
    /// too, and neither is read without OSXSAVE.
    #[test]
    fn cpuid_avx_bits_need_the_os_to_save_their_registers() {
        let words = Cpuid {
            leaf1_ecx: 1 << 20 | 1 << 28 | OSXSAVE,
            leaf7_ebx: 1 << 5 | 1 << 16 | 1 << 31 | 1 << 29,
            leaf7_ecx: 1 << 9,
            xcr0: Some(0b1110_0111),
        };
        let all = CpuFeatures::SSE42
            .with(CpuFeatures::AVX)
            .with(CpuFeatures::AVX2)
            .with(CpuFeatures::AVX512F)
            .with(CpuFeatures::AVX512VL)
            .with(CpuFeatures::SHA_NI)
            .with(CpuFeatures::VAES);
        assert_eq!(CpuFeatures::from_cpuid(words), all);
        let no_zmm = CpuFeatures::SSE42
            .with(CpuFeatures::AVX)
            .with(CpuFeatures::AVX2)
            .with(CpuFeatures::SHA_NI)
            .with(CpuFeatures::VAES);
        assert_eq!(CpuFeatures::from_cpuid(Cpuid { xcr0: Some(0b111), ..words }), no_zmm);
        let none_saved = CpuFeatures::SSE42.with(CpuFeatures::SHA_NI);
        assert_eq!(CpuFeatures::from_cpuid(Cpuid { xcr0: None, ..words }), none_saved);
        assert_eq!(
            CpuFeatures::from_cpuid(Cpuid { leaf1_ecx: words.leaf1_ecx & !OSXSAVE, ..words }),
            none_saved,
            "an XCR0 without OSXSAVE is not believed"
        );
    }

    /// What `detect` finds is what std's own detection says, feature by feature, and holds
    /// every feature the build enables.
    #[cfg(not(miri))]
    #[test]
    fn detect_agrees_with_std() {
        // aarch64 Linux takes the process's facts; the test process's source is `/proc`.
        #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
        let found = CpuFeatures::detect(&crate::facts::Aux::from_proc().unwrap()).unwrap_or_else(|error| panic!("{error}"));
        #[cfg(not(all(target_arch = "aarch64", target_os = "linux")))]
        let found = CpuFeatures::detect().unwrap_or_else(|error| panic!("{error}"));
        assert!(found.contains(CpuFeatures::built()), "{found:?} lacks the built {:?}", CpuFeatures::built());
        #[cfg(target_arch = "x86_64")]
        {
            let std: [(bool, CpuFeatures); 18] = [
                (std::is_x86_feature_detected!("ssse3"), CpuFeatures::SSSE3),
                (std::is_x86_feature_detected!("sse4.1"), CpuFeatures::SSE41),
                (std::is_x86_feature_detected!("sse4.2"), CpuFeatures::SSE42),
                (std::is_x86_feature_detected!("pclmulqdq"), CpuFeatures::PCLMULQDQ),
                (std::is_x86_feature_detected!("aes"), CpuFeatures::AES_NI),
                (std::is_x86_feature_detected!("popcnt"), CpuFeatures::POPCNT),
                (std::is_x86_feature_detected!("avx"), CpuFeatures::AVX),
                (std::is_x86_feature_detected!("avx2"), CpuFeatures::AVX2),
                (std::is_x86_feature_detected!("fma"), CpuFeatures::FMA),
                (std::is_x86_feature_detected!("bmi1"), CpuFeatures::BMI1),
                (std::is_x86_feature_detected!("bmi2"), CpuFeatures::BMI2),
                (std::is_x86_feature_detected!("sha"), CpuFeatures::SHA_NI),
                (std::is_x86_feature_detected!("avx512f"), CpuFeatures::AVX512F),
                (std::is_x86_feature_detected!("avx512vl"), CpuFeatures::AVX512VL),
                (std::is_x86_feature_detected!("avx512bw"), CpuFeatures::AVX512BW),
                (std::is_x86_feature_detected!("avx512dq"), CpuFeatures::AVX512DQ),
                (std::is_x86_feature_detected!("vaes"), CpuFeatures::VAES),
                (std::is_x86_feature_detected!("vpclmulqdq"), CpuFeatures::VPCLMULQDQ),
            ];
            for (said, feature) in std {
                // std on macOS reads AVX-512 from `hw.optional`, not XCR0 (the module doc): it
                // may say yes where XCR0 still says no, never the reverse.
                let avx512 = CpuFeatures::AVX512F
                    .with(CpuFeatures::AVX512VL)
                    .with(CpuFeatures::AVX512BW)
                    .with(CpuFeatures::AVX512DQ);
                if cfg!(target_os = "macos") && avx512.contains(feature) {
                    assert!(said || !found.contains(feature), "{feature:?}");
                } else {
                    assert_eq!(found.contains(feature), said, "{feature:?}");
                }
            }
        }
        #[cfg(all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos")))]
        {
            // std's `aes` is FEAT_AES and FEAT_PMULL, `sha2` FEAT_SHA1 and FEAT_SHA256, `sha3`
            // FEAT_SHA3 and FEAT_SHA512: each is compared with both of its bits.
            let std: [(bool, CpuFeatures); 9] = [
                (std::arch::is_aarch64_feature_detected!("neon"), CpuFeatures::NEON),
                (std::arch::is_aarch64_feature_detected!("pmull"), CpuFeatures::PMULL),
                (std::arch::is_aarch64_feature_detected!("sha2"), CpuFeatures::SHA1.with(CpuFeatures::SHA2)),
                (std::arch::is_aarch64_feature_detected!("sha3"), CpuFeatures::SHA3.with(CpuFeatures::SHA512)),
                (std::arch::is_aarch64_feature_detected!("crc"), CpuFeatures::CRC32),
                (std::arch::is_aarch64_feature_detected!("lse"), CpuFeatures::LSE),
                (std::arch::is_aarch64_feature_detected!("aes"), CpuFeatures::AES.with(CpuFeatures::PMULL)),
                (std::arch::is_aarch64_feature_detected!("sve"), CpuFeatures::SVE),
                (std::arch::is_aarch64_feature_detected!("sve2"), CpuFeatures::SVE2),
            ];
            for (said, feature) in std {
                assert_eq!(found.contains(feature), said, "{feature:?}");
            }
        }
    }
}

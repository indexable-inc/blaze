//! The CPU features the bottom of the tree dispatches on, found once per process and read with
//! one relaxed load after: what std's `is_x86_feature_detected!` and
//! `is_aarch64_feature_detected!` answer, on `core` alone, for the features blaze-blake3's lanes
//! and blaze-ids' CRC-32C use. A feature the build already enables (`-C target-feature`) is a
//! constant and costs no load.
//!
//! - x86-64: CPUID's feature bits, and for AVX2 and AVX-512 also that the OS saves their
//!   registers (OSXSAVE, then XCR0's bits through `xgetbv`), as std checks. On macOS the kernel
//!   enables AVX-512's state only on a thread's first AVX-512 instruction, so XCR0 says no
//!   until then and the lanes run AVX2: correct, and what std's own check said before it read
//!   the `hw.optional.avx512f` sysctl.
//! - aarch64: CRC32 is mandatory from Armv8.1, and every Apple core has it; on Linux the
//!   kernel's `AT_HWCAP` says (`getauxval`, libc's, which every Linux program links).

use core::sync::atomic::{AtomicU8, Ordering};

/// A feature this module answers for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Feature {
    /// x86-64's SSE4.2, whose CRC32 instruction blaze-ids' CRC-32C uses.
    Sse42,
    /// x86-64's AVX2, with its registers saved by the OS.
    Avx2,
    /// x86-64's AVX-512 Foundation, with its registers saved by the OS.
    Avx512f,
    /// aarch64's CRC32 instructions.
    Crc,
}

impl Feature {
    const fn bit(self) -> u8 {
        match self {
            Feature::Sse42 => 1 << 0,
            Feature::Avx2 => 1 << 1,
            Feature::Avx512f => 1 << 2,
            Feature::Crc => 1 << 3,
        }
    }
}

/// The features found, with [`KNOWN`] once they are: 0 until the first ask.
static FOUND: AtomicU8 = AtomicU8::new(0);

/// Set in [`FOUND`] once the features are found.
const KNOWN: u8 = 1 << 7;

/// Whether this CPU, and the OS for its registers, has `feature`.
#[inline]
pub fn has(feature: Feature) -> bool {
    if enabled(feature) {
        return true;
    }
    // Relaxed: the word is a pure function of the machine, so any thread's store is right.
    let mut found = FOUND.load(Ordering::Relaxed);
    if found & KNOWN == 0 {
        found = find() | KNOWN;
        FOUND.store(found, Ordering::Relaxed);
    }
    found & feature.bit() != 0
}

/// Whether the build enables `feature` for every CPU it runs on.
#[inline]
const fn enabled(feature: Feature) -> bool {
    match feature {
        Feature::Sse42 => cfg!(target_feature = "sse4.2"),
        Feature::Avx2 => cfg!(target_feature = "avx2"),
        Feature::Avx512f => cfg!(target_feature = "avx512f"),
        Feature::Crc => cfg!(target_feature = "crc"),
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[allow(
    unsafe_code,
    reason = "CPUID and XGETBV: `xgetbv` is called only once CPUID says the OS enabled it"
)]
fn find() -> u8 {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    // SAFETY: CPUID exists on every x86-64 CPU, and leaf 0 says the highest leaf; leaves 1
    // and 7 are read only below it. (`allow`: newer `core`s mark CPUID safe.)
    #[allow(unused_unsafe)]
    let (leaf1, leaf7) = unsafe {
        let highest = __cpuid(0).eax;
        (__cpuid(1), if highest >= 7 { Some(__cpuid_count(7, 0)) } else { None })
    };
    let mut found = 0;
    if leaf1.ecx & (1 << 20) != 0 {
        found |= Feature::Sse42.bit();
    }
    // OSXSAVE: the OS uses XSAVE and lets XGETBV read XCR0.
    let Some(leaf7) = leaf7 else { return found };
    if leaf1.ecx & (1 << 27) == 0 {
        return found;
    }
    // SAFETY: OSXSAVE is set, so XGETBV is enabled and XCR0 (register 0) readable.
    let xcr0 = unsafe { xcr0() };
    // XCR0 bits 1 and 2: the OS saves the SSE and AVX registers.
    let avx_saved = xcr0 & 0b110 == 0b110;
    // And bits 5 to 7: the opmask registers and the upper ZMM registers.
    let avx512_saved = avx_saved && xcr0 & 0b1110_0000 == 0b1110_0000;
    if avx_saved && leaf7.ebx & (1 << 5) != 0 {
        found |= Feature::Avx2.bit();
    }
    if avx512_saved && leaf7.ebx & (1 << 16) != 0 {
        found |= Feature::Avx512f.bit();
    }
    found
}

/// XCR0 by `xgetbv`, which needs the `xsave` target feature enabled for the call.
///
/// # Safety
///
/// The CPU's OSXSAVE bit is set.
#[cfg(all(target_arch = "x86_64", not(miri)))]
#[allow(unsafe_code, reason = "calls `xgetbv` under the caller's OSXSAVE check")]
#[target_feature(enable = "xsave")]
unsafe fn xcr0() -> u64 {
    // SAFETY: the caller checked OSXSAVE, and `xsave` is enabled in this function.
    unsafe { core::arch::x86_64::_xgetbv(0) }
}

#[cfg(all(target_arch = "aarch64", target_os = "linux", not(miri)))]
#[allow(unsafe_code, reason = "libc's `getauxval`, which reads the process's auxiliary vector")]
fn find() -> u8 {
    /// `AT_HWCAP` (linux/auxvec.h) and its `HWCAP_CRC32` bit (asm/hwcap.h, arm64).
    const AT_HWCAP: core::ffi::c_ulong = 16;
    const HWCAP_CRC32: core::ffi::c_ulong = 1 << 7;
    unsafe extern "C" {
        /// glibc's and musl's: the value of an auxiliary vector entry, 0 if absent.
        safe fn getauxval(kind: core::ffi::c_ulong) -> core::ffi::c_ulong;
    }
    if getauxval(AT_HWCAP) & HWCAP_CRC32 != 0 { Feature::Crc.bit() } else { 0 }
}

#[cfg(all(target_arch = "aarch64", not(target_os = "linux"), not(miri)))]
fn find() -> u8 {
    // Apple's cores, the only other aarch64 host, all have CRC32 (Armv8.1 and later).
    if cfg!(target_vendor = "apple") { Feature::Crc.bit() } else { 0 }
}

#[cfg(all(not(any(target_arch = "x86_64", target_arch = "aarch64")), not(miri)))]
fn find() -> u8 {
    0
}

/// Under Miri, which runs neither CPUID nor `getauxval`: only what the build enables, so a
/// SIMD path runs under Miri when the build enables its feature (`RUSTFLAGS="-C
/// target-feature=+avx2"`), and the scalar path otherwise.
#[cfg(miri)]
fn find() -> u8 {
    0
}

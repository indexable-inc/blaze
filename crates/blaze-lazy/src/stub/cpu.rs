//! What the x86-64 resolver saves, read from the processor once per session: the vector
//! state components the OS enabled (`XCR0`) and the size `XSAVE` or `XSAVEC` needs for them
//! (`CPUID` leaf `0xD`), as glibc's dynamic linker sizes its lazy-binding trampoline.
//!
//! Unsafe only for `CPUID` and `XGETBV`: the safe alternative would be a fixed save area
//! that is wrong on some processor.

#[cfg(target_arch = "x86_64")]
use super::X86Save;

/// The components a call may carry a value in or a caller may keep live across one: x87 and
/// MXCSR (0), `xmm` (1), `ymm`'s upper halves (2), MPX bounds (3, 4), AVX-512's masks and
/// `zmm` (5 to 7), and APX's `r16` to `r31` (19). Not PKRU (9), which code doesn't change
/// across a call, nor AMX tiles (17, 18), which no ABI passes and every call clobbers.
#[cfg(target_arch = "x86_64")]
pub(super) const SAVED: u64 = 0xff | 1 << 19;

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
pub(super) fn x86_save() -> X86Save {
    use std::arch::x86_64::{__cpuid, __cpuid_count};

    // SAFETY: `CPUID` exists on every x86-64 processor and reads nothing but itself.
    let leaf1 = unsafe { __cpuid(1) };
    let osxsave = leaf1.ecx & (1 << 27) != 0;
    if !osxsave {
        return X86Save::Fx;
    }
    // SAFETY: OSXSAVE says the OS turned XSAVE on, so `XGETBV` exists.
    let mask = unsafe { xcr0() } & SAVED;
    // SAFETY: as above.
    let compacted = unsafe { __cpuid_count(0xd, 1) }.eax & (1 << 1) != 0;
    let component = |i: u32| {
        // SAFETY: as above.
        let leaf = unsafe { __cpuid_count(0xd, i) };
        (leaf.eax, leaf.ebx, leaf.ecx & (1 << 1) != 0)
    };
    let components = (2..63).filter(|i| mask & (1 << i) != 0);
    if compacted {
        let mut size = 576u32;
        for i in components {
            let (len, _, aligned) = component(i);
            if aligned {
                size = size.next_multiple_of(64);
            }
            size += len;
        }
        X86Save::Xsavec { mask, size }
    } else {
        let size = components.map(|i| {
            let (len, offset, _) = component(i);
            offset + len
        });
        X86Save::Xsave { mask, size: size.max().unwrap_or(0).max(576) }
    }
}

/// # Safety
///
/// The processor has `XGETBV`: CPUID.1:ECX.OSXSAVE is set.
#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
#[target_feature(enable = "xsave")]
unsafe fn xcr0() -> u64 {
    // SAFETY: the caller's: XGETBV exists; `XCR0` is readable at every privilege level.
    unsafe { std::arch::x86_64::_xgetbv(0) }
}

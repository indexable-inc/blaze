//! ELF's program header and the numbers the launcher and `dl_iterate_phdr`'s callers read
//! (elf.h, the System V gABI's 64-bit forms). Linux.

/// `Elf64_Addr` (elf.h): an address in an object.
/// replaces libc::Elf64_Addr
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type Elf64_Addr = u64;

/// `Elf64_Off` (elf.h): a file offset.
/// replaces libc::Elf64_Off
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type Elf64_Off = u64;

/// `Elf64_Word` (elf.h): a 32-bit word.
/// replaces libc::Elf64_Word
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type Elf64_Word = u32;

/// `Elf64_Xword` (elf.h): a 64-bit word.
/// replaces libc::Elf64_Xword
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type Elf64_Xword = u64;

/// `Elf64_Phdr` (elf.h): one program header, a segment the loader maps.
/// replaces libc::Elf64_Phdr
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct Elf64_Phdr {
    /// `p_type`: `PT_LOAD` and the rest.
    pub p_type: Elf64_Word,
    /// `p_flags`: `PF_R`, `PF_W`, `PF_X`.
    pub p_flags: Elf64_Word,
    /// `p_offset`: where its bytes start in the file.
    pub p_offset: Elf64_Off,
    /// `p_vaddr`: where it is mapped, before the load bias.
    pub p_vaddr: Elf64_Addr,
    /// `p_paddr`.
    pub p_paddr: Elf64_Addr,
    /// `p_filesz`: its bytes in the file.
    pub p_filesz: Elf64_Xword,
    /// `p_memsz`: its bytes in memory.
    pub p_memsz: Elf64_Xword,
    /// `p_align`.
    pub p_align: Elf64_Xword,
}

// elf.h: 56 bytes; these fields.
const _: () = {
    assert!(size_of::<Elf64_Phdr>() == 56);
    assert!(align_of::<Elf64_Phdr>() == 8);
    assert!(core::mem::offset_of!(Elf64_Phdr, p_type) == 0);
    assert!(core::mem::offset_of!(Elf64_Phdr, p_flags) == 4);
    assert!(core::mem::offset_of!(Elf64_Phdr, p_offset) == 8);
    assert!(core::mem::offset_of!(Elf64_Phdr, p_vaddr) == 16);
    assert!(core::mem::offset_of!(Elf64_Phdr, p_filesz) == 32);
    assert!(core::mem::offset_of!(Elf64_Phdr, p_memsz) == 40);
    assert!(core::mem::offset_of!(Elf64_Phdr, p_align) == 48);
};

/// `PT_LOAD` (elf.h): a segment the loader maps.
/// replaces libc::PT_LOAD
pub const PT_LOAD: Elf64_Word = 1;

/// `PF_X` (elf.h): the segment is executable.
/// replaces libc::PF_X
pub const PF_X: Elf64_Word = 1;

/// `PF_R` (elf.h): the segment is readable.
/// replaces libc::PF_R
pub const PF_R: Elf64_Word = 4;

/// The auxiliary vector's end and the kernel's saved copy of it (linux/auxv.rs).
/// replaces libc::AT_NULL (blaze_sys::posix::proc::AT_NULL, which is blaze_sys::linux::auxv::AT_NULL, a u64)
/// replaces libc::PR_GET_AUXV (blaze_sys::posix::proc::prctl_get_auxv(&mut [u8]) -> Result<usize, Errno>, the whole prctl call; the number itself is private)
pub use crate::linux::auxv::{AT_NULL, prctl_get_auxv};

/// `HWCAP_ASIMD` (asm/hwcap.h, arm64): `AT_HWCAP`'s bit for Advanced SIMD (NEON).
/// replaces libc::HWCAP_ASIMD
#[cfg(target_arch = "aarch64")]
pub const HWCAP_ASIMD: u64 = 1 << 1;
/// `HWCAP_AES` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_AES
#[cfg(target_arch = "aarch64")]
pub const HWCAP_AES: u64 = 1 << 3;
/// `HWCAP_PMULL` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_PMULL
#[cfg(target_arch = "aarch64")]
pub const HWCAP_PMULL: u64 = 1 << 4;
/// `HWCAP_SHA1` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_SHA1
#[cfg(target_arch = "aarch64")]
pub const HWCAP_SHA1: u64 = 1 << 5;
/// `HWCAP_SHA2` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_SHA2
#[cfg(target_arch = "aarch64")]
pub const HWCAP_SHA2: u64 = 1 << 6;
/// `HWCAP_CRC32` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_CRC32
#[cfg(target_arch = "aarch64")]
pub const HWCAP_CRC32: u64 = 1 << 7;
/// `HWCAP_ATOMICS` (asm/hwcap.h, arm64): LSE atomics.
/// replaces libc::HWCAP_ATOMICS
#[cfg(target_arch = "aarch64")]
pub const HWCAP_ATOMICS: u64 = 1 << 8;
/// `HWCAP_SHA3` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_SHA3
#[cfg(target_arch = "aarch64")]
pub const HWCAP_SHA3: u64 = 1 << 17;
/// `HWCAP_SHA512` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_SHA512
#[cfg(target_arch = "aarch64")]
pub const HWCAP_SHA512: u64 = 1 << 21;
/// `HWCAP_SVE` (asm/hwcap.h, arm64).
/// replaces libc::HWCAP_SVE
#[cfg(target_arch = "aarch64")]
pub const HWCAP_SVE: u64 = 1 << 22;

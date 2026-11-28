//! 0-dependency syscall effect taxonomy and compile-time minimal sandbox synthesizer.
//!
//! `#![no_std]`, zero `unsafe`, zero heap allocation, zero dependencies beyond `core`.
//!
//! [`DomainMask`] mirrors `blaze-ty::syscall_ty::SyscallSet` bit-for-bit across the 8 kernel
//! authority domains (`CLOCK`, `RNG`, `VM`, `STAT` tail-resumptive; `FS`, `NET`, `PROC`, `SYNC`
//! suspending). From a single `DomainMask`, this module synthesizes:
//!
//! 1. **macOS Seatbelt SBPL** ([`synthesize_seatbelt_sbpl`]): default-deny profile emitting only
//!    the exact rules required by `allowed`.
//! 2. **Linux seccomp syscall allow/deny sets** ([`is_linux_syscall_allowed`],
//!    [`denied_linux_syscalls`]): architecture-exact syscall numbers for both `aarch64` and
//!    `x86_64`, matching [`crate::linux::nr`].

use core::fmt;

/// 8-bit kernel authority mask, bit-identical to `blaze_ty::syscall_ty::SyscallSet`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct DomainMask(u8);

impl DomainMask {
    /// No kernel authority.
    pub const EMPTY: DomainMask = DomainMask(0);

    /// Clock queries (`clock_gettime`, vDSO). Tail-resumptive.
    pub const CLOCK: DomainMask = DomainMask(1 << 0);
    /// Entropy queries (`getrandom`). Tail-resumptive.
    pub const RNG: DomainMask = DomainMask(1 << 1);
    /// Virtual memory management (`mmap`, `munmap`, `mprotect`, `madvise`). Tail-resumptive.
    pub const VM: DomainMask = DomainMask(1 << 2);
    /// Metadata & host/process topology (`fstatat`, `statx`, `getpid`). Tail-resumptive.
    pub const STAT: DomainMask = DomainMask(1 << 3);

    /// Filesystem I/O (`openat`, `read`, `write`, `fsync`, `renameat2`, `unlinkat`, `flock`). Suspending.
    pub const FS: DomainMask = DomainMask(1 << 4);
    /// Network I/O (`socket`, `bind`, `listen`, `accept4`, `connect`, `sendto`, `recvfrom`). Suspending.
    pub const NET: DomainMask = DomainMask(1 << 5);
    /// Process lifecycle (`clone`, `clone3`, `execve`, `execveat`, `waitid`, `kill`). Suspending.
    pub const PROC: DomainMask = DomainMask(1 << 6);
    /// Synchronization & timers (`futex`, `ulock`, `sched_yield`, `clock_nanosleep`). Suspending.
    pub const SYNC: DomainMask = DomainMask(1 << 7);

    /// All tail-resumptive domains (bits `0..=3`).
    pub const TAIL_RESUMPTIVE: DomainMask = DomainMask(0b0000_1111);
    /// All suspending domains (bits `4..=7`).
    pub const SUSPENDING: DomainMask = DomainMask(0b1111_0000);
    /// All 8 kernel authority domains.
    pub const ALL: DomainMask = DomainMask(0b1111_1111);

    /// Constructs a [`DomainMask`] from its raw 8-bit representation.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn from_bits(bits: u8) -> DomainMask {
        DomainMask(bits)
    }

    /// Raw 8-bit mask.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns `true` iff no domain bits are set.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` iff every bit of `flag` is present in `self`.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn contains(self, flag: DomainMask) -> bool {
        (self.0 & flag.0) == flag.0
    }

    /// Set union (`self ∪ other`).
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn union(self, other: DomainMask) -> DomainMask {
        DomainMask(self.0 | other.0)
    }

    /// Set intersection (`self ∩ other`).
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn intersection(self, other: DomainMask) -> DomainMask {
        DomainMask(self.0 & other.0)
    }

    /// Set difference (`self \ other`).
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn difference(self, other: DomainMask) -> DomainMask {
        DomainMask(self.0 & !other.0)
    }

    /// Returns `true` iff `self ⊆ allowed`.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn is_subset_of(self, allowed: DomainMask) -> bool {
        (self.0 & !allowed.0) == 0
    }

    /// Returns `true` iff any suspending domain (`FS`, `NET`, `PROC`, `SYNC`) is set.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn requires_suspend(self) -> bool {
        (self.0 & Self::SUSPENDING.0) != 0
    }
}

const DOMAIN_NAMES: [(DomainMask, &str); 8] = [
    (DomainMask::CLOCK, "clock"),
    (DomainMask::RNG, "rng"),
    (DomainMask::VM, "vm"),
    (DomainMask::STAT, "stat"),
    (DomainMask::FS, "fs"),
    (DomainMask::NET, "net"),
    (DomainMask::PROC, "proc"),
    (DomainMask::SYNC, "sync"),
];

impl fmt::Display for DomainMask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sys(")?;
        let mut first = true;
        for &(domain, name) in &DOMAIN_NAMES {
            if self.contains(domain) {
                if !first {
                    f.write_str(", ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        f.write_str(")")
    }
}

/// Target Linux architecture for seccomp syscall numbering.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SeccompArch {
    /// ARM64 (`asm-generic/unistd.h`).
    Aarch64,
    /// x86-64 (`arch/x86/entry/syscalls/syscall_64.tbl`).
    X86_64,
}

/// Planted-bug controls for sandbox policy oracle tests.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub enum PolicyPlant {
    /// Honest synthesizer: grants only the domains present in `allowed`.
    #[default]
    None,
    /// Planted bug: silently leaks `NET` authority when `allowed` is `DomainMask::EMPTY`.
    LeakNetIntoPure,
    /// Planted bug: silently leaks `PROC` authority when `allowed` has `FS` without `PROC`.
    LeakProcIntoFsOnly,
}

#[inline]
#[blaze::noalloc]
#[blaze::nopanic]
const fn apply_plant(allowed: DomainMask, plant: PolicyPlant) -> DomainMask {
    match plant {
        PolicyPlant::None => allowed,
        PolicyPlant::LeakNetIntoPure => {
            if allowed.is_empty() {
                allowed.union(DomainMask::NET)
            } else {
                allowed
            }
        }
        PolicyPlant::LeakProcIntoFsOnly => {
            if allowed.contains(DomainMask::FS) && !allowed.contains(DomainMask::PROC) {
                allowed.union(DomainMask::PROC)
            } else {
                allowed
            }
        }
    }
}

/// Synthesizes a minimal macOS Seatbelt SBPL profile permitting only the kernel domains in
/// `allowed`, written directly into `out` with zero heap allocation.
pub fn synthesize_seatbelt_sbpl(allowed: DomainMask, out: &mut impl fmt::Write) -> fmt::Result {
    synthesize_seatbelt_sbpl_with(allowed, PolicyPlant::None, out)
}

/// Synthesizes a macOS Seatbelt SBPL profile with an explicit [`PolicyPlant`] for planted-bug
/// oracle testing.
pub fn synthesize_seatbelt_sbpl_with(
    allowed: DomainMask,
    plant: PolicyPlant,
    out: &mut impl fmt::Write,
) -> fmt::Result {
    let eff = apply_plant(allowed, plant);

    out.write_str("(version 1)\n")?;
    out.write_str("(deny default)\n")?;
    out.write_str("(allow process-info-pidinfo (target self))\n")?;

    if eff.contains(DomainMask::CLOCK) {
        out.write_str(";; domain: clock (commpage read-only timebase)\n")?;
    }
    if eff.contains(DomainMask::RNG) {
        out.write_str(";; domain: rng (getentropy kernel CSRNG)\n")?;
    }
    if eff.contains(DomainMask::VM) {
        out.write_str(";; domain: vm (virtual memory mapping)\n")?;
        out.write_str("(allow file-map-executable)\n")?;
    }
    if eff.contains(DomainMask::STAT) {
        out.write_str("(allow sysctl-read)\n")?;
        if !eff.contains(DomainMask::FS) {
            out.write_str("(allow file-read-metadata)\n")?;
        }
    }
    if eff.contains(DomainMask::FS) {
        out.write_str("(allow file-read* file-write*)\n")?;
    }
    if eff.contains(DomainMask::NET) {
        out.write_str("(allow network*)\n")?;
    }
    if eff.contains(DomainMask::PROC) {
        out.write_str("(allow process-fork process-exec)\n")?;
    }
    if eff.contains(DomainMask::SYNC) {
        out.write_str(";; domain: sync (ulock and thread scheduling)\n")?;
    }
    Ok(())
}

/// One classified Linux syscall with its owning [`DomainMask`] and architecture-exact numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LinuxSyscallSpec {
    /// Canonical Linux syscall name.
    pub name: &'static str,
    /// Single authority bit gating this syscall.
    pub domain: DomainMask,
    /// Syscall number on `x86_64` (`syscall_64.tbl`).
    pub x86_64: u32,
    /// Syscall number on `aarch64` (`asm-generic/unistd.h`).
    pub aarch64: u32,
}

impl LinuxSyscallSpec {
    /// Returns this syscall's number on `arch`.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn nr(&self, arch: SeccompArch) -> u32 {
        match arch {
            SeccompArch::Aarch64 => self.aarch64,
            SeccompArch::X86_64 => self.x86_64,
        }
    }
}

/// Architecture-exact Linux syscall classification table, matching `crate::linux::nr`.
pub const LINUX_SYSCALL_TABLE: &[LinuxSyscallSpec] = &[
    // CLOCK
    LinuxSyscallSpec { name: "clock_gettime", domain: DomainMask::CLOCK, x86_64: 228, aarch64: 113 },
    // RNG
    LinuxSyscallSpec { name: "getrandom", domain: DomainMask::RNG, x86_64: 318, aarch64: 278 },
    // VM
    LinuxSyscallSpec { name: "mmap", domain: DomainMask::VM, x86_64: 9, aarch64: 222 },
    LinuxSyscallSpec { name: "munmap", domain: DomainMask::VM, x86_64: 11, aarch64: 215 },
    LinuxSyscallSpec { name: "mprotect", domain: DomainMask::VM, x86_64: 10, aarch64: 226 },
    LinuxSyscallSpec { name: "madvise", domain: DomainMask::VM, x86_64: 28, aarch64: 233 },
    // STAT
    LinuxSyscallSpec { name: "statx", domain: DomainMask::STAT, x86_64: 332, aarch64: 291 },
    LinuxSyscallSpec { name: "newfstatat", domain: DomainMask::STAT, x86_64: 262, aarch64: 79 },
    LinuxSyscallSpec { name: "getpid", domain: DomainMask::STAT, x86_64: 39, aarch64: 172 },
    // FS
    LinuxSyscallSpec { name: "openat", domain: DomainMask::FS, x86_64: 257, aarch64: 56 },
    LinuxSyscallSpec { name: "openat2", domain: DomainMask::FS, x86_64: 437, aarch64: 437 },
    LinuxSyscallSpec { name: "read", domain: DomainMask::FS, x86_64: 0, aarch64: 63 },
    LinuxSyscallSpec { name: "write", domain: DomainMask::FS, x86_64: 1, aarch64: 64 },
    LinuxSyscallSpec { name: "fsync", domain: DomainMask::FS, x86_64: 74, aarch64: 82 },
    LinuxSyscallSpec { name: "renameat2", domain: DomainMask::FS, x86_64: 316, aarch64: 276 },
    LinuxSyscallSpec { name: "renameat", domain: DomainMask::FS, x86_64: 264, aarch64: 38 },
    LinuxSyscallSpec { name: "unlinkat", domain: DomainMask::FS, x86_64: 263, aarch64: 35 },
    LinuxSyscallSpec { name: "getdents64", domain: DomainMask::FS, x86_64: 217, aarch64: 61 },
    LinuxSyscallSpec { name: "flock", domain: DomainMask::FS, x86_64: 73, aarch64: 32 },
    // NET
    LinuxSyscallSpec { name: "socket", domain: DomainMask::NET, x86_64: 41, aarch64: 198 },
    LinuxSyscallSpec { name: "connect", domain: DomainMask::NET, x86_64: 42, aarch64: 203 },
    LinuxSyscallSpec { name: "bind", domain: DomainMask::NET, x86_64: 49, aarch64: 200 },
    LinuxSyscallSpec { name: "listen", domain: DomainMask::NET, x86_64: 50, aarch64: 201 },
    LinuxSyscallSpec { name: "accept4", domain: DomainMask::NET, x86_64: 288, aarch64: 242 },
    LinuxSyscallSpec { name: "sendto", domain: DomainMask::NET, x86_64: 44, aarch64: 206 },
    LinuxSyscallSpec { name: "recvfrom", domain: DomainMask::NET, x86_64: 45, aarch64: 207 },
    LinuxSyscallSpec { name: "sendmsg", domain: DomainMask::NET, x86_64: 46, aarch64: 211 },
    LinuxSyscallSpec { name: "recvmsg", domain: DomainMask::NET, x86_64: 47, aarch64: 212 },
    // PROC
    LinuxSyscallSpec { name: "clone", domain: DomainMask::PROC, x86_64: 56, aarch64: 220 },
    LinuxSyscallSpec { name: "clone3", domain: DomainMask::PROC, x86_64: 435, aarch64: 435 },
    LinuxSyscallSpec { name: "execve", domain: DomainMask::PROC, x86_64: 59, aarch64: 221 },
    LinuxSyscallSpec { name: "execveat", domain: DomainMask::PROC, x86_64: 322, aarch64: 281 },
    LinuxSyscallSpec { name: "waitid", domain: DomainMask::PROC, x86_64: 247, aarch64: 95 },
    LinuxSyscallSpec { name: "kill", domain: DomainMask::PROC, x86_64: 62, aarch64: 129 },
    // SYNC
    LinuxSyscallSpec { name: "futex", domain: DomainMask::SYNC, x86_64: 202, aarch64: 98 },
    LinuxSyscallSpec { name: "sched_yield", domain: DomainMask::SYNC, x86_64: 24, aarch64: 124 },
    LinuxSyscallSpec { name: "clock_nanosleep", domain: DomainMask::SYNC, x86_64: 230, aarch64: 115 },
];

/// Inline capacity for [`DeniedSyscalls`], strictly covering [`LINUX_SYSCALL_TABLE`].
pub const MAX_CLASSIFIED_SYSCALLS: usize = 40;

const _: () = assert!(LINUX_SYSCALL_TABLE.len() <= MAX_CLASSIFIED_SYSCALLS);

/// Zero-allocation inline set of denied Linux syscall numbers for a seccomp filter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeniedSyscalls {
    nrs: [u32; MAX_CLASSIFIED_SYSCALLS],
    len: usize,
}

impl DeniedSyscalls {
    /// Denied syscall numbers in table order.
    #[inline]
    pub fn as_slice(&self) -> &[u32] {
        &self.nrs[..self.len]
    }

    /// Number of denied syscalls.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` iff no classified syscalls are denied.
    #[inline]
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns `true` iff `nr` is in the denied set.
    #[inline]
    pub fn contains(&self, nr: u32) -> bool {
        self.as_slice().contains(&nr)
    }

    /// Iterates over the denied syscall numbers.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, u32> {
        self.as_slice().iter()
    }
}

impl AsRef<[u32]> for DeniedSyscalls {
    #[inline]
    fn as_ref(&self) -> &[u32] {
        self.as_slice()
    }
}

/// Looks up the owning [`DomainMask`] of a Linux syscall number on `arch`.
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn linux_syscall_domain(nr: u32, arch: SeccompArch) -> Option<DomainMask> {
    let mut i = 0;
    while i < LINUX_SYSCALL_TABLE.len() {
        let spec = &LINUX_SYSCALL_TABLE[i];
        if spec.nr(arch) == nr {
            return Some(spec.domain);
        }
        i += 1;
    }
    None
}

/// Looks up a classified Linux syscall's number by canonical name on `arch`.
pub fn linux_syscall_nr(name: &str, arch: SeccompArch) -> Option<u32> {
    for spec in LINUX_SYSCALL_TABLE {
        if spec.name == name {
            return Some(spec.nr(arch));
        }
    }
    None
}

/// Returns the inline list of classified Linux syscalls denied under `allowed` on `arch`.
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn denied_linux_syscalls(allowed: DomainMask, arch: SeccompArch) -> DeniedSyscalls {
    denied_linux_syscalls_with(allowed, arch, PolicyPlant::None)
}

/// Returns the inline list of classified Linux syscalls denied under `allowed` on `arch` with an
/// explicit [`PolicyPlant`] for planted-bug oracle testing.
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn denied_linux_syscalls_with(
    allowed: DomainMask,
    arch: SeccompArch,
    plant: PolicyPlant,
) -> DeniedSyscalls {
    let eff = apply_plant(allowed, plant);
    let mut nrs = [0u32; MAX_CLASSIFIED_SYSCALLS];
    let mut len = 0;
    let mut i = 0;
    while i < LINUX_SYSCALL_TABLE.len() {
        let spec = &LINUX_SYSCALL_TABLE[i];
        if !spec.domain.is_subset_of(eff) {
            nrs[len] = spec.nr(arch);
            len += 1;
        }
        i += 1;
    }
    DeniedSyscalls { nrs, len }
}

/// Returns `true` iff Linux syscall `nr` on `arch` is permitted under `allowed`.
///
/// Unclassified syscalls are denied by default (fail-closed).
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn is_linux_syscall_allowed(nr: u32, allowed: DomainMask, arch: SeccompArch) -> bool {
    is_linux_syscall_allowed_with(nr, allowed, arch, PolicyPlant::None)
}

/// Returns `true` iff Linux syscall `nr` on `arch` is permitted under `allowed`, with an explicit
/// [`PolicyPlant`] for planted-bug oracle testing.
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn is_linux_syscall_allowed_with(
    nr: u32,
    allowed: DomainMask,
    arch: SeccompArch,
    plant: PolicyPlant,
) -> bool {
    let eff = apply_plant(allowed, plant);
    match linux_syscall_domain(nr, arch) {
        Some(domain) => domain.is_subset_of(eff),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Zero-allocation stack buffer for testing `fmt::Write` synthesizers in `#![no_std]`.
    struct StackBuf<const N: usize> {
        bytes: [u8; N],
        len: usize,
    }

    impl<const N: usize> StackBuf<N> {
        const fn new() -> Self {
            Self { bytes: [0; N], len: 0 }
        }

        fn as_str(&self) -> &str {
            core::str::from_utf8(&self.bytes[..self.len]).expect("UTF-8 written by fmt::Write")
        }
    }

    impl<const N: usize> fmt::Write for StackBuf<N> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let b = s.as_bytes();
            let end = self.len.checked_add(b.len()).ok_or(fmt::Error)?;
            if end > N {
                return Err(fmt::Error);
            }
            self.bytes[self.len..end].copy_from_slice(b);
            self.len = end;
            Ok(())
        }
    }

    /// Independent sandbox oracle: checks both SBPL output and Linux seccomp decisions against
    /// `allowed` on both architectures. Returns `Err(reason)` on any policy leak or omission.
    fn check_sandbox_oracle(allowed: DomainMask, plant: PolicyPlant) -> Result<(), &'static str> {
        let mut sbpl = StackBuf::<2048>::new();
        synthesize_seatbelt_sbpl_with(allowed, plant, &mut sbpl).map_err(|_| "sbpl write overflow")?;
        let text = sbpl.as_str();

        if !text.starts_with("(version 1)\n(deny default)\n") {
            return Err("sbpl missing (version 1) / (deny default) header");
        }
        if !text.contains("(allow process-info-pidinfo (target self))") {
            return Err("sbpl missing baseline process-info-pidinfo");
        }

        let has_net = text.contains("network*");
        if has_net != allowed.contains(DomainMask::NET) {
            return Err("sbpl network* disagrees with DomainMask::NET");
        }
        let has_fork = text.contains("process-fork");
        let has_exec = text.contains("process-exec");
        if has_fork != allowed.contains(DomainMask::PROC) || has_exec != allowed.contains(DomainMask::PROC) {
            return Err("sbpl process-fork/process-exec disagrees with DomainMask::PROC");
        }
        let has_fs_rw = text.contains("(allow file-read* file-write*)");
        if has_fs_rw != allowed.contains(DomainMask::FS) {
            return Err("sbpl file-read*/file-write* disagrees with DomainMask::FS");
        }
        let has_sysctl = text.contains("(allow sysctl-read)");
        if has_sysctl != allowed.contains(DomainMask::STAT) {
            return Err("sbpl sysctl-read disagrees with DomainMask::STAT");
        }
        let expect_meta_only = allowed.contains(DomainMask::STAT) && !allowed.contains(DomainMask::FS);
        if text.contains("(allow file-read-metadata)") != expect_meta_only {
            return Err("sbpl file-read-metadata disagrees with STAT-without-FS");
        }
        if text.contains("(allow file-map-executable)") != allowed.contains(DomainMask::VM) {
            return Err("sbpl file-map-executable disagrees with DomainMask::VM");
        }

        for arch in [SeccompArch::Aarch64, SeccompArch::X86_64] {
            let denied = denied_linux_syscalls_with(allowed, arch, plant);
            for spec in LINUX_SYSCALL_TABLE {
                let nr = spec.nr(arch);
                let expect_allow = spec.domain.is_subset_of(allowed);
                let got_allow = is_linux_syscall_allowed_with(nr, allowed, arch, plant);
                if got_allow != expect_allow {
                    return Err("linux is_linux_syscall_allowed disagrees with DomainMask");
                }
                if denied.contains(nr) == expect_allow {
                    return Err("linux denied_linux_syscalls disagrees with DomainMask");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn empty_domain_mask_denies_all_sensitive_syscalls_and_emits_minimal_sbpl() {
        let mut sbpl = StackBuf::<1024>::new();
        synthesize_seatbelt_sbpl(DomainMask::EMPTY, &mut sbpl).expect("sbpl fits");
        let text = sbpl.as_str();
        assert!(text.starts_with("(version 1)\n(deny default)\n"));
        assert!(!text.contains("network*"));
        assert!(!text.contains("process-fork"));
        assert!(!text.contains("process-exec"));
        assert!(!text.contains("file-write*"));
        assert!(!text.contains("file-read*"));
        assert!(!text.contains("sysctl-read"));

        for arch in [SeccompArch::Aarch64, SeccompArch::X86_64] {
            let socket_nr = linux_syscall_nr("socket", arch).expect("socket in table");
            let execve_nr = linux_syscall_nr("execve", arch).expect("execve in table");
            let openat_nr = linux_syscall_nr("openat", arch).expect("openat in table");

            assert!(!is_linux_syscall_allowed(socket_nr, DomainMask::EMPTY, arch));
            assert!(!is_linux_syscall_allowed(execve_nr, DomainMask::EMPTY, arch));
            assert!(!is_linux_syscall_allowed(openat_nr, DomainMask::EMPTY, arch));

            let denied = denied_linux_syscalls(DomainMask::EMPTY, arch);
            assert_eq!(denied.len(), LINUX_SYSCALL_TABLE.len());
            assert!(denied.contains(socket_nr));
            assert!(denied.contains(execve_nr));
            assert!(denied.contains(openat_nr));
        }

        // Unclassified syscall numbers fail closed even under DomainMask::ALL.
        assert!(!is_linux_syscall_allowed(999_999, DomainMask::ALL, SeccompArch::Aarch64));
        assert!(!is_linux_syscall_allowed(999_999, DomainMask::ALL, SeccompArch::X86_64));
    }

    #[test]
    fn fs_plus_clock_allows_openat_and_clock_gettime_and_denies_socket_and_execve() {
        let policy = DomainMask::FS.union(DomainMask::CLOCK);
        assert!(policy.requires_suspend());
        assert!(!DomainMask::TAIL_RESUMPTIVE.requires_suspend());

        let mut sbpl = StackBuf::<1024>::new();
        synthesize_seatbelt_sbpl(policy, &mut sbpl).expect("sbpl fits");
        let text = sbpl.as_str();
        assert!(text.contains("(allow file-read* file-write*)"));
        assert!(!text.contains("network*"));
        assert!(!text.contains("process-fork"));

        for arch in [SeccompArch::Aarch64, SeccompArch::X86_64] {
            let openat = linux_syscall_nr("openat", arch).expect("openat");
            let clock = linux_syscall_nr("clock_gettime", arch).expect("clock_gettime");
            let socket = linux_syscall_nr("socket", arch).expect("socket");
            let execve = linux_syscall_nr("execve", arch).expect("execve");

            assert!(is_linux_syscall_allowed(openat, policy, arch));
            assert!(is_linux_syscall_allowed(clock, policy, arch));
            assert!(!is_linux_syscall_allowed(socket, policy, arch));
            assert!(!is_linux_syscall_allowed(execve, policy, arch));

            let denied = denied_linux_syscalls(policy, arch);
            assert!(!denied.contains(openat));
            assert!(!denied.contains(clock));
            assert!(denied.contains(socket));
            assert!(denied.contains(execve));
        }
    }

    #[test]
    fn exhaustive_all_256_domain_masks_satisfy_oracle_and_unique_syscall_table() {
        // Verify every classified syscall number is unique per architecture.
        for arch in [SeccompArch::Aarch64, SeccompArch::X86_64] {
            for (i, a) in LINUX_SYSCALL_TABLE.iter().enumerate() {
                for b in &LINUX_SYSCALL_TABLE[i + 1..] {
                    assert_ne!(a.nr(arch), b.nr(arch), "duplicate nr for {} and {}", a.name, b.name);
                }
            }
        }

        for bits in 0u8..=u8::MAX {
            let mask = DomainMask::from_bits(bits);
            assert_eq!(check_sandbox_oracle(mask, PolicyPlant::None), Ok(()));
        }
    }

    #[test]
    fn planted_bug_controls_are_caught_by_sandbox_oracle() {
        // Honest synthesizer passes for both pure and FS-only policies.
        assert_eq!(check_sandbox_oracle(DomainMask::EMPTY, PolicyPlant::None), Ok(()));
        assert_eq!(
            check_sandbox_oracle(DomainMask::FS.union(DomainMask::CLOCK), PolicyPlant::None),
            Ok(())
        );

        // Planted bug 1: leaking NET into pure EMPTY policy is caught by the oracle.
        assert!(
            check_sandbox_oracle(DomainMask::EMPTY, PolicyPlant::LeakNetIntoPure).is_err(),
            "oracle must catch PolicyPlant::LeakNetIntoPure"
        );
        // Verify seccomp layer specifically catches LeakNetIntoPure on both architectures.
        for arch in [SeccompArch::Aarch64, SeccompArch::X86_64] {
            let socket = linux_syscall_nr("socket", arch).expect("socket");
            assert!(is_linux_syscall_allowed_with(
                socket,
                DomainMask::EMPTY,
                arch,
                PolicyPlant::LeakNetIntoPure
            ));
        }

        // Planted bug 2: leaking PROC into FS-only policy is caught by the oracle.
        let fs_only = DomainMask::FS.union(DomainMask::CLOCK);
        assert!(
            check_sandbox_oracle(fs_only, PolicyPlant::LeakProcIntoFsOnly).is_err(),
            "oracle must catch PolicyPlant::LeakProcIntoFsOnly"
        );
        for arch in [SeccompArch::Aarch64, SeccompArch::X86_64] {
            let execve = linux_syscall_nr("execve", arch).expect("execve");
            assert!(is_linux_syscall_allowed_with(execve, fs_only, arch, PolicyPlant::LeakProcIntoFsOnly));
        }
    }

    #[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn linux_syscall_table_matches_crate_linux_nr() {
        use crate::linux::nr;

        #[cfg(target_arch = "aarch64")]
        let arch = SeccompArch::Aarch64;
        #[cfg(target_arch = "x86_64")]
        let arch = SeccompArch::X86_64;

        let expected: &[(&str, usize)] = &[
            ("clock_gettime", nr::CLOCK_GETTIME),
            ("getrandom", nr::GETRANDOM),
            ("mmap", nr::MMAP),
            ("munmap", nr::MUNMAP),
            ("mprotect", nr::MPROTECT),
            ("madvise", nr::MADVISE),
            ("statx", nr::STATX),
            ("newfstatat", nr::NEWFSTATAT),
            ("getpid", nr::GETPID),
            ("openat", nr::OPENAT),
            ("openat2", nr::OPENAT2),
            ("read", nr::READ),
            ("write", nr::WRITE),
            ("fsync", nr::FSYNC),
            ("renameat2", nr::RENAMEAT2),
            ("renameat", nr::RENAMEAT),
            ("unlinkat", nr::UNLINKAT),
            ("getdents64", nr::GETDENTS64),
            ("flock", nr::FLOCK),
            ("socket", nr::SOCKET),
            ("connect", nr::CONNECT),
            ("bind", nr::BIND),
            ("listen", nr::LISTEN),
            ("accept4", nr::ACCEPT4),
            ("sendmsg", nr::SENDMSG),
            ("recvmsg", nr::RECVMSG),
            ("clone", nr::CLONE),
            ("clone3", nr::CLONE3),
            ("execveat", nr::EXECVEAT),
            ("waitid", nr::WAITID),
            ("kill", nr::KILL),
            ("futex", nr::FUTEX),
            ("sched_yield", nr::SCHED_YIELD),
            ("clock_nanosleep", nr::CLOCK_NANOSLEEP),
        ];
        for &(name, nr_val) in expected {
            assert_eq!(linux_syscall_nr(name, arch), Some(nr_val as u32), "mismatch for {name}");
        }
    }
}

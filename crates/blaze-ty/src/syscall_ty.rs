//! Syscall Authority Sets & Sound Sans-IO / Sandbox Effect Algebra (`blaze-ty::syscall_ty`).
//!
//! Tracks the 8 kernel authority domains a Blaze function may exercise as an exact 8-bit bitset
//! ([`SyscallSet`]), split cleanly into two tiers:
//!
//! - **Tail-resumptive (`TAIL_RESUMPTIVE`, bits `0..=3`)**: bounded, non-blocking kernel queries
//!   (`CLOCK`, `RNG`, `VM`, `STAT`) that never imply [`EffectSet::SUSPEND`] and lower directly as
//!   [`LoweringMode::PlainFn`].
//! - **Suspending (`SUSPENDING`, bits `4..=7`)**: I/O, process, and synchronization authority
//!   (`FS`, `NET`, `PROC`, `SYNC`) that implies [`EffectSet::SUSPEND`] and lowers through
//!   `blaze-machine` as [`LoweringMode::MachineCoroutine`].
//!
//! Policy checking ([`check_syscall_policy`]) enforces `required ⊆ allowed` at call sites and
//! sandbox boundaries, and `#![blaze::sans_io]` checking ([`check_sans_io`]) proves a pure state
//! core exercises zero kernel authority and neither blocks nor suspends.

use core::fmt;

use crate::effect_ty::{EffectSet, LoweringMode};

/// 8-bit exact bitset of kernel authority domains exercised by a function or permitted by a policy.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct SyscallSet(u8);

impl SyscallSet {
    /// No kernel authority: pure computation with zero syscalls.
    pub const EMPTY: SyscallSet = SyscallSet(0);

    /// Wall/monotonic clock queries (`clock_gettime`, vDSO). Tail-resumptive.
    pub const CLOCK: SyscallSet = SyscallSet(1 << 0);
    /// Cryptographic entropy queries (`getrandom`). Tail-resumptive.
    pub const RNG: SyscallSet = SyscallSet(1 << 1);
    /// Virtual memory management (`mmap`, `munmap`, `mprotect`, `madvise`). Tail-resumptive.
    pub const VM: SyscallSet = SyscallSet(1 << 2);
    /// Metadata and host/process topology (`fstatat`, `statx`, `getpid`). Tail-resumptive.
    pub const STAT: SyscallSet = SyscallSet(1 << 3);

    /// Filesystem I/O (`openat`, `read`, `write`, `fsync`, `renameat`, `unlinkat`, `getdents64`, `flock`). Suspending.
    pub const FS: SyscallSet = SyscallSet(1 << 4);
    /// Network I/O (`socket`, `bind`, `listen`, `accept4`, `connect`, `sendto`, `recvfrom`). Suspending.
    pub const NET: SyscallSet = SyscallSet(1 << 5);
    /// Process lifecycle (`clone3`, `posix_spawn`, `waitid`, `kill`, `execve`). Suspending.
    pub const PROC: SyscallSet = SyscallSet(1 << 6);
    /// Thread synchronization and timers (`futex`, `ulock`, `sched_yield`, timers). Suspending.
    pub const SYNC: SyscallSet = SyscallSet(1 << 7);

    /// All tail-resumptive domains (bits `0..=3`): no `SUSPEND`, lowers as [`LoweringMode::PlainFn`].
    pub const TAIL_RESUMPTIVE: SyscallSet = SyscallSet(0b0000_1111);
    /// All suspending domains (bits `4..=7`): implies [`EffectSet::SUSPEND`], lowers as
    /// [`LoweringMode::MachineCoroutine`].
    pub const SUSPENDING: SyscallSet = SyscallSet(0b1111_0000);
    /// Every kernel authority domain (bits `0..=7`).
    pub const ALL: SyscallSet = SyscallSet(0b1111_1111);

    /// Constructs a [`SyscallSet`] from its raw 8-bit mask.
    #[inline]
    pub const fn from_bits(bits: u8) -> SyscallSet {
        SyscallSet(bits)
    }

    /// Returns the raw 8-bit authority mask.
    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns `true` iff no syscall domains are set.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` iff every bit of `flag` is present in `self`.
    #[inline]
    pub const fn contains(self, flag: SyscallSet) -> bool {
        (self.0 & flag.0) == flag.0
    }

    /// Set union (`self ∪ other`).
    #[inline]
    pub const fn union(self, other: SyscallSet) -> SyscallSet {
        SyscallSet(self.0 | other.0)
    }

    /// Set intersection (`self ∩ other`).
    #[inline]
    pub const fn intersection(self, other: SyscallSet) -> SyscallSet {
        SyscallSet(self.0 & other.0)
    }

    /// Set difference (`self \ other`): bits in `self` not in `other`.
    #[inline]
    pub const fn difference(self, other: SyscallSet) -> SyscallSet {
        SyscallSet(self.0 & !other.0)
    }

    /// Subtyping check (`self ⊆ allowed`): `self` exercises no authority outside `allowed`.
    #[inline]
    pub const fn is_subset_of(self, allowed: SyscallSet) -> bool {
        (self.0 & !allowed.0) == 0
    }

    /// Parses one syscall domain keyword in either bare (`"fs"`) or prefixed (`"sys::fs"`) form.
    pub fn from_word(word: &str) -> Option<SyscallSet> {
        match word {
            "sys::clock" | "clock" => Some(Self::CLOCK),
            "sys::rng" | "rng" => Some(Self::RNG),
            "sys::vm" | "vm" => Some(Self::VM),
            "sys::stat" | "stat" => Some(Self::STAT),
            "sys::fs" | "fs" => Some(Self::FS),
            "sys::net" | "net" => Some(Self::NET),
            "sys::proc" | "proc" => Some(Self::PROC),
            "sys::sync" | "sync" => Some(Self::SYNC),
            _ => None,
        }
    }

    /// Returns `true` iff any suspending domain (`FS`, `NET`, `PROC`, `SYNC`) is present.
    #[inline]
    pub const fn requires_suspend(self) -> bool {
        (self.0 & Self::SUSPENDING.0) != 0
    }

    /// Returns the effect set implied by `self`: [`EffectSet::SUSPEND`] if
    /// [`Self::requires_suspend`] holds, otherwise [`EffectSet::PURE`].
    #[inline]
    pub const fn implied_effects(self) -> EffectSet {
        if self.requires_suspend() { EffectSet::SUSPEND } else { EffectSet::PURE }
    }

    /// Selects [`LoweringMode::MachineCoroutine`] when `self` requires suspension and
    /// [`LoweringMode::PlainFn`] when all domains in `self` are tail-resumptive (or empty).
    #[inline]
    pub const fn lowering_mode(self) -> LoweringMode {
        LoweringMode::for_effects(self.implied_effects())
    }

    /// Returns `true` iff a function with `effects` and `syscalls` satisfies `#![blaze::sans_io]`:
    /// zero syscall authority, no blocking, and no suspension.
    #[inline]
    pub const fn is_sans_io_clean(effects: EffectSet, syscalls: SyscallSet) -> bool {
        syscalls.is_empty() && !effects.contains(EffectSet::BLOCK) && !effects.contains(EffectSet::SUSPEND)
    }
}

/// Ordered `(domain, canonical_name)` table used by [`fmt::Display`].
const DOMAIN_NAMES: [(SyscallSet, &str); 8] = [
    (SyscallSet::CLOCK, "clock"),
    (SyscallSet::RNG, "rng"),
    (SyscallSet::VM, "vm"),
    (SyscallSet::STAT, "stat"),
    (SyscallSet::FS, "fs"),
    (SyscallSet::NET, "net"),
    (SyscallSet::PROC, "proc"),
    (SyscallSet::SYNC, "sync"),
];

impl fmt::Display for SyscallSet {
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

/// Returns `true` iff `(effects, syscalls)` is clean under `#![blaze::sans_io]`.
#[inline]
pub const fn is_sans_io_clean(effects: EffectSet, syscalls: SyscallSet) -> bool {
    SyscallSet::is_sans_io_clean(effects, syscalls)
}

/// Errors produced by syscall policy checks and `#![blaze::sans_io]` enforcement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyscallPolicyError {
    /// Required kernel authority domains exceed what the caller or sandbox policy permits.
    SyscallExceedsPolicy { required: SyscallSet, allowed: SyscallSet },
    /// A `#![blaze::sans_io]` item performs syscalls, blocks, or suspends.
    SansIoViolation { effects: EffectSet, syscalls: SyscallSet },
}

impl fmt::Display for SyscallPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SyscallPolicyError::SyscallExceedsPolicy { required, allowed } => {
                let excess = required.difference(allowed);
                write!(
                    f,
                    "syscall authority {required} exceeds allowed policy {allowed} (disallowed: {excess})"
                )
            }
            SyscallPolicyError::SansIoViolation { effects, syscalls } => {
                write!(
                    f,
                    "sans-io violation: item requires effects '{effects}' and syscalls '{syscalls}'"
                )
            }
        }
    }
}

impl core::error::Error for SyscallPolicyError {}

/// Checks that every kernel authority domain in `required` is permitted by `allowed`.
#[inline]
pub const fn check_syscall_policy(required: SyscallSet, allowed: SyscallSet) -> Result<(), SyscallPolicyError> {
    if required.is_subset_of(allowed) {
        Ok(())
    } else {
        Err(SyscallPolicyError::SyscallExceedsPolicy { required, allowed })
    }
}

/// Enforces `#![blaze::sans_io]`: rejects any syscall authority, `EffectSet::BLOCK`, or
/// `EffectSet::SUSPEND`.
#[inline]
pub const fn check_sans_io(effects: EffectSet, syscalls: SyscallSet) -> Result<(), SyscallPolicyError> {
    if SyscallSet::is_sans_io_clean(effects, syscalls) {
        Ok(())
    } else {
        Err(SyscallPolicyError::SansIoViolation { effects, syscalls })
    }
}

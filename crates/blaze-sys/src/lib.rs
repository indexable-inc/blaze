//! blaze's one home for system calls (`docs/v2/plan/design/sys.md`).
//!
//! `#![no_std]`, no dependency but `core`, and the only crate of blaze that may make a system
//! call. blaze-core's drivers and waits are written on it; bstd never calls it (bstd speaks
//! effects, and a driver turns an effect into the calls here).
//!
//! The API is typed per operation, never a `syscall(n, a, b, c)` passthrough: a function takes
//! an [`Fd`], a [`Path`] (a NUL-terminated view, never allocated here), flags as typed sets and
//! a [`Timespec`], and returns `Result<T, Errno>`. No function here allocates or panics; each
//! is marked `#[blaze::noalloc]` and `#[blaze::nopanic]`, and its doc comment says which effect
//! or wait uses it.
//!
//! | target | how | built in slice 1 |
//! |---|---|---|
//! | Linux (aarch64, x86-64) | raw system calls by `asm!` ([`linux`]), numbers in [`linux::nr`] | every call of sys.md's table |
//! | macOS | libSystem's symbols, declared here ([`macos`]); private ones where they do more | the declarations and their symbol tests |
//! | Windows | ntdll and kernel32 imports ([`windows`]) | the list of operations |
//!
//! What a target has not built is an [`Unbuilt`]: a typed refusal naming the operation and the
//! target, never another route (rule no-fallback).
//!
//! No C library on Linux: blaze-sys references no C-library symbol there (the check is
//! `scripts/src/bin/sys_imports.rs` over the rlib's undefined symbols, which must be empty).
//! The vDSO's `clock_gettime` is found from `AT_SYSINFO_EHDR` (`linux::vdso`), kernel code
//! and not the C library. On macOS the floor is libSystem: Apple keeps no stable system call
//! numbers and requires calls through libSystem, always dynamically linked, so blaze-sys
//! declares libSystem's functions itself; the imported set is the ratchet
//! `tools/ratchets/blaze-sys-libsystem.txt`.
//!
//! Is it faster? Not per call: a C wrapper costs a few nanoseconds against a system call's
//! hundred or more, and the vDSO's clock is used either way. The gains are elsewhere: a Linux
//! program built on blaze-sys alone can be a static binary with its own entry, so no dynamic
//! loader runs at start; no `errno` thread-local is touched, the error is the return value;
//! calls can be batched through the ring (io_uring) instead of one transition each; and the
//! layer between blaze and the kernel is one we control, typed per operation, with nothing
//! of the C library's state (locks, atfork handlers, cached pids) to reason about.
//!
//! No minimum kernel version, and the best form on each (D185): what the kernel offers is
//! probed once into `linux::probe::KernelFacts` (never parsed from `uname`), and each
//! operation with several forms is a slot in `linux::slot` whose form is chosen from those
//! facts, or refused by name when no form exists. The probe table, the slot table and the
//! honest floor (today set by blaze-core's wait ring, not by blaze-sys) are in
//! `linux::probe`'s docs. macOS has the same shape in `macos_facts::MacFacts`.

#![cfg_attr(not(test), no_std)]
#![feature(register_tool, custom_inner_attributes)]
// `openat` and `ioctl` of macOS's table of calls found at first use are defined here (variadic).
#![cfg_attr(target_os = "macos", feature(c_variadic))]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "the one home of system calls: raw `asm!` system calls on Linux and libSystem's declared symbols on macOS, each `unsafe` block stating the kernel contract it relies on"
)]
#![allow(
    unsafe_code,
    reason = "the one home of system calls: each `unsafe` block states the kernel contract it relies on"
)]

mod errno;
#[macro_use]
mod flags;
pub mod cpu;
mod types;

pub use cpu::{CpuError, CpuFeatures};
pub use errno::Errno;
pub use types::{Fd, Path, Timespec, WindowSize};

#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;
/// The measured experiment of rule 45 (2026-11-20): libsystem_kernel's own entries and the raw
/// trap as strategies beside libSystem's public calls, one seam; the default stays `Public`.
#[cfg(target_os = "macos")]
pub mod macos_kernel;
/// D182: a block device's facts on macOS (`DKIOC*`), preallocation and hole punching.
#[cfg(target_os = "macos")]
pub mod macos_block;
/// macOS's facts, probed once (D185): the OS version and which private entries exist.
#[cfg(target_os = "macos")]
pub mod macos_facts;

#[cfg(unix)]
pub mod posix;

/// The process's facts (page size, clock ticks, capability words, the vDSO's address): one
/// value handed in, with one source per kind of process (D185).
#[cfg(unix)]
pub mod facts;

/// D177: Seatbelt and a process group's kill, for a hermetic run.
#[cfg(target_os = "macos")]
pub mod seatbelt;

/// bstd's sampler's memory and frame-pointer walk (D178): every `unsafe` of the sampler.
pub mod sample;

/// The Mach thread calls of bstd's sampler (D178).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub mod macos_sample;

#[cfg(windows)]
pub mod windows;

/// An operation blaze-sys does not have on a target yet: the refusal names both, and nothing
/// stands in for the call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unbuilt {
    /// The operation, as sys.md's table names it.
    pub operation: &'static str,
    /// The target that has no such call yet.
    pub target: &'static str,
}

impl core::fmt::Display for Unbuilt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Unbuilt { operation, target } = self;
        write!(
            f,
            "blaze-sys has no `{operation}` on {target} yet: it is specified and not built \
             (docs/v2/plan/design/sys.md), and nothing stands in for it"
        )
    }
}

impl core::error::Error for Unbuilt {}

/// The operations of sys.md's table that a target with no layer yet refuses, each as a
/// function named for it.
#[cfg(any(target_os = "macos", windows))]
macro_rules! unbuilt {
    ($target:literal: $($operation:ident),* $(,)?) => {
        /// Every operation this target refuses, by name.
        pub const UNBUILT: &[&str] = &[$(stringify!($operation)),*];

        $(
            #[doc = concat!("`", stringify!($operation), "`: specified, not built on ", $target, ".")]
            #[blaze::noalloc]
            #[blaze::nopanic]
            pub const fn $operation() -> $crate::Unbuilt {
                $crate::Unbuilt { operation: stringify!($operation), target: $target }
            }
        )*
    };
}
#[cfg(any(target_os = "macos", windows))]
pub(crate) use unbuilt;

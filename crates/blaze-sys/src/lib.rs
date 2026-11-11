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

#![cfg_attr(not(test), no_std)]
#![feature(register_tool)]
#![register_tool(blaze)]
#![allow(
    unsafe_code,
    reason = "the one home of system calls: each `unsafe` block states the kernel contract it relies on"
)]

mod errno;
#[macro_use]
mod flags;
mod types;

pub use errno::Errno;
pub use types::{Fd, Path, Timespec};

#[cfg(all(target_os = "linux", any(target_arch = "aarch64", target_arch = "x86_64")))]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

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

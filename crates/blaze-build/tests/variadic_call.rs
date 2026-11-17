//! Where the arguments of a C variadic call go: `snprintf(buf, n, fmt, 42i32, ptr, 122i32,
//! 2.5f64, -9000000000i64)`, three declared parameters then five variadic arguments.
//!
//! Rust's rule (the Rust Reference, "External blocks", items.extern.variadic.syntax):
//! "Functions within external blocks may be made variadic by specifying ... as the last
//! parameter." The pinned library's own libc declares the callee so
//! (library/vendor/libc-0.2.191/src/unix/mod.rs:848):
//! `pub fn snprintf(s: *mut c_char, n: size_t, format: *const c_char, ...) -> c_int;`
//! Rust makes no default promotion at a variadic call: an `f32`, `bool` or integer narrower
//! than `c_int` passed to `...` is a type error (E0617), so every class here is already the
//! promoted C type and the builder promotes nothing.
//!
//! Where they go (the convention, not a tool's output):
//! - AAPCS64 (Linux on arm64), section 6.8: variadic arguments are assigned exactly as named
//!   ones, so after `buf`, `n`, `fmt` in `x0..x2` the integers and the pointer take `x3`, `x4`,
//!   `x5`, the `f64` takes `v0` (C.1) and the `i64` takes `x6`.
//! - Apple arm64 ("Writing ARM64 code for Apple platforms", "Update code that passes arguments
//!   to variadic functions"): "the caller places all variadic arguments on the stack", each in
//!   an 8-byte-aligned slot; so the five take stack slots 0, 8, 16, 24 and 32, 8 bytes each,
//!   and no register is used for them, the `f64` included.
//!
//! A wrong fix fails here: one that leaves the `...` arguments in registers on Apple (the
//! callee's `va_arg` then reads garbage from the stack), one that packs a variadic `i32` into a
//! 4-byte slot as Apple does for named stack arguments, or one that stacks them on Linux.
//!
//! Run: `lanecheck.sh blaze-build test '--test variadic_call'`; it prints
//! "test blaze-build: exit 0; 1 passed, 0 failed".

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_build::cabi::{ArgLoc, Class, Ext, Place, Platform, assign};

/// `char *`, `size_t`, `const char *`: the three declared parameters.
const PTR: Class = Class::Int { size: 8, signed: false };
const SIZE_T: Class = Class::Int { size: 8, signed: false };
/// The variadic arguments, already `c_int`, a pointer, `c_int`, `double`, `long`.
const I32: Class = Class::Int { size: 4, signed: true };
const F64: Class = Class::Float { size: 8 };
const I64: Class = Class::Int { size: 8, signed: true };

/// The classes of `snprintf(buf, n, fmt, 42i32, ptr, 122i32, 2.5f64, -9000000000i64)`.
const ARGS: [Class; 8] = [PTR, SIZE_T, PTR, I32, PTR, I32, F64, I64];
/// The declared parameter count: the `...` starts at argument 3.
const DECLARED: usize = 3;

const fn gpr(first: u8) -> ArgLoc {
    ArgLoc::Direct { at: Place::Gpr { first, count: 1 }, ext: Ext::None }
}

const fn fpr(first: u8) -> ArgLoc {
    ArgLoc::Direct { at: Place::Fpr { first, count: 1 }, ext: Ext::None }
}

const fn slot(offset: u32) -> ArgLoc {
    ArgLoc::Direct { at: Place::Stack { offset, size: 8 }, ext: Ext::None }
}

fn locs(platform: Platform) -> Vec<ArgLoc, Global> {
    let mut out = Vec::new_in(Global);
    match assign(&ARGS, Some(DECLARED), platform, &mut out) {
        Ok(()) => out,
        Err(error) => panic!("snprintf's arguments on {platform:?} have no places: {error}"),
    }
}

#[test]
fn snprintf_variadic_call_puts_the_varargs_on_the_stack_on_apple_and_in_registers_on_linux() {
    // Apple: the declared three in x0..x2, the five `...` arguments in 8-byte stack slots.
    let apple = locs(Platform::Apple);
    assert_eq!(apple[..], [gpr(0), gpr(1), gpr(2), slot(0), slot(8), slot(16), slot(24), slot(32)], "Apple arm64");

    // Linux arm64 (AAPCS64 as written): variadic arguments as named ones.
    let linux = locs(Platform::Aapcs);
    assert_eq!(linux[..], [gpr(0), gpr(1), gpr(2), gpr(3), gpr(4), gpr(5), fpr(0), gpr(6)], "Linux arm64");

    // The same call declared without `...` (a wrong `variadic_from`) puts them in registers on
    // Apple too: the flag is what moves them.
    let mut out = Vec::new_in(Global);
    let placed = assign(&ARGS, None, Platform::Apple, &mut out);
    assert!(placed.is_ok(), "the same classes as a non-variadic call");
    assert_eq!(out[..], linux[..], "Apple without `...` is AAPCS64's register assignment");
}

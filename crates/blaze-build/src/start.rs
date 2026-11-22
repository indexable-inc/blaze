//! The C entry of a Rust program: the one function the process's loader calls, which starts
//! the program through the library's `#[lang = "start"]` item
//! (port27/tempo-run/LADDER.md, "L1. The Rust entry through `#[lang = \"start\"]`").
//!
//! The Rust Reference, "Crates and source files", `[crate.main]`: "A crate that contains a
//! `main` function can be compiled to an executable. If a `main` function is present, it must
//! take no arguments, must not declare any trait or lifetime bounds, must not have any where
//! clauses, and its return type must implement the `Termination` trait." What runs around that
//! `main` is the library's, not the language's: std's rt.rs (nightly-2026-10-27, line 198):
//!
//! ```text
//! #[lang = "start"]
//! fn lang_start<T: crate::process::Termination + 'static>(
//!     main: fn() -> T,
//!     argc: isize,
//!     argv: *const *const u8,
//!     sigpipe: u8,
//! ) -> isize
//! ```
//!
//! which runs `rt::init` (the arguments, the main thread's name and handle), `main` inside
//! `catch_unwind` (a panic is status 101), `Termination::report` on its value, and `rt::cleanup`
//! (stdout's flush). Calling `main` directly skips every one of them; so the program's C entry
//! is built here as an ordinary LIR function that every tier and the evaluator run alike, never
//! a hand-encoded machine word:
//!
//! ```text
//! extern "C" fn main(argc: i32, argv: *const *const c_char) -> i32 {
//!     lang_start::<T>(root_main as fn() -> T, argc as isize, argv, 0) as i32
//! }
//! ```
//!
//! `0` is `sigpipe::DEFAULT` (std's sys/pal/unix/mod.rs: `pub const DEFAULT: u8 = 0;`, which
//! `reset_sigpipe` turns into `SIG_IGN` for a program without `#[unix_sigpipe]`). `argc as
//! isize` sign-extends (the Reference, "Type cast expressions": "Casting from a smaller integer
//! to a larger integer (e.g. u8 -> u32) will ... sign-extend if the source is signed");
//! `isize as i32` truncates ("Casting from a larger integer to a smaller integer (e.g. u32 ->
//! u8) will truncate"): the low 32 bits of `lang_start`'s status are the C `main`'s result, as
//! the C runtime's `exit` takes it.
//!
//! The call is a plain `call`, never an `invoke`: `lang_start_internal` catches every unwind
//! out of `main` itself (rt.rs: "Guard against the code called by this function from unwinding
//! outside of the Rust-controlled code"), so nothing unwinds out of `lang_start` into the shim.
//!
//! `lang_start::<T>` is called by blaze's Rust ABI (lib.rs, "blaze's Rust ABI"): over the
//! target's C convention, `fn() -> T` as one pointer, `isize` as one integer of the pointer's
//! width, `argv` as one pointer, `u8` zero-extended (an integer under 32 bits is widened by its
//! signedness), its `isize` result in one register: the same lowering the walk gives the
//! callee's own body (walk.rs `rust_pass` and `abi_param`), so the two agree.

use core::alloc::Allocator;

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Ext, RefId};
use blaze_lir::{Builder, Flags, Full, Op, Ty};

use crate::BuildError;

/// std's `sigpipe::DEFAULT` (sys/pal/unix/mod.rs, nightly-2026-10-27): the value rustc's entry
/// passes for a program that names no `#[unix_sigpipe]` attribute.
pub const SIGPIPE_DEFAULT: u8 = 0;

/// Builds into `b` (a builder no block has been begun in) the program's C entry
/// `extern "C" fn(i32, *const *const c_char) -> i32`: `argc` sign-extended to `isize`, the
/// address of `main` (the root's `fn main`, by its reference), [`SIGPIPE_DEFAULT`], the call of
/// `lang_start` (the instance `lang_start::<T>`, `T` main's return type, by its reference), and
/// its `isize` status truncated to `i32` and returned.
///
/// Both references are the caller's: places in the instance's references it made with
/// `b.reference(RefKind::Func, place)`, `lang_start` to the instance and `main` to the root's
/// `fn main` at no arguments. Refused by name: a target whose pointers are neither 32 nor 64
/// bits wide (`isize` is then no width this shim casts to), and any refusal of the builder
/// itself (a block already begun, a full table), as [`BuildError::Lir`].
pub fn c_main_shim<A: Allocator + Clone>(b: &mut Builder<A, Full>, lang_start: RefId, main: RefId) -> Result<(), BuildError> {
    let isize_ty = match b.ptr_bits() {
        64 => Ty::I64,
        32 => Ty::I32,
        _ => {
            return Err(BuildError::UnsupportedSig {
                what: "the C entry shim on a target whose pointers are neither 32 nor 64 bits wide (`argc as isize` has no width)",
            });
        }
    };
    // The shim's own C signature: `(argc: i32, argv: ptr) -> i32`. A 32-bit integer needs no
    // widening under the C convention.
    let own = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::PTR)], &[Param::of(Ty::I32)]);
    b.set_signature(own);
    // `lang_start::<T>` by blaze's Rust ABI: `(main: ptr, argc: isize, argv: ptr, sigpipe: u8
    // zero-extended) -> isize`.
    let callee = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(isize_ty), Param::of(Ty::PTR), Param { ext: Ext::Zext, ..Param::of(Ty::I8) }],
        &[Param::of(isize_ty)],
    );
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let argc = b.param(Ty::I32);
    let argv = b.param(Ty::PTR);
    // `argc as isize`: a sign extension where `isize` is wider than `i32`, nothing where it is
    // `i32` itself.
    let argc_wide = if isize_ty == Ty::I32 { argc } else { b.convert(Op::Sext, Flags::NONE, isize_ty, argc) };
    // `root_main as fn() -> T`: the function's address.
    let main_addr = b.func_addr(main);
    let sigpipe = b.iconst(Ty::I8, u128::from(SIGPIPE_DEFAULT));
    let status = b.call(Flags::NONE, lang_start, callee, &[main_addr, argc_wide, argv, sigpipe]);
    // `status as i32`: the low 32 bits.
    let low = if isize_ty == Ty::I32 { status } else { b.convert(Op::Trunc, Flags::NONE, Ty::I32, status) };
    b.ret(&[low]);
    match b.error() {
        Some(error) => Err(BuildError::Lir(error.clone())),
        None => Ok(()),
    }
}

//! The first stage of the compiler at the speed of the vector unit.
//!
//! Four pieces, each written once in `core::simd` and once as a scalar twin that the tests hold
//! it to:
//!
//! - [`classify`]: the classes of 64 bytes at once as one `u64` mask per class. Two pairs of
//!   16-entry nibble tables (`swizzle_dyn`: `tbl` on aarch64, `vpshufb` under AVX2), one `&`
//!   per pair; a byte is in a class when its low nibble's row and high nibble's row share the
//!   class's bit.
//! - [`lexer`]: [`tokenize`], `rustc_lexer::tokenize`'s exact tokens (kinds, lengths, error
//!   kinds), with identifier, whitespace and digit runs taken by mask and `trailing_ones`, and
//!   comments and strings by byte search.
//! - [`skip`]: [`skip_body`], the offset of the `}` that closes the `{` a text starts with,
//!   without making tokens: a structural scan over the bytes that can open or close anything,
//!   handing one token at a time to the exact lexer where masks cannot decide.
//! - [`utf8`]: [`validate`], Keiser and Lemire's three-table check with all-ASCII blocks taken
//!   by one test, the one proof from `&[u8]` to `&str` ([`Utf8`]).
//!
//! The vector width is the caller's [`Cpu`]: the scalar twin, the target's baseline vectors
//! (NEON on aarch64, SSE2 on x86-64), or AVX2, which the caller detects once and passes in
//! (rule 75); nothing here detects anything.
//!
//! The floor aimed at (rule 67), on one core of an Apple M-series or a Zen 4 at about 3.5 GHz:
//! - UTF-8: memory bandwidth on ASCII (one load and one OR per 16 bytes, about 0.1 cycle a
//!   byte); 0.5 cycle a byte on non-ASCII blocks.
//! - The skipper: the classification of every byte once (two lookups, an `&` and one mask per
//!   16 bytes, about 0.3 cycle a byte) plus one scalar step per byte that can open or close
//!   anything, about one in twelve bytes of Rust; about 1 cycle a byte in all.
//! - The lexer: one token every 4 to 5 bytes of real Rust, each a dispatch on its first byte, a
//!   run by `trailing_ones` and a store: about 12 cycles a token, 3 cycles a byte (about 1 GB/s).
//!
//! `examples/cost.rs` prints each piece's MB/s and ns per line over `library/`.
//!
//! Measured 2026-11-15, one run of `examples/cost.rs` (release) on the owner's Mac (aarch64,
//! 18 threads), over library/: 3,130 files, 1,525,930 lines, 57.0 MB.
//! - `rustc_lexer::tokenize` 531.6 MB/s on 1 thread, 6,819.5 MB/s on 18. [`tokenize`] with
//!   vectors 642.2 MB/s (1.21x) and 7,926.6 MB/s (1.16x): 0.64 of the 1 GB/s floor. The scalar
//!   twin then ran 254.4 MB/s, half rustc_lexer's, because its runs built all three run masks of
//!   each 64-byte block a byte at a time; it now tests each byte of a run once.
//! - Tokenizing and counting braces over every fn body (21.7 MB) with rustc_lexer 439.7 MB/s;
//!   [`skip_body`] 3,391.6 MB/s on 1 thread (7.7x; 0.97 of the floor) and 31,401.9 MB/s on 18;
//!   scalar 1,014.9 MB/s.
//! - `core::str::from_utf8` 47,110 MB/s; [`validate`] 59,872 MB/s (1.27x); scalar 4,131 MB/s.

#![no_std]
#![feature(portable_simd, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core, blaze::std_use, blaze::implicit_allocator, blaze::process_static)]
#![blaze::trust(
    unsafe,
    reason = "three kinds, each stated where it is: `vpshufb` called from code that runs only when an `Avx2` value proves the CPU has AVX2; `from_utf8_unchecked` on bytes the validator proved; and `Cpu::avx2_unchecked`, whose caller promises the feature"
)]

#[cfg(test)]
extern crate std;

pub mod classify;
mod cpu;
pub mod lexer;
pub mod skip;
pub mod utf8;
mod vector;

#[cfg(test)]
mod tests;

pub use classify::{Masks, classify};
pub use cpu::Cpu;
pub use lexer::{Base, DocStyle, FrontmatterAllowed, LiteralKind, Token, TokenKind, Tokens, tokenize};
pub use skip::{SkipError, skip_body};
pub use utf8::{Invalid, Utf8, validate};

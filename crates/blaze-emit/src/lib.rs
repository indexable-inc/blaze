//! The baseline tier (docs/v2/plan/design/backend-next.md): one LIR function to AArch64
//! machine code in one forward pass, with no optimization. It is the first backend that makes
//! `blaze run` run a program (crates/blaze-cli/tests/ladder/README.md); it does not replace the
//! copy-and-patch stencil backend (crates/blaze-stencil, parked), which is the fast tier this
//! one stands in for until it is back.
//!
//! # The code it makes
//!
//! - Every LIR value (an instruction's result or a block parameter) has its own 16-byte stack
//!   cell, and every LIR stack slot its own aligned stretch of the frame. An instruction loads
//!   its operands into scratch registers, computes, and stores its result to its cell.
//! - The frame: `stp x29, x30, [sp, #-16]!; mov x29, sp; sub sp, sp, #frame`; the epilogue
//!   undoes it. Parameters arrive and results leave in the platform's C convention (AAPCS64,
//!   Apple's variant on macOS), which is also the convention of Rust-ABI functions over their
//!   lowered signatures (blaze-lir's `Conv`).
//! - A call of reference `place` loads an 8-byte literal word placed after the code, then:
//!   for [`Via::Slot`], loads the slot word that literal points at and branches to it (the
//!   launcher's slot table, docs/v2/plan/design/run.md); for [`Via::Address`], branches to
//!   the literal's value (an outside symbol). Placement fills each word ([`Fixup`]).
//! - The function's own data (`data_addr` of a `RefKind::Data` reference) is copied after the
//!   literal words and reached PC-relatively (`adr`): no fixup.
//!
//! - The code is followed by its literal words (8-aligned) and its data rows (each at least
//!   8-aligned, at most 16): placement puts the code's first byte at a 16-aligned address.
//!
//! # Values
//!
//! One invariant holds for every cell, kept on every write: **a cell's 8 low bytes hold the
//! value zero-extended to 64 bits** (an `i1` is 0 or 1, an `i8` 0..=255; a pointer is its 64
//! bits). An operation computes in whole 64-bit registers: one that only needs the low bits
//! right (`add`, `sub`, `mul`, `neg`, `not`, `shl`, `trunc`) zero-extends its result back to the
//! width; one that reads the sign (`sdiv`, `srem`, `ashr`, a signed `icmp`, `sext`) sign-extends
//! its operands first. A parameter or a call's return arrives with its bits above the width
//! unspecified and is zero-extended at once.
//!
//! # Traps
//!
//! A LIR `trap` is `brk #code` (codes below 0xff00); division by zero is `brk #0xff00`,
//! `sdiv` of `MIN` by -1 `brk #0xff01`, `unreachable` `brk #0xff02`.
//!
//! # The subset
//!
//! Types `i1`, `i8`, `i16`, `i32`, `i64` and `ptr` (64-bit targets). Operations `param`,
//! `iconst`, `iconst_wide`, `null`, `add`, `sub`, `mul`, `neg`, `udiv`, `sdiv`, `urem`,
//! `srem`, `and`, `or`, `xor`, `not`, `shl`, `lshr`, `ashr`, `icmp`, `select`, `zext`,
//! `sext`, `trunc`, `freeze`, `slot_addr`, `slot_live`, `slot_dead`, `ptr_add`, `ptr_addr`,
//! `load`, `store`, `data_addr` (own data without relocations), `func_addr` (of a
//! [`Via::Address`] function), `call` (C convention, at most 8 integer arguments and one
//! integer return, no `sret` or `byval`, not variadic), `jump`, `br`, `switch`, `return`,
//! `trap`, `unreachable`. Promise and hint flags are dropped (they only allow UB or say
//! nothing); `volatile` on a load or store is honoured (each is one access of its width);
//! every other meaning flag is refused.
//!
//! Anything outside the subset is [`EmitError::Unsupported`], naming the instruction: never a
//! guess.

#![feature(allocator_api)]

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::{Op, View};

mod enc;
mod pass;

#[cfg(test)]
mod tests;

/// How a call of one function reference is made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Via {
    /// Through a launcher slot: the literal word holds the slot word's address.
    Slot,
    /// Directly: the literal word holds the function's address (an outside symbol).
    Address,
}

/// An 8-byte literal word of the code that placement fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixup {
    /// The word's offset from the code's first byte (8-byte aligned).
    pub at: u32,
    /// The function reference's place (`RefKind::Func`) whose address goes there: for
    /// [`Via::Slot`] its slot word's address, for [`Via::Address`] the function's.
    pub place: u32,
}

/// One function's machine code.
pub struct Code<A: Allocator> {
    pub bytes: Vec<u8, A>,
    pub fixups: Vec<Fixup, A>,
}

impl<A: Allocator + Clone> Code<A> {
    pub fn new_in(alloc: A) -> Code<A> {
        Code { bytes: Vec::new_in(alloc.clone()), fixups: Vec::new_in(alloc) }
    }
}

/// Why a function was not emitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmitError {
    /// Instruction `inst` is outside the baseline tier's subset.
    Unsupported { inst: u32, op: Op, why: &'static str },
    /// A reference `place` the function names has no entry in the `via` table given.
    NoVia { place: u32 },
    /// The frame or the code passed a bound of the encoding.
    TooLarge { what: &'static str },
    /// The function's own signature, or its target, is outside the subset.
    Signature { why: &'static str },
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmitError::Unsupported { inst, op, why } => {
                write!(f, "instruction {inst} (`{}`): the baseline tier does not emit it: {why}", op.name())
            }
            EmitError::NoVia { place } => write!(f, "function reference {place} has no call kind (`Via`) given"),
            EmitError::TooLarge { what } => write!(f, "{what} is past what the baseline tier encodes"),
            EmitError::Signature { why } => write!(f, "the function's signature: the baseline tier does not emit {why}"),
        }
    }
}

impl core::error::Error for EmitError {}

/// Emits `func` (verified by `blaze_lir::verify`) into `out`, which is cleared first. `via` has
/// one entry per function reference place the function names.
pub fn emit<A: Allocator>(func: View<'_>, via: &[Via], out: &mut Code<A>) -> Result<(), EmitError> {
    out.bytes.clear();
    out.fixups.clear();
    pass::run(func, via, &mut out.bytes, &mut out.fixups)
}

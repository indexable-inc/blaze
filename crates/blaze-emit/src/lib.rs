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
//! Anything outside the subset is [`EmitError::Unsupported`], naming the instruction: never a
//! guess.

#![feature(allocator_api)]

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::{Op, View};

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
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmitError::Unsupported { inst, op, why } => {
                write!(f, "instruction {inst} (`{}`): the baseline tier does not emit it: {why}", op.name())
            }
            EmitError::NoVia { place } => write!(f, "function reference {place} has no call kind (`Via`) given"),
            EmitError::TooLarge { what } => write!(f, "{what} is past what the baseline tier encodes"),
        }
    }
}

impl core::error::Error for EmitError {}

/// Emits `func` (verified by `blaze_lir::verify`) into `out`, which is cleared first. `via` has
/// one entry per function reference place the function names.
pub fn emit<A: Allocator>(func: View<'_>, via: &[Via], out: &mut Code<A>) -> Result<(), EmitError> {
    let _ = (func, via);
    out.bytes.clear();
    out.fixups.clear();
    Err(EmitError::TooLarge { what: "every function (the emitter is being written)" })
}

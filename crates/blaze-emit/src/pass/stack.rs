//! Named arguments past the registers: the stacked argument area of a call, on both sides
//! (AAPCS64, "Parameter passing", stage C; Apple's variant on macOS).
//!
//! Which arguments are stacked is the register walk's answer ([`super::Regs`]): an integer or
//! pointer past `x7`, an `i128` whose pair does not fit below `x8` (C.11: the NGRN is then 8,
//! so every later integer is stacked too; the walk's `x` goes past 8 and stays there), a float
//! past `v7` (C.5..C.6: the NSRN is then 8). Floats and integers are counted apart, so a later
//! integer can still take a register after a float was stacked, and the other way round.
//!
//! Where each stacked one goes is the next stacked argument address, the NSAA, starting at the
//! caller's `sp` at the call:
//!
//! - **Linux** (the generic AAPCS64, C.14..C.17): the NSAA is rounded up to the larger of 8 and
//!   the argument's natural alignment, and the argument takes the larger of 8 and its size ("as
//!   if the argument was copied to the least significant bits of a 64-bit register", the bits
//!   above it unspecified). So `i8..i64`, pointers, `f16..f64` take one 8-byte slot, `i128` and
//!   `f128` a 16-aligned 16-byte one.
//! - **Apple arm64** ("Writing ARM64 code for Apple platforms": "Function arguments may consume
//!   slots on the stack that are not multiples of 8 bytes"): each stacked argument takes its own
//!   size at its natural alignment (an `i8` or a `bool` one byte, an `i16` or `f16` two, an
//!   `i32` or `f32` four, an `i64`, pointer or `f64` eight, an `i128` or `f128` sixteen, 16-
//!   aligned). The caller's extension of a sub-32-bit argument (Apple's rule, pass.rs `call`)
//!   is a register's: a stacked one is its bytes.
//!
//! The area is rounded up to 16 at the call (AAPCS64 6.4.5.1: `sp` is 16-aligned at a public
//! interface). The caller opens it with `sub sp` after the register arguments are loaded and
//! before the callee's address is (pass/variadic.rs's order, which the call site needs), fills
//! it through `x9`, and closes it after the `blr`; while it is open every cell is `area` bytes
//! higher above `sp`. A C-variadic call's stacked `...` values follow the named ones in the same
//! area, from the named end rounded up to 8.
//!
//! The callee finds its stacked parameters at `x29 + 16 + offset`: the prologue's
//! `stp x29, x30, [sp, #-16]!; mov x29, sp` puts the caller's `sp` at the call 16 above `x29`.
//!
//! A landing block is entered with `sp` as the unwinder restored it, which is the `sp` at the
//! call: lowered by the area when an `invoke` had stacked arguments. So every landing block
//! first puts `sp` back where the prologue left it ([`Pass::restore_sp`]), from `x29` and the
//! frame's size, patched once the frame is known.

use alloc::alloc::Allocator;

use blaze_lir::{Scalar, Ty, Value};

use super::{EmitError, FloatWidth, Pass};
use crate::Os;
use crate::enc::{self, FP, Reg, SP, Size, X9, X16};

/// One stacked argument's place: its offset in the area and the bytes it takes there (1, 2, 4,
/// 8 or 16).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Stacked {
    pub(crate) at: u32,
    pub(crate) bytes: u32,
}

/// The NSAA walk of one signature: the next stacked argument's address, from 0.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Nsaa {
    os: Os,
    next: u32,
}

/// A value's natural size in bytes, which is also its alignment: every scalar LIR type a
/// signature takes (pass.rs `check_sig`); `None` for any other.
pub(crate) fn natural(ty: Ty) -> Option<u32> {
    if !ty.is_scalar() {
        return None;
    }
    match ty.elem() {
        Scalar::I1 | Scalar::I8 => Some(1),
        Scalar::I16 | Scalar::F16 => Some(2),
        Scalar::I32 | Scalar::F32 => Some(4),
        Scalar::I64 | Scalar::Ptr | Scalar::F64 => Some(8),
        Scalar::I128 | Scalar::F128 => Some(16),
        Scalar::None => None,
    }
}

impl Nsaa {
    pub(crate) const fn new(os: Os) -> Nsaa {
        Nsaa { os, next: 0 }
    }

    /// The place of the next stacked argument, of type `ty` (the module's rule for `os`).
    pub(crate) fn place(&mut self, ty: Ty) -> Option<Stacked> {
        let n = natural(ty)?;
        let bytes = match self.os {
            Os::MacOs => n,
            Os::Linux => n.max(8),
        };
        let at = self.next.next_multiple_of(bytes);
        self.next = at.checked_add(bytes)?;
        Some(Stacked { at, bytes })
    }

    /// The bytes the stacked arguments so far take.
    pub(crate) const fn end(&self) -> u32 {
        self.next
    }
}

/// A named argument of a call that goes on the stack: its value, its place, the width it is
/// sign-extended from first (0: none), and for a `byval` one the offset of its copy in the
/// call's stretch of the frame (the pointer to the copy is what is stacked).
#[derive(Clone, Copy, Debug)]
pub(crate) struct StackedArg {
    pub(crate) value: Value,
    pub(crate) slot: Stacked,
    pub(crate) sext: u32,
    pub(crate) byval: Option<u32>,
}

/// The access of `bytes` (1, 2, 4 or 8).
fn size_of_bytes(bytes: u32) -> Size {
    match bytes {
        1 => Size::B,
        2 => Size::H,
        4 => Size::W,
        _ => Size::X,
    }
}

/// The most bytes a call's stacked area may take: `sub sp` by two 12-bit immediates (the second
/// shifted), and every cell's offset stays below the frame's bound while it is open.
pub(crate) const MOST_AREA: u32 = 1 << 20;

impl<A: Allocator> Pass<'_, '_, A> {
    /// The bits of the cell value a scalar of `ty` holds: its integer width or its float's.
    fn cell_bits(&self, ty: Ty) -> Option<u32> {
        match FloatWidth::of(ty) {
            Some(FloatWidth::Half) => Some(16),
            Some(FloatWidth::Single) => Some(32),
            Some(FloatWidth::Double) => Some(64),
            Some(FloatWidth::Quad) => Some(128),
            None if ty == Ty::I128 => Some(128),
            None => self.width(ty),
        }
    }

    /// An entry parameter of `ty` stacked at `slot` (the caller's area, `x29 + 16` up) to
    /// `inst`'s cell, zero-extended from its width (the cell invariant: a Linux slot's bits above
    /// the value are unspecified).
    pub(super) fn stacked_in(&mut self, inst: usize, ty: Ty, slot: Stacked) -> Result<(), EmitError> {
        let bits = self.cell_bits(ty).ok_or(EmitError::Signature { why: "a stacked parameter that is not a scalar" })?;
        let from = 16 + slot.at;
        let from = i32::try_from(from).map_err(|_| EmitError::TooLarge { what: "a stacked parameter's offset" })?;
        if bits == 128 {
            self.mem(true, Size::X, X9, FP, from);
            self.st_sp(X9, inst as u32 * 16);
            self.mem(true, Size::X, X9, FP, from + 8);
            self.st_sp(X9, inst as u32 * 16 + 8);
            return Ok(());
        }
        self.mem(true, size_of_bytes(slot.bytes), X9, FP, from);
        if bits < 8 * slot.bytes {
            self.narrow(X9, bits);
        }
        self.st(X9, inst);
        Ok(())
    }

    /// `rt` to the `size` bytes at `sp + off` (any `off` below 16 MiB): `str` with its scaled
    /// immediate where it reaches, else through [`super::FAR`].
    pub(super) fn st_sp_sized(&mut self, size: Size, rt: Reg, off: u32) {
        debug_assert_ne!(rt, super::FAR, "a value stored from the far-address register (pass.rs `FAR`)");
        let scale = size as u32;
        match off % (1 << scale) == 0 && (off >> scale) < 4096 {
            true => self.put(enc::str(size, rt, SP, off >> scale)),
            false => {
                self.sp_addr(super::FAR, off);
                self.put(enc::str(size, rt, super::FAR, 0));
            }
        }
    }

    /// Opens the call's area of `area` bytes (a multiple of 16 below [`MOST_AREA`]): `sub sp`.
    pub(super) fn open_area(&mut self, area: u32) {
        if area >> 12 != 0 {
            self.put(enc::sub_imm(SP, SP, area >> 12, true));
        }
        if area & 0xfff != 0 {
            self.put(enc::sub_imm(SP, SP, area & 0xfff, false));
        }
    }

    /// Closes it: `add sp`, the same immediates.
    pub(super) fn close_area(&mut self, area: u32) {
        if area >> 12 != 0 {
            self.put(enc::add_imm(SP, SP, area >> 12, true));
        }
        if area & 0xfff != 0 {
            self.put(enc::add_imm(SP, SP, area & 0xfff, false));
        }
    }

    /// Stores the named stacked arguments into the open area of `area` bytes, through `x9`
    /// (and `FAR` for far offsets): no argument register is written.
    pub(super) fn store_stacked(&mut self, named: &[StackedArg], area: u32) {
        for arg in named {
            let (at, bytes) = (arg.slot.at, arg.slot.bytes);
            if let Some(copy) = arg.byval {
                // The copy's address; its stretch is `area` farther from the lowered `sp`.
                self.byval_addr(X9, copy + area);
                self.st_sp_sized(Size::X, X9, at);
                continue;
            }
            let cell = arg.value.0 * 16 + area;
            if bytes == 16 {
                self.ld_sp(X9, cell);
                self.st_sp_sized(Size::X, X9, at);
                self.ld_sp(X9, cell + 8);
                self.st_sp_sized(Size::X, X9, at + 8);
                continue;
            }
            self.ld_sp(X9, cell);
            if arg.sext != 0 {
                self.widen_signed(X9, arg.sext);
            }
            self.st_sp_sized(size_of_bytes(bytes), X9, at);
        }
    }

    /// At a landing block's first byte: `sp` put back where the prologue left it, from `x29`
    /// (the unwinder restored `sp` to its value at the call, lower by an open area). The two
    /// `sub`s are patched with the frame's size in `finish` ([`Pass::patch_restores`]).
    pub(super) fn restore_sp(&mut self) {
        self.sp_fixes.push(self.here());
        self.put(enc::sub_imm(X16, FP, 0, true));
        self.put(enc::sub_imm(X16, X16, 0, false));
        if self.most_align > 16 {
            let log2 = self.most_align.trailing_zeros();
            self.put(enc::lsr_imm(X16, X16, log2));
            self.put(enc::lsl_imm(X16, X16, log2));
        }
        self.put(enc::add_imm(SP, X16, 0, false));
    }

    /// Writes the frame's size `frame` into every [`Pass::restore_sp`].
    pub(super) fn patch_restores(&mut self, frame: u32) {
        let fixes = core::mem::take(&mut self.sp_fixes);
        for &at in fixes.iter() {
            self.set(at, enc::sub_imm(X16, FP, frame >> 12, true));
            self.set(at + 4, enc::sub_imm(X16, X16, frame & 0xfff, false));
        }
        self.sp_fixes = fixes;
    }
}

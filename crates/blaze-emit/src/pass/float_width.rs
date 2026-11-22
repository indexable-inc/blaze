//! The float widths the Reference tier takes: the one home of that answer (AGENTS.md rule 30).
//! Every scalar float type of LIR is one of them; `f32` and `f64` are computed in
//! (pass/float.rs), `f16` and `f128` only moved.
//!
//! Why moving is enough: blaze-lir's legalize runs before this tier (routines.rs `legalized`),
//! and its module doc says what it leaves: "What the tiers must still take after this pass:
//! `f16` and `f128` values that are only moved (loaded, stored, selected, passed, bitcast),
//! `fpext f16 -> f32` and `fptrunc f32 -> f16`". Every `f128` operation became a call of the
//! program's compiler_builtins routine (`__addtf3` and the rest, routines.rs), so an `f128`
//! operation that still reaches the tier is refused by name, naming the instruction and its
//! op ([`EmitError::Unsupported`]): never computed here, never a fallback.
//!
//! Cells (lib.rs, "Values"): an `f16`'s cell holds its 16 bits zero-extended, as an `i16`'s
//! does; an `f128`'s 16-byte cell holds the low 64 bits of its binary128 encoding at 0 and the
//! high 64 at 8, as an `i128`'s does (pass/wide.rs), which is also the byte order `str q`
//! writes on a little-endian machine. So a load, store, select, freeze or bitcast to the
//! integer of its width moves the cell as that integer's move does.
//!
//! Passing (AAPCS64, "Parameter passing", rule C.1): a half-, single-, double- or
//! quad-precision floating-point argument goes in the least significant bits of `v[NSRN]`
//! while NSRN is below 8, and NSRN goes up by one; a result is placed as the first argument
//! would be (`v0`). So the register walk ([`super::Regs`]) gives each width the next `v`
//! register, and the width rides in [`super::Loc::V`]. An `f128` moves between its cell and
//! `q<n>` by one 16-byte `ldr`/`str`; an `f16` through `w9`: `fmov s<n>, w9` (h<n> is the
//! low 16 bits; AAPCS64 leaves the bits above unspecified) and `umov w9, v<n>.h[0]` back
//! (zero-extending: the cell invariant).
//!
//! The conversions of `f16` (the Reference, "Numeric cast": "Casting from an f32 to an f64
//! is perfect and lossless", the same of every narrower float to a wider one; "from an f64 to
//! an f32 will produce the closest possible f32", the same of every narrowing): `fcvt` from
//! `h` to `s` or `d` is exact, and `fcvt` from `s` or `d` to `h` rounds by FPCR (round to
//! nearest, ties to even, which no code here changes).
//!
//! This emitter is arm64's only (`Isa` names no other architecture): an x86-64 placement of
//! `f128` (SysV's SSE class, `xmm`) belongs to the tier that emits x86-64.

use alloc::alloc::Allocator;

use blaze_lir::{Op, Scalar, Ty, Value, View};

use super::{EmitError, Pass, unsupported};
use crate::enc::{self, Cond, SP, Size, X0, X1, X9, X10, X11};

/// `v0`: where an `f16` conversion computes.
const V0: u32 = 0;

/// A scalar float type, by its width: each one the tier takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FloatWidth {
    /// `f16`: moved, and converted to and from `f32` and `f64`.
    Half,
    /// `f32`: computed in (pass/float.rs).
    Single,
    /// `f64`: computed in (pass/float.rs).
    Double,
    /// `f128`: moved only; its arithmetic is legalize's calls of compiler_builtins' routines.
    Quad,
}

impl FloatWidth {
    /// The width of `ty` if it is a scalar float. Every scalar is named, so a new float
    /// scalar in blaze-lir is a compile error here, not a quiet refusal.
    pub(crate) fn of(ty: Ty) -> Option<FloatWidth> {
        if !ty.is_scalar() {
            return None;
        }
        match ty.elem() {
            Scalar::F16 => Some(FloatWidth::Half),
            Scalar::F32 => Some(FloatWidth::Single),
            Scalar::F64 => Some(FloatWidth::Double),
            Scalar::F128 => Some(FloatWidth::Quad),
            Scalar::None
            | Scalar::I1
            | Scalar::I8
            | Scalar::I16
            | Scalar::I32
            | Scalar::I64
            | Scalar::I128
            | Scalar::Ptr => None,
        }
    }

    /// Whether the tier computes in this width: `Some(double)` (pass/float.rs's flag: `false`
    /// for `f32`, `true` for `f64`); `None` for a width it only moves.
    pub(crate) const fn computed(self) -> Option<bool> {
        match self {
            FloatWidth::Single => Some(false),
            FloatWidth::Double => Some(true),
            FloatWidth::Half | FloatWidth::Quad => None,
        }
    }

    /// The 8-byte words of its cell a value of this width fills.
    pub(crate) const fn words(self) -> u32 {
        match self {
            FloatWidth::Quad => 2,
            FloatWidth::Half | FloatWidth::Single | FloatWidth::Double => 1,
        }
    }

    /// Why `op` on a value of this width is refused (a width only moved).
    const fn refused(self) -> &'static str {
        match self {
            FloatWidth::Quad => {
                "an `f128` operation other than a move, a constant or a bitcast to or from `i128` (its arithmetic, \
                 comparisons and conversions are legalize's calls of compiler_builtins' routines)"
            }
            _ => {
                "an `f16` operation other than a move, a constant, a bitcast to or from `i16`, `fpext` to `f32` or \
                 `f64`, or `fptrunc` from them (legalize does its arithmetic in `f32`)"
            }
        }
    }
}

/// The width of the `f16` or `f128` value instruction `inst` (`op`) moves or converts, if it
/// moves one; `None` sends it on to the integer, `i128` and `f32`/`f64` routes (pass.rs
/// `lower`), which refuse an `f16` or `f128` operand by name.
pub(super) fn moved_of(view: View<'_>, inst: usize, op: Op) -> Option<FloatWidth> {
    let only_moved = |ty: Ty| FloatWidth::of(ty).filter(|w| w.computed().is_none());
    let result = view.ty(Value(inst as u32));
    match op {
        Op::Load | Op::Select | Op::Freeze | Op::Fconst | Op::FconstWide => only_moved(result),
        Op::Store => only_moved(view.ty(view.operands(inst).value())),
        Op::Bitcast | Op::Fpext | Op::Fptrunc => {
            only_moved(result).or_else(|| only_moved(view.ty(view.operands(inst).value())))
        }
        _ => None,
    }
}

impl<A: Allocator> Pass<'_, '_, A> {
    /// An entry parameter or a call's return in vector register `v`, of width `w`, to
    /// `inst`'s cell.
    pub(super) fn v_in(&mut self, v: u32, inst: usize, w: FloatWidth) {
        match w {
            FloatWidth::Single | FloatWidth::Double => self.float_in(v, inst, w == FloatWidth::Double),
            FloatWidth::Half => {
                self.put(enc::umov_h0(X9, v));
                self.st(X9, inst);
            }
            FloatWidth::Quad => self.q_cell(false, v, inst as u32 * 16),
        }
    }

    /// `value` (of width `w`) into vector register `v`, as an argument or a return.
    pub(super) fn v_out(&mut self, v: u32, value: Value, w: FloatWidth) {
        match w {
            FloatWidth::Single | FloatWidth::Double => self.float_out(v, value, w == FloatWidth::Double),
            FloatWidth::Half => {
                self.ld(X9, value);
                self.put(enc::fmov_to_s(v, X9));
            }
            FloatWidth::Quad => self.q_cell(true, v, value.0 * 16),
        }
    }

    /// `ldr q<v>` (`load`) or `str q<v>` of the 16 bytes at `sp + off` (a cell: a multiple of
    /// 16); past the scaled immediate's reach through [`super::FAR`], as `ld_sp` does.
    fn q_cell(&mut self, load: bool, v: u32, off: u32) {
        let (base, imm) = match off / 16 < 4096 {
            true => (SP, off / 16),
            false => {
                self.sp_addr(super::FAR, off);
                (super::FAR, 0)
            }
        };
        self.put(match load {
            true => enc::ldr_q(v, base, imm),
            false => enc::str_q(v, base, imm),
        });
    }

    /// `op` at `inst`, which moves or converts an `f16` or `f128` value of width `w`
    /// ([`moved_of`]).
    pub(super) fn moved(&mut self, inst: usize, op: Op, w: FloatWidth) -> Result<(), EmitError> {
        let view = self.view;
        let mut o = view.operands(inst);
        match (w, op) {
            // Two words, as an `i128`'s (pass/wide.rs moves them whatever the type).
            (FloatWidth::Quad, Op::Load | Op::Store | Op::Select) => return self.wide(inst, op),
            (FloatWidth::Quad, Op::Freeze) => {
                let a = o.value();
                self.ld2(X0, X1, a);
                self.st2(X0, X1, inst);
            }
            (FloatWidth::Quad, Op::FconstWide) => {
                let words = o.constant();
                if words.len() != 4 {
                    return Err(unsupported(inst, op, "an `f128` constant of other than four words"));
                }
                self.mov_imm(X0, u64::from(words[0]) | (u64::from(words[1]) << 32));
                self.mov_imm(X1, u64::from(words[2]) | (u64::from(words[3]) << 32));
                self.st2(X0, X1, inst);
            }
            (FloatWidth::Quad, Op::Bitcast) => {
                let a = o.value();
                let (from, to) = (view.ty(a), view.ty(Value(inst as u32)));
                let fits = |t: Ty| t == Ty::I128 || FloatWidth::of(t) == Some(FloatWidth::Quad);
                if !fits(from) || !fits(to) {
                    return Err(unsupported(inst, op, "a bitcast of `f128` to or from other than `i128`"));
                }
                self.ld2(X0, X1, a);
                self.st2(X0, X1, inst);
            }
            (FloatWidth::Half, Op::Load) => {
                let (ptr, off) = (o.value(), o.offset());
                self.ld(X9, ptr);
                self.mem(true, Size::H, X9, X9, off);
                self.st(X9, inst);
            }
            (FloatWidth::Half, Op::Store) => {
                let (value, ptr, off) = (o.value(), o.value(), o.offset());
                self.ld(X9, value);
                self.ld(X10, ptr);
                self.mem(false, Size::H, X9, X10, off);
            }
            (FloatWidth::Half, Op::Select) => {
                let (cond, a, b) = (o.value(), o.value(), o.value());
                self.ld(X9, cond);
                self.ld(X10, a);
                self.ld(X11, b);
                self.put(enc::cmp_imm(X9, 0));
                self.put(enc::csel(X9, X10, X11, Cond::Ne));
                self.st(X9, inst);
            }
            (FloatWidth::Half, Op::Freeze) => {
                let a = o.value();
                self.ld(X9, a);
                self.st(X9, inst);
            }
            (FloatWidth::Half, Op::Fconst) => {
                self.mov_imm(X9, u64::from(o.imm() & 0xffff));
                self.st(X9, inst);
            }
            (FloatWidth::Half, Op::Bitcast) => {
                let a = o.value();
                let (from, to) = (view.ty(a), view.ty(Value(inst as u32)));
                let fits = |t: Ty| t == Ty::I16 || FloatWidth::of(t) == Some(FloatWidth::Half);
                if !fits(from) || !fits(to) {
                    return Err(unsupported(inst, op, "a bitcast of `f16` to or from other than `i16`"));
                }
                self.ld(X9, a);
                self.st(X9, inst);
            }
            // `f16` to `f32` or `f64`: exact.
            (FloatWidth::Half, Op::Fpext) => {
                let a = o.value();
                let to = FloatWidth::of(view.ty(Value(inst as u32))).and_then(FloatWidth::computed);
                let (Some(double), Some(FloatWidth::Half)) = (to, FloatWidth::of(view.ty(a))) else {
                    return Err(unsupported(inst, op, "an `fpext` of `f16` to other than `f32` or `f64`"));
                };
                self.ld(X9, a);
                self.put(enc::fmov_to_s(V0, X9));
                self.put(enc::fcvt_from_h(double, V0, V0));
                self.float_in(V0, inst, double);
            }
            // `f32` or `f64` to `f16`: rounded to nearest, ties to even.
            (FloatWidth::Half, Op::Fptrunc) => {
                let a = o.value();
                let from = FloatWidth::of(view.ty(a)).and_then(FloatWidth::computed);
                let (Some(double), Some(FloatWidth::Half)) = (from, FloatWidth::of(view.ty(Value(inst as u32)))) else {
                    return Err(unsupported(inst, op, "an `fptrunc` to `f16` from other than `f32` or `f64`"));
                };
                self.float_out(V0, a, double);
                self.put(enc::fcvt_to_h(double, V0, V0));
                self.put(enc::umov_h0(X9, V0));
                self.st(X9, inst);
            }
            _ => return Err(unsupported(inst, op, w.refused())),
        }
        Ok(())
    }
}

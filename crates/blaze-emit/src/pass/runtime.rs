//! The operations arm64 has no instruction for, made as calls of the system's routines
//! (lib.rs [`Routine`]: C convention, placement binds each by its symbol).
//!
//! - `frem` at `f64`/`f32` is `fmod`/`fmodf`. The Reference, "Arithmetic and Logical Binary
//!   Operators": `%` on floats is the "Remainder" whose sign is the dividend's ("the remainder
//!   from a truncating division"), which is C's `fmod`: `7.5 % 2.0 == 1.5`, `-7.5 % 2.0 == -1.5`.
//! - `udiv`, `urem`, `sdiv`, `srem` at `i128` are `__udivti3`, `__umodti3`, `__divti3`,
//!   `__modti3` (the pinned toolchain's compiler_builtins, int/udiv.rs and int/sdiv.rs:
//!   `pub extern "C" fn __udivti3(n: u128, d: u128) -> u128`, `__umodti3(n: u128, d: u128) ->
//!   u128`, `sdiv!(__udivti3, __divti3, u128, i128,)`, `smod!(__umodti3, __modti3, u128,
//!   i128,)`). A zero divisor is undefined there (int/specialized_div_rem/mod.rs `zero_div_fn`:
//!   `core::intrinsics::unreachable()`), so it traps first, as at every width (pass.rs
//!   `trap_if_zero`, `BRK_DIV_ZERO`); `sdiv` of `MIN` by `-1` traps `BRK_DIV_OVERFLOW` first,
//!   as at 64 bits; `srem` of `MIN` by `-1` is 0 (LIR's rule), which `__modti3` gives.
//! - `sitofp`/`uitofp` from `i128` are `__floattidf`, `__floatuntidf`, `__floattisf`,
//!   `__floatuntisf` (float/conv.rs: `pub extern "C" fn __floattidf(i: i128) -> f64`, ...):
//!   the Reference's "closest possible float" is their round to nearest, ties to even.
//! - `fptosi`/`fptoui` to `i128` are `__fixdfti`, `__fixunsdfti`, `__fixsfti`,
//!   `__fixunssfti`. The Reference, "Type cast expressions": "`NaN` will return `0`; values
//!   larger than the maximum integer value, including `INFINITY`, will saturate to the maximum
//!   value of the integer type; values smaller than the minimum integer value, including
//!   `NEG_INFINITY`, will saturate to the minimum". compiler_builtins' own routines do that
//!   (float/conv.rs `float_to_int_inner`: `>= max (incl. inf)` gives `out_of_bounds()`, a
//!   `NaN` `I::ZERO`), but placement binds the SYSTEM's (macOS: libSystem's
//!   `libcompiler_rt`; Linux: libgcc_s), whose out-of-range and `NaN` results C leaves
//!   undefined. So the code decides every out-of-range input itself and calls the routine
//!   only with a value whose truncation fits, where every implementation agrees.

use alloc::alloc::Allocator;

use blaze_lir::{Op, Value};

use super::{BRK_DIV_OVERFLOW, EmitError, Pass, unsupported};
use crate::Routine;
use crate::enc::{self, Cond, Field, Reg, X0, X1, X2, X9};

const X3: Reg = 3;
/// `d0`/`s0` and `d1`/`s1`.
const V0: u32 = 0;
const V1: u32 = 1;

/// An `f64`'s bits: 2^128, 2^127, -2^127, -1.0.
const F64_2_128: u64 = (1023 + 128) << 52;
const F64_2_127: u64 = (1023 + 127) << 52;
const F64_NEG_2_127: u64 = (1 << 63) | F64_2_127;
const F64_NEG_1: u64 = (1 << 63) | (1023 << 52);
/// An `f32`'s bits: 2^128 is past `f32::MAX`, so `+inf`; 2^127, -2^127, -1.0.
const F32_INF: u64 = 0xff << 23;
const F32_2_127: u64 = (127 + 127) << 23;
const F32_NEG_2_127: u64 = (1 << 31) | F32_2_127;
const F32_NEG_1: u64 = (1 << 31) | (127 << 23);

/// The most saturating cases one conversion has (`NaN`, above, below).
const MOST_CASES: usize = 3;

impl<A: Allocator> Pass<'_, '_, A> {
    /// `frem`: `fmod(a, b)` or `fmodf(a, b)`.
    pub(super) fn frem(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let mut o = self.view.operands(inst);
        let (a, b) = (o.value(), o.value());
        let double = Self::float_ty(self.view.ty(a)).ok_or(unsupported(inst, op, "a `frem` of other than `f32` or `f64`"))?;
        self.float_out(V0, a, double);
        self.float_out(V1, b, double);
        self.call_routine(if double { Routine::Fmod } else { Routine::Fmodf });
        self.float_in(V0, inst, double);
        Ok(())
    }

    /// `udiv`, `urem`, `sdiv`, `srem` at `i128`: the checks, then the runtime's routine with
    /// the dividend in `x0:x1` and the divisor in `x2:x3`; the result comes back in `x0:x1`.
    pub(super) fn wide_div(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let mut o = self.view.operands(inst);
        let (a, b) = (o.value(), o.value());
        self.ld2(X0, X1, a);
        self.ld2(X2, X3, b);
        // Zero is both words zero.
        self.put(enc::orr(X9, X2, X3));
        self.trap_if_zero(X9);
        if op == Op::Sdiv {
            // `-1` is both words all ones; `MIN` is a low word of 0 and a high word of 1 << 63.
            self.put(enc::and(X9, X2, X3));
            self.put(enc::cmn_imm(X9, 1));
            let not_minus_one = self.here();
            self.put(enc::b_cond(Cond::Ne, 0));
            self.mov_imm(X9, 1 << 63);
            self.put(enc::eor(X9, X1, X9));
            self.put(enc::orr(X9, X9, X0));
            self.put(enc::cbnz(X9, 2));
            self.put(enc::brk(BRK_DIV_OVERFLOW));
            self.land(not_minus_one)?;
        }
        self.call_routine(match op {
            Op::Udiv => Routine::Udivti3,
            Op::Urem => Routine::Umodti3,
            Op::Sdiv => Routine::Divti3,
            Op::Srem => Routine::Modti3,
            _ => return Err(unsupported(inst, op, "a 128-bit division routine for other than `udiv`, `urem`, `sdiv`, `srem`")),
        });
        self.st2(X0, X1, inst);
        Ok(())
    }

    /// `sitofp`/`uitofp` from `i128` (the integer in `x0:x1`, the float back in `d0`/`s0`).
    pub(super) fn wide_to_float(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let a = self.view.operands(inst).value();
        let double = Self::float_ty(self.view.ty(Value(inst as u32)))
            .ok_or(unsupported(inst, op, "an `i128` to other than `f32` or `f64`"))?;
        self.ld2(X0, X1, a);
        self.call_routine(match (op == Op::SitoFp, double) {
            (true, true) => Routine::Floattidf,
            (false, true) => Routine::Floatuntidf,
            (true, false) => Routine::Floattisf,
            (false, false) => Routine::Floatuntisf,
        });
        self.float_in(V0, inst, double);
        Ok(())
    }

    /// `fptosi`/`fptoui` to `i128`: `NaN` and every value out of range decided here (the
    /// module's doc), the rest by the runtime's routine (the float in `d0`/`s0`, the result
    /// back in `x0:x1`).
    pub(super) fn float_to_wide(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let a = self.view.operands(inst).value();
        let double = Self::float_ty(self.view.ty(a)).ok_or(unsupported(inst, op, "a float to `i128` from other than `f32` or `f64`"))?;
        let bits = |d: u64, s: u64| if double { d } else { s };
        self.float_out(V0, a, double);
        let mut done = [0u32; MOST_CASES];
        let mut n = 0;
        if op == Op::FptoSi {
            // NaN: ordered with itself unless NaN.
            self.put(enc::fcmp(double, V0, V0));
            done[n] = self.wide_unless(Cond::Vc, 0, 0)?;
            n += 1;
            // x >= 2^127: MAX. Ordered from here on, so `Lt` is "below".
            self.fcmp_const(double, bits(F64_2_127, F32_2_127));
            done[n] = self.wide_unless(Cond::Lt, u64::MAX, u64::MAX >> 1)?;
            n += 1;
            // x < -2^127: MIN (-2^127 itself is MIN, in range).
            self.fcmp_const(double, bits(F64_NEG_2_127, F32_NEG_2_127));
            done[n] = self.wide_unless(Cond::Ge, 0, 1 << 63)?;
            n += 1;
        } else {
            // NaN or x <= -1.0: 0 (`Gt` is false when unordered); (-1, 0) truncates to 0 in
            // the routine, in range.
            self.fcmp_const(double, bits(F64_NEG_1, F32_NEG_1));
            done[n] = self.wide_unless(Cond::Gt, 0, 0)?;
            n += 1;
            // x >= 2^128 (for `f32`, +inf): MAX. Ordered here, so `Mi` is "below".
            self.fcmp_const(double, bits(F64_2_128, F32_INF));
            done[n] = self.wide_unless(Cond::Mi, u64::MAX, u64::MAX)?;
            n += 1;
        }
        self.call_routine(match (op == Op::FptoSi, double) {
            (true, true) => Routine::Fixdfti,
            (false, true) => Routine::Fixunsdfti,
            (true, false) => Routine::Fixsfti,
            (false, false) => Routine::Fixunssfti,
        });
        for &at in &done[..n] {
            self.land_b(at)?;
        }
        self.st2(X0, X1, inst);
        Ok(())
    }

    /// `fcmp v0, <the float of bits>` (`d1`/`s1` holds it).
    fn fcmp_const(&mut self, double: bool, bits: u64) {
        self.mov_imm(X9, bits);
        self.put(if double { enc::fmov_to_d(V1, X9) } else { enc::fmov_to_s(V1, X9) });
        self.put(enc::fcmp(double, V0, V1));
    }

    /// Unless `skip` holds: `x0:x1 = lo:hi` and a jump to the end, whose offset it returns
    /// for [`Self::land_b`].
    fn wide_unless(&mut self, skip: Cond, lo: u64, hi: u64) -> Result<u32, EmitError> {
        let over = self.here();
        self.put(enc::b_cond(skip, 0));
        self.mov_imm(X0, lo);
        self.mov_imm(X1, hi);
        let jump = self.here();
        self.put(enc::b(0));
        self.land(over)?;
        Ok(jump)
    }

    /// Points the forward `b` at `at` (an `Imm26` field) here.
    fn land_b(&mut self, at: u32) -> Result<(), EmitError> {
        let word = Field::Imm26
            .patch(self.word_at(at), i64::from(self.here()) - i64::from(at))
            .ok_or(EmitError::TooLarge { what: "a branch inside one instruction's code (128 MiB)" })?;
        self.set(at, word);
        Ok(())
    }
}

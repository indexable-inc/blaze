//! Integer arithmetic by the Reference: the checks Rust makes on `/`, `%`, `+`, `-`, `*`, `<<`,
//! `>>` and unary `-`, each a call of the pinned library's own panic entry point when it fails.
//!
//! The Reference, "Arithmetic and Logical Binary Operators" ([expr.arith-logic.behavior]):
//! "† For integer types, division by zero panics." and "Integer operators will panic when they
//! overflow when compiled in debug mode."; "Overflow" ([expr.operator.int-overflow]): "Integer
//! operators will panic when they overflow when compiled in debug mode. The -C debug-assertions
//! and -C overflow-checks compiler flags can be used to control this more directly. The
//! following things are considered to be overflow: When +, * or binary - create a value
//! greater than the maximum value, or less than the minimum value that can be stored. ..
//! Applying unary - to the most negative value of any signed integer type, unless the operand
//! is a literal expression (or a literal expression standing alone inside one or more grouped
//! expressions). Using / or %, where the left-hand argument is the smallest integer of a
//! signed integer type and the right-hand argument is -1. These checks occur even when -C
//! overflow-checks is disabled, for legacy reasons. Using << or >> where the right operand is
//! greater than or equal to the number of bits in the type of the left operand, or is
//! negative."
//!
//! So, in every build: `x / 0` and `x % 0` call `panic_const_div_by_zero` /
//! `panic_const_rem_by_zero`; `MIN / -1` and `MIN % -1` call `panic_const_div_overflow` /
//! `panic_const_rem_overflow`. With overflow checks on ([`crate::Adts::overflow_checks`]):
//! `+ - *` call `panic_const_{add,sub,mul}_overflow` when the exact result does not fit, a
//! shift whose amount, read as unsigned, is at least the left side's width calls
//! `panic_const_{shl,shr}_overflow`, and `-MIN` calls `panic_const_neg_overflow`; with them
//! off those wrap (a shift's amount masked to the width).
//!
//! The entry points are the pinned core's (library/core/src/panicking.rs, `pub mod
//! panic_const`: `#[lang = stringify!($lang)] pub const fn $lang() -> !`, one per message:
//! "attempt to add with overflow", .., "attempt to divide by zero", "attempt to calculate the
//! remainder with a divisor of zero"): ordinary lang fns of no parameter, reached like the
//! bounds check's, and called through [`Walk::call_unwinding`], the one path a call that can
//! unwind takes. Never a trap instruction: a trap neither unwinds nor says which check failed.
//!
//! The pinned entry points are `#[track_caller]`: each call passes the location of the
//! expression whose check failed, or the enclosing tracked fn's own (walk/track_caller.rs).
//!
//! The literal exception for unary `-` (`-128i8` is no overflow): the caller (walk.rs
//! `Task::Unary`) checks no negation whose operand is a literal expression, under any
//! parentheses, so `-128i8` is the wrapped `-(128 as i8)`, which is `MIN`, as Rust has it.

use core::alloc::Allocator;

use blaze_code::record::TrapKind;
use blaze_lir::func::RefKind;
use blaze_lir::{Flags, IntCond, Op, Ty, Value};
use blaze_typeck::{BinOp, ExprId, List};

use super::{Callee, PTR_BITS, Walk};
use crate::{BuildError, Instance};

/// A failed arithmetic check: which of the pinned core's `panic_const_*` lang fns it calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanicConst {
    AddOverflow,
    SubOverflow,
    MulOverflow,
    DivOverflow,
    RemOverflow,
    NegOverflow,
    ShrOverflow,
    ShlOverflow,
    DivByZero,
    RemByZero,
}

impl PanicConst {
    /// Every check's entry point: a front end tables each one the program declares, as it
    /// tables `panic_bounds_check`.
    pub const ALL: [PanicConst; 10] = [
        PanicConst::AddOverflow,
        PanicConst::SubOverflow,
        PanicConst::MulOverflow,
        PanicConst::DivOverflow,
        PanicConst::RemOverflow,
        PanicConst::NegOverflow,
        PanicConst::ShrOverflow,
        PanicConst::ShlOverflow,
        PanicConst::DivByZero,
        PanicConst::RemByZero,
    ];

    /// Its `#[lang = ".."]` name (the pinned core's `panic_const!` list).
    pub const fn lang(self) -> &'static str {
        self.declared_as()[0]
    }

    /// The names a library declares it under: the pinned core's one name.
    pub const fn declared_as(self) -> &'static [&'static str] {
        match self {
            PanicConst::AddOverflow => &["panic_const_add_overflow"],
            PanicConst::SubOverflow => &["panic_const_sub_overflow"],
            PanicConst::MulOverflow => &["panic_const_mul_overflow"],
            PanicConst::DivOverflow => &["panic_const_div_overflow"],
            PanicConst::RemOverflow => &["panic_const_rem_overflow"],
            PanicConst::NegOverflow => &["panic_const_neg_overflow"],
            PanicConst::ShrOverflow => &["panic_const_shr_overflow"],
            PanicConst::ShlOverflow => &["panic_const_shl_overflow"],
            PanicConst::DivByZero => &["panic_const_div_by_zero"],
            PanicConst::RemByZero => &["panic_const_rem_by_zero"],
        }
    }
}

/// All ones in the low `bits` bits.
fn ones(bits: u32) -> u128 {
    if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 }
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// When `failed` (an `i1`) holds, calls `which`'s entry point (it never returns); the walk
    /// goes on in the block where it does not. An entry point the program does not declare is
    /// refused by name ([`BuildError::NoLangFn`]), as Rust refuses a program missing it.
    fn panic_if(&mut self, e: ExprId, failed: Value, which: PanicConst) -> Result<(), BuildError> {
        let name = which.lang();
        let item = self.adts.lang_fn(name).ok_or(BuildError::NoLangFn { expr: Some(e), name })?;
        let ok = self.create();
        let fail = self.create();
        self.br(failed, (fail, &[]), (ok, &[]));
        self.begin(fail);
        // The pinned entry points are `#[track_caller]`: the location is passed last
        // (walk/track_caller.rs), the place of the expression whose check failed.
        let callee = Instance { item, args: List::EMPTY };
        let (sig, values) = self.lang_call_sig(Some(e), callee, &[], &[])?;
        let place = self.ref_place(callee);
        let reference = self.b.reference(RefKind::Func, place);
        self.call_unwinding(Callee::Direct(reference), sig, &values)?;
        self.unreachable_of(TrapKind::AfterPanic, Some(e));
        self.begin(ok);
        Ok(())
    }

    /// `l / r` or `l % r` on integers of `ty`: the divisor checked against zero, and a signed
    /// `MIN / -1` / `MIN % -1` checked, in every build.
    pub(super) fn checked_div(&mut self, e: ExprId, op: BinOp, l: Value, r: Value, ty: Ty, signed: bool) -> Result<Value, BuildError> {
        let div = op == BinOp::Div;
        let zero = self.b.iconst(ty, 0);
        let is_zero = self.b.icmp(IntCond::Eq, r, zero);
        self.panic_if(e, is_zero, if div { PanicConst::DivByZero } else { PanicConst::RemByZero })?;
        #[cfg(test)]
        let planted = !div && self.adts.planted() == crate::Planted::NoRemOverflowCheck;
        #[cfg(not(test))]
        let planted = false;
        if signed && !planted {
            let bits = ty.bits(PTR_BITS);
            let min = self.b.iconst(ty, 1u128 << (bits - 1));
            let minus_one = self.b.iconst(ty, ones(bits));
            let at_min = self.b.icmp(IntCond::Eq, l, min);
            let by_minus_one = self.b.icmp(IntCond::Eq, r, minus_one);
            let both = self.b.binary(Op::And, Flags::NONE, at_min, by_minus_one);
            self.panic_if(e, both, if div { PanicConst::DivOverflow } else { PanicConst::RemOverflow })?;
        }
        let o = match (div, signed) {
            (true, true) => Op::Sdiv,
            (true, false) => Op::Udiv,
            (false, true) => Op::Srem,
            (false, false) => Op::Urem,
        };
        Ok(self.b.binary(o, Flags::NONE, l, r))
    }

    /// `l + r`, `l - r` or `l * r` with overflow checks on: the exact result must fit.
    pub(super) fn checked_add_sub_mul(&mut self, e: ExprId, op: BinOp, l: Value, r: Value, signed: bool) -> Result<Value, BuildError> {
        let (o, which) = match (op, signed) {
            (BinOp::Add, false) => (Op::UaddOverflow, PanicConst::AddOverflow),
            (BinOp::Add, true) => (Op::SaddOverflow, PanicConst::AddOverflow),
            (BinOp::Sub, false) => (Op::UsubOverflow, PanicConst::SubOverflow),
            (BinOp::Sub, true) => (Op::SsubOverflow, PanicConst::SubOverflow),
            (BinOp::Mul, false) => (Op::UmulOverflow, PanicConst::MulOverflow),
            (BinOp::Mul, true) => (Op::SmulOverflow, PanicConst::MulOverflow),
            _ => return Err(super::malformed("a checked arithmetic operation that is not `+`, `-` or `*`")),
        };
        let pair = self.b.overflow(o, l, r);
        let v = self.b.project(pair, 0);
        let over = self.b.project(pair, 1);
        self.panic_if(e, over, which)?;
        Ok(v)
    }

    /// With overflow checks on, a shift's amount `r` (of type `rty`), read as unsigned, must be
    /// below the width of the left side's type `ty`.
    pub(super) fn check_shift(&mut self, e: ExprId, op: BinOp, r: Value, rty: Ty, ty: Ty) -> Result<(), BuildError> {
        let width = self.b.iconst(rty, u128::from(ty.bits(PTR_BITS)));
        let past = self.b.icmp(IntCond::Uge, r, width);
        self.panic_if(e, past, if op == BinOp::Shl { PanicConst::ShlOverflow } else { PanicConst::ShrOverflow })
    }

    /// `-x` on a signed integer of `ty` with overflow checks on, `x` no literal: `x` must not
    /// be `MIN`.
    pub(super) fn checked_neg(&mut self, e: ExprId, x: Value, ty: Ty) -> Result<Value, BuildError> {
        let min = self.b.iconst(ty, 1u128 << (ty.bits(PTR_BITS) - 1));
        let at_min = self.b.icmp(IntCond::Eq, x, min);
        self.panic_if(e, at_min, PanicConst::NegOverflow)?;
        Ok(self.b.unary(Op::Neg, Flags::NONE, x))
    }
}

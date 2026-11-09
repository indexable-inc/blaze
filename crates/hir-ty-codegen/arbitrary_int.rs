//! Integers of any width (`#![blaze::arbitrary_ints]`, docs/v2/plan/design/arbitrary-ints.md):
//! `u1`..`u127` and `i1`..`i127` but the standard widths, each held in the standard integer
//! of its storage (`u3` in a `u8`, `i24` in an `i32`, `u65` in a `u128`), zero-extended from
//! its width when unsigned and sign-extended when signed.
//!
//! Comparisons, `& | ^`, `>>`, `!` of a signed value, widening casts and moves keep that form.
//! Every other operation that makes a value of one ends in [`FunctionCx::normalize_int`], so a
//! value compiled here is the one const eval makes (the plan's table of operators, overflow and
//! casts is the contract both implement). The work is ordinary Cranelift operations of the
//! storage type, so the stencils and LLVM, which both lower this IR, need nothing of their own:
//! an operation one has no code for is its own typed error naming it. A standard integer is
//! never a [`NarrowInt`], and the IR of every program without one is what it was.

#![forbid(unsafe_code)]

use super::*;

/// An integer type only `#![blaze::arbitrary_ints]` has: its values have `bits` bits and are
/// held in an integer of `storage` bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NarrowInt {
    pub(crate) bits: u32,
    pub(crate) storage: u32,
    pub(crate) signed: bool,
    /// What the type is called, `u3`, for the panics that name it.
    pub(crate) name: &'static str,
}

impl NarrowInt {
    /// `ty` as one; `None` for every other type, the standard integers among them.
    pub(crate) fn of(ty: Ty<'_>) -> Option<NarrowInt> {
        let (width, signed, name) = match ty.kind() {
            TyKind::Int(int) => (int.arbitrary_width()?, true, int.name_str()),
            TyKind::Uint(uint) => (uint.arbitrary_width()?, false, uint.name_str()),
            _ => return None,
        };
        Some(NarrowInt { bits: width.bits(), storage: width.storage_bits(), signed, name })
    }

    /// How many bits of the storage are above the value's.
    fn unused(self) -> u32 {
        self.storage - self.bits
    }

    /// `raw`, bits of the storage, in the form a value of this type is held, with nothing
    /// above the storage: what [`FunctionCx::normalize_int`] makes of them.
    pub(crate) fn normalize_bits(self, raw: u128) -> u128 {
        if self.signed {
            let unused = 128 - self.bits;
            let extended = ((raw << unused) as i128 >> unused) as u128;
            extended & (u128::MAX >> (128 - self.storage))
        } else {
            raw & self.mask()
        }
    }

    /// The value's own bits: `2^bits - 1`.
    fn mask(self) -> u128 {
        u128::MAX >> (128 - self.bits)
    }

    /// The least value, as the bits of the storage.
    fn min(self) -> u128 {
        match self.signed {
            true => self.normalize_bits(1 << (self.bits - 1)),
            false => 0,
        }
    }

    /// The greatest value.
    fn max(self) -> u128 {
        match self.signed {
            true => (1 << (self.bits - 1)) - 1,
            false => self.mask(),
        }
    }

    /// Whether every value of an integer of `bits` bits, signed or not, is one of this type,
    /// so that converting from it to this type's storage leaves nothing to normalize.
    fn holds(self, bits: u32, signed: bool) -> bool {
        match (signed, self.signed) {
            (false, false) | (true, true) => bits <= self.bits,
            (false, true) => bits < self.bits,
            (true, false) => false,
        }
    }
}

impl OverflowOp {
    /// What core's `panic_const_*` of this check says, which an arbitrary width's panic says
    /// followed by its type, as const eval's does (`hir-ty-mir`'s `eval/narrow.rs`).
    fn message(self) -> &'static str {
        match self {
            OverflowOp::Add => "attempt to add with overflow",
            OverflowOp::Sub => "attempt to subtract with overflow",
            OverflowOp::Mul => "attempt to multiply with overflow",
            OverflowOp::Div => "attempt to divide with overflow",
            OverflowOp::Rem => "attempt to calculate the remainder with overflow",
            OverflowOp::Neg => "attempt to negate with overflow",
            OverflowOp::Shl => "attempt to shift left with overflow",
            OverflowOp::Shr => "attempt to shift right with overflow",
            OverflowOp::DivByZero => "attempt to divide by zero",
            OverflowOp::RemByZero => "attempt to calculate the remainder with a divisor of zero",
        }
    }
}

impl<'db> FunctionCx<'_, 'db> {
    /// `value`, of `int`'s storage, in the form a value of `int` is held: `value & (2^N - 1)`
    /// unsigned, `(value << (S - N)) >> (S - N)` arithmetic signed.
    pub(super) fn normalize_int(&mut self, value: Value, int: NarrowInt) -> Value {
        if !int.signed {
            let ty = self.builder.func.dfg.value_type(value);
            let mask = self.iconst(ty, int.mask());
            return self.builder.ins().band(value, mask);
        }
        let up = self.shift_unused(value, int, Shift::Left);
        self.shift_unused(up, int, Shift::Arithmetic)
    }

    /// [`Self::normalize_int`] of `value` when the type it is of is a [`NarrowInt`].
    pub(super) fn normalize_if(&mut self, value: Value, int: Option<NarrowInt>) -> Value {
        match int {
            Some(int) => self.normalize_int(value, int),
            None => value,
        }
    }

    /// `value` shifted by the bits of `int`'s storage above its own.
    fn shift_unused(&mut self, value: Value, int: NarrowInt, shift: Shift) -> Value {
        let unused = i64::from(int.unused());
        // An `i128` is shifted by an `i32`, as Rust's shifts of one are: the `_imm` forms
        // would make the amount an `i128` as well.
        if self.builder.func.dfg.value_type(value) == types::I128 {
            let by = self.builder.ins().iconst(types::I32, unused);
            let ins = self.builder.ins();
            return match shift {
                Shift::Left => ins.ishl(value, by),
                Shift::Logical => ins.ushr(value, by),
                Shift::Arithmetic => ins.sshr(value, by),
            };
        }
        let ins = self.builder.ins();
        match shift {
            Shift::Left => ins.ishl_imm_u(value, unused),
            Shift::Logical => ins.ushr_imm_u(value, unused),
            Shift::Arithmetic => ins.sshr_imm_u(value, unused),
        }
    }

    /// `value`, an `int`, as its `N` bits with nothing above them: as it is for unsigned,
    /// without the sign's extension for signed.
    fn zero_extended(&mut self, value: Value, int: NarrowInt) -> Value {
        if !int.signed {
            return value;
        }
        let ty = self.builder.func.dfg.value_type(value);
        let mask = self.iconst(ty, int.mask());
        self.builder.ins().band(value, mask)
    }

    /// `!value` of an `int`: an unsigned one's `N` bits flipped, a signed one's all of them,
    /// which keeps the sign's extension.
    pub(super) fn narrow_not(&mut self, value: Value, int: NarrowInt) -> Value {
        if int.signed {
            return self.builder.ins().bnot(value);
        }
        let ty = self.builder.func.dfg.value_type(value);
        let mask = self.iconst(ty, int.mask());
        self.builder.ins().bxor(value, mask)
    }

    /// `raw`, `+`, `-` or `*` (`mul`) of two `int`s worked out in their storage, normalized;
    /// with `flag`, whether that overflowed the storage, also whether it overflowed `int`
    /// (`raw != normalize(raw)`). Two values of `N` bits add and subtract in `N + 1`, which
    /// the storage holds, so only a product needs the storage's own flag.
    pub(super) fn narrow_overflowing(
        &mut self,
        int: NarrowInt,
        raw: Value,
        flag: Option<Value>,
        mul: bool,
    ) -> (Value, Option<Value>) {
        let value = self.normalize_int(raw, int);
        let overflow = flag.map(|flag| {
            let out = self.builder.ins().icmp(IntCC::NotEqual, raw, value);
            if mul { self.builder.ins().bor(out, flag) } else { out }
        });
        (value, overflow)
    }

    /// `amount`, a value of `amount_ty`, as an unsigned number of its type's width: a signed
    /// arbitrary width's extension of its sign is cut off. Any other type's is as it is, which
    /// a shift reads as unsigned.
    pub(super) fn unsigned_amount(&mut self, amount: Value, amount_ty: Ty<'db>) -> Value {
        match NarrowInt::of(amount_ty) {
            Some(int) if int.signed => self.zero_extended(amount, int),
            _ => amount,
        }
    }

    /// The amount a shift or rotation of an `int` shifts by: `amount`, unsigned
    /// ([`Self::unsigned_amount`]), modulo `N`, which generalizes rustc's `& (BITS - 1)`. A
    /// word when `amount` is an `i128`.
    pub(super) fn narrow_shift_amount(&mut self, amount: Value, int: NarrowInt) -> Value {
        let n = u64::from(int.bits);
        if self.builder.func.dfg.value_type(amount) != types::I128 {
            return match n.is_power_of_two() {
                true => self.builder.ins().band_imm_u(amount, (n - 1) as i64),
                false => self.builder.ins().urem_imm_u(amount, n as i64),
            };
        }
        let (low, high) = self.builder.ins().isplit(amount);
        if n.is_power_of_two() {
            return self.builder.ins().band_imm_u(low, (n - 1) as i64);
        }
        // `high * 2^64 + low` modulo `n`, a word at a time: each remainder is below 128, so
        // the product with `2^64 mod n` and the sum fit the word.
        let word = ((1u128 << 64) % u128::from(n)) as i64;
        let low = self.builder.ins().urem_imm_u(low, n as i64);
        let high = self.builder.ins().urem_imm_u(high, n as i64);
        let high = self.builder.ins().imul_imm_u(high, word);
        let sum = self.builder.ins().iadd(high, low);
        self.builder.ins().urem_imm_u(sum, n as i64)
    }

    /// `value << amount` (`left`) or `value >> amount` of an `int`, the amount of `amount_ty`
    /// taken modulo `N`.
    pub(super) fn narrow_shift(
        &mut self,
        value: Value,
        amount: Value,
        amount_ty: Ty<'db>,
        left: bool,
        int: NarrowInt,
    ) -> Value {
        let amount = self.unsigned_amount(amount, amount_ty);
        let amount = self.narrow_shift_amount(amount, int);
        let ins = self.builder.ins();
        match (left, int.signed) {
            (true, _) => {
                let value = ins.ishl(value, amount);
                self.normalize_int(value, int)
            }
            // Shifting right keeps the form a value is held in.
            (false, true) => ins.sshr(value, amount),
            (false, false) => ins.ushr(value, amount),
        }
    }

    /// The result of a cast to `to_ty` of `value`, a value of `from_ty` that `blaze_int_cast` has
    /// just converted to the storage of `to_ty` (`from` is how it was held): normalized to
    /// the destination's width when that is an arbitrary one whose values the source's are
    /// not all of.
    pub(super) fn narrow_cast(
        &mut self,
        value: Value,
        from_ty: Ty<'db>,
        from: AbiScalar,
        to_ty: Ty<'db>,
    ) -> Value {
        let Some(int) = NarrowInt::of(to_ty) else { return value };
        let source = match (NarrowInt::of(from_ty), from_ty.kind()) {
            (Some(from), _) => Some((from.bits, from.signed)),
            (None, TyKind::Bool) => Some((1, false)),
            (None, TyKind::Int(_) | TyKind::Uint(_)) => Some((from.ty.bits(), from.signed)),
            _ => None,
        };
        match source {
            Some((bits, signed)) if int.holds(bits, signed) => value,
            _ => self.normalize_int(value, int),
        }
    }

    /// `value`, an integer of `int`'s storage that a float was converted to, saturating at
    /// the storage's bounds, saturated at `int`'s; NaN, zero already, stays zero.
    pub(super) fn narrow_saturate(&mut self, value: Value, int: NarrowInt) -> Value {
        let ty = self.builder.func.dfg.value_type(value);
        let max = self.iconst(ty, int.max());
        let (below, above) = match int.signed {
            true => (IntCC::SignedLessThan, IntCC::SignedGreaterThan),
            false => (IntCC::UnsignedLessThan, IntCC::UnsignedGreaterThan),
        };
        let value = if int.signed {
            let min = self.iconst(ty, int.min());
            let low = self.builder.ins().icmp(below, value, min);
            self.builder.ins().select(low, min, value)
        } else {
            value
        };
        let high = self.builder.ins().icmp(above, value, max);
        self.builder.ins().select(high, max, value)
    }

    /// `saturating_add` (`add`) or `saturating_sub` of two `int`s: clamped to `int`'s range.
    fn narrow_saturating(&mut self, add: bool, l: Value, r: Value, int: NarrowInt) -> Value {
        let ty = self.builder.func.dfg.value_type(l);
        let raw = match add {
            true => self.builder.ins().iadd(l, r),
            false => self.builder.ins().isub(l, r),
        };
        let value = self.normalize_int(raw, int);
        let out = self.builder.ins().icmp(IntCC::NotEqual, raw, value);
        let limit = if int.signed {
            // Past MAX when the left operand is non-negative, past MIN otherwise, as for the
            // standard widths.
            let min = self.iconst(ty, int.min());
            let max = self.iconst(ty, int.max());
            let negative = self.builder.ins().icmp_imm_s(IntCC::SignedLessThan, l, 0);
            self.builder.ins().select(negative, min, max)
        } else if add {
            self.iconst(ty, int.max())
        } else {
            self.iconst(ty, 0)
        };
        self.builder.ins().select(out, limit, value)
    }

    /// `rotate_left` (`left`) or `rotate_right` of `value`, an `int`, within its `N` bits, by
    /// `amount` (of `amount_ty`) modulo `N`.
    fn narrow_rotate(
        &mut self,
        value: Value,
        amount: Value,
        amount_ty: Ty<'db>,
        left: bool,
        int: NarrowInt,
    ) -> Value {
        let amount = self.unsigned_amount(amount, amount_ty);
        let by = self.narrow_shift_amount(amount, int);
        let bits = self.builder.func.dfg.value_type(by);
        let bits = self.builder.ins().iconst(bits, i64::from(int.bits));
        // `N - by` is at most `N`, below the storage's width, so neither shift is cut.
        let rest = self.builder.ins().isub(bits, by);
        let (up, down) = if left { (by, rest) } else { (rest, by) };
        let value = self.zero_extended(value, int);
        let high = self.builder.ins().ishl(value, up);
        let low = self.builder.ins().ushr(value, down);
        let rotated = self.builder.ins().bor(high, low);
        self.normalize_int(rotated, int)
    }

    /// The integer intrinsics of two operands of an `int` (`ThreeWayCompare` aside, which
    /// only compares): `wrapping_*` and `unchecked_*` normalized, shifts and rotations modulo
    /// `N`, `saturating_*` clamped to `N` bits.
    pub(super) fn narrow_binary_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        (l, r): (Value, Value),
        r_ty: Ty<'db>,
        int: NarrowInt,
    ) -> Result<Value> {
        use Intrinsic as I;
        let ins = self.builder.ins();
        let raw = match intrinsic {
            I::WrappingAdd | I::UncheckedAdd => ins.iadd(l, r),
            I::WrappingSub | I::UncheckedSub => ins.isub(l, r),
            I::WrappingMul | I::UncheckedMul => ins.imul(l, r),
            // Only `MIN / -1` would leave the width, and it is undefined for these.
            I::UncheckedDiv | I::ExactDiv => return self.divide(l, r, true, int.signed),
            I::UncheckedRem => return self.divide(l, r, false, int.signed),
            I::UncheckedShl | I::UncheckedShr => {
                let left = intrinsic == I::UncheckedShl;
                return Ok(self.narrow_shift(l, r, r_ty, left, int));
            }
            I::RotateLeft | I::RotateRight => {
                let left = intrinsic == I::RotateLeft;
                return Ok(self.narrow_rotate(l, r, r_ty, left, int));
            }
            I::SaturatingAdd | I::SaturatingSub => {
                let add = intrinsic == I::SaturatingAdd;
                return Ok(self.narrow_saturating(add, l, r, int));
            }
            _ => unsupported!("intrinsic `{intrinsic}` of `{}`", int.name),
        };
        Ok(self.normalize_int(raw, int))
    }

    /// `ctpop`, `ctlz`, `cttz`, `bswap` and `bitreverse` of `value`, an `int`, counted or
    /// moved within its `N` bits, as a value of the storage.
    pub(super) fn narrow_bits_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        value: Value,
        int: NarrowInt,
    ) -> Result<Value> {
        use Intrinsic as I;
        let ty = self.builder.func.dfg.value_type(value);
        Ok(match intrinsic {
            I::Ctpop => {
                let value = self.zero_extended(value, int);
                self.builder.ins().popcnt(value)
            }
            // The storage's leading zeros, but those above the value's bits.
            I::Ctlz | I::CtlzNonzero => {
                let value = self.zero_extended(value, int);
                let zeros = self.builder.ins().clz(value);
                self.builder.ins().iadd_imm_s(zeros, -i64::from(int.unused()))
            }
            // Bit `N` set stops the count of zero at `N`; any other value has a bit set below.
            I::Cttz | I::CttzNonzero => {
                let stop = self.iconst(ty, 1 << int.bits);
                let value = self.builder.ins().bor(value, stop);
                self.builder.ins().ctz(value)
            }
            I::Bswap if int.bits % 8 != 0 => unsupported!(
                "intrinsic `bswap` of `{}`, whose {} bits are not whole bytes",
                int.name,
                int.bits
            ),
            // Reversed in the storage, the value's bits are its top ones.
            I::Bswap | I::Bitreverse => {
                let value = match intrinsic {
                    I::Bswap => self.builder.ins().bswap(value),
                    _ => self.builder.ins().bitrev(value),
                };
                let shift = if int.signed { Shift::Arithmetic } else { Shift::Logical };
                self.shift_unused(value, int, shift)
            }
            _ => unsupported!("intrinsic `{intrinsic}` of `{}`", int.name),
        })
    }

    /// Calls core's `panic` with the message of a failed check of `op` at the type `int`
    /// (``attempt to add with overflow at type `u3` ``), the one const eval gives.
    pub(super) fn narrow_check_panic(&mut self, op: OverflowOp, int: NarrowInt) -> Result<()> {
        // alloc: the message's bytes, owned by the blob `define_const` keeps
        let bytes = format!("{} at type `{}`", op.message(), int.name).into_bytes();
        let len = bytes.len() as i64;
        let text = self.blob_addr(ConstBlob {
            bytes,
            relocs: Vec::new(),    // alloc: empty, allocates nothing
            fn_relocs: Vec::new(), // alloc: empty, allocates nothing
            align: 1,
            type_ids: Vec::new(), // alloc: empty, allocates nothing
        })?;
        let len = self.builder.ins().iconst(self.cg.target.pointer(), len);
        let location = self.caller_location()?;
        self.call_panic(&["panicking", "panic"], &[text, len, location])
    }
}

#[derive(Clone, Copy)]
enum Shift {
    Left,
    Logical,
    Arithmetic,
}

#[cfg(test)]
mod tests {
    use super::NarrowInt;

    fn int(bits: u32, signed: bool) -> NarrowInt {
        let storage = bits.next_power_of_two().max(8);
        NarrowInt { bits, storage, signed, name: "" }
    }

    #[test]
    fn normalize_bits_extends_from_the_width_into_the_storage_only() {
        assert_eq!(int(3, false).normalize_bits(0xff), 0b111);
        assert_eq!(int(3, true).normalize_bits(0b100), 0xfc);
        assert_eq!(int(3, true).normalize_bits(0x0b), 0x03);
        assert_eq!(int(24, true).normalize_bits(0x80_0000), 0xff80_0000);
        assert_eq!(int(65, true).normalize_bits(1 << 64), u128::MAX << 64);
        assert_eq!(int(127, false).normalize_bits(u128::MAX), u128::MAX >> 1);
        assert_eq!((int(3, true).min(), int(3, true).max()), (0xfc, 3));
        assert_eq!((int(7, false).min(), int(7, false).max()), (0, 127));
    }
}

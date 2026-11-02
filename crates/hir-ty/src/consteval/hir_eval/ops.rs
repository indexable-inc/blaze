//! The scalar operations of a constant of the closed shape, with the checks MIR building puts
//! in a constant's body (`hir-ty-mir`'s `transform/asserts.rs`) and the panic each one raises.
//!
//! A value is its bits zero-extended to a `u128`: the low `bytes` of it are the value's
//! little-endian bytes, as the MIR evaluator keeps them, and the rest are zero.

use hir_def::hir::{ArithOp, BinaryOp, CmpOp, LogicOp, Ordering, UnaryOp};

/// The type of a value: an integer type of its width (`isize` and `usize` are the integer of
/// the target's pointer width, as their layout is), `bool` or `char`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub(crate) enum Kind {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
    Bool,
    Char,
}

impl Kind {
    #[cfg(test)]
    pub(crate) const ALL: [Kind; 12] = [
        Kind::I8,
        Kind::I16,
        Kind::I32,
        Kind::I64,
        Kind::I128,
        Kind::U8,
        Kind::U16,
        Kind::U32,
        Kind::U64,
        Kind::U128,
        Kind::Bool,
        Kind::Char,
    ];

    pub(crate) fn int(bytes: u64, signed: bool) -> Option<Kind> {
        Some(match (bytes, signed) {
            (1, true) => Kind::I8,
            (2, true) => Kind::I16,
            (4, true) => Kind::I32,
            (8, true) => Kind::I64,
            (16, true) => Kind::I128,
            (1, false) => Kind::U8,
            (2, false) => Kind::U16,
            (4, false) => Kind::U32,
            (8, false) => Kind::U64,
            (16, false) => Kind::U128,
            _ => return None,
        })
    }

    pub(crate) fn bytes(self) -> usize {
        match self {
            Kind::I8 | Kind::U8 | Kind::Bool => 1,
            Kind::I16 | Kind::U16 => 2,
            Kind::I32 | Kind::U32 | Kind::Char => 4,
            Kind::I64 | Kind::U64 => 8,
            Kind::I128 | Kind::U128 => 16,
        }
    }

    pub(crate) fn bits(self) -> u32 {
        self.bytes() as u32 * 8
    }

    pub(crate) fn is_signed(self) -> bool {
        matches!(self, Kind::I8 | Kind::I16 | Kind::I32 | Kind::I64 | Kind::I128)
    }

    pub(crate) fn is_int(self) -> bool {
        !matches!(self, Kind::Bool | Kind::Char)
    }

    /// `v` cut to this kind's width.
    pub(crate) fn wrap(self, v: u128) -> u128 {
        v & (u128::MAX >> (128 - self.bits()))
    }

    /// `v`, a value of this kind, as a signed number if the kind is signed: sign-extended.
    fn widen(self, v: u128) -> i128 {
        if self.is_signed() {
            let unused = 128 - self.bits();
            ((v << unused) as i128) >> unused
        } else {
            v as i128
        }
    }

    fn min_signed(self) -> u128 {
        1 << (self.bits() - 1)
    }

    fn fits(self, v: i128) -> bool {
        self.widen(self.wrap(v as u128)) == v
    }
}

/// A check that failed: the panic of the assert MIR building puts in front of the operation,
/// whose message is the [`Panic::message`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Panic {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    Neg,
}

impl Panic {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Panic::Add => "Overflow in Add",
            Panic::Sub => "Overflow in Sub",
            Panic::Mul => "Overflow in Mul",
            Panic::Div => "Overflow in Div",
            Panic::Rem => "Overflow in Rem",
            Panic::Shl => "Overflow in Shl",
            Panic::Shr => "Overflow in Shr",
            Panic::Neg => "Overflow in Neg",
        }
    }
}

/// `-a` or `!a` on a value of kind `k`: `Neg` on a signed integer, `Not` on an integer or
/// `bool`.
pub(crate) fn unary(op: UnaryOp, k: Kind, a: u128) -> Result<u128, Panic> {
    match op {
        UnaryOp::Neg => {
            debug_assert!(k.is_signed());
            if a == k.min_signed() {
                return Err(Panic::Neg);
            }
            Ok(k.wrap(a.wrapping_neg()))
        }
        UnaryOp::Not if k == Kind::Bool => Ok(a ^ 1),
        UnaryOp::Not => Ok(k.wrap(!a)),
        UnaryOp::Deref => unreachable!("a dereference is not of the shape"),
    }
}

/// `a op b`, `a` of kind `lk` and `b` of `rk` (the same kind but for a shift's right side). A
/// comparison gives a `bool`. `&&` and `||` are run by their jumps, not here.
pub(crate) fn binary(op: BinaryOp, lk: Kind, rk: Kind, a: u128, b: u128) -> Result<u128, Panic> {
    let k = lk;
    match op {
        BinaryOp::ArithOp(op) => arith(op, k, rk, a, b),
        BinaryOp::CmpOp(CmpOp::Eq { negated }) => Ok(u128::from((a == b) != negated)),
        BinaryOp::CmpOp(CmpOp::Ord { ordering, strict }) => {
            let order = if k.is_signed() { k.widen(a).cmp(&k.widen(b)) } else { a.cmp(&b) };
            let holds = match (ordering, strict) {
                (Ordering::Less, true) => order.is_lt(),
                (Ordering::Less, false) => order.is_le(),
                (Ordering::Greater, true) => order.is_gt(),
                (Ordering::Greater, false) => order.is_ge(),
            };
            Ok(u128::from(holds))
        }
        BinaryOp::LogicOp(LogicOp::And | LogicOp::Or) => {
            unreachable!("`&&` and `||` are jumps of the program")
        }
        BinaryOp::Assignment { .. } => unreachable!("an assignment is not of the shape"),
    }
}

fn arith(op: ArithOp, k: Kind, rk: Kind, a: u128, b: u128) -> Result<u128, Panic> {
    let (sa, sb) = (k.widen(a), k.widen(b));
    match op {
        ArithOp::Add | ArithOp::Sub | ArithOp::Mul => {
            let (panic, signed, unsigned): (_, fn(i128, i128) -> _, fn(u128, u128) -> _) =
                match op {
                    ArithOp::Add => (Panic::Add, i128::overflowing_add, u128::overflowing_add),
                    ArithOp::Sub => (Panic::Sub, i128::overflowing_sub, u128::overflowing_sub),
                    _ => (Panic::Mul, i128::overflowing_mul, u128::overflowing_mul),
                };
            if k.is_signed() {
                let (r, overflowed) = signed(sa, sb);
                if overflowed || !k.fits(r) {
                    return Err(panic);
                }
                Ok(k.wrap(r as u128))
            } else {
                let (r, overflowed) = unsigned(a, b);
                if overflowed || k.wrap(r) != r {
                    return Err(panic);
                }
                Ok(r)
            }
        }
        ArithOp::Div | ArithOp::Rem => {
            let panic = if op == ArithOp::Div { Panic::Div } else { Panic::Rem };
            if b == 0 {
                return Err(panic);
            }
            if k.is_signed() {
                if a == k.min_signed() && b == k.wrap(u128::MAX) {
                    return Err(panic);
                }
                // `i128::MIN / -1` is the one quotient that overflows `i128`, refused above.
                let r = if op == ArithOp::Div { sa / sb } else { sa % sb };
                Ok(k.wrap(r as u128))
            } else {
                Ok(if op == ArithOp::Div { a / b } else { a % b })
            }
        }
        ArithOp::Shl | ArithOp::Shr => {
            // The amount is the right side read as the unsigned integer of its own width, so a
            // negative one is a large one.
            let amount = rk.wrap(b);
            if amount >= u128::from(k.bits()) {
                return Err(if op == ArithOp::Shl { Panic::Shl } else { Panic::Shr });
            }
            let amount = amount as u32;
            Ok(match op {
                ArithOp::Shl => k.wrap(a << amount),
                _ if k.is_signed() => k.wrap((sa >> amount) as u128),
                _ => a >> amount,
            })
        }
        ArithOp::BitAnd => Ok(a & b),
        ArithOp::BitOr => Ok(a | b),
        ArithOp::BitXor => Ok(a ^ b),
    }
}

/// `a as to`, `a` of kind `from`: the bytes of `a` extended by its sign (by zeros for an
/// unsigned integer, `bool` and `char`) and cut to `to`'s width, as MIR's `IntToInt` cast is.
pub(crate) fn cast(from: Kind, to: Kind, a: u128) -> u128 {
    to.wrap(from.widen(a) as u128)
}

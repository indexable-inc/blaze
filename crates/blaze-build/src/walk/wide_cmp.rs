//! `==` and `!=` on wide raw pointers (`*const [T]`, `*const str`, `*const dyn Tr`): the one
//! builtin binary operator whose operands the walk holds as aggregates (`Repr::Agg`), walk.rs
//! `binary` sends them here.
//!
//! The Reference ("Comparison operators") names the operators and their traits and says nothing
//! of wide pointers; the pinned library says what the builtin comparison is. Its own impls
//! compare with the builtin operator (core/src/ptr/const_ptr.rs, `impl<T: PointeeSized>
//! PartialEq for *const T`: `#[allow(ambiguous_wide_pointer_comparisons)] fn eq(&self, other:
//! &*const T) -> bool { *self == *other }`), and core/src/ptr/mod.rs, `ptr::eq` ("This is the
//! same as using the `==` operator"), states it: "When comparing wide pointers, both the address
//! and the metadata are tested for equality." So `==` is "both words equal", `!=` its negation,
//! each word read at the offset the wide pointer's layout row gives it (walk.rs, "a wide
//! pointer: its address and its length"), never at a literal.
//!
//! The orderings (`<`, `<=`, `>`, `>=`) are REFUSED BY NAME: the library documents only "For
//! thin pointers, comparison is by address" (`impl<T: PointeeSized> PartialOrd for *const T`),
//! and nothing states how the metadata takes part in the order of wide ones; an order guessed
//! here would be a miscompile of a program that relies on another.

use core::alloc::Allocator;

use blaze_lir::{Flags, IntCond, Op, Ty, Value};
use blaze_typeck::{BinOp, ExprId, PNode};

use super::{Repr, Shape, V, Walk, malformed, unsupported};
use crate::BuildError;

/// The refusal of any aggregate operand that is not a wide raw pointer.
const NOT_WIDE_PTR: &str = "a binary operator on an aggregate that is not a wide raw pointer";

/// The refusal of `<`, `<=`, `>` and `>=` on wide raw pointers (the module doc).
const WIDE_ORDER: &str = "an ordering of wide raw pointers (the pinned library documents the order of thin pointers \
                          only: by address; how the metadata takes part is not stated)";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Binary expression `e` (`lhs op rhs`) whose operands `l` and `r` are aggregates of
    /// `lrepr` and `rrepr`: `==` or `!=` of two wide raw pointers, word by word; every other
    /// form refused by name. Returns the `bool` (an `i8` of 0 or 1).
    pub(super) fn wide_ptr_binary(&mut self, e: ExprId, op: BinOp, lhs: ExprId, (l, lrepr): (V, Repr), (r, rrepr): (V, Repr)) -> Result<V, BuildError> {
        let t = self.adjusted_ty(lhs)?;
        if !matches!(self.node_at(lhs, t)?, PNode::Ptr(..)) {
            return Err(unsupported(e, NOT_WIDE_PTR));
        }
        // `!=` is true when either word differs; `==` when neither does.
        let (cond, join) = match op {
            BinOp::Eq => (IntCond::Eq, Op::And),
            BinOp::Ne => (IntCond::Ne, Op::Or),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => return Err(unsupported(e, WIDE_ORDER)),
            _ => return Err(malformed("an operator other than a comparison on raw pointers")),
        };
        if lrepr != rrepr {
            return Err(malformed("a comparison of two types"));
        }
        let Repr::Agg(row) = lrepr else { return Err(malformed("a wide pointer not held as an aggregate")) };
        let Shape::Fields { first, len: 2 } = self.layouts[row as usize].shape else {
            return Err(malformed("a wide pointer whose layout is not its address and its metadata"));
        };
        let la = self.place_any(l)?;
        let ra = self.place_any(r)?;
        let mut acc: Option<Value> = None;
        for i in 0..2u32 {
            let (_, off, repr) = self.layout_fields[(first + i) as usize];
            let ty = repr.scalar().ok_or(malformed("a wide pointer's word that is not a scalar"))?;
            let off = i32::try_from(off).map_err(|_| malformed("a wide pointer's word past `i32` bytes"))?;
            let a = self.load(repr, (la.0, la.1 + off));
            let b = self.load(repr, (ra.0, ra.1 + off));
            let a = self.scalar(a, ty)?;
            let b = self.scalar(b, ty)?;
            let bit = self.b.icmp(cond, a, b);
            let byte = self.b.convert(Op::Zext, Flags::NONE, Ty::I8, bit);
            acc = Some(match acc {
                None => byte,
                Some(x) => self.b.binary(join, Flags::NONE, x, byte),
            });
        }
        Ok(V::Scalar(acc.ok_or(malformed("a wide pointer of no words"))?))
    }
}

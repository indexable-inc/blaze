//! Fixed vectors in the reference evaluator, up to 128 bits (every AArch64 Advanced SIMD
//! register's width, and SSE's): a vector is held as its bits in one `Val::Int`, lane `i` at
//! bits `i * w .. (i + 1) * w` for lanes of `w` bits, lane 0 lowest (as `bitmask` and a
//! little-endian store order them). An operation is applied lane by lane with the scalar
//! rules: integers by `arith::binary`, floats by soft IEEE ([`super::float::float_bin`]).
//!
//! Modeled: the lane-wise integer operations that cannot trap (`add` .. `smax`, the
//! saturating ones, shifts and rotates masking their amount per lane), the lane-wise
//! two-operand float operations, `splat`, `extract`, `insert`, `shuffle`, `swizzle`, the
//! integer reductions, `reduce_fadd`/`reduce_fmul` in lane order, and `bitmask`. Refused by
//! name: a vector wider than 128 bits, a scalable vector, a vector of pointers, a lane-wise
//! division or remainder (whose zero lane LIR does not define for a vector), and
//! `reduce_fmin`/`reduce_fmax` (whose NaN and signed-zero result op.rs does not state).
//! Every other vector instruction goes on to the scalar paths, which refuse a vector.

use core::alloc::Allocator;

use super::{
    EvalError, Run, Val,
    float::{float_bin, placed},
};
use crate::{
    Op, Scalar, Ty, Value,
    arith::{binary, mask},
    ty::Form,
};

/// The most bits a vector the evaluator holds has: one `u128`.
const MOST_BITS: u32 = 128;

/// Lane `i` of `v`, lanes `w` bits wide.
const fn lane(v: u128, i: u32, w: u32) -> u128 {
    (v >> (i * w)) & mask(w)
}

/// `v` with lane `i` (`w` bits wide) set to `x`.
const fn with_lane(v: u128, i: u32, w: u32, x: u128) -> u128 {
    (v & !(mask(w) << (i * w))) | ((x & mask(w)) << (i * w))
}

/// How instruction `op` treats vectors, if it is one this module claims.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    LaneInt,
    LaneFloat,
    LaneDivision,
    Whole,
    ToScalar,
}

const fn kind(op: Op) -> Option<Kind> {
    Some(match op {
        Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor | Op::Shl | Op::Lshr | Op::Ashr
        | Op::Rotl | Op::Rotr | Op::Umulhi | Op::Smulhi | Op::Umin | Op::Umax | Op::Smin | Op::Smax
        | Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat => Kind::LaneInt,
        Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => Kind::LaneDivision,
        Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Frem | Op::FminNum | Op::FmaxNum
        | Op::Fminimum | Op::Fmaximum | Op::Fcopysign => Kind::LaneFloat,
        Op::Splat | Op::Insert | Op::Shuffle | Op::Swizzle | Op::Bitcast => Kind::Whole,
        Op::Extract | Op::ReduceAdd | Op::ReduceMul | Op::ReduceAnd | Op::ReduceOr | Op::ReduceXor
        | Op::ReduceUmin | Op::ReduceUmax | Op::ReduceSmin | Op::ReduceSmax | Op::ReduceFadd
        | Op::ReduceFmul | Op::ReduceFmin | Op::ReduceFmax | Op::Bitmask => Kind::ToScalar,
        _ => return None,
    })
}

/// The lane-wise operation an integer reduction folds by.
const fn fold_op(op: Op) -> Op {
    match op {
        Op::ReduceAdd => Op::Add,
        Op::ReduceMul => Op::Mul,
        Op::ReduceAnd => Op::And,
        Op::ReduceOr => Op::Or,
        Op::ReduceXor => Op::Xor,
        Op::ReduceUmin => Op::Umin,
        Op::ReduceUmax => Op::Umax,
        Op::ReduceSmin => Op::Smin,
        Op::ReduceSmax => Op::Smax,
        Op::ReduceFmul => Op::Fmul,
        _ => Op::Fadd,
    }
}

impl<A: Allocator + Clone> Run<'_, A> {
    /// Whether vector type `ty` is one the evaluator holds, or what refuses it.
    fn held(&self, inst: usize, op: Op, ty: Ty) -> Result<(), EvalError> {
        let refuse = |what| Err(EvalError::Unsupported { inst, op, what });
        match ty.form() {
            Form::Vector => {}
            Form::Scalable => return refuse("a scalable vector, whose length is the machine's"),
            _ => return refuse("a vector operation on an operand that is not a vector"),
        }
        if ty.elem() == Scalar::Ptr {
            return refuse("a vector of pointers");
        }
        match ty.bits(self.view.head.ptr_bits) <= MOST_BITS {
            true => Ok(()),
            false => refuse("a vector wider than 128 bits: the evaluator holds a vector in one u128"),
        }
    }

    /// Vector operand `value` of instruction `inst`: its bits and its type.
    fn vec(&self, inst: usize, op: Op, value: Value) -> Result<(u128, Ty), EvalError> {
        let ty = self.view.ty(value);
        self.held(inst, op, ty)?;
        match self.get(inst, value)? {
            Val::Int(bits) => Ok((bits & mask(ty.bits(self.view.head.ptr_bits)), ty)),
            _ => Err(EvalError::Unsupported { inst, op, what: "a vector operand that holds no bits" }),
        }
    }

    /// Scalar operand `value` of instruction `inst`, an integer or float lane: its bits.
    fn lane_operand(&self, inst: usize, op: Op, value: Value) -> Result<u128, EvalError> {
        let ty = self.view.ty(value);
        match self.get(inst, value)? {
            Val::Int(bits) if ty.is_scalar() && !ty.is_ptr() => Ok(bits & mask(ty.bits(self.view.head.ptr_bits))),
            _ => Err(EvalError::Unsupported { inst, op, what: "a lane operand that is not a scalar's bits" }),
        }
    }

    /// Vector instruction `inst`: its value, or `None` when it is not one this module claims
    /// (an operation not on its list, or a lane-wise one whose result is not a vector), which
    /// the scalar paths then take.
    pub(super) fn vector(&self, inst: usize, op: Op) -> Result<Option<Val>, EvalError> {
        let Some(kind) = kind(op) else { return Ok(None) };
        let ty = self.view.ty(Value(inst as u32));
        let ptr_bits = self.view.head.ptr_bits;
        if (matches!(kind, Kind::LaneInt | Kind::LaneFloat | Kind::LaneDivision) || op == Op::Bitcast) && !ty.is_vector() {
            return Ok(None);
        }
        let refuse = |what| EvalError::Unsupported { inst, op, what };
        if kind != Kind::ToScalar {
            self.held(inst, op, ty)?;
        }
        let mut o = self.view.operands(inst);
        let bits = match kind {
            Kind::LaneDivision => {
                return Err(refuse("a lane-wise division or remainder, whose zero lane LIR does not define for a vector"));
            }
            Kind::LaneInt | Kind::LaneFloat => {
                let ((a, _), (b, _)) = (self.vec(inst, op, o.value())?, self.vec(inst, op, o.value())?);
                let w = ty.elem().bits(ptr_bits);
                let mut out = 0u128;
                for i in 0..ty.lanes() {
                    let (x, y) = (lane(a, i, w), lane(b, i, w));
                    let r = match kind {
                        // Not a division: `binary` traps for nothing else.
                        Kind::LaneInt => binary(op, x, y, w).map_err(|_| refuse("a lane-wise integer operation that traps"))?,
                        _ => float_bin(op, ty.lane(), x, y).map_err(|error| placed(error, inst))?,
                    };
                    out = with_lane(out, i, w, r);
                }
                out
            }
            Kind::Whole => {
                let w = ty.elem().bits(ptr_bits);
                match op {
                    Op::Splat => {
                        let x = self.lane_operand(inst, op, o.value())?;
                        (0..ty.lanes()).fold(0u128, |v, i| with_lane(v, i, w, x))
                    }
                    Op::Insert => {
                        let (v, _) = self.vec(inst, op, o.value())?;
                        let x = self.lane_operand(inst, op, o.value())?;
                        with_lane(v, o.imm(), w, x)
                    }
                    // "Two vectors and the lanes of their concatenation to take."
                    Op::Shuffle => {
                        let ((a, from), (b, _)) = (self.vec(inst, op, o.value())?, self.vec(inst, op, o.value())?);
                        let n = from.lanes();
                        let mut out = 0u128;
                        for (i, &k) in (0u32..).zip(o.mask()) {
                            let x = match k < n {
                                true => lane(a, k, w),
                                false => lane(b, k - n, w),
                            };
                            out = with_lane(out, i, w, x);
                        }
                        out
                    }
                    // "The lanes of the first vector the second's lanes index (a table lookup); 0
                    // for one out of range."
                    Op::Swizzle => {
                        let ((a, _), (index, by)) = (self.vec(inst, op, o.value())?, self.vec(inst, op, o.value())?);
                        let (n, iw) = (ty.lanes(), by.elem().bits(ptr_bits));
                        let mut out = 0u128;
                        for i in 0..n {
                            let k = lane(index, i, iw);
                            let x = match k < u128::from(n) {
                                true => lane(a, k as u32, w),
                                false => 0,
                            };
                            out = with_lane(out, i, w, x);
                        }
                        out
                    }
                    Op::Bitcast => {
                        let (v, _) = self.vec(inst, op, o.value())?;
                        v
                    }
                    _ => return Err(refuse("this vector operation")),
                }
            }
            Kind::ToScalar => match op {
                // "The vector, then the lane."
                Op::Extract => {
                    let (v, from) = self.vec(inst, op, o.value())?;
                    lane(v, o.imm(), from.elem().bits(ptr_bits))
                }
                Op::ReduceAdd | Op::ReduceMul | Op::ReduceAnd | Op::ReduceOr | Op::ReduceXor
                | Op::ReduceUmin | Op::ReduceUmax | Op::ReduceSmin | Op::ReduceSmax => {
                    let (v, from) = self.vec(inst, op, o.value())?;
                    let w = from.elem().bits(ptr_bits);
                    let mut acc = lane(v, 0, w);
                    for i in 1..from.lanes() {
                        acc = binary(fold_op(op), acc, lane(v, i, w), w)
                            .map_err(|_| refuse("an integer reduction that traps"))?;
                    }
                    acc
                }
                // "The start, then the vector: in lane order, unless `reassoc`" (a flag the
                // evaluator refuses before it gets here).
                Op::ReduceFadd | Op::ReduceFmul => {
                    let start = self.lane_operand(inst, op, o.value())?;
                    let (v, from) = self.vec(inst, op, o.value())?;
                    let w = from.elem().bits(ptr_bits);
                    let mut acc = start;
                    for i in 0..from.lanes() {
                        acc = float_bin(fold_op(op), from.lane(), acc, lane(v, i, w)).map_err(|error| placed(error, inst))?;
                    }
                    acc
                }
                // "An integer of each `<n x i1>` lane's bit, lane 0 lowest."
                Op::Bitmask => {
                    let (v, from) = self.vec(inst, op, o.value())?;
                    v & mask(from.lanes())
                }
                _ => {
                    return Err(refuse("a `reduce_fmin` or `reduce_fmax`, whose NaN and signed-zero result op.rs does not state"));
                }
            },
        };
        Ok(Some(Val::Int(bits)))
    }
}

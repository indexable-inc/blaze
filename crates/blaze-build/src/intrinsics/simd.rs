//! The `simd_*` intrinsics the library's NEON code calls, built lane by lane.
//!
//! "In this module, a "vector" is any `repr(simd)` type." (`library/core/src/intrinsics/simd/
//! mod.rs:3`). A vector is laid out as its array `[E; N]` at the target's short-vector
//! alignment (layout/simd.rs) and held by the walk as an aggregate, so every vector argument
//! reaches [`build`] as an address ([`Arg::Place`]) and every vector result is a temporary
//! the walk copies out ([`Out::At`]). Each intrinsic is N lane operations: a load of lane `i` at
//! byte `i * size_of::<E>()`, the scalar operation the walk builds for `E` (wrapping integer
//! arithmetic, the float operations of walk/numbers.rs, the compares of the scalar operators,
//! signed or unsigned by `E`), a store into the result's lane. No vector register is used:
//! the emitter has no vector form yet, and the lanes give the same values.
//!
//! In the table ([`TABLE`]): the compares (`simd_eq`/`ne`/`lt`/`le`/`gt`/`ge`), `simd_and`/
//! `or`/`xor`, `simd_add`/`sub`/`mul`, `simd_neg`, `simd_splat`, `simd_extract`/`simd_insert`
//! (the index must be a constant, as the declaration says: anything else is refused by name)
//! and `simd_select`: what hashbrown's NEON group (`vceq_u8`, `vcltz_s8`, `vcgez_s8`,
//! `vdup_n_u8`, `vget_lane_u64`, `vorr_u8`) and the vector types' `splat` reach. The other rows
//! (the reductions, `simd_bitmask`, `simd_shuffle`, the casts, shifts, divisions, saturating
//! arithmetic, float rounding, `fma`, masked and gathered accesses) are built as one LIR vector
//! operation each by intrinsics/simd_vec.rs. A `simd_*` intrinsic not in the table
//! (`simd_funnel_shl`, `simd_fsin`, the pointer casts, ...) is refused by
//! name as any intrinsic the table lacks. `f16` and `f128` lanes are built as any float lane
//! (walk/halfquad.rs; blaze-lir's legalize widens or calls for them before the Reference and
//! Fast tiers). A fn declared in an `extern "unadjusted"` block with a
//! `link_name = "llvm.aarch64.neon.*"` is not an intrinsic of this table: front2 refuses its
//! ABI by name.

use core::alloc::Allocator;

use blaze_lir::builder::{Makes, MemAttrs};
use blaze_lir::func::SlotKind;
use blaze_lir::{Flags, FloatCond, IntCond, Op, Sink, Ty, Value};
use blaze_typeck::{PNode, PTy, UintTy};

use super::{Arg, B, Call, Entry, IntrinsicError, Out, Which, e, lir, ty_arg};
use crate::layout::Prim;
use crate::layout::simd::Lanes;

#[cfg(test)]
mod tests;

/// A `simd_*` intrinsic of the table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SimdOp {
    Add,
    Sub,
    Mul,
    Neg,
    And,
    Or,
    Xor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Splat,
    Extract,
    Insert,
    Select,
    SelectBitmask,
    // Built as one LIR vector operation each (intrinsics/simd_vec.rs).
    Div,
    Rem,
    Shl,
    Shr,
    Cast,
    As,
    Fabs,
    Fsqrt,
    Floor,
    Ceil,
    Round,
    RoundTiesEven,
    Trunc,
    Fma,
    RelaxedFma,
    MinimumNumberNsz,
    MaximumNumberNsz,
    Bswap,
    Bitreverse,
    Ctlz,
    Ctpop,
    Cttz,
    SaturatingAdd,
    SaturatingSub,
    Shuffle,
    ReduceAddOrdered,
    ReduceAddUnordered,
    ReduceMulOrdered,
    ReduceMulUnordered,
    ReduceAll,
    ReduceAny,
    ReduceMax,
    ReduceMin,
    ReduceAnd,
    ReduceOr,
    ReduceXor,
    Bitmask,
    MaskedLoad,
    MaskedStore,
    Gather,
    Scatter,
}

/// The `simd_*` rows, sorted by name; each meaning the first sentence of the declaration's doc
/// comment in the pinned `library/core/src/intrinsics/simd/mod.rs`.
pub const TABLE: &[Entry] = &[
    e("simd_add", Which::Simd(SimdOp::Add), "Adds two simd vectors elementwise.", "per lane: `add` (wrapping) or `fadd`"),
    e("simd_and", Which::Simd(SimdOp::And), "\"And\"s vectors elementwise.", "per lane: `and`"),
    e("simd_as", Which::Simd(SimdOp::As), "Numerically casts a vector, elementwise.", "one vector `sext`/`zext`/`trunc`/`fpext`/`fptrunc`/`sitofp`/`uitofp`/`fptosi`/`fptoui` (saturating)"),
    e("simd_bitmask", Which::Simd(SimdOp::Bitmask), "Truncates an integer vector to a bitmask.", "`icmp ne 0`, then one `bitmask` (lane 0 lowest)"),
    e("simd_bitreverse", Which::Simd(SimdOp::Bitreverse), "Reverses bits of each element.", "one vector `bitrev`"),
    e("simd_bswap", Which::Simd(SimdOp::Bswap), "Swaps bytes of each element.", "one vector `bswap`"),
    e("simd_cast", Which::Simd(SimdOp::Cast), "Numerically casts a vector, elementwise.", "one vector conversion; float to integer `in_range` (as `to_int_unchecked`)"),
    e("simd_ceil", Which::Simd(SimdOp::Ceil), "Rounds up each element to the next highest integer-valued float.", "one vector `ceil`"),
    e("simd_ctlz", Which::Simd(SimdOp::Ctlz), "Counts the leading zeros of each element.", "one vector `clz`"),
    e("simd_ctpop", Which::Simd(SimdOp::Ctpop), "Counts the number of ones in each element.", "one vector `popcnt`"),
    e("simd_cttz", Which::Simd(SimdOp::Cttz), "Counts the trailing zeros of each element.", "one vector `ctz`"),
    e("simd_div", Which::Simd(SimdOp::Div), "Divides `lhs` by `rhs` elementwise.", "one vector `udiv`/`sdiv` (`nonzero`, signed `nsw`) or `fdiv`"),
    e("simd_eq", Which::Simd(SimdOp::Eq), "Tests elementwise equality of two vectors.", "per lane: `icmp eq` or `fcmp oeq`, then 0 or all ones"),
    e("simd_extract", Which::Simd(SimdOp::Extract), "Extracts an element from a vector.", "a `load` of the lane at the constant index"),
    e("simd_fabs", Which::Simd(SimdOp::Fabs), "Returns absolute value of a vector, elementwise.", "one vector `fabs`"),
    e("simd_floor", Which::Simd(SimdOp::Floor), "Rounds down each element to the next lowest integer-valued float.", "one vector `floor`"),
    e("simd_fma", Which::Simd(SimdOp::Fma), "Computes `(x*y) + z` for each element, but without any intermediate rounding.", "one vector `fma`"),
    e("simd_fsqrt", Which::Simd(SimdOp::Fsqrt), "Takes the square root of each element.", "one vector `sqrt`"),
    e("simd_gather", Which::Simd(SimdOp::Gather), "Reads a vector of pointers.", "one `gather` on the mask `!= 0`"),
    e("simd_ge", Which::Simd(SimdOp::Ge), "Tests if `x` is greater than or equal to `y`, elementwise.", "per lane: `icmp sge`/`uge` or `fcmp oge`, then 0 or all ones"),
    e("simd_gt", Which::Simd(SimdOp::Gt), "Tests if `x` is greater than `y`, elementwise.", "per lane: `icmp sgt`/`ugt` or `fcmp ogt`, then 0 or all ones"),
    e("simd_insert", Which::Simd(SimdOp::Insert), "Inserts an element into a vector, returning the updated vector.", "every lane copied, the one at the constant index replaced"),
    e("simd_le", Which::Simd(SimdOp::Le), "Tests if `x` is less than or equal to `y`, elementwise.", "per lane: `icmp sle`/`ule` or `fcmp ole`, then 0 or all ones"),
    e("simd_lt", Which::Simd(SimdOp::Lt), "Tests if `x` is less than `y`, elementwise.", "per lane: `icmp slt`/`ult` or `fcmp olt`, then 0 or all ones"),
    e("simd_masked_load", Which::Simd(SimdOp::MaskedLoad), "Reads a vector of pointers.", "one `masked_load` on the mask `!= 0`, aligned to one byte"),
    e("simd_masked_store", Which::Simd(SimdOp::MaskedStore), "Writes to a vector of pointers.", "one `masked_store` on the mask `!= 0`, aligned to one byte"),
    e("simd_maximum_number_nsz", Which::Simd(SimdOp::MaximumNumberNsz), "Returns the maximum of two vectors, elementwise.", "one vector `fmax_num nsz`"),
    e("simd_minimum_number_nsz", Which::Simd(SimdOp::MinimumNumberNsz), "Returns the minimum of two vectors, elementwise.", "one vector `fmin_num nsz`"),
    e("simd_mul", Which::Simd(SimdOp::Mul), "Multiplies two simd vectors elementwise.", "per lane: `mul` (wrapping) or `fmul`"),
    e("simd_ne", Which::Simd(SimdOp::Ne), "Tests elementwise inequality equality of two vectors.", "per lane: `icmp ne` or `fcmp une`, then 0 or all ones"),
    e("simd_neg", Which::Simd(SimdOp::Neg), "Negates a vector elementwise.", "per lane: `neg` (wrapping) or `fneg`"),
    e("simd_or", Which::Simd(SimdOp::Or), "\"Ors\" vectors elementwise.", "per lane: `or`"),
    e("simd_reduce_add_ordered", Which::Simd(SimdOp::ReduceAddOrdered), "Adds elements within a vector from left to right.", "`reduce_add` then `add` of the start, or `reduce_fadd` from the start"),
    e("simd_reduce_add_unordered", Which::Simd(SimdOp::ReduceAddUnordered), "Adds elements within a vector in arbitrary order.", "`reduce_add`, or `reduce_fadd reassoc` from -0.0"),
    e("simd_reduce_all", Which::Simd(SimdOp::ReduceAll), "Checks if all mask values are true.", "`reduce_and`, then `!= 0`"),
    e("simd_reduce_and", Which::Simd(SimdOp::ReduceAnd), "Logical \"and\"s all elements together.", "one `reduce_and`"),
    e("simd_reduce_any", Which::Simd(SimdOp::ReduceAny), "Checks if any mask value is true.", "`reduce_or`, then `!= 0`"),
    e("simd_reduce_max", Which::Simd(SimdOp::ReduceMax), "Returns the maximum element of a vector.", "one `reduce_umax`/`reduce_smax`/`reduce_fmax`"),
    e("simd_reduce_min", Which::Simd(SimdOp::ReduceMin), "Returns the minimum element of a vector.", "one `reduce_umin`/`reduce_smin`/`reduce_fmin`"),
    e("simd_reduce_mul_ordered", Which::Simd(SimdOp::ReduceMulOrdered), "Multiplies elements within a vector from left to right.", "`reduce_mul` then `mul` of the start, or `reduce_fmul` from the start"),
    e("simd_reduce_mul_unordered", Which::Simd(SimdOp::ReduceMulUnordered), "Multiplies elements within a vector in arbitrary order.", "`reduce_mul`, or `reduce_fmul reassoc` from 1.0"),
    e("simd_reduce_or", Which::Simd(SimdOp::ReduceOr), "Logical \"ors\" all elements together.", "one `reduce_or`"),
    e("simd_reduce_xor", Which::Simd(SimdOp::ReduceXor), "Logical \"exclusive ors\" all elements together.", "one `reduce_xor`"),
    e("simd_relaxed_fma", Which::Simd(SimdOp::RelaxedFma), "Computes `(x*y) + z` for each element, non-deterministically executing either a fused multiply-add or two operations with rounding of the intermediate result.", "one vector `fmuladd`"),
    e("simd_rem", Which::Simd(SimdOp::Rem), "Returns remainder of two vectors elementwise.", "one vector `urem`/`srem` (`nonzero`, signed `nsw`) or `frem`"),
    e("simd_round", Which::Simd(SimdOp::Round), "Rounds each element to the closest integer-valued float.", "one vector `fround` (ties away from zero)"),
    e("simd_round_ties_even", Which::Simd(SimdOp::RoundTiesEven), "Rounds each element to the closest integer-valued float.", "one vector `nearest` (ties to even)"),
    e("simd_saturating_add", Which::Simd(SimdOp::SaturatingAdd), "Adds two simd vectors elementwise, with saturation.", "one vector `uadd_sat`/`sadd_sat`"),
    e("simd_saturating_sub", Which::Simd(SimdOp::SaturatingSub), "Subtracts two simd vectors elementwise, with saturation.", "one vector `usub_sat`/`ssub_sat`"),
    e("simd_scatter", Which::Simd(SimdOp::Scatter), "Writes to a vector of pointers.", "one `scatter` on the mask `!= 0`"),
    e("simd_select", Which::Simd(SimdOp::Select), "Selects elements from a mask.", "per lane: `select` on the mask lane `!= 0`"),
    e("simd_select_bitmask", Which::Simd(SimdOp::SelectBitmask), "Selects elements from a bitmask.", "per lane `i`: `select` on bit `i` of the integer mask (of byte `i / 8`, bit `i % 8`, of a `[u8]` mask)"),
    e("simd_shl", Which::Simd(SimdOp::Shl), "Shifts vector left elementwise, with UB on overflow.", "one vector `shl in_range`"),
    e("simd_shr", Which::Simd(SimdOp::Shr), "Shifts vector right elementwise, with UB on overflow.", "one vector `lshr`/`ashr in_range`"),
    e("simd_shuffle", Which::Simd(SimdOp::Shuffle), "Shuffles two vectors by const indices.", "one `shuffle` of the const's indices"),
    e("simd_splat", Which::Simd(SimdOp::Splat), "Creates a vector where every lane has the provided value.", "a `store` of the value into every lane"),
    e("simd_sub", Which::Simd(SimdOp::Sub), "Subtracts `rhs` from `lhs` elementwise.", "per lane: `sub` (wrapping) or `fsub`"),
    e("simd_trunc", Which::Simd(SimdOp::Trunc), "Returns the integer part of each element as an integer-valued float.", "one vector `ftrunc`"),
    e("simd_xor", Which::Simd(SimdOp::Xor), "\"Exclusive ors\" vectors elementwise.", "per lane: `xor`"),
];

/// The row of `name`, when the `simd_*` table has it.
pub(super) fn named(name: &str) -> Option<Which> {
    TABLE.binary_search_by(|e| e.name.cmp(name)).ok().map(|i| TABLE[i].which)
}

/// What a lane is to the operations.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Int { signed: bool },
    Float,
    Ptr,
}

pub(super) fn kind(p: Prim) -> Kind {
    match p {
        Prim::Int { signed, .. } => Kind::Int { signed },
        Prim::Ptr => Kind::Ptr,
        Prim::F16 | Prim::F32 | Prim::F64 | Prim::F128 => Kind::Float,
    }
}

/// The lanes of vector type `t`, refused by name when `t` is not a vector. `f16` and `f128`
/// lanes are float lanes (P2 builds both; blaze-lir's legalize widens or calls for them).
pub(super) fn lanes<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, t: PTy) -> Result<Lanes, IntrinsicError> {
    s.cx.layouts.lanes(t, s.cx.cx)?.ok_or(s.shape("a type that should be a vector is not a `repr(simd)` type"))
}

/// Lane `i`'s byte offset. A vector is at most 16 bytes (layout/simd.rs), so it fits.
fn at(l: &Lanes, i: u64) -> i32 {
    (i * l.stride) as i32
}

fn load<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, l: &Lanes, base: Value, i: u64) -> Value {
    s.b.load(lir(l.elem).0, Flags::NONE, base, at(l, i), MemAttrs::aligned(l.elem.align()))
}

fn store<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, l: &Lanes, v: Value, base: Value, i: u64) {
    s.b.store(Flags::NONE, v, base, at(l, i), MemAttrs::aligned(l.elem.align()));
}

/// A fresh temporary of the vector's size and alignment: the result the walk copies out.
fn temp<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, l: &Lanes) -> Value {
    let slot = s.b.slot(l.size as u32, l.align, SlotKind::Temp);
    s.b.slot_addr(slot)
}

/// The lane index `v` as a constant: "`idx` must be `const`" (`simd_extract`'s and
/// `simd_insert`'s declarations), read back from the `iconst` the walk built for it (a literal
/// or a `const { .. }` block, as core_arch's `simd_extract!` writes it). Anything else is
/// refused by name, as is an index past the lanes ("`idx` must be in-bounds of the vector").
fn const_index<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &B<'_, '_, '_, '_, LA, S, A>, v: Value, l: &Lanes) -> Result<u64, IntrinsicError> {
    let idx = match S::MAKES {
        // The request sink makes no instructions (blaze-lir builder.rs `push`): no constant to
        // read and no code for the lane to reach. The code build of the same body reads it.
        Makes::Requests => 0,
        Makes::Code => {
            let view = s.b.func().view();
            let i = v.index();
            if i >= view.len() || view.op(i) != Op::Iconst || s.b.ty(v) != Ty::I32 {
                return Err(IntrinsicError::Unsupported { which: s.which, why: "a lane index that is not a constant (the declaration: \"`idx` must be `const`\")" });
            }
            // A 32-bit `iconst` holds its value in its one word.
            u64::from(view.operands(i).word())
        }
    };
    if idx >= l.count {
        return Err(s.shape("a lane index past the vector's lanes (the declaration: \"`idx` must be in-bounds of the vector\")"));
    }
    Ok(idx)
}

/// Builds `simd_*` call `c` of `op`, its arguments already built.
pub(super) fn build<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
    s: &mut B<'_, '_, '_, '_, LA, S, A>,
    c: &Call<'_>,
    op: SimdOp,
) -> Result<Out, IntrinsicError> {
    let which = c.which;
    let shape = move |why: &'static str| IntrinsicError::Shape { which, why };
    let place = |i: usize| match c.args.get(i) {
        Some(Arg::Place(v)) => Ok(*v),
        _ => Err(shape("a vector argument that is not held in memory")),
    };
    let scalar = |i: usize| match c.args.get(i) {
        Some(Arg::Scalar(v)) => Ok(*v),
        _ => Err(shape("a lane or index argument that is not a scalar")),
    };
    let ty = |i: usize| ty_arg(c, i).ok_or(shape("a missing type argument"));
    let not_int_or_float = "`T` must be a vector of integers or floats";
    Ok(match op {
        // "`T` must be a vector of integers or floats. For integers, wrapping arithmetic is
        // used." (`simd_add`, `simd_sub`, `simd_mul`); "`T` must be a vector of integers."
        // (`simd_and`, `simd_or`, `simd_xor`).
        SimdOp::Add | SimdOp::Sub | SimdOp::Mul | SimdOp::And | SimdOp::Or | SimdOp::Xor => {
            let l = lanes(s, ty(0)?)?;
            let (x, y) = (place(0)?, place(1)?);
            let lop = match (op, kind(l.elem)) {
                (SimdOp::Add, Kind::Int { .. }) => Op::Add,
                (SimdOp::Sub, Kind::Int { .. }) => Op::Sub,
                (SimdOp::Mul, Kind::Int { .. }) => Op::Mul,
                (SimdOp::Add, Kind::Float) => Op::Fadd,
                (SimdOp::Sub, Kind::Float) => Op::Fsub,
                (SimdOp::Mul, Kind::Float) => Op::Fmul,
                (SimdOp::And, Kind::Int { .. }) => Op::And,
                (SimdOp::Or, Kind::Int { .. }) => Op::Or,
                (SimdOp::Xor, Kind::Int { .. }) => Op::Xor,
                (SimdOp::And | SimdOp::Or | SimdOp::Xor, _) => return Err(shape("`T` must be a vector of integers")),
                _ => return Err(shape(not_int_or_float)),
            };
            let out = temp(s, &l);
            for i in 0..l.count {
                let a = load(s, &l, x, i);
                let b = load(s, &l, y, i);
                let r = s.b.binary(lop, Flags::NONE, a, b);
                store(s, &l, r, out, i);
            }
            Out::At(out)
        }
        // "`T` must be a vector of integers or floats. For integers, wrapping arithmetic is used."
        SimdOp::Neg => {
            let l = lanes(s, ty(0)?)?;
            let x = place(0)?;
            let lop = match kind(l.elem) {
                Kind::Int { .. } => Op::Neg,
                Kind::Float => Op::Fneg,
                Kind::Ptr => return Err(shape(not_int_or_float)),
            };
            let out = temp(s, &l);
            for i in 0..l.count {
                let a = load(s, &l, x, i);
                let r = s.b.unary(lop, Flags::NONE, a);
                store(s, &l, r, out, i);
            }
            Out::At(out)
        }
        // "`T` must be a vector of integers or floats. `U` must be a vector of integers with
        // the same number of elements and element size as `T`. Returns `0` for false and `!0`
        // for true." A float lane compares as the scalar operator does (walk/numbers.rs):
        // ordered, except `!=` (`une`: true when either is NaN).
        SimdOp::Eq | SimdOp::Ne | SimdOp::Lt | SimdOp::Le | SimdOp::Gt | SimdOp::Ge => {
            let l = lanes(s, ty(0)?)?;
            let m = lanes(s, ty(1)?)?;
            if m.count != l.count || m.stride != l.stride || !matches!(m.elem, Prim::Int { .. }) {
                return Err(shape("`U` must be a vector of integers with the same number of elements and element size as `T`"));
            }
            let (x, y) = (place(0)?, place(1)?);
            let k = kind(l.elem);
            let (mty, _) = lir(m.elem);
            let ones = s.b.iconst(mty, u128::MAX);
            let zero = s.b.iconst(mty, 0);
            let out = temp(s, &m);
            for i in 0..l.count {
                let a = load(s, &l, x, i);
                let b = load(s, &l, y, i);
                let bit = match k {
                    Kind::Int { signed } => {
                        let cond = match (op, signed) {
                            (SimdOp::Eq, _) => IntCond::Eq,
                            (SimdOp::Ne, _) => IntCond::Ne,
                            (SimdOp::Lt, true) => IntCond::Slt,
                            (SimdOp::Lt, false) => IntCond::Ult,
                            (SimdOp::Le, true) => IntCond::Sle,
                            (SimdOp::Le, false) => IntCond::Ule,
                            (SimdOp::Gt, true) => IntCond::Sgt,
                            (SimdOp::Gt, false) => IntCond::Ugt,
                            (SimdOp::Ge, true) => IntCond::Sge,
                            (_, _) => IntCond::Uge,
                        };
                        s.b.icmp(cond, a, b)
                    }
                    Kind::Float => {
                        let cond = match op {
                            SimdOp::Eq => FloatCond::Oeq,
                            SimdOp::Ne => FloatCond::Une,
                            SimdOp::Lt => FloatCond::Olt,
                            SimdOp::Le => FloatCond::Ole,
                            SimdOp::Gt => FloatCond::Ogt,
                            _ => FloatCond::Oge,
                        };
                        s.b.fcmp(cond, a, b)
                    }
                    Kind::Ptr => return Err(shape(not_int_or_float)),
                };
                let lane = s.b.select(Flags::NONE, bit, ones, zero);
                store(s, &m, lane, out, i);
            }
            Out::At(out)
        }
        // "`T` must be a vector with element type `U`."
        SimdOp::Splat => {
            let l = lanes(s, ty(0)?)?;
            let v = scalar(0)?;
            if s.b.ty(v) != lir(l.elem).0 {
                return Err(shape("`T` must be a vector with element type `U`"));
            }
            let out = temp(s, &l);
            for i in 0..l.count {
                store(s, &l, v, out, i);
            }
            Out::At(out)
        }
        // "`T` must be a vector with element type `U`, and `idx` must be `const`."
        SimdOp::Extract => {
            let l = lanes(s, ty(0)?)?;
            if s.scalar(ty(1)?)?.0 != lir(l.elem).0 {
                return Err(shape("`T` must be a vector with element type `U`"));
            }
            let x = place(0)?;
            let idx = const_index(s, scalar(1)?, &l)?;
            Out::Scalar(load(s, &l, x, idx))
        }
        // "`T` must be a vector with element type `U`, and `idx` must be `const`."
        SimdOp::Insert => {
            let l = lanes(s, ty(0)?)?;
            let x = place(0)?;
            let idx = const_index(s, scalar(1)?, &l)?;
            let v = scalar(2)?;
            if s.b.ty(v) != lir(l.elem).0 {
                return Err(shape("`T` must be a vector with element type `U`"));
            }
            let out = temp(s, &l);
            for i in 0..l.count {
                let lane = if i == idx { v } else { load(s, &l, x, i) };
                store(s, &l, lane, out, i);
            }
            Out::At(out)
        }
        // "`T` must be a vector. `M` must be an integer vector with the same length as `T` (but
        // any element size). For each element, if the corresponding value in `mask` is `!0`,
        // select the element from `if_true`. If the corresponding value in `mask` is `0`,
        // select the element from `if_false`." (and "`mask` must only contain `0` and `!0`":
        // a lane is tested `!= 0`).
        SimdOp::Select => {
            let m = lanes(s, ty(0)?)?;
            let l = lanes(s, ty(1)?)?;
            if m.count != l.count || !matches!(m.elem, Prim::Int { .. }) {
                return Err(shape("`M` must be an integer vector with the same length as `T`"));
            }
            let (mask, yes, no) = (place(0)?, place(1)?, place(2)?);
            let zero = s.b.iconst(lir(m.elem).0, 0);
            let out = temp(s, &l);
            for i in 0..l.count {
                let bits = load(s, &m, mask, i);
                let set = s.b.icmp(IntCond::Ne, bits, zero);
                let a = load(s, &l, yes, i);
                let b = load(s, &l, no, i);
                let lane = s.b.select(Flags::NONE, set, a, b);
                store(s, &l, lane, out, i);
            }
            Out::At(out)
        }
        // "`T` must be a vector. `M` must be an unsigned integer or array of `u8`, matching
        // `simd_bitmask`. For each element, if the bit in `mask` is `1`, select the element from
        // `if_true`. If the corresponding bit in `mask` is `0`, select the element from
        // `if_false`. The remaining bits of the mask are ignored. The bitmask bit order matches
        // `simd_bitmask`": lane 0 the lowest bit, so the bit of lane `i` is bit `i % 8` of byte
        // `i / 8` (little-endian: an integer's bit `i` too).
        SimdOp::SelectBitmask => {
            let mt = ty(0)?;
            let l = lanes(s, ty(1)?)?;
            let u8_mask = match s.node(mt) {
                Some(PNode::Uint(_)) => true,
                Some(PNode::Array(elem, _)) => s.node(elem) == Some(PNode::Uint(UintTy::U8)),
                _ => false,
            };
            if !u8_mask {
                return Err(shape("`M` must be an unsigned integer or array of `u8`"));
            }
            let id = s.layout(mt)?;
            let bytes = s.cx.layouts.get(id).size;
            if bytes * 8 < l.count {
                return Err(shape("a mask of fewer bits than the vector's lanes (`M` must match `simd_bitmask`)"));
            }
            let (yes, no) = (place(1)?, place(2)?);
            let mask = c.args.first().copied().ok_or(shape("a missing mask"))?;
            let out = temp(s, &l);
            for i in 0..l.count {
                let (word, bit) = match mask {
                    Arg::Scalar(m) => (m, i),
                    Arg::Place(at) => (s.b.load(Ty::I8, Flags::NONE, at, (i / 8) as i32, MemAttrs::aligned(0)), i % 8),
                    Arg::None => return Err(shape("a zero-sized mask")),
                };
                let wty = s.b.ty(word);
                let amount = s.b.iconst(wty, u128::from(bit));
                let shifted = s.b.binary(Op::Lshr, Flags::NONE, word, amount);
                let one = s.b.iconst(wty, 1);
                let low = s.b.binary(Op::And, Flags::NONE, shifted, one);
                let set = s.b.icmp(IntCond::Eq, low, one);
                let a = load(s, &l, yes, i);
                let b = load(s, &l, no, i);
                let lane = s.b.select(Flags::NONE, set, a, b);
                store(s, &l, lane, out, i);
            }
            Out::At(out)
        }
        // Every other row: one LIR vector operation (intrinsics/simd_vec.rs).
        _ => return super::simd_vec::build(s, c, op),
    })
}

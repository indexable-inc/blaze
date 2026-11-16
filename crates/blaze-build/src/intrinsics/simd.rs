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
//! `vdup_n_u8`, `vget_lane_u64`, `vorr_u8`) and the vector types' `splat` reach. Every other
//! `simd_*` intrinsic (the reductions, `simd_bitmask`, `simd_shuffle`, the casts, ...) is not in
//! the table and is refused by name as any intrinsic the table lacks. `f16` lanes are refused
//! by name (`f16` is not built). A fn declared in an `extern "unadjusted"` block with a
//! `link_name = "llvm.aarch64.neon.*"` is not an intrinsic of this table: front2 refuses its
//! ABI by name.

use core::alloc::Allocator;

use blaze_lir::builder::{Makes, MemAttrs};
use blaze_lir::func::SlotKind;
use blaze_lir::{Flags, FloatCond, IntCond, Op, Sink, Ty, Value};
use blaze_typeck::PTy;

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
}

/// The `simd_*` rows, sorted by name; each meaning the first sentence of the declaration's doc
/// comment in the pinned `library/core/src/intrinsics/simd/mod.rs`.
pub const TABLE: &[Entry] = &[
    e("simd_add", Which::Simd(SimdOp::Add), "Adds two simd vectors elementwise.", "per lane: `add` (wrapping) or `fadd`"),
    e("simd_and", Which::Simd(SimdOp::And), "\"And\"s vectors elementwise.", "per lane: `and`"),
    e("simd_eq", Which::Simd(SimdOp::Eq), "Tests elementwise equality of two vectors.", "per lane: `icmp eq` or `fcmp oeq`, then 0 or all ones"),
    e("simd_extract", Which::Simd(SimdOp::Extract), "Extracts an element from a vector.", "a `load` of the lane at the constant index"),
    e("simd_ge", Which::Simd(SimdOp::Ge), "Tests if `x` is greater than or equal to `y`, elementwise.", "per lane: `icmp sge`/`uge` or `fcmp oge`, then 0 or all ones"),
    e("simd_gt", Which::Simd(SimdOp::Gt), "Tests if `x` is greater than `y`, elementwise.", "per lane: `icmp sgt`/`ugt` or `fcmp ogt`, then 0 or all ones"),
    e("simd_insert", Which::Simd(SimdOp::Insert), "Inserts an element into a vector, returning the updated vector.", "every lane copied, the one at the constant index replaced"),
    e("simd_le", Which::Simd(SimdOp::Le), "Tests if `x` is less than or equal to `y`, elementwise.", "per lane: `icmp sle`/`ule` or `fcmp ole`, then 0 or all ones"),
    e("simd_lt", Which::Simd(SimdOp::Lt), "Tests if `x` is less than `y`, elementwise.", "per lane: `icmp slt`/`ult` or `fcmp olt`, then 0 or all ones"),
    e("simd_mul", Which::Simd(SimdOp::Mul), "Multiplies two simd vectors elementwise.", "per lane: `mul` (wrapping) or `fmul`"),
    e("simd_ne", Which::Simd(SimdOp::Ne), "Tests elementwise inequality equality of two vectors.", "per lane: `icmp ne` or `fcmp une`, then 0 or all ones"),
    e("simd_neg", Which::Simd(SimdOp::Neg), "Negates a vector elementwise.", "per lane: `neg` (wrapping) or `fneg`"),
    e("simd_or", Which::Simd(SimdOp::Or), "\"Ors\" vectors elementwise.", "per lane: `or`"),
    e("simd_select", Which::Simd(SimdOp::Select), "Selects elements from a mask.", "per lane: `select` on the mask lane `!= 0`"),
    e("simd_splat", Which::Simd(SimdOp::Splat), "Creates a vector where every lane has the provided value.", "a `store` of the value into every lane"),
    e("simd_sub", Which::Simd(SimdOp::Sub), "Subtracts `rhs` from `lhs` elementwise.", "per lane: `sub` (wrapping) or `fsub`"),
    e("simd_xor", Which::Simd(SimdOp::Xor), "\"Exclusive ors\" vectors elementwise.", "per lane: `xor`"),
];

/// The row of `name`, when the `simd_*` table has it.
pub(super) fn named(name: &str) -> Option<Which> {
    TABLE.binary_search_by(|e| e.name.cmp(name)).ok().map(|i| TABLE[i].which)
}

/// What a lane is to the operations.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int { signed: bool },
    Float,
    Ptr,
}

fn kind(p: Prim) -> Kind {
    match p {
        Prim::Int { signed, .. } => Kind::Int { signed },
        Prim::Ptr => Kind::Ptr,
        Prim::F16 | Prim::F32 | Prim::F64 | Prim::F128 => Kind::Float,
    }
}

/// The lanes of vector type `t`, refused by name when `t` is not a vector or its lanes are
/// `f16`.
fn lanes<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, t: PTy) -> Result<Lanes, IntrinsicError> {
    let l = s.cx.layouts.lanes(t, s.cx.cx)?.ok_or(s.shape("a type that should be a vector is not a `repr(simd)` type"))?;
    if matches!(l.elem, Prim::F16 | Prim::F128) {
        return Err(IntrinsicError::Unsupported { which: s.which, why: "a vector of `f16` or `f128` lanes" });
    }
    Ok(l)
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
    })
}

//! The `simd_*` intrinsics built as LIR vector operations: one operation each
//! (port27/tempo-run/INTRINSICS.md 3.2 and P6).
//!
//! A vector argument reaches [`build`] as an address ([`Arg::Place`], intrinsics/simd.rs); it
//! is loaded whole as the LIR vector `<count x lane>`, the one operation the intrinsic names is
//! applied, and a vector result is stored whole into a temporary the walk copies out
//! ([`Out::At`]). The Optimized tier keeps the vector for LLVM; the Reference and Fast tiers
//! take the function after blaze-lir's legalize (`legalize/vector.rs`), which makes every one
//! of these operations scalar, lane by lane.
//!
//! The rules are the declarations' doc comments in the pinned
//! `library/core/src/intrinsics/simd/mod.rs`, quoted at each arm. The promises they state as
//! "undefined behavior" become the operation's flags, with no check emitted:
//! - `simd_shl`/`simd_shr`: "Each element of `rhs` must be in `0..<int>::BITS`." -> `in_range`;
//! - `simd_div`/`simd_rem`: "For integers, `rhs` must not contain any zero elements.
//!   Additionally for signed integers, `<int>::MIN / -1` is undefined behavior." -> `nonzero`,
//!   and `nsw` when signed;
//! - `simd_cast` of a float to an integer: "following the same rules as `to_int_unchecked`"
//!   -> `in_range`; `simd_as` saturates ("This matches regular `as`"), so no flag.
//!
//! A vector LIR has no type for (a lane count that is not a power of two, or more than 512
//! bits) is refused by name: splitting it at build time is not built yet.
//! `simd_masked_load`/`simd_masked_store` are built at an alignment of one byte, which every
//! `ALIGN` (`SimdAlign`) satisfies: a weaker promise, never a wrong one.

use core::alloc::Allocator;

use blaze_lir::builder::{Makes, MemAttrs};
use blaze_lir::func::{RefId, SlotKind};
use blaze_lir::{Flags, IntCond, Op, Scalar, Sink, Ty, Value};

use super::simd::{Kind, SimdOp, kind, lanes};
use super::{Arg, B, Call, IntrinsicError, Out, lir, ty_arg};
use crate::layout::Prim;
use crate::layout::simd::Lanes;

#[cfg(test)]
mod tests;

/// The most lanes a shuffle names: `rustc_simd_monomorphize_lane_limit`'s 64 (layout/simd_rule.rs).
const MOST_LANES: usize = 64;

/// The LIR vector type of `l`'s lanes, or a refusal naming why LIR has none.
fn vector_ty(which: super::Which, l: &Lanes) -> Result<Ty, IntrinsicError> {
    let lane = lir(l.elem).0.elem();
    let ty = u32::try_from(l.count).ok().and_then(|n| Ty::vector(lane, n));
    match ty {
        // A padded vector (a lane count that is not a power of two, not packed) has bytes past
        // its lanes: the LIR vector would not cover them.
        Some(ty) if l.size == l.count * l.stride => Ok(ty),
        _ => Err(IntrinsicError::Unsupported {
            which,
            why: "a vector whose lane count is not a power of two or whose lanes pass 512 bits (LIR has no vector type for it; splitting it is not built)",
        }),
    }
}

/// The vector at `at`, loaded whole at the vector's alignment.
fn load_vector<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, l: &Lanes, ty: Ty, at: Value) -> Value {
    s.b.load(ty, Flags::NONE, at, 0, MemAttrs::aligned(l.align))
}

/// A fresh temporary of `l`'s size and alignment holding `v`: the result the walk copies out.
fn out_vector<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, l: &Lanes, v: Value) -> Out {
    let slot = s.b.slot(l.size as u32, l.align, SlotKind::Temp);
    let at = s.b.slot_addr(slot);
    s.b.store(Flags::NONE, v, at, 0, MemAttrs::aligned(l.align));
    Out::At(at)
}

/// `<count x i1>` of whether each lane of the integer vector `v` (of type `ty`) is not 0: a
/// mask lane is "`!0`" or "`0`" by the declarations, so `!= 0` reads both.
fn mask_bits<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(s: &mut B<'_, '_, '_, '_, LA, S, A>, ty: Ty, v: Value) -> Value {
    let zero = s.b.iconst(ty.lane(), 0);
    let zeros = s.b.convert(Op::Splat, Flags::NONE, ty, zero);
    s.b.icmp(IntCond::Ne, v, zeros)
}

/// The bits of `0.0`'s sign flipped (`-0.0`, the identity of `fadd`) or of `1.0` (of `fmul`) for
/// a float lane.
fn float_identity(lane: Scalar, one: bool) -> Option<u128> {
    Some(match (lane, one) {
        (Scalar::F16, false) => 0x8000,
        (Scalar::F32, false) => 0x8000_0000,
        (Scalar::F64, false) => 1 << 63,
        (Scalar::F128, false) => 1 << 127,
        (Scalar::F16, true) => 0x3c00,
        (Scalar::F32, true) => 0x3f80_0000,
        (Scalar::F64, true) => 0x3ff0_0000_0000_0000,
        (Scalar::F128, true) => 0x3fff << 112,
        _ => return None,
    })
}

/// The indices of a shuffle: "`U` must be a **const** vector of `u32`s" (`simd_shuffle`), so the
/// walk holds it as the address of the const's data row (`data_addr`), or of a temporary the
/// walk copied that row into (`memcpy`). Anything else is refused by name, as is an index past
/// the concatenation ("It is a compile-time error if `idx[i]` is out-of-bounds of `xy`").
fn shuffle_indices<LA: Allocator + Clone, S: Sink, A: Allocator + Clone>(
    s: &B<'_, '_, '_, '_, LA, S, A>,
    at: Value,
    count: usize,
    lanes_of_xy: u32,
    out: &mut [u32; MOST_LANES],
) -> Result<(), IntrinsicError> {
    let which = s.which;
    let refused = move || IntrinsicError::Unsupported {
        which,
        why: "shuffle indices that are not the bytes of a const (the declaration: \"`U` must be a **const** vector of `u32`s\")",
    };
    match S::MAKES {
        // The request sink makes no instructions (blaze-lir builder.rs `push`): no data to read
        // and no code for the shuffle to reach. The code build of the same body reads them.
        Makes::Requests => {
            out[..count].fill(0);
            return Ok(());
        }
        Makes::Code => {}
    }
    let view = s.b.func().view();
    let mut source = at.index();
    if source >= view.len() {
        return Err(refused());
    }
    if view.op(source) == Op::SlotAddr {
        // The temporary's one filling: the `memcpy` whose destination is this address.
        let mut from = None;
        for i in (source + 1..view.len()).rev() {
            if view.op(i) != Op::Memcpy {
                continue;
            }
            let mut o = view.operands(i);
            let (dst, src) = (o.value(), o.value());
            if dst == at {
                from = Some(src.index());
                break;
            }
        }
        source = from.ok_or_else(refused)?;
    }
    if source >= view.len() || view.op(source) != Op::DataAddr {
        return Err(refused());
    }
    let row = view.operands(source).word();
    let Some(data) = s.b.data_of(RefId(row)) else { return Err(refused()) };
    let bytes = view.data_bytes_of(data);
    if bytes.len() < count * 4 {
        return Err(IntrinsicError::Shape { which: s.which, why: "shuffle indices shorter than the result's lanes" });
    }
    for (k, word) in bytes.chunks_exact(4).take(count).enumerate() {
        let i = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        if i >= lanes_of_xy {
            return Err(IntrinsicError::Shape { which: s.which, why: "a shuffle index out of bounds of `xy` (the declaration: \"It is a compile-time error\")" });
        }
        out[k] = i;
    }
    Ok(())
}

/// Builds `simd_*` call `c` of `op` as one LIR vector operation, its arguments already built.
/// The table's lane-by-lane rows (intrinsics/simd.rs) never reach here.
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
        _ => Err(shape("a pointer or start argument that is not a scalar")),
    };
    let ty = |i: usize| ty_arg(c, i).ok_or(shape("a missing type argument"));
    let ints = "`T` must be a vector of integers";
    let floats = "`T` must be a vector of floats";
    let ints_or_floats = "`T` must be a vector of integers or floats";
    Ok(match op {
        // "`T` must be a vector of integers or floats." with the integer promises above.
        SimdOp::Div | SimdOp::Rem => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            let (lop, flags) = match (op, kind(l.elem)) {
                (SimdOp::Div, Kind::Int { signed: true }) => (Op::Sdiv, Flags(Flags::NONZERO.0 | Flags::NSW.0)),
                (SimdOp::Div, Kind::Int { signed: false }) => (Op::Udiv, Flags::NONZERO),
                (SimdOp::Rem, Kind::Int { signed: true }) => (Op::Srem, Flags(Flags::NONZERO.0 | Flags::NSW.0)),
                (SimdOp::Rem, Kind::Int { signed: false }) => (Op::Urem, Flags::NONZERO),
                (SimdOp::Div, Kind::Float) => (Op::Fdiv, Flags::NONE),
                (SimdOp::Rem, Kind::Float) => (Op::Frem, Flags::NONE),
                _ => return Err(shape(ints_or_floats)),
            };
            let (x, y) = (place(0)?, place(1)?);
            let a = load_vector(s, &l, vt, x);
            let b = load_vector(s, &l, vt, y);
            let r = s.b.binary(lop, flags, a, b);
            out_vector(s, &l, r)
        }
        // "`T` must be a vector of integers." "Shifts `lhs` left by `rhs`, shifting in zeros."
        // / "Shifts `lhs` right by `rhs`, shifting in sign bits for signed types."
        SimdOp::Shl | SimdOp::Shr => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            let lop = match (op, kind(l.elem)) {
                (SimdOp::Shl, Kind::Int { .. }) => Op::Shl,
                (SimdOp::Shr, Kind::Int { signed: true }) => Op::Ashr,
                (SimdOp::Shr, Kind::Int { signed: false }) => Op::Lshr,
                _ => return Err(shape(ints)),
            };
            let (x, y) = (place(0)?, place(1)?);
            let a = load_vector(s, &l, vt, x);
            let b = load_vector(s, &l, vt, y);
            let r = s.b.binary(lop, Flags::IN_RANGE, a, b);
            out_vector(s, &l, r)
        }
        // "`T` must be a vector of integer primitive types." (`simd_saturating_add`/`_sub`)
        SimdOp::SaturatingAdd | SimdOp::SaturatingSub => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            let lop = match (op, kind(l.elem)) {
                (SimdOp::SaturatingAdd, Kind::Int { signed: true }) => Op::SaddSat,
                (SimdOp::SaturatingAdd, Kind::Int { signed: false }) => Op::UaddSat,
                (SimdOp::SaturatingSub, Kind::Int { signed: true }) => Op::SsubSat,
                (SimdOp::SaturatingSub, Kind::Int { signed: false }) => Op::UsubSat,
                _ => return Err(shape("`T` must be a vector of integer primitive types")),
            };
            let (x, y) = (place(0)?, place(1)?);
            let a = load_vector(s, &l, vt, x);
            let b = load_vector(s, &l, vt, y);
            let r = s.b.binary(lop, Flags::NONE, a, b);
            out_vector(s, &l, r)
        }
        // `simd_minimum_number_nsz`/`simd_maximum_number_nsz`: a NaN lane gives the other
        // operand's, and the sign of a zero is not promised (`nsz`).
        SimdOp::MinimumNumberNsz | SimdOp::MaximumNumberNsz => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            if kind(l.elem) != Kind::Float {
                return Err(shape(floats));
            }
            let lop = if op == SimdOp::MinimumNumberNsz { Op::FminNum } else { Op::FmaxNum };
            let (x, y) = (place(0)?, place(1)?);
            let a = load_vector(s, &l, vt, x);
            let b = load_vector(s, &l, vt, y);
            let r = s.b.binary(lop, Flags::NSZ, a, b);
            out_vector(s, &l, r)
        }
        // "`T` must be a vector of floats." (`simd_fabs`, `simd_fsqrt`, `simd_ceil`, `simd_floor`,
        // `simd_round` "rounding ties away from zero", `simd_round_ties_even`, `simd_trunc`)
        SimdOp::Fabs | SimdOp::Fsqrt | SimdOp::Floor | SimdOp::Ceil | SimdOp::Round | SimdOp::RoundTiesEven | SimdOp::Trunc => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            if kind(l.elem) != Kind::Float {
                return Err(shape(floats));
            }
            let lop = match op {
                SimdOp::Fabs => Op::Fabs,
                SimdOp::Fsqrt => Op::Sqrt,
                SimdOp::Floor => Op::Floor,
                SimdOp::Ceil => Op::Ceil,
                SimdOp::Round => Op::Fround,
                SimdOp::RoundTiesEven => Op::Nearest,
                _ => Op::Ftrunc,
            };
            let x = place(0)?;
            let a = load_vector(s, &l, vt, x);
            let r = s.b.unary(lop, Flags::NONE, a);
            out_vector(s, &l, r)
        }
        // "`T` must be a vector of integers." (`simd_bswap`, `simd_bitreverse`, `simd_ctlz`,
        // `simd_ctpop`, `simd_cttz`); a zero lane counts its width (no `nonzero` promise).
        SimdOp::Bswap | SimdOp::Bitreverse | SimdOp::Ctlz | SimdOp::Ctpop | SimdOp::Cttz => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            if !matches!(kind(l.elem), Kind::Int { .. }) {
                return Err(shape(ints));
            }
            let lop = match op {
                SimdOp::Bswap => Op::Bswap,
                SimdOp::Bitreverse => Op::Bitrev,
                SimdOp::Ctlz => Op::Clz,
                SimdOp::Ctpop => Op::Popcnt,
                _ => Op::Ctz,
            };
            let x = place(0)?;
            let a = load_vector(s, &l, vt, x);
            let r = s.b.unary(lop, Flags::NONE, a);
            out_vector(s, &l, r)
        }
        // "Computes `(x*y) + z` for each element, but without any intermediate rounding."
        // (`simd_fma`); `simd_relaxed_fma`: "fused or not, as the backend finds faster".
        SimdOp::Fma | SimdOp::RelaxedFma => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            if kind(l.elem) != Kind::Float {
                return Err(shape(floats));
            }
            let lop = if op == SimdOp::Fma { Op::Fma } else { Op::Fmuladd };
            let (x, y, z) = (place(0)?, place(1)?, place(2)?);
            let a = load_vector(s, &l, vt, x);
            let b = load_vector(s, &l, vt, y);
            let cc = load_vector(s, &l, vt, z);
            let r = s.b.ternary(lop, Flags::NONE, a, b, cc);
            out_vector(s, &l, r)
        }
        // "`T` and `U` must be vectors of integers or floats, and must have the same length."
        // "When casting floats to integers, the result is truncated. ... When casting integers
        // to floats, the result is rounded. Otherwise, truncates or extends the value,
        // maintaining the sign for signed integers."
        SimdOp::Cast | SimdOp::As => {
            let l = lanes(s, ty(0)?)?;
            let m = lanes(s, ty(1)?)?;
            if l.count != m.count {
                return Err(shape("`T` and `U` must have the same length"));
            }
            let (from, to) = (vector_ty(which, &l)?, vector_ty(which, &m)?);
            let x = place(0)?;
            let a = load_vector(s, &l, from, x);
            let (fb, tb) = (l.stride, m.stride);
            let unchecked = if op == SimdOp::Cast { Flags::IN_RANGE } else { Flags::NONE };
            let r = match (kind(l.elem), kind(m.elem)) {
                (Kind::Int { .. }, Kind::Int { .. }) if fb == tb => a,
                (Kind::Int { signed }, Kind::Int { .. }) if fb < tb => s.b.convert(if signed { Op::Sext } else { Op::Zext }, Flags::NONE, to, a),
                (Kind::Int { .. }, Kind::Int { .. }) => s.b.convert(Op::Trunc, Flags::NONE, to, a),
                (Kind::Float, Kind::Float) if fb == tb => a,
                (Kind::Float, Kind::Float) if fb < tb => s.b.convert(Op::Fpext, Flags::NONE, to, a),
                (Kind::Float, Kind::Float) => s.b.convert(Op::Fptrunc, Flags::NONE, to, a),
                (Kind::Int { signed: true }, Kind::Float) => s.b.convert(Op::SitoFp, Flags::NONE, to, a),
                (Kind::Int { signed: false }, Kind::Float) => s.b.convert(Op::UitoFp, Flags::NONE, to, a),
                (Kind::Float, Kind::Int { signed: true }) => s.b.convert(Op::FptoSi, unchecked, to, a),
                (Kind::Float, Kind::Int { signed: false }) => s.b.convert(Op::FptoUi, unchecked, to, a),
                _ => return Err(shape("`T` and `U` must be vectors of integers or floats")),
            };
            out_vector(s, &m, r)
        }
        // "Returns a new vector such that element `i` is selected from `xy[idx[i]]`, where `xy`
        // is the concatenation of `x` and `y`." "`V` must be a vector with the same element
        // type as `T` and the same length as `U`."
        SimdOp::Shuffle => {
            let l = lanes(s, ty(0)?)?;
            let r = lanes(s, ty(2)?)?;
            if r.elem != l.elem {
                return Err(shape("`V` must be a vector with the same element type as `T`"));
            }
            let (vt, rt) = (vector_ty(which, &l)?, vector_ty(which, &r)?);
            let count = r.count as usize;
            let (x, y, idx) = (place(0)?, place(1)?, place(2)?);
            let mut mask = [0u32; MOST_LANES];
            shuffle_indices(s, idx, count, 2 * l.count as u32, &mut mask)?;
            let a = load_vector(s, &l, vt, x);
            let b = load_vector(s, &l, vt, y);
            let v = s.b.shuffle(rt, a, b, &mask[..count]);
            out_vector(s, &r, v)
        }
        // "`U` must be the element type of `T`." Ordered: "Starting with the value `y`, add the
        // elements of `x` and accumulate." Unordered: "in arbitrary order" (`reassoc`, starting
        // from the identity). An integer sum or product wraps, so its order is unseen.
        SimdOp::ReduceAddOrdered | SimdOp::ReduceAddUnordered | SimdOp::ReduceMulOrdered | SimdOp::ReduceMulUnordered => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            let lane = vt.lane();
            if s.scalar(c.ret)?.0 != lane {
                return Err(shape("`U` must be the element type of `T`"));
            }
            let x = place(0)?;
            let ordered = matches!(op, SimdOp::ReduceAddOrdered | SimdOp::ReduceMulOrdered);
            let mul = matches!(op, SimdOp::ReduceMulOrdered | SimdOp::ReduceMulUnordered);
            let a = load_vector(s, &l, vt, x);
            let r = match kind(l.elem) {
                Kind::Int { .. } => {
                    let all = s.b.convert(if mul { Op::ReduceMul } else { Op::ReduceAdd }, Flags::NONE, lane, a);
                    match ordered {
                        true => {
                            let start = scalar(1)?;
                            s.b.binary(if mul { Op::Mul } else { Op::Add }, Flags::NONE, start, all)
                        }
                        false => all,
                    }
                }
                Kind::Float => {
                    let lop = if mul { Op::ReduceFmul } else { Op::ReduceFadd };
                    match ordered {
                        true => {
                            let start = scalar(1)?;
                            s.b.reduce_ordered(lop, Flags::NONE, start, a)
                        }
                        false => {
                            let bits = float_identity(lane.elem(), mul).ok_or(shape(ints_or_floats))?;
                            let start = s.b.fconst(lane, bits);
                            s.b.reduce_ordered(lop, Flags::REASSOC, start, a)
                        }
                    }
                }
                Kind::Ptr => return Err(shape(ints_or_floats)),
            };
            Out::Scalar(r)
        }
        // "Returns the maximum element of a vector." / "the minimum": by the lanes' signedness;
        // a float vector (core_arch's `vmaxvq_f32`) as `fmax_num`/`fmin_num`.
        SimdOp::ReduceMax | SimdOp::ReduceMin => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            let lane = vt.lane();
            if s.scalar(c.ret)?.0 != lane {
                return Err(shape("`U` must be the element type of `T`"));
            }
            let lop = match (op, kind(l.elem)) {
                (SimdOp::ReduceMax, Kind::Int { signed: true }) => Op::ReduceSmax,
                (SimdOp::ReduceMax, Kind::Int { signed: false }) => Op::ReduceUmax,
                (SimdOp::ReduceMin, Kind::Int { signed: true }) => Op::ReduceSmin,
                (SimdOp::ReduceMin, Kind::Int { signed: false }) => Op::ReduceUmin,
                (SimdOp::ReduceMax, Kind::Float) => Op::ReduceFmax,
                (SimdOp::ReduceMin, Kind::Float) => Op::ReduceFmin,
                _ => return Err(shape(ints_or_floats)),
            };
            let x = place(0)?;
            let a = load_vector(s, &l, vt, x);
            Out::Scalar(s.b.convert(lop, Flags::NONE, lane, a))
        }
        // "Logical "and"s / "ors" / "exclusive ors" all elements together." "`T` must be a
        // vector of integers."
        SimdOp::ReduceAnd | SimdOp::ReduceOr | SimdOp::ReduceXor => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            let lane = vt.lane();
            if !matches!(kind(l.elem), Kind::Int { .. }) {
                return Err(shape(ints));
            }
            if s.scalar(c.ret)?.0 != lane {
                return Err(shape("`U` must be the element type of `T`"));
            }
            let lop = match op {
                SimdOp::ReduceAnd => Op::ReduceAnd,
                SimdOp::ReduceOr => Op::ReduceOr,
                _ => Op::ReduceXor,
            };
            let x = place(0)?;
            let a = load_vector(s, &l, vt, x);
            Out::Scalar(s.b.convert(lop, Flags::NONE, lane, a))
        }
        // "Checks if all mask values are true." / "if any mask value is true." "`x` must
        // contain only `0` or `!0`": every lane `!0` is an `and` of `!0`; any is an `or` not 0.
        SimdOp::ReduceAll | SimdOp::ReduceAny => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            let lane = vt.lane();
            if !matches!(kind(l.elem), Kind::Int { .. }) {
                return Err(shape(ints));
            }
            let (bool_ty, _) = s.scalar(c.ret)?;
            let x = place(0)?;
            let a = load_vector(s, &l, vt, x);
            let lop = if op == SimdOp::ReduceAll { Op::ReduceAnd } else { Op::ReduceOr };
            let r = s.b.convert(lop, Flags::NONE, lane, a);
            let zero = s.b.iconst(lane, 0);
            let bit = s.b.icmp(IntCond::Ne, r, zero);
            Out::Scalar(s.b.convert(Op::Zext, Flags::NONE, bool_ty, bit))
        }
        // "Each element is truncated to a single bit and packed into the result." "On little
        // endian, the least significant bit corresponds to the first vector element." `U` is
        // an unsigned integer or "the smallest array of `u8` with at least as many bits".
        // "`x` must contain only `0` and `!0`", so a lane's `!= 0` is its bit.
        SimdOp::Bitmask => {
            let l = lanes(s, ty(0)?)?;
            let vt = vector_ty(which, &l)?;
            if !matches!(kind(l.elem), Kind::Int { .. }) {
                return Err(shape("`T` must be an integer vector"));
            }
            let id = s.layout(c.ret)?;
            let (ret_scalar, ret_size, ret_align) = {
                let it = s.cx.layouts.get(id);
                (it.scalar, it.size, it.align)
            };
            let x = place(0)?;
            let a = load_vector(s, &l, vt, x);
            let bits = mask_bits(s, vt, a);
            match ret_scalar {
                Some(sc) => {
                    let (int, _) = lir(sc.prim);
                    if !matches!(sc.prim, Prim::Int { .. }) || u64::from(int.bits(64)) < l.count {
                        return Err(shape("`U` must be an unsigned integer with at least as many bits as the length of `T`"));
                    }
                    Out::Scalar(s.b.convert(Op::Bitmask, Flags::NONE, int, bits))
                }
                None => {
                    // `[u8; k]`: its k bytes are the integer's, little endian.
                    let int = match ret_size {
                        1 => Ty::I8,
                        2 => Ty::I16,
                        4 => Ty::I32,
                        8 => Ty::I64,
                        _ => {
                            return Err(IntrinsicError::Unsupported {
                                which,
                                why: "a bitmask into an array of `u8` of other than 1, 2, 4 or 8 bytes",
                            });
                        }
                    };
                    if ret_size * 8 < l.count {
                        return Err(shape("`U` must have at least as many bits as the length of `T`"));
                    }
                    let packed = s.b.convert(Op::Bitmask, Flags::NONE, int, bits);
                    let slot = s.b.slot(ret_size as u32, ret_align, SlotKind::Temp);
                    let at = s.b.slot_addr(slot);
                    s.b.store(Flags::NONE, packed, at, 0, MemAttrs::aligned(0));
                    Out::At(at)
                }
            }
        }
        // `simd_masked_load(mask: V, ptr: U, val: T)`: "For each element, if the corresponding
        // value in `mask` is `!0`, read the corresponding pointer offset from `ptr`. ...
        // Otherwise ... return the corresponding value from `val`."
        SimdOp::MaskedLoad => {
            let m = lanes(s, ty(0)?)?;
            let l = lanes(s, ty(2)?)?;
            if m.count != l.count || !matches!(kind(m.elem), Kind::Int { .. }) {
                return Err(shape("`V` must be a vector of integers with the same length as `T`"));
            }
            let (mt, vt) = (vector_ty(which, &m)?, vector_ty(which, &l)?);
            let (mask, ptr, val) = (place(0)?, scalar(1)?, place(2)?);
            let mv = load_vector(s, &m, mt, mask);
            let bits = mask_bits(s, mt, mv);
            let pass = load_vector(s, &l, vt, val);
            let r = s.b.masked_load(Op::MaskedLoad, vt, ptr, bits, pass, MemAttrs::aligned(0));
            out_vector(s, &l, r)
        }
        // `simd_masked_store(mask: V, ptr: U, val: T)`: "write the corresponding value in `val`
        // to the pointer offset from `ptr`. Otherwise ... do nothing."
        SimdOp::MaskedStore => {
            let m = lanes(s, ty(0)?)?;
            let l = lanes(s, ty(2)?)?;
            if m.count != l.count || !matches!(kind(m.elem), Kind::Int { .. }) {
                return Err(shape("`V` must be a vector of integers with the same length as `T`"));
            }
            let (mt, vt) = (vector_ty(which, &m)?, vector_ty(which, &l)?);
            let (mask, ptr, val) = (place(0)?, scalar(1)?, place(2)?);
            let mv = load_vector(s, &m, mt, mask);
            let bits = mask_bits(s, mt, mv);
            let v = load_vector(s, &l, vt, val);
            s.b.masked_store(Op::MaskedStore, v, ptr, bits, MemAttrs::aligned(0));
            Out::None
        }
        // `simd_gather(val: T, ptr: U, mask: V)` / `simd_scatter(val, ptr, mask)`: "`U` must be a
        // vector of pointers to the element type of `T`, with the same length as `T`"; each
        // pointer "readable/writable as if by `ptr::read`/`ptr::write` (e.g. aligned to the
        // element type)". Scatter: "The stores happen in left-to-right order."
        SimdOp::Gather | SimdOp::Scatter => {
            let l = lanes(s, ty(0)?)?;
            let p = lanes(s, ty(1)?)?;
            let m = lanes(s, ty(2)?)?;
            if p.count != l.count || p.elem != Prim::Ptr {
                return Err(shape("`U` must be a vector of pointers with the same length as `T`"));
            }
            if m.count != l.count || !matches!(kind(m.elem), Kind::Int { .. }) {
                return Err(shape("`V` must be a vector of integers with the same length as `T`"));
            }
            let (vt, pt, mt) = (vector_ty(which, &l)?, vector_ty(which, &p)?, vector_ty(which, &m)?);
            let (val, ptrs, mask) = (place(0)?, place(1)?, place(2)?);
            let v = load_vector(s, &l, vt, val);
            let addrs = load_vector(s, &p, pt, ptrs);
            let mv = load_vector(s, &m, mt, mask);
            let bits = mask_bits(s, mt, mv);
            let at = MemAttrs::aligned(l.elem.align());
            match op {
                SimdOp::Gather => {
                    let r = s.b.masked_load(Op::Gather, vt, addrs, bits, v, at);
                    out_vector(s, &l, r)
                }
                _ => {
                    s.b.masked_store(Op::Scatter, v, addrs, bits, at);
                    Out::None
                }
            }
        }
        // The lane-by-lane rows are built by intrinsics/simd.rs and never sent here.
        SimdOp::Add
        | SimdOp::Sub
        | SimdOp::Mul
        | SimdOp::Neg
        | SimdOp::And
        | SimdOp::Or
        | SimdOp::Xor
        | SimdOp::Eq
        | SimdOp::Ne
        | SimdOp::Lt
        | SimdOp::Le
        | SimdOp::Gt
        | SimdOp::Ge
        | SimdOp::Splat
        | SimdOp::Extract
        | SimdOp::Insert
        | SimdOp::Select
        | SimdOp::SelectBitmask => return Err(shape("a lane-by-lane `simd_*` row sent to the vector builder")),
    })
}

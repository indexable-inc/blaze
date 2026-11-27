//! 128-bit SIMD Loop Auto-Vectorizer with Adaptive 4x / 2x Unrolling (`64` or `32 B/iter`).
//!
//! Transforms countable unit-stride scalar loops into 4x-unrolled (or 2x-unrolled for larger
//! bodies) 128-bit vector loops (`<2 x i64>`, `<4 x i32>`, `<8 x i16>`, `<16 x i8>`,
//! `<2 x f64>`, `<4 x f32>`) with:
//!
//! - **Quad Independent 128-bit Accumulators** on associative reductions (`Add`, `Xor`, `Or`,
//!   `And`, `Smin`, `Smax`, `Umin`, `Umax`, `REASSOC` `Fadd`, and `REASSOC` `Fma`/`Fmuladd`),
//!   breaking loop-carried ALU latency chains and saturating all 4 NEON pipes on Apple Silicon.
//! - **Strict-IEEE Ordered FP Vectorization** (`!Flags::REASSOC` `Fadd` dot products & sums on
//!   `F32` -> `<4 x f32>` and `F64` -> `<2 x f64>`, 2x unrolled): widens contiguous loads and
//!   lane-wise FP ops (`Fmul`, etc.) to 128-bit vectors (`ldp q, q` + `fmul.4s`/`fmul.2d`), then
//!   extracts lanes `0..lanes` in exact left-to-right order into a scalar `Fadd` chain preserving
//!   exact operand order, rounding, signed zeros, and NaN payloads bit-for-bit.
//! - **Contiguous Adjacent 16-Byte Vector Loads & Stores** (`+0, +16, +32, +48`), fusing
//!   directly into TWO 32-byte `ldp q, q` / `stp q, q` pairs in AArch64 `TierName::Sota` and
//!   unrolled `movdqu`/`vmovdqu` pairs in x86-64 `TierName::Sota`.
//! - **FMA & Integer Min/Max Elementwise Ops**: supports `Op::Fma`/`Op::Fmuladd` (e.g. `saxpy`,
//!   `matmul` inner loops) and `Op::Smin`/`Smax`/`Umin`/`Umax` on `i8`/`i16`/`i32` (e.g. ReLU
//!   clamp), with loop-invariant scalars and outer-loop pointers hoisted into `vec_pre`.
//! - **Sound Runtime Aliasing Guard** on unit-stride map/transform store loops via exact
//!   wraparound-safe unsigned range distance (`d1 = dst - src; (d1 == 0) | (umin(d1, -d1) >= byte_len)`),
//!   falling back to the original scalar loop if pointers partially overlap.
//! - **Exact Scalar Epilogue**: remaining `< unroll * lanes` elements finish in the untouched
//!   scalar loop for bit-exact results across every length (`0, 1, 2, ...`).
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{NONE, each, filled, Item},
    value_info::ValueInfo,
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::BlockKind,
    verify,
};

/// Telemetry returned by [`auto_vectorize_loops`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct VectorizeStats {
    /// Number of countable scalar loops transformed into 2x-unrolled 128-bit SIMD loops.
    pub loops_vectorized: u32,
}

/// Output of [`auto_vectorize_loops`].
#[derive(Debug)]
pub struct VectorizeOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: VectorizeStats,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LoopShape {
    /// `header` checks `i < n` -> `body` -> `Jump(header)`, `header` -> `exit`.
    HeaderBody { header: u32, body: u32, exit: u32 },
    /// Single-block do-while: `header` executes body, computes `i_next = i + 1`, checks `i_next < n` -> `header` | `exit`.
    SingleBlock { header: u32, exit: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IvKind {
    /// Integer induction variable `i` (`Ty::I64` or `Ty::I32`), stepped by `Add(i, 1)`.
    Counted { iv_ty: Ty },
    /// Pointer induction variable `p: Ty::PTR`, stepped by `PtrAdd(p, elem_bytes)`, stopping at `plan.bound`.
    Pointer,
}

#[derive(Clone, Copy, Debug)]
enum PreTerm {
    /// `preheader` ends in `Op::Jump(header, pre_args)`.
    Jump,
    /// `preheader` ends in `Op::Br(cond, header(pre_args) | other_blk(other_args))`.
    Br {
        cond: u32,
        header_is_then: bool,
        other_blk: u32,
        other_args_start: usize,
        other_argc: usize,
    },
}

#[derive(Clone, Copy, Debug)]
struct SecPtrIv {
    /// Position of this secondary lockstep pointer IV in `header`'s block parameter list.
    param_pos: usize,
    /// The `header` block param instruction index (`Ty::PTR`).
    param_inst: u32,
    /// Initial pointer value passed from `preheader`.
    init_val: u32,
    /// Step instruction `PtrAdd(param_inst, elem_bytes)` in the body.
    step_inst: u32,
}

#[derive(Clone, Copy, Debug)]
enum ReductionInput {
    /// Standard binary reduction `acc_next = op(acc, elem)`.
    Elem(u32),
    /// Fused multiply-add reduction `acc_next = fma_op(a, b, acc)` (`Op::Fma` or `Op::Fmuladd`).
    Fma { fma_op: Op, a: u32, b: u32 },
    /// Conditional-count reduction `acc_next = acc + (icmp(cond, a, b) ? 1 : 0)`.
    /// Vectorized as `vacc_u = Sub(vacc_u, Select(vec_icmp(cond, a, b), splat(-1), splat(0)))`.
    CountCond { cond: IntCond, a: u32, b: u32 },
    /// Bit-population-count sum reduction `acc_next = acc + popcnt(src)` over contiguous integer
    /// loads (`I8`/`I16`/`I32`/`I64`). Vectorized as 4x-unrolled `<16 x i8>` chunked `Op::Popcnt`
    /// (up to 30 iterations = 1,920 bytes per chunk -> max `240 <= 255` per `u8` lane) with
    /// `<8 x i16>` byte-pair widening and `Op::ReduceAdd(Ty::I16)`.
    Popcnt { src: u32 },
    /// Strict-IEEE ordered floating-point addition reduction (`!Flags::REASSOC`):
    /// `acc_next = Fadd(acc, elem)` (`acc_is_lhs == true`) or `Fadd(elem, acc)` (`acc_is_lhs == false`).
    /// Vectorizes the lane-wise inputs (`Load`, `Fmul`, etc.) into 2x-unrolled `<4 x f32>` /
    /// `<2 x f64>`, then extracts lanes `0..lanes` in exact left-to-right order into scalar
    /// `Op::Fadd` preserving exact operand order and IEEE 754 bit-exact rounding.
    OrderedFadd { elem: u32, acc_is_lhs: bool },
}

#[derive(Clone, Copy, Debug)]
struct ReductionSpec {
    /// Position of this accumulator in `header`'s block parameter list.
    param_pos: usize,
    /// The `header` block param instruction index.
    param_inst: u32,
    /// Scalar accumulator type (usually `elem_ty`, or wider `I64`/`I32` for `CountCond`).
    acc_ty: Ty,
    /// Initial value passed from `preheader`.
    init_val: u32,
    /// Associative combine op (`Add`, `Xor`, `Or`, `And`, `Smin`, `Smax`, `Umin`, `Umax`, `Fadd`).
    op: Op,
    /// Flags on the reduction op (e.g. `Flags::REASSOC` for `Fadd` / `Fma`).
    flags: Flags,
    /// The instruction computing `acc_next` in the loop body.
    update_inst: u32,
    /// Element expression(s) fed into the reduction update.
    input: ReductionInput,
}

#[derive(Clone, Copy, Debug)]
struct UnitStrideMem {
    /// Original `Op::Load` or `Op::Store` instruction index.
    inst: u32,
    /// Loop-invariant base pointer value.
    base: u32,
    /// Optional loop-invariant element-index offset `row_off` (e.g. `r * cols` after 2D loop
    /// interchange!) for addresses of the form `base + (row_off + i) * elem_bytes`.
    row_idx_off: Option<u32>,
    /// Constant byte offset added to `base + (row_off + i) * elem_bytes`.
    const_off: i32,
    /// Stored value (for `Op::Store` only; `NONE` for `Op::Load`).
    store_val: u32,
    /// Original memory attributes index in `view.mems`.
    mem_attr: u32,
}

struct VecLoopPlan<A: Allocator> {
    preheader: u32,
    pre_term: PreTerm,
    shape: LoopShape,
    /// Primary induction variable kind (counted integer or running pointer).
    iv_kind: IvKind,
    /// Position of the primary induction variable in `header`'s block parameters.
    iv_pos: usize,
    /// Induction variable instruction (`header` param).
    iv_inst: u32,
    /// Initial value of the primary IV passed from `preheader`.
    iv_init: u32,
    /// Loop-invariant upper bound (`n` for counted IV, `end_ptr` for pointer IV).
    bound: u32,
    /// Secondary lockstep pointer IVs stepping by `elem_bytes` in sync with primary pointer IV.
    sec_ptr_ivs: Vec<SecPtrIv, A>,
    /// Scalar element type (`I8`, `I16`, `I32`, `I64`, `F32`, `F64`).
    elem_ty: Ty,
    /// 128-bit vector type (`<lanes x elem_ty>`).
    vec_ty: Ty,
    /// `log2(elem_bytes)` (`0, 1, 2, 3`).
    elem_shift: u32,
    /// Number of lanes per 128-bit vector (`16, 8, 4, 2`).
    lanes: u32,
    /// Original `preheader` jump arguments to `header`.
    pre_args: Vec<u32, A>,
    /// Reduction accumulators (0 for pure map/store loops, >= 1 for reduction loops).
    reductions: Vec<ReductionSpec, A>,
    /// Unit-stride loads in body order.
    loads: Vec<UnitStrideMem, A>,
    /// Optional single unit-stride store (for map/transform loops).
    store: Option<UnitStrideMem>,
    /// All pure vectorizable body instructions (in topological body order) needed to compute
    /// `elem_val`s / `store_val`, excluding `loads` and `reductions`.
    vec_exprs: Vec<u32, A>,
    /// Loop-invariant scalar values referenced by `vec_exprs` that must be splatted in `vec_pre`.
    inv_scalars: Vec<u32, A>,
    /// Original `SingleBlock` exit arguments (referencing `header` defs) for direct exit when tail is empty.
    single_exit_args: Vec<u32, A>,
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn at<T: Copy + Default>(s: &[T], i: usize) -> T {
    s.get(i).copied().unwrap_or_default()
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&c| c == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
const fn int_cond_word(c: IntCond) -> u32 {
    match c {
        IntCond::Eq => 0,
        IntCond::Ne => 1,
        IntCond::Ult => 2,
        IntCond::Ule => 3,
        IntCond::Ugt => 4,
        IntCond::Uge => 5,
        IntCond::Slt => 6,
        IntCond::Sle => 7,
        IntCond::Sgt => 8,
        IntCond::Sge => 9,
    }
}

#[inline]
const fn block_kind_byte(k: BlockKind) -> u8 {
    match k {
        BlockKind::Normal => 0,
        BlockKind::Cold => 1,
        BlockKind::Cleanup => 2,
        BlockKind::Catch => 3,
        BlockKind::Terminate => 4,
    }
}

#[inline]
fn const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = Ty(at(view.tys, inst));
    if !ty.is_scalar() || !ty.is_int() || ty.bits(64) > 64 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match view.op(inst) {
        Op::Iconst => iconst_value(at(view.data, inst), width),
        Op::IconstWide => {
            let idx = to_idx(at(view.data, inst));
            let words = view.extra.get(idx..idx + to_idx(ty.const_words()))?;
            wide_value(words, width)
        }
        _ => return None,
    };
    u64::try_from(v).ok()
}

#[inline]
fn supported_elem(ty: Ty, vec_bytes: u32) -> Option<(Ty, u32, u32)> {
    if !ty.is_scalar() || !matches!(vec_bytes, 16 | 32 | 64) {
        return None;
    }
    let elem_shift = if ty == Ty::I8 {
        0u32
    } else if ty == Ty::I16 {
        1
    } else if ty == Ty::I32 || ty == Ty::F32 {
        2
    } else if ty == Ty::I64 || ty == Ty::F64 {
        3
    } else {
        return None;
    };
    let lanes = vec_bytes >> elem_shift;
    Some((Ty::vector(ty.elem(), lanes)?, elem_shift, lanes))
}

#[inline]
const fn invert_int_cond(c: IntCond) -> IntCond {
    match c {
        IntCond::Eq => IntCond::Ne,
        IntCond::Ne => IntCond::Eq,
        IntCond::Ult => IntCond::Uge,
        IntCond::Ule => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ule,
        IntCond::Uge => IntCond::Ult,
        IntCond::Slt => IntCond::Sge,
        IntCond::Sle => IntCond::Sgt,
        IntCond::Sgt => IntCond::Sle,
        IntCond::Sge => IntCond::Slt,
    }
}

/// Unwraps up to 4 boolean negation/zext wrappers (`Not`, `Xor(_, 1)`, `Icmp(Eq|Ne, i1|zext(i1), 0|1)`).
fn unwrap_bool_cond(view: &View<'_>, mut cond_val: u32) -> (u32, bool) {
    let mut flipped = false;
    for _ in 0..4 {
        let ci = to_idx(cond_val);
        if ci >= view.len() {
            break;
        }
        match view.op(ci) {
            Op::Not => {
                cond_val = at(view.data, ci);
                flipped = !flipped;
            }
            Op::Xor => {
                let pos = to_idx(at(view.data, ci));
                let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
                if const_u64(view, to_idx(r)) == Some(1) {
                    cond_val = l;
                    flipped = !flipped;
                } else if const_u64(view, to_idx(l)) == Some(1) {
                    cond_val = r;
                    flipped = !flipped;
                } else {
                    break;
                }
            }
            Op::Icmp => {
                let cat = to_idx(at(view.data, ci));
                let Some(ic) = IntCond::from_word(at(view.extra, cat)) else {
                    break;
                };
                if ic != IntCond::Eq && ic != IntCond::Ne {
                    break;
                }
                let (lhs, rhs) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
                let (val, c) = if let Some(k) = const_u64(view, to_idx(rhs)).filter(|&k| k <= 1) {
                    (lhs, k)
                } else if let Some(k) = const_u64(view, to_idx(lhs)).filter(|&k| k <= 1) {
                    (rhs, k)
                } else {
                    break;
                };
                let vi = to_idx(val);
                let inner_i1 = if Ty(at(view.tys, vi)) == Ty::I1 {
                    val
                } else if view.op(vi) == Op::Zext {
                    let src = at(view.data, vi);
                    if Ty(at(view.tys, to_idx(src))) == Ty::I1 {
                        src
                    } else {
                        break;
                    }
                } else {
                    break;
                };
                let is_inv = (c == 0 && ic == IntCond::Eq) || (c == 1 && ic == IntCond::Ne);
                cond_val = inner_i1;
                if is_inv {
                    flipped = !flipped;
                }
            }
            Op::Trunc if Ty(at(view.tys, ci)) == Ty::I1 => {
                let src = at(view.data, ci);
                let si = to_idx(src);
                if si < view.len() && view.op(si) == Op::Zext {
                    let inner = at(view.data, si);
                    if to_idx(inner) < view.len() && Ty(at(view.tys, to_idx(inner))) == Ty::I1 {
                        cond_val = inner;
                    } else {
                        break;
                    }
                } else if si < view.len() && view.op(si) == Op::Select {
                    let sat = to_idx(at(view.data, si));
                    let (cv, tv, fv) = (
                        at(view.extra, sat),
                        at(view.extra, sat + 1),
                        at(view.extra, sat + 2),
                    );
                    let tc = const_u64(view, to_idx(tv));
                    let fc = const_u64(view, to_idx(fv));
                    if tc == Some(1) && fc == Some(0) {
                        cond_val = cv;
                    } else if tc == Some(0) && fc == Some(1) {
                        cond_val = cv;
                        flipped = !flipped;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    (cond_val, flipped)
}

#[inline]
fn unwrap_ptr_addr(view: &View<'_>, v: u32) -> u32 {
    let vi = to_idx(v);
    if view.op(vi) == Op::PtrAddr {
        at(view.data, vi)
    } else {
        v
    }
}

/// Checks whether `step_v` is `PtrAdd(ptr_iv, elem_bytes)` (with either `Flags::NONE` or `Flags::INBOUNDS`).
#[inline]
fn is_ptr_step(view: &View<'_>, step_v: u32, ptr_iv: u32, elem_bytes: u64) -> bool {
    let si = to_idx(step_v);
    if view.op(si) != Op::PtrAdd {
        return false;
    }
    let idx = to_idx(at(view.data, si));
    at(view.extra, idx) == ptr_iv && const_u64(view, to_idx(at(view.extra, idx + 1))) == Some(elem_bytes)
}

/// Returns `true` when `bound` is statically proven to differ from `iv_init` by a multiple of
/// `1 << elem_shift` bytes (e.g. `PtrAdd(iv_init, Shl(len, elem_shift))` or `Mul(len, elem_bytes)`),
/// so a pointer-IV `p != end` loop needs no runtime alignment guard.
fn is_stride_aligned_end(view: &View<'_>, iv_init: u32, bound: u32, elem_shift: u32) -> bool {
    if elem_shift == 0 {
        return true;
    }
    let elem_bytes = 1u64 << elem_shift;
    let bi = to_idx(bound);
    let off = match view.op(bi) {
        Op::PtrAdd => {
            let pos = to_idx(at(view.data, bi));
            if at(view.extra, pos) == iv_init {
                Some(at(view.extra, pos + 1))
            } else {
                None
            }
        }
        Op::Add => {
            let pos = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
            if view.op(to_idx(l)) == Op::PtrAddr && unwrap_ptr_addr(view, l) == iv_init {
                Some(r)
            } else if view.op(to_idx(r)) == Op::PtrAddr && unwrap_ptr_addr(view, r) == iv_init {
                Some(l)
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(off_v) = off else {
        return false;
    };
    let oi = to_idx(off_v);
    if let Some(c) = const_u64(view, oi) {
        return (c & (elem_bytes - 1)) == 0;
    }
    match view.op(oi) {
        Op::Shl => {
            let pos = to_idx(at(view.data, oi));
            let sh = at(view.extra, pos + 1);
            const_u64(view, to_idx(sh)).is_some_and(|k| k >= u64::from(elem_shift) && k < 64)
        }
        Op::Mul => {
            let pos = to_idx(at(view.data, oi));
            let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
            const_u64(view, to_idx(l)).is_some_and(|c| (c & (elem_bytes - 1)) == 0)
                || const_u64(view, to_idx(r)).is_some_and(|c| (c & (elem_bytes - 1)) == 0)
        }
        _ => false,
    }
}

/// When `bound` is `PtrAdd[INBOUNDS](iv_init, byte_off)` with an `I64` offset, returns `Some(byte_off)`.
/// Because `INBOUNDS` guarantees both `iv_init` and `bound` lie within the same allocation (no
/// address wraparound), `iv_init < bound` implies `byte_off == addr(bound) - addr(iv_init)` with
/// zero `Op::PtrAddr` needed.
fn inbounds_ptr_byte_offset(view: &View<'_>, iv_init: u32, bound: u32) -> Option<u32> {
    let bi = to_idx(bound);
    if view.op(bi) != Op::PtrAdd || !Flags(at(view.flags, bi)).contains(Flags::INBOUNDS) {
        return None;
    }
    let pos = to_idx(at(view.data, bi));
    if at(view.extra, pos) != iv_init {
        return None;
    }
    let off = at(view.extra, pos + 1);
    if Ty(at(view.tys, to_idx(off))) == Ty::I64 {
        Some(off)
    } else {
        None
    }
}

/// Adaptive unroll factor: 2x (32 bytes = 1 `stp q, q` / iter) for pure fill store loops
/// (`loads_len == 0`, so 32-byte fills vectorize with zero scalar tail and full store bandwidth);
/// 4x (64 bytes = 1 cache line / iter) for small/medium load-bearing bodies; 2x (32 bytes / iter)
/// for larger bodies to keep vector register pressure 0-spill.
#[inline]
const fn vec_unroll_factor(loads_len: usize, reductions_len: usize, vec_exprs_len: usize) -> usize {
    if loads_len > 0 && loads_len <= 2 && reductions_len <= 2 && vec_exprs_len <= 8 {
        4
    } else {
        2
    }
}

/// Returns the `I64` byte-length operand `byte_off` when `bound` is `PtrAdd(iv_init, byte_off)`
/// or `Add(PtrAddr(iv_init), byte_off)` (commuted). Whenever `addr(iv_init) < addr(bound)`
/// holds unsigned, modular wraparound did not occur and `addr(bound) - addr(iv_init) == byte_off`.
fn ptr_range_byte_len(view: &View<'_>, iv_init: u32, bound: u32) -> Option<u32> {
    let bi = to_idx(bound);
    let off = match view.op(bi) {
        Op::PtrAdd => {
            let pos = to_idx(at(view.data, bi));
            if at(view.extra, pos) == iv_init {
                Some(at(view.extra, pos + 1))
            } else {
                None
            }
        }
        Op::Add => {
            let pos = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
            if view.op(to_idx(l)) == Op::PtrAddr && unwrap_ptr_addr(view, l) == iv_init {
                Some(r)
            } else if view.op(to_idx(r)) == Op::PtrAddr && unwrap_ptr_addr(view, r) == iv_init {
                Some(l)
            } else {
                None
            }
        }
        _ => None,
    }?;
    if Ty(at(view.tys, to_idx(off))) == Ty::I64 {
        Some(off)
    } else {
        None
    }
}

/// Returns `true` when the loop's trip count (or byte span for `IvKind::Pointer`) is statically
/// provably strictly below the minimum vectorized chunk (`min_iters` / `min_bytes`), so emitting
/// a SIMD loop and runtime guard would only produce dead code.
fn trip_count_provably_below<A: Allocator>(
    view: &View<'_>,
    vi: &ValueInfo<A>,
    header: u32,
    iv_kind: IvKind,
    iv_init: u32,
    bound: u32,
    min_iters: u64,
    min_bytes: u64,
) -> bool {
    if min_iters == 0 || min_bytes == 0 {
        return false;
    }
    if iv_init == bound {
        return true;
    }
    let hdr = Block(header);
    match iv_kind {
        IvKind::Counted { .. } => {
            let bi = to_idx(bound);
            if bi < view.len() && view.op(bi) == Op::Add {
                let pos = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, pos), at(view.extra, pos + 1));
                let count_opt = if l == iv_init {
                    Some(r)
                } else if r == iv_init {
                    Some(l)
                } else {
                    None
                };
                if let Some(count) = count_opt
                    && vi.max_u64(hdr, to_idx(count)).is_some_and(|m| m < min_iters)
                {
                    return true;
                }
            }
            let Some(max_b) = vi.max_u64(hdr, bi) else {
                return false;
            };
            let min_init = u64::try_from(vi.range(hdr, to_idx(iv_init)).lo).unwrap_or(0);
            max_b.saturating_sub(min_init) < min_iters
        }
        IvKind::Pointer => {
            let Some(byte_off) = ptr_range_byte_len(view, iv_init, bound) else {
                return false;
            };
            vi.max_u64(hdr, to_idx(byte_off)).is_some_and(|m| m < min_bytes)
        }
    }
}

/// Matches a conditional-count reduction update on `acc` (`p_inst`) of type `acc_ty` (`I8..I64`)
/// over integer elements of type `elem_ty` (`elem_ty.bits <= acc_ty.bits`):
/// - `acc + Select(cond, 1, 0)` or `acc + Select(cond, 0, 1)`
/// - `acc + Zext(cond)` or `acc + Zext(Select(cond, 1, 0))`
/// - `acc - Sext(cond)` or `acc - Select(cond, -1, 0)` / `acc - Select(cond, 0, -1)`
/// - `Select(cond, acc + 1, acc)` or `Select(cond, acc, acc + 1)`
/// Returns `(effective_cond, cmp_a, cmp_b, optional_extra_acc_use_inst)`.
fn match_count_cond_update(
    view: &View<'_>,
    body_block: u32,
    inst_block: &[u32],
    p_inst: u32,
    back_v: u32,
    elem_ty: Ty,
) -> Option<(IntCond, u32, u32, Option<u32>)> {
    if elem_ty.is_float() || !elem_ty.is_int() {
        return None;
    }
    let bi = to_idx(back_v);
    let bop = view.op(bi);
    let (raw_cond, mut invert, extra_acc_use) = if bop == Op::Add {
        let bat = to_idx(at(view.data, bi));
        let (l, r) = (at(view.extra, bat), at(view.extra, bat + 1));
        let inc = if l == p_inst && r != p_inst {
            r
        } else if r == p_inst && l != p_inst {
            l
        } else {
            return None;
        };
        let mut cur = to_idx(inc);
        if at(inst_block, cur) != body_block {
            return None;
        }
        // Strip up to 3 `Op::Zext` wrappers (e.g. `Zext(i1 -> i8 -> i32 -> i64)` or `Zext(Select(i1, 1, 0) -> i64)`).
        for _ in 0..3 {
            if at(inst_block, cur) == body_block && view.op(cur) == Op::Zext {
                let src = at(view.data, cur);
                let src_i = to_idx(src);
                if Ty(at(view.tys, src_i)) == Ty::I1 {
                    cur = src_i;
                    break;
                }
                cur = src_i;
            } else {
                break;
            }
        }
        if at(inst_block, cur) != body_block {
            return None;
        }
        if Ty(at(view.tys, cur)) == Ty::I1 {
            (to_u32(cur), false, None)
        } else if view.op(cur) == Op::Select {
            let sat = to_idx(at(view.data, cur));
            let (cv, tv, fv) = (
                at(view.extra, sat),
                at(view.extra, sat + 1),
                at(view.extra, sat + 2),
            );
            let tc = const_u64(view, to_idx(tv));
            let fc = const_u64(view, to_idx(fv));
            if tc == Some(1) && fc == Some(0) {
                (cv, false, None)
            } else if tc == Some(0) && fc == Some(1) {
                (cv, true, None)
            } else {
                return None;
            }
        } else {
            return None;
        }
    } else if bop == Op::Sub {
        let sat = to_idx(at(view.data, bi));
        let (l, r) = (at(view.extra, sat), at(view.extra, sat + 1));
        if l != p_inst || r == p_inst {
            return None;
        }
        let ri = to_idx(r);
        if at(inst_block, ri) != body_block {
            return None;
        }
        if view.op(ri) == Op::Sext {
            let src = at(view.data, ri);
            if Ty(at(view.tys, to_idx(src))) != Ty::I1 {
                return None;
            }
            (src, false, None)
        } else if view.op(ri) == Op::Select {
            let acc_bits = Ty(at(view.tys, bi)).bits(64);
            let all_ones = if acc_bits >= 64 { u64::MAX } else { (1u64 << acc_bits) - 1 };
            let sel_at = to_idx(at(view.data, ri));
            let (cv, tv, fv) = (
                at(view.extra, sel_at),
                at(view.extra, sel_at + 1),
                at(view.extra, sel_at + 2),
            );
            let tc = const_u64(view, to_idx(tv));
            let fc = const_u64(view, to_idx(fv));
            if tc == Some(all_ones) && fc == Some(0) {
                (cv, false, None)
            } else if tc == Some(0) && fc == Some(all_ones) {
                (cv, true, None)
            } else {
                return None;
            }
        } else {
            return None;
        }
    } else if bop == Op::Select {
        let sat = to_idx(at(view.data, bi));
        let (cv, tv, fv) = (
            at(view.extra, sat),
            at(view.extra, sat + 1),
            at(view.extra, sat + 2),
        );
        let is_acc_plus_1 = |v: u32| -> bool {
            let vi = to_idx(v);
            if at(inst_block, vi) != body_block || view.op(vi) != Op::Add {
                return false;
            }
            let aat = to_idx(at(view.data, vi));
            let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
            (al == p_inst && const_u64(view, to_idx(ar)) == Some(1))
                || (ar == p_inst && const_u64(view, to_idx(al)) == Some(1))
        };
        if fv == p_inst && is_acc_plus_1(tv) {
            (cv, false, Some(tv))
        } else if tv == p_inst && is_acc_plus_1(fv) {
            (cv, true, Some(fv))
        } else {
            return None;
        }
    } else {
        return None;
    };

    let (inner_c, flipped) = unwrap_bool_cond(view, raw_cond);
    invert ^= flipped;
    let ci = to_idx(inner_c);
    if at(inst_block, ci) != body_block || view.op(ci) != Op::Icmp {
        return None;
    }
    let cat = to_idx(at(view.data, ci));
    let raw_ic = IntCond::from_word(at(view.extra, cat))?;
    let (a, b) = (at(view.extra, cat + 1), at(view.extra, cat + 2));
    if Ty(at(view.tys, to_idx(a))) != elem_ty || Ty(at(view.tys, to_idx(b))) != elem_ty {
        return None;
    }
    let eff_cond = if invert { invert_int_cond(raw_ic) } else { raw_ic };
    Some((eff_cond, a, b, extra_acc_use))
}

/// Matches a popcount-sum reduction update on `acc` (`p_inst`) of type `I32` or `I64` over
/// contiguous integer elements of type `elem_ty` (`I8`, `I16`, `I32`, `I64`):
/// - `acc + Popcnt(v)`
/// - `acc + Zext(Popcnt(v))` (e.g. `u8`/`u16`/`u32` `count_ones() as u64`)
/// - `acc + Zext(Trunc(Popcnt(v) -> I32) -> I64)` (Rust `u64::count_ones() as u64`)
/// - `acc + And(Popcnt(v), mask)` where `mask` preserves all bits of `0..=elem_bits` (e.g. `0xffff_ffff`)
/// - `acc + Trunc(Popcnt(v) -> I32)` (when `acc` is `I32` and `elem_ty` is `I64`)
/// - Optional `Zext` of `elem_ty` immediately inside `Popcnt(Zext(v))` (zero-extension adds 0 set bits).
/// Returns `Some(v)` where `v` has type `elem_ty` (verified later to be a unit-stride `Op::Load`).
fn match_popcnt_update(
    view: &View<'_>,
    body_block: u32,
    inst_block: &[u32],
    p_inst: u32,
    back_v: u32,
    elem_ty: Ty,
) -> Option<u32> {
    if elem_ty.is_float() || !elem_ty.is_int() {
        return None;
    }
    let elem_bits = elem_ty.bits(64);
    if !matches!(elem_bits, 8 | 16 | 32 | 64) {
        return None;
    }
    let bi = to_idx(back_v);
    if view.op(bi) != Op::Add {
        return None;
    }
    let bat = to_idx(at(view.data, bi));
    let (l, r) = (at(view.extra, bat), at(view.extra, bat + 1));
    let mut cur = if l == p_inst && r != p_inst {
        r
    } else if r == p_inst && l != p_inst {
        l
    } else {
        return None;
    };

    let need_mask = (u64::from(elem_bits) << 1) - 1;
    let mut eff_mask = u64::MAX;
    let mut min_width = Ty(at(view.tys, to_idx(cur))).bits(64);

    for _ in 0..5 {
        let ci = to_idx(cur);
        if ci >= view.len() || at(inst_block, ci) != body_block {
            return None;
        }
        match view.op(ci) {
            Op::Popcnt => {
                // Max popcount of an `elem_bits <= 64` integer is 64 (< 128), so any width >= 8
                // preserves all bits with sign bit 0 (making Trunc/Zext/Sext exact no-ops).
                if min_width < 8 || (eff_mask & need_mask) != need_mask {
                    return None;
                }
                let mut src = at(view.data, ci);
                for _ in 0..2 {
                    let si = to_idx(src);
                    if si >= view.len() {
                        return None;
                    }
                    if Ty(at(view.tys, si)) == elem_ty {
                        break;
                    }
                    if at(inst_block, si) == body_block && view.op(si) == Op::Zext {
                        src = at(view.data, si);
                    } else {
                        return None;
                    }
                }
                let fsi = to_idx(src);
                if fsi < view.len()
                    && at(inst_block, fsi) == body_block
                    && Ty(at(view.tys, fsi)) == elem_ty
                {
                    return Some(src);
                }
                return None;
            }
            Op::Zext | Op::Sext => {
                let inner = at(view.data, ci);
                let ii = to_idx(inner);
                if ii >= view.len() {
                    return None;
                }
                let in_bits = Ty(at(view.tys, ii)).bits(64);
                if in_bits < 8 {
                    return None;
                }
                min_width = min_width.min(in_bits);
                cur = inner;
            }
            Op::Trunc => {
                let cur_bits = Ty(at(view.tys, ci)).bits(64);
                if cur_bits < 8 {
                    return None;
                }
                min_width = min_width.min(cur_bits);
                cur = at(view.data, ci);
            }
            Op::And => {
                let aat = to_idx(at(view.data, ci));
                let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
                if let Some(m) = const_u64(view, to_idx(ar)) {
                    eff_mask &= m;
                    cur = al;
                } else if let Some(m) = const_u64(view, to_idx(al)) {
                    eff_mask &= m;
                    cur = ar;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

/// Checks whether `ptr_val` is a unit-stride address for a pointer-IV loop (`p + const_off` or
/// `PtrAdd(p, const_off)` where `p` is the primary or a secondary pointer IV).
fn match_ptr_iv_addr(
    view: &View<'_>,
    ptr_val: u32,
    extra_off: i32,
    iv_inst: u32,
    iv_init: u32,
    sec_ptr_ivs: &[SecPtrIv],
) -> Option<(u32, i32, Option<u32>)> {
    let mut cur = ptr_val;
    let mut total_off = extra_off;
    for _ in 0..2 {
        if cur == iv_inst {
            return Some((iv_init, total_off, None));
        }
        if let Some(sec) = sec_ptr_ivs.iter().find(|s| s.param_inst == cur) {
            return Some((sec.init_val, total_off, None));
        }
        let ci = to_idx(cur);
        if view.op(ci) == Op::PtrAdd {
            let pos = to_idx(at(view.data, ci));
            let (base, by) = (at(view.extra, pos), at(view.extra, pos + 1));
            if let Some(c) = const_u64(view, to_idx(by))
                && let Ok(c32) = i32::try_from(c.cast_signed())
                && let Some(sum) = total_off.checked_add(c32)
            {
                cur = base;
                total_off = sum;
                continue;
            }
        }
        break;
    }
    None
}

/// Checks whether `ptr_val` computes `base + (row_off + iv) * elem_bytes + delta` where `base`
/// (and optional `row_off`) are loop-invariant.
fn match_unit_stride_addr(
    view: &View<'_>,
    ptr_val: u32,
    extra_off: i32,
    iv_inst: u32,
    elem_shift: u32,
    is_loop_block: impl Fn(u32) -> bool,
    inst_block: &[u32],
) -> Option<(u32, i32, Option<u32>)> {
    let pi = to_idx(ptr_val);
    if view.op(pi) != Op::PtrAdd {
        return None;
    }
    let idx = to_idx(at(view.data, pi));
    let base = at(view.extra, idx);
    let by = at(view.extra, idx + 1);
    if is_loop_block(at(inst_block, to_idx(base))) {
        return None;
    }
    let is_iv = |v: u32| -> bool {
        let vi = to_idx(v);
        v == iv_inst || (view.op(vi) == Op::Zext && at(view.data, vi) == iv_inst)
    };
    let match_iv_plus_inv = |v: u32| -> Option<Option<u32>> {
        if is_iv(v) {
            return Some(None);
        }
        let vi = to_idx(v);
        if view.op(vi) == Op::Add && Ty(at(view.tys, vi)) == Ty::I64 {
            let aat = to_idx(at(view.data, vi));
            let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
            if is_iv(al) && !is_loop_block(at(inst_block, to_idx(ar))) && Ty(at(view.tys, to_idx(ar))) == Ty::I64 {
                return Some(Some(ar));
            }
            if is_iv(ar) && !is_loop_block(at(inst_block, to_idx(al))) && Ty(at(view.tys, to_idx(al))) == Ty::I64 {
                return Some(Some(al));
            }
        }
        None
    };
    let bi = to_idx(by);
    let elem_bytes = 1u64 << elem_shift;
    let row_off = if elem_shift == 0 {
        if let Some(ro) = match_iv_plus_inv(by) {
            Some(ro)
        } else if view.op(bi) == Op::Mul {
            let mat = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, mat), at(view.extra, mat + 1));
            if const_u64(view, to_idx(r)) == Some(1) {
                match_iv_plus_inv(l)
            } else if const_u64(view, to_idx(l)) == Some(1) {
                match_iv_plus_inv(r)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        match view.op(bi) {
            Op::Shl => {
                let sat = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, sat), at(view.extra, sat + 1));
                if const_u64(view, to_idx(r)) == Some(u64::from(elem_shift)) {
                    match_iv_plus_inv(l)
                } else {
                    None
                }
            }
            Op::Mul => {
                let mat = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, mat), at(view.extra, mat + 1));
                if const_u64(view, to_idx(r)) == Some(elem_bytes) {
                    match_iv_plus_inv(l)
                } else if const_u64(view, to_idx(l)) == Some(elem_bytes) {
                    match_iv_plus_inv(r)
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    row_off.map(|ro| (base, extra_off, ro))
}

/// Checks if an instruction is a pure 128-bit vectorizable op for `elem_ty` on AArch64 NEON
/// (`Mul` and `Smin`/`Smax`/`Umin`/`Umax` exclude `i64`, since NEON has no native 64-bit lanes
/// for those ops; `Fma`/`Fmuladd` lower to `fmla.4s` / `fmla.2d`).
#[inline]
fn is_vec_alu_op(op: Op, elem_ty: Ty) -> bool {
    if elem_ty.is_float() {
        matches!(op, Op::Fadd | Op::Fsub | Op::Fmul | Op::Fma | Op::Fmuladd)
    } else {
        match op {
            Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor => true,
            Op::Mul | Op::Smin | Op::Smax | Op::Umin | Op::Umax => {
                elem_ty == Ty::I8 || elem_ty == Ty::I16 || elem_ty == Ty::I32
            }
            _ => false,
        }
    }
}

/// Checks if `op` is an integer min/max op (`Smin`, `Smax`, `Umin`, `Umax`).
#[inline]
const fn is_minmax_op(op: Op) -> bool {
    matches!(op, Op::Smin | Op::Smax | Op::Umin | Op::Umax)
}

/// Checks if `op` is a supported associative binary reduction op for `elem_ty`.
#[inline]
fn is_reduction_op(op: Op, flags: Flags, elem_ty: Ty) -> bool {
    if elem_ty.is_float() {
        op == Op::Fadd && flags.contains(Flags::REASSOC)
    } else {
        match op {
            Op::Add | Op::Xor | Op::Or | Op::And => true,
            Op::Smin | Op::Smax | Op::Umin | Op::Umax => {
                elem_ty == Ty::I8 || elem_ty == Ty::I16 || elem_ty == Ty::I32
            }
            _ => false,
        }
    }
}

fn find_vectorizable_loop<A: Allocator + Copy>(
    view: &View<'_>,
    vec_bytes: u32,
    vi: &mut Option<ValueInfo<A>>,
    alloc: A,
) -> Option<VecLoopPlan<A>> {
    if view.head.ptr_bits != 64 {
        return None;
    }
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return None;
    }

    // Build block ownership & predecessor lists.
    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
        let term = view.terminator(Block(b_u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb)
                    && p.scan_for(&b_u32).is_none()
                {
                    p.push(b_u32);
                }
            }
        });
    }

    // Scan for a countable natural loop with a unique preheader ending in `Op::Jump`.
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        // Identify `(preheader, latch)` where `preheader < header <= latch`.
        let (preheader, latch) = if p0 < header && p1 >= header {
            (p0, p1)
        } else if p1 < header && p0 >= header {
            (p1, p0)
        } else {
            continue;
        };
        let pre_term = view.terminator(Block(preheader));
        let h_params = view.params(Block(header));
        if h_params.is_empty() {
            continue;
        }
        let (pre_term_kind, pre_args_start) = match view.op(pre_term) {
            Op::Jump => {
                let pre_at = to_idx(at(view.data, pre_term));
                if at(view.extra, pre_at) != header || to_idx(at(view.extra, pre_at + 1)) != h_params.len() {
                    continue;
                }
                (PreTerm::Jump, pre_at + 2)
            }
            Op::Br => {
                let pre_at = to_idx(at(view.data, pre_term));
                let pre_cv = at(view.extra, pre_at);
                let t_blk = at(view.extra, pre_at + 1);
                let t_argc = to_idx(at(view.extra, pre_at + 2));
                let e_at = pre_at + 3 + t_argc;
                let e_blk = at(view.extra, e_at);
                let e_argc = to_idx(at(view.extra, e_at + 1));
                if t_blk == header && e_blk != header && e_blk != preheader && t_argc == h_params.len() {
                    (
                        PreTerm::Br {
                            cond: pre_cv,
                            header_is_then: true,
                            other_blk: e_blk,
                            other_args_start: e_at + 2,
                            other_argc: e_argc,
                        },
                        pre_at + 3,
                    )
                } else if e_blk == header && t_blk != header && t_blk != preheader && e_argc == h_params.len() {
                    (
                        PreTerm::Br {
                            cond: pre_cv,
                            header_is_then: false,
                            other_blk: t_blk,
                            other_args_start: pre_at + 3,
                            other_argc: t_argc,
                        },
                        e_at + 2,
                    )
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let mut pre_args: Vec<u32, A> = Vec::with_capacity_in(h_params.len(), alloc);
        for k in 0..h_params.len() {
            pre_args.push(at(view.extra, pre_args_start + k));
        }

        // Determine loop shape (SingleBlock or HeaderBody), accepting either branch polarity.
        let (shape, body_block, back_args_start, cond_val, stay_is_then, single_exit_args) = if latch == header {
            // Single-block loop: `header` ends in `Br(cond, stay_arm, exit_arm)`.
            let h_term = view.terminator(Block(header));
            if view.op(h_term) != Op::Br {
                continue;
            }
            let idx = to_idx(at(view.data, h_term));
            let cv = at(view.extra, idx);
            let t_blk = at(view.extra, idx + 1);
            let t_argc = to_idx(at(view.extra, idx + 2));
            let e_at = idx + 3 + t_argc;
            let e_blk = at(view.extra, e_at);
            let e_argc = to_idx(at(view.extra, e_at + 1));
            if t_blk == header && e_blk != header && t_argc == h_params.len() {
                let mut ex_args = Vec::with_capacity_in(e_argc, alloc);
                for k in 0..e_argc {
                    ex_args.push(at(view.extra, e_at + 2 + k));
                }
                (LoopShape::SingleBlock { header, exit: e_blk }, header, idx + 3, cv, true, ex_args)
            } else if e_blk == header && t_blk != header && e_argc == h_params.len() {
                let mut ex_args = Vec::with_capacity_in(t_argc, alloc);
                for k in 0..t_argc {
                    ex_args.push(at(view.extra, idx + 3 + k));
                }
                (LoopShape::SingleBlock { header, exit: t_blk }, header, e_at + 2, cv, false, ex_args)
            } else {
                continue;
            }
        } else {
            // Two-block loop: `header` ends in `Br(cond, body_arm, exit_arm)`, `body` has 1 pred (`header`)
            // and ends in `Jump(header, back_args)`.
            if preds.get(to_idx(latch)).map(Vec::as_slice) != Some(&[header]) {
                continue;
            }
            if !view.params(Block(latch)).is_empty() {
                continue;
            }
            let h_term = view.terminator(Block(header));
            let b_term = view.terminator(Block(latch));
            if view.op(h_term) != Op::Br || view.op(b_term) != Op::Jump {
                continue;
            }
            let hat = to_idx(at(view.data, h_term));
            let cv = at(view.extra, hat);
            let t_blk = at(view.extra, hat + 1);
            let t_argc = to_idx(at(view.extra, hat + 2));
            let e_at = hat + 3 + t_argc;
            let e_blk = at(view.extra, e_at);
            let e_argc = to_idx(at(view.extra, e_at + 1));
            let bat = to_idx(at(view.data, b_term));
            let (exit_blk, stay_then) = if t_blk == latch && t_argc == 0 && e_blk != header && e_blk != latch {
                (e_blk, true)
            } else if e_blk == latch && e_argc == 0 && t_blk != header && t_blk != latch {
                (t_blk, false)
            } else {
                continue;
            };
            if at(view.extra, bat) != header || to_idx(at(view.extra, bat + 1)) != h_params.len() {
                continue;
            }
            // Header must only compute pure compare/ptr_addr/bool helpers before `Br`.
            let h_range = view.block(Block(header));
            let mut header_simple = true;
            for inst in h_params.end..(h_range.end - 1) {
                if inst != to_idx(cv)
                    && !matches!(
                        view.op(inst),
                        Op::Icmp
                            | Op::PtrAddr
                            | Op::Not
                            | Op::Xor
                            | Op::Zext
                            | Op::Trunc
                            | Op::Select
                            | Op::Iconst
                            | Op::IconstWide
                    )
                {
                    header_simple = false;
                    break;
                }
            }
            if !header_simple {
                continue;
            }
            (
                LoopShape::HeaderBody { header, body: latch, exit: exit_blk },
                latch,
                bat + 2,
                cv,
                stay_then,
                Vec::new_in(alloc),
            )
        };

        let is_loop_blk = |b: u32| b == header || b == body_block;
        let is_not_inv_blk = |b: u32| is_loop_blk(b) || b > preheader;

        // Ensure all preheader arguments (and branch operands for `PreTerm::Br`) are defined in
        // blocks dominating `preheader` in layout order.
        if !pre_args.iter().all(|&a| !is_not_inv_blk(at(&inst_block, to_idx(a)))) {
            continue;
        }
        if let PreTerm::Br { cond, other_args_start, other_argc, .. } = pre_term_kind {
            if is_not_inv_blk(at(&inst_block, to_idx(cond)))
                || (0..other_argc)
                    .any(|k| is_not_inv_blk(at(&inst_block, to_idx(at(view.extra, other_args_start + k)))))
            {
                continue;
            }
        }

        // Ensure no loop-body instruction escapes to outside blocks except via exit edge arguments.
        let mut used_outside = filled(len, false, &alloc);
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            if is_loop_blk(b_u32) {
                continue;
            }
            for inst in view.block(Block(b_u32)) {
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        set_at(&mut used_outside, to_idx(v), true);
                    }
                });
            }
        }
        let mut has_illegal_escape = false;
        for blk in [header, body_block] {
            for inst in view.block(Block(blk)) {
                if at(&used_outside, inst) {
                    // Only `HeaderBody` header params may be used directly in exit (`vec_reduce`
                    // always routes through `orig_header_new`, preserving dominance).
                    if !(matches!(shape, LoopShape::HeaderBody { .. })
                        && blk == header
                        && inst >= h_params.start
                        && inst < h_params.end)
                    {
                        has_illegal_escape = true;
                    }
                }
            }
        }
        if has_illegal_escape {
            continue;
        }

        // First body pass: determine common `elem_ty` from loads/stores and reject side effects.
        let body_range = view.block(Block(body_block));
        let body_start = if body_block == header { h_params.end } else { body_range.start };
        let body_end = body_range.end - 1; // exclude terminator

        let mut elem_ty: Option<Ty> = None;
        let mut vec_ty_info: Option<(Ty, u32, u32)> = None;
        let mut loads: Vec<UnitStrideMem, A> = Vec::new_in(alloc);
        let mut store: Option<UnitStrideMem> = None;
        let mut seen_store = false;
        let mut bad_body = false;

        for inst in body_start..body_end {
            let op = view.op(inst);
            let flags = Flags(at(view.flags, inst));
            match op {
                Op::Load => {
                    if flags.contains(Flags::VOLATILE) {
                        bad_body = true;
                        break;
                    }
                    let lt = Ty(at(view.tys, inst));
                    let Some(info) = supported_elem(lt, vec_bytes) else {
                        bad_body = true;
                        break;
                    };
                    if let Some(prev) = elem_ty {
                        if prev != lt {
                            bad_body = true;
                            break;
                        }
                    } else {
                        elem_ty = Some(lt);
                        vec_ty_info = Some(info);
                    }
                }
                Op::Store => {
                    if flags.contains(Flags::VOLATILE) || seen_store {
                        bad_body = true;
                        break;
                    }
                    seen_store = true;
                    let idx = to_idx(at(view.data, inst));
                    let st = Ty(at(view.tys, to_idx(at(view.extra, idx))));
                    let Some(info) = supported_elem(st, vec_bytes) else {
                        bad_body = true;
                        break;
                    };
                    if let Some(prev) = elem_ty {
                        if prev != st {
                            bad_body = true;
                            break;
                        }
                    } else {
                        elem_ty = Some(st);
                        vec_ty_info = Some(info);
                    }
                }
                Op::Call
                | Op::CallIndirect
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::Memcpy
                | Op::Memmove
                | Op::Memset
                | Op::Trap
                | Op::Udiv
                | Op::Sdiv
                | Op::Urem
                | Op::Srem => {
                    bad_body = true;
                    break;
                }
                _ => {}
            }
        }
        if bad_body {
            continue;
        }
        let (Some(elem_ty), Some((vec_ty, elem_shift, lanes))) = (elem_ty, vec_ty_info) else {
            continue;
        };
        let elem_bytes = 1u64 << elem_shift;

        // Parse exit condition (`Ult`/`Ugt`/`Uge`/`Ule`, plus `Ne`/`Eq` for pointer IV or 0-based counted IV).
        let (inner_cond, cond_flipped) = unwrap_bool_cond(view, cond_val);
        let stay_on_cmp = stay_is_then ^ cond_flipped;
        let ci = to_idx(inner_cond);
        if view.op(ci) != Op::Icmp {
            continue;
        }
        let cat = to_idx(at(view.data, ci));
        let Some(exit_ic) = IntCond::from_word(at(view.extra, cat)) else {
            continue;
        };
        let raw_lhs = at(view.extra, cat + 1);
        let raw_rhs = at(view.extra, cat + 2);

        // Identify the primary induction variable: either counted (`i += 1`) or pointer (`p += elem_bytes`).
        let mut found_iv: Option<(usize, u32, u32, u32, IvKind)> = None;
        for pos in 0..h_params.len() {
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let back_v = at(view.extra, back_args_start + pos);
            let back_i = to_idx(back_v);
            let p_ty = Ty(at(view.tys, p_idx));
            let cmp_target = match shape {
                LoopShape::HeaderBody { .. } => p_inst,
                LoopShape::SingleBlock { .. } => back_v,
            };

            if p_ty == Ty::I64 || p_ty == Ty::I32 {
                if view.op(back_i) != Op::Add {
                    continue;
                }
                let aat = to_idx(at(view.data, back_i));
                let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
                let is_step1 = (al == p_inst && const_u64(view, to_idx(ar)) == Some(1))
                    || (ar == p_inst && const_u64(view, to_idx(al)) == Some(1));
                if !is_step1 {
                    continue;
                }
                let start_is_zero = const_u64(view, to_idx(at(&pre_args, pos))) == Some(0);
                let bound_opt = if stay_on_cmp {
                    if raw_lhs == cmp_target && (exit_ic == IntCond::Ult || (start_is_zero && exit_ic == IntCond::Ne)) {
                        Some(raw_rhs)
                    } else if raw_rhs == cmp_target && (exit_ic == IntCond::Ugt || (start_is_zero && exit_ic == IntCond::Ne)) {
                        Some(raw_lhs)
                    } else {
                        None
                    }
                } else if raw_lhs == cmp_target && (exit_ic == IntCond::Uge || (start_is_zero && exit_ic == IntCond::Eq)) {
                    Some(raw_rhs)
                } else if raw_rhs == cmp_target && (exit_ic == IntCond::Ule || (start_is_zero && exit_ic == IntCond::Eq)) {
                    Some(raw_lhs)
                } else {
                    None
                };
                if let Some(bnd) = bound_opt
                    && (!is_not_inv_blk(at(&inst_block, to_idx(bnd))) || const_u64(view, to_idx(bnd)).is_some())
                    && Ty(at(view.tys, to_idx(bnd))) == p_ty
                {
                    found_iv = Some((pos, p_inst, back_v, bnd, IvKind::Counted { iv_ty: p_ty }));
                    break;
                }
            } else if p_ty == Ty::PTR {
                if !is_ptr_step(view, back_v, p_inst, elem_bytes) {
                    continue;
                }
                let lhs_is_addr = view.op(to_idx(raw_lhs)) == Op::PtrAddr;
                let rhs_is_addr = view.op(to_idx(raw_rhs)) == Op::PtrAddr;
                let u_lhs = unwrap_ptr_addr(view, raw_lhs);
                let u_rhs = unwrap_ptr_addr(view, raw_rhs);
                let end_opt = if stay_on_cmp {
                    if u_lhs == cmp_target && matches!(exit_ic, IntCond::Ult | IntCond::Ne) {
                        Some((u_rhs, lhs_is_addr))
                    } else if u_rhs == cmp_target && matches!(exit_ic, IntCond::Ugt | IntCond::Ne) {
                        Some((u_lhs, rhs_is_addr))
                    } else {
                        None
                    }
                } else if u_lhs == cmp_target && matches!(exit_ic, IntCond::Uge | IntCond::Eq) {
                    Some((u_rhs, lhs_is_addr))
                } else if u_rhs == cmp_target && matches!(exit_ic, IntCond::Ule | IntCond::Eq) {
                    Some((u_lhs, rhs_is_addr))
                } else {
                    None
                };
                if let Some((end_val, cmp_was_addr)) = end_opt
                    && !is_not_inv_blk(at(&inst_block, to_idx(end_val)))
                    && (Ty(at(view.tys, to_idx(end_val))) == Ty::PTR
                        || (cmp_was_addr && Ty(at(view.tys, to_idx(end_val))) == Ty::I64))
                {
                    found_iv = Some((pos, p_inst, back_v, end_val, IvKind::Pointer));
                    break;
                }
            }
        }
        let Some((iv_pos, iv_inst, iv_next_inst, bound, iv_kind)) = found_iv else {
            continue;
        };
        let iv_init = at(&pre_args, iv_pos);
        let iv_starts_zero_plan = match iv_kind {
            IvKind::Counted { .. } => const_u64(view, to_idx(iv_init)) == Some(0),
            IvKind::Pointer => true,
        };

        // Match every non-IV header parameter as either a secondary lockstep pointer IV (for
        // pointer-IV or 0-based counted zip loops), a conditional-count reduction (`CountCond`,
        // including widening `u32 -> u64`!), or an associative / strict-IEEE ordered reduction.
        let mut sec_ptr_ivs: Vec<SecPtrIv, A> = Vec::new_in(alloc);
        let mut reductions: Vec<ReductionSpec, A> = Vec::new_in(alloc);
        let mut is_reduction_update = filled(len, false, &alloc);
        let mut bad_params = false;
        for pos in 0..h_params.len() {
            if pos == iv_pos {
                continue;
            }
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let p_ty = Ty(at(view.tys, p_idx));
            let back_v = at(view.extra, back_args_start + pos);
            let bi = to_idx(back_v);
            if at(&inst_block, bi) != body_block {
                bad_params = true;
                break;
            }

            if (matches!(iv_kind, IvKind::Pointer) || iv_starts_zero_plan)
                && p_ty == Ty::PTR
                && is_ptr_step(view, back_v, p_inst, elem_bytes)
            {
                sec_ptr_ivs.push(SecPtrIv {
                    param_pos: pos,
                    param_inst: p_inst,
                    init_val: at(&pre_args, pos),
                    step_inst: back_v,
                });
                continue;
            }

            // Check conditional-count reduction first (both same-width and widening `i8/i16/i32 -> i32/i64`).
            let can_widen_count = !elem_ty.is_float()
                && p_ty.is_scalar()
                && p_ty.is_int()
                && p_ty.bits(64) <= 64
                && (p_ty == elem_ty
                    || (p_ty.bits(64) > elem_ty.bits(64)
                        && (matches!(elem_ty, Ty::I16 | Ty::I32) || (elem_ty == Ty::I8 && vec_bytes <= 32))));
            if can_widen_count
                && let Some((eff_cond, ca, cb, extra_acc_use)) =
                    match_count_cond_update(view, body_block, &inst_block, p_inst, back_v, elem_ty)
            {
                let extra_idx = extra_acc_use.map(to_idx);
                let mut other_use = false;
                for inst in body_start..=body_end {
                    if inst == bi || Some(inst) == extra_idx {
                        continue;
                    }
                    each(view, inst, |item| {
                        if let Item::Value(v) = item
                            && (v == p_inst || extra_acc_use == Some(v))
                        {
                            other_use = true;
                        }
                    });
                }
                if other_use {
                    bad_params = true;
                    break;
                }
                set_at(&mut is_reduction_update, bi, true);
                reductions.push(ReductionSpec {
                    param_pos: pos,
                    param_inst: p_inst,
                    acc_ty: p_ty,
                    init_val: at(&pre_args, pos),
                    op: Op::Add,
                    flags: Flags::NONE,
                    update_inst: back_v,
                    input: ReductionInput::CountCond { cond: eff_cond, a: ca, b: cb },
                });
                continue;
            }

            // Check contiguous byte-level `Op::Popcnt` sum reduction (`elem_ty ∈ {I8, I16, I32, I64}`,
            // `acc_ty ∈ {I32, I64}`, 16-byte SIMD `<16 x i8>` chunked path).
            if vec_bytes == 16
                && matches!(p_ty, Ty::I32 | Ty::I64)
                && let Some(pop_src) = match_popcnt_update(view, body_block, &inst_block, p_inst, back_v, elem_ty)
            {
                let mut other_use = false;
                for inst in body_start..=body_end {
                    if inst == bi {
                        continue;
                    }
                    each(view, inst, |item| {
                        if let Item::Value(v) = item
                            && v == p_inst
                        {
                            other_use = true;
                        }
                    });
                }
                if other_use {
                    bad_params = true;
                    break;
                }
                set_at(&mut is_reduction_update, bi, true);
                reductions.push(ReductionSpec {
                    param_pos: pos,
                    param_inst: p_inst,
                    acc_ty: p_ty,
                    init_val: at(&pre_args, pos),
                    op: Op::Add,
                    flags: Flags::NONE,
                    update_inst: back_v,
                    input: ReductionInput::Popcnt { src: pop_src },
                });
                continue;
            }

            if p_ty != elem_ty {
                bad_params = true;
                break;
            }
            let bop = view.op(bi);
            let bflags = Flags(at(view.flags, bi));
            let (red_op, red_input) = if elem_ty.is_float()
                && matches!(bop, Op::Fma | Op::Fmuladd)
                && bflags.contains(Flags::REASSOC)
            {
                let bat = to_idx(at(view.data, bi));
                let fa = at(view.extra, bat);
                let fb = at(view.extra, bat + 1);
                let fc = at(view.extra, bat + 2);
                if fc == p_inst && fa != p_inst && fb != p_inst {
                    (Op::Fadd, ReductionInput::Fma { fma_op: bop, a: fa, b: fb })
                } else {
                    bad_params = true;
                    break;
                }
            } else if is_reduction_op(bop, bflags, elem_ty) {
                let bat = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, bat), at(view.extra, bat + 1));
                let elem_val = if l == p_inst && r != p_inst {
                    r
                } else if r == p_inst && l != p_inst {
                    l
                } else {
                    bad_params = true;
                    break;
                };
                (bop, ReductionInput::Elem(elem_val))
            } else if elem_ty.is_float() && bop == Op::Fadd && !bflags.contains(Flags::REASSOC) {
                let bat = to_idx(at(view.data, bi));
                let (l, r) = (at(view.extra, bat), at(view.extra, bat + 1));
                if l == p_inst && r != p_inst {
                    (Op::Fadd, ReductionInput::OrderedFadd { elem: r, acc_is_lhs: true })
                } else if r == p_inst && l != p_inst {
                    (Op::Fadd, ReductionInput::OrderedFadd { elem: l, acc_is_lhs: false })
                } else {
                    bad_params = true;
                    break;
                }
            } else {
                bad_params = true;
                break;
            };
            // Ensure the accumulator param is ONLY used by its own reduction update!
            let mut other_use = false;
            for inst in body_start..body_end {
                if inst == bi {
                    continue;
                }
                each(view, inst, |item| {
                    if let Item::Value(v) = item
                        && v == p_inst
                    {
                        other_use = true;
                    }
                });
            }
            if other_use {
                bad_params = true;
                break;
            }
            set_at(&mut is_reduction_update, bi, true);
            reductions.push(ReductionSpec {
                param_pos: pos,
                param_inst: p_inst,
                acc_ty: p_ty,
                init_val: at(&pre_args, pos),
                op: red_op,
                flags: bflags,
                update_inst: back_v,
                input: red_input,
            });
        }
        if bad_params {
            continue;
        }

        // Match every load/store address as unit-stride (counted `base + (row_off + i) * elem_bytes`
        // or pointer-IV `p + const_off`).
        let mut is_mem_inst = filled(len, false, &alloc);
        for inst in body_start..body_end {
            let op = view.op(inst);
            if op == Op::Load {
                let idx = to_idx(at(view.data, inst));
                let ptr = at(view.extra, idx);
                let off = at(view.extra, idx + 1).cast_signed();
                let addr_match = match iv_kind {
                    IvKind::Counted { .. } => {
                        match_unit_stride_addr(view, ptr, off, iv_inst, elem_shift, is_not_inv_blk, &inst_block)
                            .or_else(|| {
                                if sec_ptr_ivs.is_empty() {
                                    None
                                } else {
                                    match_ptr_iv_addr(view, ptr, off, NONE, NONE, sec_ptr_ivs.as_slice())
                                }
                            })
                    }
                    IvKind::Pointer => {
                        match_ptr_iv_addr(view, ptr, off, iv_inst, iv_init, sec_ptr_ivs.as_slice())
                    }
                };
                let Some((base, const_off, row_idx_off)) = addr_match else {
                    bad_body = true;
                    break;
                };
                set_at(&mut is_mem_inst, inst, true);
                loads.push(UnitStrideMem {
                    inst: to_u32(inst),
                    base,
                    row_idx_off,
                    const_off,
                    store_val: NONE,
                    mem_attr: at(view.attrs, inst),
                });
            } else if op == Op::Store {
                if store.is_some() {
                    bad_body = true;
                    break;
                }
                let idx = to_idx(at(view.data, inst));
                let sv = at(view.extra, idx);
                let ptr = at(view.extra, idx + 1);
                let off = at(view.extra, idx + 2).cast_signed();
                let addr_match = match iv_kind {
                    IvKind::Counted { .. } => {
                        match_unit_stride_addr(view, ptr, off, iv_inst, elem_shift, is_not_inv_blk, &inst_block)
                            .or_else(|| {
                                if sec_ptr_ivs.is_empty() {
                                    None
                                } else {
                                    match_ptr_iv_addr(view, ptr, off, NONE, NONE, sec_ptr_ivs.as_slice())
                                }
                            })
                    }
                    IvKind::Pointer => {
                        match_ptr_iv_addr(view, ptr, off, iv_inst, iv_init, sec_ptr_ivs.as_slice())
                    }
                };
                let Some((base, const_off, row_idx_off)) = addr_match else {
                    bad_body = true;
                    break;
                };
                set_at(&mut is_mem_inst, inst, true);
                store = Some(UnitStrideMem {
                    inst: to_u32(inst),
                    base,
                    row_idx_off,
                    const_off,
                    store_val: sv,
                    mem_attr: at(view.attrs, inst),
                });
            }
        }
        if bad_body
            || (store.is_none() && (loads.is_empty() || reductions.is_empty()))
            || (store.is_some() && !reductions.is_empty())
        {
            continue;
        }
        // Only vectorize SingleBlock loops when all exit args are either outside values (<= preheader) or reduction updates.
        if matches!(shape, LoopShape::SingleBlock { .. }) {
            let mut exit_ok = true;
            for &ea in &single_exit_args {
                let ei = to_idx(ea);
                let eb = at(&inst_block, ei);
                if is_loop_blk(eb) {
                    if !at(&is_reduction_update, ei) {
                        exit_ok = false;
                        break;
                    }
                } else if eb > preheader {
                    exit_ok = false;
                    break;
                }
            }
            if !exit_ok {
                continue;
            }
        }

        // Walk backward from reduction inputs (or `store.store_val`) to collect `vec_exprs`
        // and verify every intermediate op is a supported vector ALU op of `elem_ty`!
        let has_popcnt = reductions
            .iter()
            .any(|r| matches!(r.input, ReductionInput::Popcnt { .. }));
        if has_popcnt && reductions.iter().any(|r| !matches!(r.input, ReductionInput::Popcnt { .. })) {
            continue;
        }
        let mut needed = filled(len, false, &alloc);
        let mut stack: Vec<u32, A> = Vec::new_in(alloc);
        let mut expr_bad = false;
        for red in &reductions {
            match red.input {
                ReductionInput::Elem(ev) | ReductionInput::OrderedFadd { elem: ev, .. } => stack.push(ev),
                ReductionInput::Fma { a, b, .. } | ReductionInput::CountCond { a, b, .. } => {
                    stack.push(a);
                    stack.push(b);
                }
                ReductionInput::Popcnt { src } => {
                    if !at(&is_mem_inst, to_idx(src)) {
                        expr_bad = true;
                        break;
                    }
                    stack.push(src);
                }
            }
        }
        if expr_bad {
            continue;
        }
        if let Some(st) = store {
            stack.push(st.store_val);
        }
        let mut inv_scalars: Vec<u32, A> = Vec::new_in(alloc);
        while let Some(v) = stack.pop() {
            let vi = to_idx(v);
            if at(&needed, vi) {
                continue;
            }
            let vb = at(&inst_block, vi);
            if !is_loop_blk(vb) {
                if vb > preheader || Ty(at(view.tys, vi)) != elem_ty {
                    expr_bad = true;
                    break;
                }
                if inv_scalars.scan_for(&v).is_none() {
                    inv_scalars.push(v);
                }
                continue;
            }
            if at(&is_mem_inst, vi) {
                set_at(&mut needed, vi, true);
                continue;
            }
            if v == iv_inst
                || v == iv_next_inst
                || sec_ptr_ivs.iter().any(|s| s.param_inst == v || s.step_inst == v)
            {
                // Direct use of IV in data expression (not just address): skip for now.
                expr_bad = true;
                break;
            }
            let op = view.op(vi);
            if matches!(op, Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide) {
                if Ty(at(view.tys, vi)) != elem_ty {
                    expr_bad = true;
                    break;
                }
                set_at(&mut needed, vi, true);
                continue;
            }
            if op == Op::Icmp {
                let idx = to_idx(at(view.data, vi));
                let (a_u32, bv_u32) = (at(view.extra, idx + 1), at(view.extra, idx + 2));
                if elem_ty.is_float()
                    || Ty(at(view.tys, to_idx(a_u32))) != elem_ty
                    || Ty(at(view.tys, to_idx(bv_u32))) != elem_ty
                {
                    expr_bad = true;
                    break;
                }
                set_at(&mut needed, vi, true);
                stack.push(a_u32);
                stack.push(bv_u32);
                continue;
            }
            if op == Op::Select {
                let idx = to_idx(at(view.data, vi));
                let (cv_u32, tv_u32, fv_u32) = (
                    at(view.extra, idx),
                    at(view.extra, idx + 1),
                    at(view.extra, idx + 2),
                );
                let cv = to_idx(cv_u32);
                if Ty(at(view.tys, vi)) != elem_ty
                    || Ty(at(view.tys, to_idx(tv_u32))) != elem_ty
                    || Ty(at(view.tys, to_idx(fv_u32))) != elem_ty
                    || !is_loop_blk(at(&inst_block, cv))
                    || view.op(cv) != Op::Icmp
                {
                    expr_bad = true;
                    break;
                }
                set_at(&mut needed, vi, true);
                stack.push(cv_u32);
                stack.push(tv_u32);
                stack.push(fv_u32);
                continue;
            }
            if matches!(op, Op::Shl | Op::Lshr) && matches!(elem_ty, Ty::I32 | Ty::I64) {
                let idx = to_idx(at(view.data, vi));
                let lhs = at(view.extra, idx);
                let rhs = at(view.extra, idx + 1);
                let max_sh = if elem_ty == Ty::I32 { 32 } else { 64 };
                if Ty(at(view.tys, vi)) == elem_ty
                    && Ty(at(view.tys, to_idx(lhs))) == elem_ty
                    && Ty(at(view.tys, to_idx(rhs))) == elem_ty
                    && const_u64(view, to_idx(rhs)).is_some_and(|k| k < max_sh)
                {
                    set_at(&mut needed, vi, true);
                    stack.push(lhs);
                    stack.push(rhs);
                    continue;
                }
                expr_bad = true;
                break;
            }
            if Ty(at(view.tys, vi)) != elem_ty || !is_vec_alu_op(op, elem_ty) {
                expr_bad = true;
                break;
            }
            set_at(&mut needed, vi, true);
            each(view, vi, |item| {
                if let Item::Value(opnd) = item {
                    stack.push(opnd);
                }
            });
        }
        if expr_bad {
            continue;
        }

        let mut vec_exprs: Vec<u32, A> = Vec::new_in(alloc);
        if body_block != header {
            let h_end = view.block(Block(header)).end - 1;
            for inst in h_params.end..h_end {
                if at(&needed, inst) && !at(&is_mem_inst, inst) {
                    vec_exprs.push(to_u32(inst));
                }
            }
        }
        for inst in body_start..body_end {
            if at(&needed, inst) && !at(&is_mem_inst, inst) {
                vec_exprs.push(to_u32(inst));
            }
        }

        // For map/store loops, check that any load sharing `(base, row_idx_off)` with the store
        // has the same constant offset (in-place `a[i] = f(a[i])` is safe; shifted `a[i] = a[i-1]` is not!).
        if let Some(st) = store {
            let mut alias_unsafe = false;
            for ld in &loads {
                if ld.base == st.base && ld.row_idx_off == st.row_idx_off && ld.const_off != st.const_off {
                    alias_unsafe = true;
                    break;
                }
            }
            if alias_unsafe {
                continue;
            }
        }

        // Skip vectorizing loops whose trip count (or pointer span) is statically provably below
        // one vectorized iteration (`unroll * lanes` elements / `unroll * vec_bytes` bytes).
        let unroll = vec_unroll_factor(loads.len(), reductions.len(), vec_exprs.len());
        let min_iters = u64::from(lanes) * u64::try_from(unroll).unwrap_or(2);
        let min_bytes = u64::from(vec_bytes) * u64::try_from(unroll).unwrap_or(2);
        let Ok(max_off) = i32::try_from(min_bytes) else {
            continue;
        };
        if let Some(st) = store
            && st.const_off.checked_add(max_off).is_none()
        {
            continue;
        }
        if loads.iter().any(|ld| ld.const_off.checked_add(max_off).is_none()) {
            continue;
        }
        let info = vi.get_or_insert_with(|| ValueInfo::analyze(*view, alloc));
        if trip_count_provably_below(
            view,
            info,
            header,
            iv_kind,
            iv_init,
            bound,
            min_iters,
            min_bytes,
        ) {
            continue;
        }

        return Some(VecLoopPlan {
            preheader,
            pre_term: pre_term_kind,
            shape,
            iv_kind,
            iv_pos,
            iv_inst,
            iv_init,
            bound,
            sec_ptr_ivs,
            elem_ty,
            vec_ty,
            elem_shift,
            lanes,
            pre_args,
            reductions,
            loads,
            store,
            vec_exprs,
            inv_scalars,
            single_exit_args,
        });
    }

    None
}

// Low-level LIR instruction emission helpers directly on `Func<A>`.
fn push_inst<A: Allocator>(
    func: &mut Func<A>,
    op: Op,
    flags: Flags,
    ty: Ty,
    data: u32,
    attr: u32,
) -> u32 {
    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(op));
    func.flags.push(flags.0);
    func.tys.push(ty.0);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(0);
    idx
}

fn push_iconst<A: Allocator>(func: &mut Func<A>, ty: Ty, val: u64) -> u32 {
    let bits = ty.bits(64);
    if bits < 32 {
        let masked = trunc_u32(val & ((1u64 << bits) - 1));
        push_inst(func, Op::Iconst, Flags::NONE, ty, masked, 0)
    } else if bits == 32 {
        push_inst(func, Op::Iconst, Flags::NONE, ty, trunc_u32(val), 0)
    } else if let Ok(v32) = i32::try_from(val.cast_signed()) {
        push_inst(func, Op::Iconst, Flags::NONE, ty, v32.cast_unsigned(), 0)
    } else {
        let idx = to_u32(func.extra.len());
        func.extra.push(trunc_u32(val));
        func.extra.push(trunc_u32(val >> 32));
        push_inst(func, Op::IconstWide, Flags::NONE, ty, idx, 0)
    }
}

fn push_fconst_zero<A: Allocator>(func: &mut Func<A>, ty: Ty) -> u32 {
    if ty == Ty::F32 {
        push_inst(func, Op::Fconst, Flags::NONE, Ty::F32, 0, 0)
    } else {
        let idx = to_u32(func.extra.len());
        func.extra.push(0);
        func.extra.push(0);
        push_inst(func, Op::FconstWide, Flags::NONE, Ty::F64, idx, 0)
    }
}

fn push_bin<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, op, flags, ty, idx, 0)
}

fn push_tern<A: Allocator>(func: &mut Func<A>, op: Op, flags: Flags, ty: Ty, a: u32, b: u32, c: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(a);
    func.extra.push(b);
    func.extra.push(c);
    push_inst(func, op, flags, ty, idx, 0)
}

fn push_extract<A: Allocator>(func: &mut Func<A>, elem_ty: Ty, vec_val: u32, lane: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(vec_val);
    func.extra.push(lane);
    push_inst(func, Op::Extract, Flags::NONE, elem_ty, idx, 0)
}

fn push_icmp<A: Allocator>(func: &mut Func<A>, cond: IntCond, a: u32, b: u32) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(int_cond_word(cond));
    func.extra.push(a);
    func.extra.push(b);
    push_inst(func, Op::Icmp, Flags::NONE, Ty::I1, idx, 0)
}

fn push_jump<A: Allocator>(func: &mut Func<A>, target: u32, args: &[u32]) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(target);
    func.extra.push(to_u32(args.len()));
    for &a in args {
        func.extra.push(a);
    }
    push_inst(func, Op::Jump, Flags::NONE, Ty::NONE, idx, 0)
}

fn push_br<A: Allocator>(func: &mut Func<A>, cond: u32, t_blk: u32, t_args: &[u32], e_blk: u32, e_args: &[u32]) -> u32 {
    let idx = to_u32(func.extra.len());
    func.extra.push(cond);
    func.extra.push(t_blk);
    func.extra.push(to_u32(t_args.len()));
    for &a in t_args {
        func.extra.push(a);
    }
    func.extra.push(e_blk);
    func.extra.push(to_u32(e_args.len()));
    for &a in e_args {
        func.extra.push(a);
    }
    push_inst(func, Op::Br, Flags::NONE, Ty::NONE, idx, 0)
}

fn copy_inst_remapped_blocks<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    blk_map: &[u32],
    func: &mut Func<A>,
) -> u32 {
    use crate::op::{Field, Layout};
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attr = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => at(val_map, to_idx(at(view.data, inst))),
        Layout::Data(Field::Block) => at(blk_map, to_idx(at(view.data, inst))),
        Layout::Data(_) => at(view.data, inst),
        Layout::Extra(fields) => {
            let src = to_idx(at(view.data, inst));
            let dst = to_u32(func.extra.len());
            let mut p = src;
            for &f in fields {
                match f {
                    Field::Value => {
                        func.extra.push(at(val_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Block => {
                        func.extra.push(at(blk_map, to_idx(at(view.extra, p))));
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Target => {
                        let b = to_idx(at(view.extra, p));
                        let n_u32 = at(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(at(blk_map, b));
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let scrutinee = at(view.extra, src);
                        let w = to_idx(Ty(at(view.tys, to_idx(scrutinee))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at(view.extra, p + k));
                            }
                            p += w;
                            let b = to_idx(at(view.extra, p));
                            let an_u32 = at(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(at(blk_map, b));
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                func.extra.push(at(val_map, to_idx(at(view.extra, p + k))));
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at(blk_map, to_idx(at(view.extra, p + k))));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(at(view.ops, inst));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

/// Returns `true` when `a` and `b` trace through `Op::PtrAdd` chains to provably non-aliasing
/// root objects: either distinct local `Op::SlotAddr`s, or distinct entry-block `Op::Param`s
/// where at least one carries `ParamFactsRec::NOALIAS` (Rust `&mut` / C `restrict`).
fn provably_noalias_bases(view: &View<'_>, mut a: u32, mut b: u32) -> bool {
    for _ in 0..8 {
        let ai = to_idx(a);
        if view.op(ai) == Op::PtrAdd {
            a = at(view.extra, to_idx(at(view.data, ai)));
        } else {
            break;
        }
    }
    for _ in 0..8 {
        let bi = to_idx(b);
        if view.op(bi) == Op::PtrAdd {
            b = at(view.extra, to_idx(at(view.data, bi)));
        } else {
            break;
        }
    }
    if a == b {
        return false;
    }
    let ai = to_idx(a);
    let bi = to_idx(b);
    if view.op(ai) == Op::SlotAddr && view.op(bi) == Op::SlotAddr {
        return at(view.data, ai) != at(view.data, bi);
    }
    let entry_params = view.params(Block(0));
    if view.op(ai) == Op::Param
        && view.op(bi) == Op::Param
        && ai >= entry_params.start
        && ai < entry_params.end
        && bi >= entry_params.start
        && bi < entry_params.end
    {
        let sig = view.sig(crate::func::SigId(view.head.sig));
        let pa = ai - entry_params.start;
        let pb = bi - entry_params.start;
        let has_noalias = |p_idx: usize| -> bool {
            sig.params
                .get(p_idx)
                .and_then(|p| view.facts(p))
                .is_some_and(|f| (f.bits & crate::func::ParamFactsRec::NOALIAS) != 0)
        };
        return has_noalias(pa) || has_noalias(pb);
    }
    false
}

struct PrefixScanPlan<A: Allocator> {
    preheader: u32,
    pre_term: PreTerm,
    shape: LoopShape,
    iv_kind: IvKind,
    iv_pos: usize,
    acc_pos: usize,
    iv_init: u32,
    acc_init: u32,
    bound: u32,
    elem_ty: Ty,
    elem_shift: u32,
    base: u32,
    row_idx_off: Option<u32>,
    const_off: i32,
    load_mem_attr: u32,
    store_mem_attr: u32,
    inclusive: bool,
    acc_update_inst: u32,
    pre_args: Vec<u32, A>,
    single_exit_args: Vec<u32, A>,
}

/// Matches a clean in-place wrapping-add prefix-scan loop (`I64` or `I32`, inclusive or
/// exclusive, `IvKind::Pointer` or `IvKind::Counted`, `HeaderBody` or `SingleBlock`,
/// `PreTerm::Jump` or `PreTerm::Br`) eligible for 8-way decoupled parallel prefix-scan
/// transformation (`1` loop-carried `add` per 8 elements).
fn find_prefix_scan_loop<A: Allocator + Copy>(
    view: &View<'_>,
    vi: &mut Option<ValueInfo<A>>,
    alloc: A,
) -> Option<PrefixScanPlan<A>> {
    if view.head.ptr_bits != 64 {
        return None;
    }
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return None;
    }

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
        let term = view.terminator(Block(b_u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb)
                    && p.scan_for(&b_u32).is_none()
                {
                    p.push(b_u32);
                }
            }
        });
    }

    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        let (preheader, latch) = if p0 < header && p1 >= header {
            (p0, p1)
        } else if p1 < header && p0 >= header {
            (p1, p0)
        } else {
            continue;
        };
        let h_params = view.params(Block(header));
        if h_params.len() != 2 {
            continue;
        }
        let pre_term = view.terminator(Block(preheader));
        let (pre_term_kind, pre_args_start) = match view.op(pre_term) {
            Op::Jump => {
                let pre_at = to_idx(at(view.data, pre_term));
                if at(view.extra, pre_at) != header || to_idx(at(view.extra, pre_at + 1)) != 2 {
                    continue;
                }
                (PreTerm::Jump, pre_at + 2)
            }
            Op::Br => {
                let pre_at = to_idx(at(view.data, pre_term));
                let pre_cv = at(view.extra, pre_at);
                let t_blk = at(view.extra, pre_at + 1);
                let t_argc = to_idx(at(view.extra, pre_at + 2));
                let e_at = pre_at + 3 + t_argc;
                let e_blk = at(view.extra, e_at);
                let e_argc = to_idx(at(view.extra, e_at + 1));
                if t_blk == header && e_blk != header && e_blk != preheader && t_argc == 2 {
                    (
                        PreTerm::Br {
                            cond: pre_cv,
                            header_is_then: true,
                            other_blk: e_blk,
                            other_args_start: e_at + 2,
                            other_argc: e_argc,
                        },
                        pre_at + 3,
                    )
                } else if e_blk == header && t_blk != header && t_blk != preheader && e_argc == 2 {
                    (
                        PreTerm::Br {
                            cond: pre_cv,
                            header_is_then: false,
                            other_blk: t_blk,
                            other_args_start: pre_at + 3,
                            other_argc: t_argc,
                        },
                        e_at + 2,
                    )
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let mut pre_args: Vec<u32, A> = Vec::with_capacity_in(2, alloc);
        pre_args.push(at(view.extra, pre_args_start));
        pre_args.push(at(view.extra, pre_args_start + 1));

        let (shape, body_block, back_args_start, cond_val, stay_is_then, single_exit_args) = if latch == header {
            let h_term = view.terminator(Block(header));
            if view.op(h_term) != Op::Br {
                continue;
            }
            let idx = to_idx(at(view.data, h_term));
            let cv = at(view.extra, idx);
            let t_blk = at(view.extra, idx + 1);
            let t_argc = to_idx(at(view.extra, idx + 2));
            let e_at = idx + 3 + t_argc;
            let e_blk = at(view.extra, e_at);
            let e_argc = to_idx(at(view.extra, e_at + 1));
            if t_blk == header && e_blk != header && t_argc == 2 {
                let mut ex_args = Vec::with_capacity_in(e_argc, alloc);
                for k in 0..e_argc {
                    ex_args.push(at(view.extra, e_at + 2 + k));
                }
                (LoopShape::SingleBlock { header, exit: e_blk }, header, idx + 3, cv, true, ex_args)
            } else if e_blk == header && t_blk != header && e_argc == 2 {
                let mut ex_args = Vec::with_capacity_in(t_argc, alloc);
                for k in 0..t_argc {
                    ex_args.push(at(view.extra, idx + 3 + k));
                }
                (LoopShape::SingleBlock { header, exit: t_blk }, header, e_at + 2, cv, false, ex_args)
            } else {
                continue;
            }
        } else {
            if preds.get(to_idx(latch)).map(Vec::as_slice) != Some(&[header]) {
                continue;
            }
            if !view.params(Block(latch)).is_empty() {
                continue;
            }
            let h_term = view.terminator(Block(header));
            let b_term = view.terminator(Block(latch));
            if view.op(h_term) != Op::Br || view.op(b_term) != Op::Jump {
                continue;
            }
            let hat = to_idx(at(view.data, h_term));
            let cv = at(view.extra, hat);
            let t_blk = at(view.extra, hat + 1);
            let t_argc = to_idx(at(view.extra, hat + 2));
            let e_at = hat + 3 + t_argc;
            let e_blk = at(view.extra, e_at);
            let e_argc = to_idx(at(view.extra, e_at + 1));
            let bat = to_idx(at(view.data, b_term));
            let (exit_blk, stay_then) = if t_blk == latch && t_argc == 0 && e_blk != header && e_blk != latch {
                (e_blk, true)
            } else if e_blk == latch && e_argc == 0 && t_blk != header && t_blk != latch {
                (t_blk, false)
            } else {
                continue;
            };
            if at(view.extra, bat) != header || to_idx(at(view.extra, bat + 1)) != 2 {
                continue;
            }
            let h_range = view.block(Block(header));
            let mut header_simple = true;
            for inst in h_params.end..(h_range.end - 1) {
                if inst != to_idx(cv)
                    && !matches!(
                        view.op(inst),
                        Op::Icmp
                            | Op::PtrAddr
                            | Op::Not
                            | Op::Xor
                            | Op::Zext
                            | Op::Trunc
                            | Op::Select
                            | Op::Iconst
                            | Op::IconstWide
                    )
                {
                    header_simple = false;
                    break;
                }
            }
            if !header_simple {
                continue;
            }
            (
                LoopShape::HeaderBody { header, body: latch, exit: exit_blk },
                latch,
                bat + 2,
                cv,
                stay_then,
                Vec::new_in(alloc),
            )
        };

        let is_loop_blk = |b: u32| b == header || b == body_block;
        let is_not_inv_blk = |b: u32| is_loop_blk(b) || b > preheader;

        if !pre_args.iter().all(|&a| !is_not_inv_blk(at(&inst_block, to_idx(a)))) {
            continue;
        }
        if let PreTerm::Br { cond, other_args_start, other_argc, .. } = pre_term_kind {
            if is_not_inv_blk(at(&inst_block, to_idx(cond)))
                || (0..other_argc)
                    .any(|k| is_not_inv_blk(at(&inst_block, to_idx(at(view.extra, other_args_start + k)))))
            {
                continue;
            }
        }

        // Verify no loop instruction escapes outside except `HeaderBody` header params.
        let mut used_outside = filled(len, false, &alloc);
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            if is_loop_blk(b_u32) {
                continue;
            }
            for inst in view.block(Block(b_u32)) {
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        set_at(&mut used_outside, to_idx(v), true);
                    }
                });
            }
        }
        let mut has_illegal_escape = false;
        for blk in [header, body_block] {
            for inst in view.block(Block(blk)) {
                if at(&used_outside, inst)
                    && !(matches!(shape, LoopShape::HeaderBody { .. })
                        && blk == header
                        && inst >= h_params.start
                        && inst < h_params.end)
                {
                    has_illegal_escape = true;
                }
            }
        }
        if has_illegal_escape {
            continue;
        }

        // Body must contain exactly 1 non-volatile `Load`, 1 non-volatile `Store` (in order
        // `load < store`), and only pure non-trapping integer/pointer/compare/const ops.
        let body_range = view.block(Block(body_block));
        let body_start = if body_block == header { h_params.end } else { body_range.start };
        let body_end = body_range.end - 1;
        let mut load_inst_opt: Option<u32> = None;
        let mut store_inst_opt: Option<u32> = None;
        let mut bad_body = false;
        for inst in body_start..body_end {
            let op = view.op(inst);
            let flags = Flags(at(view.flags, inst));
            match op {
                Op::Load => {
                    let lt = Ty(at(view.tys, inst));
                    if flags.contains(Flags::VOLATILE)
                        || load_inst_opt.is_some()
                        || !matches!(lt, Ty::I64 | Ty::I32)
                    {
                        bad_body = true;
                        break;
                    }
                    load_inst_opt = Some(to_u32(inst));
                }
                Op::Store => {
                    if flags.contains(Flags::VOLATILE) || store_inst_opt.is_some() {
                        bad_body = true;
                        break;
                    }
                    store_inst_opt = Some(to_u32(inst));
                }
                Op::Add
                | Op::PtrAdd
                | Op::PtrAddr
                | Op::Shl
                | Op::Mul
                | Op::Zext
                | Op::Trunc
                | Op::Icmp
                | Op::Not
                | Op::Xor
                | Op::Select
                | Op::Iconst
                | Op::IconstWide => {}
                _ => {
                    bad_body = true;
                    break;
                }
            }
        }
        let (Some(load_inst), Some(store_inst)) = (load_inst_opt, store_inst_opt) else {
            continue;
        };
        if bad_body || load_inst >= store_inst {
            continue;
        }
        let elem_ty = Ty(at(view.tys, to_idx(load_inst)));
        let elem_shift = if elem_ty == Ty::I32 { 2u32 } else { 3u32 };
        let elem_bytes = 1u64 << elem_shift;
        let st_idx = to_idx(at(view.data, to_idx(store_inst)));
        let store_val = at(view.extra, st_idx);
        let store_ptr = at(view.extra, st_idx + 1);
        let store_off = at(view.extra, st_idx + 2).cast_signed();
        if Ty(at(view.tys, to_idx(store_val))) != elem_ty {
            continue;
        }

        // Parse loop exit condition and identify the primary IV (`Counted` or `Pointer`).
        let (inner_cond, cond_flipped) = unwrap_bool_cond(view, cond_val);
        let stay_on_cmp = stay_is_then ^ cond_flipped;
        let ci = to_idx(inner_cond);
        if view.op(ci) != Op::Icmp {
            continue;
        }
        let cat = to_idx(at(view.data, ci));
        let Some(exit_ic) = IntCond::from_word(at(view.extra, cat)) else {
            continue;
        };
        let raw_lhs = at(view.extra, cat + 1);
        let raw_rhs = at(view.extra, cat + 2);

        let mut found_iv: Option<(usize, u32, u32, IvKind)> = None;
        for pos in 0..2 {
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            let back_v = at(view.extra, back_args_start + pos);
            let back_i = to_idx(back_v);
            let p_ty = Ty(at(view.tys, p_idx));
            let cmp_target = match shape {
                LoopShape::HeaderBody { .. } => p_inst,
                LoopShape::SingleBlock { .. } => back_v,
            };

            if p_ty == Ty::I64 || p_ty == Ty::I32 {
                if view.op(back_i) != Op::Add {
                    continue;
                }
                let aat = to_idx(at(view.data, back_i));
                let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
                let is_step1 = (al == p_inst && const_u64(view, to_idx(ar)) == Some(1))
                    || (ar == p_inst && const_u64(view, to_idx(al)) == Some(1));
                if !is_step1 {
                    continue;
                }
                let start_is_zero = const_u64(view, to_idx(at(&pre_args, pos))) == Some(0);
                let bound_opt = if stay_on_cmp {
                    if raw_lhs == cmp_target && (exit_ic == IntCond::Ult || (start_is_zero && exit_ic == IntCond::Ne)) {
                        Some(raw_rhs)
                    } else if raw_rhs == cmp_target
                        && (exit_ic == IntCond::Ugt || (start_is_zero && exit_ic == IntCond::Ne))
                    {
                        Some(raw_lhs)
                    } else {
                        None
                    }
                } else if raw_lhs == cmp_target
                    && (exit_ic == IntCond::Uge || (start_is_zero && exit_ic == IntCond::Eq))
                {
                    Some(raw_rhs)
                } else if raw_rhs == cmp_target
                    && (exit_ic == IntCond::Ule || (start_is_zero && exit_ic == IntCond::Eq))
                {
                    Some(raw_lhs)
                } else {
                    None
                };
                if let Some(bnd) = bound_opt
                    && (!is_not_inv_blk(at(&inst_block, to_idx(bnd))) || const_u64(view, to_idx(bnd)).is_some())
                    && Ty(at(view.tys, to_idx(bnd))) == p_ty
                {
                    found_iv = Some((pos, p_inst, bnd, IvKind::Counted { iv_ty: p_ty }));
                    break;
                }
            } else if p_ty == Ty::PTR {
                if !is_ptr_step(view, back_v, p_inst, elem_bytes) {
                    continue;
                }
                let lhs_is_addr = view.op(to_idx(raw_lhs)) == Op::PtrAddr;
                let rhs_is_addr = view.op(to_idx(raw_rhs)) == Op::PtrAddr;
                let u_lhs = unwrap_ptr_addr(view, raw_lhs);
                let u_rhs = unwrap_ptr_addr(view, raw_rhs);
                let end_opt = if stay_on_cmp {
                    if u_lhs == cmp_target && matches!(exit_ic, IntCond::Ult | IntCond::Ne) {
                        Some((u_rhs, lhs_is_addr))
                    } else if u_rhs == cmp_target && matches!(exit_ic, IntCond::Ugt | IntCond::Ne) {
                        Some((u_lhs, rhs_is_addr))
                    } else {
                        None
                    }
                } else if u_lhs == cmp_target && matches!(exit_ic, IntCond::Uge | IntCond::Eq) {
                    Some((u_rhs, lhs_is_addr))
                } else if u_rhs == cmp_target && matches!(exit_ic, IntCond::Ule | IntCond::Eq) {
                    Some((u_lhs, rhs_is_addr))
                } else {
                    None
                };
                if let Some((end_val, cmp_was_addr)) = end_opt
                    && !is_not_inv_blk(at(&inst_block, to_idx(end_val)))
                    && (Ty(at(view.tys, to_idx(end_val))) == Ty::PTR
                        || (cmp_was_addr && Ty(at(view.tys, to_idx(end_val))) == Ty::I64))
                {
                    found_iv = Some((pos, p_inst, end_val, IvKind::Pointer));
                    break;
                }
            }
        }
        let Some((iv_pos, iv_inst, bound, iv_kind)) = found_iv else {
            continue;
        };
        let iv_init = at(&pre_args, iv_pos);
        let acc_pos = 1 - iv_pos;
        let acc_param_idx = h_params.start + acc_pos;
        let acc_param = to_u32(acc_param_idx);
        if Ty(at(view.tys, acc_param_idx)) != elem_ty {
            continue;
        }
        let acc_init = at(&pre_args, acc_pos);
        let acc_update = at(view.extra, back_args_start + acc_pos);
        let acc_upd_i = to_idx(acc_update);
        if at(&inst_block, acc_upd_i) != body_block
            || view.op(acc_upd_i) != Op::Add
            || Flags(at(view.flags, acc_upd_i)) != Flags::NONE
            || Ty(at(view.tys, acc_upd_i)) != elem_ty
        {
            continue;
        }
        let aat = to_idx(at(view.data, acc_upd_i));
        let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
        if !((al == acc_param && ar == load_inst) || (al == load_inst && ar == acc_param)) {
            continue;
        }
        let inclusive = if store_val == acc_update {
            true
        } else if store_val == acc_param {
            false
        } else {
            continue;
        };

        // Verify strict use discipline for `load_inst`, `acc_param`, and `acc_update`.
        let store_i = to_idx(store_inst);
        let mut bad_uses = false;
        if matches!(shape, LoopShape::HeaderBody { .. }) {
            let h_range = view.block(Block(header));
            for inst in h_params.end..(h_range.end - 1) {
                each(view, inst, |item| {
                    if let Item::Value(v) = item
                        && (v == acc_param || v == load_inst || v == acc_update)
                    {
                        bad_uses = true;
                    }
                });
            }
        }
        for inst in body_start..=body_end {
            each(view, inst, |item| {
                if let Item::Value(v) = item {
                    if v == load_inst && inst != acc_upd_i {
                        bad_uses = true;
                    }
                    if v == acc_param && inst != acc_upd_i && !(!inclusive && inst == store_i) {
                        bad_uses = true;
                    }
                    if v == acc_update && inst != body_end && !(inclusive && inst == store_i) {
                        bad_uses = true;
                    }
                }
            });
        }
        if bad_uses {
            continue;
        }

        if matches!(shape, LoopShape::SingleBlock { .. }) {
            let mut exit_ok = true;
            for &ea in &single_exit_args {
                let eb = at(&inst_block, to_idx(ea));
                if is_loop_blk(eb) {
                    if ea != acc_update {
                        exit_ok = false;
                        break;
                    }
                } else if eb > preheader {
                    exit_ok = false;
                    break;
                }
            }
            if !exit_ok {
                continue;
            }
        }

        // Verify `Load` and `Store` address the exact same unit-stride location.
        let ld_idx = to_idx(at(view.data, to_idx(load_inst)));
        let load_ptr = at(view.extra, ld_idx);
        let load_off = at(view.extra, ld_idx + 1).cast_signed();
        let (ld_addr, st_addr) = match iv_kind {
            IvKind::Counted { .. } => (
                match_unit_stride_addr(view, load_ptr, load_off, iv_inst, elem_shift, is_not_inv_blk, &inst_block),
                match_unit_stride_addr(view, store_ptr, store_off, iv_inst, elem_shift, is_not_inv_blk, &inst_block),
            ),
            IvKind::Pointer => (
                match_ptr_iv_addr(view, load_ptr, load_off, iv_inst, iv_init, &[]),
                match_ptr_iv_addr(view, store_ptr, store_off, iv_inst, iv_init, &[]),
            ),
        };
        let (Some((ld_base, ld_const_off, ld_row_off)), Some((st_base, st_const_off, st_row_off))) =
            (ld_addr, st_addr)
        else {
            continue;
        };
        if ld_base != st_base || ld_const_off != st_const_off || ld_row_off != st_row_off {
            continue;
        }
        let Ok(max_delta) = i32::try_from(7 * elem_bytes) else {
            continue;
        };
        if ld_const_off.checked_add(max_delta).is_none() {
            continue;
        }
        let info = vi.get_or_insert_with(|| ValueInfo::analyze(*view, alloc));
        if trip_count_provably_below(
            view,
            info,
            header,
            iv_kind,
            iv_init,
            bound,
            8,
            8 * elem_bytes,
        ) {
            continue;
        }

        return Some(PrefixScanPlan {
            preheader,
            pre_term: pre_term_kind,
            shape,
            iv_kind,
            iv_pos,
            acc_pos,
            iv_init,
            acc_init,
            bound,
            elem_ty,
            elem_shift,
            base: ld_base,
            row_idx_off: ld_row_off,
            const_off: ld_const_off,
            load_mem_attr: at(view.attrs, to_idx(load_inst)),
            store_mem_attr: at(view.attrs, store_i),
            inclusive,
            acc_update_inst: acc_update,
            pre_args,
            single_exit_args,
        });
    }

    None
}

/// Emits an 8-way decoupled parallel prefix-scan fast loop ahead of the untouched scalar
/// remainder loop: a 3-level local prefix tree (`p01..p07`) executes completely independently
/// of `acc`, reducing the loop-carried dependency chain to 1 single `add` per 8 elements
/// (`acc_next = acc + p07`).
fn emit_prefix_scan_loop<A: Allocator + Copy>(
    view: &View<'_>,
    plan: PrefixScanPlan<A>,
    alloc: A,
) -> Result<VectorizeOutput<A>, SimplifyError> {
    const SCAN_K: u64 = 8;
    let blocks = view.blocks();
    let len = view.len();
    let elem_bytes = 1u64 << plan.elem_shift;
    let chunk_bytes = SCAN_K * elem_bytes;
    let elem_bytes_i32 = i32::try_from(elem_bytes).unwrap_or(8);
    let iv_starts_zero = match plan.iv_kind {
        IvKind::Counted { .. } => const_u64(view, to_idx(plan.iv_init)) == Some(0),
        IvKind::Pointer => true,
    };

    let insert_at = to_idx(plan.preheader) + 1;
    let has_guard_blk = matches!(plan.pre_term, PreTerm::Br { .. });
    let is_single = matches!(plan.shape, LoopShape::SingleBlock { .. });
    let guard_extra: usize = if has_guard_blk { 1 } else { 0 };
    let tail_extra: usize = if is_single { 1 } else { 0 };
    let extra_blocks = guard_extra + 2 + tail_extra;

    let scan_guard_blk = if has_guard_blk { to_u32(insert_at) } else { NONE };
    let scan_pre_blk = to_u32(insert_at + guard_extra);
    let scan_loop_blk = to_u32(insert_at + guard_extra + 1);
    let scan_tail_blk = if is_single { to_u32(insert_at + guard_extra + 2) } else { NONE };

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + extra_blocks) });
    }
    let orig_header_new = match plan.shape {
        LoopShape::HeaderBody { header, .. } | LoopShape::SingleBlock { header, .. } => {
            at(&blk_map, to_idx(header))
        }
    };
    let orig_exit_new = match plan.shape {
        LoopShape::HeaderBody { exit, .. } | LoopShape::SingleBlock { exit, .. } => {
            at(&blk_map, to_idx(exit))
        }
    };

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let term = range.end - 1;

        if b_u32 == plan.preheader {
            for inst in range.start..term {
                let mapped = copy_inst_remapped_blocks(view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            if let PreTerm::Br {
                cond,
                header_is_then,
                other_blk,
                other_args_start,
                other_argc,
            } = plan.pre_term
            {
                let cond_new = at(&val_map, to_idx(cond));
                let other_blk_new = at(&blk_map, to_idx(other_blk));
                let mut other_args_new: Vec<u32, A> = Vec::with_capacity_in(other_argc, alloc);
                for k in 0..other_argc {
                    other_args_new.push(at(&val_map, to_idx(at(view.extra, other_args_start + k))));
                }
                if header_is_then {
                    push_br(&mut func, cond_new, scan_guard_blk, &[], other_blk_new, &other_args_new);
                } else {
                    push_br(&mut func, cond_new, other_blk_new, &other_args_new, scan_guard_blk, &[]);
                }
                // --- Emit `scan_guard` trampoline block ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            }

            let iv_init_new = at(&val_map, to_idx(plan.iv_init));
            let acc_init_new = at(&val_map, to_idx(plan.acc_init));
            let bound_new = match const_u64(view, to_idx(plan.bound)) {
                Some(c) if at(&val_map, to_idx(plan.bound)) == NONE => {
                    push_iconst(&mut func, Ty(at(view.tys, to_idx(plan.bound))), c)
                }
                _ => at(&val_map, to_idx(plan.bound)),
            };
            let (rem_val, go_scan) = match plan.iv_kind {
                IvKind::Counted { iv_ty } => {
                    let k_step = push_iconst(&mut func, iv_ty, SCAN_K);
                    if iv_starts_zero {
                        let enough = push_icmp(&mut func, IntCond::Uge, bound_new, k_step);
                        (bound_new, enough)
                    } else {
                        let has_rem = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
                        let rem = push_bin(&mut func, Op::Sub, Flags::NONE, iv_ty, bound_new, iv_init_new);
                        let enough = push_icmp(&mut func, IntCond::Uge, rem, k_step);
                        let g = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);
                        (rem, g)
                    }
                }
                IvKind::Pointer => {
                    let (has_rem, byte_rem) =
                        if let Some(byte_off) = inbounds_ptr_byte_offset(view, plan.iv_init, plan.bound)
                            && at(&val_map, to_idx(byte_off)) != NONE
                        {
                            let off_new = at(&val_map, to_idx(byte_off));
                            let hr = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
                            (hr, off_new)
                        } else {
                            let p_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, iv_init_new, 0);
                            let end_addr = if Ty(at(view.tys, to_idx(plan.bound))) == Ty::PTR {
                                push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, bound_new, 0)
                            } else {
                                bound_new
                            };
                            let hr = push_icmp(&mut func, IntCond::Ult, p_addr, end_addr);
                            let br = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, end_addr, p_addr);
                            (hr, br)
                        };
                    let k_cbytes = push_iconst(&mut func, Ty::I64, chunk_bytes);
                    let enough = push_icmp(&mut func, IntCond::Uge, byte_rem, k_cbytes);
                    let mut g = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);
                    if !is_stride_aligned_end(view, plan.iv_init, plan.bound, plan.elem_shift) {
                        let k_emask = push_iconst(&mut func, Ty::I64, elem_bytes - 1);
                        let rem_misalign = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, byte_rem, k_emask);
                        let k_z64 = push_iconst(&mut func, Ty::I64, 0);
                        let aligned = push_icmp(&mut func, IntCond::Eq, rem_misalign, k_z64);
                        g = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, g, aligned);
                    }
                    (byte_rem, g)
                }
            };

            let mut fallback_args = [0u32; 2];
            set_at(&mut fallback_args, 0, at(&val_map, to_idx(at(&plan.pre_args, 0))));
            set_at(&mut fallback_args, 1, at(&val_map, to_idx(at(&plan.pre_args, 1))));
            push_br(&mut func, go_scan, scan_pre_blk, &[], orig_header_new, &fallback_args);

            // --- Emit `scan_pre` block ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let (init_ptr, scan_end, tail_iv_val) = match plan.iv_kind {
                IvKind::Pointer => {
                    let k_bmask = push_iconst(&mut func, Ty::I64, !(chunk_bytes - 1));
                    let scan_bytes = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem_val, k_bmask);
                    let s_end = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, iv_init_new, scan_bytes);
                    (iv_init_new, s_end, s_end)
                }
                IvKind::Counted { iv_ty } => {
                    let k_mask = push_iconst(&mut func, iv_ty, !(SCAN_K - 1));
                    let vec_count = push_bin(&mut func, Op::And, Flags::NONE, iv_ty, rem_val, k_mask);
                    let vec_end_i = if iv_starts_zero {
                        vec_count
                    } else {
                        push_bin(&mut func, Op::Add, Flags::NONE, iv_ty, iv_init_new, vec_count)
                    };
                    let base_new = at(&val_map, to_idx(plan.base));
                    let eff_base = if let Some(row_off) = plan.row_idx_off {
                        let row_v = at(&val_map, to_idx(row_off));
                        let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                        let row_bytes = push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, row_v, ks);
                        push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, base_new, row_bytes)
                    } else {
                        base_new
                    };
                    let start_ptr = if iv_starts_zero {
                        eff_base
                    } else {
                        let iv0_64 = if iv_ty == Ty::I32 {
                            push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv_init_new, 0)
                        } else {
                            iv_init_new
                        };
                        let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                        let iv0_bytes = push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, iv0_64, ks);
                        push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, eff_base, iv0_bytes)
                    };
                    let vc64 = if iv_ty == Ty::I32 {
                        push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, vec_count, 0)
                    } else {
                        vec_count
                    };
                    let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                    let scan_bytes = push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, vc64, ks);
                    let s_end = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, start_ptr, scan_bytes);
                    (start_ptr, s_end, vec_end_i)
                }
            };
            push_jump(&mut func, scan_loop_blk, &[init_ptr, acc_init_new]);

            // --- Emit `scan_loop` block (8-way decoupled parallel prefix scan, <= 11 live GPRs) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let p_param = push_inst(&mut func, Op::Param, Flags::NONE, Ty::PTR, 0, 0);
            let acc_param = push_inst(&mut func, Op::Param, Flags::NONE, plan.elem_ty, 0, 0);

            let emit_load_at = |func: &mut Func<A>, k: i32| -> u32 {
                let idx = to_u32(func.extra.len());
                func.extra.push(p_param);
                func.extra.push(plan.const_off.wrapping_add(k.wrapping_mul(elem_bytes_i32)).cast_unsigned());
                push_inst(func, Op::Load, Flags::NONE, plan.elem_ty, idx, plan.load_mem_attr)
            };
            let emit_store_at = |func: &mut Func<A>, k: i32, val: u32| {
                let idx = to_u32(func.extra.len());
                func.extra.push(val);
                func.extra.push(p_param);
                func.extra.push(plan.const_off.wrapping_add(k.wrapping_mul(elem_bytes_i32)).cast_unsigned());
                push_inst(func, Op::Store, Flags::NONE, Ty::NONE, idx, plan.store_mem_attr);
            };

            // 3-level local prefix tree (100% independent of `acc`, scheduled in two halves to
            // cap peak live registers at 11 GPRs while keeping adjacent load/store pairs for ldp/stp).
            let x0 = emit_load_at(&mut func, 0);
            let x1 = emit_load_at(&mut func, 1);
            let x2 = emit_load_at(&mut func, 2);
            let x3 = emit_load_at(&mut func, 3);
            let p01 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, x0, x1);
            let p23 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, x2, x3);
            let p02 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p01, x2);
            let p03 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p01, p23);

            let x4 = emit_load_at(&mut func, 4);
            let x5 = emit_load_at(&mut func, 5);
            let x6 = emit_load_at(&mut func, 6);
            let x7 = emit_load_at(&mut func, 7);
            let p45 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, x4, x5);
            let p67 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, x6, x7);
            let p46 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p45, x6);
            let p47 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p45, p67);
            let p04 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p03, x4);
            let p05 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p03, p45);
            let p06 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p03, p46);
            let p07 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, p03, p47);

            // Single loop-carried `add` per 8 elements (0.125 cycles/elem critical path).
            let acc_next = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p07);
            if plan.inclusive {
                let y0 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, x0);
                let y1 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p01);
                emit_store_at(&mut func, 0, y0);
                emit_store_at(&mut func, 1, y1);
                let y2 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p02);
                let y3 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p03);
                emit_store_at(&mut func, 2, y2);
                emit_store_at(&mut func, 3, y3);
                let y4 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p04);
                let y5 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p05);
                emit_store_at(&mut func, 4, y4);
                emit_store_at(&mut func, 5, y5);
                let y6 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p06);
                emit_store_at(&mut func, 6, y6);
                emit_store_at(&mut func, 7, acc_next);
            } else {
                let y1 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, x0);
                emit_store_at(&mut func, 0, acc_param);
                emit_store_at(&mut func, 1, y1);
                let y2 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p01);
                let y3 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p02);
                emit_store_at(&mut func, 2, y2);
                emit_store_at(&mut func, 3, y3);
                let y4 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p03);
                let y5 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p04);
                emit_store_at(&mut func, 4, y4);
                emit_store_at(&mut func, 5, y5);
                let y6 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p05);
                let y7 = push_bin(&mut func, Op::Add, Flags::NONE, plan.elem_ty, acc_param, p06);
                emit_store_at(&mut func, 6, y6);
                emit_store_at(&mut func, 7, y7);
            }

            let k_chunk = push_iconst(&mut func, Ty::I64, chunk_bytes);
            let p_next = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, p_param, k_chunk);
            let keep_scan = push_icmp(&mut func, IntCond::Ult, p_next, scan_end);

            match plan.shape {
                LoopShape::HeaderBody { .. } => {
                    let mut tail_args = [0u32; 2];
                    set_at(&mut tail_args, plan.iv_pos, tail_iv_val);
                    set_at(&mut tail_args, plan.acc_pos, acc_next);
                    push_br(&mut func, keep_scan, scan_loop_blk, &[p_next, acc_next], orig_header_new, &tail_args);
                }
                LoopShape::SingleBlock { .. } => {
                    push_br(&mut func, keep_scan, scan_loop_blk, &[p_next, acc_next], scan_tail_blk, &[acc_next]);
                    // --- Emit `scan_tail` block for `SingleBlock` do-while remainder ---
                    func.block_starts.push(to_u32(func.ops.len()));
                    func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                    let acc_final = push_inst(&mut func, Op::Param, Flags::NONE, plan.elem_ty, 0, 0);
                    let has_tail = if matches!(plan.iv_kind, IvKind::Pointer)
                        && Ty(at(view.tys, to_idx(plan.bound))) != Ty::PTR
                    {
                        let tail_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, tail_iv_val, 0);
                        push_icmp(&mut func, IntCond::Ult, tail_addr, bound_new)
                    } else {
                        push_icmp(&mut func, IntCond::Ult, tail_iv_val, bound_new)
                    };
                    let mut tail_args = [0u32; 2];
                    set_at(&mut tail_args, plan.iv_pos, tail_iv_val);
                    set_at(&mut tail_args, plan.acc_pos, acc_final);
                    let mut direct_exit_args: Vec<u32, A> =
                        Vec::with_capacity_in(plan.single_exit_args.len(), alloc);
                    for &ea in &plan.single_exit_args {
                        if ea == plan.acc_update_inst {
                            direct_exit_args.push(acc_final);
                        } else {
                            direct_exit_args.push(at(&val_map, to_idx(ea)));
                        }
                    }
                    push_br(&mut func, has_tail, orig_header_new, &tail_args, orig_exit_new, &direct_exit_args);
                }
            }
        } else {
            for inst in range {
                let mapped = copy_inst_remapped_blocks(view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(VectorizeOutput {
        func,
        stats: VectorizeStats { loops_vectorized: 1 },
    })
}

/// Automatically vectorizes eligible countable unit-stride scalar loops into 4x-unrolled (or
/// 2x-unrolled for larger bodies) 128-bit SIMD loops (`64` or `32 bytes / iteration`) with
/// independent accumulators and an exact scalar tail.
pub fn auto_vectorize_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<VectorizeOutput<A>, SimplifyError> {
    auto_vectorize_loops_with_width(view, 16, alloc)
}

/// Zero-allocation O(B) pre-filter: returns `true` iff `view` has at least one back-edge
/// `b -> tb` (`1 <= tb <= b`) targeting a `BlockKind::Normal` block with >= 1 block parameter,
/// which is a strict prerequisite for both `find_vectorizable_loop` and `find_prefix_scan_loop`.
fn has_vectorizable_loop_candidate(view: &View<'_>) -> bool {
    let blocks = view.blocks();
    if view.head.ptr_bits != 64 || blocks < 2 {
        return false;
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        let term = view.terminator(Block(b_u32));
        let mut found = false;
        each(view, term, |item| {
            if !found
                && let Item::Edge(edge) = item
                && edge.block >= 1
                && edge.block <= b_u32
                && to_idx(edge.block) < blocks
                && view.block_kind(Block(edge.block)) == BlockKind::Normal
                && !view.params(Block(edge.block)).is_empty()
            {
                found = true;
            }
        });
        if found {
            return true;
        }
    }
    false
}

/// Automatically vectorizes eligible countable unit-stride scalar loops into adaptive 4x/2x-unrolled
/// `vec_bytes`-wide SIMD loops (`16` = 128-bit NEON/SSE2 -> `64 B/iter` at 4x; `32` = 256-bit AVX2).
pub fn auto_vectorize_loops_with_width<A: Allocator + Copy>(
    view: View<'_>,
    vec_bytes: u32,
    alloc: A,
) -> Result<VectorizeOutput<A>, SimplifyError> {
    if !has_vectorizable_loop_candidate(&view) {
        return Ok(VectorizeOutput {
            func: view.to_func_in(alloc),
            stats: VectorizeStats::default(),
        });
    }
    let mut vi: Option<ValueInfo<A>> = None;
    let Some(plan) = find_vectorizable_loop(&view, vec_bytes, &mut vi, alloc) else {
        if let Some(scan_plan) = find_prefix_scan_loop(&view, &mut vi, alloc) {
            return emit_prefix_scan_loop(&view, scan_plan, alloc);
        }
        return Ok(VectorizeOutput {
            func: view.to_func_in(alloc),
            stats: VectorizeStats::default(),
        });
    };

    let blocks = view.blocks();
    let len = view.len();
    // Adaptive unroll factor: 2x (32 bytes = 1 `stp q, q` pair / iter) for pure store-fill loops
    // (`loads.is_empty()` so 32-byte fills vectorize with zero scalar tail); 4x (64 bytes = 1
    // cache line / iter, TWO `ldp q, q` / `stp q, q` pairs, 4 independent accumulators saturating
    // all 4 NEON pipes; also matches rustc's 4x unroll for small strict-IEEE ordered FP reductions)
    // for small/medium load-bearing bodies; 2x (32 bytes / iter, 1 `ldp q, q` pair) for larger
    // bodies to keep vector register pressure 0-spill.
    let unroll = vec_unroll_factor(plan.loads.len(), plan.reductions.len(), plan.vec_exprs.len());
    let unroll_step = u64::from(plan.lanes) * u64::try_from(unroll).unwrap_or(2);
    let step_bytes = u64::from(vec_bytes) * u64::try_from(unroll).unwrap_or(2);
    let iv_starts_zero = match plan.iv_kind {
        IvKind::Counted { .. } => const_u64(&view, to_idx(plan.iv_init)) == Some(0),
        IvKind::Pointer => true,
    };
    let has_count_cond = plan
        .reductions
        .iter()
        .any(|r| matches!(r.input, ReductionInput::CountCond { .. }));
    let has_popcnt = plan
        .reductions
        .iter()
        .any(|r| matches!(r.input, ReductionInput::Popcnt { .. }));
    let (loop_vec_ty, popcnt_wide_ty) = if has_popcnt {
        let (Some(v16i8), Some(v8i16)) = (
            Ty::vector(crate::ty::Scalar::I8, 16),
            Ty::vector(crate::ty::Scalar::I16, 8),
        ) else {
            return Ok(VectorizeOutput {
                func: view.to_func_in(alloc),
                stats: VectorizeStats::default(),
            });
        };
        (v16i8, v8i16)
    } else {
        (plan.vec_ty, Ty::NONE)
    };
    let needs_chunk = has_popcnt || plan.reductions.iter().any(|r| r.acc_ty != plan.elem_ty);
    let is_single = matches!(plan.shape, LoopShape::SingleBlock { .. });
    // For `<16 x i8>` byte-popcnt, cap each chunk at 30 inner iterations (`30 * 64 = 1,920` bytes
    // at 4x unroll) so every `u8` lane stays `<= 30 * 8 = 240 <= 255` (ZERO `u8` overflow), and
    // widening byte pairs into `<8 x i16>` sums to `<= 8 * 4 * 2 * 240 = 15,360 <= 65,535`!
    let chunk_elems: u64 = if has_popcnt {
        30u64.saturating_mul(unroll_step)
    } else if plan.elem_ty == Ty::I32 {
        1u64 << 28
    } else if plan.elem_ty == Ty::I16 {
        1u64 << 15
    } else if plan.elem_ty == Ty::I8 {
        (255u64 / unroll_step) * unroll_step
    } else {
        1u64 << 28
    };
    if needs_chunk && chunk_elems < unroll_step {
        return Ok(VectorizeOutput {
            func: view.to_func_in(alloc),
            stats: VectorizeStats::default(),
        });
    }

    // Insert 3 blocks (`vec_pre`, `vec_loop`, `vec_reduce`) for normal loops, or 4/5 blocks
    // (`vec_pre`, `vec_chunk`, `vec_loop`, `vec_reduce` [, `vec_tail`]) for overflow-free widening
    // reductions, plus an optional `vec_guard` trampoline block when `preheader` ends in `Op::Br`.
    let insert_at = to_idx(plan.preheader) + 1;
    let has_guard_blk = matches!(plan.pre_term, PreTerm::Br { .. });
    let guard_extra: usize = if has_guard_blk { 1 } else { 0 };
    let extra_blocks: usize = guard_extra
        + if needs_chunk {
            if is_single { 5 } else { 4 }
        } else {
            3
        };
    let vec_guard_blk = if has_guard_blk { to_u32(insert_at) } else { NONE };
    let vec_pre_blk = to_u32(insert_at + guard_extra);
    let (vec_chunk_blk, vec_loop_blk, vec_reduce_blk, vec_tail_blk) = if needs_chunk {
        (
            to_u32(insert_at + guard_extra + 1),
            to_u32(insert_at + guard_extra + 2),
            to_u32(insert_at + guard_extra + 3),
            if is_single { to_u32(insert_at + guard_extra + 4) } else { NONE },
        )
    } else {
        (NONE, to_u32(insert_at + guard_extra + 1), to_u32(insert_at + guard_extra + 2), NONE)
    };

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + extra_blocks) });
    }
    let orig_header_new = match plan.shape {
        LoopShape::HeaderBody { header, .. } | LoopShape::SingleBlock { header, .. } => {
            at(&blk_map, to_idx(header))
        }
    };
    let orig_exit_new = match plan.shape {
        LoopShape::HeaderBody { exit, .. } | LoopShape::SingleBlock { exit, .. } => {
            at(&blk_map, to_idx(exit))
        }
    };

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let elem_shift = plan.elem_shift;
    let emit_eff_base = |func: &mut Func<A>, base: u32, row_idx_off: Option<u32>, val_map: &[u32]| -> u32 {
        let b_new = at(val_map, to_idx(base));
        if let Some(row_off) = row_idx_off {
            let row_v = at(val_map, to_idx(row_off));
            let row_bytes = if elem_shift == 0 {
                row_v
            } else {
                let ks = push_iconst(func, Ty::I64, u64::from(elem_shift));
                push_bin(func, Op::Shl, Flags::NONE, Ty::I64, row_v, ks)
            };
            push_bin(func, Op::PtrAdd, Flags::NONE, Ty::PTR, b_new, row_bytes)
        } else {
            b_new
        }
    };

    let mut val_map = filled(len, NONE, &alloc);

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let term = range.end - 1;

        if b_u32 == plan.preheader {
            for inst in range.start..term {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            if let PreTerm::Br {
                cond,
                header_is_then,
                other_blk,
                other_args_start,
                other_argc,
            } = plan.pre_term
            {
                let cond_new = at(&val_map, to_idx(cond));
                let other_blk_new = at(&blk_map, to_idx(other_blk));
                let mut other_args_new: Vec<u32, A> = Vec::with_capacity_in(other_argc, alloc);
                for k in 0..other_argc {
                    other_args_new.push(at(&val_map, to_idx(at(view.extra, other_args_start + k))));
                }
                if header_is_then {
                    push_br(&mut func, cond_new, vec_guard_blk, &[], other_blk_new, &other_args_new);
                } else {
                    push_br(&mut func, cond_new, other_blk_new, &other_args_new, vec_guard_blk, &[]);
                }
                // --- Emit `vec_guard` trampoline block ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            }
            // Emit vectorization guard at end of `preheader` (or `vec_guard` when `preheader` ends in `Br`).
            let iv_init_new = at(&val_map, to_idx(plan.iv_init));
            let bound_new = match const_u64(&view, to_idx(plan.bound)) {
                Some(c) if at(&val_map, to_idx(plan.bound)) == NONE => {
                    push_iconst(&mut func, Ty(at(view.tys, to_idx(plan.bound))), c)
                }
                _ => at(&val_map, to_idx(plan.bound)),
            };
            let (rem_val, mut go_vec) = match plan.iv_kind {
                IvKind::Counted { iv_ty } => {
                    let k_step = push_iconst(&mut func, iv_ty, unroll_step);
                    if iv_starts_zero {
                        let enough = push_icmp(&mut func, IntCond::Uge, bound_new, k_step);
                        (bound_new, enough)
                    } else {
                        let has_rem = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
                        let rem = push_bin(&mut func, Op::Sub, Flags::NONE, iv_ty, bound_new, iv_init_new);
                        let enough = push_icmp(&mut func, IntCond::Uge, rem, k_step);
                        let g = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);
                        (rem, g)
                    }
                }
                IvKind::Pointer => {
                    let (has_rem, byte_rem) =
                        if let Some(byte_off) = inbounds_ptr_byte_offset(&view, plan.iv_init, plan.bound)
                            && at(&val_map, to_idx(byte_off)) != NONE
                        {
                            let off_new = at(&val_map, to_idx(byte_off));
                            let hr = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
                            (hr, off_new)
                        } else {
                            let p_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, iv_init_new, 0);
                            let end_addr = if Ty(at(view.tys, to_idx(plan.bound))) == Ty::PTR {
                                push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, bound_new, 0)
                            } else {
                                bound_new
                            };
                            let hr = push_icmp(&mut func, IntCond::Ult, p_addr, end_addr);
                            let br = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, end_addr, p_addr);
                            (hr, br)
                        };
                    let k_sbytes = push_iconst(&mut func, Ty::I64, step_bytes);
                    let enough = push_icmp(&mut func, IntCond::Uge, byte_rem, k_sbytes);
                    let mut g = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, enough);
                    if !is_stride_aligned_end(&view, plan.iv_init, plan.bound, plan.elem_shift) {
                        let k_emask = push_iconst(&mut func, Ty::I64, (1u64 << plan.elem_shift) - 1);
                        let rem_misalign = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, byte_rem, k_emask);
                        let k_z64 = push_iconst(&mut func, Ty::I64, 0);
                        let aligned = push_icmp(&mut func, IntCond::Eq, rem_misalign, k_z64);
                        g = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, g, aligned);
                    }
                    (byte_rem, g)
                }
            };

            // For map/store loops, emit compact wraparound-safe runtime non-overlap alias check:
            //   `d1 = dst - src; d2 = -d1; dist = umin(d1, d2); safe = (d1 == 0) | (dist >= byte_len)`
            // Note `(dst + start_bytes) - (src + start_bytes) == dst - src (mod 2^64)`, and `byte_len`
            // is hoisted out of outer loops (e.g. `matmul_f32`'s `k` loop) when `n` is invariant!
            let mut pre_load_eff_bases: Vec<u32, A> = Vec::new_in(alloc);
            let mut store_eff_base = NONE;
            if let Some(st) = plan.store {
                store_eff_base = emit_eff_base(&mut func, st.base, st.row_idx_off, &val_map);
                let needs_alias_check = plan.loads.iter().any(|ld| {
                    !(ld.base == st.base && ld.row_idx_off == st.row_idx_off && ld.const_off == st.const_off)
                        && !provably_noalias_bases(&view, st.base, ld.base)
                });
                let (dst_base_off, byte_len, zero64) = if needs_alias_check {
                    let blen = match plan.iv_kind {
                        IvKind::Pointer => rem_val,
                        IvKind::Counted { iv_ty } => {
                            let rem64 = if iv_ty == Ty::I32 {
                                push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, rem_val, 0)
                            } else {
                                rem_val
                            };
                            if plan.elem_shift == 0 {
                                rem64
                            } else {
                                let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                                push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, rem64, ks)
                            }
                        }
                    };
                    let dst_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, store_eff_base, 0);
                    let d_off = if st.const_off != 0 {
                        let k_off = push_iconst(&mut func, Ty::I64, i64::from(st.const_off).cast_unsigned());
                        push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, dst_addr, k_off)
                    } else {
                        dst_addr
                    };
                    let z64 = push_iconst(&mut func, Ty::I64, 0);
                    (d_off, blen, z64)
                } else {
                    (NONE, NONE, NONE)
                };
                for ld in &plan.loads {
                    let ld_eff = if ld.base == st.base && ld.row_idx_off == st.row_idx_off {
                        store_eff_base
                    } else {
                        emit_eff_base(&mut func, ld.base, ld.row_idx_off, &val_map)
                    };
                    pre_load_eff_bases.push(ld_eff);
                    if (ld.base == st.base && ld.row_idx_off == st.row_idx_off && ld.const_off == st.const_off)
                        || provably_noalias_bases(&view, st.base, ld.base)
                    {
                        continue; // in-place elementwise or provably noalias (Rust `&mut` / `NOALIAS`)!
                    }
                    let src_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, ld_eff, 0);
                    let src_base_off = if ld.const_off != 0 {
                        let kl = push_iconst(&mut func, Ty::I64, i64::from(ld.const_off).cast_unsigned());
                        push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, src_addr, kl)
                    } else {
                        src_addr
                    };
                    let d1 = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, dst_base_off, src_base_off);
                    let d2 = push_inst(&mut func, Op::Neg, Flags::NONE, Ty::I64, d1, 0);
                    let dist = push_bin(&mut func, Op::Umin, Flags::NONE, Ty::I64, d1, d2);
                    let eq_same = push_icmp(&mut func, IntCond::Eq, d1, zero64);
                    let disj = push_icmp(&mut func, IntCond::Uge, dist, byte_len);
                    let safe = push_bin(&mut func, Op::Or, Flags::NONE, Ty::I1, eq_same, disj);
                    go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, go_vec, safe);
                }
            }

            let mut fallback_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for &a in &plan.pre_args {
                fallback_args.push(at(&val_map, to_idx(a)));
            }
            push_br(&mut func, go_vec, vec_pre_blk, &[], orig_header_new, &fallback_args);

            // --- Emit `vec_pre` block ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let (vec_count, vec_end_counted, ptr_vec_bytes_total) = match plan.iv_kind {
                IvKind::Counted { iv_ty } => {
                    let mask_val = !(unroll_step - 1);
                    let k_mask = push_iconst(&mut func, iv_ty, mask_val);
                    let vc = push_bin(&mut func, Op::And, Flags::NONE, iv_ty, rem_val, k_mask);
                    let ve = if iv_starts_zero {
                        vc
                    } else {
                        push_bin(&mut func, Op::Add, Flags::NONE, iv_ty, iv_init_new, vc)
                    };
                    (vc, ve, NONE)
                }
                IvKind::Pointer => {
                    let k_bmask = push_iconst(&mut func, Ty::I64, !(step_bytes - 1));
                    let vb = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem_val, k_bmask);
                    (NONE, NONE, vb)
                }
            };

            // Build deduplicated running pointer streams `(base, row_idx_off)` across loads & store
            // so `vec_loop` uses pointer-induction strength reduction (`p_next = PtrAdd(p, step_bytes)`)
            // with ZERO `lsl` / `PtrAdd(base, i << shift)` / integer-IV instructions inside the loop!
            #[derive(Clone, Copy)]
            struct PtrStream {
                base: u32,
                row_idx_off: Option<u32>,
                init_ptr: u32,
                has_store: bool,
            }
            let has_mem_streams = !plan.loads.is_empty() || plan.store.is_some();
            let iv_init_bytes = match plan.iv_kind {
                IvKind::Pointer => NONE,
                IvKind::Counted { iv_ty } => {
                    if iv_starts_zero || !has_mem_streams {
                        NONE
                    } else {
                        let iv0_64 = if iv_ty == Ty::I32 {
                            push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, iv_init_new, 0)
                        } else {
                            iv_init_new
                        };
                        if plan.elem_shift == 0 {
                            iv0_64
                        } else {
                            let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                            push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, iv0_64, ks)
                        }
                    }
                }
            };
            let mut streams: Vec<PtrStream, A> = Vec::with_capacity_in(plan.loads.len() + 1, alloc);
            let mut load_stream_idx: Vec<usize, A> = Vec::with_capacity_in(plan.loads.len(), alloc);
            for (lidx, ld) in plan.loads.iter().enumerate() {
                if let Some(si) = streams.iter().position(|s| s.base == ld.base && s.row_idx_off == ld.row_idx_off) {
                    load_stream_idx.push(si);
                } else {
                    let eff_base = if plan.store.is_some() {
                        at(&pre_load_eff_bases, lidx)
                    } else {
                        emit_eff_base(&mut func, ld.base, ld.row_idx_off, &val_map)
                    };
                    let init_ptr = if iv_starts_zero {
                        eff_base
                    } else {
                        push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, eff_base, iv_init_bytes)
                    };
                    let si = streams.len();
                    streams.push(PtrStream {
                        base: ld.base,
                        row_idx_off: ld.row_idx_off,
                        init_ptr,
                        has_store: false,
                    });
                    load_stream_idx.push(si);
                }
            }
            let mut store_stream_idx: Option<usize> = None;
            if let Some(st) = plan.store {
                if let Some(si) = streams.iter().position(|s| s.base == st.base && s.row_idx_off == st.row_idx_off) {
                    if let Some(stream) = streams.get_mut(si) {
                        stream.has_store = true;
                    }
                    store_stream_idx = Some(si);
                } else {
                    let init_ptr = if iv_starts_zero {
                        store_eff_base
                    } else {
                        push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, store_eff_base, iv_init_bytes)
                    };
                    let si = streams.len();
                    streams.push(PtrStream {
                        base: st.base,
                        row_idx_off: st.row_idx_off,
                        init_ptr,
                        has_store: true,
                    });
                    store_stream_idx = Some(si);
                }
            }
            let use_ptr_iv = !streams.is_empty();
            let vec_bytes_total = match plan.iv_kind {
                IvKind::Pointer => ptr_vec_bytes_total,
                IvKind::Counted { iv_ty } => {
                    if use_ptr_iv {
                        let vec_count64 = if iv_ty == Ty::I32 {
                            push_inst(&mut func, Op::Zext, Flags::NONE, Ty::I64, vec_count, 0)
                        } else {
                            vec_count
                        };
                        if plan.elem_shift == 0 {
                            vec_count64
                        } else {
                            let ks = push_iconst(&mut func, Ty::I64, u64::from(plan.elem_shift));
                            push_bin(&mut func, Op::Shl, Flags::NONE, Ty::I64, vec_count64, ks)
                        }
                    } else {
                        NONE
                    }
                }
            };
            let p0_end = if let Some(s0) = streams.first() {
                push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, s0.init_ptr, vec_bytes_total)
            } else {
                NONE
            };
            let vec_end = match plan.iv_kind {
                IvKind::Counted { .. } => vec_end_counted,
                IvKind::Pointer => {
                    if streams.first().is_some_and(|s0| s0.init_ptr == iv_init_new) {
                        p0_end
                    } else {
                        push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, iv_init_new, vec_bytes_total)
                    }
                }
            };
            let mut sec_ptr_ends: Vec<u32, A> = Vec::with_capacity_in(plan.sec_ptr_ivs.len(), alloc);
            for sec in &plan.sec_ptr_ivs {
                let si_init = at(&val_map, to_idx(sec.init_val));
                let se = if streams.first().is_some_and(|s0| s0.init_ptr == si_init) {
                    p0_end
                } else {
                    push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, si_init, vec_bytes_total)
                };
                sec_ptr_ends.push(se);
            }

            let mut red_id_splats: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                if matches!(red.input, ReductionInput::OrderedFadd { .. }) {
                    // Strict-IEEE ordered reduction carries a scalar accumulator directly from `init_val`.
                    red_id_splats.push(at(&val_map, to_idx(red.init_val)));
                    continue;
                }
                if matches!(red.input, ReductionInput::Popcnt { .. }) {
                    let z8 = push_iconst(&mut func, Ty::I8, 0);
                    let sp = push_inst(&mut func, Op::Splat, Flags::NONE, loop_vec_ty, z8, 0);
                    red_id_splats.push(sp);
                    continue;
                }
                let id_scalar = if is_minmax_op(red.op) {
                    at(&val_map, to_idx(red.init_val))
                } else if plan.elem_ty.is_float() {
                    push_fconst_zero(&mut func, plan.elem_ty)
                } else if red.op == Op::And {
                    push_iconst(&mut func, plan.elem_ty, u64::MAX)
                } else {
                    push_iconst(&mut func, plan.elem_ty, 0)
                };
                let sp = push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, id_scalar, 0);
                red_id_splats.push(sp);
            }
            let (v_pop_mask_ff, v_pop_sh_8) = if has_popcnt {
                let k_ff = push_iconst(&mut func, Ty::I16, 0x00ff);
                let k_8 = push_iconst(&mut func, Ty::I16, 8);
                (
                    push_inst(&mut func, Op::Splat, Flags::NONE, popcnt_wide_ty, k_ff, 0),
                    push_inst(&mut func, Op::Splat, Flags::NONE, popcnt_wide_ty, k_8, 0),
                )
            } else {
                (NONE, NONE)
            };
            let (v_neg1_mask, v_zero_mask) = if has_count_cond {
                let c_neg1 = push_iconst(&mut func, plan.elem_ty, u64::MAX);
                let c_zero = push_iconst(&mut func, plan.elem_ty, 0);
                (
                    push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, c_neg1, 0),
                    push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, c_zero, 0),
                )
            } else {
                (NONE, NONE)
            };
            // Hoist invariant scalar splats and body constant splats once into `vec_pre`!
            let mut inv_splat_map: Vec<(u32, u32), A> = Vec::with_capacity_in(plan.inv_scalars.len(), alloc);
            for &sv in &plan.inv_scalars {
                let sp = push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, at(&val_map, to_idx(sv)), 0);
                inv_splat_map.push((sv, sp));
            }
            let mut body_const_splats: Vec<(usize, u32), A> = Vec::new_in(alloc);
            for &ei in &plan.vec_exprs {
                let e = to_idx(ei);
                if matches!(view.op(e), Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide) {
                    let c_new = copy_inst_remapped_blocks(&view, e, &val_map, &blk_map, &mut func);
                    let sp = push_inst(&mut func, Op::Splat, Flags::NONE, plan.vec_ty, c_new, 0);
                    body_const_splats.push((e, sp));
                }
            }

            let n_iv_params = if use_ptr_iv { streams.len() } else { 1 };
            let (loop_p0_end, chunk_acc_params, chunk_rem_after) = if needs_chunk {
                let mut chunk_init_args: Vec<u32, A> =
                    Vec::with_capacity_in(streams.len() + 1 + plan.reductions.len(), alloc);
                for s in &streams {
                    chunk_init_args.push(s.init_ptr);
                }
                chunk_init_args.push(vec_bytes_total);
                for red in &plan.reductions {
                    chunk_init_args.push(at(&val_map, to_idx(red.init_val)));
                }
                push_jump(&mut func, vec_chunk_blk, &chunk_init_args);

                // --- Emit `vec_chunk` block (overflow-free chunking for widening `CountCond` reductions) ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let mut chunk_ptrs: Vec<u32, A> = Vec::with_capacity_in(streams.len(), alloc);
                for _ in 0..streams.len() {
                    chunk_ptrs.push(push_inst(&mut func, Op::Param, Flags::NONE, Ty::PTR, 0, 0));
                }
                let chunk_rem_bytes = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let mut chunk_accs: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for red in &plan.reductions {
                    chunk_accs.push(push_inst(&mut func, Op::Param, Flags::NONE, red.acc_ty, 0, 0));
                }
                let cur_p0 = at(&chunk_ptrs, 0);
                let k_cbytes = push_iconst(&mut func, Ty::I64, chunk_elems << plan.elem_shift);
                let step_c = push_bin(&mut func, Op::Umin, Flags::NONE, Ty::I64, chunk_rem_bytes, k_cbytes);
                let c_end = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, cur_p0, step_c);
                let rem_after = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, chunk_rem_bytes, step_c);

                let mut vloop_args: Vec<u32, A> =
                    Vec::with_capacity_in(streams.len() + plan.reductions.len() * unroll, alloc);
                for &cp in &chunk_ptrs {
                    vloop_args.push(cp);
                }
                for (ridx, red) in plan.reductions.iter().enumerate() {
                    if matches!(red.input, ReductionInput::OrderedFadd { .. }) {
                        vloop_args.push(at(&chunk_accs, ridx));
                    } else {
                        let sp = at(&red_id_splats, ridx);
                        for _ in 0..unroll {
                            vloop_args.push(sp);
                        }
                    }
                }
                push_jump(&mut func, vec_loop_blk, &vloop_args);
                (c_end, chunk_accs, rem_after)
            } else {
                let mut vec_init_args: Vec<u32, A> =
                    Vec::with_capacity_in(n_iv_params + plan.reductions.len() * unroll, alloc);
                if use_ptr_iv {
                    for s in &streams {
                        vec_init_args.push(s.init_ptr);
                    }
                } else {
                    vec_init_args.push(iv_init_new);
                }
                for (ridx, red) in plan.reductions.iter().enumerate() {
                    let init_v = at(&red_id_splats, ridx);
                    if matches!(red.input, ReductionInput::OrderedFadd { .. }) {
                        vec_init_args.push(init_v);
                    } else {
                        for _ in 0..unroll {
                            vec_init_args.push(init_v);
                        }
                    }
                }
                push_jump(&mut func, vec_loop_blk, &vec_init_args);
                (p0_end, Vec::new_in(alloc), NONE)
            };

            // --- Emit `vec_loop` block (4x = 64B/iter or 2x = 32B/iter 128-bit SIMD!) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mut ptr_params: Vec<u32, A> = Vec::with_capacity_in(streams.len(), alloc);
            let iv_v = if use_ptr_iv {
                for _ in 0..streams.len() {
                    ptr_params.push(push_inst(&mut func, Op::Param, Flags::NONE, Ty::PTR, 0, 0));
                }
                NONE
            } else {
                let iv_ty = match plan.iv_kind {
                    IvKind::Counted { iv_ty } => iv_ty,
                    IvKind::Pointer => Ty::I64,
                };
                push_inst(&mut func, Op::Param, Flags::NONE, iv_ty, 0, 0)
            };
            let mut vacc_params: Vec<[u32; 4], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                let mut row = [NONE; 4];
                if matches!(red.input, ReductionInput::OrderedFadd { .. }) {
                    set_at(&mut row, 0, push_inst(&mut func, Op::Param, Flags::NONE, red.acc_ty, 0, 0));
                } else {
                    for u in 0..unroll {
                        set_at(&mut row, u, push_inst(&mut func, Op::Param, Flags::NONE, loop_vec_ty, 0, 0));
                    }
                }
                vacc_params.push(row);
            }

            // Emit contiguous adjacent vector loads directly off running pointers (`+0,+16,+32,+48` ->
            // TWO post-index `ldp q, q, [p], #32` = 64B at 4x with ZERO `lsl`/`PtrAdd(base, off)`!).
            let mut vmaps: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(unroll, alloc);
            for _ in 0..unroll {
                vmaps.push(filled(len, NONE, &alloc));
            }
            for &(sv, sp) in &inv_splat_map {
                for vm in &mut vmaps {
                    set_at(vm, to_idx(sv), sp);
                }
            }
            for &(e, sp) in &body_const_splats {
                for vm in &mut vmaps {
                    set_at(vm, e, sp);
                }
            }
            let mut ptr_nexts = filled(streams.len(), NONE, &alloc);
            let vec_bytes_i32 = vec_bytes.cast_signed();
            for (lidx, ld) in plan.loads.iter().enumerate() {
                let si = at(&load_stream_idx, lidx);
                let vptr = at(&ptr_params, si);
                for (u, vm) in vmaps.iter_mut().enumerate() {
                    let idx = to_u32(func.extra.len());
                    func.extra.push(vptr);
                    let u_i32 = i32::try_from(u).unwrap_or(0);
                    func.extra.push(ld.const_off.wrapping_add(u_i32.wrapping_mul(vec_bytes_i32)).cast_unsigned());
                    let q = push_inst(&mut func, Op::Load, Flags::NONE, loop_vec_ty, idx, ld.mem_attr);
                    set_at(vm, to_idx(ld.inst), q);
                }
                let is_last_load_of_stream =
                    !load_stream_idx.get(lidx + 1..).is_some_and(|rest| rest.iter().any(|&s| s == si));
                let has_store = streams.get(si).is_some_and(|s| s.has_store);
                if is_last_load_of_stream && !has_store {
                    let k_sb = push_iconst(&mut func, Ty::I64, step_bytes);
                    let p_next = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, vptr, k_sb);
                    set_at(&mut ptr_nexts, si, p_next);
                }
            }

            // Vectorize body expressions across all unrolled lanes (including `Fma`/`Fmuladd`,
            // `Smin`/`Smax`/`Umin`/`Umax`, and masked `Icmp` & `Select`!).
            let mask_ty = plan.vec_ty.with_elem(crate::ty::Scalar::I1);
            for &ei in &plan.vec_exprs {
                let e = to_idx(ei);
                let op = view.op(e);
                let fl = Flags(at(view.flags, e));
                if matches!(op, Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide) {
                    // Already hoisted and splatted once in `vec_pre`.
                } else if op == Op::Icmp {
                    let idx = to_idx(at(view.data, e));
                    let cond_w = at(view.extra, idx);
                    let (a, bv) = (to_idx(at(view.extra, idx + 1)), to_idx(at(view.extra, idx + 2)));
                    for vm in &mut vmaps {
                        let dst = to_u32(func.extra.len());
                        func.extra.push(cond_w);
                        func.extra.push(at(vm, a));
                        func.extra.push(at(vm, bv));
                        let nv = push_inst(&mut func, Op::Icmp, fl, mask_ty, dst, 0);
                        set_at(vm, e, nv);
                    }
                } else if op == Op::Select {
                    let idx = to_idx(at(view.data, e));
                    let (cv, tv, fv) = (
                        to_idx(at(view.extra, idx)),
                        to_idx(at(view.extra, idx + 1)),
                        to_idx(at(view.extra, idx + 2)),
                    );
                    for vm in &mut vmaps {
                        let dst = to_u32(func.extra.len());
                        func.extra.push(at(vm, cv));
                        func.extra.push(at(vm, tv));
                        func.extra.push(at(vm, fv));
                        let nv = push_inst(&mut func, Op::Select, fl, plan.vec_ty, dst, 0);
                        set_at(vm, e, nv);
                    }
                } else if matches!(op, Op::Fma | Op::Fmuladd) {
                    let idx = to_idx(at(view.data, e));
                    let (a, bv, cv) = (
                        to_idx(at(view.extra, idx)),
                        to_idx(at(view.extra, idx + 1)),
                        to_idx(at(view.extra, idx + 2)),
                    );
                    for vm in &mut vmaps {
                        let nv = push_tern(&mut func, op, fl, plan.vec_ty, at(vm, a), at(vm, bv), at(vm, cv));
                        set_at(vm, e, nv);
                    }
                } else {
                    let idx = to_idx(at(view.data, e));
                    let (a, bv) = (to_idx(at(view.extra, idx)), to_idx(at(view.extra, idx + 1)));
                    for vm in &mut vmaps {
                        let nv = push_bin(&mut func, op, fl, plan.vec_ty, at(vm, a), at(vm, bv));
                        set_at(vm, e, nv);
                    }
                }
            }

            // Emit contiguous adjacent vector stores directly to the stream's running pointer
            // (`+0,+16,+32,+48` -> TWO post-index `stp q, q, [p], #32` = 64B at 4x!), sharing
            // one running pointer with loads on in-place read-modify-write loops (`saxpy`, `matmul`).
            if let (Some(st), Some(si)) = (plan.store, store_stream_idx) {
                let dptr = at(&ptr_params, si);
                for (u, vm) in vmaps.iter().enumerate() {
                    let sv = at(vm, to_idx(st.store_val));
                    let idx = to_u32(func.extra.len());
                    func.extra.push(sv);
                    func.extra.push(dptr);
                    let u_i32 = i32::try_from(u).unwrap_or(0);
                    func.extra.push(st.const_off.wrapping_add(u_i32.wrapping_mul(vec_bytes_i32)).cast_unsigned());
                    push_inst(&mut func, Op::Store, Flags::NONE, Ty::NONE, idx, st.mem_attr);
                }
                let k_sb = push_iconst(&mut func, Ty::I64, step_bytes);
                let p_next = push_bin(&mut func, Op::PtrAdd, Flags::NONE, Ty::PTR, dptr, k_sb);
                set_at(&mut ptr_nexts, si, p_next);
            }

            // Update `unroll` independent vector accumulators and test `p0_next < loop_p0_end`!
            let mut vec_back_args: Vec<u32, A> =
                Vec::with_capacity_in(n_iv_params + plan.reductions.len() * unroll, alloc);
            let mut vec_red_args: Vec<u32, A> = Vec::with_capacity_in(
                (if needs_chunk { streams.len() } else { 0 }) + plan.reductions.len() * unroll,
                alloc,
            );
            let iv_v_next = if use_ptr_iv {
                for &pn in &ptr_nexts {
                    vec_back_args.push(pn);
                    if needs_chunk {
                        vec_red_args.push(pn);
                    }
                }
                NONE
            } else {
                let iv_ty = match plan.iv_kind {
                    IvKind::Counted { iv_ty } => iv_ty,
                    IvKind::Pointer => Ty::I64,
                };
                let k_ustep = push_iconst(&mut func, iv_ty, unroll_step);
                let next = push_bin(&mut func, Op::Add, Flags::NONE, iv_ty, iv_v, k_ustep);
                vec_back_args.push(next);
                next
            };
            for (ridx, red) in plan.reductions.iter().enumerate() {
                if let ReductionInput::OrderedFadd { elem, acc_is_lhs } = red.input {
                    let mut cur_acc = vacc_params.get(ridx).and_then(|row| row.first()).copied().unwrap_or(NONE);
                    for vm in &vmaps {
                        let v_elem = at(vm, to_idx(elem));
                        for lane in 0..plan.lanes {
                            let lane_v = push_extract(&mut func, plan.elem_ty, v_elem, lane);
                            let (lhs, rhs) = if acc_is_lhs { (cur_acc, lane_v) } else { (lane_v, cur_acc) };
                            cur_acc = push_bin(&mut func, Op::Fadd, red.flags, plan.elem_ty, lhs, rhs);
                        }
                    }
                    vec_back_args.push(cur_acc);
                    vec_red_args.push(cur_acc);
                    continue;
                }
                for (u, vm) in vmaps.iter().enumerate() {
                    let acc_u = vacc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                    let n_acc = match red.input {
                        ReductionInput::Elem(elem_val) => push_bin(
                            &mut func,
                            red.op,
                            red.flags,
                            plan.vec_ty,
                            acc_u,
                            at(vm, to_idx(elem_val)),
                        ),
                        ReductionInput::Fma { fma_op, a, b } => push_tern(
                            &mut func,
                            fma_op,
                            red.flags,
                            plan.vec_ty,
                            at(vm, to_idx(a)),
                            at(vm, to_idx(b)),
                            acc_u,
                        ),
                        ReductionInput::CountCond { cond, a, b } => {
                            let dst = to_u32(func.extra.len());
                            func.extra.push(int_cond_word(cond));
                            func.extra.push(at(vm, to_idx(a)));
                            func.extra.push(at(vm, to_idx(b)));
                            let vmask = push_inst(&mut func, Op::Icmp, Flags::NONE, mask_ty, dst, 0);
                            let sdst = to_u32(func.extra.len());
                            func.extra.push(vmask);
                            func.extra.push(v_neg1_mask);
                            func.extra.push(v_zero_mask);
                            let mvec = push_inst(&mut func, Op::Select, Flags::NONE, plan.vec_ty, sdst, 0);
                            push_bin(&mut func, Op::Sub, Flags::NONE, plan.vec_ty, acc_u, mvec)
                        }
                        ReductionInput::Popcnt { src } => {
                            let v_bytes = at(vm, to_idx(src));
                            let v_cnt = push_inst(&mut func, Op::Popcnt, Flags::NONE, loop_vec_ty, v_bytes, 0);
                            push_bin(&mut func, Op::Add, Flags::NONE, loop_vec_ty, acc_u, v_cnt)
                        }
                        ReductionInput::OrderedFadd { .. } => NONE,
                    };
                    vec_back_args.push(n_acc);
                    vec_red_args.push(n_acc);
                }
            }
            let keep_vec = if use_ptr_iv {
                push_icmp(&mut func, IntCond::Ult, at(&ptr_nexts, 0), loop_p0_end)
            } else {
                push_icmp(&mut func, IntCond::Ult, iv_v_next, vec_end)
            };
            push_br(&mut func, keep_vec, vec_loop_blk, &vec_back_args, vec_reduce_blk, &vec_red_args);

            // --- Emit `vec_reduce` block (tree-reduces all accumulators; uses `vec_end` from `vec_pre`!) ---
            func.block_starts.push(to_u32(func.ops.len()));
            func.block_kinds.push(block_kind_byte(BlockKind::Normal));
            let mut done_ptr_params: Vec<u32, A> = Vec::with_capacity_in(if needs_chunk { streams.len() } else { 0 }, alloc);
            if needs_chunk {
                for _ in 0..streams.len() {
                    done_ptr_params.push(push_inst(&mut func, Op::Param, Flags::NONE, Ty::PTR, 0, 0));
                }
            }
            let mut done_acc_params: Vec<[u32; 4], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for red in &plan.reductions {
                let mut row = [NONE; 4];
                if matches!(red.input, ReductionInput::OrderedFadd { .. }) {
                    set_at(&mut row, 0, push_inst(&mut func, Op::Param, Flags::NONE, red.acc_ty, 0, 0));
                } else {
                    for u in 0..unroll {
                        set_at(&mut row, u, push_inst(&mut func, Op::Param, Flags::NONE, loop_vec_ty, 0, 0));
                    }
                }
                done_acc_params.push(row);
            }
            let mut red_scalar_vals: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
            for (ridx, red) in plan.reductions.iter().enumerate() {
                let [d0, d1, d2, d3] = done_acc_params.get(ridx).copied().unwrap_or([NONE; 4]);
                if matches!(red.input, ReductionInput::OrderedFadd { .. }) {
                    red_scalar_vals.push(d0);
                    continue;
                }
                let init_new = if needs_chunk {
                    at(&chunk_acc_params, ridx)
                } else {
                    at(&val_map, to_idx(red.init_val))
                };
                if matches!(red.input, ReductionInput::Popcnt { .. }) {
                    // Widen `<16 x i8>` byte accumulators pairwise into `<8 x i16>` (ZERO `u16` overflow:
                    // max per `u16` lane = `4 * 2 * 240 = 1,920`, sum of 8 lanes = `15,360 <= 65,535`).
                    let widen_pair = |func: &mut Func<A>, vb: u32| -> u32 {
                        let vh = push_inst(func, Op::Bitcast, Flags::NONE, popcnt_wide_ty, vb, 0);
                        let lo = push_bin(func, Op::And, Flags::NONE, popcnt_wide_ty, vh, v_pop_mask_ff);
                        let hi = push_bin(func, Op::Lshr, Flags::NONE, popcnt_wide_ty, vh, v_pop_sh_8);
                        push_bin(func, Op::Add, Flags::NONE, popcnt_wide_ty, lo, hi)
                    };
                    let w0 = widen_pair(&mut func, d0);
                    let w1 = widen_pair(&mut func, d1);
                    let w_all = if unroll == 4 {
                        let w2 = widen_pair(&mut func, d2);
                        let w3 = widen_pair(&mut func, d3);
                        let w01 = push_bin(&mut func, Op::Add, Flags::NONE, popcnt_wide_ty, w0, w1);
                        let w23 = push_bin(&mut func, Op::Add, Flags::NONE, popcnt_wide_ty, w2, w3);
                        push_bin(&mut func, Op::Add, Flags::NONE, popcnt_wide_ty, w01, w23)
                    } else {
                        push_bin(&mut func, Op::Add, Flags::NONE, popcnt_wide_ty, w0, w1)
                    };
                    let chunk_sum_16 = push_inst(&mut func, Op::ReduceAdd, Flags::NONE, Ty::I16, w_all, 0);
                    let chunk_sum = push_inst(&mut func, Op::Zext, Flags::NONE, red.acc_ty, chunk_sum_16, 0);
                    let final_s = push_bin(&mut func, Op::Add, Flags::NONE, red.acc_ty, init_new, chunk_sum);
                    red_scalar_vals.push(final_s);
                    continue;
                }
                let comb = if unroll == 4 {
                    let c01 = push_bin(&mut func, red.op, red.flags, plan.vec_ty, d0, d1);
                    let c23 = push_bin(&mut func, red.op, red.flags, plan.vec_ty, d2, d3);
                    push_bin(&mut func, red.op, red.flags, plan.vec_ty, c01, c23)
                } else {
                    push_bin(&mut func, red.op, red.flags, plan.vec_ty, d0, d1)
                };
                let final_s = if red.op == Op::Fadd {
                    push_bin(&mut func, Op::ReduceFadd, red.flags, plan.elem_ty, init_new, comb)
                } else {
                    let red_op = match red.op {
                        Op::Add => Op::ReduceAdd,
                        Op::Xor => Op::ReduceXor,
                        Op::Or => Op::ReduceOr,
                        Op::And => Op::ReduceAnd,
                        Op::Smin => Op::ReduceSmin,
                        Op::Smax => Op::ReduceSmax,
                        Op::Umin => Op::ReduceUmin,
                        Op::Umax => Op::ReduceUmax,
                        _ => {
                            return Err(SimplifyError::Refused {
                                inst: to_idx(red.update_inst),
                                what: "unsupported vector reduction op",
                            });
                        }
                    };
                    let r_lane = push_inst(&mut func, red_op, Flags::NONE, plan.elem_ty, comb, 0);
                    let r_wide = if red.acc_ty == plan.elem_ty {
                        r_lane
                    } else {
                        push_inst(&mut func, Op::Zext, Flags::NONE, red.acc_ty, r_lane, 0)
                    };
                    if is_minmax_op(red.op) {
                        r_wide
                    } else {
                        push_bin(&mut func, red.op, Flags::NONE, red.acc_ty, init_new, r_wide)
                    }
                };
                red_scalar_vals.push(final_s);
            }

            let build_tail_args = |accs: &[u32]| -> Vec<u32, A> {
                let mut targs: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
                for (pos, &pre_arg) in plan.pre_args.iter().enumerate() {
                    if pos == plan.iv_pos {
                        targs.push(vec_end);
                    } else if let Some(sidx) = plan.sec_ptr_ivs.iter().position(|s| s.param_pos == pos) {
                        targs.push(at(&sec_ptr_ends, sidx));
                    } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                        targs.push(at(accs, ridx));
                    } else {
                        targs.push(at(&val_map, to_idx(pre_arg)));
                    }
                }
                targs
            };

            if needs_chunk {
                let k_z64 = push_iconst(&mut func, Ty::I64, 0);
                let more_chunks = push_icmp(&mut func, IntCond::Ne, chunk_rem_after, k_z64);
                let mut chunk_back_args: Vec<u32, A> =
                    Vec::with_capacity_in(streams.len() + 1 + plan.reductions.len(), alloc);
                for &dp in &done_ptr_params {
                    chunk_back_args.push(dp);
                }
                chunk_back_args.push(chunk_rem_after);
                for &rs in &red_scalar_vals {
                    chunk_back_args.push(rs);
                }
                if is_single {
                    push_br(&mut func, more_chunks, vec_chunk_blk, &chunk_back_args, vec_tail_blk, &red_scalar_vals);
                    // --- Emit `vec_tail` block for `SingleBlock` chunked reductions ---
                    func.block_starts.push(to_u32(func.ops.len()));
                    func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                    let mut final_accs: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                    for red in &plan.reductions {
                        final_accs.push(push_inst(&mut func, Op::Param, Flags::NONE, red.acc_ty, 0, 0));
                    }
                    let tail_args = build_tail_args(&final_accs);
                    let has_tail = if matches!(plan.iv_kind, IvKind::Pointer)
                        && Ty(at(view.tys, to_idx(plan.bound))) != Ty::PTR
                    {
                        let vec_end_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, vec_end, 0);
                        push_icmp(&mut func, IntCond::Ult, vec_end_addr, bound_new)
                    } else {
                        push_icmp(&mut func, IntCond::Ult, vec_end, bound_new)
                    };
                    let mut direct_exit_args: Vec<u32, A> = Vec::with_capacity_in(plan.single_exit_args.len(), alloc);
                    for &ea in &plan.single_exit_args {
                        if let Some(ridx) = plan.reductions.iter().position(|r| r.update_inst == ea) {
                            direct_exit_args.push(at(&final_accs, ridx));
                        } else {
                            direct_exit_args.push(at(&val_map, to_idx(ea)));
                        }
                    }
                    push_br(&mut func, has_tail, orig_header_new, &tail_args, orig_exit_new, &direct_exit_args);
                } else {
                    let tail_args = build_tail_args(&red_scalar_vals);
                    push_br(&mut func, more_chunks, vec_chunk_blk, &chunk_back_args, orig_header_new, &tail_args);
                }
            } else {
                let tail_args = build_tail_args(&red_scalar_vals);
                match plan.shape {
                    LoopShape::HeaderBody { .. } => {
                        push_jump(&mut func, orig_header_new, &tail_args);
                    }
                    LoopShape::SingleBlock { .. } => {
                        let has_tail = if matches!(plan.iv_kind, IvKind::Pointer)
                            && Ty(at(view.tys, to_idx(plan.bound))) != Ty::PTR
                        {
                            let vec_end_addr = push_inst(&mut func, Op::PtrAddr, Flags::NONE, Ty::I64, vec_end, 0);
                            push_icmp(&mut func, IntCond::Ult, vec_end_addr, bound_new)
                        } else {
                            push_icmp(&mut func, IntCond::Ult, vec_end, bound_new)
                        };
                        let mut direct_exit_args: Vec<u32, A> = Vec::with_capacity_in(plan.single_exit_args.len(), alloc);
                        for &ea in &plan.single_exit_args {
                            if let Some(ridx) = plan.reductions.iter().position(|r| r.update_inst == ea) {
                                direct_exit_args.push(at(&red_scalar_vals, ridx));
                            } else {
                                direct_exit_args.push(at(&val_map, to_idx(ea)));
                            }
                        }
                        push_br(&mut func, has_tail, orig_header_new, &tail_args, orig_exit_new, &direct_exit_args);
                    }
                }
            }
        } else {
            for inst in range {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(VectorizeOutput {
        func,
        stats: VectorizeStats { loops_vectorized: 1 },
    })
}

/// Computes exact 64-bit `MulShift` Granlund–Montgomery parameters `(magic, shift)` for divisor `d`.
fn magic_udiv64_mulshift(d: u64) -> Option<(u64, u32)> {
    if d <= 2 || d.is_power_of_two() || d > (u64::MAX / 2) {
        return None;
    }
    let d128 = u128::from(d);
    let max_n = u128::from(u64::MAX);
    let two64 = 1u128 << 64;
    let nc = two64 - (two64 % d128) - 1;
    for shift in 0..63u32 {
        let two_p = 1u128 << (64 + shift);
        let delta = d128 - (two_p % d128);
        let m = (two_p + delta) / d128;
        if m <= max_n
            && let Ok(m64) = u64::try_from(m)
            && nc.checked_mul(delta).is_some_and(|p| p < two_p)
        {
            return Some((m64, shift));
        }
    }
    None
}

/// Computes exact 32-bit `MulShift` Granlund–Montgomery parameters `(magic32, shift32)` for divisor `d`.
fn magic_udiv32_mulshift(d: u64) -> Option<(u32, u32)> {
    if d <= 2 || d.is_power_of_two() || d > (u64::from(u32::MAX) / 2) {
        return None;
    }
    let max_n = u64::from(u32::MAX);
    let two32 = 1u64 << 32;
    let nc = two32 - (two32 % d) - 1;
    for shift in 0..32u32 {
        let two_p = 1u64 << (32 + shift);
        let delta = d - (two_p % d);
        let m = (two_p + delta) / d;
        if m <= max_n
            && let Ok(m32) = u32::try_from(m)
            && nc.checked_mul(delta).is_some_and(|p| p < two_p)
        {
            return Some((m32, shift));
        }
    }
    None
}

/// Recovers `(divisor, magic32, total_rshift)` for a strength-reduced 64-bit unsigned constant division
/// `lshr(umulhi(iv, magic64), sh64)` so any runtime `bound <= u32::MAX` can execute 8-wide or 16-wide in NEON!
fn match_narrow_div_params(magic64: u64, sh64: u32) -> Option<(u64, u32, u32)> {
    if magic64 == 0 || sh64 >= 63 {
        return None;
    }
    let two_p = 1u128 << (64 + sh64);
    let d_u128 = (two_p / u128::from(magic64)) + 1;
    let d = u64::try_from(d_u128).ok()?;
    if magic_udiv64_mulshift(d) != Some((magic64, sh64)) {
        return None;
    }
    let (m32, s32) = magic_udiv32_mulshift(d)?;
    if 32 + s32 >= 64 {
        return None;
    }
    Some((d, m32, 32 + s32))
}

/// Dynamic-range 8-wide (4x `<2 x i64>`) NEON widening-multiply vectorization for unit-step `u64`
/// reduction loops containing strength-reduced constant division (`i / D`, `i % D`).
///
/// Guards dynamically on `bound <= u32::MAX && rem >= 8` (falling back to the untouched 64-bit
/// loop for huge bounds > 4B), turning 1-wide scalar `umulh` bottlenecks into 4-wide NEON `umull.2d`!
pub fn vectorize_iv_div_loops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<VectorizeOutput<A>, SimplifyError> {
    if view.head.ptr_bits != 64 || view.blocks() < 2 {
        return Ok(VectorizeOutput { func: view.to_func_in(alloc), stats: VectorizeStats::default() });
    }
    let blocks = view.blocks();
    let len = view.len();

    let mut inst_block = filled(len, 0u32, &alloc);
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
        }
        let term = view.terminator(Block(b_u32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let tb = to_idx(edge.block);
                if let Some(p) = preds.get_mut(tb)
                    && p.scan_for(&b_u32).is_none()
                {
                    p.push(b_u32);
                }
            }
        });
    }

    struct DivSite {
        umulhi_inst: u32,
        lshr_inst: u32,
        divisor: u64,
        magic32: u32,
        rshift: u32,
    }

    struct IvDivPlan<A: Allocator> {
        preheader: u32,
        header: u32,
        iv_pos: usize,
        iv_inst: u32,
        iv_init: u32,
        bound: u32,
        pre_args: Vec<u32, A>,
        reductions: Vec<ReductionSpec, A>,
        div_sites: Vec<DivSite, A>,
        body_insts: Vec<u32, A>,
    }

    let mut vi: Option<ValueInfo<A>> = None;
    let mut found: Option<IvDivPlan<A>> = None;
    for h in 1..blocks {
        let header = to_u32(h);
        if view.block_kind(Block(header)) != BlockKind::Normal {
            continue;
        }
        let (p0, p1) = match preds.get(h).map(Vec::as_slice) {
            Some(&[p0, p1]) => (p0, p1),
            _ => continue,
        };
        let (preheader, body_block) = if p0 < header && p1 > header {
            (p0, p1)
        } else if p1 < header && p0 > header {
            (p1, p0)
        } else {
            continue;
        };
        if preds.get(to_idx(body_block)).map(Vec::as_slice) != Some(&[header])
            || !view.params(Block(body_block)).is_empty()
        {
            continue;
        }
        let pre_term = view.terminator(Block(preheader));
        let h_term = view.terminator(Block(header));
        let b_term = view.terminator(Block(body_block));
        if view.op(pre_term) != Op::Jump || view.op(h_term) != Op::Br || view.op(b_term) != Op::Jump {
            continue;
        }
        let pre_at = to_idx(at(view.data, pre_term));
        let hat = to_idx(at(view.data, h_term));
        let bat = to_idx(at(view.data, b_term));
        let h_params = view.params(Block(header));
        if h_params.len() < 2
            || at(view.extra, pre_at) != header
            || to_idx(at(view.extra, pre_at + 1)) != h_params.len()
            || at(view.extra, bat) != header
            || to_idx(at(view.extra, bat + 1)) != h_params.len()
        {
            continue;
        }
        let cond_v = at(view.extra, hat);
        let cond_i = to_idx(cond_v);
        let t_blk = at(view.extra, hat + 1);
        let t_argc = to_idx(at(view.extra, hat + 2));
        let e_blk = at(view.extra, hat + 3 + t_argc);
        if t_blk != body_block || t_argc != 0 || e_blk == header || e_blk == body_block {
            continue;
        }
        let h_range = view.block(Block(header));
        if (h_params.end..(h_range.end - 1)).any(|i| i != cond_i) {
            continue;
        }
        if view.op(cond_i) != Op::Icmp {
            continue;
        }
        let cat = to_idx(at(view.data, cond_i));
        if !matches!(IntCond::from_word(at(view.extra, cat)), Some(IntCond::Ult | IntCond::Ule)) {
            continue;
        }
        let cmp_lhs = at(view.extra, cat + 1);
        let bound = at(view.extra, cat + 2);
        let is_loop_blk = |b: u32| b == header || b == body_block;
        if is_loop_blk(at(&inst_block, to_idx(bound))) || Ty(at(view.tys, to_idx(bound))) != Ty::I64 {
            continue;
        }

        let mut pre_args = Vec::with_capacity_in(h_params.len(), alloc);
        for k in 0..h_params.len() {
            pre_args.push(at(view.extra, pre_at + 2 + k));
        }

        // Find unit-step `i64` IV.
        let mut found_iv = None;
        for pos in 0..h_params.len() {
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            if p_inst != cmp_lhs || Ty(at(view.tys, p_idx)) != Ty::I64 {
                continue;
            }
            let back_v = at(view.extra, bat + 2 + pos);
            let back_i = to_idx(back_v);
            if view.op(back_i) == Op::Add {
                let aat = to_idx(at(view.data, back_i));
                let (al, ar) = (at(view.extra, aat), at(view.extra, aat + 1));
                if (al == p_inst && const_u64(&view, to_idx(ar)) == Some(1))
                    || (ar == p_inst && const_u64(&view, to_idx(al)) == Some(1))
                {
                    found_iv = Some((pos, p_inst, back_v));
                    break;
                }
            }
        }
        let Some((iv_pos, iv_inst, iv_next_inst)) = found_iv else {
            continue;
        };

        let body_range = view.block(Block(body_block));
        let body_start = body_range.start;
        let body_end = body_range.end - 1;
        if body_end - body_start == 0 || body_end - body_start > 20 {
            continue;
        }

        // Match reductions (`Add`/`Xor`/`Or`/`And` on `I64`).
        let mut reductions: Vec<ReductionSpec, A> = Vec::new_in(alloc);
        let mut is_red_upd = filled(len, false, &alloc);
        let mut bad = false;
        for pos in 0..h_params.len() {
            if pos == iv_pos {
                continue;
            }
            let p_idx = h_params.start + pos;
            let p_inst = to_u32(p_idx);
            if Ty(at(view.tys, p_idx)) != Ty::I64 {
                bad = true;
                break;
            }
            let back_v = at(view.extra, bat + 2 + pos);
            let bi = to_idx(back_v);
            let bop = view.op(bi);
            if !matches!(bop, Op::Add | Op::Xor | Op::Or | Op::And) || at(&inst_block, bi) != body_block {
                bad = true;
                break;
            }
            let aat = to_idx(at(view.data, bi));
            let (l, r) = (at(view.extra, aat), at(view.extra, aat + 1));
            let elem_val = if l == p_inst { r } else if r == p_inst { l } else { bad = true; break; };
            set_at(&mut is_red_upd, bi, true);
            reductions.push(ReductionSpec {
                param_pos: pos,
                param_inst: p_inst,
                acc_ty: Ty::I64,
                init_val: at(&pre_args, pos),
                op: bop,
                flags: Flags::NONE,
                update_inst: back_v,
                input: ReductionInput::Elem(elem_val),
            });
        }
        if bad || reductions.is_empty() {
            continue;
        }

        // Verify no body instruction escapes outside the loop.
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            if is_loop_blk(b_u32) {
                continue;
            }
            for inst in view.block(Block(b_u32)) {
                each(&view, inst, |item| {
                    if let Item::Value(v) = item {
                        let vi = to_idx(v);
                        if is_loop_blk(at(&inst_block, vi)) && !(vi >= h_params.start && vi < h_params.end) {
                            bad = true;
                        }
                    }
                });
            }
        }
        if bad {
            continue;
        }

        // Match `lshr(umulhi(iv, magic64), sh64)` division sites and validate all other body ops are `<2 x i64>`-vectorizable!
        let mut div_sites: Vec<DivSite, A> = Vec::new_in(alloc);
        let mut is_div_umulhi = filled(len, false, &alloc);
        let mut is_div_lshr = filled(len, false, &alloc);
        for inst in body_start..body_end {
            if view.op(inst) == Op::Lshr && Ty(at(view.tys, inst)) == Ty::I64 {
                let idx = to_idx(at(view.data, inst));
                let src_u32 = at(view.extra, idx);
                let src = to_idx(src_u32);
                let sh_v = to_idx(at(view.extra, idx + 1));
                if view.op(src) == Op::Umulhi && at(&inst_block, src) == body_block {
                    let uat = to_idx(at(view.data, src));
                    let (ul, ur) = (at(view.extra, uat), at(view.extra, uat + 1));
                    let magic = if ul == iv_inst {
                        const_u64(&view, to_idx(ur))
                    } else if ur == iv_inst {
                        const_u64(&view, to_idx(ul))
                    } else {
                        None
                    };
                    if let (Some(m64), Some(sh64)) = (magic, const_u64(&view, sh_v))
                        && let Ok(sh32) = u32::try_from(sh64)
                        && let Some((divisor, magic32, rshift)) = match_narrow_div_params(m64, sh32)
                    {
                        set_at(&mut is_div_umulhi, src, true);
                        set_at(&mut is_div_lshr, inst, true);
                        div_sites.push(DivSite {
                            umulhi_inst: src_u32,
                            lshr_inst: to_u32(inst),
                            divisor,
                            magic32,
                            rshift,
                        });
                    }
                }
            }
        }
        if div_sites.is_empty() {
            continue;
        }

        let iv_next_idx = to_idx(iv_next_inst);
        let mut body_insts: Vec<u32, A> = Vec::new_in(alloc);
        for inst in body_start..body_end {
            if inst == iv_next_idx || at(&is_red_upd, inst) || at(&is_div_umulhi, inst) {
                continue;
            }
            if at(&is_div_lshr, inst) {
                body_insts.push(to_u32(inst));
                continue;
            }
            let op = view.op(inst);
            if Ty(at(view.tys, inst)) != Ty::I64 {
                bad = true;
                break;
            }
            let ok = match op {
                Op::Iconst | Op::IconstWide | Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor => true,
                Op::Shl | Op::Lshr => {
                    let idx = to_idx(at(view.data, inst));
                    const_u64(&view, to_idx(at(view.extra, idx + 1))).is_some_and(|k| k < 64)
                }
                _ => false,
            };
            if !ok {
                bad = true;
                break;
            }
            body_insts.push(to_u32(inst));
        }
        // Ensure no other instruction uses a `div_site.umulhi_inst` directly.
        for &bi in &body_insts {
            let bi_idx = to_idx(bi);
            if !at(&is_div_lshr, bi_idx) {
                each(&view, bi_idx, |item| {
                    if let Item::Value(v) = item
                        && at(&is_div_umulhi, to_idx(v))
                    {
                        bad = true;
                    }
                });
            }
        }
        if bad {
            continue;
        }

        let iv_init = at(&pre_args, iv_pos);
        let info = vi.get_or_insert_with(|| ValueInfo::analyze(view, alloc));
        if trip_count_provably_below(
            &view,
            info,
            header,
            IvKind::Counted { iv_ty: Ty::I64 },
            iv_init,
            bound,
            8,
            64,
        ) {
            continue;
        }
        found = Some(IvDivPlan {
            preheader,
            header,
            iv_pos,
            iv_inst,
            iv_init,
            bound,
            pre_args,
            reductions,
            div_sites,
            body_insts,
        });
        break;
    }

    let Some(plan) = found else {
        return Ok(VectorizeOutput { func: view.to_func_in(alloc), stats: VectorizeStats::default() });
    };

    // Check whether the body DAG is provably non-overflowing and non-borrowing in `u32` for
    // `iv < 2^30` (over 1 billion iterations!) with additive `u64` reductions. If so, vectorize
    // 4x `<4 x i32>` = 16 elements/iter using `Op::Umulhi` (`umull.2d` + `umull2.2d` + `uzp2.4s`)
    // and widening pairwise accumulate (`uadalp.2d`)!
    const U32_FAST_MAX: u64 = 1u64 << 30;
    let can_use_4xi32 = plan.reductions.iter().all(|r| r.op == Op::Add)
        && plan.div_sites.iter().all(|ds| ds.rshift >= 32 && ds.rshift < 64)
        && {
            // Track `[lo, hi]` value bounds for `iv in 0..=U32_FAST_MAX`, plus exact multiples
            // `Some((d, k))` meaning `val == k * (iv / d)` so `Sub(iv, k * (iv / d))` with `k <= d`
            // is proven non-negative (`>= 0`) without borrowing!
            let mut bounds: Vec<Option<(u64, u64)>, A> = Vec::with_capacity_in(len, alloc);
            let mut q_mul: Vec<Option<(u64, u64)>, A> = Vec::with_capacity_in(len, alloc);
            for _ in 0..len {
                bounds.push(None);
                q_mul.push(None);
            }
            set_at(&mut bounds, to_idx(plan.iv_inst), Some((0, U32_FAST_MAX)));
            let mut ok32 = true;
            for &bi in &plan.body_insts {
                let idx = to_idx(bi);
                if let Some(ds) = plan.div_sites.iter().find(|ds| ds.lshr_inst == bi) {
                    set_at(&mut bounds, idx, Some((0, U32_FAST_MAX / ds.divisor)));
                    set_at(&mut q_mul, idx, Some((ds.divisor, 1)));
                    continue;
                }
                let op = view.op(idx);
                let next_mask = |x: u64| if x == 0 { 0 } else { u64::MAX >> x.leading_zeros() };
                let bound_of = |v: usize, b: &[Option<(u64, u64)>]| -> Option<(u64, u64)> {
                    if let Some(bv) = at(b, v) {
                        return Some(bv);
                    }
                    const_u64(&view, v).filter(|&c| c <= u64::from(u32::MAX)).map(|c| (c, c))
                };
                match op {
                    Op::Iconst | Op::IconstWide => {
                        if let Some(c) = const_u64(&view, idx).filter(|&c| c <= u64::from(u32::MAX)) {
                            set_at(&mut bounds, idx, Some((c, c)));
                        } else {
                            ok32 = false;
                            break;
                        }
                    }
                    Op::Add => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), bound_of(bv, &bounds)) {
                            (Some((la, ha)), Some((lb, hb))) if ha.saturating_add(hb) <= u64::from(u32::MAX) => {
                                set_at(&mut bounds, idx, Some((la + lb, ha + hb)));
                                if let (Some((da, ka)), Some((db, kb))) = (at(&q_mul, a), at(&q_mul, bv))
                                    && da == db
                                {
                                    set_at(&mut q_mul, idx, ka.checked_add(kb).map(|k| (da, k)));
                                }
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::Sub => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        if a == to_idx(plan.iv_inst)
                            && let Some((d, k)) = at(&q_mul, bv)
                            && k <= d
                        {
                            set_at(&mut bounds, idx, Some((0, U32_FAST_MAX)));
                        } else if let (Some((la, ha)), Some((lb, hb))) = (bound_of(a, &bounds), bound_of(bv, &bounds))
                            && la >= hb
                        {
                            set_at(&mut bounds, idx, Some((la - hb, ha - lb)));
                        } else {
                            ok32 = false;
                            break;
                        }
                    }
                    Op::Shl => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), const_u64(&view, bv)) {
                            (Some((la, ha)), Some(k)) if k < 32 && (ha << k) <= u64::from(u32::MAX) => {
                                set_at(&mut bounds, idx, Some((la << k, ha << k)));
                                if let Some((da, ka)) = at(&q_mul, a) {
                                    let k32 = u32::try_from(k).unwrap_or(0);
                                    set_at(&mut q_mul, idx, ka.checked_shl(k32).map(|kk| (da, kk)));
                                }
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::Lshr => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), const_u64(&view, bv)) {
                            (Some((la, ha)), Some(k)) if k < 32 => {
                                set_at(&mut bounds, idx, Some((la >> k, ha >> k)));
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::And => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), bound_of(bv, &bounds)) {
                            (Some((_, ha)), Some((_, hb))) => set_at(&mut bounds, idx, Some((0, ha.min(hb)))),
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    Op::Or | Op::Xor => {
                        let e_at = to_idx(at(view.data, idx));
                        let (a, bv) = (to_idx(at(view.extra, e_at)), to_idx(at(view.extra, e_at + 1)));
                        match (bound_of(a, &bounds), bound_of(bv, &bounds)) {
                            (Some((_, ha)), Some((_, hb))) => {
                                let hi = next_mask(ha.max(hb));
                                if hi <= u64::from(u32::MAX) {
                                    set_at(&mut bounds, idx, Some((0, hi)));
                                } else {
                                    ok32 = false;
                                    break;
                                }
                            }
                            _ => {
                                ok32 = false;
                                break;
                            }
                        }
                    }
                    _ => {
                        ok32 = false;
                        break;
                    }
                }
            }
            if ok32 {
                for red in &plan.reductions {
                    let ReductionInput::Elem(ev) = red.input else {
                        ok32 = false;
                        break;
                    };
                    if at(&bounds, to_idx(ev)).is_none() {
                        ok32 = false;
                        break;
                    }
                }
            }
            ok32
        };

    if can_use_4xi32
        && let Some(info) = vi.as_ref()
        && trip_count_provably_below(
            &view,
            info,
            plan.header,
            IvKind::Counted { iv_ty: Ty::I64 },
            plan.iv_init,
            plan.bound,
            16,
            128,
        )
    {
        return Ok(VectorizeOutput { func: view.to_func_in(alloc), stats: VectorizeStats::default() });
    }

    const UNROLL: usize = 4; // 4 x <4 x i32> = 16 iters/iter (fast) or 4 x <2 x i64> = 8 iters/iter!
    let (Some(vec_ty), Some(vec32_ty)) = (
        Ty::vector(crate::ty::Scalar::I64, 2),
        Ty::vector(crate::ty::Scalar::I32, 4),
    ) else {
        return Ok(VectorizeOutput { func: view.to_func_in(alloc), stats: VectorizeStats::default() });
    };
    let insert_at = to_idx(plan.preheader) + 1;
    let vpre_blk = to_u32(insert_at);
    let vloop_blk = to_u32(insert_at + 1);
    let vred_blk = to_u32(insert_at + 2);

    let mut blk_map: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        blk_map.push(if b < insert_at { to_u32(b) } else { to_u32(b + 3) });
    }
    let orig_hdr_new = at(&blk_map, to_idx(plan.header));

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));
        let term = range.end - 1;

        if b_u32 == plan.preheader {
            for inst in range.start..term {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
            let iv_init_new = at(&val_map, to_idx(plan.iv_init));
            let bound_new = at(&val_map, to_idx(plan.bound));
            let step_total = if can_use_4xi32 { 16u64 } else { 8u64 };
            let bound_limit = if can_use_4xi32 { U32_FAST_MAX } else { u64::from(u32::MAX) };
            let has_rem = push_icmp(&mut func, IntCond::Ult, iv_init_new, bound_new);
            let k_lim = push_iconst(&mut func, Ty::I64, bound_limit);
            let fits_u32 = push_icmp(&mut func, IntCond::Ule, bound_new, k_lim);
            let rem = push_bin(&mut func, Op::Sub, Flags::NONE, Ty::I64, bound_new, iv_init_new);
            let k_step = push_iconst(&mut func, Ty::I64, step_total);
            let enough = push_icmp(&mut func, IntCond::Uge, rem, k_step);
            let g0 = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, has_rem, fits_u32);
            let go_vec = push_bin(&mut func, Op::And, Flags::NONE, Ty::I1, g0, enough);

            let mut fb_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
            for &a in &plan.pre_args {
                fb_args.push(at(&val_map, to_idx(a)));
            }
            push_br(&mut func, go_vec, vpre_blk, &[], orig_hdr_new, &fb_args);

            let body_blk_id = at(view.extra, to_idx(at(view.data, view.terminator(Block(plan.header)))) + 1);
            if can_use_4xi32 {
                // --- `vpre` (16-wide `<4 x i32>` path with `Umulhi` + `uadalp.2d`) ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let k_mask = push_iconst(&mut func, Ty::I64, !15u64);
                let vcnt = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem, k_mask);
                let vend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init_new, vcnt);

                let i0_32 = push_inst(&mut func, Op::Trunc, Flags::NONE, Ty::I32, iv_init_new, 0);
                let k1_32 = push_iconst(&mut func, Ty::I32, 1);
                let k2_32 = push_iconst(&mut func, Ty::I32, 2);
                let k3_32 = push_iconst(&mut func, Ty::I32, 3);
                let i1_32 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I32, i0_32, k1_32);
                let i2_32 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I32, i0_32, k2_32);
                let i3_32 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I32, i0_32, k3_32);
                let mut vi0_init = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, i0_32, 0);
                for (lane, iv_lane) in [(1u32, i1_32), (2, i2_32), (3, i3_32)] {
                    let ins_at = to_u32(func.extra.len());
                    func.extra.push(vi0_init);
                    func.extra.push(iv_lane);
                    func.extra.push(lane);
                    vi0_init = push_inst(&mut func, Op::Insert, Flags::NONE, vec32_ty, ins_at, 0);
                }

                let k4_32 = push_iconst(&mut func, Ty::I32, 4);
                let v_off4 = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, k4_32, 0);
                let k_mask32 = push_iconst(&mut func, Ty::I64, u64::from(u32::MAX));
                let v_mask32 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k_mask32, 0);
                let k_sh32 = push_iconst(&mut func, Ty::I64, 32);
                let v_sh32 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k_sh32, 0);

                let mut div_splats: Vec<(u32, u32, Option<u32>), A> =
                    Vec::with_capacity_in(plan.div_sites.len(), alloc);
                for ds in &plan.div_sites {
                    let km = push_iconst(&mut func, Ty::I32, u64::from(ds.magic32));
                    let vm = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, km, 0);
                    let rem_sh = ds.rshift - 32;
                    let vs = if rem_sh > 0 {
                        let ks = push_iconst(&mut func, Ty::I32, u64::from(rem_sh));
                        Some(push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, ks, 0))
                    } else {
                        None
                    };
                    div_splats.push((ds.lshr_inst, vm, vs));
                }

                let mut inv_splats: Vec<(u32, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    if plan.div_sites.iter().any(|ds| ds.lshr_inst == bi) {
                        continue;
                    }
                    each(&view, to_idx(bi), |item| {
                        if let Item::Value(v) = item {
                            let vi = to_idx(v);
                            let vb = at(&inst_block, vi);
                            if vb != plan.header && vb != body_blk_id && inv_splats.iter().all(|&(old, _)| old != v) {
                                let t32 = if let Some(c) = const_u64(&view, vi) {
                                    push_iconst(&mut func, Ty::I32, c)
                                } else {
                                    push_inst(&mut func, Op::Trunc, Flags::NONE, Ty::I32, at(&val_map, vi), 0)
                                };
                                let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, t32, 0);
                                inv_splats.push((v, sp));
                            }
                        }
                    });
                }

                let mut vinit_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vinit_args.push(iv_init_new);
                vinit_args.push(vi0_init);
                let id_c = push_iconst(&mut func, Ty::I64, 0);
                for _ in &plan.reductions {
                    for _ in 0..UNROLL {
                        let vacc = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, id_c, 0);
                        vinit_args.push(vacc);
                    }
                }
                push_jump(&mut func, vloop_blk, &vinit_args);

                // --- `vloop`: 4 x `<4 x i32>` = 16 elements/iter via `Umulhi` + `uadalp.2d`! ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_v = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let vi0 = push_inst(&mut func, Op::Param, Flags::NONE, vec32_ty, 0, 0);
                let mut vacc_params: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    let a0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    vacc_params.push([a0, a1, a2, a3]);
                }

                let mut body_const_splats: Vec<(usize, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    let b_idx = to_idx(bi);
                    if matches!(view.op(b_idx), Op::Iconst | Op::IconstWide)
                        && let Some(cval) = const_u64(&view, b_idx)
                    {
                        let c32 = push_iconst(&mut func, Ty::I32, cval);
                        let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec32_ty, c32, 0);
                        body_const_splats.push((b_idx, sp));
                    }
                }

                let mut next_accs: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    next_accs.push([NONE; UNROLL]);
                }
                let mut vmap = filled(len, NONE, &alloc);
                let mut cur_vi = vi0;
                for u in 0..UNROLL {
                    let vi_u = if u == 0 {
                        cur_vi
                    } else {
                        cur_vi = push_bin(&mut func, Op::Add, Flags::NONE, vec32_ty, cur_vi, v_off4);
                        cur_vi
                    };
                    set_at(&mut vmap, to_idx(plan.iv_inst), vi_u);
                    for &(old, sp) in &inv_splats {
                        set_at(&mut vmap, to_idx(old), sp);
                    }
                    for &(b_idx, sp) in &body_const_splats {
                        set_at(&mut vmap, b_idx, sp);
                    }
                    for &bi in &plan.body_insts {
                        let b_idx = to_idx(bi);
                        if let Some(&(_, vm32, vsh_opt)) = div_splats.iter().find(|&&(li, _, _)| li == bi) {
                            let hi = push_bin(&mut func, Op::Umulhi, Flags::NONE, vec32_ty, vi_u, vm32);
                            let res = if let Some(vsh) = vsh_opt {
                                push_bin(&mut func, Op::Lshr, Flags::NONE, vec32_ty, hi, vsh)
                            } else {
                                hi
                            };
                            set_at(&mut vmap, b_idx, res);
                        } else {
                            let op = view.op(b_idx);
                            if !matches!(op, Op::Iconst | Op::IconstWide) {
                                let idx = to_idx(at(view.data, b_idx));
                                let (a, bv) = (to_idx(at(view.extra, idx)), to_idx(at(view.extra, idx + 1)));
                                let nv = push_bin(&mut func, op, Flags::NONE, vec32_ty, at(&vmap, a), at(&vmap, bv));
                                set_at(&mut vmap, b_idx, nv);
                            }
                        }
                    }
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        // Canonical `uadalp.2d` pattern: zero-extend & pairwise add `<4 x i32>` into `<2 x i64>`!
                        let elem_val = match red.input {
                            ReductionInput::Elem(ev) => ev,
                            ReductionInput::Fma { .. }
                            | ReductionInput::CountCond { .. }
                            | ReductionInput::Popcnt { .. }
                            | ReductionInput::OrderedFadd { .. } => NONE,
                        };
                        let e_vec = at(&vmap, to_idx(elem_val));
                        let bc = push_inst(&mut func, Op::Bitcast, Flags::NONE, vec_ty, e_vec, 0);
                        let lo = push_bin(&mut func, Op::And, Flags::NONE, vec_ty, bc, v_mask32);
                        let hi = push_bin(&mut func, Op::Lshr, Flags::NONE, vec_ty, bc, v_sh32);
                        let acc_u = vacc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                        let tmp = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, acc_u, lo);
                        let nv = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, tmp, hi);
                        if let Some(row) = next_accs.get_mut(ridx) {
                            set_at(row, u, nv);
                        }
                    }
                }

                let iv_next = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_v, k_step);
                let vi0_next = push_bin(&mut func, Op::Add, Flags::NONE, vec32_ty, cur_vi, v_off4);
                let mut vback_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vback_args.push(iv_next);
                vback_args.push(vi0_next);
                for row in &next_accs {
                    for &v in row {
                        vback_args.push(v);
                    }
                }
                let keep = push_icmp(&mut func, IntCond::Ult, iv_next, vend);
                push_br(&mut func, keep, vloop_blk, &vback_args, vred_blk, &vback_args);

                // --- `vred`: tree-reduce 4 `<2 x i64>` accumulators & jump to scalar tail ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_done = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let _vi_done = push_inst(&mut func, Op::Param, Flags::NONE, vec32_ty, 0, 0);
                let mut red_scalars: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for red in &plan.reductions {
                    let d0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let c01 = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, d0, d1);
                    let c23 = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, d2, d3);
                    let comb = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, c01, c23);
                    let r_lane = push_inst(&mut func, Op::ReduceAdd, Flags::NONE, Ty::I64, comb, 0);
                    red_scalars.push(push_bin(
                        &mut func,
                        Op::Add,
                        Flags::NONE,
                        Ty::I64,
                        at(&val_map, to_idx(red.init_val)),
                        r_lane,
                    ));
                }
                let mut tail_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
                for (pos, &pre_arg) in plan.pre_args.iter().enumerate() {
                    if pos == plan.iv_pos {
                        tail_args.push(iv_done);
                    } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                        tail_args.push(at(&red_scalars, ridx));
                    } else {
                        tail_args.push(at(&val_map, to_idx(pre_arg)));
                    }
                }
                push_jump(&mut func, orig_hdr_new, &tail_args);
            } else {
                // --- `vpre`: build initial `<2 x i64>` `[i0, i0+1]`, splat magic32 constants & masks ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let k_mask = push_iconst(&mut func, Ty::I64, !7u64);
                let vcnt = push_bin(&mut func, Op::And, Flags::NONE, Ty::I64, rem, k_mask);
                let vend = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init_new, vcnt);

                let k1 = push_iconst(&mut func, Ty::I64, 1);
                let iv_p1 = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_init_new, k1);
                let v_i0_s = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, iv_init_new, 0);
                let ins_at = to_u32(func.extra.len());
                func.extra.push(v_i0_s);
                func.extra.push(iv_p1);
                func.extra.push(1);
                let vi0_init = push_inst(&mut func, Op::Insert, Flags::NONE, vec_ty, ins_at, 0);

                let k2 = push_iconst(&mut func, Ty::I64, 2);
                let v_off2 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k2, 0);
                let v_mask32 = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, k_lim, 0);

                let mut div_splats: Vec<(u32, u32, u32), A> = Vec::with_capacity_in(plan.div_sites.len(), alloc);
                for ds in &plan.div_sites {
                    let km = push_iconst(&mut func, Ty::I64, u64::from(ds.magic32));
                    let vm = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, km, 0);
                    let ks = push_iconst(&mut func, Ty::I64, u64::from(ds.rshift));
                    let vs = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, ks, 0);
                    div_splats.push((ds.lshr_inst, vm, vs));
                }
                // Hoist any constant shifts / loop-invariant scalars used by `body_insts`.
                let mut inv_splats: Vec<(u32, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    if plan.div_sites.iter().any(|ds| ds.lshr_inst == bi) {
                        continue;
                    }
                    each(&view, to_idx(bi), |item| {
                        if let Item::Value(v) = item {
                            let vi = to_idx(v);
                            let vb = at(&inst_block, vi);
                            if vb != plan.header && vb != body_blk_id && inv_splats.iter().all(|&(old, _)| old != v) {
                                let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, at(&val_map, vi), 0);
                                inv_splats.push((v, sp));
                            }
                        }
                    });
                }

                let mut vinit_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vinit_args.push(iv_init_new);
                vinit_args.push(vi0_init);
                for red in &plan.reductions {
                    let id_c = push_iconst(&mut func, Ty::I64, if red.op == Op::And { u64::MAX } else { 0 });
                    for _ in 0..UNROLL {
                        let vacc = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, id_c, 0);
                        vinit_args.push(vacc);
                    }
                }
                push_jump(&mut func, vloop_blk, &vinit_args);

                // --- `vloop`: 4x `<2 x i64>` = 8 iterations/iter with `umull.2d` + `ushr.2d` (10 regs, 0 spills)! ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_v = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let vi0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                let mut vacc_params: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    let a0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let a3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    vacc_params.push([a0, a1, a2, a3]);
                }

                // Hoist any loop-body constants once at the top of `vloop` (constant splat).
                let mut body_const_splats: Vec<(usize, u32), A> = Vec::new_in(alloc);
                for &bi in &plan.body_insts {
                    let b_idx = to_idx(bi);
                    if matches!(view.op(b_idx), Op::Iconst | Op::IconstWide) {
                        let c_new = copy_inst_remapped_blocks(&view, b_idx, &val_map, &blk_map, &mut func);
                        let sp = push_inst(&mut func, Op::Splat, Flags::NONE, vec_ty, c_new, 0);
                        body_const_splats.push((b_idx, sp));
                    }
                }

                // Interleave lane-by-lane (`u = 0..4`) chaining `vi_{u+1} = vi_u + v_off2` so peak
                // live vector register count stays at ~10 (ZERO stack spills on AArch64!).
                let mut next_accs: Vec<[u32; UNROLL], A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for _ in 0..plan.reductions.len() {
                    next_accs.push([NONE; UNROLL]);
                }
                let mut vmap = filled(len, NONE, &alloc);
                let mut cur_vi = vi0;
                for u in 0..UNROLL {
                    let vi_u = if u == 0 {
                        cur_vi
                    } else {
                        cur_vi = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, cur_vi, v_off2);
                        cur_vi
                    };
                    set_at(&mut vmap, to_idx(plan.iv_inst), vi_u);
                    for &(old, sp) in &inv_splats {
                        set_at(&mut vmap, to_idx(old), sp);
                    }
                    for &(b_idx, sp) in &body_const_splats {
                        set_at(&mut vmap, b_idx, sp);
                    }
                    for &bi in &plan.body_insts {
                        let b_idx = to_idx(bi);
                        if let Some(&(_, vm32, vsh)) = div_splats.iter().find(|&&(li, _, _)| li == bi) {
                            let lo32 = push_bin(&mut func, Op::And, Flags::NONE, vec_ty, vi_u, v_mask32);
                            let prod = push_bin(&mut func, Op::Mul, Flags::NONE, vec_ty, lo32, vm32);
                            let res = push_bin(&mut func, Op::Lshr, Flags::NONE, vec_ty, prod, vsh);
                            set_at(&mut vmap, b_idx, res);
                        } else {
                            let op = view.op(b_idx);
                            if !matches!(op, Op::Iconst | Op::IconstWide) {
                                let idx = to_idx(at(view.data, b_idx));
                                let (a, bv) = (to_idx(at(view.extra, idx)), to_idx(at(view.extra, idx + 1)));
                                let nv = push_bin(&mut func, op, Flags::NONE, vec_ty, at(&vmap, a), at(&vmap, bv));
                                set_at(&mut vmap, b_idx, nv);
                            }
                        }
                    }
                    for (ridx, red) in plan.reductions.iter().enumerate() {
                        let elem_val = match red.input {
                            ReductionInput::Elem(ev) => ev,
                            ReductionInput::Fma { .. }
                            | ReductionInput::CountCond { .. }
                            | ReductionInput::Popcnt { .. }
                            | ReductionInput::OrderedFadd { .. } => NONE,
                        };
                        let acc_u = vacc_params.get(ridx).and_then(|row| row.get(u)).copied().unwrap_or(NONE);
                        let nv = push_bin(
                            &mut func,
                            red.op,
                            Flags::NONE,
                            vec_ty,
                            acc_u,
                            at(&vmap, to_idx(elem_val)),
                        );
                        if let Some(row) = next_accs.get_mut(ridx) {
                            set_at(row, u, nv);
                        }
                    }
                }

                let iv_next = push_bin(&mut func, Op::Add, Flags::NONE, Ty::I64, iv_v, k_step);
                let vi0_next = push_bin(&mut func, Op::Add, Flags::NONE, vec_ty, cur_vi, v_off2);
                let mut vback_args: Vec<u32, A> = Vec::with_capacity_in(2 + plan.reductions.len() * UNROLL, alloc);
                vback_args.push(iv_next);
                vback_args.push(vi0_next);
                for row in &next_accs {
                    for &v in row {
                        vback_args.push(v);
                    }
                }
                let keep = push_icmp(&mut func, IntCond::Ult, iv_next, vend);
                push_br(&mut func, keep, vloop_blk, &vback_args, vred_blk, &vback_args);

                // --- `vred`: tree-reduce 4 `<2 x i64>` accumulators & jump to scalar tail ---
                func.block_starts.push(to_u32(func.ops.len()));
                func.block_kinds.push(block_kind_byte(BlockKind::Normal));
                let iv_done = push_inst(&mut func, Op::Param, Flags::NONE, Ty::I64, 0, 0);
                let _vi_done = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                let mut red_scalars: Vec<u32, A> = Vec::with_capacity_in(plan.reductions.len(), alloc);
                for red in &plan.reductions {
                    let d0 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d1 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d2 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let d3 = push_inst(&mut func, Op::Param, Flags::NONE, vec_ty, 0, 0);
                    let c01 = push_bin(&mut func, red.op, Flags::NONE, vec_ty, d0, d1);
                    let c23 = push_bin(&mut func, red.op, Flags::NONE, vec_ty, d2, d3);
                    let comb = push_bin(&mut func, red.op, Flags::NONE, vec_ty, c01, c23);
                    let rop = match red.op {
                        Op::Add => Op::ReduceAdd,
                        Op::Xor => Op::ReduceXor,
                        Op::Or => Op::ReduceOr,
                        Op::And => Op::ReduceAnd,
                        _ => {
                            return Err(SimplifyError::Refused {
                                inst: to_idx(red.update_inst),
                                what: "unsupported vector reduction op",
                            });
                        }
                    };
                    let r_lane = push_inst(&mut func, rop, Flags::NONE, Ty::I64, comb, 0);
                    red_scalars.push(push_bin(
                        &mut func,
                        red.op,
                        Flags::NONE,
                        Ty::I64,
                        at(&val_map, to_idx(red.init_val)),
                        r_lane,
                    ));
                }
                let mut tail_args: Vec<u32, A> = Vec::with_capacity_in(plan.pre_args.len(), alloc);
                for (pos, &pre_arg) in plan.pre_args.iter().enumerate() {
                    if pos == plan.iv_pos {
                        tail_args.push(iv_done);
                    } else if let Some(ridx) = plan.reductions.iter().position(|r| r.param_pos == pos) {
                        tail_args.push(at(&red_scalars, ridx));
                    } else {
                        tail_args.push(at(&val_map, to_idx(pre_arg)));
                    }
                }
                push_jump(&mut func, orig_hdr_new, &tail_args);
            }
        } else {
            for inst in range {
                let mapped = copy_inst_remapped_blocks(&view, inst, &val_map, &blk_map, &mut func);
                set_at(&mut val_map, inst, mapped);
            }
        }
    }

    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    Ok(VectorizeOutput {
        func,
        stats: VectorizeStats { loops_vectorized: 1 },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use super::*;
    use crate::{
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::Conv,
    };

    #[test]
    fn auto_vectorizes_u64_sum_loop_into_2x_unrolled_simd_and_verifies() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));

        b.begin(body);
        let k3 = b.iconst(Ty::I64, 3);
        let off = b.binary(Op::Shl, Flags::NONE, i, k3);
        let ep = b.ptr_add(Flags::NONE, ptr, off);
        let v = b.load(Ty::I64, Flags::NONE, ep, 0, MemAttrs::aligned(8));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, v);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        let has_reduce_add = (0..v.len()).any(|i| v.op(i) == Op::ReduceAdd);
        let has_vec_load = (0..v.len()).any(|i| v.op(i) == Op::Load && Ty(v.tys[i]).is_vector());
        assert!(has_reduce_add && has_vec_load, "expected 128-bit vector loads and ReduceAdd");
    }

    #[test]
    fn auto_vectorizes_u32_map_store_loop_with_alias_guard_and_verifies() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32)],
            &[],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let key = b.param(Ty::I32);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));

        b.begin(body);
        let k2 = b.iconst(Ty::I64, 2);
        let off = b.binary(Op::Shl, Flags::NONE, i, k2);
        let sp = b.ptr_add(Flags::NONE, src, off);
        let dp = b.ptr_add(Flags::NONE, dst, off);
        let val = b.load(Ty::I32, Flags::NONE, sp, 0, MemAttrs::aligned(4));
        let xored = b.binary(Op::Xor, Flags::NONE, val, key);
        b.store(Flags::NONE, xored, dp, 0, MemAttrs::aligned(4));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));

        b.begin(exit);
        b.ret(&[]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        let vec_stores = (0..v.len())
            .filter(|&i| v.op(i) == Op::Store && Ty(v.tys[v.extra[v.data[i] as usize] as usize]).is_vector())
            .count();
        assert_eq!(vec_stores, 4, "expected 4x-unrolled adjacent 128-bit vector stores (64B/iter)");
        // Verify compact wraparound-safe unsigned range alias check in `entry` (`sub` + `neg` + `umin`, zero `add`s).
        assert_eq!(v.block(Block(0)).filter(|&k| v.op(k) == Op::Sub).count(), 1);
        assert_eq!(v.block(Block(0)).filter(|&k| v.op(k) == Op::Neg).count(), 1);
        assert_eq!(v.block(Block(0)).filter(|&k| v.op(k) == Op::Umin).count(), 1);
        assert_eq!(v.block(Block(0)).filter(|&k| v.op(k) == Op::Add).count(), 0);
    }

    #[test]
    fn vectorize_iv_div_loops_16wide_uadalp_matches_evaluator() {
        use crate::{
            egraph::{RegBudget, sota_optimize_fixpoint},
            eval::{self, Outcome, Val},
        };
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((header, &[z, z]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        b.begin(body);
        let c10 = b.iconst(Ty::I64, 10);
        let c1 = b.iconst(Ty::I64, 1);
        let q = b.binary(Op::Udiv, Flags::NONE, i, c10);
        let r = b.binary(Op::Urem, Flags::NONE, i, c10);
        let t = b.binary(Op::Add, Flags::NONE, q, r);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, t);
        let i2 = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        b.ret(&[acc]);
        let func = b.finish_checked().unwrap();

        let opt = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 4, |_| 1, TestHeap).unwrap();
        let vec = vectorize_iv_div_loops(opt.func.view(), TestHeap).unwrap();
        assert_eq!(vec.stats.loops_vectorized, 1);
        let v = vec.func.view();
        assert!(
            (0..v.len()).any(|idx| v.op(idx) == Op::Umulhi && Ty(v.tys[idx]).is_vector()),
            "expected `<4 x i32>` vector `Op::Umulhi` in vectorized div10 loop"
        );

        for &nv in &[0u128, 1, 7, 8, 15, 16, 17, 31, 32, 33, 100] {
            let Outcome::Returned(orig) = eval::run_in(func.view(), &[Val::Int(nv)], 20_000, TestHeap).unwrap() else {
                panic!("orig eval");
            };
            let Outcome::Returned(got) = eval::run_in(vec.func.view(), &[Val::Int(nv)], 20_000, TestHeap).unwrap() else {
                panic!("vec eval");
            };
            assert_eq!(orig, got, "vectorize_iv_div_loops mismatch at n={nv}");
        }
    }

    #[test]
    fn auto_vectorizes_fma_dot_f32_4x_unrolled_and_verifies() {
        use crate::egraph::{RegBudget, sota_optimize_fixpoint};

        // 1. Contracted `fadd(acc, fmul(a, b))` -> `Op::Fma` via egraph -> 4x-unrolled vector FMA reduction.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::F32)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let pa = b.param(Ty::PTR);
        let pb = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let fz = b.fconst(Ty::F32, 0);
        b.jump((header, &[zero, fz]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::F32);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let two = b.iconst(Ty::I64, 2);
        let off = b.binary(Op::Shl, Flags::NONE, i, two);
        let p1 = b.ptr_add(Flags::NONE, pa, off);
        let p2 = b.ptr_add(Flags::NONE, pb, off);
        let va = b.load(Ty::F32, Flags::NONE, p1, 0, MemAttrs::aligned(4));
        let vb = b.load(Ty::F32, Flags::NONE, p2, 0, MemAttrs::aligned(4));
        let prod = b.binary(Op::Fmul, Flags::CONTRACT | Flags::REASSOC, va, vb);
        let acc2 = b.binary(Op::Fadd, Flags::CONTRACT | Flags::REASSOC, acc, prod);
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::F32);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let opt = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 4, |_| 1, TestHeap).unwrap();
        assert!(
            (0..opt.func.view().len()).any(|idx| opt.func.view().op(idx) == Op::Fma),
            "expected egraph to contract fadd(fmul) into Op::Fma"
        );
        let out = auto_vectorize_loops(opt.func.view(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        let vec_fmas = (0..v.len()).filter(|&k| v.op(k) == Op::Fma && Ty(v.tys[k]).is_vector()).count();
        let vec_loads = (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]).is_vector()).count();
        let has_red = (0..v.len()).any(|k| v.op(k) == Op::ReduceFadd);
        assert_eq!(vec_fmas, 4, "expected 4x-unrolled vector FMA accumulators");
        assert_eq!(vec_loads, 8, "expected 4 adjacent vector loads per input array");
        assert!(has_red, "expected ReduceFadd horizontal epilogue");
    }

    #[test]
    fn auto_vectorizes_fma_saxpy_f32_and_matmul_inner_loop_and_verifies() {
        use crate::egraph::{RegBudget, sota_optimize_fixpoint};

        // 1. SAXPY: `y[i] = fma(alpha, x[i], y[i])` (in-place read-modify-write + invariant scalar).
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::F32), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let py = b.param(Ty::PTR);
        let px = b.param(Ty::PTR);
        let alpha = b.param(Ty::F32);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));
        b.begin(body);
        let two = b.iconst(Ty::I64, 2);
        let off = b.binary(Op::Shl, Flags::NONE, i, two);
        let p_x = b.ptr_add(Flags::NONE, px, off);
        let p_y = b.ptr_add(Flags::NONE, py, off);
        let vx = b.load(Ty::F32, Flags::NONE, p_x, 0, MemAttrs::aligned(4));
        let vy = b.load(Ty::F32, Flags::NONE, p_y, 0, MemAttrs::aligned(4));
        let res = b.ternary(Op::Fma, Flags::REASSOC, alpha, vx, vy);
        b.store(Flags::NONE, res, p_y, 0, MemAttrs::aligned(4));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));
        b.begin(exit);
        b.ret(&[]);
        let saxpy = b.finish_checked().unwrap();

        let saxpy_out = auto_vectorize_loops(saxpy.view(), TestHeap).unwrap();
        verify(saxpy_out.func.view()).unwrap();
        assert_eq!(saxpy_out.stats.loops_vectorized, 1);
        let sv = saxpy_out.func.view();
        assert_eq!(
            (0..sv.len()).filter(|&k| sv.op(k) == Op::Fma && Ty(sv.tys[k]).is_vector()).count(),
            4,
            "expected 4x-unrolled vector FMA in saxpy"
        );
        assert_eq!(
            (0..sv.len())
                .filter(|&k| sv.op(k) == Op::Store && Ty(sv.tys[sv.extra[sv.data[k] as usize] as usize]).is_vector())
                .count(),
            4,
            "expected 4 contiguous vector stores in saxpy"
        );

        // 2. Matmul ikj inner loop: `c_row`, `b_row`, `aik` defined in outer loop blocks (not entry!).
        let mut m = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let msig = m.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[],
        );
        m.set_signature(msig);
        let [entry, h_i, b_i, h_k, b_k, h_j, b_j, end_k, end_i, exit] =
            [(); 10].map(|_| m.create_block(BlockKind::Normal));
        m.begin(entry);
        let pc = m.param(Ty::PTR);
        let pa = m.param(Ty::PTR);
        let pb = m.param(Ty::PTR);
        let mn = m.param(Ty::I64);
        let mzero = m.iconst(Ty::I64, 0);
        let mone = m.iconst(Ty::I64, 1);
        let mtwo = m.iconst(Ty::I64, 2);
        m.jump((h_i, &[mzero]));
        m.begin(h_i);
        let mi = m.param(Ty::I64);
        let ki = m.icmp(IntCond::Ult, mi, mn);
        m.br(Flags::NONE, ki, (b_i, &[]), (exit, &[]));
        m.begin(b_i);
        let i_n = m.binary(Op::Mul, Flags::NONE, mi, mn);
        let i_bytes = m.binary(Op::Shl, Flags::NONE, i_n, mtwo);
        let c_row = m.ptr_add(Flags::NONE, pc, i_bytes);
        let a_row = m.ptr_add(Flags::NONE, pa, i_bytes);
        m.jump((h_k, &[mzero]));
        m.begin(h_k);
        let mk = m.param(Ty::I64);
        let kk = m.icmp(IntCond::Ult, mk, mn);
        m.br(Flags::NONE, kk, (b_k, &[]), (end_i, &[]));
        m.begin(b_k);
        let k_bytes = m.binary(Op::Shl, Flags::NONE, mk, mtwo);
        let a_ptr = m.ptr_add(Flags::NONE, a_row, k_bytes);
        let aik = m.load(Ty::F32, Flags::NONE, a_ptr, 0, MemAttrs::aligned(4));
        let k_n = m.binary(Op::Mul, Flags::NONE, mk, mn);
        let kn_bytes = m.binary(Op::Shl, Flags::NONE, k_n, mtwo);
        let b_row = m.ptr_add(Flags::NONE, pb, kn_bytes);
        m.jump((h_j, &[mzero]));
        m.begin(h_j);
        let mj = m.param(Ty::I64);
        let kj = m.icmp(IntCond::Ult, mj, mn);
        m.br(Flags::NONE, kj, (b_j, &[]), (end_k, &[]));
        m.begin(b_j);
        let j_bytes = m.binary(Op::Shl, Flags::NONE, mj, mtwo);
        let c_ptr = m.ptr_add(Flags::NONE, c_row, j_bytes);
        let b_ptr = m.ptr_add(Flags::NONE, b_row, j_bytes);
        let cv = m.load(Ty::F32, Flags::NONE, c_ptr, 0, MemAttrs::aligned(4));
        let bv = m.load(Ty::F32, Flags::NONE, b_ptr, 0, MemAttrs::aligned(4));
        let prod = m.binary(Op::Fmul, Flags::CONTRACT | Flags::REASSOC, aik, bv);
        let cv2 = m.binary(Op::Fadd, Flags::CONTRACT | Flags::REASSOC, cv, prod);
        m.store(Flags::NONE, cv2, c_ptr, 0, MemAttrs::aligned(4));
        let j2 = m.binary(Op::Add, Flags::NONE, mj, mone);
        m.jump((h_j, &[j2]));
        m.begin(end_k);
        let k2 = m.binary(Op::Add, Flags::NONE, mk, mone);
        m.jump((h_k, &[k2]));
        m.begin(end_i);
        let mi2 = m.binary(Op::Add, Flags::NONE, mi, mone);
        m.jump((h_i, &[mi2]));
        m.begin(exit);
        m.ret(&[]);
        let matmul = m.finish_checked().unwrap();

        let mat_opt = sota_optimize_fixpoint(matmul.view(), RegBudget::AARCH64, 4, |_| 1, TestHeap).unwrap();
        let mat_out = auto_vectorize_loops(mat_opt.func.view(), TestHeap).unwrap();
        verify(mat_out.func.view()).unwrap();
        assert_eq!(mat_out.stats.loops_vectorized, 1);
        let mv = mat_out.func.view();
        assert_eq!(
            (0..mv.len()).filter(|&k| mv.op(k) == Op::Fma && Ty(mv.tys[k]).is_vector()).count(),
            4,
            "expected 4x-unrolled vector FMA in matmul inner loop"
        );
        assert_eq!(
            (0..mv.len()).filter(|&k| mv.op(k) == Op::Load && Ty(mv.tys[k]).is_vector()).count(),
            8,
            "expected 8 vector loads (4 C + 4 B) in matmul inner loop"
        );
    }

    #[test]
    fn auto_vectorizes_smax_smin_relu_clamp_and_minmax_reductions_and_verifies() {
        // 1. `relu_clamp_i32`: `dst[i] = smin(smax(src[i], 0), hi)` -> 4x-unrolled `<4 x i32>`!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I32), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let hi = b.param(Ty::I32);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let z32 = b.iconst(Ty::I32, 0);
        b.jump((header, &[zero]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));
        b.begin(body);
        let two = b.iconst(Ty::I64, 2);
        let off = b.binary(Op::Shl, Flags::NONE, i, two);
        let ps = b.ptr_add(Flags::NONE, src, off);
        let pd = b.ptr_add(Flags::NONE, dst, off);
        let val = b.load(Ty::I32, Flags::NONE, ps, 0, MemAttrs::aligned(4));
        let v1 = b.binary(Op::Smax, Flags::NONE, val, z32);
        let v2 = b.binary(Op::Smin, Flags::NONE, v1, hi);
        b.store(Flags::NONE, v2, pd, 0, MemAttrs::aligned(4));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));
        b.begin(exit);
        b.ret(&[]);
        let clamp = b.finish_checked().unwrap();

        let clamp_out = auto_vectorize_loops(clamp.view(), TestHeap).unwrap();
        verify(clamp_out.func.view()).unwrap();
        assert_eq!(clamp_out.stats.loops_vectorized, 1);
        let cv = clamp_out.func.view();
        assert_eq!((0..cv.len()).filter(|&k| cv.op(k) == Op::Smax && Ty(cv.tys[k]).is_vector()).count(), 4);
        assert_eq!((0..cv.len()).filter(|&k| cv.op(k) == Op::Smin && Ty(cv.tys[k]).is_vector()).count(), 4);
        assert_eq!(
            (0..cv.len())
                .filter(|&k| cv.op(k) == Op::Store && Ty(cv.tys[cv.extra[cv.data[k] as usize] as usize]).is_vector())
                .count(),
            4
        );

        // 2. `Smax` reduction on `i32` -> 4x-unrolled vector `Smax` + `ReduceSmax`.
        let mut r = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let rsig = r.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32)],
            &[Param::of(Ty::I32)],
        );
        r.set_signature(rsig);
        let [entry, header, body, exit] = [(); 4].map(|_| r.create_block(BlockKind::Normal));
        r.begin(entry);
        let rptr = r.param(Ty::PTR);
        let rn = r.param(Ty::I64);
        let init = r.param(Ty::I32);
        let rzero = r.iconst(Ty::I64, 0);
        r.jump((header, &[rzero, init]));
        r.begin(header);
        let ri = r.param(Ty::I64);
        let racc = r.param(Ty::I32);
        let rkeep = r.icmp(IntCond::Ult, ri, rn);
        r.br(Flags::NONE, rkeep, (body, &[]), (exit, &[racc]));
        r.begin(body);
        let rtwo = r.iconst(Ty::I64, 2);
        let roff = r.binary(Op::Shl, Flags::NONE, ri, rtwo);
        let rp = r.ptr_add(Flags::NONE, rptr, roff);
        let rv = r.load(Ty::I32, Flags::NONE, rp, 0, MemAttrs::aligned(4));
        let racc2 = r.binary(Op::Smax, Flags::NONE, racc, rv);
        let rone = r.iconst(Ty::I64, 1);
        let ri2 = r.binary(Op::Add, Flags::NONE, ri, rone);
        r.jump((header, &[ri2, racc2]));
        r.begin(exit);
        let rres = r.param(Ty::I32);
        r.ret(&[rres]);
        let smax_red = r.finish_checked().unwrap();

        let smax_out = auto_vectorize_loops(smax_red.view(), TestHeap).unwrap();
        verify(smax_out.func.view()).unwrap();
        assert_eq!(smax_out.stats.loops_vectorized, 1);
        let rv = smax_out.func.view();
        assert!((0..rv.len()).any(|k| rv.op(k) == Op::ReduceSmax), "expected ReduceSmax horizontal reduction");

        // 3. `Smin` on `i64` must NOT vectorize (AArch64 NEON has no `smin.2d`).
        let mut b64 = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig64 = b64.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[],
        );
        b64.set_signature(sig64);
        let [entry, header, body, exit] = [(); 4].map(|_| b64.create_block(BlockKind::Normal));
        b64.begin(entry);
        let d64 = b64.param(Ty::PTR);
        let s64 = b64.param(Ty::PTR);
        let hi64 = b64.param(Ty::I64);
        let n64 = b64.param(Ty::I64);
        let z64 = b64.iconst(Ty::I64, 0);
        b64.jump((header, &[z64]));
        b64.begin(header);
        let i64_v = b64.param(Ty::I64);
        let k64 = b64.icmp(IntCond::Ult, i64_v, n64);
        b64.br(Flags::NONE, k64, (body, &[]), (exit, &[]));
        b64.begin(body);
        let sh3 = b64.iconst(Ty::I64, 3);
        let off64 = b64.binary(Op::Shl, Flags::NONE, i64_v, sh3);
        let sp64 = b64.ptr_add(Flags::NONE, s64, off64);
        let dp64 = b64.ptr_add(Flags::NONE, d64, off64);
        let v64 = b64.load(Ty::I64, Flags::NONE, sp64, 0, MemAttrs::aligned(8));
        let m64 = b64.binary(Op::Smin, Flags::NONE, v64, hi64);
        b64.store(Flags::NONE, m64, dp64, 0, MemAttrs::aligned(8));
        let o64 = b64.iconst(Ty::I64, 1);
        let i64_n = b64.binary(Op::Add, Flags::NONE, i64_v, o64);
        b64.jump((header, &[i64_n]));
        b64.begin(exit);
        b64.ret(&[]);
        let clamp64 = b64.finish_checked().unwrap();

        let clamp64_out = auto_vectorize_loops(clamp64.view(), TestHeap).unwrap();
        assert_eq!(clamp64_out.stats.loops_vectorized, 0, "i64 Smin must not vectorize on NEON");
    }

    #[test]
    fn auto_vectorizes_large_body_with_2x_unroll_fallback_and_verifies() {
        // 3 loads (`loads.len() > 2`) drops from 4x to 2x unroll to keep register pressure 0-spill.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR),
                Param::of(Ty::PTR),
                Param::of(Ty::PTR),
                Param::of(Ty::PTR),
                Param::of(Ty::I64),
            ],
            &[],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let a = b.param(Ty::PTR);
        let bp = b.param(Ty::PTR);
        let c = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));
        b.begin(body);
        let two = b.iconst(Ty::I64, 2);
        let off = b.binary(Op::Shl, Flags::NONE, i, two);
        let pa = b.ptr_add(Flags::NONE, a, off);
        let pb = b.ptr_add(Flags::NONE, bp, off);
        let pc = b.ptr_add(Flags::NONE, c, off);
        let pd = b.ptr_add(Flags::NONE, dst, off);
        let va = b.load(Ty::I32, Flags::NONE, pa, 0, MemAttrs::aligned(4));
        let vb = b.load(Ty::I32, Flags::NONE, pb, 0, MemAttrs::aligned(4));
        let vc = b.load(Ty::I32, Flags::NONE, pc, 0, MemAttrs::aligned(4));
        let s1 = b.binary(Op::Add, Flags::NONE, va, vb);
        let s2 = b.binary(Op::Add, Flags::NONE, s1, vc);
        b.store(Flags::NONE, s2, pd, 0, MemAttrs::aligned(4));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));
        b.begin(exit);
        b.ret(&[]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        let vec_loads = (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]).is_vector()).count();
        let vec_stores = (0..v.len())
            .filter(|&k| v.op(k) == Op::Store && Ty(v.tys[v.extra[v.data[k] as usize] as usize]).is_vector())
            .count();
        assert_eq!(vec_loads, 6, "expected 2x unroll (2 loads x 3 arrays)");
        assert_eq!(vec_stores, 2, "expected 2x unroll (2 adjacent stores)");
    }

    #[test]
    fn pointer_induction_dot_i32_and_nonzero_iv_init_matches_evaluator() {
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
        };

        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let slot_a = b.slot(36 * 4, 4, SlotKind::Temp);
        let slot_b = b.slot(36 * 4, 4, SlotKind::Temp);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let start = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let pa = b.slot_addr(slot_a);
        let pb = b.slot_addr(slot_b);
        for k in 0..36u32 {
            let va = b.iconst(Ty::I32, u128::from(k.wrapping_mul(7).wrapping_add(3)));
            let vb = b.iconst(Ty::I32, u128::from(k.wrapping_mul(11).wrapping_sub(5)));
            b.store(Flags::NONE, va, pa, (k * 4) as i32, MemAttrs::aligned(2));
            b.store(Flags::NONE, vb, pb, (k * 4) as i32, MemAttrs::aligned(2));
        }
        let z32 = b.iconst(Ty::I32, 0);
        let two = b.iconst(Ty::I64, 2);
        let one = b.iconst(Ty::I64, 1);
        b.jump((header, &[start, z32]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I32);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, two);
        let p1 = b.ptr_add(Flags::NONE, pa, off);
        let p2 = b.ptr_add(Flags::NONE, pb, off);
        let a_v = b.load(Ty::I32, Flags::NONE, p1, 0, MemAttrs::aligned(2));
        let b_v = b.load(Ty::I32, Flags::NONE, p2, 0, MemAttrs::aligned(2));
        let prod = b.binary(Op::Mul, Flags::NONE, a_v, b_v);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, prod);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I32);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        // `vec_loop` is block 2: carries 2 running `Ty::PTR` params + 4 `<4 x i32>` accumulators = 6 params,
        // ZERO `Op::Shl` instructions, and 2 `Op::PtrAdd` pointer bumps!
        let vloop_params = v.params(Block(2));
        assert_eq!(vloop_params.len(), 6);
        assert_eq!(Ty(v.tys[vloop_params.start]), Ty::PTR);
        assert_eq!(Ty(v.tys[vloop_params.start + 1]), Ty::PTR);
        assert_eq!(v.block(Block(2)).filter(|&k| v.op(k) == Op::Shl).count(), 0);
        assert_eq!(v.block(Block(2)).filter(|&k| v.op(k) == Op::PtrAdd).count(), 2);

        assert_eq!(v.block(Block(2)).filter(|&k| v.op(k) == Op::Load).count(), 8);
        // `eval::run_in` does not yet implement vector `Op::Load`, so verify exact evaluator
        // equivalence on all sub-vector boundary cases (`nv - s < 16`, including nonzero `start`)!
        for &(s, nv) in &[(0u128, 0u128), (0, 1), (0, 3), (0, 15), (3, 3), (3, 18)] {
            let args = [Val::Int(s), Val::Int(nv)];
            let Outcome::Returned(orig) = eval::run_in(func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("orig dot_i32 eval");
            };
            let Outcome::Returned(got) = eval::run_in(out.func.view(), &args, 50_000, TestHeap).unwrap() else {
                panic!("vec dot_i32 eval");
            };
            assert_eq!(orig, got, "dot_i32 pointer-induction mismatch at start={s}, n={nv}");
        }
    }

    #[test]
    fn pointer_induction_saxpy_and_row_off_share_single_stream_per_base() {
        // 1. In-place `saxpy_f32`: load(x), load(y), store(y) -> ONLY 2 `Ty::PTR` params in `vec_loop`!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::F32), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let py = b.param(Ty::PTR);
        let px = b.param(Ty::PTR);
        let alpha = b.param(Ty::F32);
        let row_off = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));
        b.begin(header);
        let i = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));
        b.begin(body);
        let two = b.iconst(Ty::I64, 2);
        let idx_2d = b.binary(Op::Add, Flags::NONE, row_off, i);
        let off = b.binary(Op::Shl, Flags::NONE, idx_2d, two);
        let p_x = b.ptr_add(Flags::NONE, px, off);
        let p_y = b.ptr_add(Flags::NONE, py, off);
        let vx = b.load(Ty::F32, Flags::NONE, p_x, 0, MemAttrs::aligned(2));
        let vy = b.load(Ty::F32, Flags::NONE, p_y, 0, MemAttrs::aligned(2));
        let res = b.ternary(Op::Fma, Flags::REASSOC, alpha, vx, vy);
        b.store(Flags::NONE, res, p_y, 0, MemAttrs::aligned(2));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[i2]));
        b.begin(exit);
        b.ret(&[]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        // `vec_loop` (block 2) carries ONLY 2 `Ty::PTR` params (load(y) & store(y) share ONE stream!)
        // and ZERO `Op::Shl` inside the loop!
        let vloop_params = v.params(Block(2));
        assert_eq!(vloop_params.len(), 2);
        assert_eq!(Ty(v.tys[vloop_params.start]), Ty::PTR);
        assert_eq!(Ty(v.tys[vloop_params.start + 1]), Ty::PTR);
        assert_eq!(v.block(Block(2)).filter(|&k| v.op(k) == Op::Shl).count(), 0);
        assert_eq!(v.block(Block(2)).filter(|&k| v.op(k) == Op::PtrAdd).count(), 2);
        // `vec_reduce` (block 3) needs 0 params (uses `vec_end` from `vec_pre`).
        assert_eq!(v.params(Block(3)).len(), 0);
    }

    #[test]
    fn auto_vectorizes_count_lt_u32_pointer_iv_and_counted_iv_widening_u64() {
        use crate::ty::Scalar;
        let v4i32 = Ty::vector(Scalar::I32, 4).unwrap();
        let v4i1 = Ty::vector(Scalar::I1, 4).unwrap();

        // 1. Pointer-IV `count_lt_u32`: `for &x in xs { if x < thresh { c += 1; } }` with `Select(cond, 1u64, 0u64)`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I32)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let end = b.param(Ty::PTR);
        let thresh = b.param(Ty::I32);
        let z64 = b.iconst(Ty::I64, 0);
        let one64 = b.iconst(Ty::I64, 1);
        let four = b.iconst(Ty::I64, 4);
        b.jump((header, &[ptr, z64]));

        b.begin(header);
        let p = b.param(Ty::PTR);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ne, p, end);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let x = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
        let cond = b.icmp(IntCond::Ult, x, thresh);
        let inc = b.select(Flags::NONE, cond, one64, z64);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, inc);
        let p2 = b.ptr_add(Flags::NONE, p, four);
        b.jump((header, &[p2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func_ptr = b.finish_checked().unwrap();

        let out_ptr = auto_vectorize_loops(func_ptr.view(), TestHeap).unwrap();
        verify(out_ptr.func.view()).unwrap();
        assert_eq!(out_ptr.stats.loops_vectorized, 1);
        let vp = out_ptr.func.view();
        assert_eq!(
            (0..vp.len()).filter(|&k| vp.op(k) == Op::Load && Ty(vp.tys[k]) == v4i32).count(),
            4,
            "expected 4x-unrolled <4 x i32> loads (16 u32s / iter)"
        );
        assert_eq!(
            (0..vp.len()).filter(|&k| vp.op(k) == Op::Icmp && Ty(vp.tys[k]) == v4i1).count(),
            4,
            "expected 4x-unrolled <4 x i1> NEON cmhi masks"
        );
        assert_eq!(
            (0..vp.len()).filter(|&k| vp.op(k) == Op::Select && Ty(vp.tys[k]) == v4i32).count(),
            4,
            "expected 4x-unrolled zero-cost Select(mask, -1, 0)"
        );
        assert_eq!(
            (0..vp.len()).filter(|&k| vp.op(k) == Op::Sub && Ty(vp.tys[k]) == v4i32).count(),
            4,
            "expected 4x-unrolled sub.4s mask accumulators"
        );
        assert!((0..vp.len()).any(|k| vp.op(k) == Op::ReduceAdd && Ty(vp.tys[k]) == Ty::I32));
        assert!((0..vp.len()).any(|k| vp.op(k) == Op::Zext && Ty(vp.tys[k]) == Ty::I64));

        // 2. Counted-IV `count_lt_u32`: `for i in 0..n` with `Zext(Icmp(Ult, x, thresh) -> i64)`.
        let mut c = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let csig = c.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32)],
            &[Param::of(Ty::I64)],
        );
        c.set_signature(csig);
        let [centry, cheader, cbody, cexit] = [(); 4].map(|_| c.create_block(BlockKind::Normal));
        c.begin(centry);
        let cptr = c.param(Ty::PTR);
        let cn = c.param(Ty::I64);
        let cthresh = c.param(Ty::I32);
        let cz = c.iconst(Ty::I64, 0);
        let cone = c.iconst(Ty::I64, 1);
        let ctwo = c.iconst(Ty::I64, 2);
        c.jump((cheader, &[cz, cz]));

        c.begin(cheader);
        let ci = c.param(Ty::I64);
        let cacc = c.param(Ty::I64);
        let ckeep = c.icmp(IntCond::Ult, ci, cn);
        c.br(Flags::NONE, ckeep, (cbody, &[]), (cexit, &[cacc]));

        c.begin(cbody);
        let coff = c.binary(Op::Shl, Flags::NONE, ci, ctwo);
        let cp = c.ptr_add(Flags::NONE, cptr, coff);
        let cx = c.load(Ty::I32, Flags::NONE, cp, 0, MemAttrs::aligned(2));
        let ccond = c.icmp(IntCond::Ult, cx, cthresh);
        let cinc = c.convert(Op::Zext, Flags::NONE, Ty::I64, ccond);
        let cacc2 = c.binary(Op::Add, Flags::NONE, cacc, cinc);
        let ci2 = c.binary(Op::Add, Flags::NONE, ci, cone);
        c.jump((cheader, &[ci2, cacc2]));

        c.begin(cexit);
        let cres = c.param(Ty::I64);
        c.ret(&[cres]);
        let func_cnt = c.finish_checked().unwrap();

        let out_cnt = auto_vectorize_loops(func_cnt.view(), TestHeap).unwrap();
        verify(out_cnt.func.view()).unwrap();
        assert_eq!(out_cnt.stats.loops_vectorized, 1);
        let vc = out_cnt.func.view();
        assert_eq!((0..vc.len()).filter(|&k| vc.op(k) == Op::Sub && Ty(vc.tys[k]) == v4i32).count(), 4);
    }

    #[test]
    fn auto_vectorizes_sum_u64_pointer_iv_4x_unrolled_and_verifies() {
        use crate::ty::Scalar;
        let v2i64 = Ty::vector(Scalar::I64, 2).unwrap();

        // Pointer-IV `sum_u64` with `Eq` swapped exit (`if p == end { break }`), `p += 8`.
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let end = b.param(Ty::PTR);
        let z64 = b.iconst(Ty::I64, 0);
        let eight = b.iconst(Ty::I64, 8);
        b.jump((header, &[ptr, z64]));

        b.begin(header);
        let p = b.param(Ty::PTR);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Eq, p, end);
        b.br(Flags::NONE, done, (exit, &[acc]), (body, &[]));

        b.begin(body);
        let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, x);
        let p2 = b.ptr_add(Flags::NONE, p, eight);
        b.jump((header, &[p2, acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(out.stats.loops_vectorized, 1);
        let v = out.func.view();
        assert_eq!(
            (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v2i64).count(),
            4,
            "expected 4x-unrolled <2 x i64> loads (8 u64s / iter = 64B/iter)"
        );
        assert!(
            (0..v.len()).filter(|&k| v.op(k) == Op::Add && Ty(v.tys[k]) == v2i64).count() >= 4,
            "expected 4 independent <2 x i64> accumulators"
        );
        assert!((0..v.len()).any(|k| v.op(k) == Op::ReduceAdd && Ty(v.tys[k]) == Ty::I64));
    }

    #[test]
    fn auto_vectorizes_exact_slice_iter_sum_u64_and_count_lt_u32_select_add() {
        use crate::ty::Scalar;
        let v2i64 = Ty::vector(Scalar::I64, 2).unwrap();
        let v4i32 = Ty::vector(Scalar::I32, 4).unwrap();
        let v4i1 = Ty::vector(Scalar::I1, 4).unwrap();

        // 1. Exact post-inlining `slice::Iter` `sum_u64`:
        //    - `end = PtrAdd(INBOUNDS, xs_ptr, Shl(xs_len, 3))`
        //    - `hdr` compares `PtrAddr(p) != PtrAddr(end)`
        //    - `body` computes `p_next = PtrAdd(INBOUNDS, p, 8)` BEFORE `x = Load(I64, p)`
        let mut s = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let ssig = s.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        s.set_signature(ssig);
        let [sentry, shdr, sbody, sexit] = [(); 4].map(|_| s.create_block(BlockKind::Normal));
        s.begin(sentry);
        let xs_ptr = s.param(Ty::PTR);
        let xs_len = s.param(Ty::I64);
        let k3 = s.iconst(Ty::I64, 3);
        let bytes = s.binary(Op::Shl, Flags::NONE, xs_len, k3);
        let end = s.ptr_add(Flags::INBOUNDS, xs_ptr, bytes);
        let z64 = s.iconst(Ty::I64, 0);
        let eight = s.iconst(Ty::I64, 8);
        s.jump((shdr, &[xs_ptr, z64]));

        s.begin(shdr);
        let sp = s.param(Ty::PTR);
        let sacc = s.param(Ty::I64);
        let p_addr = s.convert(Op::PtrAddr, Flags::NONE, Ty::I64, sp);
        let end_addr = s.convert(Op::PtrAddr, Flags::NONE, Ty::I64, end);
        let keep = s.icmp(IntCond::Ne, p_addr, end_addr);
        s.br(Flags::NONE, keep, (sbody, &[]), (sexit, &[sacc]));

        s.begin(sbody);
        let sp_next = s.ptr_add(Flags::INBOUNDS, sp, eight);
        let sx = s.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
        let sacc2 = s.binary(Op::Add, Flags::NONE, sacc, sx);
        s.jump((shdr, &[sp_next, sacc2]));

        s.begin(sexit);
        let sres = s.param(Ty::I64);
        s.ret(&[sres]);
        let sum_func = s.finish_checked().unwrap();

        let sum_out = auto_vectorize_loops(sum_func.view(), TestHeap).unwrap();
        verify(sum_out.func.view()).unwrap();
        assert_eq!(sum_out.stats.loops_vectorized, 1);
        let sv = sum_out.func.view();
        assert_eq!(
            (0..sv.len()).filter(|&k| sv.op(k) == Op::Load && Ty(sv.tys[k]) == v2i64).count(),
            4,
            "expected 4x-unrolled <2 x i64> loads for slice::Iter sum_u64"
        );
        assert!(
            (0..sv.len()).filter(|&k| sv.op(k) == Op::Add && Ty(sv.tys[k]) == v2i64).count() >= 4,
            "expected 4 independent <2 x i64> accumulators"
        );
        assert!((0..sv.len()).any(|k| sv.op(k) == Op::ReduceAdd && Ty(sv.tys[k]) == Ty::I64));
        // Statically stride-aligned `end = ptr_add(xs_ptr, shl(xs_len, 3))` omits redundant alignment mask!
        assert_eq!(
            sv.block(Block(0)).filter(|&k| sv.op(k) == Op::And).count(),
            1,
            "statically aligned slice end should need only 1 And (`has_rem & enough`)"
        );

        // 2. Exact post-`if_convert` `slice::Iter` `count_lt_u32`:
        //    Tests BOTH `Select(cond, Add(acc, 1), acc)` AND inverted `Select(cond, acc, Add(acc, 1))`,
        //    with `p_next = PtrAdd(INBOUNDS, p, 4)` computed BEFORE `load i32 [p]`, and verifies
        //    that an extra side use of `Add(acc, 1)` blocks vectorization!
        let build_count_select = |invert_select: bool, extra_side_use: bool| {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let ptr = b.param(Ty::PTR);
            let len = b.param(Ty::I64);
            let thresh = b.param(Ty::I32);
            let k2 = b.iconst(Ty::I64, 2);
            let b_bytes = b.binary(Op::Shl, Flags::NONE, len, k2);
            let end_p = b.ptr_add(Flags::INBOUNDS, ptr, b_bytes);
            let z = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let four = b.iconst(Ty::I64, 4);
            b.jump((hdr, &[ptr, z]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(Ty::I64);
            let done = b.icmp(IntCond::Eq, p, end_p);
            b.br(Flags::NONE, done, (exit, &[acc]), (body, &[]));

            b.begin(body);
            let p_next = b.ptr_add(Flags::INBOUNDS, p, four);
            let x = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
            let cmp_cond = if invert_select {
                b.icmp(IntCond::Uge, x, thresh)
            } else {
                b.icmp(IntCond::Ult, x, thresh)
            };
            let acc_plus_1 = b.binary(Op::Add, Flags::NONE, acc, one);
            let acc_next = if invert_select {
                b.select(Flags::NONE, cmp_cond, acc, acc_plus_1)
            } else {
                b.select(Flags::NONE, cmp_cond, acc_plus_1, acc)
            };
            let back_acc = if extra_side_use {
                b.binary(Op::Xor, Flags::NONE, acc_next, acc_plus_1)
            } else {
                acc_next
            };
            b.jump((hdr, &[p_next, back_acc]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        for invert_select in [false, true] {
            let func = build_count_select(invert_select, false);
            let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(
                out.stats.loops_vectorized, 1,
                "count_lt_u32 Select(cond, Add(acc, 1), acc) must vectorize (invert={invert_select})"
            );
            let v = out.func.view();
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v4i32).count(),
                4,
                "expected 4x-unrolled <4 x i32> loads (invert={invert_select})"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Icmp && Ty(v.tys[k]) == v4i1).count(),
                4,
                "expected 4x-unrolled <4 x i1> NEON cmhi masks (invert={invert_select})"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Select && Ty(v.tys[k]) == v4i32).count(),
                4,
                "expected 4x-unrolled Select(mask, -1, 0) (invert={invert_select})"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Sub && Ty(v.tys[k]) == v4i32).count(),
                4,
                "expected 4x-unrolled sub.4s accumulators (invert={invert_select})"
            );
            assert!((0..v.len()).any(|k| v.op(k) == Op::ReduceAdd && Ty(v.tys[k]) == Ty::I32));
            assert!((0..v.len()).any(|k| v.op(k) == Op::Zext && Ty(v.tys[k]) == Ty::I64));
        }

        // Negative control: extra side use of `acc + 1` must NOT vectorize as `CountCond`.
        let bad_side_use = build_count_select(false, true);
        let bad_out = auto_vectorize_loops(bad_side_use.view(), TestHeap).unwrap();
        assert_eq!(bad_out.stats.loops_vectorized, 0);

        // 3. Guarded `PreTerm::Br` entry (`if ptr != end { hdr } else { exit(0) }`) on `sum_u64`.
        let mut g = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let gsig = g.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR)],
            &[Param::of(Ty::I64)],
        );
        g.set_signature(gsig);
        let [gentry, ghdr, gbody, gexit] = [(); 4].map(|_| g.create_block(BlockKind::Normal));
        g.begin(gentry);
        let gp0 = g.param(Ty::PTR);
        let gend = g.param(Ty::PTR);
        let gz = g.iconst(Ty::I64, 0);
        let g8 = g.iconst(Ty::I64, 8);
        let gnonempty = g.icmp(IntCond::Ne, gp0, gend);
        g.br(Flags::NONE, gnonempty, (ghdr, &[gp0, gz]), (gexit, &[gz]));

        g.begin(ghdr);
        let gp = g.param(Ty::PTR);
        let gacc = g.param(Ty::I64);
        let gkeep = g.icmp(IntCond::Ne, gp, gend);
        g.br(Flags::NONE, gkeep, (gbody, &[]), (gexit, &[gacc]));

        g.begin(gbody);
        let gp_next = g.ptr_add(Flags::INBOUNDS, gp, g8);
        let gx = g.load(Ty::I64, Flags::NONE, gp, 0, MemAttrs::aligned(3));
        let gacc2 = g.binary(Op::Add, Flags::NONE, gacc, gx);
        g.jump((ghdr, &[gp_next, gacc2]));

        g.begin(gexit);
        let gres = g.param(Ty::I64);
        g.ret(&[gres]);
        let guarded_func = g.finish_checked().unwrap();

        let guarded_out = auto_vectorize_loops(guarded_func.view(), TestHeap).unwrap();
        verify(guarded_out.func.view()).unwrap();
        assert_eq!(guarded_out.stats.loops_vectorized, 1);
        let gv = guarded_out.func.view();
        assert_eq!(
            (0..gv.len()).filter(|&k| gv.op(k) == Op::Load && Ty(gv.tys[k]) == v2i64).count(),
            4,
            "expected 4x-unrolled <2 x i64> loads with Br preheader trampoline"
        );
        // Arbitrary `end` param emits `(byte_rem & 7) == 0` alignment guard in `vec_guard` (block 1)!
        assert!(
            gv.block(Block(1)).filter(|&k| gv.op(k) == Op::And).count() >= 3,
            "arbitrary pointer-IV end must include `(byte_rem & (elem_bytes - 1)) == 0` alignment guard"
        );
    }

    struct NoRoutines;

    impl crate::legalize::QuadRoutines for NoRoutines {
        fn routine(&self, _: crate::legalize::SoftFloat) -> Option<crate::func::RefId> {
            None
        }
    }

    #[test]
    fn slice_iter_sum_u64_both_param_orders_eval_bit_exact_and_zero_ptr_addr() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        let v2i64 = Ty::vector(Scalar::I64, 2).unwrap();
        let mut data = [0u64; 36];
        for (k, slot) in data.iter_mut().enumerate() {
            let kk = k as u64;
            *slot = kk
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(0xbf58_476d_1ce4_e5b9);
        }

        for &(acc_first, use_mul) in &[(false, false), (true, false), (true, true)] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot = b.slot(36 * 8, 4, SlotKind::Temp);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let base = b.slot_addr(slot);
            for (k, &val) in data.iter().enumerate() {
                let c = b.iconst(Ty::I64, u128::from(val));
                b.store(Flags::NONE, c, base, (k * 8) as i32, MemAttrs::aligned(3));
            }
            let bytes = if use_mul {
                let k8 = b.iconst(Ty::I64, 8);
                b.binary(Op::Mul, Flags::NONE, len, k8)
            } else {
                let k3 = b.iconst(Ty::I64, 3);
                b.binary(Op::Shl, Flags::NONE, len, k3)
            };
            let end = b.ptr_add(Flags::INBOUNDS, base, bytes);
            let z64 = b.iconst(Ty::I64, 0);
            let eight = b.iconst(Ty::I64, 8);
            if acc_first {
                b.jump((hdr, &[z64, base]));
            } else {
                b.jump((hdr, &[base, z64]));
            }

            b.begin(hdr);
            let (p, acc) = if acc_first {
                let a = b.param(Ty::I64);
                let ptr = b.param(Ty::PTR);
                (ptr, a)
            } else {
                let ptr = b.param(Ty::PTR);
                let a = b.param(Ty::I64);
                (ptr, a)
            };
            let cond = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, cond, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
            let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, x);
            if acc_first {
                b.jump((hdr, &[acc_next, p_next]));
            } else {
                b.jump((hdr, &[p_next, acc_next]));
            }

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1);
            let v = out.func.view();
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::PtrAddr).count(),
                0,
                "inbounds slice::Iter sum_u64 should emit zero Op::PtrAddr"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v2i64).count(),
                4,
                "expected 4x-unrolled <2 x i64> loads"
            );
            assert!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Add && Ty(v.tys[k]) == v2i64).count() >= 4,
                "expected >= 4 <2 x i64> Op::Add instructions"
            );
            assert!((0..v.len()).any(|k| v.op(k) == Op::ReduceAdd && Ty(v.tys[k]) == Ty::I64));

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for &n in &[0usize, 1, 7, 8, 15, 16, 33] {
                let expected: u64 = data[..n].iter().fold(0u64, |s, &x| s.wrapping_add(x));
                let args = [Val::Int(n as u128)];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig sum_u64 eval failed at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec sum_u64 eval failed at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected))]);
                assert_eq!(
                    got.as_slice(),
                    orig.as_slice(),
                    "sum_u64 mismatch at n={n} (acc_first={acc_first}, use_mul={use_mul})"
                );
            }
        }
    }

    #[test]
    fn slice_iter_count_lt_u32_variants_eval_bit_exact_and_zero_ptr_addr() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        #[derive(Clone, Copy)]
        enum CountForm {
            AddSelect10,
            AddSelect01,
            AddZext,
            SubSext,
            SubSelectNeg1,
            AddSelectI32Acc,
        }

        let v4i32 = Ty::vector(Scalar::I32, 4).unwrap();
        let mut data = [0u32; 40];
        for (k, slot) in data.iter_mut().enumerate() {
            *slot = match k {
                0 => 0,
                1 => u32::MAX,
                2 => 50,
                3 => 49,
                _ => ((k as u32).wrapping_mul(37).wrapping_add(13)) % 100,
            };
        }

        for form in [
            CountForm::AddSelect10,
            CountForm::AddSelect01,
            CountForm::AddZext,
            CountForm::SubSext,
            CountForm::SubSelectNeg1,
            CountForm::AddSelectI32Acc,
        ] {
            let acc_ty = match form {
                CountForm::AddSelectI32Acc => Ty::I32,
                _ => Ty::I64,
            };
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I32)],
                &[Param::of(acc_ty)],
            );
            b.set_signature(sig);
            let slot = b.slot(40 * 4, 4, SlotKind::Temp);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let thresh = b.param(Ty::I32);
            let base = b.slot_addr(slot);
            for (k, &val) in data.iter().enumerate() {
                let c = b.iconst(Ty::I32, u128::from(val));
                b.store(Flags::NONE, c, base, (k * 4) as i32, MemAttrs::aligned(2));
            }
            let k2 = b.iconst(Ty::I64, 2);
            let bytes = b.binary(Op::Shl, Flags::NONE, len, k2);
            let end = b.ptr_add(Flags::INBOUNDS, base, bytes);
            let zero_acc = b.iconst(acc_ty, 0);
            let one_acc = b.iconst(acc_ty, 1);
            let neg1_acc = b.iconst(acc_ty, u128::from(u64::MAX));
            let four = b.iconst(Ty::I64, 4);
            b.jump((hdr, &[base, zero_acc]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(acc_ty);
            let keep = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let p_next = b.ptr_add(Flags::INBOUNDS, p, four);
            let val = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
            let acc_next = match form {
                CountForm::AddSelect10 | CountForm::AddSelectI32Acc => {
                    let cmp = b.icmp(IntCond::Ult, val, thresh);
                    let inc = b.select(Flags::NONE, cmp, one_acc, zero_acc);
                    b.binary(Op::Add, Flags::NONE, acc, inc)
                }
                CountForm::AddSelect01 => {
                    let cmp = b.icmp(IntCond::Uge, val, thresh);
                    let inc = b.select(Flags::NONE, cmp, zero_acc, one_acc);
                    b.binary(Op::Add, Flags::NONE, acc, inc)
                }
                CountForm::AddZext => {
                    let cmp = b.icmp(IntCond::Ult, val, thresh);
                    let inc = b.convert(Op::Zext, Flags::NONE, acc_ty, cmp);
                    b.binary(Op::Add, Flags::NONE, acc, inc)
                }
                CountForm::SubSext => {
                    let cmp = b.icmp(IntCond::Ult, val, thresh);
                    let m = b.convert(Op::Sext, Flags::NONE, acc_ty, cmp);
                    b.binary(Op::Sub, Flags::NONE, acc, m)
                }
                CountForm::SubSelectNeg1 => {
                    let cmp = b.icmp(IntCond::Ult, val, thresh);
                    let m = b.select(Flags::NONE, cmp, neg1_acc, zero_acc);
                    b.binary(Op::Sub, Flags::NONE, acc, m)
                }
            };
            b.jump((hdr, &[p_next, acc_next]));

            b.begin(exit);
            let res = b.param(acc_ty);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1);
            let v = out.func.view();
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::PtrAddr).count(),
                0,
                "inbounds slice::Iter count_lt_u32 should emit zero Op::PtrAddr"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v4i32).count(),
                4,
                "expected 4x-unrolled <4 x i32> loads"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Sub && Ty(v.tys[k]) == v4i32).count(),
                4,
                "expected 4x-unrolled <4 x i32> mask-subtract accumulators"
            );

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            let thresh_val = 50u32;
            for &n in &[0usize, 1, 7, 16, 35] {
                let expected = data[..n].iter().filter(|&&x| x < thresh_val).count() as u64;
                let args = [Val::Int(n as u128), Val::Int(u128::from(thresh_val))];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig count_lt_u32 eval failed at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec count_lt_u32 eval failed at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected))]);
                assert_eq!(got.as_slice(), orig.as_slice(), "count_lt_u32 mismatch at n={n}");
            }
        }
    }

    #[test]
    fn strict_ieee_dot_f32_preserves_bit_exact_rounding_order() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        let v4f32 = Ty::vector(Scalar::F32, 4).unwrap();
        // Non-exact and cancellation-sensitive IEEE 754 floats:
        // `(1e20 + 1e-20) + (-1e20) == 0.0` in strict left-to-right order,
        // whereas reassociated `(1e20 + -1e20) + 1e-20 == 1e-20`.
        let xs: [f32; 20] = [
            -0.0,
            1.0 / 3.0,
            1e20,
            1e-20,
            -1e20,
            0.1,
            7.0 / 11.0,
            -1.0 / 3.0,
            1e20,
            1e-20,
            -1e20,
            -0.0,
            3.0 / 7.0,
            1e-20,
            1e20,
            -1e20,
            0.2,
            1.0 / 9.0,
            -1e-20,
            1.0 / 13.0,
        ];
        let ys: [f32; 20] = [
            1.0,
            1.0 / 7.0,
            1.0,
            1.0,
            1.0,
            0.2,
            -0.3,
            2.0 / 5.0,
            1.0,
            1.0,
            1.0,
            -1.0,
            0.7,
            1.0,
            1.0,
            1.0,
            -0.1,
            3.0 / 11.0,
            1.0,
            -0.25,
        ];

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::F32)],
            &[Param::of(Ty::F32)],
        );
        b.set_signature(sig);
        let slot_a = b.slot(20 * 4, 2, SlotKind::Temp);
        let slot_b = b.slot(20 * 4, 2, SlotKind::Temp);
        let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let init = b.param(Ty::F32);
        let pa = b.slot_addr(slot_a);
        let pb = b.slot_addr(slot_b);
        for k in 0..20usize {
            let ca = b.fconst(Ty::F32, u128::from(xs[k].to_bits()));
            let cb = b.fconst(Ty::F32, u128::from(ys[k].to_bits()));
            b.store(Flags::NONE, ca, pa, (k * 4) as i32, MemAttrs::aligned(2));
            b.store(Flags::NONE, cb, pb, (k * 4) as i32, MemAttrs::aligned(2));
        }
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let two = b.iconst(Ty::I64, 2);
        b.jump((hdr, &[zero, init]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::F32);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, two);
        let p1 = b.ptr_add(Flags::NONE, pa, off);
        let p2 = b.ptr_add(Flags::NONE, pb, off);
        let va = b.load(Ty::F32, Flags::NONE, p1, 0, MemAttrs::aligned(2));
        let vb = b.load(Ty::F32, Flags::NONE, p2, 0, MemAttrs::aligned(2));
        let prod = b.binary(Op::Fmul, Flags::NONE, va, vb);
        let acc_next = b.binary(Op::Fadd, Flags::NONE, acc, prod);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[i_next, acc_next]));

        b.begin(exit);
        let res = b.param(Ty::F32);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(
            out.stats.loops_vectorized, 1,
            "strict IEEE 754 dot_f32 without REASSOC should vectorize via ordered Extract + Fadd"
        );
        let v = out.func.view();
        assert_eq!(
            (0..v.len()).filter(|&k| v.op(k) == Op::ReduceFadd).count(),
            0,
            "strict IEEE 754 dot_f32 without REASSOC must never reassociate into ReduceFadd"
        );
        assert_eq!(
            (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v4f32).count(),
            8,
            "expected 4x-unrolled <4 x f32> loads (4 per input array)"
        );
        for k in 0..v.len() {
            if v.op(k) == Op::Load && Ty(v.tys[k]) == v4f32 {
                assert_eq!(
                    v.mem_attrs(k).map(|m| m.align),
                    Some(2),
                    "vector f32 load must preserve 4-byte element alignment, never over-claiming 16B"
                );
            }
        }
        assert_eq!(
            (0..v.len()).filter(|&k| v.op(k) == Op::Fmul && Ty(v.tys[k]) == v4f32).count(),
            4,
            "expected 4x-unrolled <4 x f32> Op::Fmul"
        );
        assert_eq!(
            (0..v.len()).filter(|&k| v.op(k) == Op::Extract && Ty(v.tys[k]) == Ty::F32).count(),
            16,
            "expected 16 sequential Op::Extract lanes per 4x-unrolled iteration"
        );

        let mut legalized = out.func;
        legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
        verify(legalized.view()).unwrap();

        for &init_f in &[0.0_f32, -0.0_f32] {
            for &len in &[0usize, 1, 2, 3, 4, 7, 8, 15, 16, 17, 20] {
                let mut expected = init_f;
                for k in 0..len {
                    expected += xs[k] * ys[k];
                }
                let args = [Val::Int(len as u128), Val::Int(u128::from(init_f.to_bits()))];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig strict dot_f32 eval failed at len={len}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("post-vectorize strict dot_f32 eval failed at len={len}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected.to_bits()))]);
                assert_eq!(
                    got.as_slice(),
                    orig.as_slice(),
                    "strict IEEE dot_f32 bit-exact mismatch at len={len}, init_bits={:#x}",
                    init_f.to_bits()
                );
            }
        }
    }

    #[test]
    fn slice_iter_popcount_u64_vectorizes_to_chunked_v16i8_and_matches_eval() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        let v16i8 = Ty::vector(Scalar::I8, 16).unwrap();
        let v8i16 = Ty::vector(Scalar::I16, 8).unwrap();

        // 1. Exact `slice::Iter` `popcount_u64` across 300 `u64`s (crossing the 240-`u64` = 1,920-byte
        //    chunk boundary!) covering:
        //    - `Trunc(Popcnt(I64) -> I32)` + `Zext(I32 -> I64)` (`HeaderBody`, mixed bit patterns)
        //    - `And(Popcnt(I64), 0xffff_ffff)` (`SingleBlock` rotated with `PreTerm::Br`, mixed bit patterns)
        //    - Direct `Add(acc, Popcnt(I64))` (`HeaderBody`, all-`u64::MAX` stressing `30 * 8 = 240` saturation)
        #[derive(Clone, Copy)]
        enum Pop64Form {
            TruncZextHeaderBody,
            AndMaskSingleBlockRotated,
            DirectAllOnesHeaderBody,
        }

        let mut data64 = [0u64; 300];
        for (k, slot) in data64.iter_mut().enumerate() {
            *slot = match k {
                0 => 0,
                1 => u64::MAX,
                2 => 1,
                3 => 1u64 << 63,
                4 => 0x5555_5555_5555_5555,
                5 => 0xaaaa_aaaa_aaaa_aaaa,
                _ => (k as u64)
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    .wrapping_add(0xbf58_476d_1ce4_e5b9),
            };
        }
        let all_ones64 = [u64::MAX; 300];

        for form in [
            Pop64Form::TruncZextHeaderBody,
            Pop64Form::AndMaskSingleBlockRotated,
            Pop64Form::DirectAllOnesHeaderBody,
        ] {
            let vals: &[u64; 300] = match form {
                Pop64Form::DirectAllOnesHeaderBody => &all_ones64,
                _ => &data64,
            };
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot = b.slot(300 * 8, 4, SlotKind::Temp);

            let func = match form {
                Pop64Form::AndMaskSingleBlockRotated => {
                    let [entry, body, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
                    b.begin(entry);
                    let len = b.param(Ty::I64);
                    let base = b.slot_addr(slot);
                    for (k, &val) in vals.iter().enumerate() {
                        let c = b.iconst(Ty::I64, u128::from(val));
                        b.store(Flags::NONE, c, base, (k * 8) as i32, MemAttrs::aligned(3));
                    }
                    let k3 = b.iconst(Ty::I64, 3);
                    let bytes = b.binary(Op::Shl, Flags::NONE, len, k3);
                    let end = b.ptr_add(Flags::INBOUNDS, base, bytes);
                    let z64 = b.iconst(Ty::I64, 0);
                    let eight = b.iconst(Ty::I64, 8);
                    let mask32 = b.iconst(Ty::I64, 0xffff_ffff);
                    let nonempty = b.icmp(IntCond::Ne, base, end);
                    b.br(Flags::NONE, nonempty, (body, &[base, z64]), (exit, &[z64]));

                    b.begin(body);
                    let p = b.param(Ty::PTR);
                    let acc = b.param(Ty::I64);
                    let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
                    let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
                    let pc = b.unary(Op::Popcnt, Flags::NONE, x);
                    let pc_m = b.binary(Op::And, Flags::NONE, pc, mask32);
                    let acc_next = b.binary(Op::Add, Flags::NONE, acc, pc_m);
                    let keep = b.icmp(IntCond::Ne, p_next, end);
                    b.br(Flags::NONE, keep, (body, &[p_next, acc_next]), (exit, &[acc_next]));

                    b.begin(exit);
                    let res = b.param(Ty::I64);
                    b.ret(&[res]);
                    b.finish_checked().unwrap()
                }
                Pop64Form::TruncZextHeaderBody | Pop64Form::DirectAllOnesHeaderBody => {
                    let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
                    b.begin(entry);
                    let len = b.param(Ty::I64);
                    let base = b.slot_addr(slot);
                    for (k, &val) in vals.iter().enumerate() {
                        let c = b.iconst(Ty::I64, u128::from(val));
                        b.store(Flags::NONE, c, base, (k * 8) as i32, MemAttrs::aligned(3));
                    }
                    let k3 = b.iconst(Ty::I64, 3);
                    let bytes = b.binary(Op::Shl, Flags::NONE, len, k3);
                    let end = b.ptr_add(Flags::INBOUNDS, base, bytes);
                    let z64 = b.iconst(Ty::I64, 0);
                    let eight = b.iconst(Ty::I64, 8);
                    b.jump((hdr, &[base, z64]));

                    b.begin(hdr);
                    let p = b.param(Ty::PTR);
                    let acc = b.param(Ty::I64);
                    let keep = b.icmp(IntCond::Ne, p, end);
                    b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

                    b.begin(body);
                    let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
                    let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
                    let pc = b.unary(Op::Popcnt, Flags::NONE, x);
                    let term = if matches!(form, Pop64Form::TruncZextHeaderBody) {
                        let t32 = b.convert(Op::Trunc, Flags::NONE, Ty::I32, pc);
                        b.convert(Op::Zext, Flags::NONE, Ty::I64, t32)
                    } else {
                        pc
                    };
                    let acc_next = b.binary(Op::Add, Flags::NONE, acc, term);
                    b.jump((hdr, &[p_next, acc_next]));

                    b.begin(exit);
                    let res = b.param(Ty::I64);
                    b.ret(&[res]);
                    b.finish_checked().unwrap()
                }
            };

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1, "popcount_u64 must vectorize");
            let v = out.func.view();
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::PtrAddr).count(),
                0,
                "inbounds slice::Iter popcount_u64 should emit zero Op::PtrAddr"
            );
            assert!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Popcnt && Ty(v.tys[k]) == v16i8).count() >= 4,
                "expected >= 4 <16 x i8> Op::Popcnt ops"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v16i8).count(),
                4,
                "expected 4x-unrolled <16 x i8> loads (64B/iter)"
            );
            assert!(
                (0..v.len()).any(|k| v.op(k) == Op::Bitcast && Ty(v.tys[k]) == v8i16),
                "expected <16 x i8> -> <8 x i16> bitcast widening"
            );
            assert!(
                (0..v.len()).any(|k| v.op(k) == Op::ReduceAdd && Ty(v.tys[k]) == Ty::I16),
                "expected Ty::I16 horizontal ReduceAdd on widened <8 x i16>"
            );

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for &n in &[0usize, 1, 7, 8, 9, 16, 240, 250, 300] {
                let expected: u64 = vals[..n].iter().map(|&x| u64::from(x.count_ones())).sum();
                let args = [Val::Int(n as u128)];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 500_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig popcount_u64 eval failed at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 500_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec popcount_u64 eval failed at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected))]);
                assert_eq!(got.as_slice(), orig.as_slice(), "popcount_u64 mismatch at n={n}");
            }
        }

        // 2. Direct `Add(acc, Popcnt(v))` for `I32` (`acc_ty = I32` and `I64`) and counted-IV `I8`!
        let mut data32 = [0u32; 40];
        for (k, slot) in data32.iter_mut().enumerate() {
            *slot = if k == 0 {
                u32::MAX
            } else {
                (k as u32).wrapping_mul(0x9e37_79b9).wrapping_add(17)
            };
        }
        for acc_ty in [Ty::I32, Ty::I64] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(acc_ty)]);
            b.set_signature(sig);
            let slot = b.slot(40 * 4, 4, SlotKind::Temp);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let len = b.param(Ty::I64);
            let base = b.slot_addr(slot);
            for (k, &val) in data32.iter().enumerate() {
                let c = b.iconst(Ty::I32, u128::from(val));
                b.store(Flags::NONE, c, base, (k * 4) as i32, MemAttrs::aligned(2));
            }
            let k2 = b.iconst(Ty::I64, 2);
            let bytes = b.binary(Op::Shl, Flags::NONE, len, k2);
            let end = b.ptr_add(Flags::INBOUNDS, base, bytes);
            let z_acc = b.iconst(acc_ty, 0);
            let four = b.iconst(Ty::I64, 4);
            b.jump((hdr, &[base, z_acc]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(acc_ty);
            let keep = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let p_next = b.ptr_add(Flags::INBOUNDS, p, four);
            let x = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
            let pc = b.unary(Op::Popcnt, Flags::NONE, x);
            let term = if acc_ty == Ty::I64 {
                b.convert(Op::Zext, Flags::NONE, Ty::I64, pc)
            } else {
                pc
            };
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, term);
            b.jump((hdr, &[p_next, acc_next]));

            b.begin(exit);
            let res = b.param(acc_ty);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1);
            let v = out.func.view();
            assert!((0..v.len()).filter(|&k| v.op(k) == Op::Popcnt && Ty(v.tys[k]) == v16i8).count() >= 4);

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for &n in &[0usize, 1, 15, 16, 17, 35] {
                let expected: u64 = data32[..n].iter().map(|&x| u64::from(x.count_ones())).sum();
                let args = [Val::Int(n as u128)];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig popcount_u32 eval failed at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec popcount_u32 eval failed at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected))]);
                assert_eq!(got.as_slice(), orig.as_slice(), "popcount_u32 mismatch at n={n}");
            }
        }

        // 3. Counted-IV `I8` popcount (`for i in 0..n { acc += xs[i].count_ones() as u64; }`).
        let mut data8 = [0u8; 130];
        for (k, slot) in data8.iter_mut().enumerate() {
            *slot = if k % 5 == 0 {
                0xff
            } else {
                (k as u8).wrapping_mul(37).wrapping_add(11)
            };
        }
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot = b.slot(130, 4, SlotKind::Temp);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let len = b.param(Ty::I64);
            let base = b.slot_addr(slot);
            for (k, &val) in data8.iter().enumerate() {
                let c = b.iconst(Ty::I8, u128::from(val));
                b.store(Flags::NONE, c, base, k as i32, MemAttrs::aligned(0));
            }
            let z64 = b.iconst(Ty::I64, 0);
            let one64 = b.iconst(Ty::I64, 1);
            b.jump((hdr, &[z64, z64]));

            b.begin(hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, len);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let ep = b.ptr_add(Flags::NONE, base, i);
            let x = b.load(Ty::I8, Flags::NONE, ep, 0, MemAttrs::aligned(0));
            let pc = b.unary(Op::Popcnt, Flags::NONE, x);
            let pc64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, pc);
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, pc64);
            let i_next = b.binary(Op::Add, Flags::NONE, i, one64);
            b.jump((hdr, &[i_next, acc_next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1);
            let v = out.func.view();
            assert!((0..v.len()).filter(|&k| v.op(k) == Op::Popcnt && Ty(v.tys[k]) == v16i8).count() >= 4);

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for &n in &[0usize, 1, 7, 63, 64, 65, 130] {
                let expected: u64 = data8[..n].iter().map(|&x| u64::from(x.count_ones())).sum();
                let args = [Val::Int(n as u128)];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig popcount_u8 eval failed at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec popcount_u8 eval failed at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected))]);
                assert_eq!(got.as_slice(), orig.as_slice(), "popcount_u8 mismatch at n={n}");
            }
        }

        // 4. Negative control: masked parity `And(Popcnt(v), 1)` must NOT vectorize as popcount sum!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let base = b.param(Ty::PTR);
            let len = b.param(Ty::I64);
            let k3 = b.iconst(Ty::I64, 3);
            let bytes = b.binary(Op::Shl, Flags::NONE, len, k3);
            let end = b.ptr_add(Flags::INBOUNDS, base, bytes);
            let z64 = b.iconst(Ty::I64, 0);
            let one64 = b.iconst(Ty::I64, 1);
            let eight = b.iconst(Ty::I64, 8);
            b.jump((hdr, &[base, z64]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
            let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
            let pc = b.unary(Op::Popcnt, Flags::NONE, x);
            let parity = b.binary(Op::And, Flags::NONE, pc, one64);
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, parity);
            b.jump((hdr, &[p_next, acc_next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 0, "masked parity And(Popcnt, 1) must not vectorize");
        }
    }

    #[test]
    fn strict_ieee_dot_f32_all_lengths_and_forms_bit_exact() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum F32Form {
            DotIndexShl,
            DotIndexMulRev,
            DotPtrIv,
            SumIndex,
        }

        const N: usize = 63;
        let v4f32 = Ty::vector(Scalar::F32, 4).unwrap();
        let base_xs: [f32; 12] = [
            1e7,
            0.1,
            -0.33333334,
            1e-5,
            f32::from_bits(1),
            f32::from_bits(0x0040_0000),
            -0.0,
            1e20,
            1e-20,
            -1e20,
            f32::from_bits(0x8000_0001),
            7.0 / 11.0,
        ];
        let base_ys: [f32; 12] = [
            1.0,
            0.2,
            0.7,
            -1.0 / 3.0,
            2.0,
            0.5,
            -1.0,
            1.0,
            1.0,
            1.0,
            -0.5,
            -0.3,
        ];
        let mut xs = [0.0f32; N];
        let mut ys = [0.0f32; N];
        for k in 0..N {
            let scale = if k % 7 == 0 { -1.0f32 } else { 1.0f32 };
            xs[k] = base_xs[k % base_xs.len()] * scale;
            ys[k] = base_ys[(k * 5 + 1) % base_ys.len()];
        }

        for form in [
            F32Form::DotIndexShl,
            F32Form::DotIndexMulRev,
            F32Form::DotPtrIv,
            F32Form::SumIndex,
        ] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::F32)]);
            b.set_signature(sig);
            let slot_a = b.slot((N * 4) as u32, 2, SlotKind::Temp);
            let slot_b = b.slot((N * 4) as u32, 2, SlotKind::Temp);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let pa = b.slot_addr(slot_a);
            let pb = b.slot_addr(slot_b);
            for k in 0..N {
                let ca = b.fconst(Ty::F32, u128::from(xs[k].to_bits()));
                let cb = b.fconst(Ty::F32, u128::from(ys[k].to_bits()));
                b.store(Flags::NONE, ca, pa, (k * 4) as i32, MemAttrs::aligned(2));
                b.store(Flags::NONE, cb, pb, (k * 4) as i32, MemAttrs::aligned(2));
            }
            let zero_i = b.iconst(Ty::I64, 0);
            let one_i = b.iconst(Ty::I64, 1);
            let two_i = b.iconst(Ty::I64, 2);
            let four_i = b.iconst(Ty::I64, 4);
            let fzero = b.fconst(Ty::F32, 0);

            match form {
                F32Form::DotPtrIv => {
                    let bytes = b.binary(Op::Shl, Flags::NONE, len, two_i);
                    let end_a = b.ptr_add(Flags::INBOUNDS, pa, bytes);
                    b.jump((hdr, &[pa, pb, fzero]));

                    b.begin(hdr);
                    let p1 = b.param(Ty::PTR);
                    let p2 = b.param(Ty::PTR);
                    let acc = b.param(Ty::F32);
                    let keep = b.icmp(IntCond::Ne, p1, end_a);
                    b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

                    b.begin(body);
                    let p1_next = b.ptr_add(Flags::INBOUNDS, p1, four_i);
                    let p2_next = b.ptr_add(Flags::INBOUNDS, p2, four_i);
                    let va = b.load(Ty::F32, Flags::NONE, p1, 0, MemAttrs::aligned(2));
                    let vb = b.load(Ty::F32, Flags::NONE, p2, 0, MemAttrs::aligned(2));
                    let prod = b.binary(Op::Fmul, Flags::NONE, va, vb);
                    let acc_next = b.binary(Op::Fadd, Flags::NONE, acc, prod);
                    b.jump((hdr, &[p1_next, p2_next, acc_next]));
                }
                F32Form::DotIndexShl | F32Form::DotIndexMulRev | F32Form::SumIndex => {
                    b.jump((hdr, &[zero_i, fzero]));

                    b.begin(hdr);
                    let i = b.param(Ty::I64);
                    let acc = b.param(Ty::F32);
                    let keep = b.icmp(IntCond::Ult, i, len);
                    b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

                    b.begin(body);
                    let off = if form == F32Form::DotIndexMulRev {
                        b.binary(Op::Mul, Flags::NONE, i, four_i)
                    } else {
                        b.binary(Op::Shl, Flags::NONE, i, two_i)
                    };
                    let p1 = b.ptr_add(Flags::NONE, pa, off);
                    let va = b.load(Ty::F32, Flags::NONE, p1, 0, MemAttrs::aligned(2));
                    let acc_next = match form {
                        F32Form::SumIndex => b.binary(Op::Fadd, Flags::NONE, acc, va),
                        F32Form::DotIndexMulRev => {
                            let p2 = b.ptr_add(Flags::NONE, pb, off);
                            let vb = b.load(Ty::F32, Flags::NONE, p2, 0, MemAttrs::aligned(2));
                            let prod = b.binary(Op::Fmul, Flags::NONE, va, vb);
                            b.binary(Op::Fadd, Flags::NONE, prod, acc)
                        }
                        _ => {
                            let p2 = b.ptr_add(Flags::NONE, pb, off);
                            let vb = b.load(Ty::F32, Flags::NONE, p2, 0, MemAttrs::aligned(2));
                            let prod = b.binary(Op::Fmul, Flags::NONE, va, vb);
                            b.binary(Op::Fadd, Flags::NONE, acc, prod)
                        }
                    };
                    let i_next = b.binary(Op::Add, Flags::NONE, i, one_i);
                    b.jump((hdr, &[i_next, acc_next]));
                }
            }

            b.begin(exit);
            let res = b.param(Ty::F32);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1);
            let v = out.func.view();
            assert_eq!((0..v.len()).filter(|&k| v.op(k) == Op::ReduceFadd).count(), 0);
            let want_loads = if form == F32Form::SumIndex { 4 } else { 8 };
            let want_fmuls = if form == F32Form::SumIndex { 0 } else { 4 };
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v4f32).count(),
                want_loads
            );
            for k in 0..v.len() {
                if v.op(k) == Op::Load && Ty(v.tys[k]) == v4f32 {
                    assert_eq!(v.mem_attrs(k).map(|m| m.align), Some(2));
                }
            }
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Fmul && Ty(v.tys[k]) == v4f32).count(),
                want_fmuls
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Extract && Ty(v.tys[k]) == Ty::F32).count(),
                16
            );
            // Verify exact operand order of the 16 sequential scalar Fadd ops in `vec_loop` (block 2).
            let mut vloop_fadds = 0usize;
            for k in v.block(Block(2)) {
                if v.op(k) == Op::Fadd && Ty(v.tys[k]) == Ty::F32 {
                    vloop_fadds += 1;
                    let idx = v.data[k] as usize;
                    let lhs = v.extra[idx] as usize;
                    let rhs = v.extra[idx + 1] as usize;
                    if form == F32Form::DotIndexMulRev {
                        assert_eq!(v.op(lhs), Op::Extract, "reversed Fadd(prod, acc) must keep lane on LHS");
                    } else {
                        assert_eq!(v.op(rhs), Op::Extract, "Fadd(acc, prod) must keep lane on RHS");
                    }
                }
            }
            assert_eq!(vloop_fadds, 16);

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for &n in &[0usize, 1, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 63] {
                let mut expected = 0.0f32;
                for k in 0..n {
                    expected = match form {
                        F32Form::SumIndex => expected + xs[k],
                        F32Form::DotIndexMulRev => (xs[k] * ys[k]) + expected,
                        _ => expected + (xs[k] * ys[k]),
                    };
                }
                let args = [Val::Int(n as u128)];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig strict f32 eval failed at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec strict f32 eval failed at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected.to_bits()))]);
                assert_eq!(got.as_slice(), orig.as_slice(), "strict f32 bit-exact mismatch at n={n}");
            }
        }
    }

    #[test]
    fn strict_ieee_dot_f64_all_lengths_and_forms_bit_exact() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum F64Form {
            DotIndexShl,
            DotIndexMulRev,
            DotPtrIv,
            SumIndex,
        }

        const N: usize = 63;
        let v2f64 = Ty::vector(Scalar::F64, 2).unwrap();
        let base_xs: [f64; 12] = [
            1e16,
            0.1,
            -1.0 / 3.0,
            1e-16,
            f64::from_bits(1),
            f64::from_bits(0x0008_0000_0000_0000),
            -0.0,
            1e300,
            1e-100,
            -1e300,
            f64::from_bits(0x8000_0000_0000_0001),
            7.0 / 11.0,
        ];
        let base_ys: [f64; 12] = [
            1.0,
            0.2,
            0.7,
            -1.0 / 7.0,
            2.0,
            0.5,
            -1.0,
            1.0,
            1.0,
            1.0,
            -0.5,
            -0.3,
        ];
        let mut xs = [0.0f64; N];
        let mut ys = [0.0f64; N];
        for k in 0..N {
            let scale = if k % 5 == 0 { -1.0f64 } else { 1.0f64 };
            xs[k] = base_xs[k % base_xs.len()] * scale;
            ys[k] = base_ys[(k * 7 + 2) % base_ys.len()];
        }

        for form in [
            F64Form::DotIndexShl,
            F64Form::DotIndexMulRev,
            F64Form::DotPtrIv,
            F64Form::SumIndex,
        ] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::F64)]);
            b.set_signature(sig);
            let slot_a = b.slot((N * 8) as u32, 3, SlotKind::Temp);
            let slot_b = b.slot((N * 8) as u32, 3, SlotKind::Temp);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let pa = b.slot_addr(slot_a);
            let pb = b.slot_addr(slot_b);
            for k in 0..N {
                let ca = b.fconst(Ty::F64, u128::from(xs[k].to_bits()));
                let cb = b.fconst(Ty::F64, u128::from(ys[k].to_bits()));
                b.store(Flags::NONE, ca, pa, (k * 8) as i32, MemAttrs::aligned(3));
                b.store(Flags::NONE, cb, pb, (k * 8) as i32, MemAttrs::aligned(3));
            }
            let zero_i = b.iconst(Ty::I64, 0);
            let one_i = b.iconst(Ty::I64, 1);
            let three_i = b.iconst(Ty::I64, 3);
            let eight_i = b.iconst(Ty::I64, 8);
            let fzero = b.fconst(Ty::F64, 0);

            match form {
                F64Form::DotPtrIv => {
                    let bytes = b.binary(Op::Shl, Flags::NONE, len, three_i);
                    let end_a = b.ptr_add(Flags::INBOUNDS, pa, bytes);
                    b.jump((hdr, &[pa, pb, fzero]));

                    b.begin(hdr);
                    let p1 = b.param(Ty::PTR);
                    let p2 = b.param(Ty::PTR);
                    let acc = b.param(Ty::F64);
                    let keep = b.icmp(IntCond::Ne, p1, end_a);
                    b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

                    b.begin(body);
                    let p1_next = b.ptr_add(Flags::INBOUNDS, p1, eight_i);
                    let p2_next = b.ptr_add(Flags::INBOUNDS, p2, eight_i);
                    let va = b.load(Ty::F64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
                    let vb = b.load(Ty::F64, Flags::NONE, p2, 0, MemAttrs::aligned(3));
                    let prod = b.binary(Op::Fmul, Flags::NONE, va, vb);
                    let acc_next = b.binary(Op::Fadd, Flags::NONE, acc, prod);
                    b.jump((hdr, &[p1_next, p2_next, acc_next]));
                }
                F64Form::DotIndexShl | F64Form::DotIndexMulRev | F64Form::SumIndex => {
                    b.jump((hdr, &[zero_i, fzero]));

                    b.begin(hdr);
                    let i = b.param(Ty::I64);
                    let acc = b.param(Ty::F64);
                    let keep = b.icmp(IntCond::Ult, i, len);
                    b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

                    b.begin(body);
                    let off = if form == F64Form::DotIndexMulRev {
                        b.binary(Op::Mul, Flags::NONE, i, eight_i)
                    } else {
                        b.binary(Op::Shl, Flags::NONE, i, three_i)
                    };
                    let p1 = b.ptr_add(Flags::NONE, pa, off);
                    let va = b.load(Ty::F64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
                    let acc_next = match form {
                        F64Form::SumIndex => b.binary(Op::Fadd, Flags::NONE, acc, va),
                        F64Form::DotIndexMulRev => {
                            let p2 = b.ptr_add(Flags::NONE, pb, off);
                            let vb = b.load(Ty::F64, Flags::NONE, p2, 0, MemAttrs::aligned(3));
                            let prod = b.binary(Op::Fmul, Flags::NONE, va, vb);
                            b.binary(Op::Fadd, Flags::NONE, prod, acc)
                        }
                        _ => {
                            let p2 = b.ptr_add(Flags::NONE, pb, off);
                            let vb = b.load(Ty::F64, Flags::NONE, p2, 0, MemAttrs::aligned(3));
                            let prod = b.binary(Op::Fmul, Flags::NONE, va, vb);
                            b.binary(Op::Fadd, Flags::NONE, acc, prod)
                        }
                    };
                    let i_next = b.binary(Op::Add, Flags::NONE, i, one_i);
                    b.jump((hdr, &[i_next, acc_next]));
                }
            }

            b.begin(exit);
            let res = b.param(Ty::F64);
            b.ret(&[res]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            verify(out.func.view()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1);
            let v = out.func.view();
            assert_eq!((0..v.len()).filter(|&k| v.op(k) == Op::ReduceFadd).count(), 0);
            let want_loads = if form == F64Form::SumIndex { 4 } else { 8 };
            let want_fmuls = if form == F64Form::SumIndex { 0 } else { 4 };
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v2f64).count(),
                want_loads
            );
            for k in 0..v.len() {
                if v.op(k) == Op::Load && Ty(v.tys[k]) == v2f64 {
                    assert_eq!(v.mem_attrs(k).map(|m| m.align), Some(3));
                }
            }
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Fmul && Ty(v.tys[k]) == v2f64).count(),
                want_fmuls
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::Extract && Ty(v.tys[k]) == Ty::F64).count(),
                8
            );
            let mut vloop_fadds = 0usize;
            for k in v.block(Block(2)) {
                if v.op(k) == Op::Fadd && Ty(v.tys[k]) == Ty::F64 {
                    vloop_fadds += 1;
                    let idx = v.data[k] as usize;
                    let lhs = v.extra[idx] as usize;
                    let rhs = v.extra[idx + 1] as usize;
                    if form == F64Form::DotIndexMulRev {
                        assert_eq!(v.op(lhs), Op::Extract, "reversed Fadd(prod, acc) must keep lane on LHS");
                    } else {
                        assert_eq!(v.op(rhs), Op::Extract, "Fadd(acc, prod) must keep lane on RHS");
                    }
                }
            }
            assert_eq!(vloop_fadds, 8);

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for &n in &[0usize, 1, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 63] {
                let mut expected = 0.0f64;
                for k in 0..n {
                    expected = match form {
                        F64Form::SumIndex => expected + xs[k],
                        F64Form::DotIndexMulRev => (xs[k] * ys[k]) + expected,
                        _ => expected + (xs[k] * ys[k]),
                    };
                }
                let args = [Val::Int(n as u128)];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig strict f64 eval failed at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec strict f64 eval failed at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected.to_bits()))]);
                assert_eq!(got.as_slice(), orig.as_slice(), "strict f64 bit-exact mismatch at n={n}");
            }
        }
    }

    #[test]
    fn vectorize_prefix_scan_u64_pointer_iter_mut() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
        };

        const MAX_N: usize = 64;
        const INIT_ACC: u64 = 0xffff_ffff_ffff_fff0;
        let edge_vals: [u64; 8] = [
            u64::MAX,
            u64::MAX - 1,
            0,
            1,
            0x8000_0000_0000_0000,
            0x7fff_ffff_ffff_ffff,
            0xdead_beef_cafe_babe,
            0x0123_4567_89ab_cdef,
        ];
        let mut xs = [0u64; MAX_N];
        for k in 0..MAX_N {
            xs[k] = edge_vals[k % edge_vals.len()].wrapping_add((k as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        }

        for rotated in [false, true] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let slot = b.slot((MAX_N * 8) as u32, 3, SlotKind::Temp);

            let entry = b.create_block(BlockKind::Normal);
            let hdr = b.create_block(BlockKind::Normal);
            let body = if rotated { hdr } else { b.create_block(BlockKind::Normal) };
            let exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let len = b.param(Ty::I64);
            let check_idx = b.param(Ty::I64);
            let pa = b.slot_addr(slot);
            for (k, &val) in xs.iter().enumerate() {
                let cv = b.iconst(Ty::I64, u128::from(val));
                b.store(Flags::NONE, cv, pa, (k * 8) as i32, MemAttrs::aligned(3));
            }
            let three = b.iconst(Ty::I64, 3);
            let eight = b.iconst(Ty::I64, 8);
            let init_acc = b.iconst(Ty::I64, u128::from(INIT_ACC));
            let bytes = b.binary(Op::Shl, Flags::NONE, len, three);
            let end_p = b.ptr_add(Flags::INBOUNDS, pa, bytes);
            if rotated {
                let non_empty = b.icmp(IntCond::Ne, pa, end_p);
                b.br(Flags::NONE, non_empty, (hdr, &[pa, init_acc]), (exit, &[init_acc]));

                b.begin(hdr);
                let p = b.param(Ty::PTR);
                let acc = b.param(Ty::I64);
                let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
                let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
                let acc_next = b.binary(Op::Add, Flags::NONE, acc, x);
                b.store(Flags::NONE, acc_next, p, 0, MemAttrs::aligned(3));
                let keep = b.icmp(IntCond::Ne, p_next, end_p);
                b.br(Flags::NONE, keep, (hdr, &[p_next, acc_next]), (exit, &[acc_next]));
            } else {
                b.jump((hdr, &[pa, init_acc]));

                b.begin(hdr);
                let p = b.param(Ty::PTR);
                let acc = b.param(Ty::I64);
                let keep = b.icmp(IntCond::Ne, p, end_p);
                b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

                b.begin(body);
                let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
                let x = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
                let acc_next = b.binary(Op::Add, Flags::NONE, acc, x);
                b.store(Flags::NONE, acc_next, p, 0, MemAttrs::aligned(3));
                b.jump((hdr, &[p_next, acc_next]));
            }

            b.begin(exit);
            let final_acc = b.param(Ty::I64);
            let check_off = b.binary(Op::Shl, Flags::NONE, check_idx, three);
            let check_ptr = b.ptr_add(Flags::NONE, pa, check_off);
            let elem_out = b.load(Ty::I64, Flags::NONE, check_ptr, 0, MemAttrs::aligned(3));
            b.ret(&[final_acc, elem_out]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1, "rotated={rotated}");
            verify(out.func.view()).unwrap();

            // Must not re-vectorize on a second pass.
            let second = auto_vectorize_loops(out.func.view(), blaze_own::testing::heap().alloc()).unwrap();
            assert_eq!(second.stats.loops_vectorized, 0, "rotated={rotated} re-vectorized");

            // Check that `scan_loop` has 8 loads and 8 stores of `Ty::I64`.
            let v = out.func.view();
            let scan_loop_blk = if rotated { Block(3) } else { Block(2) };
            let loop_loads = v
                .block(scan_loop_blk)
                .filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == Ty::I64)
                .count();
            let loop_stores = v.block(scan_loop_blk).filter(|&k| v.op(k) == Op::Store).count();
            assert_eq!(loop_loads, 8, "expected 8 scalar I64 loads in 8-way scan_loop");
            assert_eq!(loop_stores, 8, "expected 8 scalar I64 stores in 8-way scan_loop");

            for &n in &[0usize, 1, 2, 7, 8, 9, 15, 16, 17, 33, 64] {
                let mut expected_arr = xs;
                let mut expected_acc = INIT_ACC;
                for k in 0..n {
                    expected_acc = expected_acc.wrapping_add(expected_arr[k]);
                    expected_arr[k] = expected_acc;
                }
                let check_end = if n < MAX_N { n + 1 } else { MAX_N };
                for check_i in 0..check_end {
                    let args = [Val::Int(n as u128), Val::Int(check_i as u128)];
                    let Outcome::Returned(orig) =
                        eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                    else {
                        panic!("orig eval failed at rotated={rotated} n={n} idx={check_i}");
                    };
                    let Outcome::Returned(got) =
                        eval::run_in(out.func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                    else {
                        panic!("scan eval failed at rotated={rotated} n={n} idx={check_i}");
                    };
                    let want = [Val::Int(u128::from(expected_acc)), Val::Int(u128::from(expected_arr[check_i]))];
                    assert_eq!(orig.as_slice(), &want);
                    assert_eq!(
                        got.as_slice(),
                        orig.as_slice(),
                        "u64 prefix scan mismatch at rotated={rotated} n={n} idx={check_i}"
                    );
                }
            }
        }
    }

    #[test]
    fn vectorize_prefix_scan_i32_counted_inclusive_and_exclusive() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
        };

        const MAX_N: usize = 25;
        const INIT_ACC: u32 = 0xffff_fff5;
        let edge_vals: [u32; 6] = [u32::MAX, 0x8000_0000, 0x7fff_ffff, 0, 1, 0x1234_5678];
        let mut xs = [0u32; MAX_N];
        for k in 0..MAX_N {
            xs[k] = edge_vals[k % edge_vals.len()].wrapping_add((k as u32).wrapping_mul(0x9e37_79b9));
        }

        for inclusive in [true, false] {
            for use_mul in [false, true] {
                let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
                let sig = b.sig(
                    Conv::C,
                    Variadic::No,
                    &[Param::of(Ty::I64), Param::of(Ty::I64)],
                    &[Param::of(Ty::I32), Param::of(Ty::I32)],
                );
                b.set_signature(sig);
                let slot = b.slot((MAX_N * 4) as u32, 2, SlotKind::Temp);
                let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

                b.begin(entry);
                let len = b.param(Ty::I64);
                let check_idx = b.param(Ty::I64);
                let pa = b.slot_addr(slot);
                for (k, &val) in xs.iter().enumerate() {
                    let cv = b.iconst(Ty::I32, u128::from(val));
                    b.store(Flags::NONE, cv, pa, (k * 4) as i32, MemAttrs::aligned(2));
                }
                let zero_i = b.iconst(Ty::I64, 0);
                let one_i = b.iconst(Ty::I64, 1);
                let two_i = b.iconst(Ty::I64, 2);
                let four_i = b.iconst(Ty::I64, 4);
                let init_acc = b.iconst(Ty::I32, u128::from(INIT_ACC));
                b.jump((hdr, &[zero_i, init_acc]));

                b.begin(hdr);
                let i = b.param(Ty::I64);
                let acc = b.param(Ty::I32);
                let keep = b.icmp(IntCond::Ult, i, len);
                b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

                b.begin(body);
                let off = if use_mul {
                    b.binary(Op::Mul, Flags::NONE, i, four_i)
                } else {
                    b.binary(Op::Shl, Flags::NONE, i, two_i)
                };
                let ptr = b.ptr_add(Flags::NONE, pa, off);
                let x = b.load(Ty::I32, Flags::NONE, ptr, 0, MemAttrs::aligned(2));
                let acc_next = b.binary(Op::Add, Flags::NONE, x, acc);
                let to_store = if inclusive { acc_next } else { acc };
                b.store(Flags::NONE, to_store, ptr, 0, MemAttrs::aligned(2));
                let i_next = b.binary(Op::Add, Flags::NONE, i, one_i);
                b.jump((hdr, &[i_next, acc_next]));

                b.begin(exit);
                let final_acc = b.param(Ty::I32);
                let check_off = b.binary(Op::Shl, Flags::NONE, check_idx, two_i);
                let check_ptr = b.ptr_add(Flags::NONE, pa, check_off);
                let elem_out = b.load(Ty::I32, Flags::NONE, check_ptr, 0, MemAttrs::aligned(2));
                b.ret(&[final_acc, elem_out]);
                let func = b.finish_checked().unwrap();

                let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
                assert_eq!(out.stats.loops_vectorized, 1, "inclusive={inclusive} use_mul={use_mul}");
                verify(out.func.view()).unwrap();

                let second = auto_vectorize_loops(out.func.view(), blaze_own::testing::heap().alloc()).unwrap();
                assert_eq!(second.stats.loops_vectorized, 0);

                for &n in &[0usize, 1, 7, 8, 9, 25] {
                    let mut expected_arr = xs;
                    let mut expected_acc = INIT_ACC;
                    for k in 0..n {
                        let x = expected_arr[k];
                        let next = expected_acc.wrapping_add(x);
                        expected_arr[k] = if inclusive { next } else { expected_acc };
                        expected_acc = next;
                    }
                    let check_end = if n < MAX_N { n + 1 } else { MAX_N };
                    for check_i in 0..check_end {
                        let args = [Val::Int(n as u128), Val::Int(check_i as u128)];
                        let Outcome::Returned(orig) =
                            eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                        else {
                            panic!("orig i32 eval failed");
                        };
                        let Outcome::Returned(got) =
                            eval::run_in(out.func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                        else {
                            panic!("scan i32 eval failed");
                        };
                        let want = [Val::Int(u128::from(expected_acc)), Val::Int(u128::from(expected_arr[check_i]))];
                        assert_eq!(orig.as_slice(), &want);
                        assert_eq!(
                            got.as_slice(),
                            orig.as_slice(),
                            "i32 prefix scan mismatch at inclusive={inclusive} use_mul={use_mul} n={n} idx={check_i}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn vectorize_prefix_scan_does_not_match_mismatched_base() {
        use blaze_own::Alloc;

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let src = b.param(Ty::PTR);
        let dst = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let three = b.iconst(Ty::I64, 3);
        b.jump((hdr, &[zero, zero]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let keep = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, keep, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let off = b.binary(Op::Shl, Flags::NONE, i, three);
        let sp = b.ptr_add(Flags::NONE, src, off);
        let dp = b.ptr_add(Flags::NONE, dst, off);
        let x = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, x);
        b.store(Flags::NONE, acc_next, dp, 0, MemAttrs::aligned(3));
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[i_next, acc_next]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
        verify(out.func.view()).unwrap();
        assert_eq!(
            out.stats.loops_vectorized, 0,
            "mismatched load/store base pointers must not be transformed into an unguarded prefix scan"
        );
    }

    #[test]
    fn skips_byte_copy_vectorization_when_dominating_branch_bounds_trip_count_below_64() {
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
        };

        let build_seed_copy = |guard: Option<(IntCond, u64, bool)>, use_ptr_iv: bool| -> Func<TestHeap> {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I8)],
            );
            b.set_signature(sig);
            let src_slot = b.slot(32, 0, SlotKind::Temp);
            let dst_slot = b.slot(32, 0, SlotKind::Temp);
            let entry = b.create_block(BlockKind::Normal);
            let seed_pre = guard.map(|_| b.create_block(BlockKind::Normal));
            let header = b.create_block(BlockKind::Normal);
            let body = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let offset = b.param(Ty::I64);
            let check_idx = b.param(Ty::I64);
            let src = b.slot_addr(src_slot);
            let dst = b.slot_addr(dst_slot);
            let z8 = b.iconst(Ty::I8, 0);
            for k in 0..32u32 {
                let off_i32 = i32::try_from(k).unwrap();
                let pat = u8::try_from(((k + 1) * 7) & 0xff).unwrap();
                let cv = b.iconst(Ty::I8, u128::from(pat));
                b.store(Flags::NONE, cv, src, off_i32, MemAttrs::aligned(0));
                b.store(Flags::NONE, z8, dst, off_i32, MemAttrs::aligned(0));
            }
            let k0 = b.iconst(Ty::I64, 0);
            let k1 = b.iconst(Ty::I64, 1);
            let k3 = b.iconst(Ty::I64, 3);
            let k9 = b.iconst(Ty::I64, 9);

            if let (Some((cond, limit, true_enters)), Some(pre_blk)) = (guard, seed_pre) {
                let klim = b.iconst(Ty::I64, u128::from(limit));
                let cmp = b.icmp(cond, offset, klim);
                if true_enters {
                    b.br(Flags::NONE, cmp, (pre_blk, &[]), (exit, &[]));
                } else {
                    b.br(Flags::NONE, cmp, (exit, &[]), (pre_blk, &[]));
                }
                b.begin(pre_blk);
            }

            let is3 = b.icmp(IntCond::Eq, offset, k3);
            let off2 = b.binary(Op::Shl, Flags::NONE, offset, k1);
            let step = b.select(Flags::NONE, is3, k9, off2);

            if use_ptr_iv {
                let end_p = b.ptr_add(Flags::INBOUNDS, dst, step);
                b.jump((header, &[dst, src]));

                b.begin(header);
                let dp = b.param(Ty::PTR);
                let sp = b.param(Ty::PTR);
                let keep = b.icmp(IntCond::Ult, dp, end_p);
                b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

                b.begin(body);
                let byte = b.load(Ty::I8, Flags::NONE, sp, 0, MemAttrs::aligned(0));
                b.store(Flags::NONE, byte, dp, 0, MemAttrs::aligned(0));
                let dp_next = b.ptr_add(Flags::INBOUNDS, dp, k1);
                let sp_next = b.ptr_add(Flags::INBOUNDS, sp, k1);
                b.jump((header, &[dp_next, sp_next]));
            } else {
                b.jump((header, &[k0]));

                b.begin(header);
                let i = b.param(Ty::I64);
                let keep = b.icmp(IntCond::Ult, i, step);
                b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

                b.begin(body);
                let sp = b.ptr_add(Flags::NONE, src, i);
                let dp = b.ptr_add(Flags::NONE, dst, i);
                let byte = b.load(Ty::I8, Flags::NONE, sp, 0, MemAttrs::aligned(0));
                b.store(Flags::NONE, byte, dp, 0, MemAttrs::aligned(0));
                let i_next = b.binary(Op::Add, Flags::NONE, i, k1);
                b.jump((header, &[i_next]));
            }

            b.begin(exit);
            let chk_p = b.ptr_add(Flags::NONE, dst, check_idx);
            let out_b = b.load(Ty::I8, Flags::NONE, chk_p, 0, MemAttrs::aligned(0));
            b.ret(&[out_b]);
            b.finish_checked().unwrap()
        };

        let v16i8 = Ty::vector(crate::ty::Scalar::I8, 16).unwrap();

        // 1. Positive (skip): all 4 dominating branch forms proving `offset <= 7` -> `step <= 14 < 64`
        //    must NOT emit a 64-byte SIMD copy loop, and must preserve exact evaluator behavior.
        let small_guards = [
            (IntCond::Ult, 8u64, true),
            (IntCond::Ule, 7u64, true),
            (IntCond::Uge, 8u64, false),
            (IntCond::Ugt, 7u64, false),
        ];
        for use_ptr_iv in [false, true] {
            for &guard in &small_guards {
                let func = build_seed_copy(Some(guard), use_ptr_iv);
                let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
                verify(out.func.view()).unwrap();
                assert_eq!(
                    out.stats.loops_vectorized, 0,
                    "bounded step <= 14 < 64 must skip 64B vectorization (guard={guard:?}, ptr_iv={use_ptr_iv})"
                );
                let v = out.func.view();
                assert!(
                    !(0..v.len()).any(|k| Ty(v.tys[k]).is_vector()),
                    "expected zero vector ops when trip count is provably < 64 (guard={guard:?}, ptr_iv={use_ptr_iv})"
                );
            }

            // Evaluator equivalence on `offset < 8`: `offset in {3, 5, 6, 7}` copies `step` bytes, `offset == 8` skips.
            let func = build_seed_copy(Some((IntCond::Ult, 8, true)), use_ptr_iv);
            let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
            for &off in &[3u64, 5, 6, 7, 8] {
                let expected_step = if off < 8 {
                    if off == 3 { 9 } else { off << 1 }
                } else {
                    0
                };
                for &idx in &[0u64, 8, 9, 13, 14, 15] {
                    let args = [Val::Int(u128::from(off)), Val::Int(u128::from(idx))];
                    let Outcome::Returned(orig) = eval::run_in(func.view(), &args, 10_000, TestHeap).unwrap() else {
                        panic!("orig eval failed");
                    };
                    let Outcome::Returned(got) = eval::run_in(out.func.view(), &args, 10_000, TestHeap).unwrap() else {
                        panic!("out eval failed");
                    };
                    let want = if idx < expected_step { ((idx + 1) * 7) & 0xff } else { 0 };
                    assert_eq!(orig.as_slice(), &[Val::Int(u128::from(want))]);
                    assert_eq!(got.as_slice(), orig.as_slice());
                }
            }

            // Exact threshold: `offset <= 31` -> `step <= 62 < 64` skips; `offset <= 32` -> `step <= 64` vectorizes!
            let skip_62 = build_seed_copy(Some((IntCond::Ule, 31, true)), use_ptr_iv);
            let out_62 = auto_vectorize_loops(skip_62.view(), TestHeap).unwrap();
            assert_eq!(out_62.stats.loops_vectorized, 0, "step <= 62 < 64 must skip (ptr_iv={use_ptr_iv})");

            let vec_64 = build_seed_copy(Some((IntCond::Ule, 32, true)), use_ptr_iv);
            let out_64 = auto_vectorize_loops(vec_64.view(), TestHeap).unwrap();
            verify(out_64.func.view()).unwrap();
            assert_eq!(out_64.stats.loops_vectorized, 1, "step <= 64 must still vectorize (ptr_iv={use_ptr_iv})");

            // 2. Negative controls: unguarded (`None`) and `offset < 128` (`step <= 254 >= 64`) MUST still vectorize!
            for guard in [None, Some((IntCond::Ult, 128u64, true))] {
                let func = build_seed_copy(guard, use_ptr_iv);
                let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
                verify(out.func.view()).unwrap();
                assert_eq!(
                    out.stats.loops_vectorized, 1,
                    "unbounded / large-bound byte copy must still vectorize (guard={guard:?}, ptr_iv={use_ptr_iv})"
                );
                let v = out.func.view();
                let vloads = (0..v.len())
                    .filter(|&k| v.op(k) == Op::Load && Ty(v.tys[k]) == v16i8)
                    .count();
                let vstores = (0..v.len())
                    .filter(|&k| {
                        v.op(k) == Op::Store
                            && Ty(v.tys[to_idx(v.extra[to_idx(v.data[k])])]) == v16i8
                    })
                    .count();
                assert_eq!(vloads, 4, "expected 4x <16 x i8> vector loads (guard={guard:?}, ptr_iv={use_ptr_iv})");
                assert_eq!(vstores, 4, "expected 4x <16 x i8> vector stores (guard={guard:?}, ptr_iv={use_ptr_iv})");
            }
        }
    }

    #[test]
    fn skips_vectorization_for_and_mask_add_and_zext_bounded_trip_counts() {
        let build_masked_copy = |mask: u64, add_const: u64| -> Func<TestHeap> {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[],
            );
            b.set_signature(sig);
            let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            let k0 = b.iconst(Ty::I64, 0);
            let k1 = b.iconst(Ty::I64, 1);
            let km = b.iconst(Ty::I64, u128::from(mask));
            let masked = b.binary(Op::And, Flags::NONE, n, km);
            let bound = if add_const > 0 {
                let ka = b.iconst(Ty::I64, u128::from(add_const));
                b.binary(Op::Add, Flags::NONE, masked, ka)
            } else {
                masked
            };
            b.jump((header, &[k0]));

            b.begin(header);
            let i = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, bound);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

            b.begin(body);
            let sp = b.ptr_add(Flags::NONE, src, i);
            let dp = b.ptr_add(Flags::NONE, dst, i);
            let byte = b.load(Ty::I8, Flags::NONE, sp, 0, MemAttrs::aligned(0));
            b.store(Flags::NONE, byte, dp, 0, MemAttrs::aligned(0));
            let i_next = b.binary(Op::Add, Flags::NONE, i, k1);
            b.jump((header, &[i_next]));

            b.begin(exit);
            b.ret(&[]);
            b.finish_checked().unwrap()
        };

        // `(n & 15) + 4 <= 19 < 64` -> skip; `(n & 63) <= 63 < 64` -> skip; `(n & 63) + 1 <= 64` -> vectorize!
        let f_small = build_masked_copy(15, 4);
        assert_eq!(auto_vectorize_loops(f_small.view(), TestHeap).unwrap().stats.loops_vectorized, 0);

        let f_63 = build_masked_copy(63, 0);
        assert_eq!(auto_vectorize_loops(f_63.view(), TestHeap).unwrap().stats.loops_vectorized, 0);

        let f_64 = build_masked_copy(63, 1);
        let out_64 = auto_vectorize_loops(f_64.view(), TestHeap).unwrap();
        verify(out_64.func.view()).unwrap();
        assert_eq!(out_64.stats.loops_vectorized, 1);

        // `Zext(u8)` with `u8 < 16` -> `<= 15 < 64` skips; unbounded `Zext(u8)` -> `<= 255 >= 64` vectorizes!
        for bounded_u8 in [true, false] {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I8)],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let pre = bounded_u8.then(|| b.create_block(BlockKind::Normal));
            let header = b.create_block(BlockKind::Normal);
            let body = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let n8 = b.param(Ty::I8);
            let k0 = b.iconst(Ty::I64, 0);
            let k1 = b.iconst(Ty::I64, 1);
            if let Some(pre_blk) = pre {
                let k16 = b.iconst(Ty::I8, 16);
                let cmp = b.icmp(IntCond::Ult, n8, k16);
                b.br(Flags::NONE, cmp, (pre_blk, &[]), (exit, &[]));
                b.begin(pre_blk);
            }
            let n64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, n8);
            b.jump((header, &[k0]));

            b.begin(header);
            let i = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, n64);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

            b.begin(body);
            let sp = b.ptr_add(Flags::NONE, src, i);
            let dp = b.ptr_add(Flags::NONE, dst, i);
            let byte = b.load(Ty::I8, Flags::NONE, sp, 0, MemAttrs::aligned(0));
            b.store(Flags::NONE, byte, dp, 0, MemAttrs::aligned(0));
            let i_next = b.binary(Op::Add, Flags::NONE, i, k1);
            b.jump((header, &[i_next]));

            b.begin(exit);
            b.ret(&[]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
            verify(out.func.view()).unwrap();
            let want = if bounded_u8 { 0 } else { 1 };
            assert_eq!(out.stats.loops_vectorized, want, "bounded_u8={bounded_u8}");
        }
    }

    #[test]
    fn vectorizes_pointer_iter_slice_fill_u16_and_u32() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        const CAP: usize = 68;
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        const LENGTHS: [usize; 12] = [0, 1, 7, 8, 15, 16, 17, 31, 32, 33, 64, 65];

        let cases = [
            (Ty::I16, Ty::vector(Scalar::I16, 8).unwrap(), 1u32, 2usize, 1u8, 0x3412u64, 0xA500u64),
            (Ty::I32, Ty::vector(Scalar::I32, 4).unwrap(), 2u32, 4usize, 2u8, 0x0807_3412u64, 0xDEAD_0000u64),
        ];

        for (elem_ty, vec_ty, elem_shift, elem_bytes, align, fill_val, sentinel_base) in cases {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(elem_ty)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let slot = b.slot(u32::try_from(CAP * elem_bytes).unwrap(), align, SlotKind::Temp);
            let [entry, body, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let len = b.param(Ty::I64);
            let val = b.param(elem_ty);
            let base = b.slot_addr(slot);
            for k in 0..CAP {
                let sent = sentinel_base.wrapping_add(u64::try_from(k).unwrap());
                let sv = b.iconst(elem_ty, u128::from(sent));
                b.store(
                    Flags::NONE,
                    sv,
                    base,
                    i32::try_from(k * elem_bytes).unwrap(),
                    MemAttrs::aligned(align),
                );
            }
            let shift_c = b.iconst(Ty::I64, u128::from(elem_shift));
            let bytes = b.binary(Op::Shl, Flags::NONE, len, shift_c);
            let end = b.ptr_add(Flags::INBOUNDS, base, bytes);
            let step = b.iconst(Ty::I64, u128::try_from(elem_bytes).unwrap());
            let non_empty = b.icmp(IntCond::Ne, base, end);
            b.br(Flags::NONE, non_empty, (body, &[base]), (exit, &[]));

            b.begin(body);
            let p = b.param(Ty::PTR);
            b.store(Flags::NONE, val, p, 0, MemAttrs::aligned(align));
            let p_next = b.ptr_add(Flags::INBOUNDS, p, step);
            let keep = b.icmp(IntCond::Ne, p_next, end);
            b.br(Flags::NONE, keep, (body, &[p_next]), (exit, &[]));

            b.begin(exit);
            let mut hash = b.iconst(Ty::I64, u128::from(FNV_OFFSET));
            let prime = b.iconst(Ty::I64, u128::from(FNV_PRIME));
            for k in 0..CAP {
                let elem = b.load(
                    elem_ty,
                    Flags::NONE,
                    base,
                    i32::try_from(k * elem_bytes).unwrap(),
                    MemAttrs::aligned(align),
                );
                let elem64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, elem);
                hash = b.binary(Op::Xor, Flags::NONE, hash, elem64);
                hash = b.binary(Op::Mul, Flags::NONE, hash, prime);
            }
            b.ret(&[hash]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            assert_eq!(out.stats.loops_vectorized, 1, "expected pointer-IV fill to vectorize for {elem_ty:?}");
            verify(out.func.view()).unwrap();

            // Must not re-vectorize on a second pass.
            let second = auto_vectorize_loops(out.func.view(), blaze_own::testing::heap().alloc()).unwrap();
            assert_eq!(second.stats.loops_vectorized, 0, "fill loop must not re-vectorize");

            let v = out.func.view();
            // `PreTerm::Br` inserts `vec_guard` (Block 1), `vec_pre` (Block 2), `vec_loop` (Block 3).
            assert_eq!(
                v.block(Block(2)).filter(|&k| v.op(k) == Op::Splat && Ty(v.tys[k]) == vec_ty).count(),
                1,
                "expected 1 hoisted Op::Splat in vec_pre for {vec_ty:?}"
            );
            assert_eq!(
                v.block(Block(3)).filter(|&k| v.op(k) == Op::Splat).count(),
                0,
                "Op::Splat must be hoisted out of vec_loop"
            );
            assert_eq!(v.params(Block(3)).len(), 1, "expected single running pointer IV in vec_loop");
            let vec_stores: usize = v
                .block(Block(3))
                .filter(|&k| {
                    v.op(k) == Op::Store && Ty(v.tys[to_idx(v.extra[to_idx(v.data[k])])]) == vec_ty
                })
                .count();
            assert_eq!(vec_stores, 2, "expected 2x-unrolled 128-bit vector stores (32B = 1x stp q, q)");
            for k in v.block(Block(3)) {
                if v.op(k) == Op::Store {
                    assert_eq!(
                        v.mem_attrs(k).map(|m| m.align),
                        Some(align),
                        "vector store must preserve original scalar alignment"
                    );
                }
            }
            assert_eq!(
                (0..v.len()).filter(|&k| matches!(v.op(k), Op::Umin | Op::Neg)).count(),
                0,
                "pure fill loop needs zero runtime alias check"
            );
            assert_eq!(
                (0..v.len()).filter(|&k| v.op(k) == Op::PtrAddr).count(),
                0,
                "inbounds slice fill needs zero Op::PtrAddr"
            );

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for n in LENGTHS {
                let mut expected_hash = FNV_OFFSET;
                for k in 0..CAP {
                    let elem = if k < n {
                        fill_val
                    } else {
                        sentinel_base.wrapping_add(u64::try_from(k).unwrap())
                    };
                    expected_hash = (expected_hash ^ elem).wrapping_mul(FNV_PRIME);
                }
                let args = [Val::Int(u128::try_from(n).unwrap()), Val::Int(u128::from(fill_val))];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig fill eval failed for {elem_ty:?} at n={n}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec fill eval failed for {elem_ty:?} at n={n}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected_hash))]);
                assert_eq!(
                    got.as_slice(),
                    orig.as_slice(),
                    "pointer-IV slice fill mismatch for {elem_ty:?} at n={n}"
                );
            }
        }

        // Negative control 1: a loop with TWO stores must NOT drop one store or vectorize.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I16)],
                &[],
            );
            b.set_signature(sig);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let d0 = b.param(Ty::PTR);
            let d1 = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            let val = b.param(Ty::I16);
            let z = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            b.jump((hdr, &[z]));
            b.begin(hdr);
            let i = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));
            b.begin(body);
            let off = b.binary(Op::Shl, Flags::NONE, i, one);
            let p0 = b.ptr_add(Flags::NONE, d0, off);
            let p1 = b.ptr_add(Flags::NONE, d1, off);
            b.store(Flags::NONE, val, p0, 0, MemAttrs::aligned(1));
            b.store(Flags::NONE, val, p1, 0, MemAttrs::aligned(1));
            let i_next = b.binary(Op::Add, Flags::NONE, i, one);
            b.jump((hdr, &[i_next]));
            b.begin(exit);
            b.ret(&[]);
            let func = b.finish_checked().unwrap();
            let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
            assert_eq!(out.stats.loops_vectorized, 0, "two-store loop must not vectorize");
        }

        // Negative control 2: storing an IV-dependent value must NOT vectorize as a constant fill.
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[],
            );
            b.set_signature(sig);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let dst = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            let z = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let three = b.iconst(Ty::I64, 3);
            b.jump((hdr, &[z]));
            b.begin(hdr);
            let i = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));
            b.begin(body);
            let off = b.binary(Op::Shl, Flags::NONE, i, three);
            let p = b.ptr_add(Flags::NONE, dst, off);
            b.store(Flags::NONE, i, p, 0, MemAttrs::aligned(3));
            let i_next = b.binary(Op::Add, Flags::NONE, i, one);
            b.jump((hdr, &[i_next]));
            b.begin(exit);
            b.ret(&[]);
            let func = b.finish_checked().unwrap();
            let out = auto_vectorize_loops(func.view(), TestHeap).unwrap();
            assert_eq!(out.stats.loops_vectorized, 0, "IV-dependent store must not vectorize as constant fill");
        }
    }

    #[test]
    fn vectorizes_counted_slice_fill_u8_and_i64() {
        use blaze_own::Alloc;
        use crate::{
            eval::{self, Outcome, Val},
            func::SlotKind,
            legalize::legalize,
            ty::Scalar,
        };

        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

        let u8_lengths: &[usize] = &[0, 1, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 70];
        let i64_lengths: &[usize] = &[0, 1, 2, 3, 4, 5, 7, 8, 9, 16, 17, 33];

        let cases: [(Ty, Ty, u32, usize, u8, usize, u64, u64, Option<u64>, &[usize]); 3] = [
            (
                Ty::I8,
                Ty::vector(Scalar::I8, 16).unwrap(),
                0,
                1,
                0,
                72,
                0x5Au64,
                0x80u64,
                None,
                u8_lengths,
            ),
            (
                Ty::I64,
                Ty::vector(Scalar::I64, 2).unwrap(),
                3,
                8,
                3,
                36,
                0x1122_3344_5566_7788u64,
                0xCAFE_BABE_0000_0000u64,
                None,
                i64_lengths,
            ),
            (
                Ty::I64,
                Ty::vector(Scalar::I64, 2).unwrap(),
                3,
                8,
                3,
                36,
                0xDEAD_BEEF_CAFE_BABEu64,
                0x1357_9BDF_0000_0000u64,
                Some(0xDEAD_BEEF_CAFE_BABEu64),
                i64_lengths,
            ),
        ];

        for (elem_ty, vec_ty, elem_shift, elem_bytes, align, cap, fill_val, sentinel_base, body_const, lengths) in cases {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, blaze_own::testing::heap().alloc());
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(elem_ty)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let slot = b.slot(u32::try_from(cap * elem_bytes).unwrap(), align, SlotKind::Temp);
            let [entry, hdr, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let val_param = b.param(elem_ty);
            let base = b.slot_addr(slot);
            for k in 0..cap {
                let sent = if elem_ty == Ty::I8 {
                    (sentinel_base.wrapping_add(u64::try_from(k).unwrap())) & 0xFF
                } else {
                    sentinel_base.wrapping_add(u64::try_from(k).unwrap())
                };
                let sv = b.iconst(elem_ty, u128::from(sent));
                b.store(
                    Flags::NONE,
                    sv,
                    base,
                    i32::try_from(k * elem_bytes).unwrap(),
                    MemAttrs::aligned(align),
                );
            }
            let z64 = b.iconst(Ty::I64, 0);
            let one64 = b.iconst(Ty::I64, 1);
            b.jump((hdr, &[z64]));

            b.begin(hdr);
            let i = b.param(Ty::I64);
            let keep = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

            b.begin(body);
            let dp = if elem_shift == 0 {
                b.ptr_add(Flags::NONE, base, i)
            } else {
                let sh = b.iconst(Ty::I64, u128::from(elem_shift));
                let off = b.binary(Op::Shl, Flags::NONE, i, sh);
                b.ptr_add(Flags::NONE, base, off)
            };
            let to_store = match body_const {
                Some(c) => b.iconst(elem_ty, u128::from(c)),
                None => val_param,
            };
            b.store(Flags::NONE, to_store, dp, 0, MemAttrs::aligned(align));
            let i_next = b.binary(Op::Add, Flags::NONE, i, one64);
            b.jump((hdr, &[i_next]));

            b.begin(exit);
            let mut hash = b.iconst(Ty::I64, u128::from(FNV_OFFSET));
            let prime = b.iconst(Ty::I64, u128::from(FNV_PRIME));
            for k in 0..cap {
                let elem = b.load(
                    elem_ty,
                    Flags::NONE,
                    base,
                    i32::try_from(k * elem_bytes).unwrap(),
                    MemAttrs::aligned(align),
                );
                let elem64 = if elem_ty == Ty::I64 {
                    elem
                } else {
                    b.convert(Op::Zext, Flags::NONE, Ty::I64, elem)
                };
                hash = b.binary(Op::Xor, Flags::NONE, hash, elem64);
                hash = b.binary(Op::Mul, Flags::NONE, hash, prime);
            }
            b.ret(&[hash]);
            let func = b.finish_checked().unwrap();

            let out = auto_vectorize_loops(func.view(), blaze_own::testing::heap().alloc()).unwrap();
            assert_eq!(
                out.stats.loops_vectorized, 1,
                "expected counted fill to vectorize for {elem_ty:?} (body_const={})",
                body_const.is_some()
            );
            verify(out.func.view()).unwrap();

            let second = auto_vectorize_loops(out.func.view(), blaze_own::testing::heap().alloc()).unwrap();
            assert_eq!(second.stats.loops_vectorized, 0, "counted fill must not re-vectorize");

            let v = out.func.view();
            // `PreTerm::Jump` inserts `vec_pre` (Block 1), `vec_loop` (Block 2), `vec_reduce` (Block 3).
            assert_eq!(
                v.block(Block(1)).filter(|&k| v.op(k) == Op::Splat && Ty(v.tys[k]) == vec_ty).count(),
                1,
                "expected 1 hoisted Op::Splat in vec_pre for {vec_ty:?}"
            );
            assert_eq!(
                v.block(Block(2)).filter(|&k| v.op(k) == Op::Splat).count(),
                0,
                "Op::Splat must be hoisted out of vec_loop"
            );
            assert_eq!(
                v.block(Block(2)).filter(|&k| v.op(k) == Op::Shl).count(),
                0,
                "pointer-induction vec_loop must have zero Op::Shl"
            );
            let vec_stores: usize = v
                .block(Block(2))
                .filter(|&k| {
                    v.op(k) == Op::Store && Ty(v.tys[to_idx(v.extra[to_idx(v.data[k])])]) == vec_ty
                })
                .count();
            assert_eq!(vec_stores, 2, "expected 2x-unrolled 128-bit vector stores for {vec_ty:?}");
            assert_eq!(
                (0..v.len()).filter(|&k| matches!(v.op(k), Op::Umin | Op::Neg)).count(),
                0,
                "pure fill loop needs zero runtime alias check"
            );

            let mut legalized = out.func;
            legalize(&mut legalized, &NoRoutines, blaze_own::testing::heap().alloc()).unwrap();
            verify(legalized.view()).unwrap();

            for &len_val in lengths {
                let mut expected_hash = FNV_OFFSET;
                for k in 0..cap {
                    let elem = if k < len_val {
                        fill_val
                    } else if elem_ty == Ty::I8 {
                        (sentinel_base.wrapping_add(u64::try_from(k).unwrap())) & 0xFF
                    } else {
                        sentinel_base.wrapping_add(u64::try_from(k).unwrap())
                    };
                    expected_hash = (expected_hash ^ elem).wrapping_mul(FNV_PRIME);
                }
                let args = [Val::Int(u128::try_from(len_val).unwrap()), Val::Int(u128::from(fill_val))];
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("orig counted fill eval failed for {elem_ty:?} at n={len_val}");
                };
                let Outcome::Returned(got) =
                    eval::run_in(legalized.view(), &args, 200_000, blaze_own::testing::heap().alloc()).unwrap()
                else {
                    panic!("vec counted fill eval failed for {elem_ty:?} at n={len_val}");
                };
                assert_eq!(orig.as_slice(), &[Val::Int(u128::from(expected_hash))]);
                assert_eq!(
                    got.as_slice(),
                    orig.as_slice(),
                    "counted slice fill mismatch for {elem_ty:?} at n={len_val}"
                );
            }
        }
    }
}

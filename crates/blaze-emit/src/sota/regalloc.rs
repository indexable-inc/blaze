//! Spill-Capable, Loop-Weighted, Coalescing Chordal-SSA Register Allocator (GPR + Pair + Scalar FP)
//! for the non-LLVM `Sota` AArch64 tier.
//!
//! Because `blaze-lir` is in strict SSA form with block parameters, its interference graph is
//! chordal. This allocator:
//! 1. Computes exact `[def, last_use]` intervals (extended to a fixed point across backward loop edges)
//!    and per-block loop nesting depth so hot-loop values get priority over cold outer values.
//! 2. Propagates **coalescing hints** for:
//!    - Entry parameters (`X0..X7`, `X8` for `sret`, `V0..V7`)
//!    - Block-parameter (`phi`) back-edges so loop induction variables & accumulators stay in the
//!      same physical register across iterations (zero back-edge `mov`s!)
//!    - Return values (`X0`/`X1`, `V0`) and call arguments (`X0..X7`, `V0..V7`)
//! 3. Supports **2-word GPR pairs** (`AllocLoc::GprPair` / `AllocLoc::GprPairSpill`) for
//!    `{i64, i1}` overflow ops and 2-register returns (`Option<u64>`, `Result<T, E>`, `&[T]`).
//! 4. Performs **loop-depth-weighted spill eviction**: when physical registers saturate inside a
//!    hot loop, evicts the lowest-weight (coldest) active interval to a reusable 8-byte stack
//!    spill slot so the inner loop stays register-resident.
//! 5. Keeps pure leaf functions completely frameless (`X0..X15`, `V0..V7`/`V16..V28`, zero stack).

use core::alloc::Allocator;

use blaze_lir::{
    Block, Op, Ty, View,
    func::{ParamKind, SigId},
};
use blaze_own::Vec;
use blaze_tier::{CallClobbers, Reg as TierReg, RegSet};

use crate::enc::{Reg, X0, X1, X2, X8, X9, X10, X11, X12, X13, X14, X15};

const X3: Reg = 3;
const X4: Reg = 4;
const X5: Reg = 5;
const X6: Reg = 6;
const X7: Reg = 7;
const X19: Reg = 19;
const X20: Reg = 20;
const X21: Reg = 21;
const X22: Reg = 22;
const X23: Reg = 23;
const X24: Reg = 24;
const X25: Reg = 25;
const X26: Reg = 26;
const X27: Reg = 27;
const X28: Reg = 28;

/// Unassigned hint sentinel.
pub(crate) const NO_REG: u8 = u8::MAX;

#[inline]
fn idx(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[inline]
fn reg_u8(r: Reg) -> u8 {
    u8::try_from(r).unwrap_or(NO_REG)
}

#[inline]
fn sext32_u64(w: u32) -> u64 {
    i64::from(w.cast_signed()).cast_unsigned()
}

/// Caller-saved allocatable GPRs (`X16`/`X17` stay permanently free as scratch/reload/cycle-breakers,
/// `X18` is platform-reserved on macOS, `X8` is indirect-result/caller-saved).
const CALLER_SAVED_GPR: &[Reg] = &[X9, X10, X11, X12, X13, X14, X15, X8, X0, X1, X2, X3, X4, X5, X6, X7];

/// Callee-saved allocatable GPRs (saved/restored in canonical ARM64 unwind pairs `x19:x20..x27:x28`).
const CALLEE_SAVED_GPR: &[Reg] = &[X19, X20, X21, X22, X23, X24, X25, X26, X27, X28];

/// Caller-saved allocatable FP registers (`V29`/`V30`/`V31` stay permanently free as FP scratch/reload/cycle-breakers).
const CALLER_SAVED_FP: &[Reg] = &[
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
    0, 1, 2, 3, 4, 5, 6, 7,
];

/// Callee-saved allocatable FP registers (low 64 bits `d8..d15`, saved/restored in canonical pairs `d8:d9..d14:d15`).
const CALLEE_SAVED_FP: &[Reg] = &[8, 9, 10, 11, 12, 13, 14, 15];

/// AAPCS64 integer argument registers `X0..X7`.
pub(crate) const ARG_REGS: [Reg; 8] = [X0, X1, X2, X3, X4, X5, X6, X7];

/// AAPCS64 floating-point argument registers `V0..V7`.
pub(crate) const FP_ARG_REGS: [Reg; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// Physical location assigned to a single LIR SSA value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AllocLoc {
    /// Void, unused, or fused/skipped value.
    None,
    /// Physical AArch64 GPR (`X0..X15`, `X19..X28`).
    Gpr(u8),
    /// Physical AArch64 scalar FP register (`V0..V28`).
    Fp(u8),
    /// 8-byte stack spill slot holding a GPR integer/pointer value.
    GprSpill(u32),
    /// 8-byte stack spill slot holding an `f32`/`f64` value.
    FpSpill(u32),
    /// Two-word GPR pair (`(lo_reg, hi_reg)`) for overflow results & 2-word register returns.
    GprPair(u8, u8),
    /// Two-word GPR pair stored in two 8-byte stack spill slots (`(lo_slot, hi_slot)`).
    GprPairSpill(u32, u32),
    /// Physical AArch64 128-bit NEON SIMD vector register (`V0..V28`).
    Vec(u8),
    /// 16-byte stack spill slot pair (`slot`, `slot + 1`) holding a 128-bit SIMD vector.
    VecSpill(u32),
    /// Rematerialized 64-bit integer constant: re-emitted on use via `movz`/`movk` with ZERO stack spill!
    RematConst(u64),
}

/// Whether `ty` is a supported scalar float (`Some(false)` for `f32`, `Some(true)` for `f64`).
#[inline]
pub(crate) fn is_fp_ty(ty: Ty) -> Option<bool> {
    if ty == Ty::F32 {
        Some(false)
    } else if ty == Ty::F64 {
        Some(true)
    } else {
        None
    }
}

/// Whether `ty` is a supported 128-bit fixed SIMD vector (`<16 x i8>`, `<8 x i16>`, `<4 x i32>`,
/// `<2 x i64>`, `<4 x f32>`, `<2 x f64>`). Returns `Some((elem_size_log2, is_fp))`.
#[inline]
pub(crate) fn is_vec128_ty(ty: Ty) -> Option<(u32, bool)> {
    if ty.form() != blaze_lir::ty::Form::Vector {
        return None;
    }
    let elem = ty.elem();
    let lanes = ty.lanes();
    let ebits = elem.bits(64);
    if ebits * lanes != 128 {
        return None;
    }
    if elem.is_float() {
        match ebits {
            32 => Some((2, true)),
            64 => Some((3, true)),
            _ => None,
        }
    } else if elem.is_int() {
        match ebits {
            8 => Some((0, false)),
            16 => Some((1, false)),
            32 => Some((2, false)),
            64 => Some((3, false)),
            _ => None,
        }
    } else {
        None
    }
}

/// Whether `ty` is a supported 256-bit fixed SIMD vector (`<32 x i8>`, `<16 x i16>`, `<8 x i32>`,
/// `<4 x i64>`, `<8 x f32>`, `<4 x f64>`). Returns `Some((elem_size_log2, is_fp))`.
#[inline]
pub(crate) fn is_vec256_ty(ty: Ty) -> Option<(u32, bool)> {
    if ty.form() != blaze_lir::ty::Form::Vector {
        return None;
    }
    let elem = ty.elem();
    let lanes = ty.lanes();
    let ebits = elem.bits(64);
    if ebits * lanes != 256 {
        return None;
    }
    if elem.is_float() {
        match ebits {
            32 => Some((2, true)),
            64 => Some((3, true)),
            _ => None,
        }
    } else if elem.is_int() {
        match ebits {
            8 => Some((0, false)),
            16 => Some((1, false)),
            32 => Some((2, false)),
            64 => Some((3, false)),
            _ => None,
        }
    } else {
        None
    }
}

/// Whether `ty` is a supported 512-bit AVX-512 fixed SIMD vector (`<64 x i8>`, `<32 x i16>`,
/// `<16 x i32>`, `<8 x i64>`, `<16 x f32>`, `<8 x f64>`). Returns `Some((elem_size_log2, is_fp))`.
#[inline]
pub(crate) fn is_vec512_ty(ty: Ty) -> Option<(u32, bool)> {
    if ty.form() != blaze_lir::ty::Form::Vector {
        return None;
    }
    let elem = ty.elem();
    let lanes = ty.lanes();
    let ebits = elem.bits(64);
    if ebits * lanes != 512 {
        return None;
    }
    if elem.is_float() {
        match ebits {
            32 => Some((2, true)),
            64 => Some((3, true)),
            _ => None,
        }
    } else if elem.is_int() {
        match ebits {
            8 => Some((0, false)),
            16 => Some((1, false)),
            32 => Some((2, false)),
            64 => Some((3, false)),
            _ => None,
        }
    } else {
        None
    }
}

/// Whether `ty` is a vector lane mask (`<lanes x i1>` for `lanes in {2, 4, 8, 16, 32, 64}`).
#[inline]
pub(crate) fn is_vec_mask_ty(ty: Ty) -> Option<u32> {
    if ty.form() != blaze_lir::ty::Form::Vector {
        return None;
    }
    if ty.elem() != blaze_lir::Scalar::I1 {
        return None;
    }
    let lanes = ty.lanes();
    if matches!(lanes, 2 | 4 | 8 | 16 | 32 | 64) { Some(lanes) } else { None }
}

/// Whether `ty` is a supported 2-word integer/pointer value (`i128`, or compound `{i64, i1}`, `{ptr, i64}`).
#[inline]
pub(crate) fn is_gpr_pair_ty(view: &View<'_>, ty: Ty) -> bool {
    if ty == Ty::I128 {
        return true;
    }
    if ty.compound_row().is_none() {
        return false;
    }
    let fields = view.members(ty);
    if fields.len() != 2 {
        return false;
    }
    fields.iter().all(|&f| {
        let ft = Ty(f);
        ft.is_ptr() || (ft.is_scalar() && ft.is_int() && ft.bits(64) <= 64)
    })
}

/// Why register allocation could not fit within the SOTA frame spill budget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RegAllocError {
    /// Required spill slots exceeded the fast direct-offset spill budget.
    OutOfSpillSlots,
}

/// Complete physical register & spill-slot assignment for a verified LIR function.
pub(crate) struct Allocation<A: Allocator> {
    /// Physical register or spill slot per LIR instruction index.
    pub(crate) loc_of: Vec<AllocLoc, A>,
    /// 5-bit mask of saved callee-saved GPR pairs (`bit k` = `x(19+2k):x(20+2k)`), matching `blaze_code::unwind::Frame::x_pairs`.
    pub(crate) x_pairs: u8,
    /// 4-bit mask of saved callee-saved FP pairs (`bit k` = `d(8+2k):d(9+2k)`), matching `blaze_code::unwind::Frame::d_pairs`.
    pub(crate) d_pairs: u8,
    /// Number of 8-byte stack spill slots allocated (`0` on the fast zero-spill path).
    pub(crate) num_spill_slots: u32,
    /// `true` iff the function contains any non-tail call (requiring `FP`/`LR` frame record).
    pub(crate) has_calls: bool,
    /// Exact IPRA physical register clobber summary for callers of this function.
    pub(crate) clobbers: CallClobbers,
    /// Number of constants rematerialized instead of spilled to the stack.
    pub(crate) remat_count: u32,
}

/// Visits every value operand read by `inst` (including block-edge arguments).
pub(crate) fn for_each_use(view: &View<'_>, inst: usize, mut f: impl FnMut(u32)) {
    use blaze_lir::op::{Field, Layout};
    let op = view.op(inst);
    match op.info().layout {
        Layout::None => {}
        Layout::Data(Field::Value) => {
            if let Some(&v) = view.data.get(inst) {
                f(v);
            }
        }
        Layout::Data(_) => {}
        Layout::Extra(fields) => {
            let words = view.extra;
            let Some(&data_at) = view.data.get(inst) else {
                return;
            };
            let mut at = idx(data_at);
            let mut scrutinee = None;
            for &field in fields {
                match field {
                    Field::Value => {
                        let Some(&w) = words.get(at) else {
                            return;
                        };
                        scrutinee = Some(w);
                        f(w);
                        at += 1;
                    }
                    Field::Block
                    | Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => at += 1,
                    Field::Target => {
                        let Some(&n_raw) = words.get(at + 1) else {
                            return;
                        };
                        let n = idx(n_raw);
                        if let Some(args) = words.get(at + 2..at + 2 + n) {
                            for &v in args {
                                f(v);
                            }
                        }
                        at += 2 + n;
                    }
                    Field::Values => {
                        let Some(&n_raw) = words.get(at) else {
                            return;
                        };
                        let n = idx(n_raw);
                        if let Some(vals) = words.get(at + 1..at + 1 + n) {
                            for &v in vals {
                                f(v);
                            }
                        }
                        at += 1 + n;
                    }
                    Field::Blocks | Field::Mask => {
                        let Some(&n_raw) = words.get(at) else {
                            return;
                        };
                        let n = idx(n_raw);
                        at += 1 + n;
                    }
                    Field::Const => {
                        let Some(&ty_raw) = view.tys.get(inst) else {
                            return;
                        };
                        at += idx(Ty(ty_raw).const_words());
                    }
                    Field::Cases => {
                        let width = scrutinee
                            .and_then(|it: u32| view.tys.get(idx(it)).copied())
                            .map_or(0, |ty_raw| idx(Ty(ty_raw).const_words()));
                        let Some(&n_raw) = words.get(at) else {
                            return;
                        };
                        let n = idx(n_raw);
                        at += 1;
                        for _ in 0..n {
                            at += width;
                            let Some(&argc_raw) = words.get(at + 1) else {
                                return;
                            };
                            let argc = idx(argc_raw);
                            if let Some(args) = words.get(at + 2..at + 2 + argc) {
                                for &v in args {
                                    f(v);
                                }
                            }
                            at += 2 + argc;
                        }
                    }
                }
            }
        }
    }
}

/// Visits every CFG edge target `(target_block, args_start, args_len)` of terminator `inst`.
pub(crate) fn for_each_edge(view: &View<'_>, inst: usize, mut f: impl FnMut(u32, usize, usize)) {
    use blaze_lir::op::{Field, Layout};
    let op = view.op(inst);
    let Layout::Extra(fields) = op.info().layout else {
        return;
    };
    let words = view.extra;
    let Some(&data_at) = view.data.get(inst) else {
        return;
    };
    let mut at = idx(data_at);
    let mut scrutinee = None;
    for &field in fields {
        match field {
            Field::Value => {
                let Some(&w) = words.get(at) else {
                    return;
                };
                scrutinee = Some(w);
                at += 1;
            }
            Field::Block => {
                let Some(&w) = words.get(at) else {
                    return;
                };
                f(w, 0, 0);
                at += 1;
            }
            Field::Imm
            | Field::Offset
            | Field::IntCond
            | Field::FloatCond
            | Field::Rmw
            | Field::Ref
            | Field::Sig
            | Field::Slot
            | Field::Intrinsic
            | Field::Asm => at += 1,
            Field::Target => {
                let (Some(&block), Some(&n_raw)) = (words.get(at), words.get(at + 1)) else {
                    return;
                };
                let n = idx(n_raw);
                f(block, at + 2, n);
                at += 2 + n;
            }
            Field::Values | Field::Blocks | Field::Mask => {
                let Some(&n_raw) = words.get(at) else {
                    return;
                };
                let n = idx(n_raw);
                at += 1 + n;
            }
            Field::Const => {
                let Some(&ty_raw) = view.tys.get(inst) else {
                    return;
                };
                at += idx(Ty(ty_raw).const_words());
            }
            Field::Cases => {
                let width = scrutinee
                    .and_then(|it: u32| view.tys.get(idx(it)).copied())
                    .map_or(0, |ty_raw| idx(Ty(ty_raw).const_words()));
                let Some(&n_raw) = words.get(at) else {
                    return;
                };
                let n = idx(n_raw);
                at += 1;
                for _ in 0..n {
                    at += width;
                    let (Some(&block), Some(&n_raw)) = (words.get(at), words.get(at + 1)) else {
                        return;
                    };
                    let n = idx(n_raw);
                    f(block, at + 2, n);
                    at += 2 + n;
                }
            }
        }
    }
}

/// Active register-allocated interval entry for loop-weighted eviction.
/// Allocates physical AArch64 GPRs, 2-word GPR pairs, scalar FP/NEON registers, rematerialized
/// constants (`AllocLoc::RematConst`), and (only on true pressure overflow) reusable 8-byte stack
/// spill slots for all non-fused values in `view`, using conservative AAPCS64 call clobbers.
pub(crate) fn allocate<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    tail_calls: &[bool],
    max_spill_slots: u32,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    allocate_with_ipra(view, skip_alloc, tail_calls, max_spill_slots, |_| CallClobbers::C_ABI_ARM64, alloc)
}

/// IPRA-aware Greedy Priority-Weighted Graph-Coloring Register Allocator with True Constant
/// Rematerialization:
/// - Colors highest-loop-weight live intervals first against exact per-register interval sets.
/// - Preserves live values in caller-saved GPRs/FPs across internal calls whose `call_clobbers`
///   mask proves those registers untouched (zero callee-saved saves, zero spills!).
/// - Rematerializes evicted/uncolored `Iconst`/`IconstWide` values on-use (`AllocLoc::RematConst`)
///   instead of allocating stack spill slots.
/// - Emits the function's exact [`CallClobbers`] summary for downstream callers.
pub(crate) fn allocate_with_ipra<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    tail_calls: &[bool],
    max_spill_slots: u32,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    let len = view.len();
    let blocks = view.blocks();
    let mut last_use: Vec<usize, A> = Vec::with_capacity_in(len, alloc);
    let mut has_any_use: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut use_weight: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut inst_block: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut call_positions: Vec<usize, A> = Vec::new_in(alloc);
    for i in 0..len {
        last_use.push(i);
        has_any_use.push(false);
        use_weight.push(0);
        inst_block.push(0);
    }

    // 0. Compute per-block loop nesting depth from backward CFG edges (`target <= b`).
    let mut block_depth: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_depth.push(0);
    }
    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        for_each_edge(view, term, |target_block, _, _| {
            let tb = idx(target_block);
            if tb <= b {
                if let Some(slice) = block_depth.get_mut(tb..=b) {
                    for d in slice {
                        *d = d.saturating_add(1);
                    }
                }
            }
        });
    }

    // 1. Compute live intervals `[def, last_use]`, call sites, and loop-weighted spill priorities.
    for b in 0..blocks {
        let depth = block_depth.get(b).copied().unwrap_or(0).min(4);
        let b_weight = 1u32 << (depth * 3); // 1x outer, 8x depth-1 loop, 64x depth-2 loop, ...
        let range = view.block(Block(u32_of(b)));
        for inst in range {
            if let Some(slot) = inst_block.get_mut(inst) {
                *slot = u32_of(b);
            }
            let op = view.op(inst);
            let is_runtime_bulk = matches!(op, Op::Memcpy | Op::Memset | Op::Memcmp) && {
                let const_ok = view
                    .data
                    .get(inst)
                    .and_then(|&d| view.extra.get(idx(d) + 2))
                    .map(|&v| idx(v))
                    .is_some_and(|len_v| match view.op(len_v) {
                        Op::Iconst => view.data.get(len_v).is_some_and(|&w| sext32_u64(w) <= 256),
                        Op::IconstWide => view.data.get(len_v).is_some_and(|&d| {
                            let at = idx(d);
                            match (view.extra.get(at), view.extra.get(at + 1)) {
                                (Some(&lo), Some(&hi)) => (u64::from(lo) | (u64::from(hi) << 32)) <= 256,
                                _ => false,
                            }
                        }),
                        _ => false,
                    });
                !const_ok
            };
            let is_i128_div = view.tys.get(inst).copied().map(Ty) == Some(Ty::I128)
                && matches!(op, Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem);
            let is_non_tail_call = (matches!(
                op,
                Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::Resume
                    | Op::TlsAddr
                    | Op::Memmove
                    | Op::Frem
            ) || is_runtime_bulk
                || is_i128_div)
                && !tail_calls.get(inst).copied().unwrap_or(false);
            if is_non_tail_call {
                call_positions.push(inst);
            }
            // Value defined inside a hot loop carries that loop's weight.
            if let Some(w) = use_weight.get_mut(inst) {
                *w = w.saturating_add(b_weight);
            }
            for_each_use(view, inst, |v| {
                let vi = idx(v);
                if vi < len {
                    if let Some(u) = has_any_use.get_mut(vi) {
                        *u = true;
                    }
                    if let Some(w) = use_weight.get_mut(vi) {
                        *w = w.saturating_add(b_weight);
                    }
                    if let Some(lu) = last_use.get_mut(vi) {
                        if inst > *lu {
                            *lu = inst;
                        }
                    }
                }
            });
        }
    }

    // Propagate fused producers' operand lifetimes to their consumer's position (reverse scan).
    for inst in (0..len).rev() {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            let consumer_end = last_use.get(inst).copied().unwrap_or(0);
            for_each_use(view, inst, |v| {
                let vi = idx(v);
                if let Some(lu) = last_use.get_mut(vi) {
                    if consumer_end > *lu {
                        *lu = consumer_end;
                    }
                }
            });
        }
    }

    // Extend live intervals across backward CFG edges (loops and backward unwind edges) to a fixed point.
    loop {
        let mut changed = false;
        for b in 0..blocks {
            let term = view.terminator(Block(u32_of(b)));
            let term_is_invoke = matches!(view.op(term), Op::Invoke | Op::InvokeIndirect);
            for_each_edge(view, term, |target_block, _, _| {
                let target_start = view.block(Block(target_block)).start;
                if target_start <= term {
                    let ext_end = if term_is_invoke { term.saturating_add(1) } else { term };
                    if let (Some(used_slice), Some(lu_slice)) =
                        (has_any_use.get(..target_start), last_use.get_mut(..target_start))
                    {
                        for (&used, lu) in used_slice.iter().zip(lu_slice.iter_mut()) {
                            if used && *lu >= target_start && *lu < ext_end {
                                *lu = ext_end;
                                changed = true;
                            }
                        }
                    }
                }
            });
        }
        if !changed {
            break;
        }
        for inst in (0..len).rev() {
            if skip_alloc.get(inst).copied().unwrap_or(false) {
                let consumer_end = last_use.get(inst).copied().unwrap_or(0);
                for_each_use(view, inst, |v| {
                    let vi = idx(v);
                    if let Some(lu) = last_use.get_mut(vi) {
                        if consumer_end > *lu {
                            *lu = consumer_end;
                        }
                    }
                });
            }
        }
    }

    // 2. Coalescing hints: entry params (incl. `sret` in X8), return value (`X0`/`V0`), and
    //    block-param (`phi`) partner links (so loop step defs coalesce directly into header params!).
    let mut loc_of: Vec<AllocLoc, A> = Vec::with_capacity_in(len, alloc);
    let mut hint: Vec<u8, A> = Vec::with_capacity_in(len, alloc);
    // `phi_partner[arg]` = header block-param index `p` that `arg` feeds on a loop back-edge.
    let mut phi_partner: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        loc_of.push(AllocLoc::None);
        hint.push(NO_REG);
        phi_partner.push(u32::MAX);
    }

    let func_sig = view.sig(SigId(view.head.sig));
    let entry_params = view.params(Block(0));
    let mut gpr_arg_idx = 0usize;
    let mut fp_arg_idx = 0usize;
    for (k, p) in entry_params.clone().enumerate() {
        let pkind = func_sig
            .params
            .get(k)
            .and_then(|sp| ParamKind::from_byte(sp.kind))
            .unwrap_or(ParamKind::Value);
        if pkind == ParamKind::Sret {
            if let Some(slot) = hint.get_mut(p) {
                *slot = reg_u8(X8);
            }
            continue;
        }
        let ty = view.tys.get(p).copied().map_or(Ty::NONE, Ty);
        if is_fp_ty(ty).is_some() {
            if let Some(&reg) = FP_ARG_REGS.get(fp_arg_idx) {
                if let Some(slot) = hint.get_mut(p) {
                    *slot = reg_u8(reg);
                }
                fp_arg_idx += 1;
            }
        } else if ty == Ty::I128 {
            // AAPCS64 C.8/C.9/C.11: 16-byte-aligned `i128` takes next even GPR pair, else stacks (NGRN = 8).
            let aligned = (gpr_arg_idx + 1) & !1;
            if aligned + 2 <= ARG_REGS.len() {
                gpr_arg_idx = aligned + 2;
            } else {
                gpr_arg_idx = ARG_REGS.len();
            }
        } else if let Some(&reg) = ARG_REGS.get(gpr_arg_idx) {
            if let Some(slot) = hint.get_mut(p) {
                *slot = reg_u8(reg);
            }
            gpr_arg_idx += 1;
        }
    }
    // Landing-block exception parameters enter in `X0` from the unwinder.
    for b in 0..blocks {
        if view.block_kind(Block(u32_of(b))).is_landing() {
            let params = view.params(Block(u32_of(b)));
            if params.start < params.end {
                if let Some(slot) = hint.get_mut(params.start) {
                    *slot = reg_u8(X0);
                }
            }
        }
    }

    // Record return-value hints (`X0`/`V0`) and block-param edge coalescing links.
    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        match view.op(term) {
            Op::Return => {
                if let Some(&at_raw) = view.data.get(term) {
                    let at = idx(at_raw);
                    if let Some(&retc_raw) = view.extra.get(at) {
                        let retc = idx(retc_raw);
                        if retc >= 1 {
                            if let Some(&rv0_raw) = view.extra.get(at + 1) {
                                let rv0 = idx(rv0_raw);
                                if let Some(slot) = hint.get_mut(rv0) {
                                    if *slot == NO_REG {
                                        let is_fp = view
                                            .tys
                                            .get(rv0)
                                            .copied()
                                            .is_some_and(|t| is_fp_ty(Ty(t)).is_some());
                                        *slot = if is_fp { 0 } else { reg_u8(X0) };
                                    }
                                }
                            }
                        }
                        if retc == 2 {
                            if let Some(&rv1_raw) = view.extra.get(at + 2) {
                                let rv1 = idx(rv1_raw);
                                if let Some(slot) = hint.get_mut(rv1) {
                                    if *slot == NO_REG {
                                        let is_fp = view
                                            .tys
                                            .get(rv1)
                                            .copied()
                                            .is_some_and(|t| is_fp_ty(Ty(t)).is_some());
                                        *slot = if is_fp { 1 } else { reg_u8(X1) };
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                for_each_edge(view, term, |target_block, args_start, args_len| {
                    let target_params = view.params(Block(target_block));
                    let n = args_len.min(target_params.len());
                    if let Some(args) = view.extra.get(args_start..args_start + n) {
                        for (k, &arg_raw) in args.iter().enumerate() {
                            let p = target_params.start + k;
                            let arg = idx(arg_raw);
                            if arg < len && arg > p {
                                if let Some(slot) = phi_partner.get_mut(arg) {
                                    if *slot == u32::MAX {
                                        *slot = u32_of(p);
                                    }
                                }
                            }
                        }
                    }
                });
            }
        }
    }

    // 3. Two-Phase Chordal-SSA + Greedy Priority-Weighted Coloring + IPRA + True Constant Remat!
    //
    // By Hack–Grund–Goos (CC 2006), SSA interference graphs are chordal: coloring in program
    // order achieves the exact minimum chromatic number (keeping every <=26-live function frameless
    // with zero callee-saved saves!). If program order encounters any pressure overflow (spill or
    // remat), we automatically re-color in descending loop-weighted priority order so hot inner
    // loops stay 100% register-resident while cold constants rematerialize (`AllocLoc::RematConst`)!
    let mut remat_val: Vec<Option<u64>, A> = Vec::with_capacity_in(len, alloc);
    for i in 0..len {
        let ty = view.tys.get(i).copied().map_or(Ty::NONE, Ty);
        let cv = if ty.is_scalar() && ty.is_int() && ty.bits(64) <= 64 {
            let bits = ty.bits(64);
            let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
            match view.op(i) {
                Op::Iconst => view.data.get(i).map(|&w| {
                    let v = if bits <= 32 { u64::from(w) } else { sext32_u64(w) };
                    v & mask
                }),
                Op::IconstWide => view.data.get(i).and_then(|&d| {
                    let at = idx(d);
                    let lo = u64::from(*view.extra.get(at)?);
                    let hi = if ty.const_words() >= 2 {
                        u64::from(*view.extra.get(at + 1)?)
                    } else {
                        0
                    };
                    Some((lo | (hi << 32)) & mask)
                }),
                _ => None,
            }
        } else {
            None
        };
        remat_val.push(cv);
    }

    for (arg, &p) in phi_partner.iter().enumerate() {
        if p != u32::MAX {
            let pi = idx(p);
            let aw = use_weight.get(arg).copied().unwrap_or(0).saturating_add(1);
            if let Some(pw) = use_weight.get_mut(pi) {
                if aw > *pw {
                    *pw = aw;
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    struct Candidate {
        inst: usize,
        end: usize,
        weight: u32,
        is_remat: bool,
    }
    let mut prog_order: Vec<Candidate, A> = Vec::with_capacity_in(len, alloc);
    for inst in 0..len {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            continue;
        }
        let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
        if ty == Ty::NONE || !has_any_use.get(inst).copied().unwrap_or(false) {
            continue;
        }
        let is_remat = remat_val.get(inst).copied().flatten().is_some()
            || matches!(view.op(inst), Op::SlotAddr | Op::FuncAddr | Op::DataAddr);
        let uw = use_weight.get(inst).copied().unwrap_or(0);
        let weight = if is_remat { (uw >> 2).max(1) } else { uw };
        let end = last_use.get(inst).copied().unwrap_or(inst);
        prog_order.push(Candidate { inst, end, weight, is_remat });
    }

    // True iff `inst` reads all of its inputs before writing its destination register,
    // allowing safe zero-copy coalescing with a dying operand whose `last_use == inst`
    // (e.g. `add.2d v16, v16, v21` or `add x9, x9, #1` on loop back-edges!).
    let is_rbw = |inst: usize| -> bool {
        let op = view.op(inst);
        let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
        if is_gpr_pair_ty(view, ty) {
            return false;
        }
        if op == Op::Icmp {
            let is_i128_cmp = view
                .data
                .get(inst)
                .and_then(|&d| view.extra.get(idx(d) + 1))
                .and_then(|&lhs| view.tys.get(idx(lhs)))
                .copied()
                .map(Ty)
                == Some(Ty::I128);
            if is_i128_cmp {
                return false;
            }
        }
        matches!(
            op,
            Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Umulhi
                | Op::Smulhi
                | Op::Neg
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Not
                | Op::Shl
                | Op::Lshr
                | Op::Ashr
                | Op::Fadd
                | Op::Fsub
                | Op::Fmul
                | Op::Fdiv
                | Op::Fneg
                | Op::Fabs
                | Op::Load
                | Op::PtrAdd
                | Op::Zext
                | Op::Sext
                | Op::Trunc
                | Op::Icmp
                | Op::Fcmp
                | Op::Select
                | Op::Splat
        )
    };

    let run_coloring = |order: &[Candidate]| -> Result<(Vec<AllocLoc, A>, [bool; 32], [bool; 32], u32, u32), RegAllocError> {
        let mut loc_try: Vec<AllocLoc, A> = Vec::with_capacity_in(len, alloc);
        let mut hint_try: Vec<u8, A> = Vec::with_capacity_in(len, alloc);
        for &h in &hint {
            loc_try.push(AllocLoc::None);
            hint_try.push(h);
        }
        let mut gpr_busy: Vec<Vec<(usize, usize, bool), A>, A> = Vec::with_capacity_in(32, alloc);
        let mut fp_busy: Vec<Vec<(usize, usize, bool), A>, A> = Vec::with_capacity_in(32, alloc);
        for _ in 0..32 {
            gpr_busy.push(Vec::new_in(alloc));
            fp_busy.push(Vec::new_in(alloc));
        }
        let is_reg_free = |busy: &[(usize, usize, bool)], start: usize, end: usize, cur_rbw: bool| -> bool {
            !busy.iter().any(|&(s, e, s_rbw)| {
                (start < e && s < end)
                    || start == s
                    || (start == e && !cur_rbw)
                    || (end == s && !s_rbw)
            })
        };
        let mut spill_busy: Vec<Vec<(usize, usize), A>, A> = Vec::new_in(alloc);
        let mut used_callee_gpr = [false; 32];
        let mut used_callee_fp = [false; 32];
        let mut num_spill_slots = 0u32;
        let mut remat_count = 0u32;

        let mut alloc_spill_n = |slots: usize, start: usize, end: usize| -> Result<u32, RegAllocError> {
            let slots_u32 = u32_of(slots);
            if spill_busy.len() >= slots {
                for slot_idx in 0..=(spill_busy.len() - slots) {
                    if let Some(window) = spill_busy.get_mut(slot_idx..slot_idx + slots) {
                        if window.iter().all(|s_iv| !s_iv.iter().any(|&(s, e)| start <= e && s <= end)) {
                            for s_iv in window {
                                s_iv.push((start, end));
                            }
                            return Ok(u32_of(slot_idx));
                        }
                    }
                }
            }
            if num_spill_slots.saturating_add(slots_u32) > max_spill_slots {
                return Err(RegAllocError::OutOfSpillSlots);
            }
            let slot = num_spill_slots;
            num_spill_slots += slots_u32;
            for _ in 0..slots {
                let mut iv = Vec::new_in(alloc);
                iv.push((start, end));
                spill_busy.push(iv);
            }
            Ok(slot)
        };

        for cand in order {
            let inst = cand.inst;
            let end = cand.end;
            let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);

            let cur_hint = hint_try.get(inst).copied().unwrap_or(NO_REG);
            let partner = phi_partner.get(inst).copied().unwrap_or(u32::MAX);
            if cur_hint == NO_REG && partner != u32::MAX {
                if let Some(&ploc) = loc_try.get(idx(partner)) {
                    match ploc {
                        AllocLoc::Gpr(r) | AllocLoc::Fp(r) | AllocLoc::Vec(r) => {
                            if let Some(hslot) = hint_try.get_mut(inst) {
                                *hslot = r;
                            }
                        }
                        _ => {}
                    }
                }
            }

            let mut crossed_clobbers = CallClobbers::EMPTY;
            let mut crosses_call = false;
            for &c in &call_positions {
                if c > inst && c < end {
                    crosses_call = true;
                    // Unwinding out of an `Invoke` clobbers all caller-saved regs regardless of IPRA.
                    let clob = if matches!(view.op(c), Op::Invoke | Op::InvokeIndirect) {
                        CallClobbers::C_ABI_ARM64
                    } else {
                        call_clobbers(c)
                    };
                    crossed_clobbers.clobbered = crossed_clobbers.clobbered.union(clob.clobbered);
                }
            }
            let gpr_safe = |r: Reg| -> bool {
                !crosses_call || (X19..=X28).contains(&r) || !crossed_clobbers.clobbers_gpr(reg_u8(r))
            };
            let fp_safe = |r: Reg| -> bool {
                !crosses_call || (8..=15).contains(&r) || !crossed_clobbers.clobbers_fp(reg_u8(r))
            };

            let cur_rbw = is_rbw(inst);
            if is_gpr_pair_ty(view, ty) {
                let pool_pick = |busy: &[Vec<(usize, usize, bool), A>], skip: Option<Reg>| -> Option<Reg> {
                    CALLER_SAVED_GPR
                        .iter()
                        .copied()
                        .chain(CALLEE_SAVED_GPR.iter().copied())
                        .find(|&r| {
                            Some(r) != skip
                                && gpr_safe(r)
                                && busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                        })
                };
                let r0 = pool_pick(&gpr_busy, None);
                let r1 = r0.and_then(|first| pool_pick(&gpr_busy, Some(first)));
                if let (Some(lo), Some(hi)) = (r0, r1) {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::GprPair(reg_u8(lo), reg_u8(hi));
                    }
                    if let Some(b) = gpr_busy.get_mut(idx(lo)) {
                        b.push((inst, end, cur_rbw));
                    }
                    if let Some(b) = gpr_busy.get_mut(idx(hi)) {
                        b.push((inst, end, cur_rbw));
                    }
                    if (X19..=X28).contains(&lo) {
                        if let Some(u) = used_callee_gpr.get_mut(idx(lo)) {
                            *u = true;
                        }
                    }
                    if (X19..=X28).contains(&hi) {
                        if let Some(u) = used_callee_gpr.get_mut(idx(hi)) {
                            *u = true;
                        }
                    }
                } else {
                    let s0 = alloc_spill_n(1, inst, end)?;
                    let s1 = alloc_spill_n(1, inst, end)?;
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::GprPairSpill(s0, s1);
                    }
                }
            } else if is_vec128_ty(ty).is_some() || is_vec_mask_ty(ty).is_some() {
                let vec_safe = |r: Reg| -> bool { !crosses_call || !crossed_clobbers.clobbers_fp(reg_u8(r)) };
                let h = hint_try.get(inst).copied().unwrap_or(NO_REG);
                let chosen = if h != NO_REG
                    && CALLER_SAVED_FP.contains(&u32::from(h))
                    && vec_safe(u32::from(h))
                    && fp_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                {
                    Some(u32::from(h))
                } else {
                    CALLER_SAVED_FP
                        .iter()
                        .copied()
                        .find(|&r| {
                            vec_safe(r)
                                && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                        })
                };
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Vec(reg_u8(reg));
                    }
                    if let Some(b) = fp_busy.get_mut(idx(reg)) {
                        b.push((inst, end, cur_rbw));
                    }
                } else {
                    let slot = alloc_spill_n(2, inst, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::VecSpill(slot);
                    }
                }
            } else if is_fp_ty(ty).is_some() {
                let h = hint_try.get(inst).copied().unwrap_or(NO_REG);
                let chosen = if h != NO_REG
                    && fp_safe(u32::from(h))
                    && fp_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                {
                    Some(u32::from(h))
                } else {
                    CALLER_SAVED_FP
                        .iter()
                        .copied()
                        .chain(CALLEE_SAVED_FP.iter().copied())
                        .find(|&r| {
                            fp_safe(r)
                                && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                        })
                };
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Fp(reg_u8(reg));
                    }
                    if let Some(b) = fp_busy.get_mut(idx(reg)) {
                        b.push((inst, end, cur_rbw));
                    }
                    if (8..=15).contains(&reg) {
                        if let Some(u) = used_callee_fp.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else {
                    let slot = alloc_spill_n(1, inst, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::FpSpill(slot);
                    }
                }
            } else {
                let h = hint_try.get(inst).copied().unwrap_or(NO_REG);
                let chosen = if h != NO_REG
                    && gpr_safe(u32::from(h))
                    && gpr_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                {
                    Some(u32::from(h))
                } else {
                    CALLER_SAVED_GPR
                        .iter()
                        .copied()
                        .chain(CALLEE_SAVED_GPR.iter().copied())
                        .find(|&r| {
                            gpr_safe(r)
                                && gpr_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                        })
                };
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Gpr(reg_u8(reg));
                    }
                    if let Some(b) = gpr_busy.get_mut(idx(reg)) {
                        b.push((inst, end, cur_rbw));
                    }
                    if (X19..=X28).contains(&reg) {
                        if let Some(u) = used_callee_gpr.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else if let Some(cval) = remat_val.get(inst).copied().flatten() {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::RematConst(cval);
                    }
                    remat_count += 1;
                } else {
                    let slot = alloc_spill_n(1, inst, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::GprSpill(slot);
                    }
                }
            }
        }
        Ok((loc_try, used_callee_gpr, used_callee_fp, num_spill_slots, remat_count))
    };

    // Phase 1: exact chordal-SSA program-order coloring (provably minimal register count!).
    let (mut final_loc, mut used_callee_gpr, mut used_callee_fp, mut num_spill_slots, mut remat_count) =
        run_coloring(&prog_order)?;

    // Phase 2: if register pressure overflowed (spills > 0), run loop-weighted Greedy Priority
    // Coloring so hot inner loops NEVER spill while cold outer values/constants yield & rematerialize!
    if num_spill_slots > 0 {
        let mut greedy_order: Vec<Candidate, A> = Vec::with_capacity_in(prog_order.len(), alloc);
        for &c in &prog_order {
            greedy_order.push(c);
        }
        greedy_order.sort_unstable_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then_with(|| a.is_remat.cmp(&b.is_remat))
                .then_with(|| a.inst.cmp(&b.inst))
        });
        if let Ok((g_loc, g_cgpr, g_cfp, g_spills, g_remat)) = run_coloring(&greedy_order) {
            if g_spills <= num_spill_slots {
                final_loc = g_loc;
                used_callee_gpr = g_cgpr;
                used_callee_fp = g_cfp;
                num_spill_slots = g_spills;
                remat_count = g_remat;
            }
        }
    }
    loc_of = final_loc;

    // Build exact ARM64 unwind pair masks (`blaze_code::unwind::Frame`).
    let mut x_pairs = 0u8;
    for k in 0..5usize {
        let lo = 19 + 2 * k;
        let hi = lo + 1;
        if used_callee_gpr.get(lo).copied().unwrap_or(false)
            || used_callee_gpr.get(hi).copied().unwrap_or(false)
        {
            x_pairs |= 1 << k;
        }
    }
    let mut d_pairs = 0u8;
    for k in 0..4usize {
        let lo = 8 + 2 * k;
        let hi = lo + 1;
        if used_callee_fp.get(lo).copied().unwrap_or(false)
            || used_callee_fp.get(hi).copied().unwrap_or(false)
        {
            d_pairs |= 1 << k;
        }
    }

    // Compute exact IPRA physical clobber summary for callers of this function!
    let mut clob = RegSet::EMPTY
        .with(TierReg(16))
        .with(TierReg(17));
    for &loc in &loc_of {
        match loc {
            AllocLoc::Gpr(r) if !(19..=28).contains(&r) => clob = clob.with(TierReg(r)),
            AllocLoc::GprPair(lo, hi) => {
                if !(19..=28).contains(&lo) {
                    clob = clob.with(TierReg(lo));
                }
                if !(19..=28).contains(&hi) {
                    clob = clob.with(TierReg(hi));
                }
            }
            AllocLoc::Fp(r) | AllocLoc::Vec(r) if !(8..=15).contains(&r) => {
                clob = clob.with(TierReg(32 + r));
                clob = clob.with(TierReg(32 + 29)).with(TierReg(32 + 30)).with(TierReg(32 + 31));
            }
            _ => {}
        }
    }
    if let Some(first_ret) = func_sig.rets.first() {
        let r0 = Ty(first_ret.ty);
        if is_fp_ty(r0).is_some() || is_vec128_ty(r0).is_some() {
            clob = clob.with(TierReg(32));
        } else {
            clob = clob.with(TierReg(0));
            if func_sig.rets.len() >= 2 {
                clob = clob.with(TierReg(1));
            }
        }
    }
    for &c in &call_positions {
        clob = clob.with(TierReg(30));
        clob = clob.union(call_clobbers(c).clobbered);
    }

    Ok(Allocation {
        loc_of,
        x_pairs,
        d_pairs,
        num_spill_slots,
        has_calls: !call_positions.is_empty(),
        clobbers: CallClobbers { clobbered: clob },
        remat_count,
    })
}

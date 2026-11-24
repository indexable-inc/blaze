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
use crate::pass::{Loc, Regs};

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

/// Preferred caller-saved scratch allocation order for interprocedural register-avoidance coloring
/// (from `sota::abi::CustomCallPlan::{scratch_gpr_order, scratch_fp_order}`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ScratchOrder {
    pub gpr: [u8; 16],
    pub fp: [u8; 24],
}

impl ScratchOrder {
    /// Default caller-saved allocation order matching [`CALLER_SAVED_GPR`] and [`CALLER_SAVED_FP`]
    /// (with permanently reserved FP scratch `V29..=V31` trailing and filtered out during coloring).
    pub const DEFAULT: Self = Self {
        gpr: [9, 10, 11, 12, 13, 14, 15, 8, 0, 1, 2, 3, 4, 5, 6, 7],
        fp: [
            16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
            0, 1, 2, 3, 4, 5, 6, 7,
            29, 30, 31,
        ],
    };
}

/// Builds a sanitized, duplicate-free caller-saved GPR preference order containing every register
/// in [`CALLER_SAVED_GPR`] exactly once, honoring `scratch_order.gpr` first.
fn build_caller_gpr_order(scratch_order: Option<&ScratchOrder>) -> [Reg; 16] {
    let ord = scratch_order.unwrap_or(&ScratchOrder::DEFAULT);
    let mut out = [0u32; 16];
    let mut seen = [false; 32];
    let mut len = 0usize;
    for &r in &ord.gpr {
        let reg = u32::from(r);
        let ri = usize::from(r);
        if CALLER_SAVED_GPR.contains(&reg)
            && !seen.get(ri).copied().unwrap_or(true)
            && let Some(slot) = out.get_mut(len)
        {
            *slot = reg;
            if let Some(s) = seen.get_mut(ri) {
                *s = true;
            }
            len += 1;
        }
    }
    for &reg in CALLER_SAVED_GPR {
        let ri = idx(reg);
        if !seen.get(ri).copied().unwrap_or(true)
            && let Some(slot) = out.get_mut(len)
        {
            *slot = reg;
            if let Some(s) = seen.get_mut(ri) {
                *s = true;
            }
            len += 1;
        }
    }
    out
}

/// Builds a sanitized, duplicate-free caller-saved FP/NEON preference order containing every
/// allocatable register in [`CALLER_SAVED_FP`] exactly once (excluding reserved `V29..=V31`),
/// honoring `scratch_order.fp` first.
fn build_caller_fp_order(scratch_order: Option<&ScratchOrder>) -> [Reg; 21] {
    let ord = scratch_order.unwrap_or(&ScratchOrder::DEFAULT);
    let mut out = [0u32; 21];
    let mut seen = [false; 32];
    let mut len = 0usize;
    for &r in &ord.fp {
        let reg = u32::from(r);
        let ri = usize::from(r);
        if CALLER_SAVED_FP.contains(&reg)
            && !seen.get(ri).copied().unwrap_or(true)
            && let Some(slot) = out.get_mut(len)
        {
            *slot = reg;
            if let Some(s) = seen.get_mut(ri) {
                *s = true;
            }
            len += 1;
        }
    }
    for &reg in CALLER_SAVED_FP {
        let ri = idx(reg);
        if !seen.get(ri).copied().unwrap_or(true)
            && let Some(slot) = out.get_mut(len)
        {
            *slot = reg;
            if let Some(s) = seen.get_mut(ri) {
                *s = true;
            }
            len += 1;
        }
    }
    out
}

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

/// Register bank classification for coalescing compatibility:
/// `0` = none / 2-word GPR pair, `1` = 128-bit SIMD vector/mask, `2` = scalar FP, `3` = scalar GPR.
#[inline]
fn reg_bank(view: &View<'_>, ty: Ty) -> u8 {
    if ty == Ty::NONE || is_gpr_pair_ty(view, ty) {
        0
    } else if is_vec128_ty(ty).is_some() || is_vec_mask_ty(ty).is_some() {
        1
    } else if is_fp_ty(ty).is_some() {
        2
    } else {
        3
    }
}

/// True iff `inst` is a 3-arg NEON destructive 2-address accumulate `TargetIntrinsic`
/// (`dst = op(acc, a, b)` where `arg0` is the in-place accumulator).
#[inline]
fn is_neon_acc_intrinsic(view: &View<'_>, inst: usize) -> bool {
    if view.op(inst) != Op::TargetIntrinsic {
        return false;
    }
    let Some(&at_raw) = view.data.get(inst) else {
        return false;
    };
    let Some(&id_raw) = view.extra.get(idx(at_raw)) else {
        return false;
    };
    let name = view.intrinsic_name(blaze_lir::func::IntrinsicId(id_raw));
    name == b"blaze.neon.sdot"
        || name == b"blaze.neon.udot"
        || name == b"blaze.neon.smmla"
        || name == b"blaze.neon.ummla"
}

/// True iff `inst` reads all of its inputs before writing its destination register,
/// allowing safe zero-copy coalescing with a dying operand whose `last_use == inst`
/// (e.g. `fmla v16.4s, v20.4s, v21.4s`, `sdot v16.4s, v20.16b, v24.16b`, `add.2d v16, v16, v21`,
/// or `add x9, x9, #1`).
fn is_rbw(view: &View<'_>, inst: usize) -> bool {
    let op = view.op(inst);
    let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
    if is_gpr_pair_ty(view, ty) {
        return false;
    }
    if is_neon_acc_intrinsic(view, inst) {
        return true;
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
            | Op::Rotl
            | Op::Rotr
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::UaddSat
            | Op::UsubSat
            | Op::SaddSat
            | Op::SsubSat
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Fma
            | Op::Fmuladd
            | Op::Fneg
            | Op::Fabs
            | Op::Sqrt
            | Op::Nearest
            | Op::Ceil
            | Op::Floor
            | Op::Ftrunc
            | Op::Fround
            | Op::Fminimum
            | Op::Fmaximum
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fpext
            | Op::Fptrunc
            | Op::SitoFp
            | Op::UitoFp
            | Op::FptoSi
            | Op::FptoUi
            | Op::Bitcast
            | Op::Freeze
            | Op::Load
            | Op::PtrAdd
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Icmp
            | Op::Fcmp
            | Op::Select
            | Op::Splat
            | Op::Shuffle
            | Op::Insert
            | Op::Extract
    )
}

/// Returns the preferred same-bank operand `src < inst` that `inst` should coalesce into:
/// - Destructive 2-address NEON accumulators (`Fma`/`Fmuladd` operand `c`, `Fadd`/`Fsub` fused with
///   `Fmul`, `Add`/`Sub` fused with `Mul` or `VecUadalp`, reduction/IV chains)
/// - In-place lane insertion (`Insert` base vector)
/// - Same-bank zero-copy `Bitcast` / `Freeze`
/// - Dying operand of 128-bit vector `Select` (`bsl`/`bit`/`bif` in 1 instruction)
fn preferred_inplace_operand(
    view: &View<'_>,
    skip_alloc: &[bool],
    last_use: &[usize],
    inst: usize,
) -> Option<usize> {
    let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
    let bank = reg_bank(view, ty);
    if bank == 0 {
        return None;
    }
    let valid_src = |v: u32| -> Option<usize> {
        let vi = idx(v);
        if vi >= inst || skip_alloc.get(vi).copied().unwrap_or(false) {
            return None;
        }
        let sty = view.tys.get(vi).copied().map_or(Ty::NONE, Ty);
        if reg_bank(view, sty) == bank { Some(vi) } else { None }
    };
    let op_of = |i: usize| -> Option<Op> { (i < inst).then(|| view.op(i)) };
    match view.op(inst) {
        Op::Fma | Op::Fmuladd => {
            let at = idx(*view.data.get(inst)?);
            valid_src(*view.extra.get(at + 2)?)
        }
        Op::TargetIntrinsic if is_neon_acc_intrinsic(view, inst) => {
            let at = idx(*view.data.get(inst)?);
            valid_src(*view.extra.get(at + 2)?)
        }
        Op::Fadd => {
            let at = idx(*view.data.get(inst)?);
            let a = *view.extra.get(at)?;
            let b = *view.extra.get(at + 1)?;
            let (ai, bi) = (idx(a), idx(b));
            let (oa, ob) = (op_of(ai), op_of(bi));
            if oa == Some(Op::Fmul) && ob != Some(Op::Fmul) {
                valid_src(b)
            } else if ob == Some(Op::Fmul) && oa != Some(Op::Fmul) {
                valid_src(a)
            } else if oa == Some(Op::Param) && ob != Some(Op::Param) {
                valid_src(a)
            } else if ob == Some(Op::Param) && oa != Some(Op::Param) {
                valid_src(b)
            } else if matches!(oa, Some(Op::Fadd | Op::Fma | Op::Fmuladd))
                && !matches!(ob, Some(Op::Fadd | Op::Fma | Op::Fmuladd))
            {
                valid_src(a)
            } else if matches!(ob, Some(Op::Fadd | Op::Fma | Op::Fmuladd))
                && !matches!(oa, Some(Op::Fadd | Op::Fma | Op::Fmuladd))
            {
                valid_src(b)
            } else {
                None
            }
        }
        Op::Fsub => {
            let at = idx(*view.data.get(inst)?);
            let a = *view.extra.get(at)?;
            let b = *view.extra.get(at + 1)?;
            if op_of(idx(b)) == Some(Op::Fmul) && op_of(idx(a)) != Some(Op::Fmul) {
                valid_src(a)
            } else {
                None
            }
        }
        Op::Add => {
            let at = idx(*view.data.get(inst)?);
            let a = *view.extra.get(at)?;
            let b = *view.extra.get(at + 1)?;
            let (ai, bi) = (idx(a), idx(b));
            let (oa, ob) = (op_of(ai), op_of(bi));
            // Fused `VecUadalp`: `add(add(acc, and(...)), lshr(...))` where the inner `add` is skipped.
            if skip_alloc.get(ai).copied().unwrap_or(false) && oa == Some(Op::Add) {
                if let Some(&iat_raw) = view.data.get(ai) {
                    let iat = idx(iat_raw);
                    if let (Some(&ia), Some(&ib)) = (view.extra.get(iat), view.extra.get(iat + 1)) {
                        if op_of(idx(ib)) == Some(Op::And) {
                            if let Some(acc) = valid_src(ia) {
                                return Some(acc);
                            }
                        } else if op_of(idx(ia)) == Some(Op::And) {
                            if let Some(acc) = valid_src(ib) {
                                return Some(acc);
                            }
                        }
                    }
                }
            }
            if oa == Some(Op::Mul) && ob != Some(Op::Mul) {
                valid_src(b)
            } else if ob == Some(Op::Mul) && oa != Some(Op::Mul) {
                valid_src(a)
            } else if oa == Some(Op::Param) && ob != Some(Op::Param) {
                valid_src(a)
            } else if ob == Some(Op::Param) && oa != Some(Op::Param) {
                valid_src(b)
            } else if oa == Some(Op::Add) && ob != Some(Op::Add) {
                valid_src(a)
            } else if ob == Some(Op::Add) && oa != Some(Op::Add) {
                valid_src(b)
            } else {
                None
            }
        }
        Op::Sub => {
            let at = idx(*view.data.get(inst)?);
            let a = *view.extra.get(at)?;
            let b = *view.extra.get(at + 1)?;
            if op_of(idx(b)) == Some(Op::Mul) && op_of(idx(a)) != Some(Op::Mul) {
                valid_src(a)
            } else {
                None
            }
        }
        Op::Insert => {
            let at = idx(*view.data.get(inst)?);
            valid_src(*view.extra.get(at)?)
        }
        Op::Bitcast | Op::Freeze => valid_src(*view.data.get(inst)?),
        Op::Select if bank == 1 => {
            let at = idx(*view.data.get(inst)?);
            let cv = *view.extra.get(at)?;
            let tv = *view.extra.get(at + 1)?;
            let fv = *view.extra.get(at + 2)?;
            for cand in [cv, fv, tv] {
                if let Some(ci) = valid_src(cand) {
                    if last_use.get(ci).copied() == Some(inst) {
                        return Some(ci);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// For a 128-bit vector destructive 2-address accumulate op (`Fma`/`Fmuladd`, `Fadd`/`Fsub` fused
/// with `Fmul`, or `Add`/`Sub` fused with `Mul`), returns the physical registers of the two
/// non-accumulator operands `(a, b)` so the allocator avoids clobbering them (which would trigger
/// a 3-instruction `V31` bounce in `isel.rs`) whenever the accumulator itself cannot be reused.
fn vec_fma_avoid_regs(
    view: &View<'_>,
    skip_alloc: &[bool],
    loc_try: &[AllocLoc],
    inst: usize,
) -> (u8, u8) {
    let vec_reg_of = |v: u32| -> u8 {
        let vi = idx(v);
        if skip_alloc.get(vi).copied().unwrap_or(false) && view.op(vi) == Op::Splat {
            if let Some(&sv) = view.data.get(vi) {
                return match loc_try.get(idx(sv)).copied() {
                    Some(AllocLoc::Fp(r)) => r,
                    _ => NO_REG,
                };
            }
        }
        match loc_try.get(vi).copied() {
            Some(AllocLoc::Vec(r)) => r,
            _ => NO_REG,
        }
    };
    let mul_operands = |m: usize| -> (u8, u8) {
        let Some(&mat_raw) = view.data.get(m) else {
            return (NO_REG, NO_REG);
        };
        let mat = idx(mat_raw);
        let ra = view.extra.get(mat).copied().map_or(NO_REG, vec_reg_of);
        let rb = view.extra.get(mat + 1).copied().map_or(NO_REG, vec_reg_of);
        (ra, rb)
    };
    let Some(&at_raw) = view.data.get(inst) else {
        return (NO_REG, NO_REG);
    };
    let at = idx(at_raw);
    match view.op(inst) {
        Op::Fma | Op::Fmuladd => {
            let ra = view.extra.get(at).copied().map_or(NO_REG, vec_reg_of);
            let rb = view.extra.get(at + 1).copied().map_or(NO_REG, vec_reg_of);
            (ra, rb)
        }
        Op::TargetIntrinsic if is_neon_acc_intrinsic(view, inst) => {
            let ra = view.extra.get(at + 3).copied().map_or(NO_REG, vec_reg_of);
            let rb = view.extra.get(at + 4).copied().map_or(NO_REG, vec_reg_of);
            (ra, rb)
        }
        Op::Fadd => {
            let (Some(&a), Some(&b)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                return (NO_REG, NO_REG);
            };
            let (ai, bi) = (idx(a), idx(b));
            if ai < inst && skip_alloc.get(ai).copied().unwrap_or(false) && view.op(ai) == Op::Fmul {
                mul_operands(ai)
            } else if bi < inst && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Fmul {
                mul_operands(bi)
            } else {
                (NO_REG, NO_REG)
            }
        }
        Op::Fsub => {
            let Some(&b) = view.extra.get(at + 1) else {
                return (NO_REG, NO_REG);
            };
            let bi = idx(b);
            if bi < inst && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Fmul {
                mul_operands(bi)
            } else {
                (NO_REG, NO_REG)
            }
        }
        Op::Add => {
            let (Some(&a), Some(&b)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                return (NO_REG, NO_REG);
            };
            let (ai, bi) = (idx(a), idx(b));
            if ai < inst && skip_alloc.get(ai).copied().unwrap_or(false) && view.op(ai) == Op::Mul {
                mul_operands(ai)
            } else if bi < inst && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Mul {
                mul_operands(bi)
            } else {
                (NO_REG, NO_REG)
            }
        }
        Op::Sub => {
            let Some(&b) = view.extra.get(at + 1) else {
                return (NO_REG, NO_REG);
            };
            let bi = idx(b);
            if bi < inst && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Mul {
                mul_operands(bi)
            } else {
                (NO_REG, NO_REG)
            }
        }
        _ => (NO_REG, NO_REG),
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
    os: crate::Os,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    allocate_with_ipra(view, skip_alloc, tail_calls, max_spill_slots, os, |_| CallClobbers::C_ABI_ARM64, alloc)
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
    os: crate::Os,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    allocate_with_ipra_and_order(
        view,
        skip_alloc,
        tail_calls,
        max_spill_slots,
        os,
        call_clobbers,
        None,
        alloc,
    )
}

/// IPRA-aware register allocation with optional interprocedural register-avoidance scratch order
/// (`scratch_order`): callees allocate caller-untouched GPR/FP registers before caller-hot ones.
pub(crate) fn allocate_with_ipra_and_order<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    tail_calls: &[bool],
    max_spill_slots: u32,
    os: crate::Os,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    scratch_order: Option<&ScratchOrder>,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    let caller_gpr_order = build_caller_gpr_order(scratch_order);
    let caller_fp_order = build_caller_fp_order(scratch_order);
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
        let depth = block_depth.get(b).copied().unwrap_or(0).min(5);
        let b_weight = 1u32 << (depth * 4); // 1x outer, 16x depth-1, 256x depth-2, 4096x depth-3, ...
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
            let is_sme2_call = op == Op::TargetIntrinsic
                && view
                    .data
                    .get(inst)
                    .and_then(|&d| view.extra.get(idx(d)))
                    .is_some_and(|&id| {
                        let name = view.intrinsic_name(blaze_lir::func::IntrinsicId(id));
                        name == b"blaze.sme2.gemm_f32"
                            || name == b"blaze.sme2.gemm_f64"
                            || name == b"blaze.sme2.gemm_i8_i32"
                            || name == b"blaze.sme2.gemm_u8_u32"
                    });
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
                || is_i128_div
                || is_sme2_call)
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

    // Propagate fused producers' operand lifetimes and loop weights to their consumer's position (reverse scan).
    for inst in (0..len).rev() {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            let consumer_end = last_use.get(inst).copied().unwrap_or(0);
            let consumer_w = use_weight.get(inst).copied().unwrap_or(0);
            for_each_use(view, inst, |v| {
                let vi = idx(v);
                if let Some(lu) = last_use.get_mut(vi) {
                    if consumer_end > *lu {
                        *lu = consumer_end;
                    }
                }
                if let Some(w) = use_weight.get_mut(vi) {
                    if consumer_w > *w {
                        *w = consumer_w;
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

    // 2. Coalescing hints: entry params (incl. `sret` in X8), return value (`X0`/`V0`),
    //    block-param (`phi`) back-edges, and in-place 2-address accumulators (`fmla`/`mla`/`ins`).
    let mut hint: Vec<u8, A> = Vec::with_capacity_in(len, alloc);
    // `phi_partner[arg]` = partner index `p < arg` whose physical register `arg` should inherit.
    let mut phi_partner: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut phi_prio: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        hint.push(NO_REG);
        phi_partner.push(u32::MAX);
        phi_prio.push(0);
    }

    let func_sig = view.sig(SigId(view.head.sig));
    let entry_params = view.params(Block(0));
    let mut regs = Regs::new(os);
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
        match regs.next(ty) {
            Some(Loc::V(reg, _)) | Some(Loc::X(reg)) => {
                if let Some(slot) = hint.get_mut(p) {
                    *slot = reg_u8(reg);
                }
            }
            Some(Loc::Pair(_)) | None => {}
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
                                        let is_fp_or_vec = view.tys.get(rv0).copied().is_some_and(|t| {
                                            let ty = Ty(t);
                                            is_fp_ty(ty).is_some() || is_vec128_ty(ty).is_some()
                                        });
                                        *slot = if is_fp_or_vec { 0 } else { reg_u8(X0) };
                                    }
                                }
                            }
                        }
                        if retc == 2 {
                            if let Some(&rv1_raw) = view.extra.get(at + 2) {
                                let rv1 = idx(rv1_raw);
                                if let Some(slot) = hint.get_mut(rv1) {
                                    if *slot == NO_REG {
                                        let is_fp_or_vec = view.tys.get(rv1).copied().is_some_and(|t| {
                                            let ty = Ty(t);
                                            is_fp_ty(ty).is_some() || is_vec128_ty(ty).is_some()
                                        });
                                        *slot = if is_fp_or_vec { 1 } else { reg_u8(X1) };
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                let b_depth = block_depth.get(b).copied().unwrap_or(0);
                for_each_edge(view, term, |target_block, args_start, args_len| {
                    let target_params = view.params(Block(target_block));
                    let n = args_len.min(target_params.len());
                    let is_back = idx(target_block) <= b;
                    let edge_prio = if is_back { b_depth.saturating_add(1) } else { 0 };
                    if let Some(args) = view.extra.get(args_start..args_start + n) {
                        for (k, &arg_raw) in args.iter().enumerate() {
                            let p = target_params.start + k;
                            let arg = idx(arg_raw);
                            if arg < len && arg > p {
                                let p_ty = view.tys.get(p).copied().map_or(Ty::NONE, Ty);
                                let arg_ty = view.tys.get(arg).copied().map_or(Ty::NONE, Ty);
                                let bank = reg_bank(view, arg_ty);
                                if bank != 0 && bank == reg_bank(view, p_ty) {
                                    let cur = phi_partner.get(arg).copied().unwrap_or(u32::MAX);
                                    let cur_prio = phi_prio.get(arg).copied().unwrap_or(0);
                                    if cur == u32::MAX || edge_prio > cur_prio {
                                        if let Some(slot) = phi_partner.get_mut(arg) {
                                            *slot = u32_of(p);
                                        }
                                        if let Some(pslot) = phi_prio.get_mut(arg) {
                                            *pslot = edge_prio;
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
            }
        }
    }

    // Propagate loop back-edge `phi_partner` backward through in-place accumulator chains
    // (e.g. `p -> fma_0 -> fma_1 -> back_edge(p)` so every step stays in `p`'s register).
    for inst in (0..len).rev() {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            continue;
        }
        let partner = phi_partner.get(inst).copied().unwrap_or(u32::MAX);
        if partner == u32::MAX {
            continue;
        }
        let pi = idx(partner);
        if let Some(acc) = preferred_inplace_operand(view, skip_alloc, &last_use, inst) {
            if acc > pi
                && last_use.get(acc).copied() == Some(inst)
                && phi_partner.get(acc).copied() == Some(u32::MAX)
            {
                if let Some(slot) = phi_partner.get_mut(acc) {
                    *slot = partner;
                }
            }
        }
    }

    // Coalesce destructive 2-address accumulators & zero-copy ops (`Fma`/`Fmuladd`, `Fadd`+`Fmul`,
    // `Add`+`Mul`/`VecUadalp`, `Insert`, same-bank `Bitcast`/`Freeze`, dying vector `Select`)
    // directly into their dying source operand (eliminating `mov` in `saxpy`/`matmul`/`dot`!).
    for inst in 0..len {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            continue;
        }
        if phi_partner.get(inst).copied() != Some(u32::MAX) || !is_rbw(view, inst) {
            continue;
        }
        if let Some(src) = preferred_inplace_operand(view, skip_alloc, &last_use, inst) {
            if last_use.get(src).copied() == Some(inst) {
                if let Some(slot) = phi_partner.get_mut(inst) {
                    *slot = u32_of(src);
                }
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

    // Propagate scalar FP return hints backward through lane-0 `Op::Extract` into the source vector
    // so `v -> extract(v, 0) -> ret` stays in the same FP register with zero `fmov` instructions.
    for inst in (0..len).rev() {
        if view.op(inst) == Op::Extract {
            let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
            if is_fp_ty(ty).is_some() {
                let ih = hint.get(inst).copied().unwrap_or(NO_REG);
                if ih != NO_REG {
                    if let Some(&d) = view.data.get(inst) {
                        let at = idx(d);
                        if view.extra.get(at + 1).copied() == Some(0) {
                            if let Some(&vec_v) = view.extra.get(at) {
                                let vi = idx(vec_v);
                                if let Some(vh) = hint.get_mut(vi) {
                                    if *vh == NO_REG {
                                        *vh = ih;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Propagate priority weights and return-value hints backward along `phi_partner` chains so
    // chain heads color first in Phase 2 greedy order and avoid unnecessary return copies.
    for arg in (0..len).rev() {
        let p = phi_partner.get(arg).copied().unwrap_or(u32::MAX);
        if p != u32::MAX {
            let pi = idx(p);
            let aw = use_weight.get(arg).copied().unwrap_or(0).saturating_add(1);
            if let Some(pw) = use_weight.get_mut(pi) {
                if aw > *pw {
                    *pw = aw;
                }
            }
            let ah = hint.get(arg).copied().unwrap_or(NO_REG);
            if ah != NO_REG {
                if let Some(ph) = hint.get_mut(pi) {
                    if *ph == NO_REG {
                        *ph = ah;
                    }
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

    let run_coloring = |order: &[Candidate]| -> Result<(Vec<AllocLoc, A>, [bool; 32], [bool; 32], u32, u32, u64), RegAllocError> {
        let mut loc_try: Vec<AllocLoc, A> = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            loc_try.push(AllocLoc::None);
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
        let mut spill_cost = 0u64;

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
            let bank = reg_bank(view, ty);

            let partner = phi_partner.get(inst).copied().unwrap_or(u32::MAX);
            let primary_hint = if partner != u32::MAX {
                match (bank, loc_try.get(idx(partner)).copied()) {
                    (1, Some(AllocLoc::Vec(r)))
                    | (2, Some(AllocLoc::Fp(r)))
                    | (3, Some(AllocLoc::Gpr(r))) => r,
                    _ => NO_REG,
                }
            } else {
                NO_REG
            };
            let secondary_hint = hint.get(inst).copied().unwrap_or(NO_REG);

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

            let cur_rbw = is_rbw(view, inst);
            if is_gpr_pair_ty(view, ty) {
                let pool_pick = |busy: &[Vec<(usize, usize, bool), A>], skip: Option<Reg>| -> Option<Reg> {
                    caller_gpr_order
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
                    spill_cost = spill_cost.saturating_add(u64::from(cand.weight).saturating_mul(8));
                }
            } else if is_vec128_ty(ty).is_some() || is_vec_mask_ty(ty).is_some() {
                // Under AAPCS64, callees preserve only the low 64 bits (`d8..d15`) of `v8..v15`.
                // For any 128-bit vector whose live interval does NOT cross a call (`!crosses_call`),
                // `v8..v15` are 100% legal full 128-bit NEON registers once `used_callee_fp[r]` is
                // recorded (emitting `stp/ldp d8..d15`), expanding the pool from 21 -> 29 registers!
                let vec_safe = |r: Reg| -> bool {
                    if (8..=15).contains(&r) {
                        !crosses_call
                    } else {
                        !crosses_call || !crossed_clobbers.clobbers_fp(reg_u8(r))
                    }
                };
                let try_vec = |h: u8| -> Option<Reg> {
                    if h != NO_REG {
                        let r = u32::from(h);
                        if (CALLER_SAVED_FP.contains(&r) || CALLEE_SAVED_FP.contains(&r))
                            && vec_safe(r)
                            && fp_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                        {
                            return Some(r);
                        }
                    }
                    None
                };
                let chosen = try_vec(primary_hint)
                    .or_else(|| try_vec(secondary_hint))
                    .or_else(|| {
                        let (avoid0, avoid1) = vec_fma_avoid_regs(view, skip_alloc, &loc_try, inst);
                        if avoid0 != NO_REG || avoid1 != NO_REG {
                            caller_fp_order
                                .iter()
                                .copied()
                                .chain(CALLEE_SAVED_FP.iter().copied())
                                .find(|&r| {
                                    let ru = reg_u8(r);
                                    ru != avoid0
                                        && ru != avoid1
                                        && vec_safe(r)
                                        && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                                })
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        caller_fp_order
                            .iter()
                            .copied()
                            .chain(CALLEE_SAVED_FP.iter().copied())
                            .find(|&r| {
                                vec_safe(r)
                                    && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                            })
                    });
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Vec(reg_u8(reg));
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
                    let slot = alloc_spill_n(2, inst, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::VecSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(u64::from(cand.weight).saturating_mul(8));
                }
            } else if is_fp_ty(ty).is_some() {
                let extract_lane0_hint = if view.op(inst) == Op::Extract {
                    view.data.get(inst).and_then(|&d| {
                        let at = idx(d);
                        let vec_v = *view.extra.get(at)?;
                        let lane = *view.extra.get(at + 1)?;
                        if lane == 0 {
                            match loc_try.get(idx(vec_v)).copied() {
                                Some(AllocLoc::Vec(r)) => Some(r),
                                _ => None,
                            }
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
                .unwrap_or(NO_REG);
                let try_fp = |h: u8| -> Option<Reg> {
                    if h != NO_REG
                        && fp_safe(u32::from(h))
                        && fp_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                    {
                        Some(u32::from(h))
                    } else {
                        None
                    }
                };
                let chosen = try_fp(primary_hint)
                    .or_else(|| try_fp(secondary_hint))
                    .or_else(|| try_fp(extract_lane0_hint))
                    .or_else(|| {
                        caller_fp_order
                            .iter()
                            .copied()
                            .chain(CALLEE_SAVED_FP.iter().copied())
                            .find(|&r| {
                                fp_safe(r)
                                    && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                            })
                    });
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
                    spill_cost = spill_cost.saturating_add(u64::from(cand.weight).saturating_mul(4));
                }
            } else {
                let pair_trunc_hint = if view.op(inst) == Op::Trunc {
                    view.data.get(inst).map(|&raw| idx(raw)).and_then(|si| {
                        if view.tys.get(si).copied().map(Ty) != Some(Ty::I128) {
                            return None;
                        }
                        if !skip_alloc.get(si).copied().unwrap_or(false) {
                            match loc_try.get(si).copied() {
                                Some(AllocLoc::GprPair(lo, _)) => Some(lo),
                                _ => None,
                            }
                        } else if matches!(view.op(si), Op::Lshr | Op::Ashr) {
                            let x = view.data.get(si).and_then(|&d| view.extra.get(idx(d))).copied()?;
                            match loc_try.get(idx(x)).copied() {
                                Some(AllocLoc::GprPair(_, hi)) => Some(hi),
                                _ => None,
                            }
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
                .unwrap_or(NO_REG);
                let try_gpr = |h: u8| -> Option<Reg> {
                    if h != NO_REG
                        && gpr_safe(u32::from(h))
                        && gpr_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                    {
                        Some(u32::from(h))
                    } else {
                        None
                    }
                };
                let chosen = try_gpr(primary_hint)
                    .or_else(|| try_gpr(secondary_hint))
                    .or_else(|| try_gpr(pair_trunc_hint))
                    .or_else(|| {
                        caller_gpr_order
                            .iter()
                            .copied()
                            .chain(CALLEE_SAVED_GPR.iter().copied())
                            .find(|&r| {
                                gpr_safe(r)
                                    && gpr_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, inst, end, cur_rbw))
                            })
                    });
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
                    spill_cost = spill_cost.saturating_add(u64::from(cand.weight));
                } else {
                    let slot = alloc_spill_n(1, inst, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::GprSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(u64::from(cand.weight).saturating_mul(4));
                }
            }
        }
        Ok((loc_try, used_callee_gpr, used_callee_fp, num_spill_slots, remat_count, spill_cost))
    };

    // Phase 1: exact chordal-SSA program-order coloring (provably minimal register count!).
    let (mut final_loc, mut used_callee_gpr, mut used_callee_fp, mut num_spill_slots, mut remat_count, spill_cost) =
        run_coloring(&prog_order)?;

    // Phase 2: if register pressure overflowed (spills > 0 or remat > 0), run loop-weighted
    // Greedy Priority Coloring so hot inner loops NEVER spill while cold outer values/constants
    // yield & rematerialize! Compare by loop-weighted `spill_cost` (never raw unweighted slot
    // count) so spilling outer depth-0/1 values ALWAYS beats spilling even one inner-loop value.
    if num_spill_slots > 0 || remat_count > 0 {
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
        if let Ok((g_loc, g_cgpr, g_cfp, g_spills, g_remat, g_cost)) = run_coloring(&greedy_order) {
            if g_cost < spill_cost || (g_cost == spill_cost && g_spills <= num_spill_slots) {
                final_loc = g_loc;
                used_callee_gpr = g_cgpr;
                used_callee_fp = g_cfp;
                num_spill_slots = g_spills;
                remat_count = g_remat;
            }
        }
    }
    let loc_of = final_loc;

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
            AllocLoc::Fp(r) | AllocLoc::Vec(r) => {
                if !(8..=15).contains(&r) {
                    clob = clob.with(TierReg(32 + r));
                }
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

#[cfg(test)]
mod tests {
    use blaze_lir::{
        Flags, IntCond, Op, Scalar, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv},
    };
    use blaze_own::{Alloc, Vec};
    use blaze_tier::{CallClobbers, Machine, Names, Outside, RoutinePlaces};

    use super::{
        AllocLoc, CALLER_SAVED_FP, CALLER_SAVED_GPR, ScratchOrder, allocate,
        allocate_with_ipra_and_order, build_caller_fp_order, build_caller_gpr_order,
    };

    struct NoNames;
    impl Names for NoNames {
        fn name(&self, _of: Outside) -> Option<blaze_tier::Named<'_>> {
            None
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    #[test]
    fn scratch_order_filters_reserved_regs_and_reorders_allocation() {
        let alloc = test_alloc();

        // Default order matches CALLER_SAVED_GPR / CALLER_SAVED_FP verbatim.
        assert_eq!(build_caller_gpr_order(None).as_slice(), CALLER_SAVED_GPR);
        assert_eq!(build_caller_fp_order(None).as_slice(), CALLER_SAVED_FP);

        // Reserved regs (x16, x17, x18, x19, v29, v30, v31) are filtered out even if listed first.
        let dirty = ScratchOrder {
            gpr: [16, 17, 18, 19, 13, 14, 15, 12, 11, 10, 9, 8, 0, 1, 2, 3],
            fp: [
                29, 30, 31, 8, 22, 23, 24, 25, 26, 27, 28, 16, 17, 18, 19, 20, 21, 0, 1, 2, 3, 4,
                5, 6,
            ],
        };
        let sanitized_gpr = build_caller_gpr_order(Some(&dirty));
        let sanitized_fp = build_caller_fp_order(Some(&dirty));
        assert_eq!(sanitized_gpr[0], 13);
        assert!(sanitized_gpr.iter().all(|r| CALLER_SAVED_GPR.contains(r)));
        assert_eq!(sanitized_fp[0], 22);
        assert!(sanitized_fp.iter().all(|r| CALLER_SAVED_FP.contains(r)));

        // Leaf fn(x, y) -> ((x * y) + x) ^ y: `t1 = x * y` has both `x` and `y` live past it,
        // so `t1` picks the first caller-saved scratch GPR from `ScratchOrder`.
        let mut b = Builder::<Alloc<'static>, Full>::new_in(32, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let t1 = b.binary(Op::Mul, Flags::NONE, x, y);
        let t2 = b.binary(Op::Add, Flags::NONE, t1, x);
        let t3 = b.binary(Op::Xor, Flags::NONE, t2, y);
        b.ret(&[t3]);
        let func = b.finish_checked().expect("leaf LIR builds");
        let view = func.view();
        let skip = [false; 16];
        let tail = [false; 16];
        let n = view.len();

        let default_alloc = allocate_with_ipra_and_order(
            &view,
            &skip[..n],
            &tail[..n],
            32,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            None,
            alloc,
        )
        .expect("default regalloc succeeds");
        assert_eq!(default_alloc.loc_of[t1.index()], AllocLoc::Gpr(9));

        let avoid_x9 = ScratchOrder {
            gpr: [13, 14, 15, 12, 11, 10, 9, 8, 0, 1, 2, 3, 4, 5, 6, 7],
            fp: ScratchOrder::DEFAULT.fp,
        };
        let avoided_alloc = allocate_with_ipra_and_order(
            &view,
            &skip[..n],
            &tail[..n],
            32,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            Some(&avoid_x9),
            alloc,
        )
        .expect("register-avoidance regalloc succeeds");
        assert_eq!(avoided_alloc.loc_of[t1.index()], AllocLoc::Gpr(13));
        assert!(
            !avoided_alloc.clobbers.clobbers_gpr(9),
            "callee must avoid clobbering caller-hot X9 when demoted in ScratchOrder"
        );
    }

    #[test]
    fn sdot_loop_coalesces_four_accumulators_with_zero_moves() {
        let alloc = test_alloc();
        let v16i8 = Ty::vector(Scalar::I8, 16).expect("v16i8");
        let v4i32 = Ty::vector(Scalar::I32, 4).expect("v4i32");

        for intr_name in [
            "blaze.neon.sdot",
            "blaze.neon.udot",
            "blaze.neon.smmla",
            "blaze.neon.ummla",
        ] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let loop_hdr = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let pa = b.param(Ty::PTR);
            let pb = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            let z64 = b.iconst(Ty::I64, 0);
            let z32 = b.iconst(Ty::I32, 0);
            let init0 = b.convert(Op::Splat, Flags::NONE, v4i32, z32);
            let init1 = b.convert(Op::Splat, Flags::NONE, v4i32, z32);
            let init2 = b.convert(Op::Splat, Flags::NONE, v4i32, z32);
            let init3 = b.convert(Op::Splat, Flags::NONE, v4i32, z32);
            b.jump((loop_hdr, &[z64, init0, init1, init2, init3]));

            b.begin(loop_hdr);
            let p_i = b.param(Ty::I64);
            let p_acc0 = b.param(v4i32);
            let p_acc1 = b.param(v4i32);
            let p_acc2 = b.param(v4i32);
            let p_acc3 = b.param(v4i32);
            let ap = b.ptr_add(Flags::NONE, pa, p_i);
            let bp = b.ptr_add(Flags::NONE, pb, p_i);
            let a0 = b.load(v16i8, Flags::NONE, ap, 0, MemAttrs::aligned(4));
            let a1 = b.load(v16i8, Flags::NONE, ap, 16, MemAttrs::aligned(4));
            let a2 = b.load(v16i8, Flags::NONE, ap, 32, MemAttrs::aligned(4));
            let a3 = b.load(v16i8, Flags::NONE, ap, 48, MemAttrs::aligned(4));
            let b0 = b.load(v16i8, Flags::NONE, bp, 0, MemAttrs::aligned(4));
            let b1 = b.load(v16i8, Flags::NONE, bp, 16, MemAttrs::aligned(4));
            let b2 = b.load(v16i8, Flags::NONE, bp, 32, MemAttrs::aligned(4));
            let b3 = b.load(v16i8, Flags::NONE, bp, 48, MemAttrs::aligned(4));
            let dot_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(v4i32), Param::of(v16i8), Param::of(v16i8)],
                &[Param::of(v4i32)],
            );
            let dot_id = b.intrinsic(intr_name, dot_sig);
            let s0 = b.target_intrinsic(dot_id, &[p_acc0, a0, b0]);
            let s1 = b.target_intrinsic(dot_id, &[p_acc1, a1, b1]);
            let s2 = b.target_intrinsic(dot_id, &[p_acc2, a2, b2]);
            let s3 = b.target_intrinsic(dot_id, &[p_acc3, a3, b3]);
            let c64 = b.iconst(Ty::I64, 64);
            let i_next = b.binary(Op::Add, Flags::NONE, p_i, c64);
            let cond = b.icmp(IntCond::Ult, i_next, n);
            b.br(
                Flags::NONE,
                cond,
                (loop_hdr, &[i_next, s0, s1, s2, s3]),
                (exit, &[s0, s1, s2, s3]),
            );

            b.begin(exit);
            let e0 = b.param(v4i32);
            let e1 = b.param(v4i32);
            let e2 = b.param(v4i32);
            let e3 = b.param(v4i32);
            let sum01 = b.binary(Op::Add, Flags::NONE, e0, e1);
            let sum23 = b.binary(Op::Add, Flags::NONE, e2, e3);
            let sum = b.binary(Op::Add, Flags::NONE, sum01, sum23);
            let red = b.convert(Op::ReduceAdd, Flags::NONE, Ty::I32, sum);
            b.ret(&[red]);

            let func = b.finish_checked().expect("4x neon acc loop builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            let ra = allocate(&view, &skip, &tail, 32, crate::Os::MacOs, alloc)
                .expect("regalloc succeeds for 4x neon acc loop");

            let pairs = [(p_acc0, s0), (p_acc1, s1), (p_acc2, s2), (p_acc3, s3)];
            let mut regs = [u8::MAX; 4];
            for (k, &(acc_p, acc_next)) in pairs.iter().enumerate() {
                let loc_p = ra.loc_of[acc_p.index()];
                let loc_n = ra.loc_of[acc_next.index()];
                assert_eq!(
                    loc_n, loc_p,
                    "{intr_name} acc[{k}] must coalesce in-place with loop param"
                );
                match loc_p {
                    AllocLoc::Vec(r) => regs[k] = r,
                    other => panic!("{intr_name} acc[{k}] expected Vec reg, got {other:?}"),
                }
            }
            for i in 0..4 {
                for j in (i + 1)..4 {
                    assert_ne!(regs[i], regs[j], "{intr_name} accumulators must hold distinct regs");
                }
            }
            assert_eq!(
                ra.loc_of[i_next.index()],
                ra.loc_of[p_i.index()],
                "{intr_name} loop IV must coalesce in-place"
            );

            // For `blaze.neon.sdot`, also emit AArch64 machine code and assert ZERO `mov.16b`
            // inside the loop body and ZERO back-edge copy trampoline after `ret`.
            if intr_name == "blaze.neon.sdot" {
                let emitted = super::super::isel::emit_sota_a64(
                    view,
                    &NoNames,
                    super::super::CallMode::DirectBl,
                    Machine::LSE,
                    true,
                    alloc,
                )
                .expect("emit_sota_a64 succeeds for 4x sdot loop");
                assert_eq!(emitted.code.len() % 4, 0);
                let mut words: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(emitted.code.len() / 4, alloc);
                for chunk in emitted.code.chunks_exact(4) {
                    words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
                assert_eq!(
                    words.last().copied(),
                    Some(crate::enc::ret()),
                    "back-edge must not emit an out-of-line copy trampoline after ret"
                );
                let mut back_branch = None;
                for (i, &w) in words.iter().enumerate() {
                    if (w & 0xff00_0010) == 0x5400_0000 {
                        let imm19 = ((w >> 5) & 0x7_ffff) as i32;
                        let disp = (imm19 << 13) >> 13;
                        if disp < 0 {
                            back_branch = Some((i, (i as isize + disp as isize) as usize));
                        }
                    }
                }
                let (b_idx, loop_start) =
                    back_branch.expect("expected direct backward b.cond back-edge to loop header");
                let loop_words = &words[loop_start..=b_idx];
                let sdot_count = loop_words
                    .iter()
                    .filter(|&&w| (w & 0xffe0_fc00) == 0x4e80_9400)
                    .count();
                let mov16b_count = loop_words
                    .iter()
                    .filter(|&&w| {
                        (w & 0xffe0_fc00) == 0x4ea0_1c00 && ((w >> 16) & 0x1f) == ((w >> 5) & 0x1f)
                    })
                    .count();
                assert_eq!(sdot_count, 4, "expected 4 sdot.4s instructions in loop body");
                assert_eq!(
                    mov16b_count, 0,
                    "expected ZERO mov.16b instructions in 4x sdot loop body, got {mov16b_count}"
                );
            }
        }
    }

    #[test]
    fn matmul_4x16_neon_zero_vec_spills_and_zero_inner_ptr_spills() {
        let alloc = test_alloc();
        let v4f32 = Ty::vector(Scalar::F32, 4).expect("v4f32");
        const OFFSETS: [i32; 4] = [0, 16, 32, 48];

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR), // a_base
                Param::of(Ty::PTR), // b_base
                Param::of(Ty::PTR), // c_base
                Param::of(Ty::I64), // m
                Param::of(Ty::I64), // n
                Param::of(Ty::I64), // k_len
                Param::of(Ty::I64), // a_stride
                Param::of(Ty::I64), // b_stride
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        let i_loop = b.create_block(BlockKind::Normal);
        let j_loop = b.create_block(BlockKind::Normal);
        let k_loop = b.create_block(BlockKind::Normal);
        let j_tail = b.create_block(BlockKind::Normal);
        let i_tail = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        // Depth 0: 8 params + 14 cold outer GPRs live across the entire 3-deep loop nest
        // (total GPR pressure > 26 so spilling MUST occur on cold outer values, never inner ptrs!).
        b.begin(entry);
        let a_base = b.param(Ty::PTR);
        let b_base = b.param(Ty::PTR);
        let c_base = b.param(Ty::PTR);
        let m = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let k_len = b.param(Ty::I64);
        let a_stride = b.param(Ty::I64);
        let b_stride = b.param(Ty::I64);

        let mut outer_vals = Vec::with_capacity_in(14, alloc);
        for idx_k in 0..14u64 {
            let ck = b.iconst(Ty::I64, u128::from(idx_k * 7 + 3));
            let base_v = match idx_k % 5 {
                0 => m,
                1 => n,
                2 => k_len,
                3 => a_stride,
                _ => b_stride,
            };
            let v = b.binary(Op::Add, Flags::NONE, base_v, ck);
            outer_vals.push(v);
        }
        let z64 = b.iconst(Ty::I64, 0);
        b.jump((i_loop, &[z64]));

        // Depth 1 (`i` loop): 4 row pointers into A and 4 row pointers into C live across `j`/`k`.
        b.begin(i_loop);
        let i_iv = b.param(Ty::I64);
        let row_off = b.binary(Op::Mul, Flags::NONE, i_iv, a_stride);
        let mut a_rows = Vec::with_capacity_in(4, alloc);
        let mut c_rows = Vec::with_capacity_in(4, alloc);
        let mut cur_a = b.ptr_add(Flags::NONE, a_base, row_off);
        let mut cur_c = b.ptr_add(Flags::NONE, c_base, row_off);
        for r in 0..4usize {
            if r > 0 {
                cur_a = b.ptr_add(Flags::NONE, cur_a, a_stride);
                cur_c = b.ptr_add(Flags::NONE, cur_c, b_stride);
            }
            a_rows.push(cur_a);
            c_rows.push(cur_c);
        }
        let z64_j = b.iconst(Ty::I64, 0);
        b.jump((j_loop, &[z64_j]));

        // Depth 2 (`j` loop): load 16 `v4f32` accumulators and enter depth-3 `k` loop.
        b.begin(j_loop);
        let j_iv = b.param(Ty::I64);
        let c_two = b.iconst(Ty::I64, 2);
        let j_bytes = b.binary(Op::Shl, Flags::NONE, j_iv, c_two);
        let b_j = b.ptr_add(Flags::NONE, b_base, j_bytes);
        let mut c_j_ptrs = Vec::with_capacity_in(4, alloc);
        let mut init_c = Vec::with_capacity_in(16, alloc);
        for r in 0..4usize {
            let cj = b.ptr_add(Flags::NONE, c_rows[r], j_bytes);
            c_j_ptrs.push(cj);
            for col in 0..4usize {
                init_c.push(b.load(v4f32, Flags::NONE, cj, OFFSETS[col], MemAttrs::aligned(4)));
            }
        }
        let z64_k = b.iconst(Ty::I64, 0);
        let mut k_entry_args = Vec::with_capacity_in(22, alloc);
        k_entry_args.push(z64_k);
        for r in 0..4usize {
            k_entry_args.push(a_rows[r]);
        }
        k_entry_args.push(b_j);
        for &cv in &init_c {
            k_entry_args.push(cv);
        }
        b.jump((k_loop, &k_entry_args));

        // Depth 3 (`k` loop): 5 running pointers (`pa0..pa3, pb`), 16 `v4f32` accumulators,
        // 4 `v4f32` B loads, scalar `f32` loads + `dup.4s` + strict-IEEE `fmul.4s` + `fadd.4s`.
        b.begin(k_loop);
        let k_iv = b.param(Ty::I64);
        let mut pa = Vec::with_capacity_in(4, alloc);
        for _ in 0..4usize {
            pa.push(b.param(Ty::PTR));
        }
        let pb = b.param(Ty::PTR);
        let mut acc = Vec::with_capacity_in(16, alloc);
        for _ in 0..16usize {
            acc.push(b.param(v4f32));
        }

        let mut b_vecs = Vec::with_capacity_in(4, alloc);
        for col in 0..4usize {
            b_vecs.push(b.load(v4f32, Flags::NONE, pb, OFFSETS[col], MemAttrs::aligned(4)));
        }
        let mut next_acc = Vec::with_capacity_in(16, alloc);
        for r in 0..4usize {
            let a_sc = b.load(Ty::F32, Flags::NONE, pa[r], 0, MemAttrs::aligned(4));
            let va = b.convert(Op::Splat, Flags::NONE, v4f32, a_sc);
            for col in 0..4usize {
                let prod = b.binary(Op::Fmul, Flags::NONE, va, b_vecs[col]);
                let sum = b.binary(Op::Fadd, Flags::NONE, acc[r * 4 + col], prod);
                next_acc.push(sum);
            }
        }
        let c4_ptr = b.iconst(Ty::I64, 4);
        let mut pa_next = Vec::with_capacity_in(4, alloc);
        for r in 0..4usize {
            pa_next.push(b.ptr_add(Flags::NONE, pa[r], c4_ptr));
        }
        let pb_next = b.ptr_add(Flags::NONE, pb, b_stride);
        let c1_k = b.iconst(Ty::I64, 1);
        let k_next = b.binary(Op::Add, Flags::NONE, k_iv, c1_k);
        let cond_k = b.icmp(IntCond::Ult, k_next, k_len);
        let mut k_back_args = Vec::with_capacity_in(22, alloc);
        k_back_args.push(k_next);
        for &p in &pa_next {
            k_back_args.push(p);
        }
        k_back_args.push(pb_next);
        for &cv in &next_acc {
            k_back_args.push(cv);
        }
        b.br(Flags::NONE, cond_k, (k_loop, &k_back_args), (j_tail, &next_acc));

        // Depth 2 tail: store 16 `v4f32` accumulators and advance `j`.
        b.begin(j_tail);
        let mut out_c = Vec::with_capacity_in(16, alloc);
        for _ in 0..16usize {
            out_c.push(b.param(v4f32));
        }
        for r in 0..4usize {
            for col in 0..4usize {
                b.store(Flags::NONE, out_c[r * 4 + col], c_j_ptrs[r], OFFSETS[col], MemAttrs::aligned(4));
            }
        }
        let c16_j = b.iconst(Ty::I64, 16);
        let j_next = b.binary(Op::Add, Flags::NONE, j_iv, c16_j);
        let cond_j = b.icmp(IntCond::Ult, j_next, n);
        b.br(Flags::NONE, cond_j, (j_loop, &[j_next]), (i_tail, &[]));

        // Depth 1 tail: advance `i`.
        b.begin(i_tail);
        let c4_i = b.iconst(Ty::I64, 4);
        let i_next = b.binary(Op::Add, Flags::NONE, i_iv, c4_i);
        let cond_i = b.icmp(IntCond::Ult, i_next, m);
        b.br(Flags::NONE, cond_i, (i_loop, &[i_next]), (exit, &[]));

        // Depth 0 exit: consume all 14 outer values so they stay live across the entire loop nest.
        b.begin(exit);
        let mut ret_v = outer_vals[0];
        for &ov in &outer_vals[1..] {
            ret_v = b.binary(Op::Xor, Flags::NONE, ret_v, ov);
        }
        b.ret(&[ret_v]);

        let func = b.finish_checked().expect("4x16 NEON GEMM LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }

        let ra = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("regalloc succeeds for 4x16 NEON GEMM");

        // 1. ZERO VecSpill across the entire function, using callee-saved v8..v15 (`d_pairs != 0`).
        let vec_spills = ra
            .loc_of
            .iter()
            .filter(|loc| matches!(loc, AllocLoc::VecSpill(_)))
            .count();
        assert_eq!(
            vec_spills, 0,
            "4x16 NEON GEMM must have ZERO VecSpill (29 allocatable NEON regs via v8..v15)"
        );
        assert_ne!(
            ra.d_pairs, 0,
            "4x16 NEON GEMM must allocate callee-saved v8..v15 and mark d_pairs"
        );

        // 2. GPR pressure exceeded 26, so cold outer values spilled — NEVER the 5 depth-3 pointers!
        assert!(
            ra.num_spill_slots > 0,
            "test must exercise GPR pressure > 26 so Phase 2 loop-weighted spilling is verified"
        );
        let inner_ptrs = [
            ("pa0", pa[0], pa_next[0]),
            ("pa1", pa[1], pa_next[1]),
            ("pa2", pa[2], pa_next[2]),
            ("pa3", pa[3], pa_next[3]),
            ("pb", pb, pb_next),
        ];
        for (name, p_cur, p_nxt) in inner_ptrs {
            assert!(
                matches!(ra.loc_of[p_cur.index()], AllocLoc::Gpr(_)),
                "depth-3 inner pointer {name} must stay in a physical GPR, got {:?}",
                ra.loc_of[p_cur.index()]
            );
            assert!(
                matches!(ra.loc_of[p_nxt.index()], AllocLoc::Gpr(_)),
                "depth-3 inner pointer {name}_next must stay in a physical GPR, got {:?}",
                ra.loc_of[p_nxt.index()]
            );
        }

        // 3. Full AArch64 emission succeeds with `d_pairs != 0` in the unwind frame.
        let emitted = super::super::isel::emit_sota_a64(
            view,
            &NoNames,
            super::super::CallMode::DirectBl,
            Machine::LSE,
            true,
            alloc,
        )
        .expect("emit_sota_a64 succeeds for 4x16 NEON GEMM");
        assert_ne!(
            emitted.unwind_frame.expect("4x16 GEMM has frame").d_pairs,
            0,
            "emitted unwind frame must record saved d_pairs for v8..v15"
        );
    }
}

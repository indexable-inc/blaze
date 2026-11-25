//! Spill-Capable, Loop-Weighted, Coalescing Chordal-SSA Register Allocator (GPR + Pair + Scalar FP)
//! for the non-LLVM `Sota` AArch64 tier.
//!
//! Hybrid architecture combining exact CFG precision with guaranteed O(N log K) scaling:
//! - **Exact-CFG mode** (`num_vals <= EXACT_CFG_MAX_VALS && blocks <= EXACT_CFG_MAX_BLOCKS`, covering
//!   99.9% of functions): exact backward CFG liveness + symmetric SSA interference bitmatrix +
//!   exact `crosses_call` + pre-rematerialization of call-crossing 1-inst constants and cold-only
//!   `DataAddr`/`FuncAddr` (`AllocLoc::RematGlobal`) + aggressive non-interfering coalescing.
//! - **Mega-function RPO linear-scan mode** (functions exceeding the exact-CFG budget, e.g. 600k-inst
//!   generated parsers): O(N log K) RPO-ordered `[def, last_use]` intervals (extended to fixed point
//!   across deduplicated RPO back-edges) + `BusySet` + binary-searched RPO `call_positions` +
//!   `RematConst`/`RematGlobal` on pressure overflow.
//!
//! Both modes share:
//! 1. **Coalescing hints** for entry parameters (`X0..X7`, `X8` `sret`, `V0..V7`), block-parameter
//!    (`phi`) edges, return values (`X0`/`X1`, `V0`), and 2-address in-place accumulators.
//! 2. **2-word GPR pairs** (`AllocLoc::GprPair` / `AllocLoc::GprPairSpill`) for `{i64, i1}` overflow
//!    ops, `i128`, and 2-register returns (`Option<u64>`, `Result<T, E>`, `&[T]`).
//! 3. **Two-phase coloring** (Phase 1 minimal-register chordal order; Phase 2 loop-weighted greedy
//!    priority order when pressure overflows so inner loops stay register-resident).
//! 4. **Frameless leaf functions** (`X0..X15`, `V0..V7`/`V16..V28`, zero stack).

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
const X30: Reg = 30;

/// Unassigned hint sentinel.
pub(crate) const NO_REG: u8 = u8::MAX;

/// Maximum allocated SSA values for exact-CFG interference bitmatrix mode
/// (`6144 * 6144 / 8 = 4.5 MiB` worst-case bitmatrix).
const EXACT_CFG_MAX_VALS: usize = 6144;

/// Maximum basic blocks for exact-CFG backward liveness mode
/// (`3072 * 6144 / 8 = 2.25 MiB` worst-case `live_in` table).
const EXACT_CFG_MAX_BLOCKS: usize = 3072;

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

/// Callee-saved allocatable GPRs for provably zero-call leaf functions: appends `X30` (`LR`) at
/// the very end of the pool so it is used only once all 26 other GPRs are busy (`stp/ldp x29, x30`
/// saves and restores `X30`).
const CALLEE_SAVED_GPR_WITH_LR: &[Reg] = &[X19, X20, X21, X22, X23, X24, X25, X26, X27, X28, X30];

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
    /// Rematerialized global/data/func address (`Op::DataAddr` / `Op::FuncAddr` instruction index):
    /// re-emitted at cold use sites via `adrp` + `add`/`ldr` with ZERO callee-saved GPRs!
    RematGlobal(u32),
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
    /// `true` iff `X30` (`LR`) was allocated as a GPR in a zero-call leaf function (requiring
    /// `stp/ldp x29, x30` frame record so `LR` is saved on entry and restored before `ret`/`b`).
    pub(crate) uses_lr: bool,
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

/// Resolves a zero-cost aliased value (`Op::Trunc`/`Op::Zext`) to its non-aliased root SSA index,
/// or returns `v` unchanged when `alias_of` is empty or `alias_of[v] == u32::MAX`.
#[inline]
fn resolve_alias(alias_of: &[u32], v: usize) -> usize {
    match alias_of.get(v).copied() {
        Some(root) if root != u32::MAX => idx(root),
        _ => v,
    }
}

/// Returns the preferred same-bank operand `src` (ordered before `inst` by `is_before`) that
/// `inst` should coalesce into:
/// - Destructive 2-address NEON accumulators (`Fma`/`Fmuladd` operand `c`, `Fadd`/`Fsub` fused with
///   `Fmul`, `Add`/`Sub` fused with `Mul` or `VecUadalp`, reduction/IV chains)
/// - In-place lane insertion (`Insert` base vector)
/// - Same-bank zero-copy `Bitcast` / `Freeze`
/// - Dying operand of 128-bit vector `Select` (`bsl`/`bit`/`bif` in 1 instruction)
fn preferred_inplace_operand(
    view: &View<'_>,
    skip_alloc: &[bool],
    alias_of: &[u32],
    is_before: impl Fn(usize) -> bool,
    is_dying: impl Fn(usize) -> bool,
    inst: usize,
) -> Option<usize> {
    let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
    let bank = reg_bank(view, ty);
    if bank == 0 {
        return None;
    }
    let len = view.len();
    let valid_src = |v: u32| -> Option<usize> {
        let vi = resolve_alias(alias_of, idx(v));
        if vi >= len || !is_before(vi) || skip_alloc.get(vi).copied().unwrap_or(false) {
            return None;
        }
        let sty = view.tys.get(vi).copied().map_or(Ty::NONE, Ty);
        if reg_bank(view, sty) == bank { Some(vi) } else { None }
    };
    let op_of = |i: usize| -> Option<Op> { (i < len && is_before(i)).then(|| view.op(i)) };
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
            let raw_ai = idx(a);
            let (ai, bi) = (resolve_alias(alias_of, raw_ai), resolve_alias(alias_of, idx(b)));
            let (oa, ob) = (op_of(ai), op_of(bi));
            // Fused `VecUadalp`: `add(add(acc, and(...)), lshr(...))` where the inner `add` is skipped.
            if skip_alloc.get(raw_ai).copied().unwrap_or(false) && op_of(raw_ai) == Some(Op::Add) {
                if let Some(&iat_raw) = view.data.get(raw_ai) {
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
            let (ai, bi) = (resolve_alias(alias_of, idx(a)), resolve_alias(alias_of, idx(b)));
            let (oa, ob) = (op_of(ai), op_of(bi));
            if ob == Some(Op::Mul) && oa != Some(Op::Mul) {
                valid_src(a)
            } else if oa == Some(Op::Param) {
                valid_src(a)
            } else if oa == Some(Op::Sub) {
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
                    if is_dying(ci) {
                        return Some(ci);
                    }
                }
            }
            None
        }
        Op::Xor | Op::Or | Op::And | Op::Mul if bank == 3 => {
            let at = idx(*view.data.get(inst)?);
            let a = *view.extra.get(at)?;
            let b = *view.extra.get(at + 1)?;
            if let Some(ai) = valid_src(a).filter(|&i| is_dying(i)) {
                Some(ai)
            } else {
                valid_src(b).filter(|&i| is_dying(i))
            }
        }
        Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr if bank == 3 => {
            let at = idx(*view.data.get(inst)?);
            let a = *view.extra.get(at)?;
            valid_src(a).filter(|&i| is_dying(i))
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
    let len = view.len();
    let vec_reg_of = |v: u32| -> u8 {
        let vi = idx(v);
        if vi < len && skip_alloc.get(vi).copied().unwrap_or(false) && view.op(vi) == Op::Splat {
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
            if ai < len && skip_alloc.get(ai).copied().unwrap_or(false) && view.op(ai) == Op::Fmul {
                mul_operands(ai)
            } else if bi < len && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Fmul {
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
            if bi < len && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Fmul {
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
            if ai < len && skip_alloc.get(ai).copied().unwrap_or(false) && view.op(ai) == Op::Mul {
                mul_operands(ai)
            } else if bi < len && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Mul {
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
            if bi < len && skip_alloc.get(bi).copied().unwrap_or(false) && view.op(bi) == Op::Mul {
                mul_operands(bi)
            } else {
                (NO_REG, NO_REG)
            }
        }
        _ => (NO_REG, NO_REG),
    }
}

/// Sentinel for instructions without a dense liveness value id.
const NO_VAL: u32 = u32::MAX;

/// Visits every non-skipped transitive operand `(direct_user, val_inst)` read by `inst`,
/// expanding iteratively (zero recursion) through `skip_alloc` so fused producers (e.g.
/// cross-block `PtrAdd(base, Shl(idx, k))`, `CmpBr`, `MemBaseOff`) keep their true operands
/// live at every emitted consumer site.
fn for_each_expanded_use<A: Allocator>(
    view: &View<'_>,
    skip_alloc: &[bool],
    inst: usize,
    work: &mut Vec<(usize, u32), A>,
    mut f: impl FnMut(usize, usize),
) {
    let len = view.len();
    let max_steps = len.saturating_mul(2).max(64);
    work.clear();
    for_each_use(view, inst, |v| work.push((inst, v)));
    let mut steps = 0usize;
    while let Some((user, v)) = work.pop() {
        let vi = idx(v);
        if vi >= len {
            continue;
        }
        if skip_alloc.get(vi).copied().unwrap_or(false) {
            if steps < max_steps {
                steps += 1;
                for_each_use(view, vi, |u| work.push((vi, u)));
            }
        } else {
            f(user, vi);
        }
    }
}

/// Visits every non-skipped transitive operand of a single value `root` (e.g. a branch
/// condition or a specific edge argument), expanding iteratively through `skip_alloc`.
fn for_each_expanded_val<A: Allocator>(
    view: &View<'_>,
    skip_alloc: &[bool],
    user: usize,
    root: u32,
    work: &mut Vec<(usize, u32), A>,
    mut f: impl FnMut(usize, usize),
) {
    let len = view.len();
    let max_steps = len.saturating_mul(2).max(64);
    work.clear();
    work.push((user, root));
    let mut steps = 0usize;
    while let Some((cur_user, v)) = work.pop() {
        let vi = idx(v);
        if vi >= len {
            continue;
        }
        if skip_alloc.get(vi).copied().unwrap_or(false) {
            if steps < max_steps {
                steps += 1;
                for_each_use(view, vi, |u| work.push((vi, u)));
            }
        } else {
            f(cur_user, vi);
        }
    }
}

/// Clears bits in `[lo, hi)` from bitset word `word_idx`.
#[inline]
fn mask_out_range(word_idx: usize, word: u64, lo: usize, hi: usize) -> u64 {
    let bit_lo = word_idx.saturating_mul(64);
    let bit_hi = bit_lo.saturating_add(64);
    if hi <= bit_lo || lo >= bit_hi || lo >= hi {
        return word;
    }
    let start = lo.saturating_sub(bit_lo);
    let end = hi.min(bit_hi) - bit_lo;
    let count = end - start;
    let mask = if count >= 64 {
        u64::MAX
    } else {
        ((1u64 << count) - 1) << start
    };
    word & !mask
}

/// True iff `isel.rs` consumes constant operand `vi` (`cval`) purely as an inline instruction
/// immediate at `user` without ever loading `vi` into a GPR.
fn is_folded_imm_use(view: &View<'_>, user: usize, vi: usize, cval: u64) -> bool {
    let op = view.op(user);
    let u_ty = view.tys.get(user).copied().map_or(Ty::NONE, Ty);
    let bits = if u_ty.is_scalar() && u_ty.is_int() { u_ty.bits(64) } else { 0 };
    let Some(&at_raw) = view.data.get(user) else {
        return false;
    };
    let at = idx(at_raw);
    match op {
        Op::Add if bits == 64 || bits == 32 => {
            if super::isel::is_addsub_imm12(cval).is_none() {
                return false;
            }
            let (Some(&a), Some(&b)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                return false;
            };
            let (ai, bi) = (idx(a), idx(b));
            ai != bi && (ai == vi || bi == vi)
        }
        Op::Sub if bits == 64 || bits == 32 => {
            if super::isel::is_addsub_imm12(cval).is_none() {
                return false;
            }
            let (Some(&a), Some(&b)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                return false;
            };
            idx(b) == vi && idx(a) != vi
        }
        Op::And | Op::Or | Op::Xor if bits == 64 || bits == 32 => {
            if crate::enc::logical_imm(cval, bits).is_none() {
                return false;
            }
            let (Some(&a), Some(&b)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                return false;
            };
            let (ai, bi) = (idx(a), idx(b));
            ai != bi && (ai == vi || bi == vi)
        }
        Op::Icmp => {
            if super::isel::is_addsub_imm12(cval).is_none() {
                return false;
            }
            let (Some(&a), Some(&b)) = (view.extra.get(at + 1), view.extra.get(at + 2)) else {
                return false;
            };
            let a_ty = view.tys.get(idx(a)).copied().map_or(Ty::NONE, Ty);
            let abits = if a_ty.is_scalar() && a_ty.is_int() { a_ty.bits(64) } else { 0 };
            (abits == 64 || abits == 32)
                && idx(a) != idx(b)
                && (idx(b) == vi || (cval == 0 && idx(a) == vi))
        }
        Op::PtrAdd => {
            let Some(&b) = view.extra.get(at + 1) else {
                return false;
            };
            if idx(b) != vi {
                return false;
            }
            let b_ty = view.tys.get(vi).copied().map_or(Ty::NONE, Ty);
            let bbits = b_ty.bits(64);
            let delta = if bbits >= 64 {
                cval.cast_signed()
            } else {
                let shift = 64 - bbits;
                (cval << shift).cast_signed() >> shift
            };
            super::isel::is_addsub_imm12(delta.unsigned_abs()).is_some()
        }
        Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr if bits == 64 || bits == 32 => {
            let (Some(&a), Some(&b)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                return false;
            };
            idx(b) == vi && idx(a) != vi
        }
        Op::Select
            if (cval == 0 || cval == 1)
                && u_ty != Ty::I128
                && is_fp_ty(u_ty).is_none()
                && is_vec128_ty(u_ty).is_none() =>
        {
            let (Some(&c), Some(&t), Some(&f)) = (
                view.extra.get(at),
                view.extra.get(at + 1),
                view.extra.get(at + 2),
            ) else {
                return false;
            };
            idx(c) != vi && (idx(t) == vi || idx(f) == vi)
        }
        _ => false,
    }
}

/// True iff `inst` lowers to a non-tail call or runtime helper call that clobbers caller-saved registers.
fn is_clobbering_call_inst(view: &View<'_>, tail_calls: &[bool], inst: usize) -> bool {
    if tail_calls.get(inst).copied().unwrap_or(false) {
        return false;
    }
    let op = view.op(inst);
    let is_runtime_bulk = matches!(op, Op::Memcpy | Op::Memset | Op::Memcmp) && {
        let const_ok = view
            .data
            .get(inst)
            .and_then(|&d| view.extra.get(idx(d) + 2))
            .map(|&v| idx(v))
            .is_some_and(|len_v| extract_remat_const(view, len_v).is_some_and(|n| n <= 256));
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
    matches!(
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
        || is_sme2_call
}

/// Extracts the rematerializable 64-bit scalar integer constant for `inst`, if any.
fn extract_remat_const(view: &View<'_>, inst: usize) -> Option<u64> {
    let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
    if !(ty.is_scalar() && ty.is_int() && ty.bits(64) <= 64) {
        return None;
    }
    let bits = ty.bits(64);
    let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    match view.op(inst) {
        Op::Iconst => view.data.get(inst).map(|&w| {
            let v = if bits <= 32 { u64::from(w) } else { sext32_u64(w) };
            v & mask
        }),
        Op::IconstWide => view.data.get(inst).and_then(|&d| {
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
}

/// Number of AArch64 instructions (`1..=4`) required to materialize 64-bit integer constant `v`
/// via `movz`/`movn` + `movk` or a single logical bitmask immediate.
#[inline]
fn const_mat_cost(v: u64) -> u32 {
    if v == 0 || v == u64::MAX {
        return 1;
    }
    let mut nz_chunks = 0u32;
    let mut nf_chunks = 0u32;
    for hw in 0..4u32 {
        let chunk = (v >> (hw * 16)) & 0xffff;
        if chunk != 0 {
            nz_chunks += 1;
        }
        if chunk != 0xffff {
            nf_chunks += 1;
        }
    }
    let mov_cost = nz_chunks.min(nf_chunks).max(1);
    if mov_cost == 1 || crate::enc::logical_imm(v, 64).is_some() {
        1
    } else {
        mov_cost
    }
}

/// Computes `(priority_weight, cheap_remat)` for register-allocation ordering and eviction cost.
///
/// Weights are in uniform instruction-cost units (`8 * uw` for a non-constant scalar stack spill,
/// `2 * uw` for `adrp + add/ldr` global remat). For integer constants:
/// - `1`-inst constants (`0..=65535`, bitmask imms): `1 * net_uw.max(1)`, `cheap_remat = true` so
///   cheap immediates always sort behind non-remat scalars and never steal GPRs under pressure.
/// - `2`-inst constants inside loops: `6 * net_uw`, `cheap_remat = false`.
/// - `3`-inst and `4`-inst constants inside loops (e.g. 64-bit hash primes like
///   `0x9e3779b185ebca87`): `4 * mat_cost * uw` (`12 * uw` for 3-inst, `16 * uw` for 4-inst, using
///   full `uw` rather than `uw - def_w` so loop-local 4-inst constants never collapse to weight `1`
///   and rematerialize `movz + 3*movk` at every use), `cheap_remat = false`.
/// - Outside loops (`!in_loop`) or when `net_uw == 0` for `mat_cost <= 2`: `mat_cost * net_uw`
///   (min `1`), `cheap_remat = true`.
#[inline]
fn candidate_priority(
    op: Op,
    remat_const: Option<u64>,
    uw: u32,
    def_w: u32,
    in_loop: bool,
) -> (u64, bool) {
    if let Some(cval) = remat_const {
        let mat_cost = const_mat_cost(cval);
        if in_loop && mat_cost >= 3 {
            let scale = u64::from(mat_cost).saturating_mul(4);
            return (u64::from(uw).saturating_mul(scale).max(1), false);
        }
        let net_uw = u64::from(uw.saturating_sub(def_w));
        if net_uw == 0 {
            return (1, true);
        }
        let (scale, cheap_remat) = if !in_loop || mat_cost <= 1 {
            (u64::from(mat_cost), true)
        } else {
            (6u64, false)
        };
        (net_uw.saturating_mul(scale).max(1), cheap_remat)
    } else if matches!(op, Op::DataAddr | Op::FuncAddr) {
        (u64::from(uw).saturating_mul(2).max(1), true)
    } else {
        (u64::from(uw).saturating_mul(8), false)
    }
}

/// Computes iterative-DFS Reverse Post-Order (`rpo_blocks`, `rpo_idx`) and deduplicated per-block
/// loop nesting depth (`block_depth`) in O(B + E) time with zero recursion.
///
/// Back-edges are identified in RPO coordinate space (`rpo_idx[target] <= rpo_idx[src]`) so
/// out-of-order CFG layouts never lose loop depth or synthesize false loops from forward jumps.
/// Multiple back-edges targeting the same loop header (e.g. `continue` paths) are merged to the
/// farthest latch rank so one loop contributes depth `+1` rather than `+k`.
fn compute_rpo_and_loop_depth<A: Allocator + Copy>(
    view: &View<'_>,
    alloc: A,
) -> (Vec<usize, A>, Vec<usize, A>, Vec<u32, A>) {
    let blocks = view.blocks();
    let mut dfs_state: Vec<u8, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        dfs_state.push(0);
    }
    let mut rpo_blocks: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    let mut dfs_stack: Vec<usize, A> = Vec::with_capacity_in(blocks.min(1024), alloc);
    if blocks > 0 {
        dfs_stack.push(0);
        while let Some(b) = dfs_stack.pop() {
            let st = dfs_state.get(b).copied().unwrap_or(2);
            if st == 0 {
                if let Some(s) = dfs_state.get_mut(b) {
                    *s = 1;
                }
                dfs_stack.push(b);
                let term = view.terminator(Block(u32_of(b)));
                for_each_edge(view, term, |target_block, _, _| {
                    let t = idx(target_block);
                    if dfs_state.get(t).copied() == Some(0) {
                        dfs_stack.push(t);
                    }
                });
            } else if st == 1 {
                if let Some(s) = dfs_state.get_mut(b) {
                    *s = 2;
                }
                rpo_blocks.push(b);
            }
        }
        rpo_blocks.reverse();
        for b in 0..blocks {
            if dfs_state.get(b).copied() == Some(0) {
                rpo_blocks.push(b);
            }
        }
    }

    let mut rpo_idx: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        rpo_idx.push(0);
    }
    for (rank, &b) in rpo_blocks.iter().enumerate() {
        if let Some(slot) = rpo_idx.get_mut(b) {
            *slot = rank;
        }
    }

    let mut max_latch_rank: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        max_latch_rank.push(usize::MAX);
    }
    for (rank, &b) in rpo_blocks.iter().enumerate() {
        let term = view.terminator(Block(u32_of(b)));
        for_each_edge(view, term, |target_block, _, _| {
            let tr = rpo_idx.get(idx(target_block)).copied().unwrap_or(blocks);
            if tr <= rank {
                if let Some(slot) = max_latch_rank.get_mut(tr) {
                    if *slot == usize::MAX || rank > *slot {
                        *slot = rank;
                    }
                }
            }
        });
    }

    let mut depth_diff: Vec<i32, A> = Vec::with_capacity_in(blocks + 1, alloc);
    for _ in 0..=blocks {
        depth_diff.push(0);
    }
    for (tr, &latch) in max_latch_rank.iter().enumerate() {
        if latch != usize::MAX {
            if let Some(s) = depth_diff.get_mut(tr) {
                *s = s.saturating_add(1);
            }
            if let Some(e) = depth_diff.get_mut(latch + 1) {
                *e = e.saturating_sub(1);
            }
        }
    }

    let mut block_depth: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_depth.push(0);
    }
    let mut cur_depth: i32 = 0;
    for (rank, &b) in rpo_blocks.iter().enumerate() {
        cur_depth = cur_depth.saturating_add(depth_diff.get(rank).copied().unwrap_or(0));
        if let Some(slot) = block_depth.get_mut(b) {
            *slot = u32::try_from(cur_depth.max(0)).unwrap_or(u32::MAX);
        }
    }

    (rpo_blocks, rpo_idx, block_depth)
}

/// Seeds ABI register hints for entry parameters (including `sret` in `X8`), landing-pad
/// exception parameters (`X0`), and `Op::Return` values (`X0`/`X1`/`V0`/`V1`).
fn seed_abi_and_return_hints(view: &View<'_>, alias_of: &[u32], os: crate::Os, hint: &mut [u8]) {
    let func_sig = view.sig(SigId(view.head.sig));
    let entry_params = view.params(Block(0));
    let mut regs = Regs::new(os);
    for (k, p) in (entry_params.start..entry_params.end).enumerate() {
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
    let blocks = view.blocks();
    for b in 0..blocks {
        if view.block_kind(Block(u32_of(b))).is_landing() {
            let params = view.params(Block(u32_of(b)));
            if params.start < params.end {
                if let Some(slot) = hint.get_mut(params.start) {
                    *slot = reg_u8(X0);
                }
            }
        }
        let term = view.terminator(Block(u32_of(b)));
        if view.op(term) == Op::Return {
            if let Some(&at_raw) = view.data.get(term) {
                let at = idx(at_raw);
                if let Some(&retc_raw) = view.extra.get(at) {
                    let retc = idx(retc_raw);
                    if retc >= 1 {
                        if let Some(&rv0_raw) = view.extra.get(at + 1) {
                            let rv0 = resolve_alias(alias_of, idx(rv0_raw));
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
                            let rv1 = resolve_alias(alias_of, idx(rv1_raw));
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
    }
}

/// Propagates scalar FP return hints backward through lane-0 `Op::Extract` into the source vector
/// so `v -> extract(v, 0) -> ret` stays in the same FP register with zero `fmov` instructions.
fn propagate_extract_lane0_hints(view: &View<'_>, hint: &mut [u8], rev_insts: impl Iterator<Item = usize>) {
    for inst in rev_insts {
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
}

/// Builds the final [`Allocation`] with exact ARM64 unwind pair masks and IPRA clobber summary.
fn finalize_allocation<A: Allocator>(
    view: &View<'_>,
    loc_of: Vec<AllocLoc, A>,
    used_callee_gpr: [bool; 32],
    used_callee_fp: [bool; 32],
    num_spill_slots: u32,
    remat_count: u32,
    call_positions: &[usize],
    call_clobbers: impl Fn(usize) -> CallClobbers,
) -> Allocation<A> {
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

    let func_sig = view.sig(SigId(view.head.sig));
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
    for &c in call_positions {
        clob = clob.with(TierReg(30));
        clob = clob.union(call_clobbers(c).clobbered);
    }

    Allocation {
        loc_of,
        x_pairs,
        d_pairs,
        num_spill_slots,
        has_calls: !call_positions.is_empty(),
        uses_lr: used_callee_gpr.get(idx(X30)).copied().unwrap_or(false),
        clobbers: CallClobbers { clobbered: clob },
        remat_count,
    }
}

/// Returns `true` iff `view` fits within the bounded exact-CFG interference bitmatrix budget
/// (`blocks <= EXACT_CFG_MAX_BLOCKS` and allocated SSA values `<= EXACT_CFG_MAX_VALS`).
fn fits_exact_cfg<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    alloc: A,
) -> bool {
    if view.blocks() > EXACT_CFG_MAX_BLOCKS {
        return false;
    }
    let len = view.len();
    let mut typed_count = 0usize;
    for inst in 0..len {
        if !skip_alloc.get(inst).copied().unwrap_or(false)
            && view.tys.get(inst).copied().map_or(Ty::NONE, Ty) != Ty::NONE
        {
            typed_count += 1;
        }
    }
    if typed_count <= EXACT_CFG_MAX_VALS {
        return true;
    }
    let mut used: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        used.push(false);
    }
    for inst in 0..len {
        for_each_use(view, inst, |v| {
            let vi = idx(v);
            if let Some(u) = used.get_mut(vi) {
                *u = true;
            }
        });
    }
    let mut num_vals = 0usize;
    for inst in 0..len {
        if !skip_alloc.get(inst).copied().unwrap_or(false)
            && view.tys.get(inst).copied().map_or(Ty::NONE, Ty) != Ty::NONE
            && used.get(inst).copied().unwrap_or(false)
        {
            num_vals += 1;
        }
    }
    num_vals <= EXACT_CFG_MAX_VALS
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
        &[],
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
///
/// Dispatches to exact-CFG backward liveness + symmetric SSA interference bitmatrix coloring
/// whenever `blocks <= EXACT_CFG_MAX_BLOCKS` and allocated SSA values `<= EXACT_CFG_MAX_VALS`
/// (`<= 6.75 MiB` worst-case), and falls back to the O(N log K) RPO-ordered `BusySet` linear-scan
/// allocator for mega-functions (e.g. 600k-inst / 130k-block generated parsers).
#[allow(clippy::too_many_arguments)]
pub(crate) fn allocate_with_ipra_and_order<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    alias_of: &[u32],
    tail_calls: &[bool],
    max_spill_slots: u32,
    os: crate::Os,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    scratch_order: Option<&ScratchOrder>,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    let caller_gpr_order = build_caller_gpr_order(scratch_order);
    let caller_fp_order = build_caller_fp_order(scratch_order);
    if fits_exact_cfg(view, skip_alloc, alloc) {
        allocate_exact_cfg(
            view,
            skip_alloc,
            alias_of,
            tail_calls,
            max_spill_slots,
            os,
            call_clobbers,
            &caller_gpr_order,
            &caller_fp_order,
            alloc,
        )
    } else {
        allocate_linear_scan_rpo(
            view,
            skip_alloc,
            alias_of,
            tail_calls,
            max_spill_slots,
            os,
            call_clobbers,
            &caller_gpr_order,
            &caller_fp_order,
            alloc,
        )
    }
}

/// Exact-CFG backward liveness + symmetric SSA interference bitmatrix allocator with
/// call-crossing 1-inst constant rematerialization (`AllocLoc::RematConst`), cold-only
/// global/func address rematerialization (`AllocLoc::RematGlobal`), and aggressive
/// non-interfering coalescing.
#[allow(clippy::too_many_arguments)]
fn allocate_exact_cfg<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    alias_of: &[u32],
    tail_calls: &[bool],
    max_spill_slots: u32,
    os: crate::Os,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    caller_gpr_order: &[Reg; 16],
    caller_fp_order: &[Reg; 21],
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    let len = view.len();
    let blocks = view.blocks();
    let mut has_any_use: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut use_weight: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut def_weight: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut max_use_depth: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut has_hot_use: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut is_clobbering_call: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut call_positions: Vec<usize, A> = Vec::new_in(alloc);
    let mut remat_val: Vec<Option<u64>, A> = Vec::with_capacity_in(len, alloc);
    for i in 0..len {
        has_any_use.push(false);
        use_weight.push(0);
        def_weight.push(0);
        max_use_depth.push(0);
        has_hot_use.push(false);
        is_clobbering_call.push(false);
        remat_val.push(extract_remat_const(view, i));
    }

    // 0. Compute RPO-ordered, header-deduplicated loop nesting depth in O(B + E) and identify cold
    //    dead-end blocks (blocks that can never reach `Op::Return` and are not in a loop,
    //    e.g. panic/bounds-check/trap/unwind blocks).
    let (_rpo_blocks, rpo_idx, block_depth) = compute_rpo_and_loop_depth(view, alloc);
    let mut can_return: Vec<bool, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        can_return.push(matches!(view.op(term), Op::Return | Op::TailCall | Op::TailCallIndirect));
    }
    loop {
        let mut changed = false;
        for b in (0..blocks).rev() {
            if can_return.get(b).copied().unwrap_or(false) {
                continue;
            }
            let term = view.terminator(Block(u32_of(b)));
            let mut reaches_ret = false;
            for_each_edge(view, term, |target_block, _, _| {
                if can_return.get(idx(target_block)).copied().unwrap_or(false) {
                    reaches_ret = true;
                }
            });
            if reaches_ret {
                if let Some(slot) = can_return.get_mut(b) {
                    *slot = true;
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut is_cold_block: Vec<bool, A> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let cold = !can_return.get(b).copied().unwrap_or(false)
            && block_depth.get(b).copied().unwrap_or(0) == 0;
        is_cold_block.push(cold);
    }

    // 1. Scan emitted instructions: record clobbering call sites, loop-weighted use priorities,
    //    exact non-immediate use depths, and hot-vs-cold use status (expanding through `skip_alloc`).
    let mut work: Vec<(usize, u32), A> = Vec::with_capacity_in(16, alloc);
    for b in 0..blocks {
        let raw_depth = block_depth.get(b).copied().unwrap_or(0);
        let depth = raw_depth.min(3);
        let b_weight = 1u32 << depth.saturating_mul(7); // 1x outer, 128x depth-1, 16384x depth-2, 2097152x depth-3+
        let b_is_cold = is_cold_block.get(b).copied().unwrap_or(false);
        let range = view.block(Block(u32_of(b)));
        for inst in range {
            if skip_alloc.get(inst).copied().unwrap_or(false) {
                continue;
            }
            let op = view.op(inst);
            if is_clobbering_call_inst(view, tail_calls, inst) {
                call_positions.push(inst);
                if let Some(slot) = is_clobbering_call.get_mut(inst) {
                    *slot = true;
                }
            }
            // Record defining block weight; non-constants pay a store on spill at def, while
            // constants are weighted by net rematerialized use savings (`use_weight - def_weight`).
            if let Some(dw) = def_weight.get_mut(inst) {
                *dw = b_weight;
            }
            if remat_val.get(inst).copied().flatten().is_none() {
                if let Some(w) = use_weight.get_mut(inst) {
                    *w = w.saturating_add(b_weight);
                }
            }
            for_each_expanded_use(view, skip_alloc, inst, &mut work, |user, vi| {
                if let Some(u) = has_any_use.get_mut(vi) {
                    *u = true;
                }
                let imm_folded = remat_val
                    .get(vi)
                    .copied()
                    .flatten()
                    .is_some_and(|cval| is_folded_imm_use(view, user, vi, cval));
                if !imm_folded {
                    if let Some(w) = use_weight.get_mut(vi) {
                        *w = w.saturating_add(b_weight);
                    }
                    if let Some(md) = max_use_depth.get_mut(vi) {
                        if raw_depth > *md {
                            *md = raw_depth;
                        }
                    }
                }
            });
            // Track whether any non-cold path reads each operand (edge args belong to their target block
            // so `br(oob, (panic_blk, &[loc]), (ok_blk, &[]))` keeps `loc` cold-only!).
            if matches!(op, Op::Jump | Op::Br | Op::Switch) {
                if !b_is_cold && matches!(op, Op::Br | Op::Switch) {
                    if let Some(&d) = view.data.get(inst) {
                        if let Some(&cond_or_scrut) = view.extra.get(idx(d)) {
                            for_each_expanded_val(view, skip_alloc, inst, cond_or_scrut, &mut work, |_, vi| {
                                if let Some(h) = has_hot_use.get_mut(vi) {
                                    *h = true;
                                }
                            });
                        }
                    }
                }
                for_each_edge(view, inst, |target_block, args_start, args_len| {
                    if !is_cold_block.get(idx(target_block)).copied().unwrap_or(false) {
                        if let Some(args) = view.extra.get(args_start..args_start + args_len) {
                            for &arg_raw in args {
                                for_each_expanded_val(view, skip_alloc, inst, arg_raw, &mut work, |_, vi| {
                                    if let Some(h) = has_hot_use.get_mut(vi) {
                                        *h = true;
                                    }
                                });
                            }
                        }
                    }
                });
            } else if !b_is_cold {
                for_each_expanded_use(view, skip_alloc, inst, &mut work, |_, vi| {
                    if let Some(h) = has_hot_use.get_mut(vi) {
                        *h = true;
                    }
                });
            }
        }
    }

    // 1b. Exact CFG backward liveness + exact `crosses_call` + exact symmetric interference matrix!
    // Assign dense value IDs `0..num_vals` to every used, non-skipped SSA value in block/program order.
    let mut val_id: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut val_inst: Vec<usize, A> = Vec::with_capacity_in(len / 2 + 1, alloc);
    let mut blk_val_start: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    let mut blk_val_end: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..len {
        val_id.push(NO_VAL);
    }
    for b in 0..blocks {
        let start_id = val_inst.len();
        let range = view.block(Block(u32_of(b)));
        for inst in range {
            if skip_alloc.get(inst).copied().unwrap_or(false) {
                continue;
            }
            let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
            if ty != Ty::NONE && has_any_use.get(inst).copied().unwrap_or(false) {
                let id = u32_of(val_inst.len());
                val_inst.push(inst);
                if let Some(slot) = val_id.get_mut(inst) {
                    *slot = id;
                }
            }
        }
        blk_val_start.push(start_id);
        blk_val_end.push(val_inst.len());
    }
    let num_vals = val_inst.len();
    let val_words = num_vals.div_ceil(64);

    // Initialize `live_in[b]` with the upward-exposed uses of block `b`, then iterate to fixed point:
    // `live_in[b] = upward_use[b] | U_{s in succ(b)} (live_in[s] \ defs(b))`.
    let mut live_in: Vec<u64, A> = Vec::with_capacity_in(blocks.saturating_mul(val_words), alloc);
    for _ in 0..blocks.saturating_mul(val_words) {
        live_in.push(0);
    }
    if val_words > 0 {
        for b in 0..blocks {
            let b_base = b.saturating_mul(val_words);
            let range = view.block(Block(u32_of(b)));
            for inst in range.rev() {
                if skip_alloc.get(inst).copied().unwrap_or(false) {
                    continue;
                }
                let def_vid = val_id.get(inst).copied().unwrap_or(NO_VAL);
                if def_vid != NO_VAL {
                    let d = idx(def_vid);
                    if let Some(slot) = live_in.get_mut(b_base + (d >> 6)) {
                        *slot &= !(1u64 << (d & 63));
                    }
                }
                for_each_expanded_use(view, skip_alloc, inst, &mut work, |_, vi| {
                    let u_vid = val_id.get(vi).copied().unwrap_or(NO_VAL);
                    if u_vid != NO_VAL {
                        let u = idx(u_vid);
                        if let Some(slot) = live_in.get_mut(b_base + (u >> 6)) {
                            *slot |= 1u64 << (u & 63);
                        }
                    }
                });
            }
        }
        loop {
            let mut changed = false;
            for b in (0..blocks).rev() {
                let lo = blk_val_start.get(b).copied().unwrap_or(0);
                let hi = blk_val_end.get(b).copied().unwrap_or(0);
                let b_base = b.saturating_mul(val_words);
                let term = view.terminator(Block(u32_of(b)));
                for_each_edge(view, term, |target_block, _, _| {
                    let s_base = idx(target_block).saturating_mul(val_words);
                    for w in 0..val_words {
                        let sw = live_in.get(s_base + w).copied().unwrap_or(0);
                        let filtered = mask_out_range(w, sw, lo, hi);
                        if let Some(bw) = live_in.get_mut(b_base + w) {
                            let next = *bw | filtered;
                            if next != *bw {
                                *bw = next;
                                changed = true;
                            }
                        }
                    }
                });
            }
            if !changed {
                break;
            }
        }
    }

    // Exact per-block backward walk computing exact `val_crosses_call`, `val_crossed_clobbers`,
    // and symmetric SSA interference bitmatrix `interf` (`num_vals x val_words`).
    let mut val_crosses_call: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut val_crossed_clobbers: Vec<CallClobbers, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        val_crosses_call.push(false);
        val_crossed_clobbers.push(CallClobbers::EMPTY);
    }
    let mut interf: Vec<u64, A> = Vec::with_capacity_in(num_vals.saturating_mul(val_words), alloc);
    for _ in 0..num_vals.saturating_mul(val_words) {
        interf.push(0);
    }
    let mut live: Vec<u64, A> = Vec::with_capacity_in(val_words, alloc);
    for _ in 0..val_words {
        live.push(0);
    }
    if val_words > 0 {
        for b in 0..blocks {
            for slot in live.iter_mut() {
                *slot = 0;
            }
            let term = view.terminator(Block(u32_of(b)));
            for_each_edge(view, term, |target_block, _, _| {
                let s_base = idx(target_block).saturating_mul(val_words);
                for w in 0..val_words {
                    let sw = live_in.get(s_base + w).copied().unwrap_or(0);
                    if let Some(lw) = live.get_mut(w) {
                        *lw |= sw;
                    }
                }
            });
            let range = view.block(Block(u32_of(b)));
            for inst in range.rev() {
                if skip_alloc.get(inst).copied().unwrap_or(false) {
                    continue;
                }
                let def_vid = val_id.get(inst).copied().unwrap_or(NO_VAL);
                if def_vid != NO_VAL {
                    let d = idx(def_vid);
                    let d_word = d >> 6;
                    let d_bit = 1u64 << (d & 63);
                    if let Some(lw) = live.get_mut(d_word) {
                        *lw &= !d_bit;
                    }
                    let d_base = d.saturating_mul(val_words);
                    for w in 0..val_words {
                        let mut lw = live.get(w).copied().unwrap_or(0);
                        if lw == 0 {
                            continue;
                        }
                        if let Some(slot) = interf.get_mut(d_base + w) {
                            *slot |= lw;
                        }
                        while lw != 0 {
                            let tz = idx(lw.trailing_zeros());
                            let u = w.saturating_mul(64) + tz;
                            if let Some(uslot) = interf.get_mut(u.saturating_mul(val_words) + d_word) {
                                *uslot |= d_bit;
                            }
                            lw &= lw - 1;
                        }
                    }
                    // Non-read-before-write ops cannot reuse a dying operand's register.
                    if view.op(inst) != Op::Param && !is_rbw(view, inst) {
                        for_each_expanded_use(view, skip_alloc, inst, &mut work, |_, vi| {
                            let u_vid = val_id.get(vi).copied().unwrap_or(NO_VAL);
                            if u_vid != NO_VAL && u_vid != def_vid {
                                let u = idx(u_vid);
                                if let Some(slot) = interf.get_mut(d_base + (u >> 6)) {
                                    *slot |= 1u64 << (u & 63);
                                }
                                if let Some(uslot) = interf.get_mut(u.saturating_mul(val_words) + d_word) {
                                    *uslot |= d_bit;
                                }
                            }
                        });
                    }
                }
                // Only values genuinely live AFTER this clobbering call in the CFG cross it!
                if is_clobbering_call.get(inst).copied().unwrap_or(false) {
                    let clob = if matches!(view.op(inst), Op::Invoke | Op::InvokeIndirect) {
                        CallClobbers::C_ABI_ARM64
                    } else {
                        call_clobbers(inst)
                    };
                    for w in 0..val_words {
                        let mut lw = live.get(w).copied().unwrap_or(0);
                        while lw != 0 {
                            let tz = idx(lw.trailing_zeros());
                            let u = w.saturating_mul(64) + tz;
                            if let Some(&u_inst) = val_inst.get(u) {
                                if let Some(cc) = val_crosses_call.get_mut(u_inst) {
                                    *cc = true;
                                }
                                if let Some(cb) = val_crossed_clobbers.get_mut(u_inst) {
                                    cb.clobbered = cb.clobbered.union(clob.clobbered);
                                }
                            }
                            lw &= lw - 1;
                        }
                    }
                }
                for_each_expanded_use(view, skip_alloc, inst, &mut work, |_, vi| {
                    let u_vid = val_id.get(vi).copied().unwrap_or(NO_VAL);
                    if u_vid != NO_VAL {
                        let u = idx(u_vid);
                        if let Some(lw) = live.get_mut(u >> 6) {
                            *lw |= 1u64 << (u & 63);
                        }
                    }
                });
            }
            // All live `Op::Param`s of the same block are defined simultaneously on block entry.
            let params = view.params(Block(u32_of(b)));
            for p1 in params.start..params.end {
                let v1 = val_id.get(p1).copied().unwrap_or(NO_VAL);
                if v1 == NO_VAL {
                    continue;
                }
                let u1 = idx(v1);
                for p2 in (p1 + 1)..params.end {
                    let v2 = val_id.get(p2).copied().unwrap_or(NO_VAL);
                    if v2 == NO_VAL {
                        continue;
                    }
                    let u2 = idx(v2);
                    if let Some(slot) = interf.get_mut(u1.saturating_mul(val_words) + (u2 >> 6)) {
                        *slot |= 1u64 << (u2 & 63);
                    }
                    if let Some(slot) = interf.get_mut(u2.saturating_mul(val_words) + (u1 >> 6)) {
                        *slot |= 1u64 << (u1 & 63);
                    }
                }
            }
        }
    }

    // 1c. Pre-rematerialize cheap call-crossing 1-inst constants and cold-only `DataAddr`/`FuncAddr`
    //     so they NEVER burn callee-saved GPRs (`x19..x28`) or execute dead `adrp + add` in hot prologues!
    let mut pre_remat: Vec<AllocLoc, A> = Vec::with_capacity_in(len, alloc);
    let mut pre_remat_count = 0u32;
    for inst in 0..len {
        if skip_alloc.get(inst).copied().unwrap_or(false)
            || !has_any_use.get(inst).copied().unwrap_or(false)
        {
            pre_remat.push(AllocLoc::None);
            continue;
        }
        if let Some(cval) = remat_val.get(inst).copied().flatten() {
            let one_inst = const_mat_cost(cval) == 1;
            let crosses = val_crosses_call.get(inst).copied().unwrap_or(false);
            let depth = max_use_depth.get(inst).copied().unwrap_or(0);
            if crosses && one_inst && (depth == 0 || cval == 0) {
                pre_remat.push(AllocLoc::RematConst(cval));
                pre_remat_count += 1;
                continue;
            }
        }
        if matches!(view.op(inst), Op::DataAddr | Op::FuncAddr)
            && !has_hot_use.get(inst).copied().unwrap_or(true)
        {
            pre_remat.push(AllocLoc::RematGlobal(u32_of(inst)));
            pre_remat_count += 1;
            continue;
        }
        pre_remat.push(AllocLoc::None);
    }

    let vals_interfere = |u: usize, v: usize| -> bool {
        if u == v || val_words == 0 {
            return false;
        }
        let (Some(&uid), Some(&vid)) = (val_id.get(u), val_id.get(v)) else {
            return false;
        };
        if uid == NO_VAL || vid == NO_VAL {
            return false;
        }
        let ui = idx(uid);
        let vi = idx(vid);
        interf
            .get(ui.saturating_mul(val_words) + (vi >> 6))
            .is_some_and(|&w| (w & (1u64 << (vi & 63))) != 0)
    };

    // 2. Coalescing hints: entry params (incl. `sret` in X8), return value (`X0`/`V0`),
    //    block-param (`phi`) back-edges & non-interfering forward edges, and in-place 2-address
    //    accumulators (`fmla`/`mla`/`ins`/`add`/`sub`).
    let mut hint: Vec<u8, A> = Vec::with_capacity_in(len, alloc);
    let mut phi_partner: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut phi_prio: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut is_back_phi_arg: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut back_partner_of: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut back_partner_prio: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        hint.push(NO_REG);
        phi_partner.push(u32::MAX);
        phi_prio.push(0);
        is_back_phi_arg.push(false);
        back_partner_of.push(u32::MAX);
        back_partner_prio.push(0);
    }

    seed_abi_and_return_hints(view, alias_of, os, &mut hint);

    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        if view.op(term) == Op::Return {
            continue;
        }
        let b_depth = block_depth.get(b).copied().unwrap_or(0);
        let b_rpo = rpo_idx.get(b).copied().unwrap_or(0);
        for_each_edge(view, term, |target_block, args_start, args_len| {
            let target_params = view.params(Block(target_block));
            let n = args_len.min(target_params.len());
            let is_back = rpo_idx.get(idx(target_block)).copied().unwrap_or(blocks) <= b_rpo;
            let edge_prio = if is_back { b_depth.saturating_add(2) } else { 1 };
            if let Some(args) = view.extra.get(args_start..args_start + n) {
                for (k, &arg_raw) in args.iter().enumerate() {
                    let p = target_params.start + k;
                    let arg = resolve_alias(alias_of, idx(arg_raw));
                    if arg >= len
                        || p >= len
                        || arg == p
                        || pre_remat.get(arg).copied().unwrap_or(AllocLoc::None) != AllocLoc::None
                        || pre_remat.get(p).copied().unwrap_or(AllocLoc::None) != AllocLoc::None
                    {
                        continue;
                    }
                    let p_ty = view.tys.get(p).copied().map_or(Ty::NONE, Ty);
                    let arg_ty = view.tys.get(arg).copied().map_or(Ty::NONE, Ty);
                    let bank = reg_bank(view, arg_ty);
                    if bank == 0 || bank != reg_bank(view, p_ty) {
                        continue;
                    }
                    if vals_interfere(arg, p) {
                        continue;
                    }
                    if arg > p {
                        let cur = phi_partner.get(arg).copied().unwrap_or(u32::MAX);
                        let cur_prio = phi_prio.get(arg).copied().unwrap_or(0);
                        if cur == u32::MAX || edge_prio > cur_prio {
                            if let Some(slot) = phi_partner.get_mut(arg) {
                                *slot = u32_of(p);
                            }
                            if let Some(pslot) = phi_prio.get_mut(arg) {
                                *pslot = edge_prio;
                            }
                            if is_back
                                && !skip_alloc.get(arg).copied().unwrap_or(false)
                                && remat_val.get(arg).copied().flatten().is_none()
                                && !matches!(view.op(arg), Op::DataAddr | Op::FuncAddr)
                            {
                                if let Some(bslot) = is_back_phi_arg.get_mut(arg) {
                                    *bslot = true;
                                }
                                let cur_bp = back_partner_of.get(p).copied().unwrap_or(u32::MAX);
                                let cur_bp_prio = back_partner_prio.get(p).copied().unwrap_or(0);
                                if cur_bp == u32::MAX || edge_prio > cur_bp_prio {
                                    if let Some(bpslot) = back_partner_of.get_mut(p) {
                                        *bpslot = u32_of(arg);
                                    }
                                    if let Some(bppslot) = back_partner_prio.get_mut(p) {
                                        *bppslot = edge_prio;
                                    }
                                }
                            }
                        }
                    } else if remat_val.get(arg).copied().flatten().is_none()
                        && (!val_crosses_call.get(p).copied().unwrap_or(false)
                            || val_crosses_call.get(arg).copied().unwrap_or(false))
                    {
                        let cur = phi_partner.get(p).copied().unwrap_or(u32::MAX);
                        let cur_prio = phi_prio.get(p).copied().unwrap_or(0);
                        if cur == u32::MAX || edge_prio > cur_prio {
                            if let Some(slot) = phi_partner.get_mut(p) {
                                *slot = u32_of(arg);
                            }
                            if let Some(pslot) = phi_prio.get_mut(p) {
                                *pslot = edge_prio;
                            }
                        }
                    }
                }
            }
        });
    }

    // Propagate loop back-edge `phi_partner` backward through in-place accumulator chains.
    for inst in (0..len).rev() {
        if skip_alloc.get(inst).copied().unwrap_or(false)
            || pre_remat.get(inst).copied().unwrap_or(AllocLoc::None) != AllocLoc::None
        {
            continue;
        }
        let partner = phi_partner.get(inst).copied().unwrap_or(u32::MAX);
        if partner == u32::MAX {
            continue;
        }
        let pi = idx(partner);
        if let Some(acc) =
            preferred_inplace_operand(view, skip_alloc, alias_of, |vi| vi < inst, |ci| !vals_interfere(inst, ci), inst)
        {
            if acc > pi
                && pre_remat.get(acc).copied().unwrap_or(AllocLoc::None) == AllocLoc::None
                && !vals_interfere(inst, acc)
                && phi_partner.get(acc).copied() == Some(u32::MAX)
            {
                if let Some(slot) = phi_partner.get_mut(acc) {
                    *slot = partner;
                }
            }
        }
    }

    // Coalesce destructive 2-address accumulators & zero-copy ops into non-interfering source operands.
    for inst in 0..len {
        if skip_alloc.get(inst).copied().unwrap_or(false)
            || pre_remat.get(inst).copied().unwrap_or(AllocLoc::None) != AllocLoc::None
        {
            continue;
        }
        if phi_partner.get(inst).copied() != Some(u32::MAX) || !is_rbw(view, inst) {
            continue;
        }
        if let Some(src) =
            preferred_inplace_operand(view, skip_alloc, alias_of, |vi| vi < inst, |ci| !vals_interfere(inst, ci), inst)
        {
            if pre_remat.get(src).copied().unwrap_or(AllocLoc::None) == AllocLoc::None
                && remat_val.get(src).copied().flatten().is_none()
                && !matches!(view.op(src), Op::DataAddr | Op::FuncAddr)
                && !vals_interfere(inst, src)
            {
                if let Some(slot) = phi_partner.get_mut(inst) {
                    *slot = u32_of(src);
                }
            }
        }
    }

    propagate_extract_lane0_hints(view, &mut hint, (0..len).rev());

    // Boost loop back-edge phi arguments by +1 before backward `phi_partner` propagation so
    // Phase 2 colors `loop_param` -> `back_phi_arg` (claiming `loop_param`'s reg) -> same-depth temp.
    for (arg, &is_back_arg) in is_back_phi_arg.iter().enumerate() {
        if is_back_arg {
            if let Some(w) = use_weight.get_mut(arg) {
                *w = w.saturating_add(1);
            }
        }
    }

    // Propagate priority weights and return-value hints backward along `phi_partner` chains.
    for arg in (0..len).rev() {
        let p = phi_partner.get(arg).copied().unwrap_or(u32::MAX);
        if p != u32::MAX {
            let pi = idx(p);
            if remat_val.get(pi).copied().flatten().is_none()
                && !matches!(view.op(pi), Op::DataAddr | Op::FuncAddr)
            {
                let aw = use_weight.get(arg).copied().unwrap_or(0).saturating_add(1);
                if let Some(pw) = use_weight.get_mut(pi) {
                    if aw > *pw {
                        *pw = aw;
                    }
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
        vid: usize,
        weight: u64,
        is_remat: bool,
    }
    let mut prog_order: Vec<Candidate, A> = Vec::with_capacity_in(len, alloc);
    for inst in 0..len {
        if skip_alloc.get(inst).copied().unwrap_or(false)
            || pre_remat.get(inst).copied().unwrap_or(AllocLoc::None) != AllocLoc::None
        {
            continue;
        }
        let vid_raw = val_id.get(inst).copied().unwrap_or(NO_VAL);
        if vid_raw == NO_VAL {
            continue;
        }
        let cval = remat_val.get(inst).copied().flatten();
        let uw = use_weight.get(inst).copied().unwrap_or(0);
        let dw = def_weight.get(inst).copied().unwrap_or(0);
        let in_loop = max_use_depth.get(inst).copied().unwrap_or(0) > 0;
        let (weight, is_remat) = candidate_priority(view.op(inst), cval, uw, dw, in_loop);
        prog_order.push(Candidate { inst, vid: idx(vid_raw), weight, is_remat });
    }

    let allow_lr = call_positions.is_empty();
    let callee_gpr_pool = if allow_lr { CALLEE_SAVED_GPR_WITH_LR } else { CALLEE_SAVED_GPR };

    let run_coloring = |order: &[Candidate]| -> Result<(Vec<AllocLoc, A>, [bool; 32], [bool; 32], u32, u32, u64), RegAllocError> {
        let mut loc_try: Vec<AllocLoc, A> = Vec::with_capacity_in(len, alloc);
        for &loc in &pre_remat {
            loc_try.push(loc);
        }
        let busy_len = 32usize.saturating_mul(val_words);
        let mut gpr_busy: Vec<u64, A> = Vec::with_capacity_in(busy_len, alloc);
        let mut fp_busy: Vec<u64, A> = Vec::with_capacity_in(busy_len, alloc);
        for _ in 0..busy_len {
            gpr_busy.push(0);
            fp_busy.push(0);
        }
        let is_reg_free = |busy: &[u64], reg: usize, vid: usize| -> bool {
            let r_base = reg.saturating_mul(val_words);
            let v_base = vid.saturating_mul(val_words);
            // A register or spill slot already claimed by `vid` itself (e.g. the low half of a
            // `GprPairSpill`) is never free for `vid`'s second half, even though `interf[vid][vid]`
            // is zero.
            let self_bit = 1u64 << (vid & 63);
            if (busy.get(r_base + (vid >> 6)).copied().unwrap_or(0) & self_bit) != 0 {
                return false;
            }
            for w in 0..val_words {
                let bw = busy.get(r_base + w).copied().unwrap_or(0);
                let iw = interf.get(v_base + w).copied().unwrap_or(0);
                if (bw & iw) != 0 {
                    return false;
                }
            }
            true
        };
        let mark_reg_busy = |busy: &mut [u64], reg: usize, vid: usize| {
            let off = reg.saturating_mul(val_words) + (vid >> 6);
            if let Some(slot) = busy.get_mut(off) {
                *slot |= 1u64 << (vid & 63);
            }
        };
        let mut spill_busy: Vec<u64, A> = Vec::new_in(alloc);
        let mut used_callee_gpr = [false; 32];
        let mut used_callee_fp = [false; 32];
        let mut num_spill_slots = 0u32;
        let mut remat_count = 0u32;
        let mut spill_cost = 0u64;

        let mut alloc_spill_n = |slots: usize, vid: usize| -> Result<u32, RegAllocError> {
            let slots_u32 = u32_of(slots);
            let existing = if val_words == 0 { 0 } else { spill_busy.len() / val_words };
            if existing >= slots {
                for slot_idx in 0..=(existing - slots) {
                    if (0..slots).all(|k| is_reg_free(&spill_busy, slot_idx + k, vid)) {
                        for k in 0..slots {
                            mark_reg_busy(&mut spill_busy, slot_idx + k, vid);
                        }
                        return Ok(u32_of(slot_idx));
                    }
                }
            }
            if num_spill_slots.saturating_add(slots_u32) > max_spill_slots {
                return Err(RegAllocError::OutOfSpillSlots);
            }
            let slot = num_spill_slots;
            num_spill_slots += slots_u32;
            for _ in 0..slots.saturating_mul(val_words) {
                spill_busy.push(0);
            }
            let base_slot = idx(slot);
            for k in 0..slots {
                mark_reg_busy(&mut spill_busy, base_slot + k, vid);
            }
            Ok(slot)
        };

        for cand in order {
            let inst = cand.inst;
            let vid = cand.vid;
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

            let crosses_call = val_crosses_call.get(inst).copied().unwrap_or(false);
            let crossed_clobbers = val_crossed_clobbers
                .get(inst)
                .copied()
                .unwrap_or(CallClobbers::EMPTY);
            let gpr_safe = |r: Reg| -> bool {
                !crosses_call || (X19..=X28).contains(&r) || !crossed_clobbers.clobbers_gpr(reg_u8(r))
            };
            let fp_safe = |r: Reg| -> bool {
                !crosses_call || (8..=15).contains(&r) || !crossed_clobbers.clobbers_fp(reg_u8(r))
            };

            if is_gpr_pair_ty(view, ty) {
                let pool_pick = |busy: &[u64], skip: Option<Reg>| -> Option<Reg> {
                    caller_gpr_order
                        .iter()
                        .copied()
                        .chain(callee_gpr_pool.iter().copied())
                        .find(|&r| Some(r) != skip && gpr_safe(r) && is_reg_free(busy, idx(r), vid))
                };
                let r0 = pool_pick(&gpr_busy, None);
                let r1 = r0.and_then(|first| pool_pick(&gpr_busy, Some(first)));
                if let (Some(lo), Some(hi)) = (r0, r1) {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::GprPair(reg_u8(lo), reg_u8(hi));
                    }
                    mark_reg_busy(&mut gpr_busy, idx(lo), vid);
                    mark_reg_busy(&mut gpr_busy, idx(hi), vid);
                    if (X19..=X28).contains(&lo) || lo == X30 {
                        if let Some(u) = used_callee_gpr.get_mut(idx(lo)) {
                            *u = true;
                        }
                    }
                    if (X19..=X28).contains(&hi) || hi == X30 {
                        if let Some(u) = used_callee_gpr.get_mut(idx(hi)) {
                            *u = true;
                        }
                    }
                } else {
                    let s0 = alloc_spill_n(1, vid)?;
                    let s1 = alloc_spill_n(1, vid)?;
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::GprPairSpill(s0, s1);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight.saturating_mul(2));
                }
            } else if is_vec128_ty(ty).is_some() || is_vec_mask_ty(ty).is_some() {
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
                            && is_reg_free(&fp_busy, usize::from(h), vid)
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
                                        && is_reg_free(&fp_busy, idx(r), vid)
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
                            .find(|&r| vec_safe(r) && is_reg_free(&fp_busy, idx(r), vid))
                    });
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Vec(reg_u8(reg));
                    }
                    mark_reg_busy(&mut fp_busy, idx(reg), vid);
                    if (8..=15).contains(&reg) {
                        if let Some(u) = used_callee_fp.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else {
                    let slot = alloc_spill_n(2, vid)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::VecSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight.saturating_mul(2));
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
                    if h != NO_REG && fp_safe(u32::from(h)) && is_reg_free(&fp_busy, usize::from(h), vid) {
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
                            .find(|&r| fp_safe(r) && is_reg_free(&fp_busy, idx(r), vid))
                    });
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Fp(reg_u8(reg));
                    }
                    mark_reg_busy(&mut fp_busy, idx(reg), vid);
                    if (8..=15).contains(&reg) {
                        if let Some(u) = used_callee_fp.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else {
                    let slot = alloc_spill_n(1, vid)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::FpSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight);
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
                let bp_raw = back_partner_of.get(inst).copied().unwrap_or(u32::MAX);
                let bp_info = if bp_raw != u32::MAX {
                    let bpi = idx(bp_raw);
                    let bp_uncolored = loc_try.get(bpi).copied().unwrap_or(AllocLoc::None) == AllocLoc::None;
                    let bp_vid_raw = val_id.get(bpi).copied().unwrap_or(NO_VAL);
                    if bp_uncolored && bp_vid_raw != NO_VAL {
                        let bp_crosses = val_crosses_call.get(bpi).copied().unwrap_or(false);
                        let bp_clobbers = val_crossed_clobbers
                            .get(bpi)
                            .copied()
                            .unwrap_or(CallClobbers::EMPTY);
                        Some((idx(bp_vid_raw), bp_crosses, bp_clobbers))
                    } else {
                        None
                    }
                } else {
                    None
                };
                let try_gpr = |h: u8| -> Option<Reg> {
                    if h != NO_REG
                        && (u32::from(h) != X30 || allow_lr)
                        && gpr_safe(u32::from(h))
                        && is_reg_free(&gpr_busy, usize::from(h), vid)
                    {
                        Some(u32::from(h))
                    } else {
                        None
                    }
                };
                let joint_chosen = if let Some((bp_vid, bp_crosses, bp_clobbers)) = bp_info {
                    let bp_safe = |r: Reg| -> bool {
                        !bp_crosses || (X19..=X28).contains(&r) || !bp_clobbers.clobbers_gpr(reg_u8(r))
                    };
                    let try_gpr_joint = |h: u8| -> Option<Reg> {
                        let r = try_gpr(h)?;
                        (bp_safe(r) && is_reg_free(&gpr_busy, idx(r), bp_vid)).then_some(r)
                    };
                    try_gpr_joint(primary_hint)
                        .or_else(|| try_gpr_joint(secondary_hint))
                        .or_else(|| try_gpr_joint(pair_trunc_hint))
                        .or_else(|| {
                            caller_gpr_order
                                .iter()
                                .copied()
                                .chain(callee_gpr_pool.iter().copied())
                                .find(|&r| {
                                    gpr_safe(r)
                                        && bp_safe(r)
                                        && is_reg_free(&gpr_busy, idx(r), vid)
                                        && is_reg_free(&gpr_busy, idx(r), bp_vid)
                                })
                        })
                } else {
                    None
                };
                let chosen = joint_chosen
                    .or_else(|| try_gpr(primary_hint))
                    .or_else(|| try_gpr(secondary_hint))
                    .or_else(|| try_gpr(pair_trunc_hint))
                    .or_else(|| {
                        caller_gpr_order
                            .iter()
                            .copied()
                            .chain(callee_gpr_pool.iter().copied())
                            .find(|&r| gpr_safe(r) && is_reg_free(&gpr_busy, idx(r), vid))
                    });
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Gpr(reg_u8(reg));
                    }
                    mark_reg_busy(&mut gpr_busy, idx(reg), vid);
                    if (X19..=X28).contains(&reg) || reg == X30 {
                        if let Some(u) = used_callee_gpr.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else if let Some(cval) = remat_val.get(inst).copied().flatten() {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::RematConst(cval);
                    }
                    remat_count += 1;
                    spill_cost = spill_cost.saturating_add(cand.weight);
                } else if matches!(view.op(inst), Op::DataAddr | Op::FuncAddr) {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::RematGlobal(u32_of(inst));
                    }
                    remat_count += 1;
                    spill_cost = spill_cost.saturating_add(cand.weight);
                } else {
                    let slot = alloc_spill_n(1, vid)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::GprSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight);
                }
            }
        }
        Ok((loc_try, used_callee_gpr, used_callee_fp, num_spill_slots, remat_count, spill_cost))
    };

    // Phase 1: exact chordal-SSA program-order coloring (provably minimal register count!).
    let (mut final_loc, mut used_callee_gpr, mut used_callee_fp, mut num_spill_slots, mut remat_count, spill_cost) =
        run_coloring(&prog_order)?;

    // Phase 2: if register pressure overflowed (spills > 0 or remat > 0), run loop-weighted
    // Greedy Priority Coloring so hot inner loops stay register-resident while cold outer
    // values/constants yield & rematerialize.
    if num_spill_slots > 0 || remat_count > 0 {
        let mut greedy_order: Vec<Candidate, A> = Vec::with_capacity_in(prog_order.len(), alloc);
        for &c in &prog_order {
            greedy_order.push(c);
        }
        greedy_order.sort_unstable_by(|a, b| {
            a.is_remat
                .cmp(&b.is_remat)
                .then_with(|| b.weight.cmp(&a.weight))
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
    remat_count = remat_count.saturating_add(pre_remat_count);

    Ok(finalize_allocation(
        view,
        final_loc,
        used_callee_gpr,
        used_callee_fp,
        num_spill_slots,
        remat_count,
        &call_positions,
        call_clobbers,
    ))
}

/// O(N log K) RPO-ordered linear-scan register allocator for mega-functions exceeding the
/// exact-CFG bitmatrix budget (`num_vals > EXACT_CFG_MAX_VALS || blocks > EXACT_CFG_MAX_BLOCKS`).
///
/// All intervals `[start, end]`, back-edges, and call positions live in a single consistent
/// Reverse Post-Order (RPO) instruction coordinate space so SSA dominance (`rpo_pos[def] < rpo_pos[use]`)
/// and call-crossing across loops hold even when LIR blocks are not in RPO order.
#[allow(clippy::too_many_arguments)]
fn allocate_linear_scan_rpo<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    alias_of: &[u32],
    tail_calls: &[bool],
    max_spill_slots: u32,
    os: crate::Os,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    caller_gpr_order: &[Reg; 16],
    caller_fp_order: &[Reg; 21],
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    let len = view.len();
    let blocks = view.blocks();

    // 0a. Compute iterative-DFS Reverse Post-Order (RPO) and deduplicated per-block loop depth
    //     in O(B + E) time with zero recursion.
    let (rpo_blocks, rpo_idx, block_depth) = compute_rpo_and_loop_depth(view, alloc);

    let mut rpo_block_start: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    let mut rpo_block_end: Vec<usize, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        rpo_block_start.push(0);
        rpo_block_end.push(0);
    }
    let mut rpo_pos: Vec<usize, A> = Vec::with_capacity_in(len, alloc);
    let mut rpo_to_inst: Vec<usize, A> = Vec::with_capacity_in(len, alloc);
    let mut inst_block: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        rpo_pos.push(0);
        inst_block.push(0);
    }
    for &b in &rpo_blocks {
        let b_start = rpo_to_inst.len();
        let range = view.block(Block(u32_of(b)));
        for inst in range {
            let p = rpo_to_inst.len();
            rpo_to_inst.push(inst);
            if let Some(slot) = rpo_pos.get_mut(inst) {
                *slot = p;
            }
            if let Some(slot) = inst_block.get_mut(inst) {
                *slot = u32_of(b);
            }
        }
        let b_end = rpo_to_inst.len();
        if let Some(slot) = rpo_block_start.get_mut(b) {
            *slot = b_start;
        }
        if let Some(slot) = rpo_block_end.get_mut(b) {
            *slot = b_end;
        }
    }

    // 1. Compute RPO live intervals `[rpo_pos[def], last_use_pos[def]]`, sorted RPO call sites,
    //    and loop-weighted spill priorities.
    let mut last_use_pos: Vec<usize, A> = Vec::with_capacity_in(len, alloc);
    let mut has_any_use: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut use_weight: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut def_weight: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut max_use_depth: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut remat_val: Vec<Option<u64>, A> = Vec::with_capacity_in(len, alloc);
    let mut call_rpo_positions: Vec<(usize, usize), A> = Vec::new_in(alloc);
    let mut call_positions: Vec<usize, A> = Vec::new_in(alloc);
    for i in 0..len {
        last_use_pos.push(rpo_pos.get(i).copied().unwrap_or(0));
        has_any_use.push(false);
        use_weight.push(0);
        def_weight.push(0);
        max_use_depth.push(0);
        remat_val.push(extract_remat_const(view, i));
    }

    for &b in &rpo_blocks {
        let raw_depth = block_depth.get(b).copied().unwrap_or(0);
        let depth = raw_depth.min(3);
        let b_weight = 1u32 << depth.saturating_mul(7); // 1x outer, 128x depth-1, 16384x depth-2, 2097152x depth-3+
        let range = view.block(Block(u32_of(b)));
        for inst in range {
            let inst_pos = rpo_pos.get(inst).copied().unwrap_or(0);
            if !skip_alloc.get(inst).copied().unwrap_or(false)
                && is_clobbering_call_inst(view, tail_calls, inst)
            {
                call_rpo_positions.push((inst_pos, inst));
                call_positions.push(inst);
            }
            if let Some(dw) = def_weight.get_mut(inst) {
                *dw = b_weight;
            }
            if remat_val.get(inst).copied().flatten().is_none() {
                if let Some(w) = use_weight.get_mut(inst) {
                    *w = w.saturating_add(b_weight);
                }
            }
            for_each_use(view, inst, |v| {
                let vi = idx(v);
                if vi < len {
                    if let Some(u) = has_any_use.get_mut(vi) {
                        *u = true;
                    }
                    let imm_folded = remat_val
                        .get(vi)
                        .copied()
                        .flatten()
                        .is_some_and(|cval| is_folded_imm_use(view, inst, vi, cval));
                    if !imm_folded {
                        if let Some(w) = use_weight.get_mut(vi) {
                            *w = w.saturating_add(b_weight);
                        }
                        if let Some(md) = max_use_depth.get_mut(vi) {
                            if raw_depth > *md {
                                *md = raw_depth;
                            }
                        }
                    }
                    if let Some(lu) = last_use_pos.get_mut(vi) {
                        if inst_pos > *lu {
                            *lu = inst_pos;
                        }
                    }
                }
            });
        }
    }

    // Propagate fused producers' operand lifetimes and loop weights in reverse RPO order.
    for &inst in rpo_to_inst.iter().rev() {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            let consumer_end = last_use_pos.get(inst).copied().unwrap_or(0);
            let consumer_w = use_weight.get(inst).copied().unwrap_or(0);
            let consumer_d = max_use_depth.get(inst).copied().unwrap_or(0);
            for_each_use(view, inst, |v| {
                let vi = idx(v);
                if let Some(lu) = last_use_pos.get_mut(vi) {
                    if consumer_end > *lu {
                        *lu = consumer_end;
                    }
                }
                let imm_folded = remat_val
                    .get(vi)
                    .copied()
                    .flatten()
                    .is_some_and(|cval| is_folded_imm_use(view, inst, vi, cval));
                if !imm_folded {
                    if let Some(w) = use_weight.get_mut(vi) {
                        if consumer_w > *w {
                            *w = consumer_w;
                        }
                    }
                    if let Some(md) = max_use_depth.get_mut(vi) {
                        if consumer_d > *md {
                            *md = consumer_d;
                        }
                    }
                }
            });
        }
    }

    // Extend RPO live intervals across backward RPO edges (loops & backward unwind edges) to a
    // fixed point. Deduplicate by `target_start` (keeping max `ext_end`), and only scan values
    // whose live interval crosses a block boundary.
    let mut back_edges: Vec<(usize, usize), A> = Vec::new_in(alloc);
    for &b in &rpo_blocks {
        let term = view.terminator(Block(u32_of(b)));
        let term_pos = rpo_pos.get(term).copied().unwrap_or(0);
        let term_is_invoke = matches!(view.op(term), Op::Invoke | Op::InvokeIndirect);
        for_each_edge(view, term, |target_block, _, _| {
            let target_start = rpo_block_start.get(idx(target_block)).copied().unwrap_or(usize::MAX);
            if target_start <= term_pos {
                let ext_end = if term_is_invoke { term_pos.saturating_add(1) } else { term_pos };
                back_edges.push((target_start, ext_end));
            }
        });
    }
    if !back_edges.is_empty() {
        back_edges.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        let mut unique_edges: Vec<(usize, usize), A> = Vec::with_capacity_in(back_edges.len(), alloc);
        for &(ts, ee) in &back_edges {
            if unique_edges.last().is_none_or(|&(prev_ts, _)| prev_ts != ts) {
                unique_edges.push((ts, ee));
            }
        }
        let mut cross_vals: Vec<(usize, usize), A> = Vec::new_in(alloc);
        let mut skip_list: Vec<usize, A> = Vec::new_in(alloc);
        for (pos, &inst) in rpo_to_inst.iter().enumerate() {
            if skip_alloc.get(inst).copied().unwrap_or(false) {
                skip_list.push(inst);
            }
            if has_any_use.get(inst).copied().unwrap_or(false) {
                let b = idx(inst_block.get(inst).copied().unwrap_or(0));
                let b_end = rpo_block_end.get(b).copied().unwrap_or(0);
                let lu = last_use_pos.get(inst).copied().unwrap_or(0);
                if lu >= b_end {
                    cross_vals.push((pos, inst));
                }
            }
        }
        loop {
            let mut changed = false;
            for &(target_start, ext_end) in &unique_edges {
                let hi_idx = cross_vals.partition_point(|&(v_pos, _)| v_pos < target_start);
                if let Some(slice) = cross_vals.get(..hi_idx) {
                    for &(_, v) in slice {
                        if let Some(lu) = last_use_pos.get_mut(v) {
                            if *lu >= target_start && *lu < ext_end {
                                *lu = ext_end;
                                changed = true;
                            }
                        }
                    }
                }
            }
            if !changed {
                break;
            }
            for &inst in skip_list.iter().rev() {
                let consumer_end = last_use_pos.get(inst).copied().unwrap_or(0);
                for_each_use(view, inst, |v| {
                    let vi = idx(v);
                    if let Some(lu) = last_use_pos.get_mut(vi) {
                        if consumer_end > *lu {
                            *lu = consumer_end;
                        }
                    }
                });
            }
        }
    }

    // 2. Coalescing hints in RPO order: entry params, return values, block-param back-edges,
    //    and in-place 2-address accumulators.
    let mut hint: Vec<u8, A> = Vec::with_capacity_in(len, alloc);
    let mut phi_partner: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut phi_prio: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut is_back_phi_arg: Vec<bool, A> = Vec::with_capacity_in(len, alloc);
    let mut back_partner_of: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    let mut back_partner_prio: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        hint.push(NO_REG);
        phi_partner.push(u32::MAX);
        phi_prio.push(0);
        is_back_phi_arg.push(false);
        back_partner_of.push(u32::MAX);
        back_partner_prio.push(0);
    }

    seed_abi_and_return_hints(view, alias_of, os, &mut hint);

    #[inline]
    fn intervals_conflict(s: usize, e: usize, s_rbw: bool, start: usize, end: usize, cur_rbw: bool) -> bool {
        (start < e && s < end)
            || start == s
            || (start == e && !cur_rbw)
            || (end == s && !s_rbw)
    }

    for (rank, &b) in rpo_blocks.iter().enumerate() {
        let term = view.terminator(Block(u32_of(b)));
        if view.op(term) == Op::Return {
            continue;
        }
        let b_depth = block_depth.get(b).copied().unwrap_or(0);
        for_each_edge(view, term, |target_block, args_start, args_len| {
            let target_params = view.params(Block(target_block));
            let n = args_len.min(target_params.len());
            let is_back = rpo_idx.get(idx(target_block)).copied().unwrap_or(blocks) <= rank;
            let edge_prio = if is_back { b_depth.saturating_add(1) } else { 0 };
            if let Some(args) = view.extra.get(args_start..args_start + n) {
                for (k, &arg_raw) in args.iter().enumerate() {
                    let p = target_params.start + k;
                    let arg = resolve_alias(alias_of, idx(arg_raw));
                    if arg < len
                        && p < len
                        && rpo_pos.get(arg).copied().unwrap_or(0) > rpo_pos.get(p).copied().unwrap_or(usize::MAX)
                    {
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
                                if is_back
                                    && !skip_alloc.get(arg).copied().unwrap_or(false)
                                    && remat_val.get(arg).copied().flatten().is_none()
                                    && !matches!(view.op(arg), Op::DataAddr | Op::FuncAddr)
                                {
                                    if let Some(bslot) = is_back_phi_arg.get_mut(arg) {
                                        *bslot = true;
                                    }
                                    let p_start = rpo_pos.get(p).copied().unwrap_or(0);
                                    let p_end = last_use_pos.get(p).copied().unwrap_or(p_start).max(p_start);
                                    let arg_start = rpo_pos.get(arg).copied().unwrap_or(0);
                                    let arg_end = last_use_pos.get(arg).copied().unwrap_or(arg_start).max(arg_start);
                                    if !intervals_conflict(
                                        p_start,
                                        p_end,
                                        is_rbw(view, p),
                                        arg_start,
                                        arg_end,
                                        is_rbw(view, arg),
                                    ) {
                                        let cur_bp = back_partner_of.get(p).copied().unwrap_or(u32::MAX);
                                        let cur_bp_prio = back_partner_prio.get(p).copied().unwrap_or(0);
                                        if cur_bp == u32::MAX || edge_prio > cur_bp_prio {
                                            if let Some(bpslot) = back_partner_of.get_mut(p) {
                                                *bpslot = u32_of(arg);
                                            }
                                            if let Some(bppslot) = back_partner_prio.get_mut(p) {
                                                *bppslot = edge_prio;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Propagate loop back-edge `phi_partner` backward through in-place accumulator chains (reverse RPO).
    for &inst in rpo_to_inst.iter().rev() {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            continue;
        }
        let partner = phi_partner.get(inst).copied().unwrap_or(u32::MAX);
        if partner == u32::MAX {
            continue;
        }
        let pi = idx(partner);
        let inst_pos = rpo_pos.get(inst).copied().unwrap_or(0);
        let pi_pos = rpo_pos.get(pi).copied().unwrap_or(usize::MAX);
        if let Some(acc) = preferred_inplace_operand(
            view,
            skip_alloc,
            alias_of,
            |vi| rpo_pos.get(vi).copied().unwrap_or(usize::MAX) < inst_pos,
            |ci| last_use_pos.get(ci).copied() == Some(inst_pos),
            inst,
        ) {
            if rpo_pos.get(acc).copied().unwrap_or(0) > pi_pos
                && last_use_pos.get(acc).copied() == Some(inst_pos)
                && phi_partner.get(acc).copied() == Some(u32::MAX)
            {
                if let Some(slot) = phi_partner.get_mut(acc) {
                    *slot = partner;
                }
            }
        }
    }

    // Coalesce destructive 2-address accumulators & zero-copy ops into dying source operands (RPO).
    for &inst in &rpo_to_inst {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            continue;
        }
        if phi_partner.get(inst).copied() != Some(u32::MAX) || !is_rbw(view, inst) {
            continue;
        }
        let inst_pos = rpo_pos.get(inst).copied().unwrap_or(0);
        if let Some(src) = preferred_inplace_operand(
            view,
            skip_alloc,
            alias_of,
            |vi| rpo_pos.get(vi).copied().unwrap_or(usize::MAX) < inst_pos,
            |ci| last_use_pos.get(ci).copied() == Some(inst_pos),
            inst,
        ) {
            if last_use_pos.get(src).copied() == Some(inst_pos)
                && remat_val.get(src).copied().flatten().is_none()
                && !matches!(view.op(src), Op::DataAddr | Op::FuncAddr)
            {
                if let Some(slot) = phi_partner.get_mut(inst) {
                    *slot = u32_of(src);
                }
            }
        }
    }

    propagate_extract_lane0_hints(view, &mut hint, rpo_to_inst.iter().copied().rev());

    // Boost loop back-edge phi arguments by +1 before backward `phi_partner` propagation so
    // Phase 2 colors `loop_param` -> `back_phi_arg` (claiming `loop_param`'s reg) -> same-depth temp.
    for (arg, &is_back_arg) in is_back_phi_arg.iter().enumerate() {
        if is_back_arg {
            if let Some(w) = use_weight.get_mut(arg) {
                *w = w.saturating_add(1);
            }
        }
    }

    // Propagate priority weights and return-value hints backward along `phi_partner` chains (reverse RPO).
    for &arg in rpo_to_inst.iter().rev() {
        let p = phi_partner.get(arg).copied().unwrap_or(u32::MAX);
        if p != u32::MAX {
            let pi = idx(p);
            if remat_val.get(pi).copied().flatten().is_none()
                && !matches!(view.op(pi), Op::DataAddr | Op::FuncAddr)
            {
                let aw = use_weight.get(arg).copied().unwrap_or(0).saturating_add(1);
                if let Some(pw) = use_weight.get_mut(pi) {
                    if aw > *pw {
                        *pw = aw;
                    }
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

    // 3. Two-Phase O(N log K) RPO Linear-Scan + Greedy Priority-Weighted Coloring + IPRA + Remat!
    #[derive(Clone, Copy)]
    struct Candidate {
        inst: usize,
        start: usize,
        end: usize,
        weight: u64,
        is_remat: bool,
    }
    let mut prog_order: Vec<Candidate, A> = Vec::with_capacity_in(len, alloc);
    for (pos, &inst) in rpo_to_inst.iter().enumerate() {
        if skip_alloc.get(inst).copied().unwrap_or(false) {
            continue;
        }
        let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
        if ty == Ty::NONE || !has_any_use.get(inst).copied().unwrap_or(false) {
            continue;
        }
        let cval = remat_val.get(inst).copied().flatten();
        let uw = use_weight.get(inst).copied().unwrap_or(0);
        let dw = def_weight.get(inst).copied().unwrap_or(0);
        let in_loop = max_use_depth.get(inst).copied().unwrap_or(0) > 0;
        let (weight, is_remat) = candidate_priority(view.op(inst), cval, uw, dw, in_loop);
        let end = last_use_pos.get(inst).copied().unwrap_or(pos).max(pos);
        prog_order.push(Candidate { inst, start: pos, end, weight, is_remat });
    }

    const BUSY_BUCKET_SHIFT: u32 = 10;
    const BUSY_BUCKET_SPAN: usize = 1 << BUSY_BUCKET_SHIFT;
    let num_buckets = (len >> BUSY_BUCKET_SHIFT) + 1;

    enum BusySet<A: Allocator> {
        /// Monotonic ascending `start` order (Phase 1): only the latest interval can overlap.
        Monotonic(Option<(usize, usize, bool)>),
        /// Arbitrary insertion order (Phase 2): O(1) amortized bucketed intervals.
        Bucketed {
            long: Vec<(usize, usize, bool), A>,
            buckets: Vec<Vec<(usize, usize, bool), A>, A>,
        },
    }

    impl<A: Allocator + Copy> BusySet<A> {
        fn new(monotonic: bool, num_buckets: usize, alloc: A) -> Self {
            if monotonic {
                Self::Monotonic(None)
            } else {
                let mut buckets = Vec::with_capacity_in(num_buckets, alloc);
                for _ in 0..num_buckets {
                    buckets.push(Vec::new_in(alloc));
                }
                Self::Bucketed { long: Vec::new_in(alloc), buckets }
            }
        }

        #[inline]
        fn is_free(&self, start: usize, end: usize, cur_rbw: bool) -> bool {
            match self {
                Self::Monotonic(None) => true,
                Self::Monotonic(Some((s, e, s_rbw))) => !intervals_conflict(*s, *e, *s_rbw, start, end, cur_rbw),
                Self::Bucketed { long, buckets } => {
                    if long.iter().any(|&(s, e, s_rbw)| intervals_conflict(s, e, s_rbw, start, end, cur_rbw)) {
                        return false;
                    }
                    let lo_b = (start >> BUSY_BUCKET_SHIFT).saturating_sub(1);
                    let hi_b = (end >> BUSY_BUCKET_SHIFT).min(buckets.len().saturating_sub(1));
                    if let Some(slice) = buckets.get(lo_b..=hi_b) {
                        for b in slice {
                            if b.iter().any(|&(s, e, s_rbw)| intervals_conflict(s, e, s_rbw, start, end, cur_rbw)) {
                                return false;
                            }
                        }
                    }
                    true
                }
            }
        }

        #[inline]
        fn insert(&mut self, start: usize, end: usize, rbw: bool) {
            match self {
                Self::Monotonic(last) => *last = Some((start, end, rbw)),
                Self::Bucketed { long, buckets } => {
                    if end.saturating_sub(start) > BUSY_BUCKET_SPAN {
                        long.push((start, end, rbw));
                    } else if let Some(b) = buckets.get_mut(start >> BUSY_BUCKET_SHIFT) {
                        b.push((start, end, rbw));
                    } else {
                        long.push((start, end, rbw));
                    }
                }
            }
        }
    }

    let allow_lr = call_positions.is_empty();
    let callee_gpr_pool = if allow_lr { CALLEE_SAVED_GPR_WITH_LR } else { CALLEE_SAVED_GPR };

    let run_coloring = |order: &[Candidate], monotonic: bool| -> Result<(Vec<AllocLoc, A>, [bool; 32], [bool; 32], u32, u32, u64), RegAllocError> {
        let mut loc_try: Vec<AllocLoc, A> = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            loc_try.push(AllocLoc::None);
        }
        let mut gpr_busy: Vec<BusySet<A>, A> = Vec::with_capacity_in(32, alloc);
        let mut fp_busy: Vec<BusySet<A>, A> = Vec::with_capacity_in(32, alloc);
        for _ in 0..32 {
            gpr_busy.push(BusySet::new(monotonic, num_buckets, alloc));
            fp_busy.push(BusySet::new(monotonic, num_buckets, alloc));
        }
        let is_reg_free = |busy: &BusySet<A>, start: usize, end: usize, cur_rbw: bool| -> bool {
            busy.is_free(start, end, cur_rbw)
        };
        let mut spill_busy: Vec<BusySet<A>, A> = Vec::new_in(alloc);
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
                        if window.iter().all(|s_iv| s_iv.is_free(start, end, false)) {
                            for s_iv in window {
                                s_iv.insert(start, end, false);
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
                let mut iv = BusySet::new(monotonic, num_buckets, alloc);
                iv.insert(start, end, false);
                spill_busy.push(iv);
            }
            Ok(slot)
        };

        for cand in order {
            let inst = cand.inst;
            let start = cand.start;
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

            let call_clobbers_for = |s_pos: usize, e_pos: usize| -> (bool, CallClobbers) {
                let mut clobbers = CallClobbers::EMPTY;
                let mut crosses = false;
                let first_call = call_rpo_positions.partition_point(|&(c_pos, _)| c_pos <= s_pos);
                if let Some(calls) = call_rpo_positions.get(first_call..) {
                    for &(c_pos, c_inst) in calls {
                        if c_pos >= e_pos {
                            break;
                        }
                        crosses = true;
                        // Unwinding out of an `Invoke` clobbers all caller-saved regs regardless of IPRA.
                        let clob = if matches!(view.op(c_inst), Op::Invoke | Op::InvokeIndirect) {
                            CallClobbers::C_ABI_ARM64
                        } else {
                            call_clobbers(c_inst)
                        };
                        clobbers.clobbered = clobbers.clobbered.union(clob.clobbered);
                        if clobbers.clobbered.union(CallClobbers::C_ABI_ARM64.clobbered)
                            == clobbers.clobbered
                        {
                            break;
                        }
                    }
                }
                (crosses, clobbers)
            };
            let (crosses_call, crossed_clobbers) = call_clobbers_for(start, end);
            let gpr_safe = |r: Reg| -> bool {
                !crosses_call || (X19..=X28).contains(&r) || !crossed_clobbers.clobbers_gpr(reg_u8(r))
            };
            let fp_safe = |r: Reg| -> bool {
                !crosses_call || (8..=15).contains(&r) || !crossed_clobbers.clobbers_fp(reg_u8(r))
            };

            let cur_rbw = is_rbw(view, inst);
            if is_gpr_pair_ty(view, ty) {
                let pool_pick = |busy: &[BusySet<A>], skip: Option<Reg>| -> Option<Reg> {
                    caller_gpr_order
                        .iter()
                        .copied()
                        .chain(callee_gpr_pool.iter().copied())
                        .find(|&r| {
                            Some(r) != skip
                                && gpr_safe(r)
                                && busy.get(idx(r)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
                        })
                };
                let r0 = pool_pick(&gpr_busy, None);
                let r1 = r0.and_then(|first| pool_pick(&gpr_busy, Some(first)));
                if let (Some(lo), Some(hi)) = (r0, r1) {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::GprPair(reg_u8(lo), reg_u8(hi));
                    }
                    if let Some(b) = gpr_busy.get_mut(idx(lo)) {
                        b.insert(start, end, cur_rbw);
                    }
                    if let Some(b) = gpr_busy.get_mut(idx(hi)) {
                        b.insert(start, end, cur_rbw);
                    }
                    if (X19..=X28).contains(&lo) || lo == X30 {
                        if let Some(u) = used_callee_gpr.get_mut(idx(lo)) {
                            *u = true;
                        }
                    }
                    if (X19..=X28).contains(&hi) || hi == X30 {
                        if let Some(u) = used_callee_gpr.get_mut(idx(hi)) {
                            *u = true;
                        }
                    }
                } else {
                    let s0 = alloc_spill_n(1, start, end)?;
                    let s1 = alloc_spill_n(1, start, end)?;
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::GprPairSpill(s0, s1);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight.saturating_mul(2));
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
                            && fp_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
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
                                        && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
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
                                    && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
                            })
                    });
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Vec(reg_u8(reg));
                    }
                    if let Some(b) = fp_busy.get_mut(idx(reg)) {
                        b.insert(start, end, cur_rbw);
                    }
                    if (8..=15).contains(&reg) {
                        if let Some(u) = used_callee_fp.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else {
                    let slot = alloc_spill_n(2, start, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::VecSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight.saturating_mul(2));
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
                        && fp_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
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
                                    && fp_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
                            })
                    });
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Fp(reg_u8(reg));
                    }
                    if let Some(b) = fp_busy.get_mut(idx(reg)) {
                        b.insert(start, end, cur_rbw);
                    }
                    if (8..=15).contains(&reg) {
                        if let Some(u) = used_callee_fp.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else {
                    let slot = alloc_spill_n(1, start, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::FpSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight);
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
                let bp_raw = back_partner_of.get(inst).copied().unwrap_or(u32::MAX);
                let bp_info = if bp_raw != u32::MAX {
                    let bpi = idx(bp_raw);
                    let bp_uncolored = loc_try.get(bpi).copied().unwrap_or(AllocLoc::None) == AllocLoc::None;
                    if bp_uncolored
                        && !skip_alloc.get(bpi).copied().unwrap_or(false)
                        && has_any_use.get(bpi).copied().unwrap_or(false)
                    {
                        let bp_start = rpo_pos.get(bpi).copied().unwrap_or(0);
                        let bp_end = last_use_pos.get(bpi).copied().unwrap_or(bp_start).max(bp_start);
                        let bp_rbw = is_rbw(view, bpi);
                        let (bp_crosses, bp_clobbers) = call_clobbers_for(bp_start, bp_end);
                        Some((bp_start, bp_end, bp_rbw, bp_crosses, bp_clobbers))
                    } else {
                        None
                    }
                } else {
                    None
                };
                let try_gpr = |h: u8| -> Option<Reg> {
                    if h != NO_REG
                        && (u32::from(h) != X30 || allow_lr)
                        && gpr_safe(u32::from(h))
                        && gpr_busy.get(usize::from(h)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
                    {
                        Some(u32::from(h))
                    } else {
                        None
                    }
                };
                let joint_chosen = if let Some((bp_start, bp_end, bp_rbw, bp_crosses, bp_clobbers)) = bp_info {
                    let bp_safe = |r: Reg| -> bool {
                        !bp_crosses || (X19..=X28).contains(&r) || !bp_clobbers.clobbers_gpr(reg_u8(r))
                    };
                    let bp_free = |r: Reg| -> bool {
                        bp_safe(r)
                            && gpr_busy
                                .get(idx(r))
                                .is_some_and(|b| is_reg_free(b, bp_start, bp_end, bp_rbw))
                    };
                    let try_gpr_joint = |h: u8| -> Option<Reg> {
                        let r = try_gpr(h)?;
                        bp_free(r).then_some(r)
                    };
                    try_gpr_joint(primary_hint)
                        .or_else(|| try_gpr_joint(secondary_hint))
                        .or_else(|| try_gpr_joint(pair_trunc_hint))
                        .or_else(|| {
                            caller_gpr_order
                                .iter()
                                .copied()
                                .chain(callee_gpr_pool.iter().copied())
                                .find(|&r| {
                                    gpr_safe(r)
                                        && gpr_busy
                                            .get(idx(r))
                                            .is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
                                        && bp_free(r)
                                })
                        })
                } else {
                    None
                };
                let chosen = joint_chosen
                    .or_else(|| try_gpr(primary_hint))
                    .or_else(|| try_gpr(secondary_hint))
                    .or_else(|| try_gpr(pair_trunc_hint))
                    .or_else(|| {
                        caller_gpr_order
                            .iter()
                            .copied()
                            .chain(callee_gpr_pool.iter().copied())
                            .find(|&r| {
                                gpr_safe(r)
                                    && gpr_busy.get(idx(r)).is_some_and(|b| is_reg_free(b, start, end, cur_rbw))
                            })
                    });
                if let Some(reg) = chosen {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Gpr(reg_u8(reg));
                    }
                    if let Some(b) = gpr_busy.get_mut(idx(reg)) {
                        b.insert(start, end, cur_rbw);
                    }
                    if (X19..=X28).contains(&reg) || reg == X30 {
                        if let Some(u) = used_callee_gpr.get_mut(idx(reg)) {
                            *u = true;
                        }
                    }
                } else if let Some(cval) = remat_val.get(inst).copied().flatten() {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::RematConst(cval);
                    }
                    remat_count += 1;
                    spill_cost = spill_cost.saturating_add(cand.weight);
                } else if matches!(view.op(inst), Op::DataAddr | Op::FuncAddr) {
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::RematGlobal(u32_of(inst));
                    }
                    remat_count += 1;
                    spill_cost = spill_cost.saturating_add(cand.weight);
                } else {
                    let slot = alloc_spill_n(1, start, end)?;
                    if let Some(loc_slot) = loc_try.get_mut(inst) {
                        *loc_slot = AllocLoc::GprSpill(slot);
                    }
                    spill_cost = spill_cost.saturating_add(cand.weight);
                }
            }
        }
        Ok((loc_try, used_callee_gpr, used_callee_fp, num_spill_slots, remat_count, spill_cost))
    };

    // Phase 1: monotonic RPO chordal-SSA coloring.
    let (mut final_loc, mut used_callee_gpr, mut used_callee_fp, mut num_spill_slots, mut remat_count, spill_cost) =
        run_coloring(&prog_order, true)?;

    // Phase 2: if register pressure overflowed (spills > 0 or remat > 0), run loop-weighted
    // Greedy Priority Coloring so hot inner loops stay register-resident while cold outer
    // values/constants yield & rematerialize.
    if num_spill_slots > 0 || remat_count > 0 {
        let mut greedy_order: Vec<Candidate, A> = Vec::with_capacity_in(prog_order.len(), alloc);
        for &c in &prog_order {
            greedy_order.push(c);
        }
        greedy_order.sort_unstable_by(|a, b| {
            a.is_remat
                .cmp(&b.is_remat)
                .then_with(|| b.weight.cmp(&a.weight))
                .then_with(|| a.start.cmp(&b.start))
        });
        if let Ok((g_loc, g_cgpr, g_cfp, g_spills, g_remat, g_cost)) = run_coloring(&greedy_order, false) {
            if g_cost < spill_cost || (g_cost == spill_cost && g_spills <= num_spill_slots) {
                final_loc = g_loc;
                used_callee_gpr = g_cgpr;
                used_callee_fp = g_cfp;
                num_spill_slots = g_spills;
                remat_count = g_remat;
            }
        }
    }

    Ok(finalize_allocation(
        view,
        final_loc,
        used_callee_gpr,
        used_callee_fp,
        num_spill_slots,
        remat_count,
        &call_positions,
        call_clobbers,
    ))
}

#[cfg(test)]
mod tests {
    use blaze_lir::{
        Flags, IntCond, Op, Scalar, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv, RefKind},
    };
    use blaze_own::{Alloc, Vec};
    use blaze_tier::{CallClobbers, Machine, Named, Names, Outside, RoutinePlaces};

    use super::{
        AllocLoc, CALLER_SAVED_FP, CALLER_SAVED_GPR, ScratchOrder, allocate,
        allocate_linear_scan_rpo, allocate_with_ipra_and_order, build_caller_fp_order,
        build_caller_gpr_order, u32_of,
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
            &[],
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
            &[],
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

    struct ExtFuncNames;
    impl Names for ExtFuncNames {
        fn name(&self, of: Outside) -> Option<Named<'_>> {
            match of {
                Outside::Func(_) => Some(Named::C("ext_fn")),
                _ => None,
            }
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    #[test]
    fn exact_crosses_call_ignores_linear_straddling() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        let loop_hdr = b.create_block(BlockKind::Normal);
        // Laid out linearly BETWEEN `loop_hdr` and `loop_body`!
        let exit_call = b.create_block(BlockKind::Normal);
        let loop_body = b.create_block(BlockKind::Normal);
        let inc_blk = b.create_block(BlockKind::Normal);
        // Uses `p_i` linearly AFTER `inc_blk` so linear intervals would falsely block coalescing!
        let exit_ret = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let live_x = b.param(Ty::I64);
        let z0 = b.iconst(Ty::I64, 0);
        b.jump((loop_hdr, &[z0]));

        b.begin(loop_hdr);
        let p_i = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, p_i, n);
        b.br(Flags::NONE, cond, (loop_body, &[]), (exit_call, &[]));

        b.begin(exit_call);
        let call_res = b.call(Flags::NONE, callee_ref, callee_sig, &[ptr]);
        let call_out = b.binary(Op::Add, Flags::NONE, call_res, live_x);
        b.ret(&[call_out]);

        b.begin(loop_body);
        let c3 = b.iconst(Ty::I64, 3);
        let off = b.binary(Op::Shl, Flags::NONE, p_i, c3);
        let elem_ptr = b.ptr_add(Flags::NONE, ptr, off);
        let val = b.load(Ty::I64, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(8));
        let keep_going = b.icmp(IntCond::Ult, val, n);
        b.br(Flags::NONE, keep_going, (inc_blk, &[]), (exit_ret, &[]));

        b.begin(inc_blk);
        let c1 = b.iconst(Ty::I64, 1);
        let i_next = b.binary(Op::Add, Flags::NONE, p_i, c1);
        b.jump((loop_hdr, &[i_next]));

        b.begin(exit_ret);
        b.ret(&[p_i]);

        let func = b.finish_checked().expect("straddling call loop LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }

        // Verify linear layout genuinely straddles `call_res`.
        assert!(p_i.index() < call_res.index() && call_res.index() < i_next.index());

        let ra = allocate(&view, &skip, &tail, 32, crate::Os::MacOs, alloc)
            .expect("regalloc succeeds for straddling call loop");

        // 1. `p_i` does NOT cross `call_res` in the CFG -> must stay in a caller-saved GPR!
        match ra.loc_of[p_i.index()] {
            AllocLoc::Gpr(r) => {
                assert!(
                    !(19..=28).contains(&r),
                    "loop IV p_i must stay in caller-saved GPR, got x{r}"
                );
            }
            other => panic!("expected caller-saved GPR for p_i, got {other:?}"),
        }

        // 2. `i_next = add(p_i, 1)` must coalesce in-place with `p_i` even though `exit_ret` uses `p_i`!
        assert_eq!(
            ra.loc_of[i_next.index()],
            ra.loc_of[p_i.index()],
            "loop IV update i_next must coalesce into p_i's physical register"
        );

        // 3. Positive control: `live_x` genuinely crosses `call_res` -> must use a callee-saved GPR!
        match ra.loc_of[live_x.index()] {
            AllocLoc::Gpr(r) => {
                assert!(
                    (19..=28).contains(&r),
                    "live_x genuinely crosses call and must use callee-saved x19..x28, got x{r}"
                );
            }
            other => panic!("expected callee-saved GPR for live_x, got {other:?}"),
        }
    }

    #[test]
    fn call_crossing_small_const_and_cold_global_remat() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let panic_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
        let callee_ref = b.reference(RefKind::Func, 0);
        let panic_ref = b.reference(RefKind::Func, 1);
        let panic_data = b.data(b"panic_location_str", 1, &[]);

        let entry = b.create_block(BlockKind::Normal);
        let cold_panic = b.create_block(BlockKind::Normal);
        let ok_ret = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let flag = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let panic_loc = b.data_addr(panic_data);
        let res = b.call(Flags::NONE, callee_ref, callee_sig, &[flag]);
        let is_zero = b.icmp(IntCond::Eq, res, c0);
        b.br(Flags::NONE, is_zero, (cold_panic, &[]), (ok_ret, &[res]));

        b.begin(cold_panic);
        b.call(Flags::NONE, panic_ref, panic_sig, &[panic_loc]);
        b.unreachable();

        b.begin(ok_ret);
        let r = b.param(Ty::I64);
        let is_one = b.icmp(IntCond::Eq, r, c1);
        let chosen = b.select(Flags::NONE, is_one, c0, c1);
        b.ret(&[chosen]);

        let func = b.finish_checked().expect("remat const + cold global LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }

        let ra = allocate(&view, &skip, &tail, 32, crate::Os::MacOs, alloc)
            .expect("regalloc succeeds for remat const + cold global");

        assert_eq!(ra.loc_of[c0.index()], AllocLoc::RematConst(0));
        assert_eq!(ra.loc_of[c1.index()], AllocLoc::RematConst(1));
        assert_eq!(
            ra.loc_of[panic_loc.index()],
            AllocLoc::RematGlobal(u32_of(panic_loc.index()))
        );
        assert_eq!(
            ra.x_pairs, 0,
            "Iconst(0), Iconst(1), and cold-only DataAddr must consume ZERO callee-saved x19..x28 pairs"
        );

        let emitted = super::super::isel::emit_sota_a64(
            view,
            &ExtFuncNames,
            super::super::CallMode::DirectBl,
            Machine::LSE,
            true,
            alloc,
        )
        .expect("emit_sota_a64 succeeds with RematConst + RematGlobal");
        assert_eq!(
            emitted.unwind_frame.map_or(0, |f| f.x_pairs),
            0,
            "emitted function must save ZERO callee-saved x19..x28 pairs"
        );
        assert!(
            emitted.relocs.iter().any(|r| r.target == blaze_code::record::OWN_RELRO),
            "rematerialized cold DataAddr must still emit OWN_RELRO relocations at the cold panic site"
        );
    }

    #[test]
    fn linear_scan_rpo_protects_call_crossing_loop_values() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        let loop_hdr = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);
        let loop_body = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let z0 = b.iconst(Ty::I64, 0);
        b.jump((loop_hdr, &[z0]));

        // `n` is ONLY read here (via fused `cond = icmp(Ult, p_i, n)`), BEFORE `loop_body`'s call,
        // yet stays live across the back-edge `loop_body -> loop_hdr`!
        b.begin(loop_hdr);
        let p_i = b.param(Ty::I64);
        let i_next = b.binary(Op::Add, Flags::NONE, p_i, z0);
        let cond = b.icmp(IntCond::Ult, p_i, n);
        b.br(Flags::NONE, cond, (loop_body, &[]), (exit, &[p_i]));

        b.begin(exit);
        let ret_v = b.param(Ty::I64);
        b.ret(&[ret_v]);

        // `call` executes AFTER `i_next` is defined and BEFORE the back-edge passing `i_next`!
        b.begin(loop_body);
        let call_res = b.call(Flags::NONE, callee_ref, callee_sig, &[ptr]);
        let bumped = b.binary(Op::Add, Flags::NONE, i_next, call_res);
        b.jump((loop_hdr, &[bumped]));

        let func = b.finish_checked().expect("call-in-loop LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        // Fuse `cond = icmp(Ult, p_i, n)` into `br` just like `isel.rs` does!
        skip[cond.index()] = true;

        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let ra_rpo = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            32,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan regalloc succeeds");
        let ra_exact = allocate(&view, &skip, &tail, 32, crate::Os::MacOs, alloc)
            .expect("exact-CFG regalloc succeeds");

        for (label, ra) in [("linear_scan_rpo", &ra_rpo), ("exact_cfg", &ra_exact)] {
            for (vname, val) in [("n", n), ("i_next", i_next), ("ptr", ptr)] {
                match ra.loc_of[val.index()] {
                    AllocLoc::Gpr(r) => {
                        assert!(
                            (19..=28).contains(&r),
                            "{label}: {vname} crosses call in loop back-edge and must use callee-saved x19..x28, got x{r}"
                        );
                    }
                    other => panic!("{label}: expected callee-saved GPR for {vname}, got {other:?}"),
                }
            }
            assert_ne!(ra.x_pairs, 0, "{label}: must save callee-saved GPR pairs");
        }
    }

    #[test]
    fn linear_scan_rpo_rematerializes_consts_and_globals_under_pressure() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let gdata = b.data(b"mega_fn_global_str", 8, &[]);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        // 28 simultaneously live values (> 26 allocatable GPRs) so pressure overflows into remat!
        let big_const = b.iconst(Ty::I64, 0x1234_5678);
        let gaddr = b.data_addr(gdata);
        let mut live = Vec::with_capacity_in(28, alloc);
        let c1 = b.iconst(Ty::I64, 1);
        let mut cur = x;
        for _ in 0..27usize {
            cur = b.binary(Op::Add, Flags::NONE, cur, c1);
            live.push(cur);
        }
        let mut acc = big_const;
        for &v in &live {
            acc = b.binary(Op::Xor, Flags::NONE, acc, v);
        }
        let g_val = b.load(Ty::I64, Flags::NONE, gaddr, 0, MemAttrs::aligned(8));
        let out = b.binary(Op::Xor, Flags::NONE, acc, g_val);
        b.ret(&[out]);

        let func = b.finish_checked().expect("high-pressure LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }

        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let ra = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            32,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan regalloc succeeds under pressure");

        assert!(
            matches!(ra.loc_of[big_const.index()], AllocLoc::RematConst(0x1234_5678))
                || matches!(ra.loc_of[gaddr.index()], AllocLoc::RematGlobal(_)),
            "expected RPO linear-scan to rematerialize constants/globals under >26 GPR pressure, got const={:?}, global={:?}",
            ra.loc_of[big_const.index()],
            ra.loc_of[gaddr.index()],
        );
        assert!(ra.remat_count > 0, "expected remat_count > 0 under >26 GPR pressure");
    }

    #[test]
    fn exact_cfg_gpr_pair_spill_uses_distinct_slots() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I128)], &[Param::of(Ty::I128)]);
        b.set_signature(sig);
        let helper_sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
        let helper = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let base = b.param(Ty::I128);
        // 8 simultaneously live `i128` values crossing a call require 16 callee-saved GPRs,
        // exceeding the 10 available (`x19..=x28`) and forcing `AllocLoc::GprPairSpill`.
        let mut vals: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(8, alloc);
        let mut cur = base;
        for k in 1u128..=8 {
            let ck = b.iconst(Ty::I128, k);
            cur = b.binary(Op::Add, Flags::NONE, cur, ck);
            vals.push(cur);
        }
        let call_res = b.call(Flags::NONE, helper, helper_sig, &[]);
        let mut acc = b.convert(Op::Zext, Flags::NONE, Ty::I128, call_res);
        for &v in &vals {
            acc = b.binary(Op::Xor, Flags::NONE, acc, v);
        }
        b.ret(&[acc]);

        let func = b.finish_checked().expect("spilled i128 LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }

        let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG regalloc succeeds");
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let ra_rpo = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            64,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan regalloc succeeds");

        for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
            let mut spill_count = 0usize;
            let mut live_slots: Vec<u32, Alloc<'static>> = Vec::new_in(alloc);
            for &v in &vals {
                match ra.loc_of[v.index()] {
                    AllocLoc::GprPair(lo, hi) => {
                        assert_ne!(lo, hi, "{label}: GprPair halves must use distinct regs");
                    }
                    AllocLoc::GprPairSpill(s0, s1) => {
                        assert_ne!(
                            s0, s1,
                            "{label}: GprPairSpill halves must use distinct spill slots (got ({s0}, {s1}))"
                        );
                        assert!(
                            !live_slots.iter().any(|&s| s == s0 || s == s1),
                            "{label}: simultaneously live GprPairSpill ({s0}, {s1}) collided with {live_slots:?}"
                        );
                        live_slots.push(s0);
                        live_slots.push(s1);
                        spill_count += 1;
                    }
                    other => panic!("{label}: unexpected i128 loc {other:?}"),
                }
            }
            assert!(
                spill_count >= 3,
                "{label}: expected at least 3 spilled i128 values across call, got {spill_count}"
            );
        }
    }

    #[test]
    fn hot_loop_64bit_hash_primes_stay_in_gprs_under_pressure() {
        // 1. Unit-check `const_mat_cost` across 1..=4 instruction constants.
        assert_eq!(super::const_mat_cost(0), 1);
        assert_eq!(super::const_mat_cost(u64::MAX), 1);
        assert_eq!(super::const_mat_cost(0x1234), 1);
        assert_eq!(super::const_mat_cost(0x1234_0000_0000_0000), 1);
        assert_eq!(super::const_mat_cost(!0x1234u64), 1);
        assert_eq!(super::const_mat_cost(0x00ff_00ff_00ff_00ff), 1);
        assert_eq!(super::const_mat_cost(0x1234_5678), 2);
        assert_eq!(super::const_mat_cost(0x0000_1234_5678_9abc), 3);
        const HASH7_PRIME: u64 = 0x00cf_1bbc_dcb9_e377;
        const HASH4_PRIME: u64 = 0x9e37_79b1_85eb_ca87;
        assert_eq!(super::const_mat_cost(HASH7_PRIME), 4);
        assert_eq!(super::const_mat_cost(HASH4_PRIME), 4);

        // 2. Build a high-pressure loop (>26 live GPRs: 24 cold depth-0 scalars + 2 hot 64-bit
        //    hash primes + 1-inst loop constant + loop IV/acc + call in loop) and verify BOTH
        //    exact-CFG and RPO linear-scan keep both 4-inst 64-bit primes in physical GPRs!
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let loop_hdr = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let x = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let c1 = b.iconst(Ty::I64, 1);
        let mut cold_vals = Vec::with_capacity_in(24, alloc);
        let mut cur = x;
        for _ in 0..24usize {
            cur = b.binary(Op::Add, Flags::NONE, cur, c1);
            cold_vals.push(cur);
        }
        let prime7 = b.iconst(Ty::I64, u128::from(HASH7_PRIME));
        let prime4 = b.iconst(Ty::I64, u128::from(HASH4_PRIME));
        let cheap_mask = b.iconst(Ty::I64, 0x3fff);
        let z0 = b.iconst(Ty::I64, 0);
        b.jump((loop_hdr, &[z0, x]));

        b.begin(loop_hdr);
        let iv = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let h1 = b.binary(Op::Mul, Flags::NONE, acc, prime7);
        let h2 = b.binary(Op::Mul, Flags::NONE, h1, prime4);
        let h3 = b.binary(Op::Xor, Flags::NONE, h2, cheap_mask);
        let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
        let cond = b.icmp(IntCond::Ult, iv_next, n);
        b.br(Flags::NONE, cond, (loop_hdr, &[iv_next, h3]), (exit, &[h3]));

        b.begin(exit);
        let mut out = b.param(Ty::I64);
        for &cv in &cold_vals {
            out = b.binary(Op::Xor, Flags::NONE, out, cv);
        }
        b.ret(&[out]);

        let func = b.finish_checked().expect("hash-prime pressure loop LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }

        let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG regalloc succeeds for hash-prime pressure loop");
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let ra_rpo = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            64,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan regalloc succeeds for hash-prime pressure loop");

        for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
            assert!(
                ra.num_spill_slots > 0,
                "{label}: test must overflow 26 GPRs and spill cold depth-0 scalars"
            );
            assert!(
                matches!(ra.loc_of[prime7.index()], AllocLoc::Gpr(_)),
                "{label}: 4-inst HASH7_PRIME inside hot loop must stay in a GPR, got {:?}",
                ra.loc_of[prime7.index()]
            );
            assert!(
                matches!(ra.loc_of[prime4.index()], AllocLoc::Gpr(_)),
                "{label}: 4-inst HASH4_PRIME inside hot loop must stay in a GPR, got {:?}",
                ra.loc_of[prime4.index()]
            );
        }
    }

    #[test]
    fn skipped_i128_load_keeps_src_addr_live_to_store_in_both_allocators() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(32, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR)],
            &[Param::of(Ty::PTR)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let loaded = b.load(Ty::I128, Flags::NONE, src, 0, MemAttrs::aligned(0));
        let c16 = b.iconst(Ty::I64, 16);
        let src_next = b.ptr_add(Flags::NONE, src, c16);
        b.store(Flags::NONE, loaded, dst, 0, MemAttrs::aligned(0));
        b.ret(&[src_next]);
        let func = b.finish_checked().expect("fused i128 regalloc LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        skip[loaded.index()] = true;

        let ra_exact = allocate(&view, &skip, &tail, 32, crate::Os::MacOs, alloc)
            .expect("exact-CFG regalloc succeeds for skipped i128 load");
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let ra_rpo = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            32,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan regalloc succeeds for skipped i128 load");

        for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
            assert_eq!(
                ra.loc_of[loaded.index()],
                AllocLoc::None,
                "{label}: skipped I128 load must allocate ZERO GPR pair"
            );
            assert!(
                matches!(ra.loc_of[src.index()], AllocLoc::Gpr(_)),
                "{label}: src ptr must stay allocated in a GPR, got {:?}",
                ra.loc_of[src.index()]
            );
            assert!(
                matches!(ra.loc_of[src_next.index()], AllocLoc::Gpr(_)),
                "{label}: src_next ptr must be allocated in a GPR, got {:?}",
                ra.loc_of[src_next.index()]
            );
            assert_ne!(
                ra.loc_of[src.index()],
                ra.loc_of[src_next.index()],
                "{label}: src must stay live until Store so src_next cannot clobber it"
            );
        }
    }

    #[test]
    fn loop_4inst_hash_prime_keeps_callee_saved_gpr_across_cold_call() {
        const HASH8_PRIME: u64 = 0x9e37_79b1_85eb_ca87;
        const CHEAP_MASK: u64 = 0x3fff;

        // Verify `candidate_priority` scales non-constant scalars to `8 * uw`, keeps 3-4 inst loop
        // constants at `4 * mat_cost * uw` (even when loop-local `uw == def_w`), and marks 1-inst
        // constants `cheap_remat = true` with weight `net_uw.max(1)`.
        let (p4_w, p4_cheap) =
            super::candidate_priority(Op::IconstWide, Some(HASH8_PRIME), 32, 1, true);
        let (p3_w, p3_cheap) = super::candidate_priority(
            Op::IconstWide,
            Some(0x0000_1234_5678_9abc),
            32,
            1,
            true,
        );
        let (p1_w, p1_cheap) =
            super::candidate_priority(Op::Iconst, Some(CHEAP_MASK), 32, 1, true);
        let (s3_w, s3_cheap) = super::candidate_priority(Op::Add, None, 49, 1, true);
        let (s2_w, s2_cheap) = super::candidate_priority(Op::Add, None, 33, 1, true);
        let (same_depth_p4_w, same_depth_p4_cheap) =
            super::candidate_priority(Op::IconstWide, Some(HASH8_PRIME), 16, 16, true);
        let (same_depth_p1_w, same_depth_p1_cheap) =
            super::candidate_priority(Op::Iconst, Some(CHEAP_MASK), 16, 16, true);
        assert!(
            !p4_cheap
                && !p3_cheap
                && p1_cheap
                && !s3_cheap
                && !s2_cheap
                && !same_depth_p4_cheap
                && same_depth_p1_cheap
        );
        assert_eq!(p4_w, 32 * 16);
        assert_eq!(p3_w, 32 * 12);
        assert_eq!(p1_w, 31);
        assert_eq!(s3_w, 49 * 8);
        assert_eq!(same_depth_p4_w, 16 * 16);
        assert_eq!(same_depth_p1_w, 1);
        assert!(
            p4_w > s3_w,
            "2 loop uses of 4-inst prime ({p4_w}) must outrank 3-use scalar ({s3_w})"
        );
        assert!(
            p3_w > s2_w,
            "2 loop uses of 3-inst const ({p3_w}) must outrank 2-use scalar ({s2_w})"
        );

        // Build a loop using `HASH8_PRIME` twice per iteration alongside a call in a cold block
        // and >10 call-crossing values competing for the 10 callee-saved GPRs (`x19..=x28`),
        // including 8 hot 3-use call-crossing scalars (which previously beat `scale = 5`) and
        // 4 cold post-loop scalars.
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR),
                Param::of(Ty::PTR),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let callee_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        let loop_hdr = b.create_block(BlockKind::Normal);
        let cold_call = b.create_block(BlockKind::Normal);
        let loop_latch = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr0 = b.param(Ty::PTR);
        let ptr1 = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let z0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let mut cold_vals = Vec::with_capacity_in(4, alloc);
        let mut cur = seed;
        for _ in 0..4usize {
            cur = b.binary(Op::Add, Flags::NONE, cur, c1);
            cold_vals.push(cur);
        }
        let mut hot_mix = Vec::with_capacity_in(8, alloc);
        for _ in 0..8usize {
            cur = b.binary(Op::Add, Flags::NONE, cur, c1);
            hot_mix.push(cur);
        }
        // Defined after 12 other call-crossing values so Phase 1 program-order coloring fills
        // all 10 callee-saved GPRs before reaching `prime`, exercising Phase 2 priority recovery.
        let prime = b.iconst(Ty::I64, u128::from(HASH8_PRIME));
        let cheap_mask = b.iconst(Ty::I64, u128::from(CHEAP_MASK));
        b.jump((loop_hdr, &[z0, seed]));

        b.begin(loop_hdr);
        let iv = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let p0 = b.ptr_add(Flags::NONE, ptr0, iv);
        let p1 = b.ptr_add(Flags::NONE, ptr1, iv);
        let v0 = b.load(Ty::I64, Flags::NONE, p0, 0, MemAttrs::aligned(8));
        let v1 = b.load(Ty::I64, Flags::NONE, p1, 0, MemAttrs::aligned(8));
        let h0 = b.binary(Op::Mul, Flags::NONE, v0, prime);
        let h1 = b.binary(Op::Mul, Flags::NONE, v1, prime);
        let mixed = b.binary(Op::Xor, Flags::NONE, h0, h1);
        let mut acc1 = b.binary(Op::Add, Flags::NONE, acc, mixed);
        acc1 = b.binary(Op::Xor, Flags::NONE, acc1, cheap_mask);
        for &hm in &hot_mix {
            acc1 = b.binary(Op::Add, Flags::NONE, acc1, hm);
            acc1 = b.binary(Op::Xor, Flags::NONE, acc1, hm);
            acc1 = b.binary(Op::Sub, Flags::NONE, acc1, hm);
        }
        let is_cold = b.icmp(IntCond::Eq, mixed, z0);
        b.br(Flags::NONE, is_cold, (cold_call, &[]), (loop_latch, &[acc1]));

        b.begin(cold_call);
        let call_res = b.call(Flags::NONE, callee_ref, callee_sig, &[acc1]);
        let acc_cold = b.binary(Op::Add, Flags::NONE, acc1, call_res);
        b.jump((loop_latch, &[acc_cold]));

        b.begin(loop_latch);
        let acc_merged = b.param(Ty::I64);
        let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
        let keep_going = b.icmp(IntCond::Ult, iv_next, n);
        b.br(
            Flags::NONE,
            keep_going,
            (loop_hdr, &[iv_next, acc_merged]),
            (exit, &[acc_merged]),
        );

        b.begin(exit);
        let mut out = b.param(Ty::I64);
        for &cv in &cold_vals {
            out = b.binary(Op::Xor, Flags::NONE, out, cv);
        }
        b.ret(&[out]);

        let func = b.finish_checked().expect("cold-call hash prime loop LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }

        let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG regalloc succeeds for cold-call hash prime loop");
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let ra_rpo = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            64,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan regalloc succeeds for cold-call hash prime loop");

        for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
            assert!(
                ra.num_spill_slots > 0,
                "{label}: test must overflow the 10 callee-saved GPRs and spill lower-weight values"
            );
            assert_ne!(ra.x_pairs, 0, "{label}: must save callee-saved GPR pairs");
            match ra.loc_of[prime.index()] {
                AllocLoc::Gpr(r) => {
                    assert!(
                        (19..=28).contains(&r),
                        "{label}: call-crossing 4-inst HASH8_PRIME must stay in callee-saved x19..=x28, got x{r}"
                    );
                }
                other => panic!(
                    "{label}: 4-inst HASH8_PRIME used twice per loop iteration must stay in a physical GPR, got {other:?}"
                ),
            }
            assert_eq!(
                ra.loc_of[cheap_mask.index()],
                AllocLoc::RematConst(CHEAP_MASK),
                "{label}: 1-inst constant 0x3fff should stay cheap to rematerialize under pressure"
            );
        }
    }

    #[test]
    fn cheap_1inst_const_never_steals_gpr_from_loop_scalar() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let loop_hdr = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let x = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        // Define 1-inst constant `#0x10` FIRST so Phase 1 program-order coloring tries to give it
        // a GPR before the 24 loop-invariant scalars, forcing Phase 2 to evict `#0x10` instead.
        let c16 = b.iconst(Ty::I64, 0x10);
        let c1 = b.iconst(Ty::I64, 1);
        let mut loop_scalars = Vec::with_capacity_in(24, alloc);
        let mut cur = x;
        for _ in 0..24usize {
            cur = b.binary(Op::Add, Flags::NONE, cur, c1);
            loop_scalars.push(cur);
        }
        let z0 = b.iconst(Ty::I64, 0);
        b.jump((loop_hdr, &[z0, cur]));

        b.begin(loop_hdr);
        let iv = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let mut tmp = acc;
        for &sv in &loop_scalars {
            tmp = b.binary(Op::Xor, Flags::NONE, tmp, sv);
        }
        // Two non-imm-folded uses of `#0x10` inside the loop (Mul & Xor).
        tmp = b.binary(Op::Mul, Flags::NONE, tmp, c16);
        tmp = b.binary(Op::Xor, Flags::NONE, tmp, c16);
        let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
        let cond = b.icmp(IntCond::Ult, iv_next, n);
        b.br(Flags::NONE, cond, (loop_hdr, &[iv_next, tmp]), (exit, &[tmp]));

        b.begin(exit);
        let out = b.param(Ty::I64);
        b.ret(&[out]);

        let func = b.finish_checked().expect("loop scalar vs #0x10 LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        // Fold `c1` (add_imm) and `cond` (CmpBr) just like `isel.rs` so peak non-remat GPR
        // pressure inside `loop_hdr` is exactly 27 (`n`, `iv`, `acc`, and 24 `loop_scalars`).
        skip[c1.index()] = true;
        skip[cond.index()] = true;

        let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG regalloc succeeds");
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let ra_rpo = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            64,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan regalloc succeeds");

        for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
            assert_eq!(
                ra.loc_of[c16.index()],
                AllocLoc::RematConst(16),
                "{label}: 1-inst constant #0x10 must yield RematConst(16), got {:?}",
                ra.loc_of[c16.index()]
            );
            assert_eq!(
                ra.num_spill_slots, 0,
                "{label}: all 27 loop scalars must stay in physical GPRs with zero spills"
            );
            for (k, &sv) in loop_scalars.iter().enumerate() {
                assert!(
                    matches!(ra.loc_of[sv.index()], AllocLoc::Gpr(_)),
                    "{label}: loop scalar[{k}] must stay in a GPR, got {:?}",
                    ra.loc_of[sv.index()]
                );
            }
            assert!(
                matches!(ra.loc_of[n.index()], AllocLoc::Gpr(_))
                    && matches!(ra.loc_of[iv.index()], AllocLoc::Gpr(_))
                    && matches!(ra.loc_of[acc.index()], AllocLoc::Gpr(_)),
                "{label}: loop control/accumulator scalars must stay in GPRs"
            );
        }
    }

    #[test]
    fn leaf_function_can_allocate_lr_when_pressure_exceeds_26() {
        let alloc = test_alloc();
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);

        let uses_x30 = |loc_of: &[AllocLoc]| {
            loc_of.iter().any(|&loc| {
                matches!(
                    loc,
                    AllocLoc::Gpr(30) | AllocLoc::GprPair(30, _) | AllocLoc::GprPair(_, 30)
                )
            })
        };

        // 1. Zero-call leaf function with 27 simultaneously live GPRs allocates X30 with 0 spills!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let mut live = Vec::with_capacity_in(26, alloc);
            let mut cur = x;
            for _ in 0..26usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                live.push(cur);
            }
            let mut acc = b.binary(Op::Xor, Flags::NONE, x, live[0]);
            for &v in &live[1..] {
                acc = b.binary(Op::Xor, Flags::NONE, acc, v);
            }
            b.ret(&[acc]);

            let func = b.finish_checked().expect("27-GPR leaf LIR builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            skip[c1.index()] = true;

            let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("exact-CFG leaf regalloc succeeds");
            let ra_rpo = allocate_linear_scan_rpo(
                &view,
                &skip,
                &[],
                &tail,
                64,
                crate::Os::MacOs,
                |_| CallClobbers::C_ABI_ARM64,
                &gpr_ord,
                &fp_ord,
                alloc,
            )
            .expect("RPO linear-scan leaf regalloc succeeds");

            for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
                assert_eq!(ra.num_spill_slots, 0, "{label}: 27 live GPRs in leaf must have 0 spills");
                assert!(!ra.has_calls, "{label}: leaf function must have has_calls == false");
                assert!(ra.uses_lr, "{label}: 27 live GPRs in leaf must set uses_lr = true");
                assert!(uses_x30(&ra.loc_of), "{label}: expected one value allocated to Gpr(30)");
                assert_eq!(ra.x_pairs, 0x1f, "{label}: X30 must only be used after all 5 x19..x28 pairs");
            }
        }

        // 2. Function with an `Op::Call` MUST NEVER allocate X30!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let callee_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let mut live = Vec::with_capacity_in(26, alloc);
            let mut cur = x;
            for _ in 0..26usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                live.push(cur);
            }
            let call_res = b.call(Flags::NONE, callee_ref, callee_sig, &[x]);
            let mut acc = b.binary(Op::Xor, Flags::NONE, call_res, x);
            for &v in &live {
                acc = b.binary(Op::Xor, Flags::NONE, acc, v);
            }
            b.ret(&[acc]);

            let func = b.finish_checked().expect("27-GPR call LIR builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            skip[c1.index()] = true;

            let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("exact-CFG call regalloc succeeds");
            let ra_rpo = allocate_linear_scan_rpo(
                &view,
                &skip,
                &[],
                &tail,
                64,
                crate::Os::MacOs,
                |_| CallClobbers::C_ABI_ARM64,
                &gpr_ord,
                &fp_ord,
                alloc,
            )
            .expect("RPO linear-scan call regalloc succeeds");

            for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
                assert!(ra.has_calls, "{label}: call function must report has_calls = true");
                assert!(!ra.uses_lr, "{label}: function with Op::Call must never set uses_lr");
                assert!(!uses_x30(&ra.loc_of), "{label}: function with Op::Call must never allocate X30");
            }
        }

        // 3. Function with dynamic-length `Op::Memcpy` (`bl memcpy`) MUST NEVER allocate X30!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I64),
                    Param::of(Ty::PTR),
                    Param::of(Ty::PTR),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let dyn_len = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let mut live = Vec::with_capacity_in(26, alloc);
            let mut cur = x;
            for _ in 0..26usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                live.push(cur);
            }
            b.bulk(Op::Memcpy, Flags::NONE, dst, src, dyn_len, MemAttrs::aligned(1));
            let mut acc = b.binary(Op::Xor, Flags::NONE, x, live[0]);
            for &v in &live[1..] {
                acc = b.binary(Op::Xor, Flags::NONE, acc, v);
            }
            b.ret(&[acc]);

            let func = b.finish_checked().expect("27-GPR memcpy LIR builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            skip[c1.index()] = true;

            let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("exact-CFG memcpy regalloc succeeds");
            let ra_rpo = allocate_linear_scan_rpo(
                &view,
                &skip,
                &[],
                &tail,
                64,
                crate::Os::MacOs,
                |_| CallClobbers::C_ABI_ARM64,
                &gpr_ord,
                &fp_ord,
                alloc,
            )
            .expect("RPO linear-scan memcpy regalloc succeeds");

            for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
                assert!(ra.has_calls, "{label}: dynamic Op::Memcpy must report has_calls = true");
                assert!(!ra.uses_lr, "{label}: function with dynamic Op::Memcpy must never set uses_lr");
                assert!(
                    !uses_x30(&ra.loc_of),
                    "{label}: function with dynamic Op::Memcpy must never allocate X30"
                );
            }
        }
    }

    #[test]
    fn depth1_loop_invariant_beats_40_depth0_uses_of_epilogue_ptr() {
        let alloc = test_alloc();
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);

        // 1. Depth-1 loop invariant with 1 loop use (weight 128 + 1 = 129) must stay in a GPR
        //    even when 3 cold depth-0 epilogue pointers each have 40 depth-0 uses (weight 41)
        //    under >27 live GPR pressure (previously evicted under 16x depth weighting!).
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::PTR),
                    Param::of(Ty::PTR),
                    Param::of(Ty::PTR),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let loop_hdr = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let ep0 = b.param(Ty::PTR);
            let ep1 = b.param(Ty::PTR);
            let ep2 = b.param(Ty::PTR);
            let x = b.param(Ty::I64);
            let n = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let c0 = b.iconst(Ty::I64, 0);
            let lit_limit = b.binary(Op::Add, Flags::NONE, x, n);
            let mut invs = Vec::with_capacity_in(21, alloc);
            let mut cur = x;
            for _ in 0..21usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                invs.push(cur);
            }
            b.jump((loop_hdr, &[c0, x]));

            b.begin(loop_hdr);
            let iv = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            // `lit_limit` has a SINGLE use at depth 1; each `inv` has 2 uses at depth 1.
            let mut h = b.binary(Op::Xor, Flags::NONE, acc, lit_limit);
            for &inv in &invs {
                let t = b.binary(Op::Add, Flags::NONE, h, inv);
                h = b.binary(Op::Xor, Flags::NONE, t, inv);
            }
            let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
            let cond = b.icmp(IntCond::Ult, iv_next, n);
            b.br(Flags::NONE, cond, (loop_hdr, &[iv_next, h]), (exit, &[h]));

            b.begin(exit);
            let out = b.param(Ty::I64);
            for k in 0..40usize {
                let off = i32::try_from(k * 8).unwrap_or(0);
                b.store(Flags::NONE, out, ep0, off, MemAttrs::aligned(8));
                b.store(Flags::NONE, out, ep1, off, MemAttrs::aligned(8));
                b.store(Flags::NONE, out, ep2, off, MemAttrs::aligned(8));
            }
            b.ret(&[out]);

            let func = b.finish_checked().expect("depth1 vs 40-use epilogue ptr LIR builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            skip[c1.index()] = true;

            let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("exact-CFG regalloc succeeds");
            let ra_rpo = allocate_linear_scan_rpo(
                &view,
                &skip,
                &[],
                &tail,
                64,
                crate::Os::MacOs,
                |_| CallClobbers::C_ABI_ARM64,
                &gpr_ord,
                &fp_ord,
                alloc,
            )
            .expect("RPO linear-scan regalloc succeeds");

            for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
                assert!(
                    ra.num_spill_slots >= 2,
                    "{label}: must overflow 27 leaf GPRs and spill cold depth-0 epilogue pointers"
                );
                assert!(
                    matches!(ra.loc_of[lit_limit.index()], AllocLoc::Gpr(_)),
                    "{label}: single-use depth-1 loop invariant `lit_limit` must stay in a GPR, got {:?}",
                    ra.loc_of[lit_limit.index()]
                );
                assert!(
                    matches!(ra.loc_of[n.index()], AllocLoc::Gpr(_)),
                    "{label}: single-use depth-1 loop invariant `n` must stay in a GPR, got {:?}",
                    ra.loc_of[n.index()]
                );
                for (i, &inv) in invs.iter().enumerate() {
                    assert!(
                        matches!(ra.loc_of[inv.index()], AllocLoc::Gpr(_)),
                        "{label}: depth-1 invariant inv[{i}] must stay in a GPR, got {:?}",
                        ra.loc_of[inv.index()]
                    );
                }
                let ep_spills = [ep0, ep1, ep2]
                    .iter()
                    .filter(|&&ep| matches!(ra.loc_of[ep.index()], AllocLoc::GprSpill(_)))
                    .count();
                assert!(
                    ep_spills >= 2,
                    "{label}: expected cold 40-use depth-0 epilogue pointers to spill before depth-1 invariants, ep_spills={ep_spills}"
                );
            }
        }

        // 2. Back-edge phi argument coalesces with its loop header `Param` over a same-depth
        //    single-use temporary that appears earlier in program order (Phase 2 greedy coloring).
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let loop_hdr = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let tbl_ptr = b.param(Ty::PTR);
            let a0 = b.param(Ty::I64);
            let n = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let c7 = b.iconst(Ty::I64, 7);
            let c0 = b.iconst(Ty::I64, 0);
            let mut cold = Vec::with_capacity_in(25, alloc);
            let mut cur = a0;
            for _ in 0..25usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                cold.push(cur);
            }
            b.jump((loop_hdr, &[a0, c0]));

            b.begin(loop_hdr);
            let p0 = b.param(Ty::I64);
            let iv = b.param(Ty::I64);
            let u0 = b.binary(Op::Add, Flags::NONE, p0, n);
            let tmp = b.binary(Op::Add, Flags::NONE, u0, c7);
            let p0_next = b.load(Ty::I64, Flags::NONE, tbl_ptr, 0, MemAttrs::aligned(8));
            let iv_next = b.binary(Op::Add, Flags::NONE, iv, tmp);
            let cond = b.icmp(IntCond::Ult, iv_next, n);
            b.br(Flags::NONE, cond, (loop_hdr, &[p0_next, iv_next]), (exit, &[iv_next]));

            b.begin(exit);
            let mut out = b.param(Ty::I64);
            out = b.binary(Op::Xor, Flags::NONE, out, a0);
            for &cv in &cold {
                out = b.binary(Op::Xor, Flags::NONE, out, cv);
            }
            b.ret(&[out]);

            let func = b.finish_checked().expect("back-edge phi coalescing LIR builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            skip[c1.index()] = true;
            skip[c7.index()] = true;

            let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("exact-CFG back-edge phi regalloc succeeds");
            let ra_rpo = allocate_linear_scan_rpo(
                &view,
                &skip,
                &[],
                &tail,
                64,
                crate::Os::MacOs,
                |_| CallClobbers::C_ABI_ARM64,
                &gpr_ord,
                &fp_ord,
                alloc,
            )
            .expect("RPO linear-scan back-edge phi regalloc succeeds");

            for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
                assert!(
                    ra.num_spill_slots > 0,
                    "{label}: must trigger Phase 2 greedy coloring via cold depth-0 pressure"
                );
                assert!(
                    matches!(ra.loc_of[p0.index()], AllocLoc::Gpr(_)),
                    "{label}: loop header param `p0` must stay in a GPR, got {:?}",
                    ra.loc_of[p0.index()]
                );
                assert_eq!(
                    ra.loc_of[p0_next.index()],
                    ra.loc_of[p0.index()],
                    "{label}: back-edge phi arg `p0_next` must coalesce with loop param `p0` instead of losing to same-depth temp `tmp` (tmp={:?})",
                    ra.loc_of[tmp.index()]
                );
            }
        }
    }

    #[test]
    fn loop_param_skips_reg_busy_for_backedge_arg() {
        let alloc = test_alloc();
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let loop_hdr = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let tbl_ptr = b.param(Ty::PTR);
        let a0 = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let c1 = b.iconst(Ty::I64, 1);
        let c0 = b.iconst(Ty::I64, 0);
        // 25 cold depth-0 values live into `exit` so Phase 1 overflows and Phase 2 greedy coloring runs.
        let mut cold = Vec::with_capacity_in(25, alloc);
        let mut cur = a0;
        for _ in 0..25usize {
            cur = b.binary(Op::Add, Flags::NONE, cur, c1);
            cold.push(cur);
        }
        b.jump((loop_hdr, &[a0, c0]));

        b.begin(loop_hdr);
        let p = b.param(Ty::I64);
        let iv = b.param(Ty::I64);
        // Top of loop: `p` is read once and dies before `hot_tmp` is defined.
        let u0 = b.binary(Op::Add, Flags::NONE, p, iv);
        // Bottom of loop: `hot_tmp` does not interfere with `p`, has 5 depth-1 uses (colored
        // BEFORE `p` in Phase 2, taking `X9`), and stays live across `p_next` (interfering with `p_next`).
        let hot_tmp = b.binary(Op::Xor, Flags::NONE, u0, n);
        let p_next = b.load(Ty::I64, Flags::NONE, tbl_ptr, 0, MemAttrs::aligned(8));
        let h1 = b.binary(Op::Add, Flags::NONE, hot_tmp, hot_tmp);
        let h2 = b.binary(Op::Xor, Flags::NONE, h1, hot_tmp);
        let h3 = b.binary(Op::Add, Flags::NONE, h2, hot_tmp);
        let h4 = b.binary(Op::Xor, Flags::NONE, h3, hot_tmp);
        let iv_next = b.binary(Op::Add, Flags::NONE, iv, h4);
        let cond = b.icmp(IntCond::Ult, iv_next, n);
        b.br(Flags::NONE, cond, (loop_hdr, &[p_next, iv_next]), (exit, &[iv_next]));

        b.begin(exit);
        let mut out = b.param(Ty::I64);
        out = b.binary(Op::Xor, Flags::NONE, out, a0);
        for &cv in &cold {
            out = b.binary(Op::Xor, Flags::NONE, out, cv);
        }
        b.ret(&[out]);

        let func = b.finish_checked().expect("joint loop-param co-coloring LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        skip[c1.index()] = true;

        let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG joint loop-param regalloc succeeds");
        let ra_rpo = allocate_linear_scan_rpo(
            &view,
            &skip,
            &[],
            &tail,
            64,
            crate::Os::MacOs,
            |_| CallClobbers::C_ABI_ARM64,
            &gpr_ord,
            &fp_ord,
            alloc,
        )
        .expect("RPO linear-scan joint loop-param regalloc succeeds");

        for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
            assert!(
                ra.num_spill_slots > 0,
                "{label}: must trigger Phase 2 greedy coloring via cold depth-0 pressure"
            );
            assert_eq!(
                ra.loc_of[hot_tmp.index()],
                AllocLoc::Gpr(9),
                "{label}: highest-weight bottom-of-loop `hot_tmp` must claim X9"
            );
            assert_ne!(
                ra.loc_of[p.index()],
                AllocLoc::Gpr(9),
                "{label}: loop header `p` must skip X9 because X9 interferes with back-edge `p_next`"
            );
            assert!(
                matches!(ra.loc_of[p.index()], AllocLoc::Gpr(_)),
                "{label}: loop header `p` must stay in a GPR, got {:?}",
                ra.loc_of[p.index()]
            );
            assert_eq!(
                ra.loc_of[p.index()],
                ra.loc_of[p_next.index()],
                "{label}: loop header `p` and back-edge `p_next` must share one GPR (0 back-edge mov)"
            );
        }
    }
}

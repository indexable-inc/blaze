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

use crate::enc::{FP, Reg, X0, X1, X2, X8, X9, X10, X11, X12, X13, X14, X15};
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

/// Maximum parallel-copy pairs on one CFG edge tracked for 2-cycle swap cost modeling (Rule 18).
const MAX_PARALLEL_COPY_GROUP: usize = 16;

/// Maximum parallel-copy CFG edge groups tracked per function in `refine_gpr_moves` (Rule 18).
const MAX_EDGE_GROUPS: usize = 4096;

/// Safe upper ceiling for solved `BlockLayout` edge/block weights fed into `u64` cost sums.
const MAX_EDGE_WEIGHT: u64 = 1u64 << 44;

/// Priority bit marking back-edge phi copies so they strictly dominate all forward edges.
const BACK_EDGE_PRIO_BIT: u32 = 1u32 << 31;

/// Maximum forward-edge phi-coalescing priority (one below [`BACK_EDGE_PRIO_BIT`]).
const FWD_EDGE_PRIO_MAX: u32 = BACK_EDGE_PRIO_BIT - 1;

/// Combines two phi-copy edge priorities for the same `(u, v)` value pair, preserving
/// [`BACK_EDGE_PRIO_BIT`] and saturating the depth-weighted payload at [`FWD_EDGE_PRIO_MAX`].
#[inline]
fn merge_phi_prio(a: u32, b: u32) -> u32 {
    let back = (a | b) & BACK_EDGE_PRIO_BIT;
    let sum = (a & FWD_EDGE_PRIO_MAX)
        .saturating_add(b & FWD_EDGE_PRIO_MAX)
        .min(FWD_EDGE_PRIO_MAX);
    back | sum
}

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

/// Callee-saved allocatable GPRs for provably zero-call, non-unwinding, fixed-frame (`most_align <= 16`)
/// leaf functions: appends `X30` (`LR`) and `X29` (`FP`) at the very end of the pool so `FP` is used
/// ONLY once all 27 other GPRs are busy (`stp/ldp x29, x30` saves and restores both, stacked entry
/// args load via `[sp, #offset]`, and epilogue restores `sp` via `add sp, sp, #frame_size`).
const CALLEE_SAVED_GPR_WITH_LR_FP: &[Reg] = &[X19, X20, X21, X22, X23, X24, X25, X26, X27, X28, X30, FP];

/// Returns `true` iff `r` is tracked in `used_callee_gpr` (`X19..=X28`, `X30`, or `FP`).
#[inline]
fn is_tracked_callee_gpr(r: Reg) -> bool {
    (X19..=X28).contains(&r) || r == X30 || r == FP
}

/// Returns `true` iff `X29` (`FP`) may safely be allocated as the 28th callee-saved GPR.
///
/// Requires a provably zero-call, non-unwinding, fixed-frame (`most_align <= 16`) leaf function
/// with no inline assembly, coroutine suspension, or SME2 helper calls.
fn can_use_fp_as_gpr(view: &View<'_>, allow_lr: bool) -> bool {
    if !allow_lr {
        return false;
    }
    if view.slots.iter().any(|s| s.align > 4) {
        return false;
    }
    let blocks = view.blocks();
    for b in 0..blocks {
        if view.block_kind(Block(u32_of(b))).is_landing() {
            return false;
        }
    }
    let len = view.len();
    for i in 0..len {
        let op = view.op(i);
        if matches!(
            op,
            Op::Invoke
                | Op::InvokeIndirect
                | Op::Resume
                | Op::Asm
                | Op::AsmBr
                | Op::Suspend
        ) {
            return false;
        }
        if op == Op::TargetIntrinsic
            && view
                .data
                .get(i)
                .and_then(|&d| view.extra.get(idx(d)))
                .is_some_and(|&id| {
                    let name = view.intrinsic_name(blaze_lir::func::IntrinsicId(id));
                    name == b"blaze.sme2.gemm_f32"
                        || name == b"blaze.sme2.gemm_f64"
                        || name == b"blaze.sme2.gemm_i8_i32"
                        || name == b"blaze.sme2.gemm_u8_u32"
                })
        {
            return false;
        }
    }
    true
}

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
    /// `true` iff `X29` (`FP`) was allocated as a 28th GPR in a provably zero-call, fixed-frame
    /// leaf function (requires `stp/ldp x29, x30` frame save/restore, `SP`-relative stacked
    /// entry-arg loads, and `add sp, sp, #frame_size` epilogue instead of `FP`-based `SP` restore).
    pub(crate) uses_fp: bool,
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
        Op::Shl | Op::Lshr | Op::Ashr if bits == 64 || bits == 32 || bits == 16 || bits == 8 => {
            let (Some(&a), Some(&b)) = (view.extra.get(at), view.extra.get(at + 1)) else {
                return false;
            };
            idx(b) == vi && idx(a) != vi
        }
        Op::Rotl | Op::Rotr if bits == 64 || bits == 32 => {
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
        let limit = match op {
            Op::Memset => super::isel::SOTA_MEMSET_INLINE_LOOP_MOST,
            _ => super::isel::SOTA_BULK_INLINE_MOST,
        };
        let const_ok = view
            .data
            .get(inst)
            .and_then(|&d| view.extra.get(idx(d) + 2))
            .map(|&v| idx(v))
            .is_some_and(|len_v| extract_remat_const(view, len_v).is_some_and(|n| n <= limit));
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

/// Computes the full set of physical registers clobbered at a non-tail call site `inst`, unioning
/// the callee's IPRA [`CallClobbers`] with every register written by the caller's own ABI argument
/// setup (`X0..X(ng-1)`, `V0..V(nf-1)`, `X8` for `sret`, return registers, and `X16`/`X17`/`X30`).
/// Any non-`Op::Call` clobbering site (`Invoke`, `CallIndirect`, runtime `_memcpy`/`_memset`,
/// `TlsAddr`, `Frem`, `I128` div, SME2 helper) conservatively returns `CallClobbers::C_ABI_ARM64`.
fn effective_call_clobbers(view: &View<'_>, inst: usize, callee: CallClobbers) -> CallClobbers {
    if view.op(inst) != Op::Call {
        return CallClobbers::C_ABI_ARM64;
    }
    let mut clob = callee
        .clobbered
        .with(TierReg(16))
        .with(TierReg(17))
        .with(TierReg(30));
    let Some(&data_at) = view.data.get(inst) else {
        return CallClobbers::C_ABI_ARM64;
    };
    let Some(&sig_word) = view.extra.get(idx(data_at) + 1) else {
        return CallClobbers::C_ABI_ARM64;
    };
    let sig = view.sig(SigId(sig_word));
    let mut ng = 0u8;
    let mut nf = 0u8;
    for p in sig.params {
        match ParamKind::from_byte(p.kind) {
            Some(ParamKind::Sret) => {
                clob = clob.with(TierReg(8));
            }
            Some(ParamKind::Byval) => {
                clob = clob.with(TierReg(32 + 29)).with(TierReg(32 + 30));
            }
            _ => {
                let ty = Ty(p.ty);
                if is_fp_ty(ty).is_some() || is_vec128_ty(ty).is_some() {
                    if nf < 8 {
                        clob = clob.with(TierReg(32 + nf));
                        nf += 1;
                    }
                } else if ty == Ty::I128 || is_gpr_pair_ty(view, ty) {
                    if ng < 8 {
                        clob = clob.with(TierReg(ng));
                    }
                    if ng + 1 < 8 {
                        clob = clob.with(TierReg(ng + 1));
                    }
                    if ng + 2 < 8 {
                        // Conservative for 16B-aligned odd-GPR bump on AAPCS64 Linux.
                        clob = clob.with(TierReg(ng + 2));
                    }
                    ng = ng.saturating_add(2);
                } else if ng < 8 {
                    clob = clob.with(TierReg(ng));
                    ng += 1;
                }
            }
        }
    }
    for (ri, r) in sig.rets.iter().enumerate() {
        let rty = Ty(r.ty);
        if is_fp_ty(rty).is_some() || is_vec128_ty(rty).is_some() {
            if ri < 4 {
                clob = clob.with(TierReg(32 + u8::try_from(ri).unwrap_or(0)));
            }
        } else if rty == Ty::I128 || is_gpr_pair_ty(view, rty) {
            clob = clob.with(TierReg(0)).with(TierReg(1));
        } else if ri < 2 {
            clob = clob.with(TierReg(u8::try_from(ri).unwrap_or(0)));
        }
    }
    CallClobbers { clobbered: clob }
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
    let mut uses_fp = false;
    for &loc in &loc_of {
        match loc {
            AllocLoc::Gpr(r) => {
                if u32::from(r) == FP {
                    uses_fp = true;
                } else if !(19..=28).contains(&r) {
                    clob = clob.with(TierReg(r));
                }
            }
            AllocLoc::GprPair(lo, hi) => {
                if u32::from(lo) == FP {
                    uses_fp = true;
                } else if !(19..=28).contains(&lo) {
                    clob = clob.with(TierReg(lo));
                }
                if u32::from(hi) == FP {
                    uses_fp = true;
                } else if !(19..=28).contains(&hi) {
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
    debug_assert_eq!(uses_fp, used_callee_gpr.get(idx(FP)).copied().unwrap_or(false));
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
        clob = clob.union(effective_call_clobbers(view, c, call_clobbers(c)).clobbered);
    }
    for i in 0..view.len() {
        if matches!(view.op(i), Op::Memcpy | Op::Memmove | Op::Memset) {
            clob = clob.with(TierReg(32 + 29)).with(TierReg(32 + 30)).with(TierReg(32 + 31));
            break;
        }
    }

    Allocation {
        loc_of,
        x_pairs,
        d_pairs,
        num_spill_slots,
        has_calls: !call_positions.is_empty(),
        uses_lr: used_callee_gpr.get(idx(X30)).copied().unwrap_or(false),
        uses_fp,
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
        None,
        alloc,
    )
}

/// IPRA-aware register allocation with optional interprocedural register-avoidance scratch order
/// (`scratch_order`) and optional Pillar-2 `BlockLayout` solved edge weights (`layout`): callees
/// allocate caller-untouched GPR/FP registers before caller-hot ones.
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
    layout: Option<&super::layout::BlockLayout<'_>>,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    allocate_with_ipra_and_order_profile(
        view,
        skip_alloc,
        alias_of,
        tail_calls,
        max_spill_slots,
        os,
        call_clobbers,
        scratch_order,
        layout,
        false,
        alloc,
    )
}

/// Like [`allocate_with_ipra_and_order`], with explicit `fast_mode` selecting the `O(N log K)`
/// RPO linear-scan allocator directly (skipping the exact-CFG interference bitmatrix) for
/// [`super::EmitProfile::Fast`] cold compilation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn allocate_with_ipra_and_order_profile<A: Allocator + Copy>(
    view: &View<'_>,
    skip_alloc: &[bool],
    alias_of: &[u32],
    tail_calls: &[bool],
    max_spill_slots: u32,
    os: crate::Os,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    scratch_order: Option<&ScratchOrder>,
    layout: Option<&super::layout::BlockLayout<'_>>,
    fast_mode: bool,
    alloc: A,
) -> Result<Allocation<A>, RegAllocError> {
    let caller_gpr_order = build_caller_gpr_order(scratch_order);
    let caller_fp_order = build_caller_fp_order(scratch_order);
    if !fast_mode && fits_exact_cfg(view, skip_alloc, alloc) {
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
            layout,
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

/// Picks the first GPR satisfying `ok` from `caller_gpr_order` and `callee_gpr_pool`.
///
/// When `avoid_mask` is non-zero (anti-hinting against registers wanted by interfering uncolored
/// `phi_adj` values), prefers registers outside `avoid_mask` within each cost tier:
/// 1. Non-avoided caller-saved GPRs, then non-avoided already-saved callee-saved GPRs
/// 2. Avoided caller-saved GPRs, then avoided already-saved callee-saved GPRs
/// 3. Non-avoided fresh callee-saved GPRs
/// 4. Avoided fresh callee-saved GPRs
///
/// This guarantees zero extra callee-saved prologue/epilogue pairs and zero extra spills.
#[inline]
fn pick_gpr_from_pool(
    caller_gpr_order: &[Reg; 16],
    callee_gpr_pool: &[Reg],
    used_callee_gpr: &[bool; 32],
    avoid_mask: u32,
    mut ok: impl FnMut(Reg) -> bool,
) -> Option<Reg> {
    if avoid_mask == 0 {
        return caller_gpr_order
            .iter()
            .copied()
            .chain(callee_gpr_pool.iter().copied())
            .find(|&r| ok(r));
    }
    let is_avoided = |r: Reg| -> bool { r < 32 && (avoid_mask & (1u32 << r)) != 0 };
    let is_paid_callee = |r: Reg| -> bool {
        if used_callee_gpr.get(idx(r)).copied().unwrap_or(false) {
            return true;
        }
        if (X19..=X28).contains(&r) {
            let mate = if (r & 1) != 0 { r + 1 } else { r - 1 };
            return used_callee_gpr.get(idx(mate)).copied().unwrap_or(false);
        }
        false
    };
    if let Some(r) = caller_gpr_order
        .iter()
        .copied()
        .find(|&r| !is_avoided(r) && ok(r))
    {
        return Some(r);
    }
    if let Some(r) = callee_gpr_pool
        .iter()
        .copied()
        .find(|&r| is_paid_callee(r) && !is_avoided(r) && ok(r))
    {
        return Some(r);
    }
    if let Some(r) = caller_gpr_order
        .iter()
        .copied()
        .find(|&r| is_avoided(r) && ok(r))
    {
        return Some(r);
    }
    if let Some(r) = callee_gpr_pool
        .iter()
        .copied()
        .find(|&r| is_paid_callee(r) && is_avoided(r) && ok(r))
    {
        return Some(r);
    }
    if let Some(r) = callee_gpr_pool
        .iter()
        .copied()
        .find(|&r| !is_paid_callee(r) && !is_avoided(r) && ok(r))
    {
        return Some(r);
    }
    callee_gpr_pool
        .iter()
        .copied()
        .find(|&r| !is_paid_callee(r) && is_avoided(r) && ok(r))
}

/// Post-coloring bounded GPR move-cost refinement (single-value recolor + 2-cycle pair-swap +
/// bounded colliding edge-group pairwise color-swap, with zero-affinity scratch-blocker eviction
/// and parallel-copy 2-cycle swap cost modeling).
///
/// Runs at most `MAX_REFINE_ROUNDS` rounds on functions with `<= MAX_REFINE_GPR_VALS` scalar GPR
/// values. Every committed move, eviction, or swap is verified bit-for-bit against the exact
/// symmetric interference matrix (`interf`) and `gpr_busy`, call-clobber safety
/// (`val_crosses_call` / `val_crossed_clobbers`), non-expansion of callee-saved pairs, and strict
/// reduction in total weighted phi + parallel-copy 2-cycle swap + ABI move cost.
/// Returns `true` iff at least one GPR assignment changed.
#[allow(clippy::too_many_arguments)]
fn refine_gpr_moves<A: Allocator + Copy>(
    view: &View<'_>,
    alias_of: &[u32],
    os: crate::Os,
    val_id: &[u32],
    val_inst: &[usize],
    val_words: usize,
    interf: &[u64],
    val_crosses_call: &[bool],
    val_crossed_clobbers: &[CallClobbers],
    coalesce_edges: &[(u32, u32, u64)],
    edge_groups: &[(u32, u32)],
    abi_weight: u64,
    allow_lr: bool,
    allow_fp: bool,
    callee_gpr_pool: &[Reg],
    loc: &mut [AllocLoc],
    used_callee_gpr: &mut [bool; 32],
    alloc: A,
) -> bool {
    const MAX_REFINE_ROUNDS: usize = 4;
    const MAX_REFINE_GPR_VALS: usize = 2048;
    const MAX_SCRATCH_EVICTS: usize = 4;
    const MAX_BLOCKERS: usize = 1 + MAX_SCRATCH_EVICTS;
    const MAX_TOTAL_SCRATCH_EVICTS: usize = MAX_SCRATCH_EVICTS * 2;
    const MAX_ACTION_MOVES: usize = 2 + MAX_TOTAL_SCRATCH_EVICTS;
    const MAX_ACTION_GROUPS: usize = 64;
    const MAX_PAIR_SWAP_RAW: usize = 2048;
    const MAX_PAIR_SWAP_CANDS: usize = 256;

    if val_words == 0 || coalesce_edges.is_empty() {
        return false;
    }

    let len = view.len();
    let mut num_gpr_vals = 0usize;
    for &l in loc.iter() {
        if matches!(l, AllocLoc::Gpr(_)) {
            num_gpr_vals += 1;
            if num_gpr_vals > MAX_REFINE_GPR_VALS {
                return false;
            }
        }
    }
    if num_gpr_vals == 0 {
        return false;
    }

    // Build symmetric weighted scalar-GPR coalesce adjacency CSR (`w_off`, `w_adj`).
    let mut w_raw: Vec<(u32, u32, u64), A> =
        Vec::with_capacity_in(coalesce_edges.len().saturating_mul(2), alloc);
    for &(a, p, weight) in coalesce_edges {
        if a == p || weight == 0 {
            continue;
        }
        let ai = idx(a);
        let pi = idx(p);
        let a_ty = view.tys.get(ai).copied().map_or(Ty::NONE, Ty);
        let p_ty = view.tys.get(pi).copied().map_or(Ty::NONE, Ty);
        if reg_bank(view, a_ty) == 3 && reg_bank(view, p_ty) == 3 {
            w_raw.push((a, p, weight));
            w_raw.push((p, a, weight));
        }
    }
    if w_raw.is_empty() {
        return false;
    }
    w_raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut w_dedup: Vec<(u32, u32, u64), A> = Vec::with_capacity_in(w_raw.len(), alloc);
    for &(u, v, weight) in &w_raw {
        if let Some(last) = w_dedup.last_mut()
            && last.0 == u
            && last.1 == v
        {
            last.2 = last.2.saturating_add(weight);
            continue;
        }
        w_dedup.push((u, v, weight));
    }
    let mut w_off: Vec<u32, A> = Vec::with_capacity_in(len + 1, alloc);
    let mut w_adj: Vec<(u32, u64), A> = Vec::with_capacity_in(w_dedup.len(), alloc);
    let mut edge_nodes: Vec<usize, A> = Vec::new_in(alloc);
    let mut cur = 0usize;
    for u in 0..len {
        let start_len = w_adj.len();
        w_off.push(u32_of(start_len));
        let u_u32 = u32_of(u);
        while let Some(&(src, dst, weight)) = w_dedup.get(cur) {
            if src != u_u32 {
                break;
            }
            w_adj.push((dst, weight));
            cur += 1;
        }
        if w_adj.len() > start_len && matches!(loc.get(u).copied(), Some(AllocLoc::Gpr(_))) {
            edge_nodes.push(u);
        }
    }
    w_off.push(u32_of(w_adj.len()));
    if edge_nodes.is_empty() {
        return false;
    }

    // Build bounded per-value CSR (`vg_off`, `vg_adj`) mapping each SSA instruction to the
    // parallel-copy CFG edge groups (`valid_groups`) in which it participates, sorted hottest-first
    // so both 2-cycle swap penalties (`reg[a_i] == reg[p_j] && reg[a_j] == reg[p_i]`, costing `+1`
    // extra `X16` `mov`) and bounded pairwise edge-group swaps prioritize hot inner loops.
    let mut valid_groups: Vec<(u32, u32, u64), A> = Vec::new_in(alloc);
    for &(s_u32, e_u32) in edge_groups {
        if valid_groups.len() >= MAX_EDGE_GROUPS {
            break;
        }
        let si = idx(s_u32);
        let ei = idx(e_u32);
        if ei <= si || ei > coalesce_edges.len() {
            continue;
        }
        let group_len = ei - si;
        if !(2..=MAX_PARALLEL_COPY_GROUP).contains(&group_len) {
            continue;
        }
        let Some(pairs) = coalesce_edges.get(si..ei) else {
            continue;
        };
        let mut gpr_pairs = 0usize;
        let mut max_w = 0u64;
        for &(a, p, weight) in pairs {
            if a == p || weight == 0 {
                continue;
            }
            let a_ty = view.tys.get(idx(a)).copied().map_or(Ty::NONE, Ty);
            let p_ty = view.tys.get(idx(p)).copied().map_or(Ty::NONE, Ty);
            if reg_bank(view, a_ty) == 3 && reg_bank(view, p_ty) == 3 {
                gpr_pairs += 1;
                if weight > max_w {
                    max_w = weight;
                }
            }
        }
        if gpr_pairs >= 2 {
            valid_groups.push((s_u32, e_u32, max_w));
        }
    }
    valid_groups.sort_unstable_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    let (vg_off, vg_adj): (Vec<u32, A>, Vec<u32, A>) = if valid_groups.is_empty() {
        (Vec::new_in(alloc), Vec::new_in(alloc))
    } else {
        let mut vg_raw: Vec<(u32, u32), A> =
            Vec::with_capacity_in(valid_groups.len().saturating_mul(4), alloc);
        for (gid, &(s_u32, e_u32, _)) in valid_groups.iter().enumerate() {
            let gid_u32 = u32_of(gid);
            if let Some(pairs) = coalesce_edges.get(idx(s_u32)..idx(e_u32)) {
                for &(a, p, weight) in pairs {
                    if a == p || weight == 0 {
                        continue;
                    }
                    let a_ty = view.tys.get(idx(a)).copied().map_or(Ty::NONE, Ty);
                    let p_ty = view.tys.get(idx(p)).copied().map_or(Ty::NONE, Ty);
                    if reg_bank(view, a_ty) == 3 && reg_bank(view, p_ty) == 3 {
                        vg_raw.push((a, gid_u32));
                        vg_raw.push((p, gid_u32));
                    }
                }
            }
        }
        vg_raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut off: Vec<u32, A> = Vec::with_capacity_in(len + 1, alloc);
        let mut adj: Vec<u32, A> = Vec::with_capacity_in(vg_raw.len(), alloc);
        let mut g_cur = 0usize;
        for u in 0..len {
            off.push(u32_of(adj.len()));
            let u_u32 = u32_of(u);
            let mut last_gid = u32::MAX;
            while let Some(&(inst_u, gid)) = vg_raw.get(g_cur) {
                if inst_u != u_u32 {
                    break;
                }
                if gid != last_gid {
                    adj.push(gid);
                    last_gid = gid;
                }
                g_cur += 1;
            }
        }
        off.push(u32_of(adj.len()));
        (off, adj)
    };

    // Direct entry/landing-pad/return ABI register hints (`abi_weight` each) so depth-0 moves
    // never pointlessly shift a prologue or epilogue copy without strict net gain.
    let mut abi_reg: Vec<u8, A> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        abi_reg.push(NO_REG);
    }
    seed_abi_and_return_hints(view, alias_of, os, &mut abi_reg);

    // Reconstruct exact `gpr_busy` bitmatrix from `loc` (covering both `Gpr` and `GprPair`).
    let busy_len = 32usize.saturating_mul(val_words);
    let mut gpr_busy: Vec<u64, A> = Vec::with_capacity_in(busy_len, alloc);
    for _ in 0..busy_len {
        gpr_busy.push(0);
    }
    for (v_vid, &v_inst) in val_inst.iter().enumerate() {
        let bit = 1u64 << (v_vid & 63);
        let word = v_vid >> 6;
        match loc.get(v_inst).copied().unwrap_or(AllocLoc::None) {
            AllocLoc::Gpr(r) if usize::from(r) < 32 => {
                if let Some(slot) = gpr_busy.get_mut(usize::from(r).saturating_mul(val_words) + word)
                {
                    *slot |= bit;
                }
            }
            AllocLoc::GprPair(lo, hi) => {
                if usize::from(lo) < 32
                    && let Some(slot) =
                        gpr_busy.get_mut(usize::from(lo).saturating_mul(val_words) + word)
                {
                    *slot |= bit;
                }
                if usize::from(hi) < 32
                    && let Some(slot) =
                        gpr_busy.get_mut(usize::from(hi).saturating_mul(val_words) + word)
                {
                    *slot |= bit;
                }
            }
            _ => {}
        }
    }

    let mut caller_saved_mask = 0u32;
    for &r in CALLER_SAVED_GPR {
        if r < 32 {
            caller_saved_mask |= 1u32 << r;
        }
    }
    let mut allocatable_mask = caller_saved_mask;
    for &r in callee_gpr_pool {
        if r < 32 {
            allocatable_mask |= 1u32 << r;
        }
    }

    let is_paid_callee_reg = |r: u8, used: &[bool; 32]| -> bool {
        let ri = usize::from(r);
        if used.get(ri).copied().unwrap_or(false) {
            return true;
        }
        if (19..=28).contains(&r) {
            let mate = if (r & 1) != 0 { ri + 1 } else { ri - 1 };
            return used.get(mate).copied().unwrap_or(false);
        }
        false
    };

    let is_paid_reg = |r: u8, used: &[bool; 32]| -> bool {
        r < 32
            && ((caller_saved_mask & (1u32 << u32::from(r))) != 0
                || is_paid_callee_reg(r, used))
    };

    let gpr_reg_safe = |u_inst: usize, r: u8| -> bool {
        if r >= 32 || (allocatable_mask & (1u32 << u32::from(r))) == 0 {
            return false;
        }
        if u32::from(r) == X30 && !allow_lr {
            return false;
        }
        if u32::from(r) == FP && !allow_fp {
            return false;
        }
        if !val_crosses_call.get(u_inst).copied().unwrap_or(false) {
            return true;
        }
        if (19..=28).contains(&r) {
            return true;
        }
        let clob = val_crossed_clobbers
            .get(u_inst)
            .copied()
            .unwrap_or(CallClobbers::EMPTY);
        !clob.clobbers_gpr(r)
    };

    let is_zero_affinity_scratch =
        |loc_cur: &[AllocLoc], inst_idx: usize, expected_reg: u8| -> bool {
            if loc_cur.get(inst_idx).copied() != Some(AllocLoc::Gpr(expected_reg)) {
                return false;
            }
            let start = w_off.get(inst_idx).copied().map_or(0, idx);
            let end = w_off.get(inst_idx + 1).copied().map_or(start, idx);
            start == end && abi_reg.get(inst_idx).copied().unwrap_or(NO_REG) == NO_REG
        };

    let set_busy = |busy: &mut [u64], reg: u8, vid: usize| {
        let off = usize::from(reg).saturating_mul(val_words) + (vid >> 6);
        if let Some(slot) = busy.get_mut(off) {
            *slot |= 1u64 << (vid & 63);
        }
    };

    let clear_busy = |busy: &mut [u64], reg: u8, vid: usize| {
        let off = usize::from(reg).saturating_mul(val_words) + (vid >> 6);
        if let Some(slot) = busy.get_mut(off) {
            *slot &= !(1u64 << (vid & 63));
        }
    };

    let is_free_in_busy = |busy: &[u64], reg: u8, vid: usize| -> bool {
        let r_base = usize::from(reg).saturating_mul(val_words);
        let v_base = vid.saturating_mul(val_words);
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

    let find_evict_reg = |busy: &[u64],
                          w_inst: usize,
                          w_vid: usize,
                          forbid_a: u8,
                          forbid_b: u8,
                          used: &[bool; 32]|
     -> Option<u8> {
        for &r_raw in CALLER_SAVED_GPR {
            let r = reg_u8(r_raw);
            if r == forbid_a || r == forbid_b {
                continue;
            }
            if gpr_reg_safe(w_inst, r) && is_free_in_busy(busy, r, w_vid) {
                return Some(r);
            }
        }
        for &r_raw in callee_gpr_pool {
            let r = reg_u8(r_raw);
            if r == forbid_a || r == forbid_b {
                continue;
            }
            if is_paid_callee_reg(r, used)
                && gpr_reg_safe(w_inst, r)
                && is_free_in_busy(busy, r, w_vid)
            {
                return Some(r);
            }
        }
        None
    };

    let node_move_cost = |loc_cur: &[AllocLoc], u_inst: usize, r: u8, skip_nb: usize| -> u64 {
        let start = w_off.get(u_inst).copied().map_or(0, idx);
        let end = w_off.get(u_inst + 1).copied().map_or(start, idx);
        let mut cost = 0u64;
        if let Some(edges) = w_adj.get(start..end) {
            for &(nb_u32, w) in edges {
                let nb = idx(nb_u32);
                if nb == skip_nb {
                    continue;
                }
                if loc_cur.get(nb).copied() != Some(AllocLoc::Gpr(r)) {
                    cost = cost.saturating_add(w);
                }
            }
        }
        let abi = abi_reg.get(u_inst).copied().unwrap_or(NO_REG);
        if abi != NO_REG && abi != r {
            cost = cost.saturating_add(abi_weight);
        }
        cost
    };

    // Exact combined weighted coalesce + ABI move cost for a pair `(u_inst, v_inst)` evaluated
    // directly against `loc_cur` without double-counting any direct `(u_inst, v_inst)` edge.
    let pair_edge_and_abi_cost = |loc_cur: &[AllocLoc], u_inst: usize, v_inst: usize| -> u64 {
        let mut cost = 0u64;
        let u_loc = loc_cur.get(u_inst).copied().unwrap_or(AllocLoc::None);
        let v_loc = loc_cur.get(v_inst).copied().unwrap_or(AllocLoc::None);
        let u_start = w_off.get(u_inst).copied().map_or(0, idx);
        let u_end = w_off.get(u_inst + 1).copied().map_or(u_start, idx);
        if let Some(edges) = w_adj.get(u_start..u_end) {
            for &(nb_u32, w) in edges {
                if loc_cur.get(idx(nb_u32)).copied().unwrap_or(AllocLoc::None) != u_loc {
                    cost = cost.saturating_add(w);
                }
            }
        }
        let v_start = w_off.get(v_inst).copied().map_or(0, idx);
        let v_end = w_off.get(v_inst + 1).copied().map_or(v_start, idx);
        if let Some(edges) = w_adj.get(v_start..v_end) {
            for &(nb_u32, w) in edges {
                let nb = idx(nb_u32);
                if nb == u_inst {
                    continue;
                }
                if loc_cur.get(nb).copied().unwrap_or(AllocLoc::None) != v_loc {
                    cost = cost.saturating_add(w);
                }
            }
        }
        let u_abi = abi_reg.get(u_inst).copied().unwrap_or(NO_REG);
        if u_abi != NO_REG && u_loc != AllocLoc::Gpr(u_abi) {
            cost = cost.saturating_add(abi_weight);
        }
        let v_abi = abi_reg.get(v_inst).copied().unwrap_or(NO_REG);
        if v_abi != NO_REG && v_loc != AllocLoc::Gpr(v_abi) {
            cost = cost.saturating_add(abi_weight);
        }
        cost
    };

    // Parallel-copy 2-cycle swap penalty for one CFG edge group (`gid`), counting `+w` per
    // swapping pair `(a_i -> p_i, a_j -> p_j)` where `reg[a_i] == reg[p_j]` and
    // `reg[a_j] == reg[p_i]` with `reg[a_i] != reg[p_i]` (1 extra `X16` temp `mov`).
    let group_swap_penalty = |gid: u32,
                              loc_cur: &[AllocLoc],
                              ov1_inst: usize,
                              ov1_reg: u8,
                              ov2_inst: usize,
                              ov2_reg: u8|
     -> u64 {
        let Some(&(s_u32, e_u32, _)) = valid_groups.get(idx(gid)) else {
            return 0;
        };
        let Some(pairs) = coalesce_edges.get(idx(s_u32)..idx(e_u32)) else {
            return 0;
        };
        let reg_of = |inst_u32: u32| -> u8 {
            let i = idx(inst_u32);
            if i == ov1_inst {
                return ov1_reg;
            }
            if i == ov2_inst {
                return ov2_reg;
            }
            match loc_cur.get(i).copied() {
                Some(AllocLoc::Gpr(r)) => r,
                _ => NO_REG,
            }
        };
        let mut penalty = 0u64;
        for (i, &(a_i, p_i, w_i)) in pairs.iter().enumerate() {
            if a_i == p_i || w_i == 0 {
                continue;
            }
            let ra_i = reg_of(a_i);
            let rp_i = reg_of(p_i);
            if ra_i == NO_REG || rp_i == NO_REG || ra_i == rp_i {
                continue;
            }
            for &(a_j, p_j, w_j) in pairs.iter().skip(i + 1) {
                if a_j == p_j || w_j == 0 {
                    continue;
                }
                let ra_j = reg_of(a_j);
                let rp_j = reg_of(p_j);
                if ra_j == NO_REG || rp_j == NO_REG || ra_j == rp_j {
                    continue;
                }
                if ra_i == rp_j && ra_j == rp_i {
                    penalty = penalty.saturating_add(w_i.min(w_j));
                }
            }
        }
        penalty
    };

    let eval_single_swap = |loc_cur: &[AllocLoc], u_inst: usize, new_r: u8| -> u64 {
        let g_start = vg_off.get(u_inst).copied().map_or(0, idx);
        let g_end = vg_off.get(u_inst + 1).copied().map_or(g_start, idx);
        let Some(gids) = vg_adj.get(g_start..g_end) else {
            return 0;
        };
        let mut total = 0u64;
        for &gid in gids.iter().take(MAX_ACTION_GROUPS) {
            total = total.saturating_add(group_swap_penalty(
                gid,
                loc_cur,
                u_inst,
                new_r,
                usize::MAX,
                NO_REG,
            ));
        }
        total
    };

    // Exact 2-pointer union over sorted incident groups of `u_inst` and `v_inst` (zero
    // double-counting when both values share the same parallel-copy CFG edge).
    let eval_pair_swap = |loc_cur: &[AllocLoc],
                          u_inst: usize,
                          u_new_r: u8,
                          v_inst: usize,
                          v_new_r: u8|
     -> (u64, u64) {
        let us = vg_off.get(u_inst).copied().map_or(0, idx);
        let ue = vg_off.get(u_inst + 1).copied().map_or(us, idx);
        let vs = vg_off.get(v_inst).copied().map_or(0, idx);
        let ve = vg_off.get(v_inst + 1).copied().map_or(vs, idx);
        let ug = vg_adj.get(us..ue).unwrap_or(&[]);
        let vg = vg_adj.get(vs..ve).unwrap_or(&[]);
        if ug.is_empty() && vg.is_empty() {
            return (0, 0);
        }
        let mut i = 0usize;
        let mut j = 0usize;
        let mut checked = 0usize;
        let mut before = 0u64;
        let mut after = 0u64;
        while (i < ug.len() || j < vg.len()) && checked < MAX_ACTION_GROUPS {
            let gid = match (ug.get(i).copied(), vg.get(j).copied()) {
                (Some(a), Some(b)) if a < b => {
                    i += 1;
                    a
                }
                (Some(a), Some(b)) if b < a => {
                    j += 1;
                    b
                }
                (Some(a), Some(_)) => {
                    i += 1;
                    j += 1;
                    a
                }
                (Some(a), None) => {
                    i += 1;
                    a
                }
                (None, Some(b)) => {
                    j += 1;
                    b
                }
                (None, None) => break,
            };
            before = before.saturating_add(group_swap_penalty(
                gid,
                loc_cur,
                usize::MAX,
                NO_REG,
                usize::MAX,
                NO_REG,
            ));
            after = after.saturating_add(group_swap_penalty(
                gid,
                loc_cur,
                u_inst,
                u_new_r,
                v_inst,
                v_new_r,
            ));
            checked += 1;
        }
        (before, after)
    };

    #[derive(Clone, Copy)]
    struct GprMove {
        inst: usize,
        vid: usize,
        from: u8,
        to: u8,
    }

    impl GprMove {
        const EMPTY: Self = Self {
            inst: 0,
            vid: 0,
            from: NO_REG,
            to: NO_REG,
        };
    }

    #[derive(Clone, Copy)]
    struct RefineAction {
        moves: [GprMove; MAX_ACTION_MOVES],
        len: usize,
    }

    impl RefineAction {
        const EMPTY: Self = Self {
            moves: [GprMove::EMPTY; MAX_ACTION_MOVES],
            len: 0,
        };

        #[inline]
        fn push(&mut self, mv: GprMove) {
            if let Some(slot) = self.moves.get_mut(self.len) {
                *slot = mv;
                self.len += 1;
            }
        }
    }

    let mut pair_raw: Vec<(u32, u32, u64), A> = if valid_groups.is_empty() {
        Vec::new_in(alloc)
    } else {
        Vec::with_capacity_in(valid_groups.len().saturating_mul(4).min(MAX_PAIR_SWAP_RAW), alloc)
    };
    let mut pair_cands: Vec<(u32, u32, u64), A> = if valid_groups.is_empty() {
        Vec::new_in(alloc)
    } else {
        Vec::with_capacity_in(MAX_PAIR_SWAP_CANDS, alloc)
    };

    let push_pair_cand =
        |raw: &mut Vec<(u32, u32, u64), A>, x: u32, y: u32, w: u64| {
            if x != y && raw.len() < MAX_PAIR_SWAP_RAW {
                let (lo, hi) = if x < y { (x, y) } else { (y, x) };
                raw.push((lo, hi, w));
            }
        };

    let mut any_changed = false;
    for _round in 0..MAX_REFINE_ROUNDS {
        let mut round_changed = false;
        for &u_inst in &edge_nodes {
            let Some(AllocLoc::Gpr(r_u)) = loc.get(u_inst).copied() else {
                continue;
            };
            if r_u >= 32 {
                continue;
            }
            let u_vid_raw = val_id.get(u_inst).copied().unwrap_or(NO_VAL);
            if u_vid_raw == NO_VAL {
                continue;
            }
            let u_vid = idx(u_vid_raw);
            let start = w_off.get(u_inst).copied().map_or(0, idx);
            let end = w_off.get(u_inst + 1).copied().map_or(start, idx);
            let mut cand_mask = 0u32;
            let mut has_uncoalesced = false;
            if let Some(edges) = w_adj.get(start..end) {
                for &(nb_u32, _w) in edges {
                    if let Some(AllocLoc::Gpr(nr)) = loc.get(idx(nb_u32)).copied() {
                        if nr != r_u && nr < 32 {
                            has_uncoalesced = true;
                            cand_mask |= 1u32 << u32::from(nr);
                        }
                    }
                }
            }
            if !has_uncoalesced {
                continue;
            }
            let u_abi = abi_reg.get(u_inst).copied().unwrap_or(NO_REG);
            if u_abi != NO_REG && u_abi != r_u && u_abi < 32 {
                cand_mask |= 1u32 << u32::from(u_abi);
            }

            let mut best_gain = 0u64;
            let mut best_action: Option<RefineAction> = None;
            let u_swap_before = eval_single_swap(loc, u_inst, r_u);
            let u_single_before =
                node_move_cost(loc, u_inst, r_u, usize::MAX).saturating_add(u_swap_before);
            let u_base = u_vid.saturating_mul(val_words);
            let u_word = u_vid >> 6;
            let u_bit = 1u64 << (u_vid & 63);
            let ru_base = usize::from(r_u).saturating_mul(val_words);

            let mut rem_mask = cand_mask;
            while rem_mask != 0 {
                let r_cand_u32 = rem_mask.trailing_zeros();
                rem_mask &= rem_mask - 1;
                let r_cand = reg_u8(r_cand_u32);
                if r_cand == r_u || !gpr_reg_safe(u_inst, r_cand) {
                    continue;
                }
                let rc_base = usize::from(r_cand).saturating_mul(val_words);

                // Collect interfering values currently holding `r_cand` (up to `MAX_BLOCKERS`).
                let mut rc_blockers = [0usize; MAX_BLOCKERS];
                let mut rc_blocker_count = 0usize;
                let mut rc_too_many = false;
                for w in 0..val_words {
                    let mut bits = gpr_busy.get(rc_base + w).copied().unwrap_or(0)
                        & interf.get(u_base + w).copied().unwrap_or(0);
                    while bits != 0 {
                        if rc_blocker_count >= MAX_BLOCKERS {
                            rc_too_many = true;
                            break;
                        }
                        let b_vid = w.saturating_mul(64) + idx(bits.trailing_zeros());
                        bits &= bits - 1;
                        if let Some(slot) = rc_blockers.get_mut(rc_blocker_count) {
                            *slot = b_vid;
                        }
                        rc_blocker_count += 1;
                    }
                    if rc_too_many {
                        break;
                    }
                }
                if rc_too_many {
                    continue;
                }

                if rc_blocker_count == 0 {
                    // Case A: single-value move — never expand callee-saved pairs or `uses_lr`.
                    if !is_paid_reg(r_cand, used_callee_gpr) {
                        continue;
                    }
                    let swap_after = eval_single_swap(loc, u_inst, r_cand);
                    let after =
                        node_move_cost(loc, u_inst, r_cand, usize::MAX).saturating_add(swap_after);
                    let gain = u_single_before.saturating_sub(after);
                    if gain > best_gain {
                        let mut act = RefineAction::EMPTY;
                        act.push(GprMove {
                            inst: u_inst,
                            vid: u_vid,
                            from: r_u,
                            to: r_cand,
                        });
                        best_gain = gain;
                        best_action = Some(act);
                    }
                    continue;
                }

                // Classify `r_cand` blockers into at most 1 non-scratch scalar GPR `v` and up to
                // `MAX_SCRATCH_EVICTS` zero-affinity scratch temporaries.
                let mut non_scratch: Option<(usize, usize)> = None;
                let mut rc_scratch = [GprMove::EMPTY; MAX_SCRATCH_EVICTS];
                let mut rc_scratch_count = 0usize;
                let mut blockers_ok = true;
                for &b_vid in rc_blockers.iter().take(rc_blocker_count) {
                    let Some(&b_inst) = val_inst.get(b_vid) else {
                        blockers_ok = false;
                        break;
                    };
                    if b_inst == u_inst {
                        blockers_ok = false;
                        break;
                    }
                    if is_zero_affinity_scratch(loc, b_inst, r_cand) {
                        if rc_scratch_count >= MAX_SCRATCH_EVICTS {
                            blockers_ok = false;
                            break;
                        }
                        if let Some(slot) = rc_scratch.get_mut(rc_scratch_count) {
                            *slot = GprMove {
                                inst: b_inst,
                                vid: b_vid,
                                from: r_cand,
                                to: NO_REG,
                            };
                        }
                        rc_scratch_count += 1;
                    } else if non_scratch.is_none()
                        && loc.get(b_inst).copied() == Some(AllocLoc::Gpr(r_cand))
                    {
                        non_scratch = Some((b_inst, b_vid));
                    } else {
                        blockers_ok = false;
                        break;
                    }
                }
                if !blockers_ok {
                    continue;
                }

                if let Some((v_inst, v_vid)) = non_scratch {
                    // Case C: 2-cycle pair-swap `(u, r_u) <-> (v, r_cand)` with optional eviction
                    // of up to 2 zero-affinity scratch blockers on `r_cand` and up to 2 on `r_u`.
                    if !gpr_reg_safe(v_inst, r_u) {
                        continue;
                    }
                    let (swap_before, swap_after) =
                        eval_pair_swap(loc, u_inst, r_cand, v_inst, r_u);
                    let before = node_move_cost(loc, u_inst, r_u, v_inst)
                        .saturating_add(node_move_cost(loc, v_inst, r_cand, u_inst))
                        .saturating_add(swap_before);
                    let after = node_move_cost(loc, u_inst, r_cand, v_inst)
                        .saturating_add(node_move_cost(loc, v_inst, r_u, u_inst))
                        .saturating_add(swap_after);
                    let gain = before.saturating_sub(after);
                    if gain <= best_gain {
                        continue;
                    }

                    // Collect interfering blockers of `v` in `r_u` (excluding `u`).
                    let v_base = v_vid.saturating_mul(val_words);
                    let mut ru_blockers = [0usize; MAX_SCRATCH_EVICTS];
                    let mut ru_blocker_count = 0usize;
                    let mut ru_too_many = false;
                    for w in 0..val_words {
                        let mut bw = gpr_busy.get(ru_base + w).copied().unwrap_or(0);
                        if w == u_word {
                            bw &= !u_bit;
                        }
                        let mut bits = bw & interf.get(v_base + w).copied().unwrap_or(0);
                        while bits != 0 {
                            if ru_blocker_count >= MAX_SCRATCH_EVICTS {
                                ru_too_many = true;
                                break;
                            }
                            let b_vid = w.saturating_mul(64) + idx(bits.trailing_zeros());
                            bits &= bits - 1;
                            if let Some(slot) = ru_blockers.get_mut(ru_blocker_count) {
                                *slot = b_vid;
                            }
                            ru_blocker_count += 1;
                        }
                        if ru_too_many {
                            break;
                        }
                    }
                    if ru_too_many {
                        continue;
                    }

                    let mut evicts = [GprMove::EMPTY; MAX_TOTAL_SCRATCH_EVICTS];
                    let mut evict_count = 0usize;
                    for &mv in rc_scratch.iter().take(rc_scratch_count) {
                        if let Some(slot) = evicts.get_mut(evict_count) {
                            *slot = mv;
                        }
                        evict_count += 1;
                    }
                    let mut ru_ok = true;
                    for &b_vid in ru_blockers.iter().take(ru_blocker_count) {
                        let Some(&b_inst) = val_inst.get(b_vid) else {
                            ru_ok = false;
                            break;
                        };
                        if b_inst == u_inst
                            || b_inst == v_inst
                            || !is_zero_affinity_scratch(loc, b_inst, r_u)
                        {
                            ru_ok = false;
                            break;
                        }
                        if let Some(slot) = evicts.get_mut(evict_count) {
                            *slot = GprMove {
                                inst: b_inst,
                                vid: b_vid,
                                from: r_u,
                                to: NO_REG,
                            };
                        }
                        evict_count += 1;
                    }
                    if !ru_ok {
                        continue;
                    }

                    if evict_count == 0 {
                        let mut act = RefineAction::EMPTY;
                        act.push(GprMove {
                            inst: u_inst,
                            vid: u_vid,
                            from: r_u,
                            to: r_cand,
                        });
                        act.push(GprMove {
                            inst: v_inst,
                            vid: v_vid,
                            from: r_cand,
                            to: r_u,
                        });
                        best_gain = gain;
                        best_action = Some(act);
                    } else {
                        // Tentatively apply swap + clear scratch blockers to find non-interfering
                        // free paid GPRs (excluding `r_u` and `r_cand`), then restore `gpr_busy`.
                        clear_busy(&mut gpr_busy, r_u, u_vid);
                        clear_busy(&mut gpr_busy, r_cand, v_vid);
                        for ev in evicts.iter().take(evict_count) {
                            clear_busy(&mut gpr_busy, ev.from, ev.vid);
                        }
                        set_busy(&mut gpr_busy, r_cand, u_vid);
                        set_busy(&mut gpr_busy, r_u, v_vid);

                        let mut placed = 0usize;
                        let mut all_evicted = true;
                        for ev in evicts.iter_mut().take(evict_count) {
                            if let Some(r_evict) = find_evict_reg(
                                &gpr_busy,
                                ev.inst,
                                ev.vid,
                                r_cand,
                                r_u,
                                used_callee_gpr,
                            ) {
                                ev.to = r_evict;
                                set_busy(&mut gpr_busy, r_evict, ev.vid);
                                placed += 1;
                            } else {
                                all_evicted = false;
                                break;
                            }
                        }

                        // Restore `gpr_busy` bit-for-bit before evaluating remaining candidates.
                        for ev in evicts.iter().take(placed) {
                            clear_busy(&mut gpr_busy, ev.to, ev.vid);
                        }
                        clear_busy(&mut gpr_busy, r_u, v_vid);
                        clear_busy(&mut gpr_busy, r_cand, u_vid);
                        for ev in evicts.iter().take(evict_count) {
                            set_busy(&mut gpr_busy, ev.from, ev.vid);
                        }
                        set_busy(&mut gpr_busy, r_cand, v_vid);
                        set_busy(&mut gpr_busy, r_u, u_vid);

                        if all_evicted {
                            let mut act = RefineAction::EMPTY;
                            act.push(GprMove {
                                inst: u_inst,
                                vid: u_vid,
                                from: r_u,
                                to: r_cand,
                            });
                            act.push(GprMove {
                                inst: v_inst,
                                vid: v_vid,
                                from: r_cand,
                                to: r_u,
                            });
                            for &ev in evicts.iter().take(evict_count) {
                                act.push(ev);
                            }
                            best_gain = gain;
                            best_action = Some(act);
                        }
                    }
                } else if rc_scratch_count > 0 {
                    // Case B: all blockers on `r_cand` are 1..=2 zero-affinity scratch temps.
                    if !is_paid_reg(r_cand, used_callee_gpr) {
                        continue;
                    }
                    let swap_after = eval_single_swap(loc, u_inst, r_cand);
                    let after =
                        node_move_cost(loc, u_inst, r_cand, usize::MAX).saturating_add(swap_after);
                    let gain = u_single_before.saturating_sub(after);
                    if gain <= best_gain {
                        continue;
                    }

                    // Tentatively move `u -> r_cand` and clear scratch blockers; note `r_u` IS
                    // eligible as an eviction target once `u` leaves `r_u`.
                    clear_busy(&mut gpr_busy, r_u, u_vid);
                    for ev in rc_scratch.iter().take(rc_scratch_count) {
                        clear_busy(&mut gpr_busy, r_cand, ev.vid);
                    }
                    set_busy(&mut gpr_busy, r_cand, u_vid);

                    let mut placed = 0usize;
                    let mut all_evicted = true;
                    for ev in rc_scratch.iter_mut().take(rc_scratch_count) {
                        if let Some(r_evict) = find_evict_reg(
                            &gpr_busy,
                            ev.inst,
                            ev.vid,
                            r_cand,
                            NO_REG,
                            used_callee_gpr,
                        ) {
                            ev.to = r_evict;
                            set_busy(&mut gpr_busy, r_evict, ev.vid);
                            placed += 1;
                        } else {
                            all_evicted = false;
                            break;
                        }
                    }

                    // Restore `gpr_busy` bit-for-bit before evaluating remaining candidates.
                    for ev in rc_scratch.iter().take(placed) {
                        clear_busy(&mut gpr_busy, ev.to, ev.vid);
                    }
                    clear_busy(&mut gpr_busy, r_cand, u_vid);
                    for ev in rc_scratch.iter().take(rc_scratch_count) {
                        set_busy(&mut gpr_busy, r_cand, ev.vid);
                    }
                    set_busy(&mut gpr_busy, r_u, u_vid);

                    if all_evicted {
                        let mut act = RefineAction::EMPTY;
                        act.push(GprMove {
                            inst: u_inst,
                            vid: u_vid,
                            from: r_u,
                            to: r_cand,
                        });
                        for &ev in rc_scratch.iter().take(rc_scratch_count) {
                            act.push(ev);
                        }
                        best_gain = gain;
                        best_action = Some(act);
                    }
                }
            }

            // If `u_inst` currently participates in an active parallel-copy 2-cycle swap, also
            // consider any completely free already-paid GPR outside `cand_mask`: moving one side
            // of a pinned 2-cycle swap to a free register eliminates the extra `X16` temp `mov`
            // (3 -> 2 moves) with zero spills and zero callee-saved expansion.
            if u_swap_before > 0 {
                for &r_raw in CALLER_SAVED_GPR.iter().chain(callee_gpr_pool.iter()) {
                    let r = reg_u8(r_raw);
                    if r == r_u
                        || r >= 32
                        || (cand_mask & (1u32 << u32::from(r))) != 0
                        || !is_paid_reg(r, used_callee_gpr)
                        || !gpr_reg_safe(u_inst, r)
                        || !is_free_in_busy(&gpr_busy, r, u_vid)
                    {
                        continue;
                    }
                    let swap_after = eval_single_swap(loc, u_inst, r);
                    let after =
                        node_move_cost(loc, u_inst, r, usize::MAX).saturating_add(swap_after);
                    let gain = u_single_before.saturating_sub(after);
                    if gain > best_gain {
                        let mut act = RefineAction::EMPTY;
                        act.push(GprMove {
                            inst: u_inst,
                            vid: u_vid,
                            from: r_u,
                            to: r,
                        });
                        best_gain = gain;
                        best_action = Some(act);
                    }
                }
            }

            if let Some(action) = best_action {
                // Transactional commit: remove all moved values from their old `gpr_busy` rows,
                // verify every target row has 0 interference overlap before marking it busy, and
                // roll back `gpr_busy` untouched without mutating `loc` if any check fails.
                let count = action.len.min(MAX_ACTION_MOVES);
                for mv in action.moves.iter().take(count) {
                    clear_busy(&mut gpr_busy, mv.from, mv.vid);
                }
                let mut placed = 0usize;
                let mut verified = true;
                for mv in action.moves.iter().take(count) {
                    if !is_free_in_busy(&gpr_busy, mv.to, mv.vid) {
                        verified = false;
                        break;
                    }
                    set_busy(&mut gpr_busy, mv.to, mv.vid);
                    placed += 1;
                }
                if verified {
                    for mv in action.moves.iter().take(count) {
                        if let Some(slot) = loc.get_mut(mv.inst) {
                            *slot = AllocLoc::Gpr(mv.to);
                        }
                    }
                    round_changed = true;
                    any_changed = true;
                } else {
                    for mv in action.moves.iter().take(placed) {
                        clear_busy(&mut gpr_busy, mv.to, mv.vid);
                    }
                    for mv in action.moves.iter().take(count) {
                        set_busy(&mut gpr_busy, mv.from, mv.vid);
                    }
                }
            }
        }

        // Phase 2: bounded pairwise color-swap pass for colliding edge-group vregs.
        // Catches any pair `(u, v)` (`u < v`) sharing an uncoalesced parallel-copy CFG edge group
        // (e.g., `pos` holding `X4` while `word` arrives in `X4` and wants `X4` from `X20`),
        // evaluating exact before-vs-after weighted move + parallel-copy swap + ABI cost via a
        // temporary color swap so direct `(u, v)` edges are never double-counted.
        if !valid_groups.is_empty() {
            pair_raw.clear();
            pair_cands.clear();
            for &(s_u32, e_u32, _) in &valid_groups {
                if pair_raw.len() >= MAX_PAIR_SWAP_RAW {
                    break;
                }
                let Some(pairs) = coalesce_edges.get(idx(s_u32)..idx(e_u32)) else {
                    continue;
                };
                for (i, &(a_i, p_i, w_i)) in pairs.iter().enumerate() {
                    if a_i == p_i || w_i == 0 {
                        continue;
                    }
                    let (Some(AllocLoc::Gpr(ra_i)), Some(AllocLoc::Gpr(rp_i))) = (
                        loc.get(idx(a_i)).copied(),
                        loc.get(idx(p_i)).copied(),
                    ) else {
                        continue;
                    };
                    if ra_i >= 32 || rp_i >= 32 {
                        continue;
                    }
                    for &(a_j, p_j, w_j) in pairs.iter().skip(i + 1) {
                        if a_j == p_j || w_j == 0 {
                            continue;
                        }
                        let (Some(AllocLoc::Gpr(ra_j)), Some(AllocLoc::Gpr(rp_j))) = (
                            loc.get(idx(a_j)).copied(),
                            loc.get(idx(p_j)).copied(),
                        ) else {
                            continue;
                        };
                        if ra_j >= 32 || rp_j >= 32 {
                            continue;
                        }
                        if ra_i == rp_i && ra_j == rp_j {
                            continue;
                        }
                        let collides = rp_i == ra_j || rp_j == ra_i;
                        let base_w = w_i.max(w_j);
                        let prio_w = if collides {
                            base_w.saturating_mul(2)
                        } else {
                            base_w
                        };
                        if rp_i != rp_j {
                            push_pair_cand(&mut pair_raw, p_i, p_j, prio_w);
                        }
                        if ra_i != ra_j {
                            push_pair_cand(&mut pair_raw, a_i, a_j, prio_w);
                        }
                        if collides {
                            if ra_i != rp_j {
                                push_pair_cand(&mut pair_raw, a_i, p_j, base_w);
                            }
                            if rp_i != ra_j {
                                push_pair_cand(&mut pair_raw, p_i, a_j, base_w);
                            }
                        }
                        if pair_raw.len() >= MAX_PAIR_SWAP_RAW {
                            break;
                        }
                    }
                    if pair_raw.len() >= MAX_PAIR_SWAP_RAW {
                        break;
                    }
                }
            }

            if !pair_raw.is_empty() {
                pair_raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                for &(u, v, w) in &pair_raw {
                    if let Some(last) = pair_cands.last_mut()
                        && last.0 == u
                        && last.1 == v
                    {
                        last.2 = last.2.saturating_add(w);
                        continue;
                    }
                    pair_cands.push((u, v, w));
                }
                pair_cands.sort_unstable_by(|a, b| {
                    b.2.cmp(&a.2)
                        .then_with(|| a.0.cmp(&b.0))
                        .then_with(|| a.1.cmp(&b.1))
                });

                for &(u_u32, v_u32, _prio) in pair_cands.iter().take(MAX_PAIR_SWAP_CANDS) {
                    let u_inst = idx(u_u32);
                    let v_inst = idx(v_u32);
                    if u_inst >= v_inst {
                        continue;
                    }
                    let (Some(AllocLoc::Gpr(cu)), Some(AllocLoc::Gpr(cv))) = (
                        loc.get(u_inst).copied(),
                        loc.get(v_inst).copied(),
                    ) else {
                        continue;
                    };
                    if cu == cv || cu >= 32 || cv >= 32 {
                        continue;
                    }
                    let u_vid_raw = val_id.get(u_inst).copied().unwrap_or(NO_VAL);
                    let v_vid_raw = val_id.get(v_inst).copied().unwrap_or(NO_VAL);
                    if u_vid_raw == NO_VAL || v_vid_raw == NO_VAL || u_vid_raw == v_vid_raw {
                        continue;
                    }
                    if !gpr_reg_safe(u_inst, cv) || !gpr_reg_safe(v_inst, cu) {
                        continue;
                    }
                    let u_vid = idx(u_vid_raw);
                    let v_vid = idx(v_vid_raw);

                    // Exact combined before-vs-after cost via temporary color swap.
                    let (swap_before, swap_after) = eval_pair_swap(loc, u_inst, cv, v_inst, cu);
                    let old_cost =
                        pair_edge_and_abi_cost(loc, u_inst, v_inst).saturating_add(swap_before);
                    if let Some(slot) = loc.get_mut(u_inst) {
                        *slot = AllocLoc::Gpr(cv);
                    }
                    if let Some(slot) = loc.get_mut(v_inst) {
                        *slot = AllocLoc::Gpr(cu);
                    }
                    let new_cost =
                        pair_edge_and_abi_cost(loc, u_inst, v_inst).saturating_add(swap_after);
                    if let Some(slot) = loc.get_mut(u_inst) {
                        *slot = AllocLoc::Gpr(cu);
                    }
                    if let Some(slot) = loc.get_mut(v_inst) {
                        *slot = AllocLoc::Gpr(cv);
                    }
                    if new_cost >= old_cost {
                        continue;
                    }

                    // Verify interference legality against `gpr_busy` after temporarily vacating
                    // `u` from `cu` and `v` from `cv` (so `w != v` in `interf[u]` cannot hold `cv`
                    // and `w != u` in `interf[v]` cannot hold `cu`).
                    clear_busy(&mut gpr_busy, cu, u_vid);
                    clear_busy(&mut gpr_busy, cv, v_vid);
                    if is_free_in_busy(&gpr_busy, cv, u_vid)
                        && is_free_in_busy(&gpr_busy, cu, v_vid)
                    {
                        set_busy(&mut gpr_busy, cv, u_vid);
                        set_busy(&mut gpr_busy, cu, v_vid);
                        if let Some(slot) = loc.get_mut(u_inst) {
                            *slot = AllocLoc::Gpr(cv);
                        }
                        if let Some(slot) = loc.get_mut(v_inst) {
                            *slot = AllocLoc::Gpr(cu);
                        }
                        round_changed = true;
                        any_changed = true;
                        continue;
                    }

                    // Otherwise allow evicting up to `MAX_SCRATCH_EVICTS` zero-affinity scratch
                    // blockers per side into free paid GPRs (excluding `cu` and `cv`).
                    let mut evicts = [GprMove::EMPTY; MAX_TOTAL_SCRATCH_EVICTS];
                    let mut evict_count = 0usize;
                    let mut evict_ok = true;
                    let cv_base = usize::from(cv).saturating_mul(val_words);
                    let u_base = u_vid.saturating_mul(val_words);
                    let mut cv_count = 0usize;
                    for w in 0..val_words {
                        let mut bits = gpr_busy.get(cv_base + w).copied().unwrap_or(0)
                            & interf.get(u_base + w).copied().unwrap_or(0);
                        while bits != 0 {
                            if cv_count >= MAX_SCRATCH_EVICTS {
                                evict_ok = false;
                                break;
                            }
                            let b_vid = w.saturating_mul(64) + idx(bits.trailing_zeros());
                            bits &= bits - 1;
                            let Some(&b_inst) = val_inst.get(b_vid) else {
                                evict_ok = false;
                                break;
                            };
                            if b_inst == u_inst
                                || b_inst == v_inst
                                || !is_zero_affinity_scratch(loc, b_inst, cv)
                            {
                                evict_ok = false;
                                break;
                            }
                            if let Some(slot) = evicts.get_mut(evict_count) {
                                *slot = GprMove {
                                    inst: b_inst,
                                    vid: b_vid,
                                    from: cv,
                                    to: NO_REG,
                                };
                            }
                            evict_count += 1;
                            cv_count += 1;
                        }
                        if !evict_ok {
                            break;
                        }
                    }

                    if evict_ok {
                        let cu_base = usize::from(cu).saturating_mul(val_words);
                        let v_base = v_vid.saturating_mul(val_words);
                        let mut cu_count = 0usize;
                        for w in 0..val_words {
                            let mut bits = gpr_busy.get(cu_base + w).copied().unwrap_or(0)
                                & interf.get(v_base + w).copied().unwrap_or(0);
                            while bits != 0 {
                                if cu_count >= MAX_SCRATCH_EVICTS {
                                    evict_ok = false;
                                    break;
                                }
                                let b_vid = w.saturating_mul(64) + idx(bits.trailing_zeros());
                                bits &= bits - 1;
                                let Some(&b_inst) = val_inst.get(b_vid) else {
                                    evict_ok = false;
                                    break;
                                };
                                if b_inst == u_inst
                                    || b_inst == v_inst
                                    || !is_zero_affinity_scratch(loc, b_inst, cu)
                                {
                                    evict_ok = false;
                                    break;
                                }
                                if let Some(slot) = evicts.get_mut(evict_count) {
                                    *slot = GprMove {
                                        inst: b_inst,
                                        vid: b_vid,
                                        from: cu,
                                        to: NO_REG,
                                    };
                                }
                                evict_count += 1;
                                cu_count += 1;
                            }
                            if !evict_ok {
                                break;
                            }
                        }
                    }

                    if evict_ok && evict_count > 0 {
                        for ev in evicts.iter().take(evict_count) {
                            clear_busy(&mut gpr_busy, ev.from, ev.vid);
                        }
                        if is_free_in_busy(&gpr_busy, cv, u_vid)
                            && is_free_in_busy(&gpr_busy, cu, v_vid)
                        {
                            set_busy(&mut gpr_busy, cv, u_vid);
                            set_busy(&mut gpr_busy, cu, v_vid);
                            let mut placed = 0usize;
                            let mut all_evicted = true;
                            for ev in evicts.iter_mut().take(evict_count) {
                                if let Some(r_evict) = find_evict_reg(
                                    &gpr_busy,
                                    ev.inst,
                                    ev.vid,
                                    cv,
                                    cu,
                                    used_callee_gpr,
                                ) {
                                    ev.to = r_evict;
                                    set_busy(&mut gpr_busy, r_evict, ev.vid);
                                    placed += 1;
                                } else {
                                    all_evicted = false;
                                    break;
                                }
                            }
                            if all_evicted {
                                if let Some(slot) = loc.get_mut(u_inst) {
                                    *slot = AllocLoc::Gpr(cv);
                                }
                                if let Some(slot) = loc.get_mut(v_inst) {
                                    *slot = AllocLoc::Gpr(cu);
                                }
                                for ev in evicts.iter().take(evict_count) {
                                    if let Some(slot) = loc.get_mut(ev.inst) {
                                        *slot = AllocLoc::Gpr(ev.to);
                                    }
                                }
                                round_changed = true;
                                any_changed = true;
                            } else {
                                for ev in evicts.iter().take(placed) {
                                    clear_busy(&mut gpr_busy, ev.to, ev.vid);
                                }
                                clear_busy(&mut gpr_busy, cv, u_vid);
                                clear_busy(&mut gpr_busy, cu, v_vid);
                                for ev in evicts.iter().take(evict_count) {
                                    set_busy(&mut gpr_busy, ev.from, ev.vid);
                                }
                                set_busy(&mut gpr_busy, cu, u_vid);
                                set_busy(&mut gpr_busy, cv, v_vid);
                            }
                        } else {
                            for ev in evicts.iter().take(evict_count) {
                                set_busy(&mut gpr_busy, ev.from, ev.vid);
                            }
                            set_busy(&mut gpr_busy, cu, u_vid);
                            set_busy(&mut gpr_busy, cv, v_vid);
                        }
                    } else {
                        set_busy(&mut gpr_busy, cu, u_vid);
                        set_busy(&mut gpr_busy, cv, v_vid);
                    }
                }
            }
        }
        if !round_changed {
            break;
        }
    }

    if any_changed {
        *used_callee_gpr = [false; 32];
        for &l in loc.iter() {
            match l {
                AllocLoc::Gpr(r) => {
                    let ru = u32::from(r);
                    if is_tracked_callee_gpr(ru) {
                        if let Some(slot) = used_callee_gpr.get_mut(usize::from(r)) {
                            *slot = true;
                        }
                    }
                }
                AllocLoc::GprPair(lo, hi) => {
                    for r in [lo, hi] {
                        let ru = u32::from(r);
                        if is_tracked_callee_gpr(ru) {
                            if let Some(slot) = used_callee_gpr.get_mut(usize::from(r)) {
                                *slot = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    any_changed
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
    layout: Option<&super::layout::BlockLayout<'_>>,
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
                    let clob = effective_call_clobbers(view, inst, call_clobbers(inst));
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

    // Compact symmetric per-value phi-copy affinity CSR (`phi_adj_off`, `phi_adj`), coalescable
    // edge cost list (`coalesce_edges`), and per-CFG-edge parallel-copy ranges (`edge_groups`)
    // for bidirectional multi-edge coalescing, immediate joint co-coloring, and cycle-swap cost
    // modeling.
    let mut coalesce_edges: Vec<(u32, u32, u64), A> = Vec::new_in(alloc);
    let mut edge_groups: Vec<(u32, u32), A> = Vec::new_in(alloc);
    let (phi_adj_off, phi_adj) = {
        let mut phi_raw: Vec<(u32, u32, u32, u32), A> = Vec::new_in(alloc);
        for b in 0..blocks {
            let term = view.terminator(Block(u32_of(b)));
            if view.op(term) == Op::Return {
                continue;
            }
            let b_depth = block_depth.get(b).copied().unwrap_or(0);
            let b_rpo = rpo_idx.get(b).copied().unwrap_or(0);
            let b_rpo_u32 = u32_of(b_rpo);
            for_each_edge(view, term, |target_block, args_start, args_len| {
                let target_params = view.params(Block(target_block));
                let n = args_len.min(target_params.len());
                let is_back = rpo_idx.get(idx(target_block)).copied().unwrap_or(blocks) <= b_rpo;
                let target_cold = if let Some(bl) = layout {
                    bl.is_cold.get(idx(target_block)).copied().unwrap_or(false)
                } else {
                    is_cold_block.get(idx(target_block)).copied().unwrap_or(false)
                };
                let eff_depth = if target_cold && !is_back { 0 } else { b_depth };
                let depth_prio = eff_depth.saturating_add(1).min(FWD_EDGE_PRIO_MAX);
                let edge_prio = if is_back { BACK_EDGE_PRIO_BIT | depth_prio } else { depth_prio };
                let fallback_weight = (1u64 << eff_depth.min(3).saturating_mul(7))
                    .saturating_mul(if is_back { 2 } else { 1 });
                let edge_weight = if let Some(bl) = layout {
                    let solved_w = bl
                        .edge_weight_between(b, idx(target_block))
                        .min(MAX_EDGE_WEIGHT);
                    if solved_w > 0 {
                        solved_w.saturating_mul(if is_back { 2 } else { 1 })
                    } else {
                        fallback_weight
                    }
                } else {
                    fallback_weight
                };
                let group_start = coalesce_edges.len();
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
                        let arg_u32 = u32_of(arg);
                        let p_u32 = u32_of(p);
                        if !skip_alloc.get(arg).copied().unwrap_or(false)
                            && !skip_alloc.get(p).copied().unwrap_or(false)
                            && val_id.get(arg).copied().unwrap_or(NO_VAL) != NO_VAL
                            && val_id.get(p).copied().unwrap_or(NO_VAL) != NO_VAL
                        {
                            phi_raw.push((arg_u32, p_u32, edge_prio, b_rpo_u32));
                            phi_raw.push((p_u32, arg_u32, edge_prio, b_rpo_u32));
                            coalesce_edges.push((arg_u32, p_u32, edge_weight));
                        }
                        if arg > p {
                            let cur = phi_partner.get(arg).copied().unwrap_or(u32::MAX);
                            let cur_prio = phi_prio.get(arg).copied().unwrap_or(0);
                            if cur == u32::MAX || edge_prio > cur_prio {
                                if let Some(slot) = phi_partner.get_mut(arg) {
                                    *slot = p_u32;
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
                                            *bpslot = arg_u32;
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
                            if cur == arg_u32 {
                                if let Some(pslot) = phi_prio.get_mut(p) {
                                    *pslot = merge_phi_prio(cur_prio, edge_prio);
                                }
                            } else if cur == u32::MAX || edge_prio > cur_prio {
                                if let Some(slot) = phi_partner.get_mut(p) {
                                    *slot = arg_u32;
                                }
                                if let Some(pslot) = phi_prio.get_mut(p) {
                                    *pslot = edge_prio;
                                }
                            }
                        }
                    }
                }
                let group_end = coalesce_edges.len();
                let group_len = group_end.saturating_sub(group_start);
                if (2..=MAX_PARALLEL_COPY_GROUP).contains(&group_len)
                    && edge_groups.len() < MAX_EDGE_GROUPS
                {
                    edge_groups.push((u32_of(group_start), u32_of(group_end)));
                }
            });
        }

        // Deduplicate & accumulate priorities per `(src, dst)` pair, keeping earliest edge RPO,
        // then sort each node's neighbors by descending priority, ascending edge RPO (favoring
        // earlier branches such as Position 0 over Position 1 at equal loop depth), and
        // deterministic ascending `neighbor_inst` tie-break.
        phi_raw.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut phi_dedup: Vec<(u32, u32, u32, u32), A> = Vec::with_capacity_in(phi_raw.len(), alloc);
        for &(u, v, prio, edge_rpo) in &phi_raw {
            if let Some(last) = phi_dedup.last_mut()
                && last.0 == u
                && last.1 == v
            {
                last.2 = merge_phi_prio(last.2, prio);
                if edge_rpo < last.3 {
                    last.3 = edge_rpo;
                }
                continue;
            }
            phi_dedup.push((u, v, prio, edge_rpo));
        }
        phi_dedup.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.3.cmp(&b.3))
                .then_with(|| a.1.cmp(&b.1))
        });

        let mut off: Vec<u32, A> = Vec::with_capacity_in(len + 1, alloc);
        let mut adj: Vec<(u32, u32), A> = Vec::with_capacity_in(phi_dedup.len(), alloc);
        let mut cur_idx = 0usize;
        for u in 0..len {
            off.push(u32_of(adj.len()));
            let u_u32 = u32_of(u);
            while let Some(&(src, dst, prio, _)) = phi_dedup.get(cur_idx) {
                if src != u_u32 {
                    break;
                }
                adj.push((dst, prio));
                cur_idx += 1;
            }
        }
        off.push(u32_of(adj.len()));
        (off, adj)
    };

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
    let allow_fp = can_use_fp_as_gpr(view, allow_lr);
    let callee_gpr_pool = if allow_lr && allow_fp {
        CALLEE_SAVED_GPR_WITH_LR_FP
    } else if allow_lr {
        CALLEE_SAVED_GPR_WITH_LR
    } else {
        CALLEE_SAVED_GPR
    };

    #[derive(Clone, Copy)]
    struct CoColorPartner {
        inst: usize,
        vid: usize,
        crosses_call: bool,
        clobbers: CallClobbers,
        prio: u32,
    }

    struct ColoringResult<A: Allocator> {
        loc: Vec<AllocLoc, A>,
        used_callee_gpr: [bool; 32],
        used_callee_fp: [bool; 32],
        num_spill_slots: u32,
        remat_count: u32,
        spill_cost: u64,
        move_cost: u64,
    }

    let adj_slice = |u: usize| -> &[(u32, u32)] {
        let start = phi_adj_off.get(u).copied().map_or(0, idx);
        let end = phi_adj_off.get(u + 1).copied().map_or(start, idx);
        phi_adj.get(start..end).unwrap_or(&[])
    };

    // Precompute bitmask of scalar GPR values with at least one `phi_adj` coalescing partner so
    // `run_coloring` can compute anti-hint `avoid_mask` in O(words) via `interf[vid] & val_has_phi_adj`.
    let mut val_has_phi_adj: Vec<u64, A> = Vec::with_capacity_in(val_words, alloc);
    for _ in 0..val_words {
        val_has_phi_adj.push(0);
    }
    let mut has_any_gpr_phi_adj = false;
    for (u_vid, &u_inst) in val_inst.iter().enumerate() {
        if !adj_slice(u_inst).is_empty() {
            let u_ty = view.tys.get(u_inst).copied().map_or(Ty::NONE, Ty);
            if reg_bank(view, u_ty) == 3
                && let Some(slot) = val_has_phi_adj.get_mut(u_vid >> 6)
            {
                *slot |= 1u64 << (u_vid & 63);
                has_any_gpr_phi_adj = true;
            }
        }
    }

    let run_coloring = |order: &[Candidate]| -> Result<ColoringResult<A>, RegAllocError> {
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
        // Anti-hinting: collect GPRs wanted by uncolored interfering `phi_adj` values so an
        // unhinted fallback pool scan avoids stealing a partner's register and forcing an edge mov.
        let compute_avoid_mask = |loc_try: &[AllocLoc], target_vid: usize| -> u32 {
            if !has_any_gpr_phi_adj {
                return 0;
            }
            let mut avoid_mask = 0u32;
            let mut checked = 0u32;
            let v_base = target_vid.saturating_mul(val_words);
            for w in 0..val_words {
                let mut bits = interf.get(v_base + w).copied().unwrap_or(0)
                    & val_has_phi_adj.get(w).copied().unwrap_or(0);
                while bits != 0 {
                    let tz = idx(bits.trailing_zeros());
                    bits &= bits - 1;
                    let u_vid = w.saturating_mul(64) + tz;
                    let Some(&u_inst) = val_inst.get(u_vid) else {
                        continue;
                    };
                    if loc_try.get(u_inst).copied().unwrap_or(AllocLoc::None) != AllocLoc::None {
                        continue;
                    }
                    let u_crosses_call = val_crosses_call.get(u_inst).copied().unwrap_or(false);
                    let u_clobbers = if u_crosses_call {
                        val_crossed_clobbers
                            .get(u_inst)
                            .copied()
                            .unwrap_or(CallClobbers::EMPTY)
                    } else {
                        CallClobbers::EMPTY
                    };
                    for &(partner, _prio) in adj_slice(u_inst).iter().take(2) {
                        let p_inst = idx(partner);
                        let wanted = match loc_try.get(p_inst).copied() {
                            Some(AllocLoc::Gpr(r)) if r < 32 => r,
                            Some(AllocLoc::None) | None => {
                                hint.get(p_inst).copied().unwrap_or(NO_REG)
                            }
                            _ => NO_REG,
                        };
                        if wanted < 32
                            && (!u_crosses_call
                                || (19..=28).contains(&wanted)
                                || !u_clobbers.clobbers_gpr(wanted))
                        {
                            avoid_mask |= 1u32 << u32::from(wanted);
                        }
                    }
                    checked += 1;
                    if checked >= 32 {
                        return avoid_mask;
                    }
                }
            }
            avoid_mask
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
            if loc_try.get(inst).copied().unwrap_or(AllocLoc::None) != AllocLoc::None {
                continue;
            }
            let vid = cand.vid;
            let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
            let bank = reg_bank(view, ty);
            let adj = adj_slice(inst);

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
                    if is_tracked_callee_gpr(lo) {
                        if let Some(u) = used_callee_gpr.get_mut(idx(lo)) {
                            *u = true;
                        }
                    }
                    if is_tracked_callee_gpr(hi) {
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
                let adj_vec = adj.iter().find_map(|&(nb, _)| match loc_try.get(idx(nb)).copied() {
                    Some(AllocLoc::Vec(r)) => try_vec(r),
                    _ => None,
                });
                let chosen = adj_vec
                    .or_else(|| try_vec(primary_hint))
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
                let adj_fp = adj.iter().find_map(|&(nb, _)| match loc_try.get(idx(nb)).copied() {
                    Some(AllocLoc::Fp(r)) => try_fp(r),
                    _ => None,
                });
                let chosen = adj_fp
                    .or_else(|| try_fp(primary_hint))
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
                let try_gpr = |h: u8| -> Option<Reg> {
                    if h != NO_REG
                        && (u32::from(h) != X30 || allow_lr)
                        && (u32::from(h) != FP || allow_fp)
                        && gpr_safe(u32::from(h))
                        && is_reg_free(&gpr_busy, usize::from(h), vid)
                    {
                        Some(u32::from(h))
                    } else {
                        None
                    }
                };
                let best_colored = adj.iter().find_map(|&(nb, prio)| match loc_try.get(idx(nb)).copied() {
                    Some(AllocLoc::Gpr(r)) => try_gpr(r).map(|reg| (reg, prio)),
                    _ => None,
                });
                let can_cocolor_gpr = |bpi: usize, prio: u32| -> Option<CoColorPartner> {
                    if bpi == inst || bpi >= len {
                        return None;
                    }
                    if loc_try.get(bpi).copied().unwrap_or(AllocLoc::None) != AllocLoc::None {
                        return None;
                    }
                    if skip_alloc.get(bpi).copied().unwrap_or(false)
                        || remat_val.get(bpi).copied().flatten().is_some()
                        || matches!(view.op(bpi), Op::DataAddr | Op::FuncAddr)
                    {
                        return None;
                    }
                    let bp_vid_raw = val_id.get(bpi).copied().unwrap_or(NO_VAL);
                    if bp_vid_raw == NO_VAL {
                        return None;
                    }
                    let bp_ty = view.tys.get(bpi).copied().map_or(Ty::NONE, Ty);
                    if reg_bank(view, bp_ty) != 3 || is_gpr_pair_ty(view, bp_ty) {
                        return None;
                    }
                    if vals_interfere(inst, bpi) {
                        return None;
                    }
                    let crosses_call = val_crosses_call.get(bpi).copied().unwrap_or(false);
                    let clobbers = val_crossed_clobbers
                        .get(bpi)
                        .copied()
                        .unwrap_or(CallClobbers::EMPTY);
                    Some(CoColorPartner {
                        inst: bpi,
                        vid: idx(bp_vid_raw),
                        crosses_call,
                        clobbers,
                        prio,
                    })
                };
                // Pick the highest-priority uncolored non-interfering GPR partner from
                // `back_partner_of` or loop/back-edge `phi_adj` (`prio >= 2`, which covers both
                // back-edges with `BACK_EDGE_PRIO_BIT` and forward edges inside loops, while
                // keeping single depth-0 edges out of joint co-coloring).
                let mut bp_cand: Option<CoColorPartner> = None;
                let bp_raw = back_partner_of.get(inst).copied().unwrap_or(u32::MAX);
                if bp_raw != u32::MAX {
                    let prio = back_partner_prio
                        .get(inst)
                        .copied()
                        .unwrap_or(BACK_EDGE_PRIO_BIT);
                    bp_cand = can_cocolor_gpr(idx(bp_raw), prio);
                }
                for &(nb, prio) in adj {
                    if prio < 2 {
                        break;
                    }
                    if bp_cand.is_some_and(|cur| prio <= cur.prio) {
                        break;
                    }
                    if let Some(partner) = can_cocolor_gpr(idx(nb), prio) {
                        bp_cand = Some(partner);
                        break;
                    }
                }
                let joint_chosen: Option<(Reg, (usize, usize))> = if let Some(bp) = bp_cand {
                    let bp_safe = |r: Reg| -> bool {
                        !bp.crosses_call
                            || (X19..=X28).contains(&r)
                            || !bp.clobbers.clobbers_gpr(reg_u8(r))
                    };
                    let try_gpr_joint = |h: u8| -> Option<Reg> {
                        let r = try_gpr(h)?;
                        (bp_safe(r) && is_reg_free(&gpr_busy, idx(r), bp.vid)).then_some(r)
                    };
                    let pool_joint = || -> Option<Reg> {
                        let avoid_mask = compute_avoid_mask(&loc_try, vid)
                            | compute_avoid_mask(&loc_try, bp.vid);
                        pick_gpr_from_pool(
                            caller_gpr_order,
                            callee_gpr_pool,
                            &used_callee_gpr,
                            avoid_mask,
                            |r| {
                                gpr_safe(r)
                                    && bp_safe(r)
                                    && is_reg_free(&gpr_busy, idx(r), vid)
                                    && is_reg_free(&gpr_busy, idx(r), bp.vid)
                            },
                        )
                    };
                    let reg = if let Some((colored_reg, colored_prio)) = best_colored {
                        if colored_prio > bp.prio {
                            // A higher-priority partner is already colored: co-color `bp`
                            // only if that register is also safe & free for `bp`.
                            try_gpr_joint(reg_u8(colored_reg))
                        } else {
                            adj.iter()
                                .find_map(|&(nb, _)| match loc_try.get(idx(nb)).copied() {
                                    Some(AllocLoc::Gpr(hr)) => try_gpr_joint(hr),
                                    _ => None,
                                })
                                .or_else(|| try_gpr_joint(primary_hint))
                                .or_else(|| try_gpr_joint(secondary_hint))
                                .or_else(|| try_gpr_joint(pair_trunc_hint))
                                .or_else(pool_joint)
                        }
                    } else {
                        try_gpr_joint(primary_hint)
                            .or_else(|| try_gpr_joint(secondary_hint))
                            .or_else(|| try_gpr_joint(pair_trunc_hint))
                            .or_else(pool_joint)
                    };
                    reg.map(|r| (r, (bp.inst, bp.vid)))
                } else {
                    None
                };
                let (chosen, cocolor_bp) = if let Some((reg, bp_pair)) = joint_chosen {
                    (Some(reg), Some(bp_pair))
                } else {
                    let reg = best_colored
                        .map(|(r, _)| r)
                        .or_else(|| try_gpr(primary_hint))
                        .or_else(|| try_gpr(secondary_hint))
                        .or_else(|| try_gpr(pair_trunc_hint))
                        .or_else(|| {
                            let avoid_mask = compute_avoid_mask(&loc_try, vid);
                            pick_gpr_from_pool(
                                caller_gpr_order,
                                callee_gpr_pool,
                                &used_callee_gpr,
                                avoid_mask,
                                |r| gpr_safe(r) && is_reg_free(&gpr_busy, idx(r), vid),
                            )
                        });
                    (reg, None)
                };
                if let Some(reg) = chosen {
                    let reg_byte = reg_u8(reg);
                    if let Some(slot) = loc_try.get_mut(inst) {
                        *slot = AllocLoc::Gpr(reg_byte);
                    }
                    mark_reg_busy(&mut gpr_busy, idx(reg), vid);
                    // Immediately lock the co-colored partner into `reg` in the order-independent
                    // interference bitmatrix so intervening temps cannot steal `reg`.
                    if let Some((bpi, bp_vid)) = cocolor_bp {
                        if let Some(bp_slot) = loc_try.get_mut(bpi) {
                            *bp_slot = AllocLoc::Gpr(reg_byte);
                        }
                        mark_reg_busy(&mut gpr_busy, idx(reg), bp_vid);
                    }
                    if is_tracked_callee_gpr(reg) {
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
        let mut move_cost = 0u64;
        for &(arg, p, weight) in &coalesce_edges {
            let a_loc = loc_try.get(idx(arg)).copied().unwrap_or(AllocLoc::None);
            let p_loc = loc_try.get(idx(p)).copied().unwrap_or(AllocLoc::None);
            if a_loc != p_loc {
                move_cost = move_cost.saturating_add(weight);
            }
        }
        for &(s_u32, e_u32) in &edge_groups {
            let Some(pairs) = coalesce_edges.get(idx(s_u32)..idx(e_u32)) else {
                continue;
            };
            for (i, &(a_i, p_i, w_i)) in pairs.iter().enumerate() {
                if a_i == p_i || w_i == 0 {
                    continue;
                }
                let (Some(AllocLoc::Gpr(ra_i)), Some(AllocLoc::Gpr(rp_i))) = (
                    loc_try.get(idx(a_i)).copied(),
                    loc_try.get(idx(p_i)).copied(),
                ) else {
                    continue;
                };
                if ra_i == rp_i {
                    continue;
                }
                for &(a_j, p_j, w_j) in pairs.iter().skip(i + 1) {
                    if a_j == p_j || w_j == 0 {
                        continue;
                    }
                    let (Some(AllocLoc::Gpr(ra_j)), Some(AllocLoc::Gpr(rp_j))) = (
                        loc_try.get(idx(a_j)).copied(),
                        loc_try.get(idx(p_j)).copied(),
                    ) else {
                        continue;
                    };
                    if ra_j != rp_j && ra_i == rp_j && ra_j == rp_i {
                        move_cost = move_cost.saturating_add(w_i.min(w_j));
                    }
                }
            }
        }
        Ok(ColoringResult {
            loc: loc_try,
            used_callee_gpr,
            used_callee_fp,
            num_spill_slots,
            remat_count,
            spill_cost,
            move_cost,
        })
    };

    // Phase 1: exact chordal-SSA program-order coloring (provably minimal register count!).
    let mut best = run_coloring(&prog_order)?;

    // Phase 2: if register pressure overflowed (spills > 0 or remat > 0), OR Phase 1 left
    // uncoalesced phi moves in a small/medium function, run loop-weighted Greedy Priority
    // Coloring and adopt it strictly when `(spill_cost, num_spill_slots, move_cost)` improves.
    if best.num_spill_slots > 0
        || best.remat_count > 0
        || (best.move_cost > 0 && prog_order.len() <= 512)
    {
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
        if let Ok(greedy) = run_coloring(&greedy_order) {
            if (greedy.spill_cost, greedy.num_spill_slots, greedy.move_cost)
                < (best.spill_cost, best.num_spill_slots, best.move_cost)
            {
                best = greedy;
            }
        }
    }

    // Post-coloring bounded GPR refinement: single-value recolor + 2-cycle pair-swap to
    // eliminate residual merge-block moves and 2-cycle swap stubs without increasing spills
    // or callee-saved pairs.
    if best.move_cost > 0 {
        let abi_weight = layout
            .and_then(|bl| bl.block_freq.first().copied())
            .filter(|&f| f > 0)
            .map_or(1, |f| f.min(MAX_EDGE_WEIGHT));
        refine_gpr_moves(
            view,
            alias_of,
            os,
            &val_id,
            &val_inst,
            val_words,
            &interf,
            &val_crosses_call,
            &val_crossed_clobbers,
            &coalesce_edges,
            &edge_groups,
            abi_weight,
            allow_lr,
            allow_fp,
            callee_gpr_pool,
            &mut best.loc,
            &mut best.used_callee_gpr,
            alloc,
        );
    }
    let remat_count = best.remat_count.saturating_add(pre_remat_count);

    Ok(finalize_allocation(
        view,
        best.loc,
        best.used_callee_gpr,
        best.used_callee_fp,
        best.num_spill_slots,
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

    // 2. Coalescing hints in RPO order: entry params, return values, block-param forward/back edges,
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

    let interval_crosses_call = |s_pos: usize, e_pos: usize| -> bool {
        let first_call = call_rpo_positions.partition_point(|&(c_pos, _)| c_pos <= s_pos);
        call_rpo_positions.get(first_call).is_some_and(|&(c_pos, _)| c_pos < e_pos)
    };
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
            let depth_prio = b_depth.saturating_add(1).min(FWD_EDGE_PRIO_MAX);
            let back_prio = if is_back { BACK_EDGE_PRIO_BIT | depth_prio } else { 0 };
            if let Some(args) = view.extra.get(args_start..args_start + n) {
                for (k, &arg_raw) in args.iter().enumerate() {
                    let p = target_params.start + k;
                    let arg = resolve_alias(alias_of, idx(arg_raw));
                    if arg >= len || p >= len || arg == p {
                        continue;
                    }
                    let p_ty = view.tys.get(p).copied().map_or(Ty::NONE, Ty);
                    let arg_ty = view.tys.get(arg).copied().map_or(Ty::NONE, Ty);
                    let bank = reg_bank(view, arg_ty);
                    if bank == 0 || bank != reg_bank(view, p_ty) {
                        continue;
                    }
                    let (Some(&p_start), Some(&arg_start)) = (rpo_pos.get(p), rpo_pos.get(arg)) else {
                        continue;
                    };
                    if arg_start > p_start {
                        let cur = phi_partner.get(arg).copied().unwrap_or(u32::MAX);
                        let cur_prio = phi_prio.get(arg).copied().unwrap_or(0);
                        if cur == u32::MAX || back_prio > cur_prio {
                            if let Some(slot) = phi_partner.get_mut(arg) {
                                *slot = u32_of(p);
                            }
                            if let Some(pslot) = phi_prio.get_mut(arg) {
                                *pslot = back_prio;
                            }
                            if is_back
                                && !skip_alloc.get(arg).copied().unwrap_or(false)
                                && remat_val.get(arg).copied().flatten().is_none()
                                && !matches!(view.op(arg), Op::DataAddr | Op::FuncAddr)
                            {
                                if let Some(bslot) = is_back_phi_arg.get_mut(arg) {
                                    *bslot = true;
                                }
                                let p_end = last_use_pos.get(p).copied().unwrap_or(p_start).max(p_start);
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
                                    if cur_bp == u32::MAX || back_prio > cur_bp_prio {
                                        if let Some(bpslot) = back_partner_of.get_mut(p) {
                                            *bpslot = u32_of(arg);
                                        }
                                        if let Some(bppslot) = back_partner_prio.get_mut(p) {
                                            *bppslot = back_prio;
                                        }
                                    }
                                }
                            }
                        }
                    } else if arg_start < p_start
                        && !skip_alloc.get(arg).copied().unwrap_or(false)
                        && !skip_alloc.get(p).copied().unwrap_or(false)
                        && has_any_use.get(p).copied().unwrap_or(false)
                        && remat_val.get(arg).copied().flatten().is_none()
                        && !matches!(view.op(arg), Op::DataAddr | Op::FuncAddr)
                    {
                        let p_end = last_use_pos.get(p).copied().unwrap_or(p_start).max(p_start);
                        let arg_end = last_use_pos.get(arg).copied().unwrap_or(arg_start).max(arg_start);
                        if !intervals_conflict(
                            p_start,
                            p_end,
                            is_rbw(view, p),
                            arg_start,
                            arg_end,
                            is_rbw(view, arg),
                        ) && (!interval_crosses_call(p_start, p_end)
                            || interval_crosses_call(arg_start, arg_end))
                        {
                            let arg_u32 = u32_of(arg);
                            let cur = phi_partner.get(p).copied().unwrap_or(u32::MAX);
                            let cur_prio = phi_prio.get(p).copied().unwrap_or(0);
                            if cur == arg_u32 {
                                if let Some(pslot) = phi_prio.get_mut(p) {
                                    if cur_prio < BACK_EDGE_PRIO_BIT {
                                        *pslot = merge_phi_prio(cur_prio, depth_prio);
                                    }
                                }
                            } else if cur == u32::MAX || depth_prio > cur_prio {
                                if let Some(slot) = phi_partner.get_mut(p) {
                                    *slot = arg_u32;
                                }
                                if let Some(pslot) = phi_prio.get_mut(p) {
                                    *pslot = depth_prio;
                                }
                            }
                            // Also seed read-only joint-free lookup on `arg` for loop forward
                            // edges (strictly lower priority than back-edges; never inserts out
                            // of order into `BusySet::Monotonic`).
                            if bank == 3 && depth_prio >= 2 {
                                let p_u32 = u32_of(p);
                                let cur_bp = back_partner_of.get(arg).copied().unwrap_or(u32::MAX);
                                let cur_bp_prio = back_partner_prio.get(arg).copied().unwrap_or(0);
                                if cur_bp == p_u32 {
                                    if let Some(bppslot) = back_partner_prio.get_mut(arg) {
                                        if cur_bp_prio < BACK_EDGE_PRIO_BIT {
                                            *bppslot = merge_phi_prio(cur_bp_prio, depth_prio);
                                        }
                                    }
                                } else if cur_bp == u32::MAX || depth_prio > cur_bp_prio {
                                    if let Some(bpslot) = back_partner_of.get_mut(arg) {
                                        *bpslot = p_u32;
                                    }
                                    if let Some(bppslot) = back_partner_prio.get_mut(arg) {
                                        *bppslot = depth_prio;
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
    let allow_fp = can_use_fp_as_gpr(view, allow_lr);
    let callee_gpr_pool = if allow_lr && allow_fp {
        CALLEE_SAVED_GPR_WITH_LR_FP
    } else if allow_lr {
        CALLEE_SAVED_GPR_WITH_LR
    } else {
        CALLEE_SAVED_GPR
    };

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
                        // Unwinding out of an `Invoke` and caller-side arg setup both clobber regs.
                        let clob = effective_call_clobbers(view, c_inst, call_clobbers(c_inst));
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
                    if is_tracked_callee_gpr(lo) {
                        if let Some(u) = used_callee_gpr.get_mut(idx(lo)) {
                            *u = true;
                        }
                    }
                    if is_tracked_callee_gpr(hi) {
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
                        && (u32::from(h) != FP || allow_fp)
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
                    if is_tracked_callee_gpr(reg) {
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
            None,
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
        // a GPR before the 25 loop-invariant scalars, forcing Phase 2 to evict `#0x10` instead.
        let c16 = b.iconst(Ty::I64, 0x10);
        let c1 = b.iconst(Ty::I64, 1);
        let mut loop_scalars = Vec::with_capacity_in(25, alloc);
        let mut cur = x;
        for _ in 0..25usize {
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
        // pressure inside `loop_hdr` is exactly 28 (`n`, `iv`, `acc`, and 25 `loop_scalars`).
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
    fn leaf_function_can_allocate_fp_only_when_zero_call_fixed_frame() {
        use blaze_lir::func::SlotKind;

        let alloc = test_alloc();
        let gpr_ord = build_caller_gpr_order(None);
        let fp_ord = build_caller_fp_order(None);
        let uses_x29 = |locs: &[AllocLoc]| {
            locs.iter().any(|&loc| {
                matches!(
                    loc,
                    AllocLoc::Gpr(29) | AllocLoc::GprPair(29, _) | AllocLoc::GprPair(_, 29)
                )
            })
        };

        // 1. Zero-call, fixed-frame leaf with 28 simultaneously live GPRs allocates X29 (FP) with 0 spills!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let mut live = Vec::with_capacity_in(27, alloc);
            let mut cur = x;
            for _ in 0..27usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                live.push(cur);
            }
            let mut acc = b.binary(Op::Xor, Flags::NONE, x, live[0]);
            for &v in &live[1..] {
                acc = b.binary(Op::Xor, Flags::NONE, acc, v);
            }
            b.ret(&[acc]);

            let func = b.finish_checked().expect("28-GPR leaf LIR builds");
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
                .expect("exact-CFG 28-GPR leaf regalloc succeeds");
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
            .expect("RPO linear-scan 28-GPR leaf regalloc succeeds");

            for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
                assert_eq!(ra.num_spill_slots, 0, "{label}: 28 live GPRs in leaf must have 0 spills");
                assert!(!ra.has_calls, "{label}: leaf function must have has_calls == false");
                assert!(ra.uses_lr, "{label}: 28 live GPRs in leaf must set uses_lr = true");
                assert!(ra.uses_fp, "{label}: 28 live GPRs in leaf must set uses_fp = true");
                assert!(uses_x29(&ra.loc_of), "{label}: expected one value allocated to Gpr(29)");
                assert_eq!(ra.x_pairs, 0x1f, "{label}: FP must only be used after all 5 x19..x28 pairs");
                assert!(
                    !ra.clobbers.clobbers_gpr(29),
                    "{label}: callee-saved FP must not be reported as clobbered"
                );
            }
        }

        // 2. Function with an `Op::Call` MUST NEVER allocate X29 (FP)!
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
            let mut live = Vec::with_capacity_in(27, alloc);
            let mut cur = x;
            for _ in 0..27usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                live.push(cur);
            }
            let call_res = b.call(Flags::NONE, callee_ref, callee_sig, &[x]);
            let mut acc = b.binary(Op::Xor, Flags::NONE, call_res, x);
            for &v in &live {
                acc = b.binary(Op::Xor, Flags::NONE, acc, v);
            }
            b.ret(&[acc]);

            let func = b.finish_checked().expect("28-GPR call LIR builds");
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
                assert!(!ra.uses_fp, "{label}: function with Op::Call must never set uses_fp");
                assert!(!uses_x29(&ra.loc_of), "{label}: function with Op::Call must never allocate X29");
            }
        }

        // 3. Leaf with an over-aligned stack slot (`align = 32`, `align_log2 = 5 > 4`) MUST NEVER allocate X29 (FP)!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot32 = b.slot(32, 5, SlotKind::Temp);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c1 = b.iconst(Ty::I64, 1);
            let sptr = b.slot_addr(slot32);
            b.store(Flags::NONE, x, sptr, 0, MemAttrs::aligned(5));
            let mut live = Vec::with_capacity_in(27, alloc);
            let mut cur = x;
            for _ in 0..27usize {
                cur = b.binary(Op::Add, Flags::NONE, cur, c1);
                live.push(cur);
            }
            let mut acc = b.binary(Op::Xor, Flags::NONE, x, live[0]);
            for &v in &live[1..] {
                acc = b.binary(Op::Xor, Flags::NONE, acc, v);
            }
            let reloaded = b.load(Ty::I64, Flags::NONE, sptr, 0, MemAttrs::aligned(5));
            let out = b.binary(Op::Xor, Flags::NONE, acc, reloaded);
            b.ret(&[out]);

            let func = b.finish_checked().expect("28-GPR over-aligned leaf LIR builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            skip[c1.index()] = true;
            skip[sptr.index()] = true;

            let ra_exact = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("exact-CFG over-aligned leaf regalloc succeeds");
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
            .expect("RPO linear-scan over-aligned leaf regalloc succeeds");

            for (label, ra) in [("exact_cfg", &ra_exact), ("linear_scan_rpo", &ra_rpo)] {
                assert!(!ra.has_calls, "{label}: over-aligned leaf still has has_calls == false");
                assert!(ra.uses_lr, "{label}: over-aligned 28-GPR leaf may still allocate X30");
                assert!(!ra.uses_fp, "{label}: over-aligned (align 32) leaf must never set uses_fp");
                assert!(
                    !uses_x29(&ra.loc_of),
                    "{label}: over-aligned (align 32) leaf must never allocate X29"
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
            let mut invs = Vec::with_capacity_in(22, alloc);
            let mut cur = x;
            for _ in 0..22usize {
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

    #[test]
    fn linear_scan_rpo_coalesces_forward_merge_block_params() {
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
        let check1 = b.create_block(BlockKind::Normal);
        let latch = b.create_block(BlockKind::Normal);
        let done = b.create_block(BlockKind::Normal);
        let merge = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let limit = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let init_pos = b.binary(Op::Add, Flags::NONE, seed, c1);
        let init_acc = b.binary(Op::Xor, Flags::NONE, seed, limit);
        let init_cond = b.icmp(IntCond::Ult, seed, limit);
        b.br(
            Flags::NONE,
            init_cond,
            (loop_hdr, &[init_pos, init_acc]),
            (merge, &[init_pos, seed]),
        );

        b.begin(loop_hdr);
        let pos = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let guard = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let pos0 = b.binary(Op::Add, Flags::NONE, pos, c1);
        let cur8 = b.load(Ty::I64, Flags::NONE, ptr, 8, MemAttrs::aligned(8));
        let tag0 = b.binary(Op::Xor, Flags::NONE, cur8, acc);
        let no_hit0 = b.icmp(IntCond::Ne, tag0, guard);
        b.br(Flags::NONE, no_hit0, (check1, &[]), (merge, &[pos0, cur8]));

        b.begin(check1);
        let pos1 = b.binary(Op::Add, Flags::NONE, pos, c3);
        let tag1 = b.binary(Op::Add, Flags::NONE, cur8, pos1);
        let no_hit1 = b.icmp(IntCond::Ne, tag1, guard);
        b.br(Flags::NONE, no_hit1, (latch, &[]), (merge, &[pos1, cur8]));

        b.begin(latch);
        let pos_next = b.binary(Op::Add, Flags::NONE, pos, c1);
        let acc_next = b.binary(Op::Xor, Flags::NONE, acc, guard);
        let cont = b.icmp(IntCond::Ult, pos_next, limit);
        b.br(Flags::NONE, cont, (loop_hdr, &[pos_next, acc_next]), (done, &[acc_next]));

        b.begin(done);
        let final_acc = b.param(Ty::I64);
        let out = b.binary(Op::Xor, Flags::NONE, final_acc, seed);
        b.ret(&[out]);

        b.begin(merge);
        let hit_pos = b.param(Ty::I64);
        let cur8_m = b.param(Ty::I64);
        let res = b.binary(Op::Add, Flags::NONE, hit_pos, cur8_m);
        b.ret(&[res]);

        let func = b.finish_checked().expect("forward merge coalescing LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        skip[c1.index()] = true;
        skip[c3.index()] = true;

        let ra = allocate_linear_scan_rpo(
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
        .expect("RPO linear-scan forward merge regalloc succeeds");

        assert!(
            matches!(ra.loc_of[cur8.index()], AllocLoc::Gpr(_)),
            "shared loop value `cur8` must live in a GPR, got {:?}",
            ra.loc_of[cur8.index()]
        );
        assert_eq!(
            ra.loc_of[cur8_m.index()],
            ra.loc_of[cur8.index()],
            "merge param `cur8_m` must coalesce into shared incoming `cur8` GPR (0 moves on both loop exits)"
        );
        assert_ne!(
            ra.loc_of[cur8_m.index()],
            ra.loc_of[seed.index()],
            "deeper loop-exit `cur8` must beat cold depth-0 entry edge `seed`"
        );
        assert!(
            matches!(ra.loc_of[hit_pos.index()], AllocLoc::Gpr(_)),
            "merge param `hit_pos` must live in a GPR, got {:?}",
            ra.loc_of[hit_pos.index()]
        );
        assert!(
            ra.loc_of[hit_pos.index()] == ra.loc_of[pos0.index()]
                || ra.loc_of[hit_pos.index()] == ra.loc_of[pos1.index()],
            "merge param `hit_pos` ({:?}) must coalesce into incoming `pos0` ({:?}) or `pos1` ({:?})",
            ra.loc_of[hit_pos.index()],
            ra.loc_of[pos0.index()],
            ra.loc_of[pos1.index()]
        );
    }

    #[test]
    fn exact_cfg_coalesces_loop_diamond_forward_edges_zero_moves() {
        let alloc = test_alloc();

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
        let fast = b.create_block(BlockKind::Normal);
        let slow = b.create_block(BlockKind::Normal);
        let merge = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c5 = b.iconst(Ty::I64, 5);
        b.jump((loop_hdr, &[c0, seed, n]));

        b.begin(loop_hdr);
        let iv = b.param(Ty::I64);
        let st0 = b.param(Ty::I64);
        let st1 = b.param(Ty::I64);
        let w = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let tag = b.binary(Op::And, Flags::NONE, w, c1);
        let take_fast = b.icmp(IntCond::Eq, tag, c0);
        b.br(Flags::NONE, take_fast, (fast, &[]), (slow, &[]));

        b.begin(fast);
        let f0 = b.binary(Op::Add, Flags::NONE, st0, w);
        let tmp_a = b.binary(Op::Xor, Flags::NONE, st1, c3);
        let f1 = b.binary(Op::Add, Flags::NONE, tmp_a, f0);
        let tmp_b = b.binary(Op::Add, Flags::NONE, w, c5);
        let f2 = b.binary(Op::Xor, Flags::NONE, st1, tmp_b);
        b.jump((merge, &[f0, f1, f2]));

        b.begin(slow);
        let s0 = b.binary(Op::Xor, Flags::NONE, st0, c5);
        let s1 = b.binary(Op::Sub, Flags::NONE, st1, w);
        let s2 = b.binary(Op::Add, Flags::NONE, s0, s1);
        b.jump((merge, &[s0, s1, s2]));

        b.begin(merge);
        let p0 = b.param(Ty::I64);
        let p1 = b.param(Ty::I64);
        let p2 = b.param(Ty::I64);
        let mix = b.binary(Op::Xor, Flags::NONE, p0, p1);
        let next0 = b.binary(Op::Add, Flags::NONE, mix, p2);
        let next1 = b.binary(Op::Xor, Flags::NONE, p1, p2);
        let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
        let cont = b.icmp(IntCond::Ult, iv_next, n);
        b.br(
            Flags::NONE,
            cont,
            (loop_hdr, &[iv_next, next0, next1]),
            (exit, &[next0]),
        );

        b.begin(exit);
        let out = b.param(Ty::I64);
        b.ret(&[out]);

        let func = b.finish_checked().expect("loop diamond forward-edge LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        skip[c1.index()] = true;
        skip[c3.index()] = true;
        skip[c5.index()] = true;

        let ra = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG loop diamond regalloc succeeds");

        for (name, arg, param) in [("p0", f0, p0), ("p1", f1, p1), ("p2", f2, p2)] {
            assert!(
                matches!(ra.loc_of[param.index()], AllocLoc::Gpr(_)),
                "merge param `{name}` must live in a GPR, got {:?}",
                ra.loc_of[param.index()]
            );
            assert_eq!(
                ra.loc_of[arg.index()],
                ra.loc_of[param.index()],
                "fast-arm arg for `{name}` must coalesce with `{name}` (0 forward-edge movs, no cycle swap)"
            );
        }
        for (name, arg, param) in [("p0", s0, p0), ("p1", s1, p1), ("p2", s2, p2)] {
            assert_eq!(
                ra.loc_of[arg.index()],
                ra.loc_of[param.index()],
                "slow-arm arg for `{name}` must also coalesce with `{name}` (0 forward-edge movs)"
            );
        }
    }

    #[test]
    fn exact_cfg_bidirectional_coalescing_when_param_colored_first() {
        let alloc = test_alloc();

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
        // Lay out `merge` before `arm_a`/`arm_b` (still a forward edge in RPO) so `p` has
        // strictly higher weight than `arg_a` and `arg_b` in Phase 2 and is colored first.
        let merge = b.create_block(BlockKind::Normal);
        let arm_a = b.create_block(BlockKind::Normal);
        let arm_b = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let a0 = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c5 = b.iconst(Ty::I64, 5);
        // 25 cold depth-0 values live into `exit` so Phase 1 overflows and Phase 2 greedy runs.
        let mut cold = Vec::with_capacity_in(25, alloc);
        let mut cur = a0;
        for _ in 0..25usize {
            cur = b.binary(Op::Add, Flags::NONE, cur, c1);
            cold.push(cur);
        }
        b.jump((loop_hdr, &[c0, a0]));

        b.begin(loop_hdr);
        let iv = b.param(Ty::I64);
        let st = b.param(Ty::I64);
        let w = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let tag = b.binary(Op::And, Flags::NONE, w, c1);
        let cond = b.icmp(IntCond::Eq, tag, c0);
        b.br(Flags::NONE, cond, (arm_a, &[]), (arm_b, &[]));

        b.begin(merge);
        let p = b.param(Ty::I64);
        // 5 depth-1 uses of `p` so Phase 2 colors `p` before `st`, `w`, `mid_tmp`, `arg_a`, `arg_b`.
        let u1 = b.binary(Op::Add, Flags::NONE, p, st);
        let u2 = b.binary(Op::Xor, Flags::NONE, u1, p);
        let u3 = b.binary(Op::Add, Flags::NONE, u2, p);
        let u4 = b.binary(Op::Xor, Flags::NONE, u3, p);
        let next_st = b.binary(Op::Add, Flags::NONE, u4, p);
        let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
        let keep = b.icmp(IntCond::Ult, iv_next, n);
        b.br(
            Flags::NONE,
            keep,
            (loop_hdr, &[iv_next, next_st]),
            (exit, &[next_st]),
        );

        b.begin(arm_a);
        let mid_tmp = b.binary(Op::Xor, Flags::NONE, st, w);
        let arg_a = b.binary(Op::Add, Flags::NONE, st, c3);
        // 3 depth-1 uses of `mid_tmp` after `arg_a` so `mid_tmp` interferes with `arg_a` (not `p`)
        // and sorts between `p` and `arg_a` in Phase 2.
        let m1 = b.binary(Op::Add, Flags::NONE, mid_tmp, w);
        let m2 = b.binary(Op::Xor, Flags::NONE, m1, mid_tmp);
        let m3 = b.binary(Op::Add, Flags::NONE, m2, mid_tmp);
        b.store(Flags::NONE, m3, ptr, 8, MemAttrs::aligned(8));
        b.jump((merge, &[arg_a]));

        b.begin(arm_b);
        let arg_b = b.binary(Op::Xor, Flags::NONE, st, c5);
        b.jump((merge, &[arg_b]));

        b.begin(exit);
        let mut out = b.param(Ty::I64);
        out = b.binary(Op::Xor, Flags::NONE, out, a0);
        for &cv in &cold {
            out = b.binary(Op::Xor, Flags::NONE, out, cv);
        }
        b.ret(&[out]);

        let func = b
            .finish_checked()
            .expect("bidirectional param-first coalescing LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        skip[c1.index()] = true;
        skip[c3.index()] = true;
        skip[c5.index()] = true;

        let ra = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG bidirectional coalescing regalloc succeeds");

        assert!(
            ra.num_spill_slots > 0,
            "must trigger Phase 2 greedy coloring via cold depth-0 pressure"
        );
        assert!(
            matches!(ra.loc_of[p.index()], AllocLoc::Gpr(_)),
            "hot merge param `p` must stay in a GPR, got {:?}",
            ra.loc_of[p.index()]
        );
        assert_eq!(
            ra.loc_of[arg_a.index()],
            ra.loc_of[p.index()],
            "forward arg `arg_a` must coalesce with higher-weight `p` without `mid_tmp` ({:?}) stealing its GPR",
            ra.loc_of[mid_tmp.index()]
        );
        assert_eq!(
            ra.loc_of[arg_b.index()],
            ra.loc_of[p.index()],
            "second forward arg `arg_b` must also coalesce with `p` via bidirectional affinity"
        );
        assert_ne!(
            ra.loc_of[mid_tmp.index()],
            ra.loc_of[p.index()],
            "interfering `mid_tmp` must not collide with co-colored `(p, arg_a)`"
        );
    }

    #[test]
    fn exact_cfg_anti_hint_avoids_stealing_phi_partner_reg() {
        let alloc = test_alloc();

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
        let check1 = b.create_block(BlockKind::Normal);
        let hit = b.create_block(BlockKind::Normal);
        let latch = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let a0 = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        // 25 cold depth-0 values live into `exit` so Phase 1 overflows and Phase 2 greedy runs.
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
        // High-weight loop header param `p0` colors first in Phase 2 and co-colors with its
        // back-edge `p0_next` (leaving forward merge param `hit_pos` uncolored).
        let u1 = b.binary(Op::Add, Flags::NONE, p0, n);
        let u2 = b.binary(Op::Xor, Flags::NONE, u1, p0);
        let u3 = b.binary(Op::Add, Flags::NONE, u2, p0);
        let u4 = b.binary(Op::Xor, Flags::NONE, u3, p0);
        let u5 = b.binary(Op::Add, Flags::NONE, u4, p0);
        let u6 = b.binary(Op::Xor, Flags::NONE, u5, p0);
        let cond_hit0 = b.icmp(IntCond::Eq, u6, c0);
        b.br(Flags::NONE, cond_hit0, (hit, &[p0]), (check1, &[]));

        b.begin(check1);
        // Position 1 forward edge at the same depth; `p0` dies here so `p0` never interferes
        // with `hit_pos` or `hot_tmp`.
        let p1 = b.binary(Op::Add, Flags::NONE, p0, c3);
        let cond_hit1 = b.icmp(IntCond::Eq, p1, c0);
        b.br(Flags::NONE, cond_hit1, (hit, &[p1]), (latch, &[p1]));

        b.begin(hit);
        let hit_pos = b.param(Ty::I64);
        // Unhinted `hot_tmp` has higher weight than `hit_pos` (colors BEFORE `hit_pos` in Phase 2),
        // interferes with `hit_pos` (NOT `p0` or `p0_next`), so without anti-hinting it would grab
        // `p0`'s register and block `hit_pos` from coalescing into `p0`.
        let hot_tmp = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let t1 = b.binary(Op::Add, Flags::NONE, hot_tmp, hot_tmp);
        let t2 = b.binary(Op::Xor, Flags::NONE, t1, hot_tmp);
        let t3 = b.binary(Op::Add, Flags::NONE, t2, hot_tmp);
        let t4 = b.binary(Op::Xor, Flags::NONE, t3, hot_tmp);
        let hit_out = b.binary(Op::Add, Flags::NONE, t4, hit_pos);
        b.jump((latch, &[hit_out]));

        b.begin(latch);
        let acc = b.param(Ty::I64);
        b.store(Flags::NONE, acc, ptr, 8, MemAttrs::aligned(8));
        let p0_next = b.load(Ty::I64, Flags::NONE, ptr, 16, MemAttrs::aligned(8));
        let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
        let cont = b.icmp(IntCond::Ult, iv_next, n);
        b.br(
            Flags::NONE,
            cont,
            (loop_hdr, &[p0_next, iv_next]),
            (exit, &[acc]),
        );

        b.begin(exit);
        let mut out = b.param(Ty::I64);
        out = b.binary(Op::Xor, Flags::NONE, out, a0);
        for &cv in &cold {
            out = b.binary(Op::Xor, Flags::NONE, out, cv);
        }
        b.ret(&[out]);

        let func = b
            .finish_checked()
            .expect("anti-hint exact-CFG coalescing LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        skip[c1.index()] = true;
        skip[c3.index()] = true;

        let ra = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG anti-hint regalloc succeeds");

        assert!(
            ra.num_spill_slots > 0,
            "must trigger Phase 2 greedy coloring via cold depth-0 pressure"
        );
        assert!(
            matches!(ra.loc_of[p0.index()], AllocLoc::Gpr(_)),
            "loop param `p0` must stay in a GPR, got {:?}",
            ra.loc_of[p0.index()]
        );
        assert_eq!(
            ra.loc_of[p0_next.index()],
            ra.loc_of[p0.index()],
            "back-edge `p0_next` must co-color with loop param `p0`"
        );
        assert_ne!(
            ra.loc_of[hot_tmp.index()],
            ra.loc_of[p0.index()],
            "interfering unhinted `hot_tmp` must avoid stealing `p0`'s GPR ({:?})",
            ra.loc_of[p0.index()]
        );
        assert_eq!(
            ra.loc_of[hit_pos.index()],
            ra.loc_of[p0.index()],
            "merge param `hit_pos` must coalesce into `p0`'s GPR (0 moves on Position 0 hit)"
        );
    }

    #[test]
    fn exact_cfg_phi_sibling_affinity_coalesces_merge_args_zero_moves() {
        let alloc = test_alloc();

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let arm0 = b.create_block(BlockKind::Normal);
        let arm1 = b.create_block(BlockKind::Normal);
        let merge = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let cond = b.icmp(IntCond::Eq, x, c0);
        b.br(Flags::NONE, cond, (arm0, &[]), (arm1, &[]));

        b.begin(arm0);
        // `u0` stays live across `val0` (stored after `val0`), forcing `val0` off `X9` onto `X10`.
        let u0 = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let val0 = b.binary(Op::Add, Flags::NONE, x, y);
        b.store(Flags::NONE, u0, ptr, 8, MemAttrs::aligned(8));
        b.jump((merge, &[val0]));

        b.begin(arm1);
        // In `arm1`, `u0` is dead so `X9` is free; without phi-sibling affinity, `val1`
        // (colored before `merge` param `m`) would grab `X9` instead of matching `val0`'s `X10`.
        let val1 = b.binary(Op::Xor, Flags::NONE, x, y);
        b.jump((merge, &[val1]));

        b.begin(merge);
        let m = b.param(Ty::I64);
        let t = b.binary(Op::Add, Flags::NONE, m, x);
        let out = b.binary(Op::Xor, Flags::NONE, t, y);
        b.ret(&[out]);

        let func = b
            .finish_checked()
            .expect("phi-sibling diamond merge LIR builds");
        let view = func.view();
        let n_inst = view.len();
        let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            skip.push(false);
            tail.push(false);
        }
        skip[c0.index()] = true;

        let ra = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
            .expect("exact-CFG phi-sibling regalloc succeeds");

        assert!(
            matches!(ra.loc_of[m.index()], AllocLoc::Gpr(_)),
            "merge param `m` must live in a GPR, got {:?}",
            ra.loc_of[m.index()]
        );
        assert_eq!(
            ra.loc_of[val0.index()],
            ra.loc_of[val1.index()],
            "non-interfering phi-sibling args `val0` and `val1` must receive the same GPR"
        );
        assert_eq!(
            ra.loc_of[val0.index()],
            ra.loc_of[m.index()],
            "merge param `m` must coalesce with `val0` (0 moves on arm0 -> merge)"
        );
        assert_eq!(
            ra.loc_of[val1.index()],
            ra.loc_of[m.index()],
            "merge param `m` must coalesce with `val1` (0 moves on arm1 -> merge)"
        );
        assert_ne!(
            ra.loc_of[u0.index()],
            ra.loc_of[val0.index()],
            "interfering `u0` and `val0` must hold distinct GPRs"
        );
        assert_ne!(ra.loc_of[m.index()], ra.loc_of[x.index()]);
        assert_ne!(ra.loc_of[m.index()], ra.loc_of[y.index()]);
    }

    #[test]
    fn exact_cfg_pair_swap_refinement_untangles_two_cycle_swap() {
        let alloc = test_alloc();

        // 1. Direct verification on `refine_gpr_moves`: every node in the 2-cycle swap
        //    (`pos0/pos1 = X2, val0/val1 = X4` feeding `hit_pos = X4, hit_val = X2`) is
        //    blocked for single-variable moves, so only pair-swap refinement can untangle it.
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
            let pred0 = b.create_block(BlockKind::Normal);
            let pred1 = b.create_block(BlockKind::Normal);
            let merge = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let ptr = b.param(Ty::PTR);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let live_x = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
            let cond = b.icmp(IntCond::Eq, x, c0);
            b.br(Flags::NONE, cond, (pred0, &[]), (pred1, &[]));

            b.begin(pred0);
            let pos0 = b.binary(Op::Add, Flags::NONE, x, y);
            let val0 = b.binary(Op::Xor, Flags::NONE, x, live_x);
            b.jump((merge, &[pos0, val0]));

            b.begin(pred1);
            let pos1 = b.binary(Op::Sub, Flags::NONE, x, y);
            let val1 = b.binary(Op::Or, Flags::NONE, y, live_x);
            b.jump((merge, &[pos1, val1]));

            b.begin(merge);
            let hit_pos = b.param(Ty::I64);
            let hit_val = b.param(Ty::I64);
            let sum = b.binary(Op::Add, Flags::NONE, hit_pos, hit_val);
            let out = b.binary(Op::Xor, Flags::NONE, sum, live_x);
            b.ret(&[out]);

            let func = b
                .finish_checked()
                .expect("2-cycle swap refinement LIR builds");
            let view = func.view();
            let n_inst = view.len();

            let mut val_id: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut val_inst: Vec<usize, Alloc<'static>> = Vec::new_in(alloc);
            for _ in 0..n_inst {
                val_id.push(super::NO_VAL);
            }
            for inst in 0..n_inst {
                let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
                if ty != Ty::NONE && inst != c0.index() {
                    let id = u32_of(val_inst.len());
                    val_inst.push(inst);
                    val_id[inst] = id;
                }
            }
            let num_vals = val_inst.len();
            assert!(num_vals <= 64);
            let val_words = 1usize;
            let mut interf: Vec<u64, Alloc<'static>> = Vec::with_capacity_in(num_vals, alloc);
            for _ in 0..num_vals {
                interf.push(0);
            }
            let mut add_interf = |a: usize, c: usize| {
                let ua = super::idx(val_id[a]);
                let uc = super::idx(val_id[c]);
                interf[ua] |= 1u64 << uc;
                interf[uc] |= 1u64 << ua;
            };
            add_interf(pos0.index(), val0.index());
            add_interf(pos1.index(), val1.index());
            add_interf(hit_pos.index(), hit_val.index());
            for v in [
                x.index(),
                y.index(),
                pos0.index(),
                val0.index(),
                pos1.index(),
                val1.index(),
                hit_pos.index(),
                hit_val.index(),
                sum.index(),
            ] {
                add_interf(live_x.index(), v);
            }
            add_interf(x.index(), y.index());
            add_interf(x.index(), pos0.index());
            add_interf(y.index(), pos1.index());

            let mut loc: Vec<AllocLoc, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut crosses: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut clobbers: Vec<CallClobbers, Alloc<'static>> =
                Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                loc.push(AllocLoc::None);
                crosses.push(false);
                clobbers.push(CallClobbers::EMPTY);
            }
            loc[ptr.index()] = AllocLoc::Gpr(0);
            loc[x.index()] = AllocLoc::Gpr(1);
            loc[y.index()] = AllocLoc::Gpr(3);
            loc[live_x.index()] = AllocLoc::Gpr(9);
            loc[cond.index()] = AllocLoc::Gpr(10);
            // 2-cycle swap: preds hold (pos = X2, val = X4), merge holds (hit_pos = X4, hit_val = X2).
            loc[pos0.index()] = AllocLoc::Gpr(2);
            loc[val0.index()] = AllocLoc::Gpr(4);
            loc[pos1.index()] = AllocLoc::Gpr(2);
            loc[val1.index()] = AllocLoc::Gpr(4);
            loc[hit_pos.index()] = AllocLoc::Gpr(4);
            loc[hit_val.index()] = AllocLoc::Gpr(2);
            loc[sum.index()] = AllocLoc::Gpr(4);
            loc[out.index()] = AllocLoc::Gpr(0);

            let coalesce_edges = [
                (u32_of(pos0.index()), u32_of(hit_pos.index()), 128u64),
                (u32_of(val0.index()), u32_of(hit_val.index()), 128u64),
                (u32_of(pos1.index()), u32_of(hit_pos.index()), 128u64),
                (u32_of(val1.index()), u32_of(hit_val.index()), 128u64),
            ];
            let mut used_callee_gpr = [false; 32];

            let changed = super::refine_gpr_moves(
                &view,
                &[],
                crate::Os::MacOs,
                &val_id,
                &val_inst,
                val_words,
                &interf,
                &crosses,
                &clobbers,
                &coalesce_edges,
                &[],
                1,
                true,
                false,
                super::CALLEE_SAVED_GPR_WITH_LR,
                &mut loc,
                &mut used_callee_gpr,
                alloc,
            );
            assert!(changed, "pair-swap refinement must untangle the 2-cycle swap");

            let residual_cost: u64 = coalesce_edges
                .iter()
                .filter(|&&(a, p, _)| loc[super::idx(a)] != loc[super::idx(p)])
                .map(|&(_, _, w)| w)
                .sum();
            assert_eq!(
                residual_cost, 0,
                "expected 0 residual move cost after pair-swap refinement (hit_pos={:?}, hit_val={:?}, pos0={:?}, val0={:?}, pos1={:?}, val1={:?})",
                loc[hit_pos.index()],
                loc[hit_val.index()],
                loc[pos0.index()],
                loc[val0.index()],
                loc[pos1.index()],
                loc[val1.index()],
            );
            assert_ne!(loc[hit_pos.index()], loc[hit_val.index()]);
            assert!(
                used_callee_gpr.iter().all(|&u| !u),
                "pair-swap must not allocate any callee-saved GPRs"
            );
            for (u_vid, &u_inst) in val_inst.iter().enumerate() {
                let mut bits = interf[u_vid];
                while bits != 0 {
                    let v_vid = super::idx(bits.trailing_zeros());
                    bits &= bits - 1;
                    let v_inst = val_inst[v_vid];
                    assert_ne!(
                        loc[u_inst], loc[v_inst],
                        "interference violation between inst {u_inst} and {v_inst}: both hold {:?}",
                        loc[u_inst]
                    );
                }
            }
        }

        // 2. End-to-end `allocate` verification: reversed entry edge `(a, b) -> merge` vs
        //    `(b, a) -> loop_hdr` must align `merge(m0, m1)` with the hot loop-exit `(lp0, lp1)`
        //    with zero moves on all loop-exit and back-edges.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::PTR),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let loop_hdr = b.create_block(BlockKind::Normal);
            let check1 = b.create_block(BlockKind::Normal);
            let latch = b.create_block(BlockKind::Normal);
            let merge = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let ptr = b.param(Ty::PTR);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let n = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let empty = b.icmp(IntCond::Eq, n, c0);
            b.br(
                Flags::NONE,
                empty,
                (merge, &[a, bv]),
                (loop_hdr, &[bv, a, c0]),
            );

            b.begin(loop_hdr);
            let lp0 = b.param(Ty::I64);
            let lp1 = b.param(Ty::I64);
            let iv = b.param(Ty::I64);
            let w = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
            let hit0 = b.icmp(IntCond::Eq, w, lp0);
            b.br(Flags::NONE, hit0, (merge, &[lp0, lp1]), (check1, &[]));

            b.begin(check1);
            let hit1 = b.icmp(IntCond::Eq, w, lp1);
            b.br(Flags::NONE, hit1, (merge, &[lp0, lp1]), (latch, &[]));

            b.begin(latch);
            let lp0_next = b.binary(Op::Add, Flags::NONE, lp0, c1);
            let lp1_next = b.binary(Op::Xor, Flags::NONE, lp1, w);
            let iv_next = b.binary(Op::Add, Flags::NONE, iv, c1);
            let cont = b.icmp(IntCond::Ult, iv_next, n);
            b.br(
                Flags::NONE,
                cont,
                (loop_hdr, &[lp0_next, lp1_next, iv_next]),
                (merge, &[lp0_next, lp1_next]),
            );

            b.begin(merge);
            let m0 = b.param(Ty::I64);
            let m1 = b.param(Ty::I64);
            let diff = b.binary(Op::Sub, Flags::NONE, m0, m1);
            b.ret(&[diff]);

            let func = b
                .finish_checked()
                .expect("loop-exit 2-param merge LIR builds");
            let view = func.view();
            let n_inst = view.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            skip[c1.index()] = true;

            let ra = allocate(&view, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("exact-CFG loop-exit 2-param merge regalloc succeeds");

            assert!(matches!(ra.loc_of[m0.index()], AllocLoc::Gpr(_)));
            assert!(matches!(ra.loc_of[m1.index()], AllocLoc::Gpr(_)));
            assert_ne!(ra.loc_of[m0.index()], ra.loc_of[m1.index()]);
            assert_eq!(
                ra.loc_of[lp0.index()],
                ra.loc_of[lp0_next.index()],
                "loop back-edge `lp0_next` must coalesce with `lp0`"
            );
            assert_eq!(
                ra.loc_of[lp1.index()],
                ra.loc_of[lp1_next.index()],
                "loop back-edge `lp1_next` must coalesce with `lp1`"
            );
            assert_eq!(
                ra.loc_of[m0.index()],
                ra.loc_of[lp0.index()],
                "merge param `m0` must align with hot loop-exit `lp0` (0 swap moves)"
            );
            assert_eq!(
                ra.loc_of[m1.index()],
                ra.loc_of[lp1.index()],
                "merge param `m1` must align with hot loop-exit `lp1` (0 swap moves)"
            );
        }
    }

    #[test]
    fn exact_cfg_pair_swap_evicts_zero_affinity_scratch_blocker() {
        let alloc = test_alloc();

        // 1. Exact `short_hit` + `xor_diff` scenario (`lz77_scan_default_leaf`):
        //    - `pos0 = X2, val0 = X4` anchored to `long_hit(long_pos = X2, long_val = X4)`.
        //    - `short_hit` holds swapped `(short_pos = X4, short_val = X2)` AND a pure 1-block
        //      zero-affinity scratch `xor_diff = xor(short_val, loaded)` also in `X2` that
        //      interferes with `short_pos`.
        //    - `pred1` holds `val1 = X2` feeding both `short_val` and `long_val` (weight 128 each),
        //      so `val1` cannot move in Round 1.
        //    - Round 1 evicts `xor_diff` to a free caller-saved GPR and swaps
        //      `(short_pos, short_val) -> (X2, X4)`; Round 2 then moves `val1: X2 -> X4` cleanly.
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
            let pred0 = b.create_block(BlockKind::Normal);
            let pred1 = b.create_block(BlockKind::Normal);
            let long_hit = b.create_block(BlockKind::Normal);
            let short_hit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let ptr = b.param(Ty::PTR);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let live_x = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
            let cond = b.icmp(IntCond::Eq, x, c0);
            b.br(Flags::NONE, cond, (pred0, &[]), (pred1, &[]));

            b.begin(pred0);
            let pos0 = b.binary(Op::Add, Flags::NONE, x, y);
            let val0 = b.binary(Op::Xor, Flags::NONE, x, live_x);
            let cond0 = b.icmp(IntCond::Eq, pos0, c0);
            b.br(
                Flags::NONE,
                cond0,
                (long_hit, &[pos0, val0]),
                (short_hit, &[pos0, val0]),
            );

            b.begin(pred1);
            let pos1 = b.binary(Op::Sub, Flags::NONE, x, y);
            let val1 = b.binary(Op::Or, Flags::NONE, y, live_x);
            let cond1 = b.icmp(IntCond::Eq, pos1, c0);
            b.br(
                Flags::NONE,
                cond1,
                (long_hit, &[pos1, val1]),
                (short_hit, &[pos1, val1]),
            );

            b.begin(long_hit);
            let long_pos = b.param(Ty::I64);
            let long_val = b.param(Ty::I64);
            let lsum = b.binary(Op::Add, Flags::NONE, long_pos, long_val);
            let lout = b.binary(Op::Xor, Flags::NONE, lsum, live_x);
            b.ret(&[lout]);

            b.begin(short_hit);
            let short_pos = b.param(Ty::I64);
            let short_val = b.param(Ty::I64);
            let loaded = b.load(Ty::I64, Flags::NONE, ptr, 8, MemAttrs::aligned(8));
            let xor_diff = b.binary(Op::Xor, Flags::NONE, short_val, loaded);
            let sout = b.binary(Op::Add, Flags::NONE, short_pos, xor_diff);
            b.ret(&[sout]);

            let func = b
                .finish_checked()
                .expect("short_hit + xor_diff scratch eviction LIR builds");
            let view = func.view();
            let n_inst = view.len();

            let mut val_id: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut val_inst: Vec<usize, Alloc<'static>> = Vec::new_in(alloc);
            for _ in 0..n_inst {
                val_id.push(super::NO_VAL);
            }
            for inst in 0..n_inst {
                let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
                if ty != Ty::NONE && inst != c0.index() {
                    let id = u32_of(val_inst.len());
                    val_inst.push(inst);
                    val_id[inst] = id;
                }
            }
            let num_vals = val_inst.len();
            assert!(num_vals <= 64);
            let val_words = 1usize;
            let mut interf: Vec<u64, Alloc<'static>> = Vec::with_capacity_in(num_vals, alloc);
            for _ in 0..num_vals {
                interf.push(0);
            }
            let mut add_interf = |a: usize, c: usize| {
                let ua = super::idx(val_id[a]);
                let uc = super::idx(val_id[c]);
                interf[ua] |= 1u64 << uc;
                interf[uc] |= 1u64 << ua;
            };
            for v in [
                x.index(),
                y.index(),
                live_x.index(),
                cond.index(),
                pos0.index(),
                val0.index(),
                cond0.index(),
                pos1.index(),
                val1.index(),
                cond1.index(),
                short_pos.index(),
                short_val.index(),
            ] {
                add_interf(ptr.index(), v);
            }
            for v in [
                x.index(),
                y.index(),
                cond.index(),
                pos0.index(),
                val0.index(),
                cond0.index(),
                pos1.index(),
                val1.index(),
                cond1.index(),
                long_pos.index(),
                long_val.index(),
                lsum.index(),
            ] {
                add_interf(live_x.index(), v);
            }
            add_interf(x.index(), y.index());
            add_interf(x.index(), pos0.index());
            add_interf(y.index(), pos1.index());
            add_interf(pos0.index(), val0.index());
            add_interf(pos0.index(), cond0.index());
            add_interf(val0.index(), cond0.index());
            add_interf(pos1.index(), val1.index());
            add_interf(pos1.index(), cond1.index());
            add_interf(val1.index(), cond1.index());
            add_interf(long_pos.index(), long_val.index());
            // In `short_hit`, `short_val` dies at `xor_diff` (so both legally occupy `X2`), while
            // `short_pos` stays live across `xor_diff` and interferes with BOTH `short_val` and `xor_diff`.
            add_interf(short_pos.index(), short_val.index());
            add_interf(short_pos.index(), loaded.index());
            add_interf(short_val.index(), loaded.index());
            add_interf(short_pos.index(), xor_diff.index());
            add_interf(loaded.index(), xor_diff.index());

            let mut loc: Vec<AllocLoc, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut crosses: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut clobbers: Vec<CallClobbers, Alloc<'static>> =
                Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                loc.push(AllocLoc::None);
                crosses.push(false);
                clobbers.push(CallClobbers::EMPTY);
            }
            loc[ptr.index()] = AllocLoc::Gpr(0);
            loc[x.index()] = AllocLoc::Gpr(1);
            loc[y.index()] = AllocLoc::Gpr(3);
            loc[live_x.index()] = AllocLoc::Gpr(12);
            loc[cond.index()] = AllocLoc::Gpr(13);
            loc[pos0.index()] = AllocLoc::Gpr(2);
            loc[val0.index()] = AllocLoc::Gpr(4);
            loc[cond0.index()] = AllocLoc::Gpr(13);
            loc[pos1.index()] = AllocLoc::Gpr(5);
            loc[val1.index()] = AllocLoc::Gpr(2);
            loc[cond1.index()] = AllocLoc::Gpr(13);
            loc[long_pos.index()] = AllocLoc::Gpr(2);
            loc[long_val.index()] = AllocLoc::Gpr(4);
            loc[lsum.index()] = AllocLoc::Gpr(14);
            loc[lout.index()] = AllocLoc::Gpr(0);
            loc[short_pos.index()] = AllocLoc::Gpr(4);
            loc[short_val.index()] = AllocLoc::Gpr(2);
            loc[loaded.index()] = AllocLoc::Gpr(11);
            loc[xor_diff.index()] = AllocLoc::Gpr(2);
            loc[sout.index()] = AllocLoc::Gpr(0);

            let coalesce_edges = [
                (u32_of(pos0.index()), u32_of(long_pos.index()), 512u64),
                (u32_of(val0.index()), u32_of(long_val.index()), 512u64),
                (u32_of(pos0.index()), u32_of(short_pos.index()), 256u64),
                (u32_of(val0.index()), u32_of(short_val.index()), 256u64),
                (u32_of(val1.index()), u32_of(long_val.index()), 128u64),
                (u32_of(val1.index()), u32_of(short_val.index()), 128u64),
            ];
            let initial_cost: u64 = coalesce_edges
                .iter()
                .filter(|&&(a, p, _)| loc[super::idx(a)] != loc[super::idx(p)])
                .map(|&(_, _, w)| w)
                .sum();
            assert_eq!(initial_cost, 640);

            let mut used_callee_gpr = [false; 32];
            let changed = super::refine_gpr_moves(
                &view,
                &[],
                crate::Os::MacOs,
                &val_id,
                &val_inst,
                val_words,
                &interf,
                &crosses,
                &clobbers,
                &coalesce_edges,
                &[],
                1,
                true,
                false,
                super::CALLEE_SAVED_GPR_WITH_LR,
                &mut loc,
                &mut used_callee_gpr,
                alloc,
            );
            assert!(
                changed,
                "refine_gpr_moves must evict zero-affinity `xor_diff` and swap `(short_pos, short_val)`"
            );

            assert_eq!(
                loc[short_pos.index()],
                AllocLoc::Gpr(2),
                "`short_pos` must swap from X4 to X2"
            );
            assert_eq!(
                loc[short_val.index()],
                AllocLoc::Gpr(4),
                "`short_val` must swap from X2 to X4"
            );
            assert_eq!(
                loc[val1.index()],
                AllocLoc::Gpr(4),
                "`val1` must follow `short_val` and `long_val` to X4 on the next round"
            );
            assert!(
                matches!(loc[xor_diff.index()], AllocLoc::Gpr(r) if r != 2 && r != 4),
                "`xor_diff` must be evicted out of X2/X4 into a free caller-saved GPR, got {:?}",
                loc[xor_diff.index()]
            );

            let residual_cost: u64 = coalesce_edges
                .iter()
                .filter(|&&(a, p, _)| loc[super::idx(a)] != loc[super::idx(p)])
                .map(|&(_, _, w)| w)
                .sum();
            assert!(
                residual_cost < initial_cost,
                "expected strictly lower move cost ({residual_cost} < {initial_cost})"
            );
            assert_eq!(
                residual_cost, 0,
                "expected 0 residual move cost after scratch eviction + swap + round-2 follow-up"
            );
            assert!(
                used_callee_gpr.iter().all(|&u| !u),
                "scratch-evicting pair-swap must not allocate any callee-saved GPRs"
            );
            for (u_vid, &u_inst) in val_inst.iter().enumerate() {
                let mut bits = interf[u_vid];
                while bits != 0 {
                    let v_vid = super::idx(bits.trailing_zeros());
                    bits &= bits - 1;
                    let v_inst = val_inst[v_vid];
                    assert_ne!(
                        loc[u_inst], loc[v_inst],
                        "interference violation between inst {u_inst} and {v_inst}: both hold {:?}",
                        loc[u_inst]
                    );
                }
            }
        }

        // 2. Pair-swap with mutually-interfering zero-affinity scratch blockers on BOTH `r_cand`
        //    (`w_c` in `X2` interfering with `u`) AND `r_u` (`w_u` in `X4` interfering with `v`,
        //    plus `w_c <-> w_u`): verifies both evict to distinct free caller-saved GPRs with
        //    zero interference violations and zero callee-saved expansion.
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
            let long_hit = b.create_block(BlockKind::Normal);
            let short_hit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let ptr = b.param(Ty::PTR);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let pos0 = b.binary(Op::Add, Flags::NONE, x, y);
            let val0 = b.binary(Op::Xor, Flags::NONE, x, y);
            let cond = b.icmp(IntCond::Eq, pos0, c0);
            b.br(
                Flags::NONE,
                cond,
                (long_hit, &[pos0, val0]),
                (short_hit, &[pos0, val0]),
            );

            b.begin(long_hit);
            let lpos = b.param(Ty::I64);
            let lval = b.param(Ty::I64);
            let lout = b.binary(Op::Add, Flags::NONE, lpos, lval);
            b.ret(&[lout]);

            b.begin(short_hit);
            let u = b.param(Ty::I64);
            let v = b.param(Ty::I64);
            let w_c = b.load(Ty::I64, Flags::NONE, ptr, 8, MemAttrs::aligned(8));
            let w_u = b.load(Ty::I64, Flags::NONE, ptr, 16, MemAttrs::aligned(8));
            let t0 = b.binary(Op::Xor, Flags::NONE, u, w_c);
            let t1 = b.binary(Op::Add, Flags::NONE, v, w_u);
            let sout = b.binary(Op::Xor, Flags::NONE, t0, t1);
            b.ret(&[sout]);

            let func = b
                .finish_checked()
                .expect("dual-side scratch eviction LIR builds");
            let view = func.view();
            let n_inst = view.len();

            let mut val_id: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut val_inst: Vec<usize, Alloc<'static>> = Vec::new_in(alloc);
            for _ in 0..n_inst {
                val_id.push(super::NO_VAL);
            }
            for inst in 0..n_inst {
                let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
                if ty != Ty::NONE && inst != c0.index() {
                    let id = u32_of(val_inst.len());
                    val_inst.push(inst);
                    val_id[inst] = id;
                }
            }
            let num_vals = val_inst.len();
            assert!(num_vals <= 64);
            let val_words = 1usize;
            let mut interf: Vec<u64, Alloc<'static>> = Vec::with_capacity_in(num_vals, alloc);
            for _ in 0..num_vals {
                interf.push(0);
            }
            let mut add_interf = |a: usize, c: usize| {
                let ua = super::idx(val_id[a]);
                let uc = super::idx(val_id[c]);
                interf[ua] |= 1u64 << uc;
                interf[uc] |= 1u64 << ua;
            };
            add_interf(pos0.index(), val0.index());
            add_interf(lpos.index(), lval.index());
            add_interf(u.index(), v.index());
            add_interf(u.index(), w_c.index());
            add_interf(v.index(), w_u.index());
            add_interf(w_c.index(), w_u.index());

            let mut loc: Vec<AllocLoc, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut crosses: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut clobbers: Vec<CallClobbers, Alloc<'static>> =
                Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                loc.push(AllocLoc::None);
                crosses.push(false);
                clobbers.push(CallClobbers::EMPTY);
            }
            loc[ptr.index()] = AllocLoc::Gpr(0);
            loc[x.index()] = AllocLoc::Gpr(1);
            loc[y.index()] = AllocLoc::Gpr(3);
            loc[pos0.index()] = AllocLoc::Gpr(2);
            loc[val0.index()] = AllocLoc::Gpr(4);
            loc[cond.index()] = AllocLoc::Gpr(13);
            loc[lpos.index()] = AllocLoc::Gpr(2);
            loc[lval.index()] = AllocLoc::Gpr(4);
            loc[lout.index()] = AllocLoc::Gpr(0);
            loc[u.index()] = AllocLoc::Gpr(4);
            loc[v.index()] = AllocLoc::Gpr(2);
            loc[w_c.index()] = AllocLoc::Gpr(2);
            loc[w_u.index()] = AllocLoc::Gpr(4);
            loc[t0.index()] = AllocLoc::Gpr(11);
            loc[t1.index()] = AllocLoc::Gpr(12);
            loc[sout.index()] = AllocLoc::Gpr(0);

            let coalesce_edges = [
                (u32_of(pos0.index()), u32_of(lpos.index()), 512u64),
                (u32_of(val0.index()), u32_of(lval.index()), 512u64),
                (u32_of(pos0.index()), u32_of(u.index()), 256u64),
                (u32_of(val0.index()), u32_of(v.index()), 256u64),
            ];
            let mut used_callee_gpr = [false; 32];
            let changed = super::refine_gpr_moves(
                &view,
                &[],
                crate::Os::MacOs,
                &val_id,
                &val_inst,
                val_words,
                &interf,
                &crosses,
                &clobbers,
                &coalesce_edges,
                &[],
                1,
                true,
                false,
                super::CALLEE_SAVED_GPR_WITH_LR,
                &mut loc,
                &mut used_callee_gpr,
                alloc,
            );
            assert!(changed, "dual-side scratch eviction pair-swap must succeed");
            assert_eq!(loc[u.index()], AllocLoc::Gpr(2));
            assert_eq!(loc[v.index()], AllocLoc::Gpr(4));
            assert_ne!(
                loc[w_c.index()],
                loc[w_u.index()],
                "mutually-interfering evicted scratch temps must receive distinct GPRs"
            );
            assert!(matches!(loc[w_c.index()], AllocLoc::Gpr(r) if r != 2 && r != 4));
            assert!(matches!(loc[w_u.index()], AllocLoc::Gpr(r) if r != 2 && r != 4));
            assert!(used_callee_gpr.iter().all(|&used| !used));
            for (u_vid, &u_inst) in val_inst.iter().enumerate() {
                let mut bits = interf[u_vid];
                while bits != 0 {
                    let v_vid = super::idx(bits.trailing_zeros());
                    bits &= bits - 1;
                    let v_inst = val_inst[v_vid];
                    assert_ne!(
                        loc[u_inst], loc[v_inst],
                        "interference violation between inst {u_inst} and {v_inst}: both hold {:?}",
                        loc[u_inst]
                    );
                }
            }
        }
    }

    #[test]
    fn exact_cfg_parallel_copy_cycle_swap_cost_breaks_tied_and_pinned_swaps() {
        let alloc = test_alloc();

        // Build a compact LIR CFG with a 2-param `short_hit(short_pos, short_val)` receiving
        // `(pos, val)` on one edge (`entry -> short_hit`) and forwarding `short_pos` and
        // `short_val` along two separate outgoing edges (`ext_pos_blk` and `other_val_blk`).
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let short_hit = b.create_block(BlockKind::Normal);
        let ext_pos_blk = b.create_block(BlockKind::Normal);
        let other_val_blk = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let pos = b.binary(Op::Add, Flags::NONE, a, bv);
        let val = b.binary(Op::Xor, Flags::NONE, a, bv);
        let anchor_pos = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let anchor_val = b.load(Ty::I64, Flags::NONE, ptr, 8, MemAttrs::aligned(8));
        let anchor_ext = b.load(Ty::I64, Flags::NONE, ptr, 16, MemAttrs::aligned(8));
        let anchor_oth = b.load(Ty::I64, Flags::NONE, ptr, 24, MemAttrs::aligned(8));
        b.jump((short_hit, &[pos, val]));

        b.begin(short_hit);
        let short_pos = b.param(Ty::I64);
        let short_val = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Eq, short_pos, c0);
        b.br(
            Flags::NONE,
            cond,
            (ext_pos_blk, &[short_pos]),
            (other_val_blk, &[short_val]),
        );

        b.begin(ext_pos_blk);
        let ext_pos = b.param(Ty::I64);
        let r1a = b.binary(Op::Add, Flags::NONE, ext_pos, anchor_pos);
        let r1 = b.binary(Op::Xor, Flags::NONE, r1a, anchor_ext);
        b.ret(&[r1]);

        b.begin(other_val_blk);
        let other_val = b.param(Ty::I64);
        let r2a = b.binary(Op::Add, Flags::NONE, other_val, anchor_val);
        let r2 = b.binary(Op::Xor, Flags::NONE, r2a, anchor_oth);
        b.ret(&[r2]);

        let func = b
            .finish_checked()
            .expect("parallel-copy cycle-swap test LIR builds");
        let view = func.view();
        let n_inst = view.len();

        let mut val_id: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut val_inst: Vec<usize, Alloc<'static>> = Vec::new_in(alloc);
        for _ in 0..n_inst {
            val_id.push(super::NO_VAL);
        }
        for inst in 0..n_inst {
            let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
            if ty != Ty::NONE && inst != c0.index() {
                let id = u32_of(val_inst.len());
                val_inst.push(inst);
                val_id[inst] = id;
            }
        }
        let num_vals = val_inst.len();
        assert!(num_vals <= 64);
        let val_words = 1usize;
        let mut interf: Vec<u64, Alloc<'static>> = Vec::with_capacity_in(num_vals, alloc);
        for _ in 0..num_vals {
            interf.push(0);
        }
        let mut add_interf = |x: usize, y: usize| {
            let ux = super::idx(val_id[x]);
            let uy = super::idx(val_id[y]);
            interf[ux] |= 1u64 << uy;
            interf[uy] |= 1u64 << ux;
        };
        add_interf(pos.index(), val.index());
        add_interf(short_pos.index(), short_val.index());
        add_interf(short_pos.index(), cond.index());
        add_interf(short_val.index(), cond.index());

        let mut base_loc: Vec<AllocLoc, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut crosses: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut clobbers: Vec<CallClobbers, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            base_loc.push(AllocLoc::None);
            crosses.push(false);
            clobbers.push(CallClobbers::EMPTY);
        }
        base_loc[ptr.index()] = AllocLoc::Gpr(0);
        base_loc[a.index()] = AllocLoc::Gpr(1);
        base_loc[bv.index()] = AllocLoc::Gpr(3);
        base_loc[pos.index()] = AllocLoc::Gpr(2);
        base_loc[val.index()] = AllocLoc::Gpr(4);
        base_loc[anchor_pos.index()] = AllocLoc::Gpr(2);
        base_loc[anchor_val.index()] = AllocLoc::Gpr(4);
        base_loc[anchor_ext.index()] = AllocLoc::Gpr(4);
        base_loc[anchor_oth.index()] = AllocLoc::Gpr(2);
        base_loc[short_pos.index()] = AllocLoc::Gpr(4);
        base_loc[short_val.index()] = AllocLoc::Gpr(2);
        base_loc[cond.index()] = AllocLoc::Gpr(13);
        base_loc[ext_pos.index()] = AllocLoc::Gpr(4);
        base_loc[r1a.index()] = AllocLoc::Gpr(11);
        base_loc[r1.index()] = AllocLoc::Gpr(0);
        base_loc[other_val.index()] = AllocLoc::Gpr(2);
        base_loc[r2a.index()] = AllocLoc::Gpr(12);
        base_loc[r2.index()] = AllocLoc::Gpr(0);

        // 1. Naive-tie 2-cycle swap: `(pos=X2 -> short_pos=X4, val=X4 -> short_val=X2)` on edge 0
        //    vs separate outgoing anchors `(short_pos -> ext_pos=X4)` and `(short_val -> other_val=X2)`.
        //    Under naive 1-move accounting, swapping ties at `256 vs 256` (delta 0, never taken);
        //    with `edge_groups = &[(0, 2)]`, true parallel-copy cost drops `384 -> 256` (3 -> 2 movs)
        //    and breaks the 2-cycle swap cleanly.
        {
            let coalesce_edges = [
                (u32_of(pos.index()), u32_of(short_pos.index()), 128u64),
                (u32_of(val.index()), u32_of(short_val.index()), 128u64),
                (u32_of(short_pos.index()), u32_of(ext_pos.index()), 128u64),
                (u32_of(short_val.index()), u32_of(other_val.index()), 128u64),
                (u32_of(anchor_pos.index()), u32_of(pos.index()), 512u64),
                (u32_of(anchor_val.index()), u32_of(val.index()), 512u64),
                (u32_of(anchor_ext.index()), u32_of(ext_pos.index()), 512u64),
                (u32_of(anchor_oth.index()), u32_of(other_val.index()), 512u64),
            ];

            let mut naive_loc: Vec<AllocLoc, Alloc<'static>> =
                Vec::with_capacity_in(n_inst, alloc);
            for &l in &base_loc {
                naive_loc.push(l);
            }
            let mut naive_callee = [false; 32];
            let naive_changed = super::refine_gpr_moves(
                &view,
                &[],
                crate::Os::MacOs,
                &val_id,
                &val_inst,
                val_words,
                &interf,
                &crosses,
                &clobbers,
                &coalesce_edges,
                &[],
                1,
                true,
                false,
                super::CALLEE_SAVED_GPR_WITH_LR,
                &mut naive_loc,
                &mut naive_callee,
                alloc,
            );
            assert!(
                !naive_changed,
                "without parallel-copy edge groups, tied 2-vs-2 swap must not trigger"
            );

            let mut loc: Vec<AllocLoc, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for &l in &base_loc {
                loc.push(l);
            }
            let mut used_callee_gpr = [false; 32];
            let edge_groups = [(0u32, 2u32)];
            let changed = super::refine_gpr_moves(
                &view,
                &[],
                crate::Os::MacOs,
                &val_id,
                &val_inst,
                val_words,
                &interf,
                &crosses,
                &clobbers,
                &coalesce_edges,
                &edge_groups,
                1,
                true,
                false,
                super::CALLEE_SAVED_GPR_WITH_LR,
                &mut loc,
                &mut used_callee_gpr,
                alloc,
            );
            assert!(
                changed,
                "parallel-copy cycle-swap cost must break the tied 2-cycle swap (3 -> 2 moves)"
            );
            assert_eq!(loc[short_pos.index()], AllocLoc::Gpr(2));
            assert_eq!(loc[short_val.index()], AllocLoc::Gpr(4));
            assert!(used_callee_gpr.iter().all(|&u| !u));
            for (u_vid, &u_inst) in val_inst.iter().enumerate() {
                let mut bits = interf[u_vid];
                while bits != 0 {
                    let v_vid = super::idx(bits.trailing_zeros());
                    bits &= bits - 1;
                    let v_inst = val_inst[v_vid];
                    assert_ne!(loc[u_inst], loc[v_inst]);
                }
            }
        }

        // 2. Heavily-pinned 2-cycle swap: `short_pos` is pinned to X4 (weight 512) while
        //    `short_val` has no outgoing X2 anchor. Pair-swap is unprofitable (loses 512), but
        //    moving `short_val` to a free caller-saved GPR breaks the `X16` 2-cycle swap (3 -> 2
        //    moves, true cost `384 -> 256`) while preserving `short_pos = X4`.
        {
            let coalesce_edges = [
                (u32_of(pos.index()), u32_of(short_pos.index()), 128u64),
                (u32_of(val.index()), u32_of(short_val.index()), 128u64),
                (u32_of(short_pos.index()), u32_of(ext_pos.index()), 512u64),
                (u32_of(anchor_pos.index()), u32_of(pos.index()), 512u64),
                (u32_of(anchor_val.index()), u32_of(val.index()), 512u64),
                (u32_of(anchor_ext.index()), u32_of(ext_pos.index()), 512u64),
            ];

            let mut loc: Vec<AllocLoc, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for &l in &base_loc {
                loc.push(l);
            }
            let mut used_callee_gpr = [false; 32];
            let edge_groups = [(0u32, 2u32)];
            let changed = super::refine_gpr_moves(
                &view,
                &[],
                crate::Os::MacOs,
                &val_id,
                &val_inst,
                val_words,
                &interf,
                &crosses,
                &clobbers,
                &coalesce_edges,
                &edge_groups,
                1,
                true,
                false,
                super::CALLEE_SAVED_GPR_WITH_LR,
                &mut loc,
                &mut used_callee_gpr,
                alloc,
            );
            assert!(
                changed,
                "free-register cycle-breaker must break pinned 2-cycle swap (3 -> 2 moves)"
            );
            assert_eq!(
                loc[short_pos.index()],
                AllocLoc::Gpr(4),
                "heavily-anchored `short_pos` must remain in X4"
            );
            assert!(
                matches!(loc[short_val.index()], AllocLoc::Gpr(r) if r != 2 && r != 4),
                "`short_val` must move out of X2/X4 to a free GPR to eliminate X16 swap, got {:?}",
                loc[short_val.index()]
            );
            assert!(used_callee_gpr.iter().all(|&u| !u));
            for (u_vid, &u_inst) in val_inst.iter().enumerate() {
                let mut bits = interf[u_vid];
                while bits != 0 {
                    let v_vid = super::idx(bits.trailing_zeros());
                    bits &= bits - 1;
                    let v_inst = val_inst[v_vid];
                    assert_ne!(loc[u_inst], loc[v_inst]);
                }
            }
        }

        // 3. End-to-end `allocate_with_ipra_and_order` with `Some(&BlockLayout)`: verifies solved
        //    edge weights and cold flags wire cleanly through `allocate_exact_cfg`.
        {
            let layout = super::super::layout::compute_block_layout(&view, alloc);
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                skip.push(false);
                tail.push(false);
            }
            let ra = allocate_with_ipra_and_order(
                &view,
                &skip,
                &[],
                &tail,
                64,
                crate::Os::MacOs,
                |_| CallClobbers::C_ABI_ARM64,
                None,
                Some(&layout),
                alloc,
            )
            .expect("exact-CFG regalloc with BlockLayout succeeds");
            assert!(matches!(ra.loc_of[short_pos.index()], AllocLoc::Gpr(_)));
            assert!(matches!(ra.loc_of[short_val.index()], AllocLoc::Gpr(_)));
            assert_ne!(ra.loc_of[short_pos.index()], ra.loc_of[short_val.index()]);
        }
    }

    #[test]
    fn exact_cfg_pairwise_edge_group_swap_eliminates_short_hit_moves() {
        let alloc = test_alloc();

        // Models the 2-predecessor `short_hit(short_pos, short_word)` merge block from
        // `lz77_scan_default_leaf`:
        // - `lane1` passes `(pos1 = X2, word1 = X4)`
        // - `lane2` passes `(pos2 = X0, word2 = X4)`
        // - `short_hit` initially holds `(short_pos = X4, short_word = X20)` while `X2`/`X0`
        //   are occupied across `short_pos` by pinned values (`blk_x2`, `blk_x0`).
        // Single-vreg recoloring cannot move `short_word` to `X4` (blocked by `short_pos`) or
        // `short_pos` to `X2`/`X0` (blocked by `blk_x2`/`blk_x0`), so pairwise swap of
        // `(short_pos, short_word)` -> `(X20, X4)` eliminates BOTH `mov x20, x4` edge copies
        // (weighted move cost `512 -> 256`).
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let lane1 = b.create_block(BlockKind::Normal);
        let lane2 = b.create_block(BlockKind::Normal);
        let short_hit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let anchor_x2 = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let anchor_x0 = b.load(Ty::I64, Flags::NONE, ptr, 8, MemAttrs::aligned(8));
        let anchor_x4 = b.load(Ty::I64, Flags::NONE, ptr, 16, MemAttrs::aligned(8));
        let cond = b.icmp(IntCond::Eq, x, c0);
        b.br(Flags::NONE, cond, (lane1, &[]), (lane2, &[]));

        b.begin(lane1);
        let pos1 = b.binary(Op::Add, Flags::NONE, x, y);
        let word1 = b.load(Ty::I64, Flags::NONE, ptr, 24, MemAttrs::aligned(8));
        b.jump((short_hit, &[pos1, word1]));

        b.begin(lane2);
        let pos2 = b.binary(Op::Sub, Flags::NONE, x, y);
        let word2 = b.load(Ty::I64, Flags::NONE, ptr, 32, MemAttrs::aligned(8));
        b.jump((short_hit, &[pos2, word2]));

        b.begin(short_hit);
        let short_pos = b.param(Ty::I64);
        let short_word = b.param(Ty::I64);
        let blk_x2 = b.load(Ty::I64, Flags::NONE, ptr, 40, MemAttrs::aligned(8));
        let blk_x0 = b.load(Ty::I64, Flags::NONE, ptr, 48, MemAttrs::aligned(8));
        let s0 = b.load(Ty::I64, Flags::NONE, ptr, 56, MemAttrs::aligned(8));
        let s1 = b.load(Ty::I64, Flags::NONE, ptr, 64, MemAttrs::aligned(8));
        let s2 = b.load(Ty::I64, Flags::NONE, ptr, 72, MemAttrs::aligned(8));
        let t0 = b.binary(Op::Add, Flags::NONE, short_pos, short_word);
        let t1 = b.binary(Op::Xor, Flags::NONE, t0, blk_x2);
        let t2 = b.binary(Op::Add, Flags::NONE, t1, blk_x0);
        let t3 = b.binary(Op::Xor, Flags::NONE, t2, s0);
        let t4 = b.binary(Op::Add, Flags::NONE, t3, s1);
        let t5 = b.binary(Op::Xor, Flags::NONE, t4, s2);
        let t6 = b.binary(Op::Add, Flags::NONE, t5, anchor_x2);
        let t7 = b.binary(Op::Xor, Flags::NONE, t6, anchor_x0);
        let out = b.binary(Op::Add, Flags::NONE, t7, anchor_x4);
        b.ret(&[out]);

        let func = b
            .finish_checked()
            .expect("2-predecessor short_hit merge LIR builds");
        let view = func.view();
        let n_inst = view.len();

        let mut val_id: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut val_inst: Vec<usize, Alloc<'static>> = Vec::new_in(alloc);
        for _ in 0..n_inst {
            val_id.push(super::NO_VAL);
        }
        for inst in 0..n_inst {
            let ty = view.tys.get(inst).copied().map_or(Ty::NONE, Ty);
            if ty != Ty::NONE && inst != c0.index() {
                let id = u32_of(val_inst.len());
                val_inst.push(inst);
                val_id[inst] = id;
            }
        }
        let num_vals = val_inst.len();
        assert!(num_vals <= 64);
        let val_words = 1usize;

        let coalesce_edges = [
            (u32_of(pos1.index()), u32_of(short_pos.index()), 128u64),
            (u32_of(word1.index()), u32_of(short_word.index()), 128u64),
            (u32_of(pos2.index()), u32_of(short_pos.index()), 128u64),
            (u32_of(word2.index()), u32_of(short_word.index()), 128u64),
            (u32_of(anchor_x2.index()), u32_of(pos1.index()), 512u64),
            (u32_of(anchor_x0.index()), u32_of(pos2.index()), 512u64),
            (u32_of(anchor_x4.index()), u32_of(word1.index()), 512u64),
            (u32_of(anchor_x4.index()), u32_of(word2.index()), 512u64),
            (u32_of(anchor_x2.index()), u32_of(blk_x2.index()), 512u64),
            (u32_of(anchor_x0.index()), u32_of(blk_x0.index()), 512u64),
        ];
        let edge_groups = [(0u32, 2u32), (2u32, 4u32)];

        let mut crosses: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        let mut clobbers: Vec<CallClobbers, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
        for _ in 0..n_inst {
            crosses.push(false);
            clobbers.push(CallClobbers::EMPTY);
        }

        // 1. Clean colliding merge: `(short_pos=X4, short_word=X20)` swaps to `(X20, X4)`.
        // 2. Same merge with 3 zero-affinity scratch temps (`s0`, `s1`, `s2`) also in `X4`
        //    interfering with `short_word`: all 3 evict cleanly and `(short_pos, short_word)`
        //    still swaps to `(X20, X4)`.
        for with_three_scratch_blockers in [false, true] {
            let mut interf: Vec<u64, Alloc<'static>> = Vec::with_capacity_in(num_vals, alloc);
            for _ in 0..num_vals {
                interf.push(0);
            }
            let mut add_interf = |u: usize, v: usize| {
                let uu = super::idx(val_id[u]);
                let uv = super::idx(val_id[v]);
                interf[uu] |= 1u64 << uv;
                interf[uv] |= 1u64 << uu;
            };
            add_interf(pos1.index(), word1.index());
            add_interf(pos2.index(), word2.index());
            add_interf(short_pos.index(), short_word.index());
            add_interf(short_pos.index(), blk_x2.index());
            add_interf(short_pos.index(), blk_x0.index());
            add_interf(short_word.index(), blk_x2.index());
            add_interf(short_word.index(), blk_x0.index());
            if with_three_scratch_blockers {
                add_interf(short_word.index(), s0.index());
                add_interf(short_word.index(), s1.index());
                add_interf(short_word.index(), s2.index());
            }

            let mut loc: Vec<AllocLoc, Alloc<'static>> = Vec::with_capacity_in(n_inst, alloc);
            for _ in 0..n_inst {
                loc.push(AllocLoc::None);
            }
            loc[ptr.index()] = AllocLoc::Gpr(3);
            loc[x.index()] = AllocLoc::Gpr(1);
            loc[y.index()] = AllocLoc::Gpr(5);
            loc[anchor_x2.index()] = AllocLoc::Gpr(2);
            loc[anchor_x0.index()] = AllocLoc::Gpr(0);
            loc[anchor_x4.index()] = AllocLoc::Gpr(4);
            loc[cond.index()] = AllocLoc::Gpr(13);
            loc[pos1.index()] = AllocLoc::Gpr(2);
            loc[word1.index()] = AllocLoc::Gpr(4);
            loc[pos2.index()] = AllocLoc::Gpr(0);
            loc[word2.index()] = AllocLoc::Gpr(4);
            loc[short_pos.index()] = AllocLoc::Gpr(4);
            loc[short_word.index()] = AllocLoc::Gpr(20);
            loc[blk_x2.index()] = AllocLoc::Gpr(2);
            loc[blk_x0.index()] = AllocLoc::Gpr(0);
            let scratch_reg = if with_three_scratch_blockers { 4 } else { 6 };
            loc[s0.index()] = AllocLoc::Gpr(scratch_reg);
            loc[s1.index()] = AllocLoc::Gpr(scratch_reg);
            loc[s2.index()] = AllocLoc::Gpr(scratch_reg);
            loc[t0.index()] = AllocLoc::Gpr(9);
            loc[t1.index()] = AllocLoc::Gpr(10);
            loc[t2.index()] = AllocLoc::Gpr(11);
            loc[t3.index()] = AllocLoc::Gpr(12);
            loc[t4.index()] = AllocLoc::Gpr(14);
            loc[t5.index()] = AllocLoc::Gpr(15);
            loc[t6.index()] = AllocLoc::Gpr(8);
            loc[t7.index()] = AllocLoc::Gpr(7);
            loc[out.index()] = AllocLoc::Gpr(0);

            let initial_cost: u64 = coalesce_edges
                .iter()
                .filter(|&&(a, p, _)| loc[super::idx(a)] != loc[super::idx(p)])
                .map(|&(_, _, w)| w)
                .sum();
            assert_eq!(initial_cost, 512);

            let mut used_callee_gpr = [false; 32];
            used_callee_gpr[20] = true;
            let changed = super::refine_gpr_moves(
                &view,
                &[],
                crate::Os::MacOs,
                &val_id,
                &val_inst,
                val_words,
                &interf,
                &crosses,
                &clobbers,
                &coalesce_edges,
                &edge_groups,
                1,
                true,
                false,
                super::CALLEE_SAVED_GPR_WITH_LR,
                &mut loc,
                &mut used_callee_gpr,
                alloc,
            );
            assert!(
                changed,
                "pairwise swap must eliminate colliding `mov x20, x4` copies (scratch={with_three_scratch_blockers})"
            );
            assert_eq!(
                loc[short_word.index()],
                AllocLoc::Gpr(4),
                "`short_word` must coalesce with both incoming lanes' X4"
            );
            assert_eq!(
                loc[short_pos.index()],
                AllocLoc::Gpr(20),
                "`short_pos` must swap into X20"
            );
            let residual_cost: u64 = coalesce_edges
                .iter()
                .filter(|&&(a, p, _)| loc[super::idx(a)] != loc[super::idx(p)])
                .map(|&(_, _, w)| w)
                .sum();
            assert_eq!(
                residual_cost, 256,
                "expected weighted move cost to drop 512 -> 256 (4 moves -> 2 moves)"
            );
            for (r, &used) in used_callee_gpr.iter().enumerate() {
                if used {
                    assert!(
                        r == 19 || r == 20,
                        "pairwise swap must not expand callee-saved pairs beyond X19:X20, got X{r}"
                    );
                }
            }
            for (u_vid, &u_inst) in val_inst.iter().enumerate() {
                let mut bits = interf[u_vid];
                while bits != 0 {
                    let v_vid = super::idx(bits.trailing_zeros());
                    bits &= bits - 1;
                    let v_inst = val_inst[v_vid];
                    assert_ne!(
                        loc[u_inst], loc[v_inst],
                        "interference violation between inst {u_inst} and {v_inst}: both hold {:?}",
                        loc[u_inst]
                    );
                }
            }
        }

        // 3. End-to-end `allocate` on a 2-predecessor merge where both lanes pass `word` in the
        //    same register and distinct `pos1`/`pos2` values: verifies `short_word` matches `word`
        //    with zero `word` copies and at most 1 move across both predecessors.
        {
            let mut b2 = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig2 = b2.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b2.set_signature(sig2);
            let e2 = b2.create_block(BlockKind::Normal);
            let l1 = b2.create_block(BlockKind::Normal);
            let l2 = b2.create_block(BlockKind::Normal);
            let merge = b2.create_block(BlockKind::Normal);

            b2.begin(e2);
            let p2 = b2.param(Ty::PTR);
            let a2 = b2.param(Ty::I64);
            let bv2 = b2.param(Ty::I64);
            let z2 = b2.iconst(Ty::I64, 0);
            let p1_val = b2.binary(Op::Add, Flags::NONE, a2, bv2);
            let p2_val = b2.binary(Op::Sub, Flags::NONE, a2, bv2);
            let word_val = b2.load(Ty::I64, Flags::NONE, p2, 0, MemAttrs::aligned(8));
            let c2 = b2.icmp(IntCond::Eq, a2, z2);
            b2.br(Flags::NONE, c2, (l1, &[]), (l2, &[]));

            b2.begin(l1);
            b2.jump((merge, &[p1_val, word_val]));

            b2.begin(l2);
            b2.jump((merge, &[p2_val, word_val]));

            b2.begin(merge);
            let m_pos = b2.param(Ty::I64);
            let m_word = b2.param(Ty::I64);
            let m_out = b2.binary(Op::Xor, Flags::NONE, m_pos, m_word);
            b2.ret(&[m_out]);

            let func2 = b2
                .finish_checked()
                .expect("end-to-end 2-predecessor merge LIR builds");
            let view2 = func2.view();
            let n2 = view2.len();
            let mut skip: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n2, alloc);
            let mut tail: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(n2, alloc);
            for _ in 0..n2 {
                skip.push(false);
                tail.push(false);
            }
            let ra = allocate(&view2, &skip, &tail, 64, crate::Os::MacOs, alloc)
                .expect("end-to-end 2-predecessor merge regalloc succeeds");
            assert!(matches!(ra.loc_of[m_pos.index()], AllocLoc::Gpr(_)));
            assert!(matches!(ra.loc_of[m_word.index()], AllocLoc::Gpr(_)));
            assert_ne!(ra.loc_of[m_pos.index()], ra.loc_of[m_word.index()]);
            assert_eq!(
                ra.loc_of[m_word.index()],
                ra.loc_of[word_val.index()],
                "`short_word` must stay in `word_val`'s register across both predecessors"
            );
            let total_moves = usize::from(ra.loc_of[p1_val.index()] != ra.loc_of[m_pos.index()])
                + usize::from(ra.loc_of[word_val.index()] != ra.loc_of[m_word.index()])
                + usize::from(ra.loc_of[p2_val.index()] != ra.loc_of[m_pos.index()])
                + usize::from(ra.loc_of[word_val.index()] != ra.loc_of[m_word.index()]);
            assert!(
                total_moves <= 1,
                "expected at most 1 parallel-copy move across both lanes, got {total_moves}"
            );
        }
    }
}

//! Pure SSA `VReg` LIR -> `MFunc` Pattern-Fusing Instruction Selector & Full V3 Pipeline Driver
//! (`misel`) for `TierName::Sota` (V3 Wave 3).
//!
//! Lowers verified `blaze_lir` directly into typed SoA SSA [`MFunc`] (`VReg` + explicit `VFlag`
//! + edge `ParallelCopy` + side-band `MNote`) and executes the complete V3 backend pipeline:
//!
//! ```text
//! Verified LIR
//!   -> check_misel_subset (zero silent fallbacks, Rules 3 & 4)
//!   -> layout::compute_block_layout (Ext-TSP block order & BlockFreq weights)
//!   -> select_mfunc_ssa (SSA MFunc with VRegs, VFlags, MNotes, ParallelCopy on CFG edges)
//!   -> mverify (SSA structural check)
//!   -> mopt::optimize_mfunc (pre-RA SSA peepholes, MovImm CSE, redundant loads, Ldp/Stp, branch opt)
//!   -> msched::schedule_mfunc (pre-RA exact loop-carried recurrence height + TargetModel O(N) window)
//!   -> mra::allocate_mfunc (SSA regalloc: remat, cold-exit split, IPRA CallClobbers, X29 leaf, ParallelCopy)
//!   -> insert_prologue_epilogue (exact callee-saved Stp/Ldp + SP frame + incoming stack-arg patching)
//!   -> mopt::optimize_mfunc (post-RA PostIdx + copy/branch cleanup)
//!   -> msched::schedule_mfunc (post-RA macro-op cmp+b.cc fusion lock)
//!   -> mverify (post-RA check)
//!   -> mfunc::mencode (1-pass binary encoding with >32KB branch relaxation + direct MNote relocs/traps)
//! ```

use blaze_code::record::{
    Held, MOST_PART_ALIGN, OWN_RELRO, PositionField, PositionPart, PositionRow, Space, Symbol,
    Target, TrapKind, TrapRow, Word,
};
use blaze_lir::{
    Block, BlockKind, Flags, FloatCond, IntCond, Op, Ty, Value, Verified, View,
    func::{Conv, DataId, ParamKind, RefId, RefKind, SigId, SigRec},
    ty::Form,
};
use blaze_own::{Alloc, Vec};
use blaze_tier::{
    Arch, CallClobbers, Emitted, Machine, Named, Names, Os, Outside, RegSet, Reloc, RelocKind,
    TierError, TierName,
};

use crate::enc;
use crate::pass::{BRK_DIV_OVERFLOW, BRK_DIV_ZERO, BRK_UNREACHABLE};

use super::CallMode;
use super::layout::{BlockLayout, compute_block_layout, compute_fast_block_layout};
use super::mfunc::{
    AddrMode, DEFAULT_RET_LIVE_GPRS, EncodedMFunc, FpAluOp, LogicalKind, MBlock, MCond,
    MEncodeError, MExt, MFunc, MFuncOpts, MInst, MInstFlags, MNoteKind, MOp, MReg, MShift,
    MVerifyError, MemSize, NO_NOTE, NO_VFLAG, PReg, ShiftKind, VReg, VRegClass, VecAluOp, mencode,
    mverify,
};
use super::mopt::{MOptStats, optimize_mfunc};
use super::mra::{MRaError, MRaOpts, MRaOut, MRaStats, allocate_mfunc};
use super::msched::{MSchedStats, TargetModel, schedule_mfunc};

const TIER: TierName = TierName::Sota;

/// Maximum supported stack frame size (16 MiB, addressable by two 12-bit `sub`/`add sp` immediates).
const FRAME_LIMIT: u32 = 1 << 24;

/// Maximum byte length for inline NEON `Memcpy` / `Memset`.
const INLINE_BULK_MAX: u64 = 256;

/// Maximum byte length for overlap-safe inline `Memmove`.
const INLINE_MEMMOVE_MAX: u64 = 32;

/// Sentinel `u32` meaning "none / unset".
const NONE_U32: u32 = u32::MAX;

// -----------------------------------------------------------------------------
// Zero-`as` numeric helpers
// -----------------------------------------------------------------------------

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(v: usize) -> Option<u32> {
    u32::try_from(v).ok()
}

#[inline]
fn u16_of(v: u32) -> Option<u16> {
    u16::try_from(v).ok()
}

#[inline]
fn u8_of(v: u32) -> Option<u8> {
    u8::try_from(v).ok()
}

#[inline]
fn i32_of(v: u32) -> Option<i32> {
    i32::try_from(v).ok()
}

#[inline]
const fn trunc_u8(v: u64) -> u8 {
    v.to_le_bytes()[0]
}

#[inline]
const fn trunc_u32(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
const fn align_up_u32(val: u32, align: u32) -> Option<u32> {
    let mask = align - 1;
    match val.checked_add(mask) {
        Some(v) => Some(v & !mask),
        None => None,
    }
}

#[inline]
const fn bit_mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

#[inline]
const fn sign_extend_u64(val: u64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        val.cast_signed()
    } else {
        let shift = 64 - bits;
        ((val << shift).cast_signed()) >> shift
    }
}

fn vec_filled<'a, T: Copy>(len: usize, val: T, alloc: Alloc<'a>) -> Vec<T, Alloc<'a>> {
    let mut v = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        v.push(val);
    }
    v
}

#[inline]
fn at<T: Copy>(s: &[T], i: usize, def: T) -> T {
    s.get(i).copied().unwrap_or(def)
}

#[inline]
fn set<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn sat_inc_u32(s: &mut [u32], i: usize, by: u32) {
    if let Some(slot) = s.get_mut(i) {
        *slot = slot.saturating_add(by);
    }
}

#[inline]
fn sat_dec_u32(s: &mut [u32], i: usize, by: u32) {
    if let Some(slot) = s.get_mut(i) {
        *slot = slot.saturating_sub(by);
    }
}

#[inline]
fn ty_at(view: &View<'_>, i: usize) -> Ty {
    view.tys.get(i).copied().map_or(Ty::NONE, Ty)
}

// -----------------------------------------------------------------------------
// Public Options, Statistics & Entry Point (Rules 83, 85)
// -----------------------------------------------------------------------------

/// Options configuring [`emit_mfunc_ssa_a64`] (Rule 83).
#[derive(Clone, Copy, Debug)]
pub struct MIseOpts<'a> {
    pub machine: Machine,
    pub call_mode: CallMode,
    pub scratch_gpr_order: Option<[u8; 16]>,
    pub preserve_all_cold: bool,
    pub call_clobbers: &'a [(u32, CallClobbers)],
    /// When `true`, this function's own signature uses `abi::INTERNAL_FAST_GPR_ARGS`
    /// (`X0..X7, X9..X15`, keeping `X8` for `Sret`) and `abi::INTERNAL_FAST_FP_ARGS`
    /// (`V0..V7, V16..V23`).
    pub internal_fast: bool,
    /// Sorted slice of LIR `Op::Call` instruction indices whose callee uses `InternalFast`.
    pub internal_fast_calls: &'a [u32],
    /// Fast cold-compilation mode ([`super::EmitProfile::Fast`]): uses `O(V + E)` RPO block
    /// layout (skipping Ext-TSP), skips pre-RA & post-RA `mopt` and `msched`, and runs single-pass
    /// bitmask linear-scan `mra`, while preserving exact ABI, IPRA `CallClobbers`, unwind frames,
    /// call sites, traps, and runtime semantics.
    pub fast_mode: bool,
}

/// Aggregate V3 pipeline telemetry returned by [`emit_mfunc_ssa_a64`] (Rule 85).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MIseStats {
    pub mopt_pre: MOptStats,
    pub msched_pre: MSchedStats,
    pub mra: MRaStats,
    pub mopt_post: MOptStats,
    pub msched_post: MSchedStats,
}

/// Compiles verified LIR through the full V3 pure-SSA `MFunc` pipeline into native AArch64 code:
/// `check_misel_subset -> select_mfunc_ssa -> mverify -> mopt (pre-RA) -> msched (pre-RA) ->
///  mra -> prologue/epilogue -> mopt (post-RA) -> msched (post-RA) -> mverify -> mencode`.
pub fn emit_mfunc_ssa_a64<'a>(
    verified: Verified<'_>,
    names: &dyn Names,
    opts: MIseOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<(Emitted<'a>, CallClobbers, MIseStats), TierError> {
    let view = verified.view();

    // 1. Strict upfront subset validation (zero silent fallbacks, Rules 3 & 4).
    check_misel_subset(&view, names, opts)?;

    // 2. Block layout: O(V + E) RPO in fast cold mode; Ext-TSP + BlockFreq in full SOTA mode.
    let layout = if opts.fast_mode {
        compute_fast_block_layout(&view, alloc)
    } else {
        compute_block_layout(&view, alloc)
    };

    // 3. Pure SSA VReg + explicit VFlag + ParallelCopy instruction selection.
    let mut sel = select_mfunc_ssa(&view, names, &layout, opts, alloc)?;

    // 4 & 5. Pre-RA SSA optimization & loop-recurrence scheduling (skipped in fast cold mode;
    // unwind CFG edges are still wired before `allocate_mfunc`).
    let (mopt_pre, msched_pre) = if opts.fast_mode {
        if !sel.unwind_entries.is_empty() {
            add_unwind_cfg_edges(&mut sel.mfunc, &sel.unwind_entries, alloc);
        }
        (MOptStats::default(), MSchedStats::default())
    } else {
        mverify(&sel.mfunc).map_err(map_verify_err)?;
        let target_model = TargetModel::from_machine(opts.machine);
        let mopt_pre = optimize_mfunc(&mut sel.mfunc, alloc);
        if !sel.unwind_entries.is_empty() {
            add_unwind_cfg_edges(&mut sel.mfunc, &sel.unwind_entries, alloc);
        }
        mverify(&sel.mfunc).map_err(|_| TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel internal post-mopt_pre mverify failure",
        })?;
        let msched_pre = schedule_mfunc(&mut sel.mfunc, target_model, alloc);
        mverify(&sel.mfunc).map_err(|_| TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel internal post-msched_pre mverify failure",
        })?;
        (mopt_pre, msched_pre)
    };

    // 6. Rebind per-call IPRA CallClobbers to post-mopt/msched MFunc instruction indices.
    let mfunc_call_clobbers = bind_call_clobbers(&sel.mfunc, &sel.call_clobbers_seq, alloc)?;

    // 7. Linear-time SSA register allocation (fast single-pass bitmask scan in fast cold mode;
    // remat, cold-exit split, coalescing, IPRA, X29 leaf, ParallelCopy in full SOTA mode).
    let mra_opts = MRaOpts {
        allow_x29_leaf: !sel.has_calls && !sel.uses_fp,
        scratch_gpr_order: opts.scratch_gpr_order,
        block_weights: if opts.fast_mode { &[] } else { &sel.block_weights },
        call_clobbers: &mfunc_call_clobbers,
        spill_base_off: sel.spill_base_off,
        fast_mode: opts.fast_mode,
    };
    let mra_out = allocate_mfunc(&mut sel.mfunc, mra_opts, alloc).map_err(map_mra_err)?;

    // 8. Exact AAPCS64 / preserve_all_cold prologue & epilogue lowering + incoming stack-arg patching.
    let frame_info = insert_prologue_epilogue(
        &mut sel.mfunc,
        &mra_out,
        sel.spill_base_off,
        sel.has_calls,
        opts.preserve_all_cold && !sel.has_landing_blocks,
        sel.ret_gpr_mask,
        sel.ret_fp_mask,
        &sel.incoming_stack_params,
        alloc,
    )?;

    // 9. Post-RA optimization (PostIdx + copy/branch cleanup) & macro-op cmp+b.cc scheduling
    // (skipped in fast cold mode).
    let (mopt_post, msched_post) = if opts.fast_mode {
        (MOptStats::default(), MSchedStats::default())
    } else {
        let target_model = TargetModel::from_machine(opts.machine);
        let mopt_post = optimize_mfunc(&mut sel.mfunc, alloc);
        let msched_post = schedule_mfunc(&mut sel.mfunc, target_model, alloc);
        (mopt_post, msched_post)
    };

    // 10. Post-RA structural verification & 1-pass binary encoding with >32KB branch relaxation.
    mverify(&sel.mfunc).map_err(map_verify_err)?;
    let encoded = mencode(&sel.mfunc, alloc).map_err(map_encode_err)?;

    // 11. Assemble final `Emitted<'a>` code record directly from `EncodedMFunc` + side tables.
    let emitted = build_emitted(
        &encoded,
        frame_info.unwind_frame,
        sel.targets,
        sel.relro,
        &sel.trap_kinds,
        &sel.unwind_entries,
        alloc,
    )?;

    let stats = MIseStats {
        mopt_pre,
        msched_pre,
        mra: mra_out.stats,
        mopt_post,
        msched_post,
    };

    Ok((emitted, frame_info.final_clobbers, stats))
}

fn map_verify_err(_err: MVerifyError) -> TierError {
    TierError::Refused {
        tier: TIER,
        inst: None,
        what: "misel internal mverify failure",
    }
}

fn map_mra_err(err: MRaError) -> TierError {
    match err {
        MRaError::FunctionTooLarge | MRaError::InvalidSpillOffset { .. } => TierError::TooLarge {
            tier: TIER,
            what: "register allocation frame or function size",
        },
        MRaError::UndefinedVReg { .. } => TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel mra: UndefinedVReg",
        },
        MRaError::UninitializedSpillSlot { .. } => TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel mra: UninitializedSpillSlot",
        },
        MRaError::SpillScratchExhausted { .. } => TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel mra: SpillScratchExhausted",
        },
        MRaError::LivenessDidNotConverge => TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel mra: LivenessDidNotConverge",
        },
        MRaError::InvalidParallelCopy { .. } => TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel mra: InvalidParallelCopy",
        },
        MRaError::VerifyFailed(_) | MRaError::NotInSsa | MRaError::UnsupportedVRegClass { .. } => {
            TierError::Refused {
                tier: TIER,
                inst: None,
                what: "misel register allocation refused function",
            }
        }
    }
}

fn map_encode_err(err: MEncodeError) -> TierError {
    match err {
        MEncodeError::BranchOutOfRange { .. }
        | MEncodeError::RelaxationDidNotConverge
        | MEncodeError::CodeTooLarge => TierError::TooLarge {
            tier: TIER,
            what: "encoded AArch64 machine code",
        },
        _ => TierError::Refused {
            tier: TIER,
            inst: None,
            what: "misel binary encoding failed",
        },
    }
}

// -----------------------------------------------------------------------------
// Type & ABI Classification Helpers
// -----------------------------------------------------------------------------

#[inline]
const fn is_scalar_int_or_ptr(ty: Ty) -> bool {
    matches!(ty, Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64 | Ty::PTR)
}

#[inline]
const fn is_scalar_fp(ty: Ty) -> bool {
    matches!(ty, Ty::F32 | Ty::F64)
}

#[inline]
const fn is_supported_scalar(ty: Ty) -> bool {
    is_scalar_int_or_ptr(ty) || is_scalar_fp(ty)
}

#[inline]
const fn is_vec128(ty: Ty) -> bool {
    matches!(ty.form(), Form::Vector) && ty.bits(64) == 128
}

/// True for 128-bit values lowered in a single NEON `Q128` (`Vec128`) register:
/// 128-bit fixed SIMD vectors and `Ty::I128` (16-byte load/store/copy/bitcast/select).
#[inline]
const fn is_q128_ty(ty: Ty) -> bool {
    is_vec128(ty) || matches!(ty, Ty::I128)
}

#[inline]
const fn is_supported_value_ty(ty: Ty) -> bool {
    is_supported_scalar(ty) || is_q128_ty(ty)
}

/// Classifies a supported 128-bit `Op::Splat` target vector type.
#[derive(Clone, Copy)]
enum SplatKind {
    /// Integer or pointer lane (`I8`/`I16`/`I32`/`I64`/`PTR`) -> `VecAluOp::DupGen`.
    Int { elem_size: u8 },
    /// Scalar float lane (`F32`/`F64`) -> `VecAluOp::DupElem0`.
    Float { elem_size: u8 },
    /// Single 128-bit lane (`<1 x i128>`) -> direct `Vec128` alias/copy.
    Identity128,
}

#[inline]
const fn splat_kind_for_ty(ty: Ty) -> Option<SplatKind> {
    if !is_vec128(ty) {
        return None;
    }
    match ty.lane() {
        Ty::I8 => Some(SplatKind::Int { elem_size: 0 }),
        Ty::I16 => Some(SplatKind::Int { elem_size: 1 }),
        Ty::I32 => Some(SplatKind::Int { elem_size: 2 }),
        Ty::I64 | Ty::PTR => Some(SplatKind::Int { elem_size: 3 }),
        Ty::F32 => Some(SplatKind::Float { elem_size: 2 }),
        Ty::F64 => Some(SplatKind::Float { elem_size: 3 }),
        Ty::I128 => Some(SplatKind::Identity128),
        _ => None,
    }
}

/// Maps a 128-bit vector LIR ALU op and vector result type to its typed NEON `(VecAluOp, elem_size)`
/// encoding (`elem_size`: `0=B8, 1=H16, 2=S32/F32, 3=D64/F64`), or `None` if unsupported.
#[inline]
fn vec128_alu_op(op: Op, res_ty: Ty) -> Option<(VecAluOp, u8)> {
    match splat_kind_for_ty(res_ty)? {
        SplatKind::Int { elem_size } => match op {
            Op::Add => Some((VecAluOp::Add, elem_size)),
            Op::Sub => Some((VecAluOp::Sub, elem_size)),
            Op::UaddSat => Some((VecAluOp::Uqadd, elem_size)),
            Op::UsubSat => Some((VecAluOp::Uqsub, elem_size)),
            Op::SaddSat => Some((VecAluOp::Sqadd, elem_size)),
            Op::SsubSat => Some((VecAluOp::Sqsub, elem_size)),
            Op::Mul if elem_size <= 2 => Some((VecAluOp::Mul, elem_size)),
            Op::Smin if elem_size <= 2 => Some((VecAluOp::Smin, elem_size)),
            Op::Smax if elem_size <= 2 => Some((VecAluOp::Smax, elem_size)),
            Op::Umin if elem_size <= 2 => Some((VecAluOp::Umin, elem_size)),
            Op::Umax if elem_size <= 2 => Some((VecAluOp::Umax, elem_size)),
            Op::And => Some((VecAluOp::And, 0)),
            Op::Or => Some((VecAluOp::Orr, 0)),
            Op::Xor => Some((VecAluOp::Eor, 0)),
            Op::Not => Some((VecAluOp::Not, 0)),
            _ => None,
        },
        SplatKind::Float { elem_size } => match op {
            Op::Fadd => Some((VecAluOp::Fadd, elem_size)),
            Op::Fsub => Some((VecAluOp::Fsub, elem_size)),
            Op::Fmul => Some((VecAluOp::Fmul, elem_size)),
            _ => None,
        },
        SplatKind::Identity128 => None,
    }
}

/// True when two SSA `VRegClass`es can safely share one zero-instruction aliased `VReg`
/// (`Gpr32`/`Gpr64` share the GPR bank and 64-bit spill slots; FP/Vec classes match strictly).
#[inline]
const fn vreg_classes_alias_compat(a: VRegClass, b: VRegClass) -> bool {
    (a.is_gpr() && b.is_gpr())
        || matches!(
            (a, b),
            (VRegClass::Fp32, VRegClass::Fp32)
                | (VRegClass::Fp64, VRegClass::Fp64)
                | (VRegClass::Vec128, VRegClass::Vec128)
        )
}

#[inline]
const fn int_bits(ty: Ty) -> u32 {
    match ty {
        Ty::I1 => 1,
        Ty::I8 => 8,
        Ty::I16 => 16,
        Ty::I32 => 32,
        Ty::I64 | Ty::PTR => 64,
        _ => 0,
    }
}

#[inline]
const fn vreg_class_for_ty(ty: Ty) -> Option<VRegClass> {
    if matches!(ty, Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32) {
        Some(VRegClass::Gpr32)
    } else if matches!(ty, Ty::I64 | Ty::PTR) {
        Some(VRegClass::Gpr64)
    } else if matches!(ty, Ty::F32) {
        Some(VRegClass::Fp32)
    } else if matches!(ty, Ty::F64) {
        Some(VRegClass::Fp64)
    } else if is_q128_ty(ty) {
        Some(VRegClass::Vec128)
    } else {
        None
    }
}

#[inline]
const fn mem_size_for_ty(ty: Ty) -> Option<MemSize> {
    if matches!(ty, Ty::I1 | Ty::I8) {
        Some(MemSize::B8)
    } else if matches!(ty, Ty::I16) {
        Some(MemSize::H16)
    } else if matches!(ty, Ty::I32) {
        Some(MemSize::W32)
    } else if matches!(ty, Ty::I64 | Ty::PTR) {
        Some(MemSize::X64)
    } else if matches!(ty, Ty::F32) {
        Some(MemSize::S32)
    } else if matches!(ty, Ty::F64) {
        Some(MemSize::D64)
    } else if is_q128_ty(ty) {
        Some(MemSize::Q128)
    } else {
        None
    }
}

/// AAPCS64 stacked-argument cursor (`Os::MacOs` uses natural size/align; `Os::Linux` uses 8B slots).
#[derive(Clone, Copy)]
struct StackArgCursor {
    os: Os,
    next: u32,
}

impl StackArgCursor {
    #[inline]
    const fn new(os: Os) -> Self {
        Self { os, next: 0 }
    }

    fn place(&mut self, ty: Ty) -> Option<(u32, MemSize)> {
        let sz = mem_size_for_ty(ty)?;
        if sz == MemSize::Q128 {
            return None;
        }
        let natural = sz.byte_size();
        let slot_bytes = match self.os {
            Os::MacOs => natural,
            Os::Linux => natural.max(8),
        };
        let off = align_up_u32(self.next, slot_bytes)?;
        self.next = off.checked_add(slot_bytes)?;
        let access_sz = match self.os {
            Os::MacOs => sz,
            Os::Linux => {
                if sz.is_fp_or_vec() {
                    sz
                } else {
                    MemSize::X64
                }
            }
        };
        Some((off, access_sz))
    }

    #[inline]
    const fn aligned16(&self) -> Option<u32> {
        align_up_u32(self.next, 16)
    }
}

/// Computes AAPCS64 C-ABI (`X0..X7`, `V0..V7`) or `InternalFast` (`X0..X7, X9..X15`,
/// `V0..V7, V16..V23`, keeping `X8` dedicated to `ParamKind::Sret`) argument locations.
#[derive(Clone, Copy)]
enum AbiLoc {
    Gpr(u8),
    Fp(u8),
    Stack { off: u32, size: MemSize },
}

#[inline]
fn is_internal_fast_call(calls: &[u32], inst: usize) -> bool {
    !calls.is_empty() && u32_of(inst).is_some_and(|id| calls.binary_search(&id).is_ok())
}

#[derive(Clone, Copy)]
struct AbiWalk {
    internal_fast: bool,
    next_gpr: u8,
    next_fp: u8,
    stack: StackArgCursor,
}

impl AbiWalk {
    #[inline]
    const fn new(os: Os, internal_fast: bool) -> Self {
        Self {
            internal_fast,
            next_gpr: 0,
            next_fp: 0,
            stack: StackArgCursor::new(os),
        }
    }

    fn next_param(&mut self, p: &blaze_lir::func::ParamRec) -> Option<AbiLoc> {
        if ParamKind::from_byte(p.kind) == Some(ParamKind::Sret) {
            return if Ty(p.ty) == Ty::PTR {
                Some(AbiLoc::Gpr(8))
            } else {
                None
            };
        }
        self.next_arg(Ty(p.ty))
    }

    fn next_arg(&mut self, ty: Ty) -> Option<AbiLoc> {
        if is_scalar_int_or_ptr(ty) {
            let cap = if self.internal_fast {
                super::abi::INTERNAL_FAST_GPR_ARGS.len()
            } else {
                8
            };
            let idx = usize::from(self.next_gpr);
            if idx < cap
                && let Some(&r) = super::abi::INTERNAL_FAST_GPR_ARGS.get(idx)
            {
                self.next_gpr += 1;
                Some(AbiLoc::Gpr(r))
            } else {
                let (off, size) = self.stack.place(ty)?;
                Some(AbiLoc::Stack { off, size })
            }
        } else if is_scalar_fp(ty) {
            let cap = if self.internal_fast {
                super::abi::INTERNAL_FAST_FP_ARGS.len()
            } else {
                8
            };
            let idx = usize::from(self.next_fp);
            if idx < cap
                && let Some(&v) = super::abi::INTERNAL_FAST_FP_ARGS.get(idx)
            {
                self.next_fp += 1;
                Some(AbiLoc::Fp(v))
            } else {
                let (off, size) = self.stack.place(ty)?;
                Some(AbiLoc::Stack { off, size })
            }
        } else {
            None
        }
    }
}

// -----------------------------------------------------------------------------
// Stage 1: Strict Subset Checker (`check_misel_subset`)
// -----------------------------------------------------------------------------

fn refuse(inst: Option<usize>, what: &'static str) -> TierError {
    TierError::Refused {
        tier: TIER,
        inst: inst.and_then(u32_of),
        what,
    }
}

fn check_sig_c_abi(
    view: &View<'_>,
    sig_rec: SigRec,
    params: &[blaze_lir::func::ParamRec],
    rets: &[blaze_lir::func::ParamRec],
    os: Os,
    max_rets: usize,
    internal_fast: bool,
    inst: Option<usize>,
) -> Result<(), TierError> {
    if Conv::from_byte(sig_rec.conv) != Some(Conv::C) {
        return Err(refuse(inst, "calling convention other than Conv::C"));
    }
    if (sig_rec.bits & SigRec::VARIADIC) != 0 {
        return Err(refuse(inst, "variadic signature"));
    }
    let mut walk = AbiWalk::new(os, internal_fast);
    let mut has_sret = false;
    let mut byval_bytes: u32 = 0;
    for p in params {
        let pkind = ParamKind::from_byte(p.kind);
        let ty = Ty(p.ty);
        if pkind == Some(ParamKind::Sret) {
            if has_sret || ty != Ty::PTR {
                return Err(refuse(inst, "invalid sret parameter"));
            }
            has_sret = true;
        } else if pkind == Some(ParamKind::Byval) {
            if internal_fast {
                return Err(refuse(inst, "InternalFast with byval parameter"));
            }
            if ty != Ty::PTR {
                return Err(refuse(inst, "non-ptr byval parameter"));
            }
            let Some(facts) = view.facts(p) else {
                return Err(refuse(inst, "byval without size/align"));
            };
            if facts.align > 4 {
                return Err(refuse(inst, "byval align > 16"));
            }
            let align = 1u32 << u32::from(facts.align);
            let at = align_up_u32(byval_bytes, align).ok_or(TierError::TooLarge {
                tier: TIER,
                what: "byval argument area",
            })?;
            byval_bytes = at.checked_add(facts.size).ok_or(TierError::TooLarge {
                tier: TIER,
                what: "byval argument area",
            })?;
        } else if pkind != Some(ParamKind::Value) {
            return Err(refuse(inst, "unsupported parameter kind"));
        } else if !is_supported_scalar(ty) {
            return Err(refuse(inst, "unsupported parameter type"));
        }
        if walk.next_param(p).is_none() {
            return Err(refuse(inst, "stack parameter overflow"));
        }
    }
    if walk.stack.aligned16().is_none_or(|b| b >= FRAME_LIMIT / 2)
        || align_up_u32(byval_bytes, 16).is_none_or(|b| b >= FRAME_LIMIT / 2)
    {
        return Err(TierError::TooLarge {
            tier: TIER,
            what: "stacked call argument area",
        });
    }
    if rets.len() > max_rets {
        return Err(refuse(inst, "more than supported direct return values"));
    }
    let mut gpr_rets: u8 = 0;
    let mut fp_rets: u8 = 0;
    for r in rets {
        if ParamKind::from_byte(r.kind) != Some(ParamKind::Value) {
            return Err(refuse(inst, "non-value return kind"));
        }
        let ty = Ty(r.ty);
        if is_scalar_int_or_ptr(ty) {
            if gpr_rets >= 8 {
                return Err(refuse(inst, "more than 8 GPR return values"));
            }
            gpr_rets += 1;
        } else if is_scalar_fp(ty) {
            if fp_rets >= 8 {
                return Err(refuse(inst, "more than 8 FP return values"));
            }
            fp_rets += 1;
        } else {
            return Err(refuse(inst, "unsupported return type"));
        }
    }
    Ok(())
}

/// Validates that every op, type, signature, slot, and external reference in `view` is within
/// `misel`'s supported subset. Returns a typed [`TierError`] for anything unsupported.
pub(crate) fn check_misel_subset(
    view: &View<'_>,
    names: &dyn Names,
    opts: MIseOpts<'_>,
) -> Result<(), TierError> {
    let machine = opts.machine;
    if machine.arch != Arch::Arm64 {
        return Err(TierError::Machine { tier: TIER, machine });
    }
    if view.head.ptr_bits != 64 {
        return Err(refuse(None, "non-64-bit pointer width"));
    }
    if view.blocks() == 0 {
        return Err(refuse(None, "empty function"));
    }
    let Some(&fn_sig_rec) = view.sigs.get(to_idx(view.head.sig)) else {
        return Err(refuse(None, "missing function signature"));
    };
    let fn_sig = view.sig(SigId(view.head.sig));
    check_sig_c_abi(
        view,
        fn_sig_rec,
        fn_sig.params,
        fn_sig.rets,
        machine.os,
        16,
        opts.internal_fast,
        None,
    )?;

    let mut has_landing_blocks = false;
    for b in 0..view.blocks() {
        let Some(b_u32) = u32_of(b) else {
            return Err(TierError::TooLarge { tier: TIER, what: "block count" });
        };
        let blk = Block(b_u32);
        match view.block_kind(blk) {
            BlockKind::Normal | BlockKind::Cold => {}
            BlockKind::Cleanup | BlockKind::Catch | BlockKind::Terminate => {
                has_landing_blocks = true;
                let params = view.params(blk);
                if params.len() != 1 || ty_at(view, params.start) != Ty::PTR {
                    return Err(refuse(None, "unwind landing block without single ptr param"));
                }
            }
        }
    }
    if opts.internal_fast && has_landing_blocks {
        return Err(refuse(None, "InternalFast on function with unwind landing blocks"));
    }

    for slot in view.slots {
        if slot.align > 4 {
            return Err(refuse(None, "stack slot alignment > 16 bytes"));
        }
    }

    for inst in 0..view.len() {
        let op = view.op(inst);
        let res_ty = ty_at(view, inst);
        let mut o = view.operands(inst);

        match op {
            Op::Param => {
                if !is_supported_value_ty(res_ty) {
                    return Err(refuse(Some(inst), "unsupported param type"));
                }
            }
            Op::Iconst => {
                if (!is_scalar_int_or_ptr(res_ty) || res_ty == Ty::PTR) && res_ty != Ty::I128 {
                    return Err(refuse(Some(inst), "unsupported iconst type"));
                }
            }
            Op::IconstWide => {
                if res_ty == Ty::I128 {
                    if o.constant().len() != 4 {
                        return Err(refuse(Some(inst), "non-4-word i128 iconst_wide"));
                    }
                } else if res_ty != Ty::I64 {
                    return Err(refuse(Some(inst), "unsupported iconst_wide type"));
                }
            }
            Op::Fconst => {
                if res_ty != Ty::F32 {
                    return Err(refuse(Some(inst), "unsupported fconst type (e.g. f16)"));
                }
            }
            Op::FconstWide => {
                if res_ty != Ty::F64 {
                    return Err(refuse(Some(inst), "unsupported fconst_wide type (e.g. f128)"));
                }
            }
            Op::Null => {
                if res_ty != Ty::PTR {
                    return Err(refuse(Some(inst), "non-ptr null"));
                }
            }
            Op::Uninit | Op::Freeze => {
                if !is_supported_value_ty(res_ty) {
                    return Err(refuse(Some(inst), "unsupported uninit/freeze type"));
                }
            }
            Op::Assume | Op::SlotLive | Op::SlotDead | Op::Jump | Op::Unreachable => {}
            Op::Trap => {
                let code = o.imm();
                if code >= BRK_DIV_ZERO {
                    return Err(refuse(Some(inst), "trap code in reserved range"));
                }
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr => {
                if (!is_scalar_int_or_ptr(res_ty) || res_ty == Ty::PTR)
                    && res_ty != Ty::I128
                    && vec128_alu_op(op, res_ty).is_none()
                {
                    return Err(refuse(Some(inst), "integer op on unsupported type"));
                }
            }
            Op::Umulhi
            | Op::Smulhi
            | Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
            | Op::Rotl
            | Op::Rotr
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::UaddSat
            | Op::UsubSat
            | Op::SaddSat
            | Op::SsubSat => {
                if (!is_scalar_int_or_ptr(res_ty) || res_ty == Ty::PTR)
                    && vec128_alu_op(op, res_ty).is_none()
                {
                    return Err(refuse(Some(inst), "integer op on unsupported type"));
                }
            }
            Op::UaddOverflow
            | Op::UsubOverflow
            | Op::SaddOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow => {
                let a = o.value();
                let b = o.value();
                let op_ty = view.ty(a);
                if !is_scalar_int_or_ptr(op_ty)
                    || op_ty == Ty::PTR
                    || view.ty(b) != op_ty
                    || view.members(res_ty) != [op_ty.0, Ty::I1.0]
                {
                    return Err(refuse(Some(inst), "overflow op on unsupported type"));
                }
            }
            Op::Project => {
                let src = o.value();
                let field = o.imm();
                let src_op = view.op(src.index());
                let is_ovf = matches!(
                    src_op,
                    Op::UaddOverflow
                        | Op::UsubOverflow
                        | Op::SaddOverflow
                        | Op::SsubOverflow
                        | Op::UmulOverflow
                        | Op::SmulOverflow
                );
                let is_call = matches!(
                    src_op,
                    Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect
                );
                let field_ok = if is_ovf {
                    field <= 1
                } else if is_call {
                    to_idx(field) < view.members(view.ty(src)).len()
                } else {
                    false
                };
                if !field_ok || !is_supported_scalar(res_ty) {
                    return Err(refuse(Some(inst), "project of unsupported source or field"));
                }
            }
            Op::DataAddr => {
                if res_ty != Ty::PTR {
                    return Err(refuse(Some(inst), "non-ptr data_addr"));
                }
                let ref_id = o.reference();
                let Some(ref_rec) = view.refs.get(ref_id.index()).copied() else {
                    return Err(refuse(Some(inst), "data_addr ref out of bounds"));
                };
                match RefKind::from_word(ref_rec.kind) {
                    Some(RefKind::Static) => {
                        let outside = Outside::Static(ref_rec.place);
                        if names.name(outside).is_none() {
                            return Err(TierError::NoName { tier: TIER, of: outside });
                        }
                    }
                    Some(RefKind::Vtable) => {
                        let outside = Outside::Vtable(ref_rec.place);
                        match names.name(outside) {
                            Some(Named::Symbol(_)) => {}
                            Some(Named::C(_) | Named::WeakC(_)) => {
                                return Err(refuse(Some(inst), "C name for vtable"));
                            }
                            None => return Err(TierError::NoName { tier: TIER, of: outside }),
                        }
                    }
                    Some(RefKind::Data) => {
                        let row = ref_rec.place;
                        let Some(&data) = view.datas.get(to_idx(row)) else {
                            return Err(refuse(Some(inst), "missing data row"));
                        };
                        if data.align > u32::from(MOST_PART_ALIGN) {
                            return Err(refuse(Some(inst), "data align > 4 KiB"));
                        }
                        for p in view.data_positions_of(DataId(row)) {
                            if blaze_lir::PositionField::from_word(p.field).is_none() {
                                return Err(refuse(Some(inst), "unknown data position field"));
                            }
                            let end = u64::from(p.offset).saturating_add(4);
                            if end > u64::from(data.bytes_len) {
                                return Err(refuse(Some(inst), "data position past row end"));
                            }
                        }
                        for r in view.data_relocs_of(DataId(row)) {
                            let Some(target) = view.refs.get(RefId(r.target).index()).copied() else {
                                return Err(refuse(Some(inst), "data reloc target out of bounds"));
                            };
                            match RefKind::from_word(target.kind) {
                                Some(RefKind::Static) => {
                                    let outside = Outside::Static(target.place);
                                    if names.name(outside).is_none() {
                                        return Err(TierError::NoName { tier: TIER, of: outside });
                                    }
                                }
                                Some(RefKind::Vtable) => {
                                    let outside = Outside::Vtable(target.place);
                                    match names.name(outside) {
                                        Some(Named::Symbol(_)) => {}
                                        Some(Named::C(_) | Named::WeakC(_)) => {
                                            return Err(refuse(Some(inst), "C name for vtable in data"));
                                        }
                                        None => {
                                            return Err(TierError::NoName { tier: TIER, of: outside });
                                        }
                                    }
                                }
                                Some(RefKind::Func) => {
                                    let outside = Outside::Func(target.place);
                                    if names.name(outside).is_none() {
                                        return Err(TierError::NoName { tier: TIER, of: outside });
                                    }
                                }
                                Some(RefKind::Data) => {}
                                Some(RefKind::Tls) | None => {
                                    return Err(refuse(Some(inst), "tls reloc in data"));
                                }
                            }
                            if r.offset % 8 != 0
                                || u64::from(r.offset).saturating_add(8) > u64::from(data.bytes_len)
                            {
                                return Err(refuse(Some(inst), "invalid data reloc offset"));
                            }
                        }
                    }
                    _ => return Err(refuse(Some(inst), "unsupported data_addr kind")),
                }
            }
            Op::FuncAddr => {
                if res_ty != Ty::PTR {
                    return Err(refuse(Some(inst), "non-ptr func_addr"));
                }
                let ref_id = o.reference();
                let Some(ref_rec) = view.refs.get(ref_id.index()).copied() else {
                    return Err(refuse(Some(inst), "func_addr ref out of bounds"));
                };
                if RefKind::from_word(ref_rec.kind) != Some(RefKind::Func) {
                    return Err(refuse(Some(inst), "non-func func_addr"));
                }
                let outside = Outside::Func(ref_rec.place);
                if names.name(outside).is_none() {
                    return Err(TierError::NoName { tier: TIER, of: outside });
                }
            }
            Op::TargetIntrinsic => {
                let intr_id = o.intrinsic();
                let args = o.values();
                let name = view.intrinsic_name(intr_id);
                let ok = name == b"blaze.prefetch.r1"
                    && args.len() == 1
                    && res_ty == Ty::NONE
                    && args.iter().next().is_some_and(|ptr| view.ty(ptr) == Ty::PTR);
                if !ok {
                    return Err(refuse(Some(inst), "unsupported target_intrinsic"));
                }
            }
            Op::Switch => {
                let scrut = o.value();
                let scrut_ty = view.ty(scrut);
                if (!is_scalar_int_or_ptr(scrut_ty) || scrut_ty == Ty::PTR) && scrut_ty != Ty::I128 {
                    return Err(refuse(Some(inst), "switch on unsupported scrutinee type"));
                }
            }
            Op::Icmp => {
                let _cond = o.int_cond();
                let a = o.value();
                let a_ty = view.ty(a);
                if res_ty != Ty::I1 || (!is_scalar_int_or_ptr(a_ty) && a_ty != Ty::I128) {
                    return Err(refuse(Some(inst), "icmp on unsupported type"));
                }
            }
            Op::Fcmp => {
                let _cond = o.float_cond();
                let a = o.value();
                if res_ty != Ty::I1 || !is_scalar_fp(view.ty(a)) {
                    return Err(refuse(Some(inst), "fcmp on unsupported type"));
                }
            }
            Op::Select => {
                let cond = o.value();
                if view.ty(cond) != Ty::I1 || !is_supported_value_ty(res_ty) {
                    return Err(refuse(Some(inst), "select on unsupported type"));
                }
            }
            Op::Splat => {
                let a = o.value();
                if splat_kind_for_ty(res_ty).is_none() || view.ty(a) != res_ty.lane() {
                    return Err(refuse(Some(inst), "splat on unsupported vector type"));
                }
            }
            Op::Zext | Op::Sext | Op::Trunc => {
                let a = o.value();
                let src_ty = view.ty(a);
                let src_ok = (is_scalar_int_or_ptr(src_ty) && src_ty != Ty::PTR) || src_ty == Ty::I128;
                let res_ok = (is_scalar_int_or_ptr(res_ty) && res_ty != Ty::PTR) || res_ty == Ty::I128;
                if !src_ok || !res_ok {
                    return Err(refuse(Some(inst), "int cast on unsupported type"));
                }
            }
            Op::Bitcast => {
                let a = o.value();
                let src_ty = view.ty(a);
                if !is_supported_value_ty(src_ty) || !is_supported_value_ty(res_ty) {
                    return Err(refuse(Some(inst), "bitcast on unsupported type"));
                }
                if src_ty.bits(64) != res_ty.bits(64) {
                    return Err(refuse(Some(inst), "bitcast width mismatch"));
                }
            }
            Op::PtrAddr | Op::Expose => {
                let a = o.value();
                if view.ty(a) != Ty::PTR || res_ty != Ty::I64 {
                    return Err(refuse(Some(inst), "ptr_addr/expose on unsupported type"));
                }
            }
            Op::FromExposed => {
                let a = o.value();
                if view.ty(a) != Ty::I64 || res_ty != Ty::PTR {
                    return Err(refuse(Some(inst), "from_exposed on unsupported type"));
                }
            }
            Op::PtrWithAddr => {
                let p = o.value();
                let a = o.value();
                if view.ty(p) != Ty::PTR || view.ty(a) != Ty::I64 || res_ty != Ty::PTR {
                    return Err(refuse(Some(inst), "ptr_with_addr on unsupported type"));
                }
            }
            Op::PtrAdd => {
                let p = o.value();
                let off = o.value();
                if view.ty(p) != Ty::PTR || !is_scalar_int_or_ptr(view.ty(off)) || view.ty(off) == Ty::PTR {
                    return Err(refuse(Some(inst), "ptr_add on unsupported type"));
                }
            }
            Op::SlotAddr => {
                let slot = o.slot();
                if slot.index() >= view.slots.len() {
                    return Err(refuse(Some(inst), "slot_addr out of bounds"));
                }
            }
            Op::Load => {
                let ptr = o.value();
                if view.ty(ptr) != Ty::PTR || !is_supported_value_ty(res_ty) {
                    return Err(refuse(Some(inst), "load of unsupported type"));
                }
            }
            Op::Store => {
                let val = o.value();
                let ptr = o.value();
                if view.ty(ptr) != Ty::PTR || !is_supported_value_ty(view.ty(val)) {
                    return Err(refuse(Some(inst), "store of unsupported type"));
                }
            }
            Op::Memcpy | Op::Memmove | Op::Memset => {
                let dst = o.value();
                let src_or_byte = o.value();
                let len = o.value();
                if view.ty(dst) != Ty::PTR || !is_scalar_int_or_ptr(view.ty(len)) || view.ty(len) == Ty::PTR {
                    return Err(refuse(Some(inst), "bulk memop on unsupported type"));
                }
                if op == Op::Memset {
                    if view.ty(src_or_byte) != Ty::I8 {
                        return Err(refuse(Some(inst), "memset byte is not i8"));
                    }
                } else if view.ty(src_or_byte) != Ty::PTR {
                    return Err(refuse(Some(inst), "memcpy/memmove src is not ptr"));
                }
            }
            Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv
            | Op::Fneg
            | Op::Fabs
            | Op::Fma
            | Op::Fmuladd => {
                if !is_scalar_fp(res_ty) && vec128_alu_op(op, res_ty).is_none() {
                    return Err(refuse(Some(inst), "float op on unsupported type"));
                }
            }
            Op::Fpext => {
                let a = o.value();
                if view.ty(a) != Ty::F32 || res_ty != Ty::F64 {
                    return Err(refuse(Some(inst), "fpext not f32 -> f64"));
                }
            }
            Op::Fptrunc => {
                let a = o.value();
                if view.ty(a) != Ty::F64 || res_ty != Ty::F32 {
                    return Err(refuse(Some(inst), "fptrunc not f64 -> f32"));
                }
            }
            Op::UitoFp | Op::SitoFp => {
                let a = o.value();
                let src_ty = view.ty(a);
                if !is_scalar_int_or_ptr(src_ty) || src_ty == Ty::PTR || !is_scalar_fp(res_ty) {
                    return Err(refuse(Some(inst), "int-to-float on unsupported type"));
                }
            }
            Op::FptoUi | Op::FptoSi => {
                let a = o.value();
                if !is_scalar_fp(view.ty(a))
                    || !matches!(res_ty, Ty::I8 | Ty::I16 | Ty::I32 | Ty::I64)
                {
                    return Err(refuse(Some(inst), "float-to-int on unsupported type"));
                }
            }
            Op::Br => {
                let cond = o.value();
                if view.ty(cond) != Ty::I1 {
                    return Err(refuse(Some(inst), "br condition is not i1"));
                }
            }
            Op::Return => {
                let vals = o.values();
                if vals.len() != fn_sig.rets.len() {
                    return Err(refuse(Some(inst), "return arity mismatch"));
                }
            }
            Op::Call => {
                let callee_ref = o.reference();
                let sig_id = o.sig();
                let args = o.values();
                let Some(ref_rec) = view.refs.get(callee_ref.index()).copied() else {
                    return Err(refuse(Some(inst), "call ref out of bounds"));
                };
                if RefKind::from_word(ref_rec.kind) != Some(RefKind::Func) {
                    return Err(refuse(Some(inst), "call target is not RefKind::Func"));
                }
                let outside = Outside::Func(ref_rec.place);
                let call_fast = is_internal_fast_call(opts.internal_fast_calls, inst);
                match names.name(outside) {
                    None => return Err(TierError::NoName { tier: TIER, of: outside }),
                    Some(Named::C(_) | Named::WeakC(_)) if call_fast => {
                        return Err(refuse(Some(inst), "InternalFast on extern C call"));
                    }
                    Some(_) => {}
                }
                let Some(&sig_rec) = view.sigs.get(sig_id.index()) else {
                    return Err(refuse(Some(inst), "call signature out of bounds"));
                };
                let sig = view.sig(sig_id);
                check_sig_c_abi(
                    view,
                    sig_rec,
                    sig.params,
                    sig.rets,
                    machine.os,
                    16,
                    call_fast,
                    Some(inst),
                )?;
                if args.len() != sig.params.len() {
                    return Err(refuse(Some(inst), "call argument count mismatch"));
                }
            }
            Op::CallIndirect => {
                let callee = o.value();
                let sig_id = o.sig();
                let args = o.values();
                if view.ty(callee) != Ty::PTR {
                    return Err(refuse(Some(inst), "call_indirect target is not ptr"));
                }
                let Some(&sig_rec) = view.sigs.get(sig_id.index()) else {
                    return Err(refuse(Some(inst), "call_indirect signature out of bounds"));
                };
                let sig = view.sig(sig_id);
                check_sig_c_abi(
                    view,
                    sig_rec,
                    sig.params,
                    sig.rets,
                    machine.os,
                    16,
                    false,
                    Some(inst),
                )?;
                if args.len() != sig.params.len() {
                    return Err(refuse(Some(inst), "call_indirect argument count mismatch"));
                }
            }
            Op::Invoke => {
                let callee_ref = o.reference();
                let sig_id = o.sig();
                let _normal_blk = o.block();
                let unwind_blk = o.block();
                let args = o.values();
                let Some(ref_rec) = view.refs.get(callee_ref.index()).copied() else {
                    return Err(refuse(Some(inst), "invoke ref out of bounds"));
                };
                if RefKind::from_word(ref_rec.kind) != Some(RefKind::Func) {
                    return Err(refuse(Some(inst), "invoke target is not RefKind::Func"));
                }
                let outside = Outside::Func(ref_rec.place);
                let call_fast = is_internal_fast_call(opts.internal_fast_calls, inst);
                match names.name(outside) {
                    None => return Err(TierError::NoName { tier: TIER, of: outside }),
                    Some(Named::C(_) | Named::WeakC(_)) if call_fast => {
                        return Err(refuse(Some(inst), "InternalFast on extern C invoke"));
                    }
                    Some(_) => {}
                }
                if !view.block_kind(unwind_blk).is_landing() {
                    return Err(refuse(Some(inst), "invoke unwind target is not a landing block"));
                }
                let Some(&sig_rec) = view.sigs.get(sig_id.index()) else {
                    return Err(refuse(Some(inst), "invoke signature out of bounds"));
                };
                let sig = view.sig(sig_id);
                check_sig_c_abi(
                    view,
                    sig_rec,
                    sig.params,
                    sig.rets,
                    machine.os,
                    16,
                    call_fast,
                    Some(inst),
                )?;
                if args.len() != sig.params.len() {
                    return Err(refuse(Some(inst), "invoke argument count mismatch"));
                }
            }
            Op::InvokeIndirect => {
                let callee = o.value();
                let sig_id = o.sig();
                let _normal_blk = o.block();
                let unwind_blk = o.block();
                let args = o.values();
                if view.ty(callee) != Ty::PTR {
                    return Err(refuse(Some(inst), "invoke_indirect target is not ptr"));
                }
                if !view.block_kind(unwind_blk).is_landing() {
                    return Err(refuse(Some(inst), "invoke_indirect unwind target is not a landing block"));
                }
                let Some(&sig_rec) = view.sigs.get(sig_id.index()) else {
                    return Err(refuse(Some(inst), "invoke_indirect signature out of bounds"));
                };
                let sig = view.sig(sig_id);
                check_sig_c_abi(
                    view,
                    sig_rec,
                    sig.params,
                    sig.rets,
                    machine.os,
                    16,
                    false,
                    Some(inst),
                )?;
                if args.len() != sig.params.len() {
                    return Err(refuse(Some(inst), "invoke_indirect argument count mismatch"));
                }
            }
            Op::Resume => {
                let exc = o.value();
                if view.ty(exc) != Ty::PTR {
                    return Err(refuse(Some(inst), "resume operand is not ptr"));
                }
            }
            _ => {
                return Err(refuse(Some(inst), op.name()));
            }
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Stage 2: Target Table, Pre-Analysis & Pattern Recognition
// -----------------------------------------------------------------------------

/// Deduplicated external target & C-name table built during selection.
struct TargetTable<'a> {
    targets: Vec<Target, Alloc<'a>>,
    names: Vec<(Symbol, u32, u32), Alloc<'a>>,
    text: Vec<u8, Alloc<'a>>,
}

impl<'a> TargetTable<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        Self {
            targets: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            text: Vec::new_in(alloc),
        }
    }

    fn intern_target(&mut self, target: Target) -> Result<u32, TierError> {
        for (i, &existing) in self.targets.iter().enumerate() {
            if existing == target {
                return u32_of(i).ok_or(TierError::TooLarge { tier: TIER, what: "targets" });
            }
        }
        let idx = u32_of(self.targets.len())
            .ok_or(TierError::TooLarge { tier: TIER, what: "targets" })?;
        self.targets.push(target);
        Ok(idx)
    }

    fn intern_c_name(&mut self, c_name: &str) -> Result<u32, TierError> {
        self.intern_c_name_in_space(c_name, Space::Outside)
    }

    fn intern_c_name_in_space(&mut self, c_name: &str, space: Space) -> Result<u32, TierError> {
        let sym = Symbol::of_name(c_name);
        let mut have_name = false;
        for &(existing_sym, _, _) in self.names.iter() {
            if existing_sym == sym {
                have_name = true;
                break;
            }
        }
        if !have_name {
            let start = u32_of(self.text.len())
                .ok_or(TierError::TooLarge { tier: TIER, what: "outside name text" })?;
            let len = u32_of(c_name.len())
                .ok_or(TierError::TooLarge { tier: TIER, what: "outside name text" })?;
            for &b in c_name.as_bytes() {
                self.text.push(b);
            }
            self.names.push((sym, start, len));
        }
        self.intern_target(Target { space, symbol: sym })
    }

    fn intern_outside(
        &mut self,
        of: Outside,
        space: Space,
        allow_c_name: bool,
        names: &dyn Names,
        inst: usize,
    ) -> Result<u32, TierError> {
        let Some(named) = names.name(of) else {
            return Err(TierError::NoName { tier: TIER, of });
        };
        match named {
            Named::Symbol(sym) => self.intern_target(Target { space, symbol: sym }),
            Named::C(c_name) => {
                if !allow_c_name {
                    return Err(refuse(Some(inst), "C name for non-func/static"));
                }
                self.intern_c_name_in_space(c_name, Space::Outside)
            }
            Named::WeakC(c_name) => {
                if !allow_c_name {
                    return Err(refuse(Some(inst), "C name for non-func/static"));
                }
                self.intern_c_name_in_space(c_name, Space::WeakOutside)
            }
        }
    }

    fn intern_outside_func(&mut self, place: u32, names: &dyn Names) -> Result<u32, TierError> {
        let outside = Outside::Func(place);
        let Some(named) = names.name(outside) else {
            return Err(TierError::NoName { tier: TIER, of: outside });
        };
        match named {
            Named::Symbol(sym) => self.intern_target(Target { space: Space::Func, symbol: sym }),
            Named::C(c_name) => self.intern_c_name_in_space(c_name, Space::Outside),
            Named::WeakC(c_name) => self.intern_c_name_in_space(c_name, Space::WeakOutside),
        }
    }
}

/// Accumulated read-only data (`relro`), internal pointer words, and source-position slots.
struct RelroData<'a> {
    relro: Vec<u8, Alloc<'a>>,
    relro_align: u8,
    words: Vec<Word, Alloc<'a>>,
    position_slots: Vec<PositionRow, Alloc<'a>>,
}

impl<'a> RelroData<'a> {
    fn new(alloc: Alloc<'a>) -> Self {
        Self {
            relro: Vec::new_in(alloc),
            relro_align: 0,
            words: Vec::new_in(alloc),
            position_slots: Vec::new_in(alloc),
        }
    }
}

/// Matched single-block instruction-selection pattern for one LIR instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SelPattern {
    None,
    /// `Add(acc, Mul(a, b))` -> `MOp::Madd`.
    Madd { a: u32, b: u32, acc: u32 },
    /// `Sub(acc, Mul(a, b))` -> `MOp::Msub`.
    Msub { a: u32, b: u32, acc: u32 },
    /// `Add/Sub/And/Or/Xor(lhs, Shl/Lshr/Ashr(rhs, k))` -> shifted-register op.
    ShiftedBin { lhs: u32, rhs: u32, shift: MShift },
    /// `Add/Sub(lhs, Zext/Sext(rhs))` -> `AddExt`/`SubExt`.
    ExtAddSub { lhs: u32, rhs: u32, ext: MExt },
    /// `And/Or/Xor(lhs, Not(rhs))` -> `LogicalReg { invert_rm: true }`.
    LogicalNot { lhs: u32, rhs: u32 },
    /// `And(Lshr(x, s), (1<<w)-1)` -> single `Ubfm`.
    Ubfx { x: u32, immr: u8, imms: u8 },
    /// `Load`/`Store` with folded `SlotAddr` (`[SP, #off]`).
    MemSp { off: i32 },
    /// `Load`/`Store` with folded `PtrAdd(base, const_off)`.
    MemBaseOff { base: u32, off: i32 },
    /// `Load`/`Store` with folded `PtrAdd(base, Shl(idx, shift))` or `PtrAdd(base, idx)`.
    MemScaledIdx { base: u32, idx: u32, shift: u8 },
    /// `Icmp(Eq | Ne, And(x, mask_const), 0)` with folded single-use `And` -> `Tbz`/`Tbnz` or `TstImm`.
    IcmpTstImm { x: u32, mask_val: u32 },
}

/// Extracts `(offset, MemSize)` for an `Op::Load` or `Op::Store` instruction.
#[inline]
fn load_store_off_size(view: &View<'_>, inst: usize) -> Option<(i32, MemSize)> {
    let op = view.op(inst);
    let mut o = view.operands(inst);
    let (off, mem_ty) = if op == Op::Load {
        let _ptr = o.value();
        (o.offset(), ty_at(view, inst))
    } else if op == Op::Store {
        let val = o.value();
        let _ptr = o.value();
        (o.offset(), view.ty(val))
    } else {
        return None;
    };
    let size = mem_size_for_ty(mem_ty)?;
    Some((off, size))
}

/// Zero-`as` check whether `off` fits AArch64 `AddrMode::BaseOff` for `size`.
#[inline]
fn fits_base_off_checked(off: i64, size: MemSize) -> Option<i32> {
    let off32 = i32::try_from(off).ok()?;
    let bytes = i32_of(size.byte_size())?;
    let scaled_ok = off32 >= 0 && (off32 % bytes) == 0 && (off32 / bytes) <= 4095;
    let unscaled_ok = (-256..=255).contains(&off32);
    if scaled_ok || unscaled_ok { Some(off32) } else { None }
}

/// Checks if `val` fits AArch64 `AddImm` / `SubImm` (`imm12` optionally shifted left by 12).
#[inline]
fn encode_add_sub_imm12(val: u64) -> Option<(u16, bool)> {
    if val <= 0xfff {
        u16::try_from(val).ok().map(|v| (v, false))
    } else if (val & 0xfff) == 0 && (val >> 12) <= 0xfff {
        u16::try_from(val >> 12).ok().map(|v| (v, true))
    } else {
        None
    }
}

#[inline]
const fn swap_int_cond(c: IntCond) -> IntCond {
    match c {
        IntCond::Eq => IntCond::Eq,
        IntCond::Ne => IntCond::Ne,
        IntCond::Ult => IntCond::Ugt,
        IntCond::Ule => IntCond::Uge,
        IntCond::Ugt => IntCond::Ult,
        IntCond::Uge => IntCond::Ule,
        IntCond::Slt => IntCond::Sgt,
        IntCond::Sle => IntCond::Sge,
        IntCond::Sgt => IntCond::Slt,
        IntCond::Sge => IntCond::Sle,
    }
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

/// Single step of shift-amount mask/cast peeling for hardware-masked AArch64 shifts (`ShiftVar`).
/// Peels:
/// - `Op::And(a, b)` / `Op::And(b, a)` where one operand is a constant `m` with `(m & mask) == mask`
///   and the other is non-constant (on integer width `>= 8` bits).
/// - `Op::Trunc` / `Op::Zext` whose source and destination integer widths are `>= 8` bits.
fn peel_shift_mask_step(
    view: &View<'_>,
    const_u64: &[Option<u64>],
    idx: usize,
    mask: u64,
) -> Option<usize> {
    if idx >= view.len() {
        return None;
    }
    let dst_bits = int_bits(ty_at(view, idx));
    if dst_bits < 8 {
        return None;
    }
    match view.op(idx) {
        Op::And => {
            let mut o = view.operands(idx);
            let a = o.value();
            let b = o.value();
            let ai = a.index();
            let bi = b.index();
            let a_const = const_u64.get(ai).copied().flatten();
            let b_const = const_u64.get(bi).copied().flatten();
            if let Some(m) = b_const
                && a_const.is_none()
                && (m & mask) == mask
            {
                Some(ai)
            } else if let Some(m) = a_const
                && b_const.is_none()
                && (m & mask) == mask
            {
                Some(bi)
            } else {
                None
            }
        }
        Op::Trunc | Op::Zext => {
            let src = view.operands(idx).value();
            let src_bits = int_bits(view.ty(src));
            if src_bits >= 8 {
                Some(src.index())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Peels redundant shift-amount masks (`And(_, m)` with `(m & mask) == mask`) and width-preserving
/// `Trunc`/`Zext` (`src_bits >= 8`) in a bounded non-recursive loop (`<= 4` steps).
fn peel_shift_mask(
    view: &View<'_>,
    const_u64: &[Option<u64>],
    rhs: usize,
    mask: u64,
) -> usize {
    let mut cur = rhs;
    for _ in 0..4 {
        let Some(next) = peel_shift_mask_step(view, const_u64, cur, mask) else {
            break;
        };
        cur = next;
    }
    cur
}

/// Single step of same-block boolean condition peeling for `Op::Br` / `Op::Select`.
/// Returns `(inner_idx, inverted, optional_middle_zext_idx)`.
fn peel_bool_cond_step(
    view: &View<'_>,
    const_u64: &[Option<u64>],
    inst_block: &[u32],
    idx: usize,
    cur_b: u32,
) -> Option<(usize, bool, Option<usize>)> {
    if idx >= view.len() || inst_block.get(idx).copied() != Some(cur_b) {
        return None;
    }
    match view.op(idx) {
        Op::Freeze => {
            let x = view.operands(idx).value();
            if view.ty(x) == Ty::I1 && const_u64.get(x.index()).copied().flatten().is_none() {
                Some((x.index(), false, None))
            } else {
                None
            }
        }
        Op::Not => {
            let x = view.operands(idx).value();
            if view.ty(x) == Ty::I1 && const_u64.get(x.index()).copied().flatten().is_none() {
                Some((x.index(), true, None))
            } else {
                None
            }
        }
        Op::Xor => {
            if ty_at(view, idx) != Ty::I1 {
                return None;
            }
            let mut o = view.operands(idx);
            let a = o.value();
            let b = o.value();
            let ai = a.index();
            let bi = b.index();
            let ac = const_u64.get(ai).copied().flatten();
            let bc = const_u64.get(bi).copied().flatten();
            let (other, c) = if let Some(cv) = bc
                && ac.is_none()
            {
                (ai, cv & 1)
            } else if let Some(cv) = ac
                && bc.is_none()
            {
                (bi, cv & 1)
            } else {
                return None;
            };
            Some((other, c == 1, None))
        }
        Op::Icmp => {
            let mut o = view.operands(idx);
            let ic = o.int_cond();
            if !matches!(ic, IntCond::Eq | IntCond::Ne) {
                return None;
            }
            let mut a = o.value();
            let mut b = o.value();
            let ac = const_u64.get(a.index()).copied().flatten();
            let bc = const_u64.get(b.index()).copied().flatten();
            if ac.is_some() && bc.is_none() {
                core::mem::swap(&mut a, &mut b);
            }
            let ai = a.index();
            if const_u64.get(ai).copied().flatten().is_some() {
                return None;
            }
            let Some(c_raw) = const_u64.get(b.index()).copied().flatten() else {
                return None;
            };
            let cmp_bits = int_bits(view.ty(a));
            if cmp_bits == 0 {
                return None;
            }
            let c = c_raw & bit_mask(cmp_bits);
            if c != 0 && c != 1 {
                return None;
            }
            let inv = (ic == IntCond::Eq) ^ (c == 1);
            if view.ty(a) == Ty::I1 {
                return Some((ai, inv, None));
            }
            if view.op(ai) == Op::Zext && inst_block.get(ai).copied() == Some(cur_b) {
                let inner = view.operands(ai).value();
                if view.ty(inner) == Ty::I1
                    && const_u64.get(inner.index()).copied().flatten().is_none()
                {
                    return Some((inner.index(), inv, Some(ai)));
                }
            }
            None
        }
        _ => None,
    }
}

/// Peels up to 4 same-block boolean wrapper steps from `cond`, returning `(inner, inverted)`.
fn peel_bool_cond(
    view: &View<'_>,
    const_u64: &[Option<u64>],
    inst_block: &[u32],
    cond: Value,
    cur_b: u32,
) -> (Value, bool) {
    let mut cur = cond.index();
    let mut inv = false;
    for _ in 0..4 {
        let Some((next, step_inv, _mid)) =
            peel_bool_cond_step(view, const_u64, inst_block, cur, cur_b)
        else {
            break;
        };
        cur = next;
        inv ^= step_inv;
    }
    (Value(u32_of(cur).unwrap_or(cond.0)), inv)
}

#[inline]
const fn int_cond_to_mcond(c: IntCond) -> MCond {
    match c {
        IntCond::Eq => MCond::Eq,
        IntCond::Ne => MCond::Ne,
        IntCond::Ult => MCond::Lo,
        IntCond::Ule => MCond::Ls,
        IntCond::Ugt => MCond::Hi,
        IntCond::Uge => MCond::Hs,
        IntCond::Slt => MCond::Lt,
        IntCond::Sle => MCond::Le,
        IntCond::Sgt => MCond::Gt,
        IntCond::Sge => MCond::Ge,
    }
}

#[inline]
const fn float_cond_to_mcond(c: FloatCond) -> Option<MCond> {
    match c {
        FloatCond::Oeq => Some(MCond::Eq),
        FloatCond::Une => Some(MCond::Ne),
        FloatCond::Olt => Some(MCond::Mi),
        FloatCond::Ole => Some(MCond::Ls),
        FloatCond::Ogt => Some(MCond::Gt),
        FloatCond::Oge => Some(MCond::Ge),
        FloatCond::Ord => Some(MCond::Vc),
        FloatCond::Uno => Some(MCond::Vs),
        FloatCond::Ult => Some(MCond::Lt),
        FloatCond::Ule => Some(MCond::Le),
        FloatCond::Ugt => Some(MCond::Hi),
        FloatCond::Uge => Some(MCond::Pl),
        FloatCond::False | FloatCond::True | FloatCond::One | FloatCond::Ueq => None,
    }
}

fn lookup_lir_call_clobbers(table: &[(u32, CallClobbers)], lir_inst: u32) -> CallClobbers {
    if let Ok(pos) = table.binary_search_by_key(&lir_inst, |&(idx, _)| idx) {
        return table[pos].1;
    }
    CallClobbers::C_ABI_ARM64
}

/// Converts a LIR block's `BlockLayout` frequency/cold/depth info into a `mra` block weight.
#[inline]
fn mra_weight_for_lir_block(layout: &BlockLayout<'_>, lir_b: usize) -> u32 {
    if layout.is_cold.get(lir_b).copied().unwrap_or(false) {
        return 1;
    }
    let depth = layout.loop_depth.get(lir_b).copied().unwrap_or(0);
    let freq = layout.block_freq.get(lir_b).copied().unwrap_or(1024);
    if depth > 0 {
        let scaled = freq >> 9;
        let clamped = scaled.clamp(8, 1_000_000);
        u32::try_from(clamped).unwrap_or(1_000_000)
    } else if layout.is_terminal_exit.get(lir_b).copied().unwrap_or(false) {
        1
    } else {
        2
    }
}

// -----------------------------------------------------------------------------
// Stage 3: Pure SSA `VReg` + `VFlag` + `ParallelCopy` Selector
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct IncomingStackParam {
    caller_off: u32,
    size: MemSize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct LiveIntCmp {
    lhs: u32,
    rhs: u32,
    rhs_const: Option<u64>,
    bits: u32,
    signed_narrow: bool,
}

#[derive(Clone, Copy)]
struct LiveCmp {
    lir_cmp: u32,
    vflag: u32,
    cond: MCond,
    int_cmp: Option<LiveIntCmp>,
}

#[derive(Clone, Copy)]
struct DeferredEdgeSplit {
    split_blk: MBlock,
    target_blk: MBlock,
    copy_start: u32,
    copy_len: u16,
}

#[derive(Clone, Copy)]
enum CallCallee {
    Direct(RefId),
    Indirect(Value),
}

struct SelectOut<'a> {
    mfunc: MFunc<'a>,
    targets: TargetTable<'a>,
    relro: RelroData<'a>,
    trap_kinds: Vec<(TrapKind, Option<u32>), Alloc<'a>>,
    unwind_entries: Vec<Option<(MBlock, blaze_code::unwind::Action)>, Alloc<'a>>,
    block_weights: Vec<u32, Alloc<'a>>,
    call_clobbers_seq: Vec<CallClobbers, Alloc<'a>>,
    incoming_stack_params: Vec<IncomingStackParam, Alloc<'a>>,
    spill_base_off: u32,
    has_calls: bool,
    has_landing_blocks: bool,
    uses_fp: bool,
    ret_gpr_mask: u32,
    ret_fp_mask: u32,
}

struct Selector<'a, 'v> {
    view: &'v View<'v>,
    names: &'v dyn Names,
    opts: MIseOpts<'v>,
    alloc: Alloc<'a>,
    mfunc: MFunc<'a>,
    targets: TargetTable<'a>,
    relro: RelroData<'a>,
    data_relro_off: Vec<Option<u32>, Alloc<'a>>,
    trap_kinds: Vec<(TrapKind, Option<u32>), Alloc<'a>>,
    unwind_entries: Vec<Option<(MBlock, blaze_code::unwind::Action)>, Alloc<'a>>,
    unreach_ord: Vec<u32, Alloc<'a>>,
    block_weights: Vec<u32, Alloc<'a>>,
    call_clobbers_seq: Vec<CallClobbers, Alloc<'a>>,
    incoming_stack_params: Vec<IncomingStackParam, Alloc<'a>>,
    /// Per LIR block: entry `MBlock`.
    mblock_of_lir: Vec<MBlock, Alloc<'a>>,
    /// Per LIR block: whether reachable from `Cleanup | Terminate` via ordinary edges.
    is_cleanup_block: Vec<bool, Alloc<'a>>,
    /// Per LIR instruction: owning LIR block index.
    inst_block: Vec<u32, Alloc<'a>>,
    /// Per LIR instruction: constant integer/null value if statically known (<= 64-bit).
    const_u64: Vec<Option<u64>, Alloc<'a>>,
    /// Per LIR instruction: constant `Ty::I128` value `(lo, hi)` if statically known.
    const_i128: Vec<Option<(u64, u64)>, Alloc<'a>>,
    /// Per LIR instruction: constant float bits if `Fconst` / `FconstWide`.
    const_fp_bits: Vec<Option<u64>, Alloc<'a>>,
    /// Per LIR instruction: total uses across the whole function.
    use_count: Vec<u32, Alloc<'a>>,
    /// Per LIR instruction: whether producer emission is skipped (lazy const or folded).
    skip_inst: Vec<bool, Alloc<'a>>,
    /// Per LIR instruction: whether `Icmp`/`Fcmp` has ALL uses as `Br`/`Select` in its own block.
    cmp_all_fused: Vec<bool, Alloc<'a>>,
    /// Per LIR instruction: matched selection pattern.
    pattern: Vec<SelPattern, Alloc<'a>>,
    /// Per LIR instruction: assigned SSA `VReg` (`NONE_U32` if unallocated / void).
    val_vreg: Vec<u32, Alloc<'a>>,
    /// Per LIR `Ty::I128` scalar-pair instruction: `(lo_vreg, hi_vreg)` (`Gpr64` pair).
    i128_pair: Vec<(u32, u32), Alloc<'a>>,
    /// Order-independent static classification: true iff a `Ty::I128` instruction is represented
    /// as a `Vec128` (`Q128`) VReg (`Load`/`Bitcast`/`Select`/`Freeze` of Q128) rather than `i128_pair`.
    i128_is_q128: Vec<bool, Alloc<'a>>,
    /// Per LIR overflow instruction: `(val_vreg, flag_vreg)` for zero-cost `Op::Project`.
    ovf_pair: Vec<(u32, u32), Alloc<'a>>,
    /// Per LIR multi-return call/invoke instruction: base offset into `call_ret_vregs` (`NONE_U32` if unset).
    call_ret_off: Vec<u32, Alloc<'a>>,
    /// Flat SSA `VReg` ids for multi-return call/invoke results (indexed by `call_ret_off[inst] + field`).
    call_ret_vregs: Vec<u32, Alloc<'a>>,
    /// Per LIR stack slot: byte offset from `SP` (final before `mra`).
    slot_off: Vec<u32, Alloc<'a>>,
    /// Base `SP`-relative byte offset of the outgoing `ParamKind::Byval` copy area.
    byval_base_off: u32,
    /// Currently open `MBlock` during lowering.
    cur_blk: MBlock,
    /// Currently lowering LIR block index.
    cur_lir_blk: u32,
    /// Weight of the currently open LIR block (applied to any intra-block splits).
    cur_weight: u32,
    /// Next unique virtual flag ID.
    next_vflag: u32,
    /// Live condition flags in the current `MBlock` (for sharing across contiguous sibling `Select`s).
    live_cmp: Option<LiveCmp>,
    /// Deferred non-fallthrough `Br` edge-split blocks emitted at the end of `block_order`.
    deferred_splits: Vec<DeferredEdgeSplit, Alloc<'a>>,
    /// Scratch storage for deferred split copy pairs.
    deferred_pairs: Vec<(MReg, MReg), Alloc<'a>>,
    /// Lazily created cold trap block for div-by-zero.
    div_zero_trap_blk: Option<MBlock>,
    /// Lazily created cold trap block for signed div overflow (`MIN / -1`).
    div_ovf_trap_blk: Option<MBlock>,
    /// Whether any call (`Op::Call`/`CallIndirect`/`Invoke`/`InvokeIndirect`/`Resume` or runtime bulk) was emitted.
    has_calls: bool,
    /// Whether the function has any unwind landing block (`Cleanup | Catch | Terminate`).
    has_landing_blocks: bool,
}

impl<'a, 'v> Selector<'a, 'v> {
    #[inline]
    fn alloc_vflag(&mut self) -> u32 {
        let id = self.next_vflag;
        self.next_vflag = self.next_vflag.saturating_add(1);
        id
    }

    #[inline]
    fn push_op(&mut self, op: MOp, flags: MInstFlags) -> MInst {
        if op.writes_flags() {
            self.live_cmp = None;
        }
        self.mfunc.push_inst(op, flags)
    }

    #[inline]
    fn push_flag_writer(&mut self, op: MOp, flags: MInstFlags) -> (MInst, u32) {
        self.live_cmp = None;
        let vf = self.alloc_vflag();
        let inst = self.mfunc.push_inst(op, flags);
        self.mfunc.set_vflag_def(inst, vf);
        (inst, vf)
    }

    #[inline]
    fn new_mblock(&mut self, align16: bool, weight: u32) -> MBlock {
        let blk = self.mfunc.new_block(align16);
        self.block_weights.push(weight);
        blk
    }

    /// Emits a fresh `VReg` holding integer constant `imm` (`wide` = 64-bit) with `REMATERIALIZABLE`.
    fn materialize_int_const(&mut self, imm: u64, wide: bool) -> VReg {
        let cls = if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 };
        let v = self.mfunc.alloc_vreg(cls);
        self.push_op(
            MOp::MovImm { dst: MReg::V(v), imm, wide },
            MInstFlags::REMATERIALIZABLE,
        );
        v
    }

    /// Emits a fresh `VReg` holding float constant bits (`double` = f64).
    fn materialize_fp_const(&mut self, bits: u64, double: bool) -> VReg {
        let cls = if double { VRegClass::Fp64 } else { VRegClass::Fp32 };
        let dst = self.mfunc.alloc_vreg(cls);
        if bits == 0 {
            self.push_op(
                MOp::FpAlu {
                    op: FpAluOp::MovFromGpr,
                    dst: MReg::V(dst),
                    rn: MReg::XZR,
                    rm: None,
                    ra: None,
                    double,
                },
                MInstFlags::NONE,
            );
        } else {
            let gpr = self.materialize_int_const(bits, double);
            self.push_op(
                MOp::FpAlu {
                    op: FpAluOp::MovFromGpr,
                    dst: MReg::V(dst),
                    rn: MReg::V(gpr),
                    rm: None,
                    ra: None,
                    double,
                },
                MInstFlags::NONE,
            );
        }
        dst
    }

    /// Returns an `MReg` for LIR value `val`, materializing constants on demand into a fresh `VReg`
    /// (preserving SSA dominance across blocks) or returning `MReg::XZR` if `allow_xzr && const == 0`.
    fn get_val(&mut self, val: Value, allow_xzr: bool) -> MReg {
        let idx = val.index();
        let existing = self.val_vreg.get(idx).copied().unwrap_or(NONE_U32);
        if existing != NONE_U32 {
            return MReg::V(VReg(existing));
        }
        let ty = self.view.ty(val);
        if let Some(c) = self.const_u64.get(idx).copied().flatten() {
            if allow_xzr && c == 0 {
                return MReg::XZR;
            }
            let wide = int_bits(ty) == 64;
            return MReg::V(self.materialize_int_const(c, wide));
        }
        if let Some(bits) = self.const_fp_bits.get(idx).copied().flatten() {
            return MReg::V(self.materialize_fp_const(bits, ty == Ty::F64));
        }
        if self.view.op(idx) == Op::Uninit {
            if is_q128_ty(ty) {
                let v = self.mfunc.alloc_vreg(VRegClass::Vec128);
                self.push_op(
                    MOp::VecAlu {
                        op: VecAluOp::Zero,
                        dst: MReg::V(v),
                        rn: MReg::vec(0),
                        rm: None,
                        elem_size: 0,
                    },
                    MInstFlags::REMATERIALIZABLE,
                );
                return MReg::V(v);
            } else if is_scalar_fp(ty) {
                return MReg::V(self.materialize_fp_const(0, ty == Ty::F64));
            } else if allow_xzr {
                return MReg::XZR;
            } else {
                return MReg::V(self.materialize_int_const(0, int_bits(ty) == 64));
            }
        }
        // Forward reference when Ext-TSP `layout.order` lowers a dominated block before its
        // dominator: pre-allocate the SSA `VReg` and record it in `self.val_vreg[idx]` so the
        // defining instruction reuses and defines the exact same `VReg`.
        let cls = vreg_class_for_ty(ty).unwrap_or(VRegClass::Gpr64);
        let v = self.mfunc.alloc_vreg(cls);
        if let Some(slot) = self.val_vreg.get_mut(idx) {
            *slot = v.0;
        }
        MReg::V(v)
    }

    /// Allocates (or reuses a forward-preallocated) destination `VReg` for LIR instruction `inst`.
    fn alloc_res_vreg(&mut self, inst: usize, ty: Ty) -> VReg {
        if let Some(&existing) = self.val_vreg.get(inst)
            && existing != NONE_U32
        {
            return VReg(existing);
        }
        let cls = vreg_class_for_ty(ty).unwrap_or(VRegClass::Gpr64);
        let v = self.mfunc.alloc_vreg(cls);
        if let Some(slot) = self.val_vreg.get_mut(inst) {
            *slot = v.0;
        }
        v
    }

    /// Binds LIR instruction `inst` (of type `ty`) as a zero-instruction alias of `src` when
    /// `self.val_vreg[inst]` has not already been forward-preallocated and `src` is a compatible
    /// `VReg`; otherwise emits a bank-matched register move into `alloc_res_vreg(inst, ty)`.
    fn bind_alias_vreg(&mut self, inst: usize, src: MReg, ty: Ty) {
        let dst_cls = vreg_class_for_ty(ty).unwrap_or(VRegClass::Gpr64);
        let existing = self.val_vreg.get(inst).copied().unwrap_or(NONE_U32);
        if existing == NONE_U32
            && let MReg::V(sv) = src
            && self
                .mfunc
                .vreg_class
                .get(to_idx(sv.0))
                .copied()
                .is_some_and(|sc| vreg_classes_alias_compat(sc, dst_cls))
        {
            if let Some(slot) = self.val_vreg.get_mut(inst) {
                *slot = sv.0;
            }
            return;
        }
        let dst = self.alloc_res_vreg(inst, ty);
        if MReg::V(dst) == src {
            return;
        }
        match dst_cls {
            VRegClass::Vec128 => {
                self.push_op(
                    MOp::VecAlu {
                        op: VecAluOp::Mov,
                        dst: MReg::V(dst),
                        rn: src,
                        rm: None,
                        elem_size: 0,
                    },
                    MInstFlags::NONE,
                );
            }
            VRegClass::Fp32 => {
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::Mov,
                        dst: MReg::V(dst),
                        rn: src,
                        rm: None,
                        ra: None,
                        double: false,
                    },
                    MInstFlags::NONE,
                );
            }
            VRegClass::Fp64 => {
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::Mov,
                        dst: MReg::V(dst),
                        rn: src,
                        rm: None,
                        ra: None,
                        double: true,
                    },
                    MInstFlags::NONE,
                );
            }
            VRegClass::Gpr32 | VRegClass::Gpr64 | VRegClass::Nzcv => {
                self.push_op(MOp::Mov { dst: MReg::V(dst), src, wide: true }, MInstFlags::NONE);
            }
        }
    }

    /// Lazily gets or pre-allocates `(val_vreg, flag_vreg)` for an overflow instruction so
    /// `Op::Project` and `lower_overflow` always share the exact same SSA `VReg`s regardless of
    /// Ext-TSP `layout.order`.
    fn get_or_alloc_ovf_pair(&mut self, ovf_inst: usize) -> (u32, u32) {
        if let Some(&(v0, v1)) = self.ovf_pair.get(ovf_inst)
            && v0 != NONE_U32
            && v1 != NONE_U32
        {
            return (v0, v1);
        }
        if ovf_inst >= self.view.len() {
            return (NONE_U32, NONE_U32);
        }
        let op = self.view.op(ovf_inst);
        if !matches!(
            op,
            Op::UaddOverflow
                | Op::UsubOverflow
                | Op::SaddOverflow
                | Op::SsubOverflow
                | Op::UmulOverflow
                | Op::SmulOverflow
        ) {
            return (NONE_U32, NONE_U32);
        }
        let a = self.view.operands(ovf_inst).value();
        let op_ty = self.view.ty(a);
        let val_cls = vreg_class_for_ty(op_ty).unwrap_or(VRegClass::Gpr64);
        let val_v = self.mfunc.alloc_vreg(val_cls);
        let flag_v = self.mfunc.alloc_vreg(VRegClass::Gpr32);
        if let Some(slot) = self.ovf_pair.get_mut(ovf_inst) {
            *slot = (val_v.0, flag_v.0);
        }
        (val_v.0, flag_v.0)
    }

    /// Lazily gets or pre-allocates the SSA `VReg` for projected return `field` of a multi-return
    /// `Call` / `CallIndirect` / `Invoke` / `InvokeIndirect` instruction so `Op::Project` and
    /// `lower_call` always share exact SSA `VReg`s regardless of Ext-TSP `layout.order`.
    fn get_or_alloc_call_ret_vreg(&mut self, call_inst: usize, field: usize) -> Option<u32> {
        if call_inst >= self.view.len() {
            return None;
        }
        let op = self.view.op(call_inst);
        if !matches!(op, Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect) {
            return None;
        }
        let res_ty = ty_at(self.view, call_inst);
        let members = self.view.members(res_ty);
        if members.len() < 2 || field >= members.len() {
            return None;
        }
        let existing = self.call_ret_off.get(call_inst).copied().unwrap_or(NONE_U32);
        let base = if existing != NONE_U32 {
            existing
        } else {
            let start = u32_of(self.call_ret_vregs.len())?;
            for &m_raw in members {
                let cls = vreg_class_for_ty(Ty(m_raw)).unwrap_or(VRegClass::Gpr64);
                let v = self.mfunc.alloc_vreg(cls);
                self.call_ret_vregs.push(v.0);
            }
            if let Some(slot) = self.call_ret_off.get_mut(call_inst) {
                *slot = start;
            }
            start
        };
        let idx = to_idx(base).checked_add(field)?;
        self.call_ret_vregs.get(idx).copied()
    }

    /// Appends an unwind row entry and attaches `MNoteKind::Unwind` to `call_inst`.
    fn attach_unwind_row(
        &mut self,
        call_inst: MInst,
        entry: Option<(MBlock, blaze_code::unwind::Action)>,
    ) -> Result<(), TierError> {
        let unwind_idx = u32_of(self.unwind_entries.len()).ok_or(TierError::TooLarge {
            tier: TIER,
            what: "unwind table rows",
        })?;
        self.unwind_entries.push(entry);
        self.mfunc.attach_note(call_inst, MNoteKind::Unwind { unwind_idx });
        Ok(())
    }

    /// Emits `dst = SP + off` (using 1 or 2 `AddImm`s, marked `REMATERIALIZABLE` when single).
    fn emit_sp_plus_off(&mut self, dst: MReg, off: u32) {
        let hi = off >> 12;
        let lo = off & 0xfff;
        if hi == 0 {
            let lo16 = u16_of(lo).unwrap_or(0);
            self.push_op(
                MOp::AddImm {
                    dst,
                    rn: MReg::SP,
                    imm12: lo16,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::REMATERIALIZABLE,
            );
        } else if lo == 0 {
            let hi16 = u16_of(hi).unwrap_or(0);
            self.push_op(
                MOp::AddImm {
                    dst,
                    rn: MReg::SP,
                    imm12: hi16,
                    shift12: true,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::REMATERIALIZABLE,
            );
        } else {
            let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            let hi16 = u16_of(hi).unwrap_or(0);
            let lo16 = u16_of(lo).unwrap_or(0);
            self.push_op(
                MOp::AddImm {
                    dst: MReg::V(tmp),
                    rn: MReg::SP,
                    imm12: hi16,
                    shift12: true,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            self.push_op(
                MOp::AddImm {
                    dst,
                    rn: MReg::V(tmp),
                    imm12: lo16,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
    }

    /// Emits `dst = rn + off` (signed 64-bit byte offset), choosing `AddImm`/`SubImm` when small.
    fn emit_ptr_add_const(&mut self, dst: MReg, base: MReg, off: i64) {
        if off == 0 {
            self.push_op(MOp::Mov { dst, src: base, wide: true }, MInstFlags::NONE);
            return;
        }
        if off > 0 {
            let pos = off.cast_unsigned();
            if let Some((imm12, shift12)) = encode_add_sub_imm12(pos) {
                self.push_op(
                    MOp::AddImm {
                        dst,
                        rn: base,
                        imm12,
                        shift12,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
        } else if let Some(neg) = off.checked_neg() {
            let pos = neg.cast_unsigned();
            if let Some((imm12, shift12)) = encode_add_sub_imm12(pos) {
                self.push_op(
                    MOp::SubImm {
                        dst,
                        rn: base,
                        imm12,
                        shift12,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
        }
        let imm_v = self.materialize_int_const(off.cast_unsigned(), true);
        self.push_op(
            MOp::AddReg {
                dst,
                rn: base,
                rm: MReg::V(imm_v),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
    }

    /// Masks `src` to low `bits` (`1`, `8`, or `16`), writing into `dst` (32-bit zero-extended).
    fn emit_narrow_mask(&mut self, dst: MReg, src: MReg, bits: u32) {
        if bits < 32 {
            let imms = u8_of(bits - 1).unwrap_or(7);
            self.push_op(
                MOp::Ubfm { dst, rn: src, immr: 0, imms, wide: false },
                MInstFlags::NONE,
            );
        }
    }

    /// Sign-extends `src` from `from_bits` (`1`, `8`, `16`, or `32`) into a fresh `VReg` of `wide`.
    fn sign_extend_operand(&mut self, src: MReg, from_bits: u32, wide: bool) -> MReg {
        let target_bits = if wide { 64 } else { 32 };
        if from_bits >= target_bits {
            return src;
        }
        let cls = if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 };
        let tmp = self.mfunc.alloc_vreg(cls);
        let imms = u8_of(from_bits - 1).unwrap_or(7);
        self.push_op(
            MOp::Sbfm { dst: MReg::V(tmp), rn: src, immr: 0, imms, wide },
            MInstFlags::NONE,
        );
        MReg::V(tmp)
    }

    /// Computes `[SP + off]` `AddrMode` for a stack store/load of `size`, materializing base if far.
    fn addr_sp_off(&mut self, off: u32, size: MemSize) -> AddrMode {
        if let Some(off_i32) = fits_base_off_checked(i64::from(off), size) {
            AddrMode::BaseOff { base: MReg::SP, off: off_i32 }
        } else {
            let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            self.emit_sp_plus_off(MReg::V(tmp), off);
            AddrMode::BaseOff { base: MReg::V(tmp), off: 0 }
        }
    }

    /// Computes `[base + off]` `AddrMode` for `size`, materializing `base + off` if out of range.
    fn addr_base_off(&mut self, base: MReg, off: i64, size: MemSize) -> AddrMode {
        if let Some(off_i32) = fits_base_off_checked(off, size) {
            AddrMode::BaseOff { base, off: off_i32 }
        } else {
            let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            self.emit_ptr_add_const(MReg::V(tmp), base, off);
            AddrMode::BaseOff { base: MReg::V(tmp), off: 0 }
        }
    }

    /// Lazily gets or creates the cold trap block for div-by-zero (`BRK_DIV_ZERO`).
    fn get_div_zero_trap(&mut self) -> MBlock {
        if let Some(blk) = self.div_zero_trap_blk {
            blk
        } else {
            let blk = self.new_mblock(false, 1);
            self.div_zero_trap_blk = Some(blk);
            blk
        }
    }

    /// Lazily gets or creates the cold trap block for signed div overflow (`BRK_DIV_OVERFLOW`).
    fn get_div_ovf_trap(&mut self) -> MBlock {
        if let Some(blk) = self.div_ovf_trap_blk {
            blk
        } else {
            let blk = self.new_mblock(false, 1);
            self.div_ovf_trap_blk = Some(blk);
            blk
        }
    }

    /// Splits the currently open `MBlock` immediately after a conditional trap branch, continuing
    /// straight-line lowering in a fresh fallthrough `MBlock` with the same execution weight.
    fn split_fallthrough(&mut self) {
        self.live_cmp = None;
        self.mfunc.end_block(self.cur_blk);
        let cont = self.new_mblock(false, self.cur_weight);
        self.mfunc.begin_block(cont);
        self.cur_blk = cont;
    }

    /// Attempts to reuse currently live NZCV flags from an earlier compatible `CmpImm`/`CmpReg`
    /// on the same canonical operands (in either order, for any compatible `IntCond`).
    fn try_reuse_live_icmp(
        &self,
        cond: IntCond,
        mut a_val: Value,
        mut b_val: Value,
    ) -> Option<(u32, MCond)> {
        let live = self.live_cmp?;
        let live_ic = live.int_cmp?;
        let bits = int_bits(self.view.ty(a_val));
        let mut c = cond;
        let signed_narrow = bits < 32
            && matches!(c, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge);
        if bits != live_ic.bits || signed_narrow != live_ic.signed_narrow {
            return None;
        }
        let a_const = self.const_u64.get(a_val.index()).copied().flatten();
        let b_const = self.const_u64.get(b_val.index()).copied().flatten();
        if a_const.is_some() && b_const.is_none() {
            core::mem::swap(&mut a_val, &mut b_val);
            c = swap_int_cond(c);
        }
        let b_const_now = self
            .const_u64
            .get(b_val.index())
            .copied()
            .flatten()
            .map(|v| v & bit_mask(bits));
        if let Some(imm) = b_const_now {
            if live_ic.rhs_const == Some(imm) && live_ic.lhs == a_val.0 {
                return Some((live.vflag, int_cond_to_mcond(c)));
            }
            return None;
        }
        if live_ic.rhs_const.is_some() {
            return None;
        }
        if live_ic.lhs == a_val.0 && live_ic.rhs == b_val.0 {
            Some((live.vflag, int_cond_to_mcond(c)))
        } else if live_ic.lhs == b_val.0 && live_ic.rhs == a_val.0 {
            Some((live.vflag, int_cond_to_mcond(swap_int_cond(c))))
        } else {
            None
        }
    }

    /// Emits a `CmpImm`/`CmpReg`/`TstImm`/`TstReg` for `Icmp(cond, a, b)` and returns
    /// `(vflag, MCond, Option<LiveIntCmp>)`.
    fn emit_icmp_flags(
        &mut self,
        icmp_inst: usize,
        cond: IntCond,
        mut a_val: Value,
        mut b_val: Value,
    ) -> (u32, MCond, Option<LiveIntCmp>) {
        let mut c = cond;
        let op_ty = self.view.ty(a_val);
        if op_ty == Ty::I128 {
            let (a_lo, a_hi) = self.get_i128_pair(a_val, true);
            let (b_lo, b_hi) = self.get_i128_pair(b_val, true);
            if matches!(c, IntCond::Eq | IntCond::Ne) {
                let t0 = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(t0),
                        rn: a_lo,
                        rm: b_lo,
                        shift: None,
                        invert_rm: false,
                        kind: LogicalKind::Eor,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                let t1 = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(t1),
                        rn: a_hi,
                        rm: b_hi,
                        shift: None,
                        invert_rm: false,
                        kind: LogicalKind::Eor,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                let t2 = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(t2),
                        rn: MReg::V(t0),
                        rm: MReg::V(t1),
                        shift: None,
                        invert_rm: false,
                        kind: LogicalKind::Orr,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpImm { rn: MReg::V(t2), imm12: 0, shift12: false, wide: true },
                    MInstFlags::NONE,
                );
                return (vf, int_cond_to_mcond(c), None);
            }
            // Ordered 128-bit comparison via `subs xzr, lhs_lo, rhs_lo; sbcs xzr, lhs_hi, rhs_hi`.
            // Swap operands for `>` / `<=` so we only ever test `<` (`Lo`/`Lt`) or `>=` (`Hs`/`Ge`),
            // avoiding dependence on `Z` (which only reflects the high 64 bits after `sbcs`).
            let ((lhs_lo, lhs_hi), (rhs_lo, rhs_hi), mc) = match c {
                IntCond::Ult => ((a_lo, a_hi), (b_lo, b_hi), MCond::Lo),
                IntCond::Uge => ((a_lo, a_hi), (b_lo, b_hi), MCond::Hs),
                IntCond::Ugt => ((b_lo, b_hi), (a_lo, a_hi), MCond::Lo),
                IntCond::Ule => ((b_lo, b_hi), (a_lo, a_hi), MCond::Hs),
                IntCond::Slt => ((a_lo, a_hi), (b_lo, b_hi), MCond::Lt),
                IntCond::Sge => ((a_lo, a_hi), (b_lo, b_hi), MCond::Ge),
                IntCond::Sgt => ((b_lo, b_hi), (a_lo, a_hi), MCond::Lt),
                IntCond::Sle => ((b_lo, b_hi), (a_lo, a_hi), MCond::Ge),
                IntCond::Eq => ((a_lo, a_hi), (b_lo, b_hi), MCond::Eq),
                IntCond::Ne => ((a_lo, a_hi), (b_lo, b_hi), MCond::Ne),
            };
            let (_, vf1) = self.push_flag_writer(
                MOp::SubReg {
                    dst: MReg::XZR,
                    rn: lhs_lo,
                    rm: rhs_lo,
                    shift: None,
                    wide: true,
                    sets_flags: true,
                },
                MInstFlags::NONE,
            );
            let vf2 = self.alloc_vflag();
            self.live_cmp = None;
            let mi2 = self.push_op(
                MOp::CarryAddSub {
                    dst: MReg::XZR,
                    rn: lhs_hi,
                    rm: rhs_hi,
                    is_sub: true,
                    sets_flags: true,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(mi2, vf1);
            self.mfunc.set_vflag_def(mi2, vf2);
            return (vf2, mc, None);
        }

        let bits = int_bits(op_ty);
        let wide = bits == 64;
        let signed = matches!(c, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge);

        // Canonicalize constant onto RHS for `CmpImm` / `TstImm`!
        let a_const = self.const_u64.get(a_val.index()).copied().flatten();
        let b_const = self.const_u64.get(b_val.index()).copied().flatten();
        if a_const.is_some() && b_const.is_none() {
            core::mem::swap(&mut a_val, &mut b_val);
            c = swap_int_cond(c);
        }

        // Folded single-use `Icmp(Eq | Ne, And(x, mask_const), 0)` (`SelPattern::IcmpTstImm`):
        // the `And` was skipped by `match_patterns`; emit `TstImm` directly on `x`.
        if matches!(c, IntCond::Eq | IntCond::Ne)
            && let Some(SelPattern::IcmpTstImm { x, mask_val }) =
                self.pattern.get(icmp_inst).copied()
        {
            let x_val = Value(x);
            let mask_c = self
                .const_u64
                .get(Value(mask_val).index())
                .copied()
                .flatten()
                .unwrap_or(0);
            let m = mask_c & bit_mask(bits);
            let xr = self.get_val(x_val, false);
            let (_, vf) = self.push_flag_writer(
                MOp::TstImm { rn: xr, imm: m, wide },
                MInstFlags::NONE,
            );
            return (vf, int_cond_to_mcond(c), None);
        }

        let b_const_now = self.const_u64.get(b_val.index()).copied().flatten();

        // Check `TstImm` for non-skipped multi-use `And(x, mask_const)` compared against `0`!
        if matches!(c, IntCond::Eq | IntCond::Ne)
            && b_const_now == Some(0)
            && self.view.op(a_val.index()) == Op::And
            && self.inst_block.get(a_val.index()).copied() == Some(self.cur_lir_blk)
            && !self.skip_inst.get(a_val.index()).copied().unwrap_or(false)
            && self.pattern.get(a_val.index()).copied() == Some(SelPattern::None)
        {
            let mut ao = self.view.operands(a_val.index());
            let mut x = ao.value();
            let mut y = ao.value();
            let x_c = self.const_u64.get(x.index()).copied().flatten();
            let y_c = self.const_u64.get(y.index()).copied().flatten();
            if x_c.is_some() && y_c.is_none() {
                core::mem::swap(&mut x, &mut y);
            }
            if !self.skip_inst.get(x.index()).copied().unwrap_or(false)
                && let Some(mask_c) = self.const_u64.get(y.index()).copied().flatten()
            {
                let m = mask_c & bit_mask(bits);
                let cmp_w = if wide { 64 } else { 32 };
                if enc::logical_imm(m, cmp_w).is_some() {
                    let xr = self.get_val(x, false);
                    let (_, vf) = self.push_flag_writer(
                        MOp::TstImm { rn: xr, imm: m, wide },
                        MInstFlags::NONE,
                    );
                    return (vf, int_cond_to_mcond(c), None);
                }
            }
        }

        let live_key = LiveIntCmp {
            lhs: a_val.0,
            rhs: b_val.0,
            rhs_const: b_const_now.map(|v| v & bit_mask(bits)),
            bits,
            signed_narrow: signed && bits < 32,
        };

        // For narrow signed comparisons (`I1`/`I8`/`I16`), sign-extend both operands to 32 bits.
        if signed && bits < 32 {
            let a_raw = self.get_val(a_val, false);
            let b_raw = self.get_val(b_val, false);
            let a_s = self.sign_extend_operand(a_raw, bits, false);
            let b_s = self.sign_extend_operand(b_raw, bits, false);
            let (_, vf) = self.push_flag_writer(
                MOp::CmpReg { rn: a_s, rm: b_s, shift: None, wide: false },
                MInstFlags::NONE,
            );
            return (vf, int_cond_to_mcond(c), Some(live_key));
        }

        let a_reg = self.get_val(a_val, false);
        if let Some(imm) = b_const_now {
            let masked_imm = imm & bit_mask(bits);
            if let Some((imm12, shift12)) = encode_add_sub_imm12(masked_imm) {
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpImm { rn: a_reg, imm12, shift12, wide },
                    MInstFlags::NONE,
                );
                return (vf, int_cond_to_mcond(c), Some(live_key));
            }
        }
        let b_reg = self.get_val(b_val, true);
        let (_, vf) = self.push_flag_writer(
            MOp::CmpReg { rn: a_reg, rm: b_reg, shift: None, wide },
            MInstFlags::NONE,
        );
        (vf, int_cond_to_mcond(c), Some(live_key))
    }

    /// Emits `FpCmp` for a single-condition `Fcmp`, returning `(vflag, MCond)`.
    fn emit_fcmp_flags(&mut self, cond: FloatCond, a_val: Value, b_val: Value) -> Option<(u32, MCond)> {
        let mc = float_cond_to_mcond(cond)?;
        let double = self.view.ty(a_val) == Ty::F64;
        let a_reg = self.get_val(a_val, false);
        let b_reg = self.get_val(b_val, false);
        let (_, vf) = self.push_flag_writer(
            MOp::FpCmp { rn: a_reg, rm: b_reg, double },
            MInstFlags::NONE,
        );
        Some((vf, mc))
    }

    /// Ensures live flags for LIR condition `cond_val` (reusing `self.live_cmp` across
    /// non-flag-clobbering ops and compatible sibling `Icmp`s, or emitting a fresh `Cmp`/`FpCmp`).
    fn ensure_cond_flags(&mut self, cond_val: Value) -> (u32, MCond) {
        if let Some(live) = self.live_cmp
            && live.lir_cmp == cond_val.0
        {
            return (live.vflag, live.cond);
        }

        let (eff_val, inverted) = peel_bool_cond(
            self.view,
            &self.const_u64,
            &self.inst_block,
            cond_val,
            self.cur_lir_blk,
        );
        let apply_inv = |mc: MCond| if inverted { mc.invert() } else { mc };

        let ci = eff_val.index();
        if let Some(live) = self.live_cmp
            && live.lir_cmp == eff_val.0
        {
            return (live.vflag, apply_inv(live.cond));
        }

        let op = self.view.op(ci);
        let in_cur_block = self.inst_block.get(ci).copied() == Some(self.cur_lir_blk);
        if op == Op::Icmp {
            let mut o = self.view.operands(ci);
            let ic = o.int_cond();
            let a = o.value();
            let b = o.value();
            if let Some((vf, mc)) = self.try_reuse_live_icmp(ic, a, b) {
                return (vf, apply_inv(mc));
            }
            if in_cur_block && self.cmp_all_fused.get(ci).copied().unwrap_or(false) {
                let (vf, mc, int_cmp) = self.emit_icmp_flags(ci, ic, a, b);
                self.live_cmp = Some(LiveCmp {
                    lir_cmp: eff_val.0,
                    vflag: vf,
                    cond: mc,
                    int_cmp,
                });
                return (vf, apply_inv(mc));
            }
        }
        if op == Op::Fcmp && in_cur_block && self.cmp_all_fused.get(ci).copied().unwrap_or(false) {
            let mut o = self.view.operands(ci);
            let fc = o.float_cond();
            let a = o.value();
            let b = o.value();
            if let Some((vf, mc)) = self.emit_fcmp_flags(fc, a, b) {
                self.live_cmp = Some(LiveCmp {
                    lir_cmp: eff_val.0,
                    vflag: vf,
                    cond: mc,
                    int_cmp: None,
                });
                return (vf, apply_inv(mc));
            }
        }

        let cond_reg = self.get_val(eff_val, false);
        let (_, vf) = self.push_flag_writer(
            MOp::CmpImm { rn: cond_reg, imm12: 0, shift12: false, wide: false },
            MInstFlags::NONE,
        );
        self.live_cmp = Some(LiveCmp {
            lir_cmp: eff_val.0,
            vflag: vf,
            cond: MCond::Ne,
            int_cmp: None,
        });
        (vf, apply_inv(MCond::Ne))
    }

    /// Emits a direct call sequence (`DirectBl` or `GotSlot`) to `target_id` with `clobbers`,
    /// returning the emitted call (`Bl` / `Blr`) instruction for side-band note attachment.
    fn emit_direct_call_site(&mut self, target_id: u32, clobbers: CallClobbers) -> MInst {
        self.has_calls = true;
        self.live_cmp = None;
        self.call_clobbers_seq.push(clobbers);
        match self.opts.call_mode {
            CallMode::DirectBl => {
                let bl = self.push_op(MOp::Bl, MInstFlags::CALL.union(MInstFlags::PINNED));
                self.mfunc.attach_note(
                    bl,
                    MNoteKind::Reloc {
                        kind: RelocKind::Branch26,
                        target: target_id,
                        addend: 0,
                    },
                );
                bl
            }
            CallMode::GotSlot => {
                let adrp = self.push_op(
                    MOp::Adrp { dst: MReg::p(PReg::X16) },
                    MInstFlags::PINNED,
                );
                self.mfunc.attach_note(
                    adrp,
                    MNoteKind::Reloc {
                        kind: RelocKind::GotPage21,
                        target: target_id,
                        addend: 0,
                    },
                );
                let ldr = self.push_op(
                    MOp::Ldr {
                        dst: MReg::p(PReg::X16),
                        addr: AddrMode::BaseOff { base: MReg::p(PReg::X16), off: 0 },
                        size: MemSize::X64,
                        signed: false,
                    },
                    MInstFlags::MEM_READ.union(MInstFlags::PINNED),
                );
                self.mfunc.attach_note(
                    ldr,
                    MNoteKind::Reloc {
                        kind: RelocKind::GotPageOff12,
                        target: target_id,
                        addend: 0,
                    },
                );
                self.push_op(
                    MOp::Blr { target: MReg::p(PReg::X16) },
                    MInstFlags::CALL.union(MInstFlags::PINNED),
                )
            }
        }
    }

    /// Emits a C runtime call (`memcpy` / `memmove` / `memset`) taking pre-lowered `(X0, X1, X2)`.
    fn emit_c_bulk_call_regs(&mut self, name: &str, r0: MReg, r1: MReg, r2: MReg) -> Result<(), TierError> {
        let target_id = self.targets.intern_c_name(name)?;
        let pairs = [
            (MReg::p(PReg::X0), r0),
            (MReg::p(PReg::X1), r1),
            (MReg::p(PReg::X2), r2),
        ];
        self.mfunc.push_parallel_copy(&pairs, MInstFlags::NONE);
        let _ = self.emit_direct_call_site(target_id, CallClobbers::C_ABI_ARM64);
        Ok(())
    }

    /// Emits a C runtime call (`memcpy` / `memmove` / `memset`) taking `(X0, X1, X2)`.
    fn emit_c_bulk_call(&mut self, name: &str, a0: Value, a1: Value, a2: Value) -> Result<(), TierError> {
        let r0 = self.get_val(a0, true);
        let r1 = self.get_val(a1, true);
        let r2 = self.get_val(a2, true);
        self.emit_c_bulk_call_regs(name, r0, r1, r2)
    }

    /// Emits a non-overlapping fixed-length byte copy from `src_r` to `dst_r` (inline Q128/GPR
    /// up to `INLINE_BULK_MAX`, or a C `memcpy` call above that).
    fn emit_const_memcpy_regs(&mut self, dst_r: MReg, src_r: MReg, n: u64) -> Result<(), TierError> {
        if n == 0 {
            return Ok(());
        }
        if n <= INLINE_BULK_MAX {
            let mut off: i32 = 0;
            let mut rem = n;
            while rem >= 16 {
                let tmp = self.mfunc.alloc_vreg(VRegClass::Vec128);
                self.push_op(
                    MOp::Ldr {
                        dst: MReg::V(tmp),
                        addr: AddrMode::BaseOff { base: src_r, off },
                        size: MemSize::Q128,
                        signed: false,
                    },
                    MInstFlags::MEM_READ,
                );
                self.push_op(
                    MOp::Str {
                        src: MReg::V(tmp),
                        addr: AddrMode::BaseOff { base: dst_r, off },
                        size: MemSize::Q128,
                    },
                    MInstFlags::MEM_WRITE,
                );
                off += 16;
                rem -= 16;
            }
            for (bytes, sz, cls) in [
                (8u64, MemSize::X64, VRegClass::Gpr64),
                (4, MemSize::W32, VRegClass::Gpr32),
                (2, MemSize::H16, VRegClass::Gpr32),
                (1, MemSize::B8, VRegClass::Gpr32),
            ] {
                if rem >= bytes {
                    let tmp = self.mfunc.alloc_vreg(cls);
                    self.push_op(
                        MOp::Ldr {
                            dst: MReg::V(tmp),
                            addr: AddrMode::BaseOff { base: src_r, off },
                            size: sz,
                            signed: false,
                        },
                        MInstFlags::MEM_READ,
                    );
                    self.push_op(
                        MOp::Str {
                            src: MReg::V(tmp),
                            addr: AddrMode::BaseOff { base: dst_r, off },
                            size: sz,
                        },
                        MInstFlags::MEM_WRITE,
                    );
                    off += i32::try_from(bytes).unwrap_or(0);
                    rem -= bytes;
                }
            }
            return Ok(());
        }
        let len_v = self.materialize_int_const(n, true);
        self.emit_c_bulk_call_regs("memcpy", dst_r, src_r, MReg::V(len_v))
    }

    /// Allocates a `(lo, hi)` `Gpr64` SSA `VReg` pair for `Ty::I128` instruction `inst`.
    fn alloc_i128_pair(&mut self, inst: usize) -> (VReg, VReg) {
        let cur = at(&self.i128_pair, inst, (NONE_U32, NONE_U32));
        if cur.0 != NONE_U32 {
            return (VReg(cur.0), VReg(cur.1));
        }
        let lo = self.mfunc.alloc_vreg(VRegClass::Gpr64);
        let hi = self.mfunc.alloc_vreg(VRegClass::Gpr64);
        set(&mut self.i128_pair, inst, (lo.0, hi.0));
        (lo, hi)
    }

    /// Binds `Ty::I128` instruction `inst` to `(lo, hi)`, aliasing zero-cost when `inst` has NOT
    /// been forward-preallocated by a non-RPO block and both halves are `Gpr64` `VReg`s, or
    /// emitting 64-bit `Mov`s into `alloc_i128_pair(inst)` otherwise.
    fn bind_i128_pair(&mut self, inst: usize, lo: MReg, hi: MReg) {
        let existing = at(&self.i128_pair, inst, (NONE_U32, NONE_U32));
        if existing.0 == NONE_U32
            && let (MReg::V(lv), MReg::V(hv)) = (lo, hi)
            && self.mfunc.vreg_class.get(to_idx(lv.0)).copied() == Some(VRegClass::Gpr64)
            && self.mfunc.vreg_class.get(to_idx(hv.0)).copied() == Some(VRegClass::Gpr64)
        {
            set(&mut self.i128_pair, inst, (lv.0, hv.0));
            return;
        }
        let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
        if MReg::V(dst_lo) != lo {
            self.push_op(MOp::Mov { dst: MReg::V(dst_lo), src: lo, wide: true }, MInstFlags::NONE);
        }
        if MReg::V(dst_hi) != hi {
            self.push_op(MOp::Mov { dst: MReg::V(dst_hi), src: hi, wide: true }, MInstFlags::NONE);
        }
    }

    #[inline]
    fn i128_has_q128(&self, val: Value) -> bool {
        at(&self.i128_is_q128, val.index(), false)
    }

    /// Returns `(lo, hi)` 64-bit GPR operands for a `Ty::I128` LIR value, lazily materializing
    /// 128-bit constants, extracting 64-bit lanes from `Q128` loads via `umov`, or forward
    /// pre-allocating `i128_pair[idx]` when Ext-TSP lowers a dominated block before its dominator.
    fn get_i128_pair(&mut self, val: Value, allow_xzr: bool) -> (MReg, MReg) {
        let idx = val.index();
        if let Some((lo_c, hi_c)) = self.const_i128.get(idx).copied().flatten() {
            let lo = if allow_xzr && lo_c == 0 {
                MReg::XZR
            } else {
                MReg::V(self.materialize_int_const(lo_c, true))
            };
            let hi = if allow_xzr && hi_c == 0 {
                MReg::XZR
            } else {
                MReg::V(self.materialize_int_const(hi_c, true))
            };
            return (lo, hi);
        }
        if self.view.op(idx) == Op::Uninit {
            if allow_xzr {
                return (MReg::XZR, MReg::XZR);
            }
            let z = self.materialize_int_const(0, true);
            return (MReg::V(z), MReg::V(z));
        }
        if self.i128_has_q128(val) {
            let q = self.get_val(val, false);
            let lo = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            let hi = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            self.push_op(
                MOp::VecAlu {
                    op: VecAluOp::UmovLane { lane: 0 },
                    dst: MReg::V(lo),
                    rn: q,
                    rm: None,
                    elem_size: 3,
                },
                MInstFlags::NONE,
            );
            self.push_op(
                MOp::VecAlu {
                    op: VecAluOp::UmovLane { lane: 1 },
                    dst: MReg::V(hi),
                    rn: q,
                    rm: None,
                    elem_size: 3,
                },
                MInstFlags::NONE,
            );
            return (MReg::V(lo), MReg::V(hi));
        }
        let (lo, hi) = self.alloc_i128_pair(idx);
        (MReg::V(lo), MReg::V(hi))
    }

    /// Lowers a `Ty::I128` scalar-pair ALU or shift instruction.
    fn lower_i128_alu(&mut self, inst: usize, op: Op, a: Value, b_opt: Option<Value>) {
        let (a_lo, a_hi) = self.get_i128_pair(a, true);
        match op {
            Op::Add | Op::Sub => {
                let b = b_opt.unwrap_or(a);
                let (b_lo, b_hi) = self.get_i128_pair(b, true);
                let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
                let is_sub = op == Op::Sub;
                let lo_op = if is_sub {
                    MOp::SubReg {
                        dst: MReg::V(dst_lo),
                        rn: a_lo,
                        rm: b_lo,
                        shift: None,
                        wide: true,
                        sets_flags: true,
                    }
                } else {
                    MOp::AddReg {
                        dst: MReg::V(dst_lo),
                        rn: a_lo,
                        rm: b_lo,
                        shift: None,
                        wide: true,
                        sets_flags: true,
                    }
                };
                let (_, vf) = self.push_flag_writer(lo_op, MInstFlags::NONE);
                let mi_hi = self.push_op(
                    MOp::CarryAddSub {
                        dst: MReg::V(dst_hi),
                        rn: a_hi,
                        rm: b_hi,
                        is_sub,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi_hi, vf);
            }
            Op::Neg => {
                let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
                let (_, vf) = self.push_flag_writer(
                    MOp::SubReg {
                        dst: MReg::V(dst_lo),
                        rn: MReg::XZR,
                        rm: a_lo,
                        shift: None,
                        wide: true,
                        sets_flags: true,
                    },
                    MInstFlags::NONE,
                );
                let mi_hi = self.push_op(
                    MOp::CarryAddSub {
                        dst: MReg::V(dst_hi),
                        rn: MReg::XZR,
                        rm: a_hi,
                        is_sub: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi_hi, vf);
            }
            Op::And | Op::Or | Op::Xor => {
                let b = b_opt.unwrap_or(a);
                let (b_lo, b_hi) = self.get_i128_pair(b, true);
                let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
                let kind = match op {
                    Op::And => LogicalKind::And,
                    Op::Or => LogicalKind::Orr,
                    _ => LogicalKind::Eor,
                };
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(dst_lo),
                        rn: a_lo,
                        rm: b_lo,
                        shift: None,
                        invert_rm: false,
                        kind,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(dst_hi),
                        rn: a_hi,
                        rm: b_hi,
                        shift: None,
                        invert_rm: false,
                        kind,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
            }
            Op::Not => {
                let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
                self.push_op(
                    MOp::Mvn { dst: MReg::V(dst_lo), rm: a_lo, shift: None, wide: true },
                    MInstFlags::NONE,
                );
                self.push_op(
                    MOp::Mvn { dst: MReg::V(dst_hi), rm: a_hi, shift: None, wide: true },
                    MInstFlags::NONE,
                );
            }
            Op::Mul => {
                let b = b_opt.unwrap_or(a);
                let (b_lo, b_hi) = self.get_i128_pair(b, true);
                let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
                self.push_op(
                    MOp::Mul { dst: MReg::V(dst_lo), rn: a_lo, rm: b_lo, wide: true },
                    MInstFlags::NONE,
                );
                let hi0 = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::Umulh { dst: MReg::V(hi0), rn: a_lo, rm: b_lo },
                    MInstFlags::NONE,
                );
                let hi1 = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::Madd {
                        dst: MReg::V(hi1),
                        rn: a_lo,
                        rm: b_hi,
                        ra: MReg::V(hi0),
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                self.push_op(
                    MOp::Madd {
                        dst: MReg::V(dst_hi),
                        rn: a_hi,
                        rm: b_lo,
                        ra: MReg::V(hi1),
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
            }
            Op::Shl | Op::Lshr | Op::Ashr => {
                let b = b_opt.unwrap_or(a);
                let const_k = if self.view.ty(b) == Ty::I128 {
                    self.const_i128.get(b.index()).copied().flatten().map(|(lo, _)| lo & 127)
                } else {
                    self.const_u64.get(b.index()).copied().flatten().map(|v| v & 127)
                };
                if let Some(k) = const_k {
                    let k_u8 = u8::try_from(k).unwrap_or(0);
                    let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
                    if k_u8 == 0 {
                        self.push_op(
                            MOp::Mov { dst: MReg::V(dst_lo), src: a_lo, wide: true },
                            MInstFlags::NONE,
                        );
                        self.push_op(
                            MOp::Mov { dst: MReg::V(dst_hi), src: a_hi, wide: true },
                            MInstFlags::NONE,
                        );
                    } else if op == Op::Shl {
                        if k_u8 < 64 {
                            self.push_op(
                                MOp::Ubfm {
                                    dst: MReg::V(dst_lo),
                                    rn: a_lo,
                                    immr: 64 - k_u8,
                                    imms: 63 - k_u8,
                                    wide: true,
                                },
                                MInstFlags::NONE,
                            );
                            self.push_op(
                                MOp::Extr {
                                    dst: MReg::V(dst_hi),
                                    rn: a_hi,
                                    rm: a_lo,
                                    lsb: 64 - k_u8,
                                    wide: true,
                                },
                                MInstFlags::NONE,
                            );
                        } else {
                            self.push_op(
                                MOp::MovImm { dst: MReg::V(dst_lo), imm: 0, wide: true },
                                MInstFlags::REMATERIALIZABLE,
                            );
                            let rem = k_u8 - 64;
                            if rem == 0 {
                                self.push_op(
                                    MOp::Mov { dst: MReg::V(dst_hi), src: a_lo, wide: true },
                                    MInstFlags::NONE,
                                );
                            } else {
                                self.push_op(
                                    MOp::Ubfm {
                                        dst: MReg::V(dst_hi),
                                        rn: a_lo,
                                        immr: 64 - rem,
                                        imms: 63 - rem,
                                        wide: true,
                                    },
                                    MInstFlags::NONE,
                                );
                            }
                        }
                    } else {
                        let is_signed = op == Op::Ashr;
                        if k_u8 < 64 {
                            self.push_op(
                                MOp::Extr {
                                    dst: MReg::V(dst_lo),
                                    rn: a_hi,
                                    rm: a_lo,
                                    lsb: k_u8,
                                    wide: true,
                                },
                                MInstFlags::NONE,
                            );
                            let hi_op = if is_signed {
                                MOp::Sbfm { dst: MReg::V(dst_hi), rn: a_hi, immr: k_u8, imms: 63, wide: true }
                            } else {
                                MOp::Ubfm { dst: MReg::V(dst_hi), rn: a_hi, immr: k_u8, imms: 63, wide: true }
                            };
                            self.push_op(hi_op, MInstFlags::NONE);
                        } else {
                            let rem = k_u8 - 64;
                            if rem == 0 {
                                self.push_op(
                                    MOp::Mov { dst: MReg::V(dst_lo), src: a_hi, wide: true },
                                    MInstFlags::NONE,
                                );
                            } else if is_signed {
                                self.push_op(
                                    MOp::Sbfm { dst: MReg::V(dst_lo), rn: a_hi, immr: rem, imms: 63, wide: true },
                                    MInstFlags::NONE,
                                );
                            } else {
                                self.push_op(
                                    MOp::Ubfm { dst: MReg::V(dst_lo), rn: a_hi, immr: rem, imms: 63, wide: true },
                                    MInstFlags::NONE,
                                );
                            }
                            if is_signed {
                                self.push_op(
                                    MOp::Sbfm { dst: MReg::V(dst_hi), rn: a_hi, immr: 63, imms: 63, wide: true },
                                    MInstFlags::NONE,
                                );
                            } else {
                                self.push_op(
                                    MOp::MovImm { dst: MReg::V(dst_hi), imm: 0, wide: true },
                                    MInstFlags::REMATERIALIZABLE,
                                );
                            }
                        }
                    }
                    return;
                }

                // Variable 128-bit shift using `inv = ~s` identity (`(x >> 1) >> (~s & 63)` is 0 when s&63 == 0).
                let sr = if self.view.ty(b) == Ty::I128 {
                    self.get_i128_pair(b, true).0
                } else {
                    self.get_val(b, true)
                };
                let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
                let inv_s = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::Mvn { dst: MReg::V(inv_s), rm: sr, shift: None, wide: true },
                    MInstFlags::NONE,
                );
                if op == Op::Shl {
                    let lo_small = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::ShiftVar { dst: MReg::V(lo_small), rn: a_lo, rm: sr, kind: ShiftKind::Lsl, wide: true },
                        MInstFlags::NONE,
                    );
                    let hi_sh = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::ShiftVar { dst: MReg::V(hi_sh), rn: a_hi, rm: sr, kind: ShiftKind::Lsl, wide: true },
                        MInstFlags::NONE,
                    );
                    let carry1 = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(carry1), rn: a_lo, immr: 1, imms: 63, wide: true },
                        MInstFlags::NONE,
                    );
                    let carry = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::ShiftVar {
                            dst: MReg::V(carry),
                            rn: MReg::V(carry1),
                            rm: MReg::V(inv_s),
                            kind: ShiftKind::Lsr,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    let hi_small = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::LogicalReg {
                            dst: MReg::V(hi_small),
                            rn: MReg::V(hi_sh),
                            rm: MReg::V(carry),
                            shift: None,
                            invert_rm: false,
                            kind: LogicalKind::Orr,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    let (_, vf) = self.push_flag_writer(
                        MOp::TstImm { rn: sr, imm: 64, wide: true },
                        MInstFlags::NONE,
                    );
                    let m0 = self.push_op(
                        MOp::Csel {
                            dst: MReg::V(dst_lo),
                            rn: MReg::XZR,
                            rm: MReg::V(lo_small),
                            cond: MCond::Ne,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    self.mfunc.set_vflag_use(m0, vf);
                    let m1 = self.push_op(
                        MOp::Csel {
                            dst: MReg::V(dst_hi),
                            rn: MReg::V(lo_small),
                            rm: MReg::V(hi_small),
                            cond: MCond::Ne,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    self.mfunc.set_vflag_use(m1, vf);
                } else {
                    let is_signed = op == Op::Ashr;
                    let lo_sh = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::ShiftVar { dst: MReg::V(lo_sh), rn: a_lo, rm: sr, kind: ShiftKind::Lsr, wide: true },
                        MInstFlags::NONE,
                    );
                    let hi_small = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::ShiftVar {
                            dst: MReg::V(hi_small),
                            rn: a_hi,
                            rm: sr,
                            kind: if is_signed { ShiftKind::Asr } else { ShiftKind::Lsr },
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    let carry1 = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(carry1), rn: a_hi, immr: 63, imms: 62, wide: true },
                        MInstFlags::NONE,
                    );
                    let carry = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::ShiftVar {
                            dst: MReg::V(carry),
                            rn: MReg::V(carry1),
                            rm: MReg::V(inv_s),
                            kind: ShiftKind::Lsl,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    let lo_small = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::LogicalReg {
                            dst: MReg::V(lo_small),
                            rn: MReg::V(lo_sh),
                            rm: MReg::V(carry),
                            shift: None,
                            invert_rm: false,
                            kind: LogicalKind::Orr,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    let hi_big = if is_signed {
                        let sgn = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                        self.push_op(
                            MOp::Sbfm { dst: MReg::V(sgn), rn: a_hi, immr: 63, imms: 63, wide: true },
                            MInstFlags::NONE,
                        );
                        MReg::V(sgn)
                    } else {
                        MReg::XZR
                    };
                    let (_, vf) = self.push_flag_writer(
                        MOp::TstImm { rn: sr, imm: 64, wide: true },
                        MInstFlags::NONE,
                    );
                    let m0 = self.push_op(
                        MOp::Csel {
                            dst: MReg::V(dst_lo),
                            rn: MReg::V(hi_small),
                            rm: MReg::V(lo_small),
                            cond: MCond::Ne,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    self.mfunc.set_vflag_use(m0, vf);
                    let m1 = self.push_op(
                        MOp::Csel {
                            dst: MReg::V(dst_hi),
                            rn: hi_big,
                            rm: MReg::V(hi_small),
                            cond: MCond::Ne,
                            wide: true,
                        },
                        MInstFlags::NONE,
                    );
                    self.mfunc.set_vflag_use(m1, vf);
                }
            }
            _ => {}
        }
    }

    /// Lowers a `Ty::I128` scalar-pair store as two adjacent 64-bit `Str`s (paired into `Stp` by Pass 3).
    fn lower_i128_store(&mut self, val: Value, ptr: Value, off: i32, flags: Flags, pat: SelPattern) {
        let (lo, hi) = self.get_i128_pair(val, true);
        let (addr_lo, addr_hi) = match pat {
            SelPattern::MemSp { off: sp_off } => (
                self.addr_base_off(MReg::SP, i64::from(sp_off), MemSize::X64),
                self.addr_base_off(MReg::SP, i64::from(sp_off) + 8, MemSize::X64),
            ),
            SelPattern::MemBaseOff { base, off: comb_off } => {
                let br = self.get_val(Value(base), false);
                (
                    self.addr_base_off(br, i64::from(comb_off), MemSize::X64),
                    self.addr_base_off(br, i64::from(comb_off) + 8, MemSize::X64),
                )
            }
            SelPattern::MemScaledIdx { base, idx, shift } => {
                let br = self.get_val(Value(base), false);
                let ir = self.get_val(Value(idx), true);
                let ptr_v = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                let mshift = if shift != 0 { Some(MShift::Lsl(shift)) } else { None };
                self.push_op(
                    MOp::AddReg {
                        dst: MReg::V(ptr_v),
                        rn: br,
                        rm: ir,
                        shift: mshift,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                (
                    AddrMode::BaseOff { base: MReg::V(ptr_v), off: 0 },
                    AddrMode::BaseOff { base: MReg::V(ptr_v), off: 8 },
                )
            }
            _ => {
                let br = self.get_val(ptr, false);
                (
                    self.addr_base_off(br, i64::from(off), MemSize::X64),
                    self.addr_base_off(br, i64::from(off) + 8, MemSize::X64),
                )
            }
        };
        let is_vol = flags.contains(Flags::VOLATILE);
        let mflags = if is_vol {
            MInstFlags::MEM_WRITE.union(MInstFlags::VOLATILE)
        } else {
            MInstFlags::MEM_WRITE
        };
        let m0 = self.push_op(MOp::Str { src: lo, addr: addr_lo, size: MemSize::X64 }, mflags);
        let m1 = self.push_op(MOp::Str { src: hi, addr: addr_hi, size: MemSize::X64 }, mflags);
        if is_vol {
            self.mfunc.attach_note(m0, MNoteKind::Volatile);
            self.mfunc.attach_note(m1, MNoteKind::Volatile);
        }
    }

    /// Lowers `Op::Switch` on a `Ty::I128` scrutinee via `(lo ^ c_lo) | (hi ^ c_hi) == 0` chain.
    fn lower_switch_i128(
        &mut self,
        scrut: Value,
        default_tgt: blaze_lir::op::Target<'_>,
        cases: blaze_lir::op::Cases<'_>,
        scratch_pairs: &mut Vec<(MReg, MReg), Alloc<'a>>,
    ) {
        let (sr_lo, sr_hi) = self.get_i128_pair(scrut, true);
        let mut resolved: Vec<((u64, u64), MBlock), Alloc<'a>> = Vec::new_in(self.alloc);
        for (words, tgt) in cases {
            let w0 = words.first().copied().unwrap_or(0);
            let w1 = words.get(1).copied().unwrap_or(0);
            let w2 = words.get(2).copied().unwrap_or(0);
            let w3 = words.get(3).copied().unwrap_or(0);
            let lo = u64::from(w0) | (u64::from(w1) << 32);
            let hi = u64::from(w2) | (u64::from(w3) << 32);
            let mblk = self.resolve_deferred_edge(tgt.block, tgt.args, scratch_pairs);
            resolved.push(((lo, hi), mblk));
        }
        if resolved.is_empty() {
            let def_mblk = at(&self.mblock_of_lir, default_tgt.block.index(), MBlock(0));
            self.build_edge_copy_pairs(default_tgt.block, default_tgt.args, scratch_pairs);
            if !scratch_pairs.is_empty() {
                self.mfunc.push_parallel_copy(scratch_pairs, MInstFlags::NONE);
            }
            self.push_op(MOp::B { target: def_mblk }, MInstFlags::TERMINATOR);
            return;
        }
        let def_mblk = self.resolve_deferred_edge(default_tgt.block, default_tgt.args, scratch_pairs);
        let n = resolved.len();
        for (k, &((c_lo, c_hi), ctgt)) in resolved.iter().enumerate() {
            let is_last = k + 1 == n;
            let fall_blk = if is_last {
                def_mblk
            } else {
                self.new_mblock(false, self.cur_weight)
            };
            let d_lo = if c_lo == 0 {
                sr_lo
            } else {
                let cv = self.materialize_int_const(c_lo, true);
                let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(tmp),
                        rn: sr_lo,
                        rm: MReg::V(cv),
                        shift: None,
                        invert_rm: false,
                        kind: LogicalKind::Eor,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                MReg::V(tmp)
            };
            let d_hi = if c_hi == 0 {
                sr_hi
            } else {
                let cv = self.materialize_int_const(c_hi, true);
                let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(tmp),
                        rn: sr_hi,
                        rm: MReg::V(cv),
                        shift: None,
                        invert_rm: false,
                        kind: LogicalKind::Eor,
                        wide: true,
                    },
                    MInstFlags::NONE,
                );
                MReg::V(tmp)
            };
            let diff = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            self.push_op(
                MOp::LogicalReg {
                    dst: MReg::V(diff),
                    rn: d_lo,
                    rm: d_hi,
                    shift: None,
                    invert_rm: false,
                    kind: LogicalKind::Orr,
                    wide: true,
                },
                MInstFlags::NONE,
            );
            self.push_op(
                MOp::Cbz { rt: MReg::V(diff), target: ctgt, wide: true },
                MInstFlags::TERMINATOR,
            );
            self.push_op(MOp::B { target: fall_blk }, MInstFlags::TERMINATOR);
            if !is_last {
                self.mfunc.end_block(self.cur_blk);
                self.mfunc.begin_block(fall_blk);
                self.cur_blk = fall_blk;
                self.live_cmp = None;
            }
        }
    }

    /// Builds `ParallelCopy` pairs for target block `target_lir_blk` with arguments `args`,
    /// omitting dead parameters (`use_count == 0`).
    fn build_edge_copy_pairs(
        &mut self,
        target_lir_blk: Block,
        args: blaze_lir::op::Values<'_>,
        out_pairs: &mut Vec<(MReg, MReg), Alloc<'a>>,
    ) {
        out_pairs.clear();
        let param_range = self.view.params(target_lir_blk);
        for (param_inst, arg_val) in param_range.zip(args.iter()) {
            if self.use_count.get(param_inst).copied().unwrap_or(0) == 0 {
                continue;
            }
            if ty_at(self.view, param_inst) == Ty::I128 {
                let (dst_lo, dst_hi) = at(&self.i128_pair, param_inst, (NONE_U32, NONE_U32));
                if dst_lo == NONE_U32 {
                    continue;
                }
                let (src_lo, src_hi) = self.get_i128_pair(arg_val, true);
                let d_lo = MReg::V(VReg(dst_lo));
                let d_hi = MReg::V(VReg(dst_hi));
                if d_lo != src_lo {
                    out_pairs.push((d_lo, src_lo));
                }
                if d_hi != src_hi {
                    out_pairs.push((d_hi, src_hi));
                }
                continue;
            }
            let dst_v = self.val_vreg.get(param_inst).copied().unwrap_or(NONE_U32);
            if dst_v == NONE_U32 {
                continue;
            }
            let src_reg = self.get_val(arg_val, true);
            let dst_reg = MReg::V(VReg(dst_v));
            if dst_reg != src_reg {
                out_pairs.push((dst_reg, src_reg));
            }
        }
    }

    /// Emits a `Select` instruction, handling constant shortcuts, materialized `I1` zero-cost
    /// aliasing, `Csinc`/`Csinv`/`Csneg` fusion, and contiguous sibling `VFlag` reuse.
    fn lower_select(&mut self, inst: usize, cond_val: Value, t_val: Value, f_val: Value) {
        let res_ty = ty_at(self.view, inst);

        // Identical arms: `Select(c, x, x)` -> alias `x`!
        if t_val == f_val {
            if res_ty == Ty::I128 && !self.i128_has_q128(t_val) {
                let (lo, hi) = self.get_i128_pair(t_val, true);
                self.bind_i128_pair(inst, lo, hi);
                return;
            }
            let src = self.get_val(t_val, false);
            self.bind_alias_vreg(inst, src, res_ty);
            return;
        }

        // Scalar-pair `I128` select -> two `csel`s (avoids cross-bank NEON `dup`/`bsl`).
        if res_ty == Ty::I128 && !(self.i128_has_q128(t_val) && self.i128_has_q128(f_val)) {
            let (t_lo, t_hi) = self.get_i128_pair(t_val, true);
            let (f_lo, f_hi) = self.get_i128_pair(f_val, true);
            let (vf, mc) = self.ensure_cond_flags(cond_val);
            let (dst_lo, dst_hi) = self.alloc_i128_pair(inst);
            let m0 = self.push_op(
                MOp::Csel { dst: MReg::V(dst_lo), rn: t_lo, rm: f_lo, cond: mc, wide: true },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(m0, vf);
            let m1 = self.push_op(
                MOp::Csel { dst: MReg::V(dst_hi), rn: t_hi, rm: f_hi, cond: mc, wide: true },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(m1, vf);
            return;
        }

        // 128-bit vector / Q128 `I128` select -> `csetm` + `dup.2d` + `bsl.16b`.
        if is_q128_ty(res_ty) {
            let tr = self.get_val(t_val, false);
            let fr = self.get_val(f_val, false);
            let (vf, mc) = self.ensure_cond_flags(cond_val);
            let mask_gpr = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            let cs = self.push_op(
                MOp::Csinv {
                    dst: MReg::V(mask_gpr),
                    rn: MReg::XZR,
                    rm: MReg::XZR,
                    cond: mc.invert(),
                    wide: true,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(cs, vf);
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.push_op(
                MOp::VecAlu {
                    op: VecAluOp::DupGen,
                    dst: MReg::V(dst),
                    rn: MReg::V(mask_gpr),
                    rm: None,
                    elem_size: 3,
                },
                MInstFlags::NONE,
            );
            self.push_op(
                MOp::VecAlu {
                    op: VecAluOp::Bsl,
                    dst: MReg::V(dst),
                    rn: tr,
                    rm: Some(fr),
                    elem_size: 0,
                },
                MInstFlags::NONE,
            );
            return;
        }

        let t_const = self.const_u64.get(t_val.index()).copied().flatten();
        let f_const = self.const_u64.get(f_val.index()).copied().flatten();

        // Identical constant arms: `Select(c, k, k)` -> `MovImm k`!
        if let (Some(tc), Some(fc)) = (t_const, f_const)
            && tc == fc
            && is_scalar_int_or_ptr(res_ty)
        {
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.push_op(
                MOp::MovImm { dst: MReg::V(dst), imm: tc, wide: int_bits(res_ty) == 64 },
                MInstFlags::REMATERIALIZABLE,
            );
            return;
        }

        // Materialized `I1` shortcuts:
        // - `Select(cond_i1, 1, 0)` -> zero-cost alias (NO `cmp #0; cset ne`!)
        // - `Select(cond_i1, 0, 1)` -> `eor rd, rc, #1`!
        let cond_is_materialized = self.val_vreg.get(cond_val.index()).copied().unwrap_or(NONE_U32) != NONE_U32
            && !self.skip_inst.get(cond_val.index()).copied().unwrap_or(false)
            && !self.cmp_all_fused.get(cond_val.index()).copied().unwrap_or(false)
            && self.live_cmp.is_none_or(|lc| lc.lir_cmp != cond_val.0);

        if cond_is_materialized && is_scalar_int_or_ptr(res_ty) {
            if t_const == Some(1) && f_const == Some(0) {
                let cv = MReg::V(VReg(at(&self.val_vreg, cond_val.index(), NONE_U32)));
                self.bind_alias_vreg(inst, cv, res_ty);
                return;
            }
            if t_const == Some(0) && f_const == Some(1) {
                let cv = MReg::V(VReg(at(&self.val_vreg, cond_val.index(), NONE_U32)));
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::LogicalImm {
                        dst: MReg::V(dst),
                        rn: cv,
                        imm: 1,
                        kind: LogicalKind::Eor,
                        wide: int_bits(res_ty) == 64,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
        }

        // Scalar FP select -> `Fcsel`.
        if is_scalar_fp(res_ty) {
            let double = res_ty == Ty::F64;
            let tr = self.get_val(t_val, false);
            let fr = self.get_val(f_val, false);
            let (vf, mc) = self.ensure_cond_flags(cond_val);
            let dst = self.alloc_res_vreg(inst, res_ty);
            let mi = self.push_op(
                MOp::Fcsel { dst: MReg::V(dst), rn: tr, rm: fr, cond: mc, double },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(mi, vf);
            return;
        }

        // Integer / pointer select -> `Csinc` / `Csinv` / `Csneg` / `Csel`.
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let full_w = bits == 32 || bits == 64;
        let all_ones = bit_mask(bits);

        // `Select(cond, 1, 0)` -> `csinc dst, xzr, xzr, !cond` (`cset`).
        if t_const == Some(1) && f_const == Some(0) {
            let (vf, mc) = self.ensure_cond_flags(cond_val);
            let dst = self.alloc_res_vreg(inst, res_ty);
            let mi = self.push_op(
                MOp::Csinc {
                    dst: MReg::V(dst),
                    rn: MReg::XZR,
                    rm: MReg::XZR,
                    cond: mc.invert(),
                    wide,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(mi, vf);
            return;
        }
        // `Select(cond, 0, 1)` -> `csinc dst, xzr, xzr, cond`.
        if t_const == Some(0) && f_const == Some(1) {
            let (vf, mc) = self.ensure_cond_flags(cond_val);
            let dst = self.alloc_res_vreg(inst, res_ty);
            let mi = self.push_op(
                MOp::Csinc {
                    dst: MReg::V(dst),
                    rn: MReg::XZR,
                    rm: MReg::XZR,
                    cond: mc,
                    wide,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(mi, vf);
            return;
        }
        // `Select(cond, -1, 0)` / `(0, -1)` (full 32/64-bit only) -> `csinv dst, xzr, xzr, ...`.
        if full_w && t_const == Some(all_ones) && f_const == Some(0) {
            let (vf, mc) = self.ensure_cond_flags(cond_val);
            let dst = self.alloc_res_vreg(inst, res_ty);
            let mi = self.push_op(
                MOp::Csinv {
                    dst: MReg::V(dst),
                    rn: MReg::XZR,
                    rm: MReg::XZR,
                    cond: mc.invert(),
                    wide,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(mi, vf);
            return;
        }
        if full_w && t_const == Some(0) && f_const == Some(all_ones) {
            let (vf, mc) = self.ensure_cond_flags(cond_val);
            let dst = self.alloc_res_vreg(inst, res_ty);
            let mi = self.push_op(
                MOp::Csinv {
                    dst: MReg::V(dst),
                    rn: MReg::XZR,
                    rm: MReg::XZR,
                    cond: mc,
                    wide,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(mi, vf);
            return;
        }

        // `Select(cond, x + 1, x)` / `Select(cond, x, x + 1)` -> `csinc dst, x, x, ...` (full 32/64-bit).
        if full_w {
            if let Some(base) = self.match_add_one_of(t_val, f_val) {
                let br = self.get_val(base, true);
                let (vf, mc) = self.ensure_cond_flags(cond_val);
                let dst = self.alloc_res_vreg(inst, res_ty);
                let mi = self.push_op(
                    MOp::Csinc { dst: MReg::V(dst), rn: br, rm: br, cond: mc.invert(), wide },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi, vf);
                return;
            }
            if let Some(base) = self.match_add_one_of(f_val, t_val) {
                let br = self.get_val(base, true);
                let (vf, mc) = self.ensure_cond_flags(cond_val);
                let dst = self.alloc_res_vreg(inst, res_ty);
                let mi = self.push_op(
                    MOp::Csinc { dst: MReg::V(dst), rn: br, rm: br, cond: mc, wide },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi, vf);
                return;
            }
            // `Select(cond, -x, x)` / `Select(cond, x, -x)` -> `csneg dst, x, x, ...`.
            if self.is_neg_of(t_val, f_val) {
                let xr = self.get_val(f_val, true);
                let (vf, mc) = self.ensure_cond_flags(cond_val);
                let dst = self.alloc_res_vreg(inst, res_ty);
                let mi = self.push_op(
                    MOp::Csneg { dst: MReg::V(dst), rn: xr, rm: xr, cond: mc.invert(), wide },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi, vf);
                return;
            }
            if self.is_neg_of(f_val, t_val) {
                let xr = self.get_val(t_val, true);
                let (vf, mc) = self.ensure_cond_flags(cond_val);
                let dst = self.alloc_res_vreg(inst, res_ty);
                let mi = self.push_op(
                    MOp::Csneg { dst: MReg::V(dst), rn: xr, rm: xr, cond: mc, wide },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi, vf);
                return;
            }
            // `Select(cond, !x, x)` / `Select(cond, x, !x)` -> `csinv dst, x, x, ...`.
            if self.is_not_of(t_val, f_val) {
                let xr = self.get_val(f_val, true);
                let (vf, mc) = self.ensure_cond_flags(cond_val);
                let dst = self.alloc_res_vreg(inst, res_ty);
                let mi = self.push_op(
                    MOp::Csinv { dst: MReg::V(dst), rn: xr, rm: xr, cond: mc.invert(), wide },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi, vf);
                return;
            }
            if self.is_not_of(f_val, t_val) {
                let xr = self.get_val(t_val, true);
                let (vf, mc) = self.ensure_cond_flags(cond_val);
                let dst = self.alloc_res_vreg(inst, res_ty);
                let mi = self.push_op(
                    MOp::Csinv { dst: MReg::V(dst), rn: xr, rm: xr, cond: mc, wide },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi, vf);
                return;
            }
        }

        // General `Csel`: materialize operands BEFORE flags if needed (neither touches flags,
        // and doing them before `ensure_cond_flags` keeps `Cmp` adjacent to `Csel`).
        let tr = self.get_val(t_val, true);
        let fr = self.get_val(f_val, true);
        let (vf, mc) = self.ensure_cond_flags(cond_val);
        let dst = self.alloc_res_vreg(inst, res_ty);
        let mi = self.push_op(
            MOp::Csel { dst: MReg::V(dst), rn: tr, rm: fr, cond: mc, wide },
            MInstFlags::NONE,
        );
        self.mfunc.set_vflag_use(mi, vf);
    }

    fn match_add_one_of(&self, cand: Value, other: Value) -> Option<Value> {
        let ci = cand.index();
        if self.view.op(ci) != Op::Add {
            return None;
        }
        let mut o = self.view.operands(ci);
        let a = o.value();
        let b = o.value();
        if a == other && self.const_u64.get(b.index()).copied().flatten() == Some(1) {
            Some(other)
        } else if b == other && self.const_u64.get(a.index()).copied().flatten() == Some(1) {
            Some(other)
        } else {
            None
        }
    }

    fn is_neg_of(&self, cand: Value, other: Value) -> bool {
        let ci = cand.index();
        if self.view.op(ci) != Op::Neg {
            return false;
        }
        self.view.operands(ci).value() == other
    }

    fn is_not_of(&self, cand: Value, other: Value) -> bool {
        let ci = cand.index();
        if self.view.op(ci) != Op::Not {
            return false;
        }
        self.view.operands(ci).value() == other
    }

    /// Emits `Op::Br(cond, then_tgt, else_tgt)` with `Tbz`/`Tbnz`, `Cbz`/`Cbnz`, fused `Cmp+BCond`,
    /// and clean SSA `ParallelCopy` edge splitting.
    fn lower_br(
        &mut self,
        cond_val: Value,
        then_tgt: blaze_lir::op::Target<'_>,
        else_tgt: blaze_lir::op::Target<'_>,
        next_lir_blk: Option<usize>,
    ) {
        let then_orig = at(&self.mblock_of_lir, then_tgt.block.index(), MBlock(0));
        let else_orig = at(&self.mblock_of_lir, else_tgt.block.index(), MBlock(0));

        let mut then_pairs: Vec<(MReg, MReg), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut else_pairs: Vec<(MReg, MReg), Alloc<'a>> = Vec::new_in(self.alloc);
        self.build_edge_copy_pairs(then_tgt.block, then_tgt.args, &mut then_pairs);
        self.build_edge_copy_pairs(else_tgt.block, else_tgt.args, &mut else_pairs);

        // Decide whether an inline fallthrough split block should be placed right after `cur_blk`.
        let mut inline_split: Option<(MBlock, MBlock, Vec<(MReg, MReg), Alloc<'a>>)> = None;

        let then_eff = if then_pairs.is_empty() {
            then_orig
        } else {
            let split = self.new_mblock(false, self.cur_weight);
            if next_lir_blk == Some(then_tgt.block.index()) && inline_split.is_none() {
                inline_split = Some((split, then_orig, then_pairs));
            } else {
                let start = u32_of(self.deferred_pairs.len()).unwrap_or(u32::MAX);
                let len = u16::try_from(then_pairs.len()).unwrap_or(u16::MAX);
                for &p in then_pairs.iter().take(usize::from(len)) {
                    self.deferred_pairs.push(p);
                }
                self.deferred_splits.push(DeferredEdgeSplit {
                    split_blk: split,
                    target_blk: then_orig,
                    copy_start: start,
                    copy_len: len,
                });
            }
            split
        };

        let else_eff = if else_pairs.is_empty() {
            else_orig
        } else {
            let split = self.new_mblock(false, self.cur_weight);
            if next_lir_blk == Some(else_tgt.block.index()) && inline_split.is_none() {
                inline_split = Some((split, else_orig, else_pairs));
            } else {
                let start = u32_of(self.deferred_pairs.len()).unwrap_or(u32::MAX);
                let len = u16::try_from(else_pairs.len()).unwrap_or(u16::MAX);
                for &p in else_pairs.iter().take(usize::from(len)) {
                    self.deferred_pairs.push(p);
                }
                self.deferred_splits.push(DeferredEdgeSplit {
                    split_blk: split,
                    target_blk: else_orig,
                    copy_start: start,
                    copy_len: len,
                });
            }
            split
        };

        // Emit conditional branch to `then_eff` + unconditional `B { target: else_eff }`.
        // (`mopt_branch_opt` automatically inverts over trailing `B` and elides fallthrough `B`!)
        self.emit_br_terminator(cond_val, then_eff, else_eff);

        // If we chose an inline fallthrough split block, emit it right now so `cur_blk` falls
        // directly into the `ParallelCopy` and then straight into its target block!
        if let Some((split_blk, target_blk, pairs)) = inline_split {
            self.mfunc.end_block(self.cur_blk);
            self.mfunc.begin_block(split_blk);
            self.cur_blk = split_blk;
            self.live_cmp = None;
            if !pairs.is_empty() {
                self.mfunc.push_parallel_copy(&pairs, MInstFlags::NONE);
            }
            self.push_op(MOp::B { target: target_blk }, MInstFlags::TERMINATOR);
        }
    }

    fn emit_br_terminator(&mut self, cond_val: Value, then_blk: MBlock, else_blk: MBlock) {
        // Constant boolean condition -> unconditional branch!
        if let Some(c) = self.const_u64.get(cond_val.index()).copied().flatten() {
            let target = if (c & 1) != 0 { then_blk } else { else_blk };
            self.push_op(MOp::B { target }, MInstFlags::TERMINATOR);
            return;
        }

        // Reuse live flags immediately on exact match.
        if let Some(live) = self.live_cmp
            && live.lir_cmp == cond_val.0
        {
            let br = self.push_op(
                MOp::BCond { cond: live.cond, target: then_blk },
                MInstFlags::TERMINATOR,
            );
            self.mfunc.set_vflag_use(br, live.vflag);
            self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
            return;
        }

        let (eff_val, inverted) = peel_bool_cond(
            self.view,
            &self.const_u64,
            &self.inst_block,
            cond_val,
            self.cur_lir_blk,
        );
        let ci = eff_val.index();

        if let Some(c) = self.const_u64.get(ci).copied().flatten() {
            let taken = ((c & 1) != 0) ^ inverted;
            let target = if taken { then_blk } else { else_blk };
            self.push_op(MOp::B { target }, MInstFlags::TERMINATOR);
            return;
        }

        // Reuse live flags if already computed above (for peeled inner condition).
        if let Some(live) = self.live_cmp
            && live.lir_cmp == eff_val.0
        {
            let cond = if inverted { live.cond.invert() } else { live.cond };
            let br = self.push_op(
                MOp::BCond { cond, target: then_blk },
                MInstFlags::TERMINATOR,
            );
            self.mfunc.set_vflag_use(br, live.vflag);
            self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
            return;
        }

        let op = self.view.op(ci);
        let in_cur_block = self.inst_block.get(ci).copied() == Some(self.cur_lir_blk);
        if op == Op::Icmp {
            let mut o = self.view.operands(ci);
            let mut ic = o.int_cond();
            let mut a = o.value();
            let mut b = o.value();
            if inverted {
                ic = invert_int_cond(ic);
            }
            if let Some((vf, mc)) = self.try_reuse_live_icmp(ic, a, b) {
                let br = self.push_op(
                    MOp::BCond { cond: mc, target: then_blk },
                    MInstFlags::TERMINATOR,
                );
                self.mfunc.set_vflag_use(br, vf);
                self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
                return;
            }
            if in_cur_block {
                let a_c = self.const_u64.get(a.index()).copied().flatten();
                let b_c = self.const_u64.get(b.index()).copied().flatten();
                if a_c.is_some() && b_c.is_none() {
                    core::mem::swap(&mut a, &mut b);
                    ic = swap_int_cond(ic);
                }
                let rhs_zero = self.view.ty(a) != Ty::I128
                    && self.const_u64.get(b.index()).copied().flatten() == Some(0);
                if rhs_zero && matches!(ic, IntCond::Eq | IntCond::Ne) {
                    let op_bits = int_bits(self.view.ty(a));
                    let wide = op_bits == 64;

                    // Folded single-use `Icmp(Eq | Ne, And(x, mask_const), 0)` (`SelPattern::IcmpTstImm`):
                    // single-bit power-of-2 mask -> `Tbz`/`Tbnz`; general logical bitmask ->
                    // `TstImm` + `BCond` (Apple Silicon 1-uop macro-op fused, zero `and` + zero extra reg!).
                    if let Some(SelPattern::IcmpTstImm { x, mask_val }) =
                        self.pattern.get(ci).copied()
                    {
                        let x_val = Value(x);
                        let mask_c = self
                            .const_u64
                            .get(Value(mask_val).index())
                            .copied()
                            .flatten()
                            .unwrap_or(0);
                        let m = mask_c & bit_mask(op_bits);
                        let xr = self.get_val(x_val, false);
                        if m != 0 && (m & (m - 1)) == 0 {
                            let bit = u8_of(m.trailing_zeros()).unwrap_or(0);
                            if u32::from(bit) < op_bits && bit <= 63 {
                                let br_op = if ic == IntCond::Eq {
                                    MOp::Tbz { rt: xr, bit, target: then_blk }
                                } else {
                                    MOp::Tbnz { rt: xr, bit, target: then_blk }
                                };
                                self.push_op(br_op, MInstFlags::TERMINATOR);
                                self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
                                return;
                            }
                        }
                        let (_, vf) = self.push_flag_writer(
                            MOp::TstImm { rn: xr, imm: m, wide },
                            MInstFlags::NONE,
                        );
                        let br = self.push_op(
                            MOp::BCond { cond: int_cond_to_mcond(ic), target: then_blk },
                            MInstFlags::TERMINATOR,
                        );
                        self.mfunc.set_vflag_use(br, vf);
                        self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
                        return;
                    }

                    // Check `Tbz` / `Tbnz` for non-skipped multi-use `(x & (1 << bit)) == 0` / `!= 0`!
                    if self.view.op(a.index()) == Op::And
                        && !self.skip_inst.get(a.index()).copied().unwrap_or(false)
                        && self.pattern.get(a.index()).copied() == Some(SelPattern::None)
                    {
                        let mut ao = self.view.operands(a.index());
                        let mut x = ao.value();
                        let mut y = ao.value();
                        if self.const_u64.get(x.index()).copied().flatten().is_some()
                            && self.const_u64.get(y.index()).copied().flatten().is_none()
                        {
                            core::mem::swap(&mut x, &mut y);
                        }
                        if !self.skip_inst.get(x.index()).copied().unwrap_or(false)
                            && let Some(mask_c) = self.const_u64.get(y.index()).copied().flatten()
                        {
                            let m = mask_c & bit_mask(op_bits);
                            if m != 0 && (m & (m - 1)) == 0 {
                                let bit = u8_of(m.trailing_zeros()).unwrap_or(0);
                                if u32::from(bit) < op_bits && bit <= 63 {
                                    let xr = self.get_val(x, false);
                                    let br_op = if ic == IntCond::Eq {
                                        MOp::Tbz { rt: xr, bit, target: then_blk }
                                    } else {
                                        MOp::Tbnz { rt: xr, bit, target: then_blk }
                                    };
                                    self.push_op(br_op, MInstFlags::TERMINATOR);
                                    self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
                                    return;
                                }
                            }
                        }
                    }

                    // `Cbz` / `Cbnz` for `a == 0` / `a != 0`!
                    let ar = self.get_val(a, false);
                    let br_op = if ic == IntCond::Eq {
                        MOp::Cbz { rt: ar, target: then_blk, wide }
                    } else {
                        MOp::Cbnz { rt: ar, target: then_blk, wide }
                    };
                    self.push_op(br_op, MInstFlags::TERMINATOR);
                    self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
                    return;
                }

                let (vf, mc, _int_cmp) = self.emit_icmp_flags(ci, ic, a, b);
                let br = self.push_op(
                    MOp::BCond { cond: mc, target: then_blk },
                    MInstFlags::TERMINATOR,
                );
                self.mfunc.set_vflag_use(br, vf);
                self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
                return;
            }
        }

        if op == Op::Fcmp && in_cur_block {
            let mut o = self.view.operands(ci);
            let fc = o.float_cond();
            let a = o.value();
            let b = o.value();
            match fc {
                FloatCond::False => {
                    let target = if inverted { then_blk } else { else_blk };
                    self.push_op(MOp::B { target }, MInstFlags::TERMINATOR);
                    return;
                }
                FloatCond::True => {
                    let target = if inverted { else_blk } else { then_blk };
                    self.push_op(MOp::B { target }, MInstFlags::TERMINATOR);
                    return;
                }
                _ => {
                    if let Some((vf, mc)) = self.emit_fcmp_flags(fc, a, b) {
                        let cond = if inverted { mc.invert() } else { mc };
                        let br = self.push_op(
                            MOp::BCond { cond, target: then_blk },
                            MInstFlags::TERMINATOR,
                        );
                        self.mfunc.set_vflag_use(br, vf);
                        self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
                        return;
                    }
                }
            }
        }

        // Plain materialized `I1`: single `Cbz` / `Cbnz`!
        let cond_reg = self.get_val(eff_val, false);
        let br_op = if inverted {
            MOp::Cbz { rt: cond_reg, target: then_blk, wide: false }
        } else {
            MOp::Cbnz { rt: cond_reg, target: then_blk, wide: false }
        };
        self.push_op(br_op, MInstFlags::TERMINATOR);
        self.push_op(MOp::B { target: else_blk }, MInstFlags::TERMINATOR);
    }

    /// Lowers a single LIR instruction `inst` inside LIR block `lir_b`.
    fn lower_inst(
        &mut self,
        inst: usize,
        next_lir_blk: Option<usize>,
        scratch_pairs: &mut Vec<(MReg, MReg), Alloc<'a>>,
    ) -> Result<(), TierError> {
        if at(&self.skip_inst, inst, false) {
            return Ok(());
        }

        let op = self.view.op(inst);
        let res_ty = ty_at(self.view, inst);
        let flags = self.view.flags(inst);
        let pat = at(&self.pattern, inst, SelPattern::None);
        let mut o = self.view.operands(inst);

        // Native 128-bit NEON SIMD ALU (`v16i8`/`v8i16`/`v4i32`/`v2i64`/`v4f32`/`v2f64`).
        if is_vec128(res_ty)
            && let Some((vec_op, elem_size)) = vec128_alu_op(op, res_ty)
        {
            let dst = self.alloc_res_vreg(inst, res_ty);
            if op == Op::Not {
                let a = o.value();
                let rn = self.get_val(a, false);
                self.push_op(
                    MOp::VecAlu { op: vec_op, dst: MReg::V(dst), rn, rm: None, elem_size },
                    MInstFlags::NONE,
                );
            } else {
                let a = o.value();
                let b = o.value();
                let rn = self.get_val(a, false);
                let rm = self.get_val(b, false);
                self.push_op(
                    MOp::VecAlu { op: vec_op, dst: MReg::V(dst), rn, rm: Some(rm), elem_size },
                    MInstFlags::NONE,
                );
            }
            return Ok(());
        }

        // Native 128-bit scalar-pair integer ops (`Ty::I128`).
        if res_ty == Ty::I128 {
            match op {
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Shl
                | Op::Lshr
                | Op::Ashr => {
                    let a = o.value();
                    let b = o.value();
                    self.lower_i128_alu(inst, op, a, Some(b));
                    return Ok(());
                }
                Op::Neg | Op::Not => {
                    let a = o.value();
                    self.lower_i128_alu(inst, op, a, None);
                    return Ok(());
                }
                Op::Freeze => {
                    let a = o.value();
                    if self.i128_has_q128(a) {
                        let ar = self.get_val(a, false);
                        self.bind_alias_vreg(inst, ar, res_ty);
                    } else {
                        let (lo, hi) = self.get_i128_pair(a, true);
                        self.bind_i128_pair(inst, lo, hi);
                    }
                    return Ok(());
                }
                Op::Zext => {
                    let a = o.value();
                    let ar = self.get_val(a, true);
                    let hi = MReg::V(self.materialize_int_const(0, true));
                    self.bind_i128_pair(inst, ar, hi);
                    return Ok(());
                }
                Op::Sext => {
                    let a = o.value();
                    let from_bits = int_bits(self.view.ty(a));
                    let ar = self.get_val(a, false);
                    let lo_reg = if from_bits == 64 {
                        ar
                    } else {
                        let v = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                        let imms = u8_of(from_bits - 1).unwrap_or(7);
                        self.push_op(
                            MOp::Sbfm { dst: MReg::V(v), rn: ar, immr: 0, imms, wide: true },
                            MInstFlags::NONE,
                        );
                        MReg::V(v)
                    };
                    let hi_v = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::Sbfm { dst: MReg::V(hi_v), rn: lo_reg, immr: 63, imms: 63, wide: true },
                        MInstFlags::NONE,
                    );
                    self.bind_i128_pair(inst, lo_reg, MReg::V(hi_v));
                    return Ok(());
                }
                _ => {}
            }
        }

        match op {
            Op::Param
            | Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Null
            | Op::Uninit
            | Op::Assume
            | Op::SlotLive
            | Op::SlotDead => {}

            Op::Freeze | Op::PtrAddr | Op::Expose | Op::FromExposed => {
                let a = o.value();
                let ar = self.get_val(a, false);
                self.bind_alias_vreg(inst, ar, res_ty);
            }

            Op::PtrWithAddr => {
                let _p = o.value();
                let addr = o.value();
                let ar = self.get_val(addr, false);
                self.bind_alias_vreg(inst, ar, res_ty);
            }

            Op::SlotAddr => {
                let slot = o.slot();
                let off = at(&self.slot_off, slot.index(), 0);
                let dst = self.alloc_res_vreg(inst, Ty::PTR);
                self.emit_sp_plus_off(MReg::V(dst), off);
            }

            Op::PtrAdd => {
                let p = o.value();
                let by = o.value();
                let base = self.get_val(p, false);
                let by_bits = int_bits(self.view.ty(by));
                if let Some(c) = self.const_u64.get(by.index()).copied().flatten() {
                    let signed_off = sign_extend_u64(c, by_bits);
                    if signed_off == 0 {
                        self.bind_alias_vreg(inst, base, Ty::PTR);
                        return Ok(());
                    }
                    let dst = self.alloc_res_vreg(inst, Ty::PTR);
                    self.emit_ptr_add_const(MReg::V(dst), base, signed_off);
                } else if let SelPattern::ShiftedBin { lhs, rhs, shift } = pat {
                    let lr = self.get_val(Value(lhs), false);
                    let rr = self.get_val(Value(rhs), false);
                    let dst = self.alloc_res_vreg(inst, Ty::PTR);
                    self.push_op(
                        MOp::AddReg {
                            dst: MReg::V(dst),
                            rn: lr,
                            rm: rr,
                            shift: Some(shift),
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                } else if by_bits == 64 {
                    let by_r = self.get_val(by, true);
                    let dst = self.alloc_res_vreg(inst, Ty::PTR);
                    self.push_op(
                        MOp::AddReg {
                            dst: MReg::V(dst),
                            rn: base,
                            rm: by_r,
                            shift: None,
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                } else if by_bits == 32 {
                    let by_r = self.get_val(by, true);
                    let dst = self.alloc_res_vreg(inst, Ty::PTR);
                    self.push_op(
                        MOp::AddExt {
                            dst: MReg::V(dst),
                            rn: base,
                            rm: by_r,
                            ext: MExt::Sxtw,
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                } else {
                    let by_raw = self.get_val(by, false);
                    let by_s = self.sign_extend_operand(by_raw, by_bits, true);
                    let dst = self.alloc_res_vreg(inst, Ty::PTR);
                    self.push_op(
                        MOp::AddReg {
                            dst: MReg::V(dst),
                            rn: base,
                            rm: by_s,
                            shift: None,
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                }
            }

            Op::Add => self.lower_add(inst, o.value(), o.value(), res_ty, pat),
            Op::Sub => self.lower_sub(inst, o.value(), o.value(), res_ty, pat),

            Op::Mul => {
                let a = o.value();
                let b = o.value();
                let bits = int_bits(res_ty);
                let wide = bits == 64;
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let raw = if bits < 32 {
                    self.mfunc.alloc_vreg(VRegClass::Gpr32)
                } else {
                    self.alloc_res_vreg(inst, res_ty)
                };
                self.push_op(MOp::Mul { dst: MReg::V(raw), rn: ar, rm: br, wide }, MInstFlags::NONE);
                if bits < 32 {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.emit_narrow_mask(MReg::V(dst), MReg::V(raw), bits);
                }
            }

            Op::Umulhi | Op::Smulhi => {
                let a = o.value();
                let b = o.value();
                let bits = int_bits(res_ty);
                let signed = op == Op::Smulhi;
                if bits == 64 {
                    let ar = self.get_val(a, true);
                    let br = self.get_val(b, true);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    let mop = if signed {
                        MOp::Smulh { dst: MReg::V(dst), rn: ar, rm: br }
                    } else {
                        MOp::Umulh { dst: MReg::V(dst), rn: ar, rm: br }
                    };
                    self.push_op(mop, MInstFlags::NONE);
                } else if bits == 1 {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::MovImm { dst: MReg::V(dst), imm: 0, wide: false },
                        MInstFlags::REMATERIALIZABLE,
                    );
                } else {
                    let ar_raw = self.get_val(a, false);
                    let br_raw = self.get_val(b, false);
                    let (ar, br) = if signed {
                        (
                            self.sign_extend_operand(ar_raw, bits, true),
                            self.sign_extend_operand(br_raw, bits, true),
                        )
                    } else {
                        (ar_raw, br_raw)
                    };
                    let prod = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                    self.push_op(
                        MOp::Mul { dst: MReg::V(prod), rn: ar, rm: br, wide: true },
                        MInstFlags::NONE,
                    );
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    let immr = u8_of(bits).unwrap_or(32);
                    let imms = u8_of(bits * 2 - 1).unwrap_or(63);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(dst), rn: MReg::V(prod), immr, imms, wide: true },
                        MInstFlags::NONE,
                    );
                }
            }

            Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => {
                self.lower_div_rem(inst, op, flags, o.value(), o.value(), res_ty);
            }

            Op::Neg => {
                let a = o.value();
                let bits = int_bits(res_ty);
                let wide = bits == 64;
                let ar = self.get_val(a, true);
                let raw = if bits < 32 {
                    self.mfunc.alloc_vreg(VRegClass::Gpr32)
                } else {
                    self.alloc_res_vreg(inst, res_ty)
                };
                self.push_op(
                    MOp::SubReg {
                        dst: MReg::V(raw),
                        rn: MReg::XZR,
                        rm: ar,
                        shift: None,
                        wide,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                if bits < 32 {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.emit_narrow_mask(MReg::V(dst), MReg::V(raw), bits);
                }
            }

            Op::And | Op::Or | Op::Xor => {
                self.lower_logical(inst, op, o.value(), o.value(), res_ty, pat);
            }

            Op::Not => {
                let a = o.value();
                let bits = int_bits(res_ty);
                let wide = bits == 64;
                let ar = self.get_val(a, true);
                if bits == 1 {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::LogicalImm {
                            dst: MReg::V(dst),
                            rn: ar,
                            imm: 1,
                            kind: LogicalKind::Eor,
                            wide: false,
                        },
                        MInstFlags::NONE,
                    );
                } else {
                    let raw = if bits < 32 {
                        self.mfunc.alloc_vreg(VRegClass::Gpr32)
                    } else {
                        self.alloc_res_vreg(inst, res_ty)
                    };
                    self.push_op(
                        MOp::Mvn { dst: MReg::V(raw), rm: ar, shift: None, wide },
                        MInstFlags::NONE,
                    );
                    if bits < 32 {
                        let dst = self.alloc_res_vreg(inst, res_ty);
                        self.emit_narrow_mask(MReg::V(dst), MReg::V(raw), bits);
                    }
                }
            }

            Op::Shl | Op::Lshr | Op::Ashr => {
                self.lower_shift(inst, op, o.value(), o.value(), res_ty);
            }

            Op::Rotl | Op::Rotr => {
                self.lower_rotate(inst, op, o.value(), o.value(), res_ty);
            }

            Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => {
                self.lower_bitop(inst, op, o.value(), res_ty);
            }

            Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                self.lower_minmax(inst, op, o.value(), o.value(), res_ty);
            }

            Op::UaddSat | Op::UsubSat | Op::SaddSat | Op::SsubSat => {
                self.lower_sat_arith(inst, op, o.value(), o.value(), res_ty);
            }

            Op::UaddOverflow
            | Op::UsubOverflow
            | Op::SaddOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow => {
                self.lower_overflow(inst, op, o.value(), o.value());
            }

            Op::Project => {
                let src = o.value();
                let field = o.imm();
                let src_op = self.view.op(src.index());
                let aliased = if matches!(
                    src_op,
                    Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect
                ) {
                    self.get_or_alloc_call_ret_vreg(src.index(), to_idx(field)).unwrap_or(NONE_U32)
                } else {
                    let (v0, v1) = self.get_or_alloc_ovf_pair(src.index());
                    if field == 0 { v0 } else { v1 }
                };
                if aliased != NONE_U32 {
                    self.bind_alias_vreg(inst, MReg::V(VReg(aliased)), res_ty);
                } else {
                    return Err(refuse(Some(inst), "project of unsupported source"));
                }
            }

            Op::DataAddr => {
                if at(&self.use_count, inst, 0) == 0 {
                    return Ok(());
                }
                let ref_id = o.reference();
                let ref_rec = self.view.reference(ref_id);
                let dst = self.alloc_res_vreg(inst, Ty::PTR);
                match RefKind::from_word(ref_rec.kind) {
                    Some(RefKind::Static) => {
                        let target = self.targets.intern_outside(
                            Outside::Static(ref_rec.place),
                            Space::Static,
                            true,
                            self.names,
                            inst,
                        )?;
                        self.emit_got_load_addr(dst, target);
                    }
                    Some(RefKind::Vtable) => {
                        let target = self.targets.intern_outside(
                            Outside::Vtable(ref_rec.place),
                            Space::Vtable,
                            false,
                            self.names,
                            inst,
                        )?;
                        self.emit_got_load_addr(dst, target);
                    }
                    Some(RefKind::Data) => {
                        let off = self.place_data_row(ref_rec.place, inst)?;
                        self.emit_relro_addr(dst, off);
                    }
                    _ => return Err(refuse(Some(inst), "unsupported data_addr kind")),
                }
            }

            Op::FuncAddr => {
                if at(&self.use_count, inst, 0) == 0 {
                    return Ok(());
                }
                let ref_id = o.reference();
                let ref_rec = self.view.reference(ref_id);
                let target = self.targets.intern_outside_func(ref_rec.place, self.names)?;
                let dst = self.alloc_res_vreg(inst, Ty::PTR);
                self.emit_got_load_addr(dst, target);
            }

            Op::TargetIntrinsic => {
                let intr_id = o.intrinsic();
                let args = o.values();
                if self.view.intrinsic_name(intr_id) == b"blaze.prefetch.r1" {
                    if let Some(ptr_val) = args.iter().next() {
                        let ptr_reg = self.get_val(ptr_val, false);
                        self.push_op(
                            MOp::Prfm { base: ptr_reg, imm12: 0 },
                            MInstFlags::MEM_READ,
                        );
                    }
                } else {
                    return Err(refuse(Some(inst), "unsupported target_intrinsic"));
                }
            }

            Op::Icmp => {
                if at(&self.cmp_all_fused, inst, false) {
                    return Ok(());
                }
                let ic = o.int_cond();
                let a = o.value();
                let b = o.value();
                let (vf, mc) = if let Some((lvf, lmc)) = self.try_reuse_live_icmp(ic, a, b) {
                    (lvf, lmc)
                } else {
                    let (vf, mc, int_cmp) = self.emit_icmp_flags(inst, ic, a, b);
                    // Keep flags live across non-flag-clobbering ops for sibling `Select`/`Br`/`Icmp`!
                    self.live_cmp = Some(LiveCmp {
                        lir_cmp: u32_of(inst).unwrap_or(NONE_U32),
                        vflag: vf,
                        cond: mc,
                        int_cmp,
                    });
                    (vf, mc)
                };
                let dst = self.alloc_res_vreg(inst, Ty::I1);
                let mi = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(dst),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: mc.invert(),
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(mi, vf);
            }

            Op::Fcmp => {
                if at(&self.cmp_all_fused, inst, false) {
                    return Ok(());
                }
                let fc = o.float_cond();
                let a = o.value();
                let b = o.value();
                match fc {
                    FloatCond::False => {
                        let dst = self.alloc_res_vreg(inst, Ty::I1);
                        self.push_op(
                            MOp::MovImm { dst: MReg::V(dst), imm: 0, wide: false },
                            MInstFlags::REMATERIALIZABLE,
                        );
                    }
                    FloatCond::True => {
                        let dst = self.alloc_res_vreg(inst, Ty::I1);
                        self.push_op(
                            MOp::MovImm { dst: MReg::V(dst), imm: 1, wide: false },
                            MInstFlags::REMATERIALIZABLE,
                        );
                    }
                    FloatCond::One | FloatCond::Ueq => {
                        let double = self.view.ty(a) == Ty::F64;
                        let ar = self.get_val(a, false);
                        let br = self.get_val(b, false);
                        let (_, vf) = self.push_flag_writer(
                            MOp::FpCmp { rn: ar, rm: br, double },
                            MInstFlags::NONE,
                        );
                        let (c1, c2) = if fc == FloatCond::One {
                            (MCond::Mi, MCond::Gt)
                        } else {
                            (MCond::Eq, MCond::Vs)
                        };
                        let t1 = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                        let i1 = self.push_op(
                            MOp::Csinc {
                                dst: MReg::V(t1),
                                rn: MReg::XZR,
                                rm: MReg::XZR,
                                cond: c1.invert(),
                                wide: false,
                            },
                            MInstFlags::NONE,
                        );
                        self.mfunc.set_vflag_use(i1, vf);
                        let t2 = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                        let i2 = self.push_op(
                            MOp::Csinc {
                                dst: MReg::V(t2),
                                rn: MReg::XZR,
                                rm: MReg::XZR,
                                cond: c2.invert(),
                                wide: false,
                            },
                            MInstFlags::NONE,
                        );
                        self.mfunc.set_vflag_use(i2, vf);
                        let dst = self.alloc_res_vreg(inst, Ty::I1);
                        self.push_op(
                            MOp::LogicalReg {
                                dst: MReg::V(dst),
                                rn: MReg::V(t1),
                                rm: MReg::V(t2),
                                shift: None,
                                invert_rm: false,
                                kind: LogicalKind::Orr,
                                wide: false,
                            },
                            MInstFlags::NONE,
                        );
                    }
                    _ => {
                        if let Some((vf, mc)) = self.emit_fcmp_flags(fc, a, b) {
                            let dst = self.alloc_res_vreg(inst, Ty::I1);
                            let mi = self.push_op(
                                MOp::Csinc {
                                    dst: MReg::V(dst),
                                    rn: MReg::XZR,
                                    rm: MReg::XZR,
                                    cond: mc.invert(),
                                    wide: false,
                                },
                                MInstFlags::NONE,
                            );
                            self.mfunc.set_vflag_use(mi, vf);
                            self.live_cmp = Some(LiveCmp {
                                lir_cmp: u32_of(inst).unwrap_or(NONE_U32),
                                vflag: vf,
                                cond: mc,
                                int_cmp: None,
                            });
                        }
                    }
                }
            }

            Op::Select => {
                let c = o.value();
                let t = o.value();
                let f = o.value();
                self.lower_select(inst, c, t, f);
            }

            Op::Zext => {
                // Our SSA invariant guarantees bits `[63:w]` are ALWAYS zero for every narrow integer!
                let a = o.value();
                let ar = self.get_val(a, false);
                self.bind_alias_vreg(inst, ar, res_ty);
            }

            Op::Sext => {
                let a = o.value();
                let from_bits = int_bits(self.view.ty(a));
                let to_bits = int_bits(res_ty);
                let ar = self.get_val(a, false);
                let wide = to_bits == 64;
                let imms = u8_of(from_bits - 1).unwrap_or(7);
                if to_bits < 32 {
                    let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                    self.push_op(
                        MOp::Sbfm { dst: MReg::V(tmp), rn: ar, immr: 0, imms, wide: false },
                        MInstFlags::NONE,
                    );
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.emit_narrow_mask(MReg::V(dst), MReg::V(tmp), to_bits);
                } else {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::Sbfm { dst: MReg::V(dst), rn: ar, immr: 0, imms, wide },
                        MInstFlags::NONE,
                    );
                }
            }

            Op::Trunc => {
                let a = o.value();
                let to_bits = int_bits(res_ty);
                if self.view.ty(a) == Ty::I128 {
                    let (lo, _) = self.get_i128_pair(a, true);
                    if to_bits == 64 {
                        if let MReg::V(v) = lo {
                            self.bind_alias_vreg(inst, MReg::V(v), res_ty);
                        } else {
                            let dst = self.alloc_res_vreg(inst, res_ty);
                            self.push_op(
                                MOp::Mov { dst: MReg::V(dst), src: lo, wide: true },
                                MInstFlags::NONE,
                            );
                        }
                    } else if to_bits == 32 {
                        let dst = self.alloc_res_vreg(inst, res_ty);
                        self.push_op(
                            MOp::Mov { dst: MReg::V(dst), src: lo, wide: false },
                            MInstFlags::NONE,
                        );
                    } else {
                        let dst = self.alloc_res_vreg(inst, res_ty);
                        self.emit_narrow_mask(MReg::V(dst), lo, to_bits);
                    }
                    return Ok(());
                }
                if self.value_fits_zero_extended_bits(a, to_bits) {
                    let ar = self.get_val(a, false);
                    self.bind_alias_vreg(inst, ar, res_ty);
                    return Ok(());
                }
                let ar = self.get_val(a, true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                if to_bits == 32 {
                    // 32-bit `mov wd, wn` clears upper 32 bits!
                    self.push_op(
                        MOp::Mov { dst: MReg::V(dst), src: ar, wide: false },
                        MInstFlags::NONE,
                    );
                } else {
                    self.emit_narrow_mask(MReg::V(dst), ar, to_bits);
                }
            }

            Op::Bitcast => {
                let a = o.value();
                let src_ty = self.view.ty(a);
                if src_ty == Ty::I128 && !self.i128_has_q128(a) {
                    if res_ty == Ty::I128 {
                        let (lo, hi) = self.get_i128_pair(a, true);
                        self.bind_i128_pair(inst, lo, hi);
                        return Ok(());
                    }
                    return Err(refuse(Some(inst), "bitcast from scalar-pair i128 to vector"));
                }
                let ar = self.get_val(a, false);
                if is_scalar_int_or_ptr(src_ty) && is_scalar_fp(res_ty) {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::FpAlu {
                            op: FpAluOp::MovFromGpr,
                            dst: MReg::V(dst),
                            rn: ar,
                            rm: None,
                            ra: None,
                            double: res_ty == Ty::F64,
                        },
                        MInstFlags::NONE,
                    );
                } else if is_scalar_fp(src_ty) && is_scalar_int_or_ptr(res_ty) {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::FpAlu {
                            op: FpAluOp::MovToGpr,
                            dst: MReg::V(dst),
                            rn: ar,
                            rm: None,
                            ra: None,
                            double: src_ty == Ty::F64,
                        },
                        MInstFlags::NONE,
                    );
                } else {
                    self.bind_alias_vreg(inst, ar, res_ty);
                }
            }

            Op::Splat => {
                let a = o.value();
                let Some(kind) = splat_kind_for_ty(res_ty) else {
                    return Err(refuse(Some(inst), "splat on unsupported vector type"));
                };
                match kind {
                    SplatKind::Int { elem_size } => {
                        let dst = self.alloc_res_vreg(inst, res_ty);
                        if self.const_u64.get(a.index()).copied().flatten() == Some(0) {
                            self.push_op(
                                MOp::VecAlu {
                                    op: VecAluOp::Zero,
                                    dst: MReg::V(dst),
                                    rn: MReg::vec(0),
                                    rm: None,
                                    elem_size: 0,
                                },
                                MInstFlags::REMATERIALIZABLE,
                            );
                        } else {
                            let sr = self.get_val(a, true);
                            self.push_op(
                                MOp::VecAlu {
                                    op: VecAluOp::DupGen,
                                    dst: MReg::V(dst),
                                    rn: sr,
                                    rm: None,
                                    elem_size,
                                },
                                MInstFlags::NONE,
                            );
                        }
                    }
                    SplatKind::Float { elem_size } => {
                        let dst = self.alloc_res_vreg(inst, res_ty);
                        if self.const_fp_bits.get(a.index()).copied().flatten() == Some(0) {
                            self.push_op(
                                MOp::VecAlu {
                                    op: VecAluOp::Zero,
                                    dst: MReg::V(dst),
                                    rn: MReg::vec(0),
                                    rm: None,
                                    elem_size: 0,
                                },
                                MInstFlags::REMATERIALIZABLE,
                            );
                        } else {
                            let sr = self.get_val(a, false);
                            self.push_op(
                                MOp::VecAlu {
                                    op: VecAluOp::DupElem0,
                                    dst: MReg::V(dst),
                                    rn: sr,
                                    rm: None,
                                    elem_size,
                                },
                                MInstFlags::NONE,
                            );
                        }
                    }
                    SplatKind::Identity128 => {
                        if self.view.ty(a) == Ty::I128 && !self.i128_has_q128(a) {
                            return Err(refuse(Some(inst), "splat from scalar-pair i128"));
                        }
                        let sr = self.get_val(a, false);
                        self.bind_alias_vreg(inst, sr, res_ty);
                    }
                }
            }

            Op::Load => {
                let ptr = o.value();
                let off = o.offset();
                let size = mem_size_for_ty(res_ty).unwrap_or(MemSize::X64);
                let addr = self.select_mem_addr(ptr, off, size, pat);
                let is_vol = flags.contains(Flags::VOLATILE);
                let mflags = if is_vol {
                    MInstFlags::MEM_READ.union(MInstFlags::VOLATILE)
                } else {
                    MInstFlags::MEM_READ
                };
                if res_ty == Ty::I1 {
                    let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                    let mi = self.push_op(
                        MOp::Ldr { dst: MReg::V(raw), addr, size: MemSize::B8, signed: false },
                        mflags,
                    );
                    if is_vol {
                        self.mfunc.attach_note(mi, MNoteKind::Volatile);
                    }
                    let dst = self.alloc_res_vreg(inst, Ty::I1);
                    self.push_op(
                        MOp::LogicalImm {
                            dst: MReg::V(dst),
                            rn: MReg::V(raw),
                            imm: 1,
                            kind: LogicalKind::And,
                            wide: false,
                        },
                        MInstFlags::NONE,
                    );
                } else {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    let mi = self.push_op(
                        MOp::Ldr { dst: MReg::V(dst), addr, size, signed: false },
                        mflags,
                    );
                    if is_vol {
                        self.mfunc.attach_note(mi, MNoteKind::Volatile);
                    }
                }
            }

            Op::Store => {
                let val = o.value();
                let ptr = o.value();
                let off = o.offset();
                let val_ty = self.view.ty(val);
                if val_ty == Ty::I128 && !self.i128_has_q128(val) {
                    self.lower_i128_store(val, ptr, off, flags, pat);
                    return Ok(());
                }
                let size = mem_size_for_ty(val_ty).unwrap_or(MemSize::X64);
                let src = self.get_val(val, !size.is_fp_or_vec());
                let addr = self.select_mem_addr(ptr, off, size, pat);
                let is_vol = flags.contains(Flags::VOLATILE);
                let mflags = if is_vol {
                    MInstFlags::MEM_WRITE.union(MInstFlags::VOLATILE)
                } else {
                    MInstFlags::MEM_WRITE
                };
                let mi = self.push_op(MOp::Str { src, addr, size }, mflags);
                if is_vol {
                    self.mfunc.attach_note(mi, MNoteKind::Volatile);
                }
            }

            Op::Memcpy | Op::Memmove | Op::Memset => {
                self.lower_bulk(op, flags, o.value(), o.value(), o.value())?;
            }

            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv => {
                let a = o.value();
                let b = o.value();
                let double = res_ty == Ty::F64;
                let ar = self.get_val(a, false);
                let br = self.get_val(b, false);
                let fp_op = match op {
                    Op::Fadd => FpAluOp::Add,
                    Op::Fsub => FpAluOp::Sub,
                    Op::Fmul => FpAluOp::Mul,
                    _ => FpAluOp::Div,
                };
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::FpAlu { op: fp_op, dst: MReg::V(dst), rn: ar, rm: Some(br), ra: None, double },
                    MInstFlags::NONE,
                );
            }

            Op::Fneg | Op::Fabs => {
                let a = o.value();
                let double = res_ty == Ty::F64;
                let ar = self.get_val(a, false);
                let fp_op = if op == Op::Fneg { FpAluOp::Neg } else { FpAluOp::Abs };
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::FpAlu { op: fp_op, dst: MReg::V(dst), rn: ar, rm: None, ra: None, double },
                    MInstFlags::NONE,
                );
            }

            Op::Fma | Op::Fmuladd => {
                let a = o.value();
                let b = o.value();
                let c = o.value();
                let double = res_ty == Ty::F64;
                let ar = self.get_val(a, false);
                let br = self.get_val(b, false);
                let cr = self.get_val(c, false);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::Madd,
                        dst: MReg::V(dst),
                        rn: ar,
                        rm: Some(br),
                        ra: Some(cr),
                        double,
                    },
                    MInstFlags::NONE,
                );
            }

            Op::Fpext => {
                let a = o.value();
                let ar = self.get_val(a, false);
                let dst = self.alloc_res_vreg(inst, Ty::F64);
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::CvtS2D,
                        dst: MReg::V(dst),
                        rn: ar,
                        rm: None,
                        ra: None,
                        double: true,
                    },
                    MInstFlags::NONE,
                );
            }

            Op::Fptrunc => {
                let a = o.value();
                let ar = self.get_val(a, false);
                let dst = self.alloc_res_vreg(inst, Ty::F32);
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::CvtD2S,
                        dst: MReg::V(dst),
                        rn: ar,
                        rm: None,
                        ra: None,
                        double: false,
                    },
                    MInstFlags::NONE,
                );
            }

            Op::UitoFp | Op::SitoFp => {
                let a = o.value();
                let src_bits = int_bits(self.view.ty(a));
                let double = res_ty == Ty::F64;
                let int_wide = src_bits == 64;
                let ar_raw = self.get_val(a, true);
                let ar = if op == Op::SitoFp && src_bits < 32 {
                    self.sign_extend_operand(ar_raw, src_bits, false)
                } else {
                    ar_raw
                };
                let fp_op = if op == Op::SitoFp {
                    FpAluOp::Scvtf { int_wide }
                } else {
                    FpAluOp::Ucvtf { int_wide }
                };
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::FpAlu { op: fp_op, dst: MReg::V(dst), rn: ar, rm: None, ra: None, double },
                    MInstFlags::NONE,
                );
            }

            Op::FptoUi | Op::FptoSi => {
                self.lower_fp_to_int(inst, op, o.value(), res_ty);
            }

            Op::Call => {
                let callee_ref = o.reference();
                let sig_id = o.sig();
                let args = o.values();
                self.lower_call(
                    inst,
                    CallCallee::Direct(callee_ref),
                    sig_id,
                    args,
                    res_ty,
                    None,
                    scratch_pairs,
                )?;
            }

            Op::CallIndirect => {
                let callee = o.value();
                let sig_id = o.sig();
                let args = o.values();
                self.lower_call(
                    inst,
                    CallCallee::Indirect(callee),
                    sig_id,
                    args,
                    res_ty,
                    None,
                    scratch_pairs,
                )?;
            }

            Op::Invoke => {
                let callee_ref = o.reference();
                let sig_id = o.sig();
                let normal_blk = o.block();
                let unwind_blk = o.block();
                let args = o.values();
                self.lower_call(
                    inst,
                    CallCallee::Direct(callee_ref),
                    sig_id,
                    args,
                    res_ty,
                    Some((normal_blk, unwind_blk)),
                    scratch_pairs,
                )?;
            }

            Op::InvokeIndirect => {
                let callee = o.value();
                let sig_id = o.sig();
                let normal_blk = o.block();
                let unwind_blk = o.block();
                let args = o.values();
                self.lower_call(
                    inst,
                    CallCallee::Indirect(callee),
                    sig_id,
                    args,
                    res_ty,
                    Some((normal_blk, unwind_blk)),
                    scratch_pairs,
                )?;
            }

            Op::Resume => {
                let exc = o.value();
                let exc_r = self.get_val(exc, true);
                self.mfunc.push_parallel_copy(&[(MReg::gpr(0), exc_r)], MInstFlags::NONE);
                let target_id = self.targets.intern_c_name("_Unwind_Resume")?;
                let call_mi = self.emit_direct_call_site(target_id, CallClobbers::C_ABI_ARM64);
                // `_Unwind_Resume` ALWAYS gets a `Continue` row (libunwind phase 2 queries this site!).
                self.attach_unwind_row(call_mi, None)?;
                let imm = u16_of(BRK_UNREACHABLE).unwrap_or(0xff02);
                self.push_op(
                    MOp::Brk { imm },
                    MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP).union(MInstFlags::COLD),
                );
            }

            Op::Jump => {
                let tgt = o.target();
                let dst_mblk = at(&self.mblock_of_lir, tgt.block.index(), MBlock(0));
                self.build_edge_copy_pairs(tgt.block, tgt.args, scratch_pairs);
                if !scratch_pairs.is_empty() {
                    self.mfunc.push_parallel_copy(scratch_pairs, MInstFlags::NONE);
                }
                self.push_op(MOp::B { target: dst_mblk }, MInstFlags::TERMINATOR);
            }

            Op::Br => {
                let cond_val = o.value();
                let then_tgt = o.target();
                let else_tgt = o.target();
                self.lower_br(cond_val, then_tgt, else_tgt, next_lir_blk);
            }

            Op::Switch => {
                let scrut = o.value();
                let default_tgt = o.target();
                let cases = o.cases(scrut);
                self.lower_switch(scrut, default_tgt, cases, scratch_pairs);
            }

            Op::Return => {
                let vals = o.values();
                scratch_pairs.clear();
                let mut gpr_r: u8 = 0;
                let mut fp_r: u8 = 0;
                for rv in vals.iter() {
                    let ty = self.view.ty(rv);
                    if is_scalar_fp(ty) {
                        let sr = self.get_val(rv, false);
                        scratch_pairs.push((MReg::vec(fp_r), sr));
                        fp_r += 1;
                    } else {
                        let sr = self.get_val(rv, true);
                        scratch_pairs.push((MReg::gpr(gpr_r), sr));
                        gpr_r += 1;
                    }
                }
                if !scratch_pairs.is_empty() {
                    self.mfunc.push_parallel_copy(scratch_pairs, MInstFlags::NONE);
                }
                self.push_op(MOp::Ret, MInstFlags::TERMINATOR);
            }

            Op::Unreachable => {
                let imm = u16_of(BRK_UNREACHABLE).unwrap_or(0xff02);
                let trap_idx = u32_of(self.trap_kinds.len()).unwrap_or(u32::MAX);
                let ord = self.unreach_ord.get(inst).copied().filter(|&o| o != NONE_U32);
                self.trap_kinds.push((TrapKind::Intrinsic, ord));
                let mi = self.push_op(
                    MOp::Brk { imm },
                    MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP),
                );
                self.mfunc.attach_note(mi, MNoteKind::Trap { trap_idx });
            }

            Op::Trap => {
                let code = o.imm();
                let imm = u16_of(code).unwrap_or(0);
                self.push_op(
                    MOp::Brk { imm },
                    MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP),
                );
            }

            _ => return Err(refuse(Some(inst), op.name())),
        }

        Ok(())
    }

    fn select_mem_addr(&mut self, ptr: Value, off: i32, size: MemSize, pat: SelPattern) -> AddrMode {
        match pat {
            SelPattern::MemSp { off: sp_off } => AddrMode::BaseOff { base: MReg::SP, off: sp_off },
            SelPattern::MemBaseOff { base, off: comb_off } => {
                let br = self.get_val(Value(base), false);
                AddrMode::BaseOff { base: br, off: comb_off }
            }
            SelPattern::MemScaledIdx { base, idx, shift } => {
                let br = self.get_val(Value(base), false);
                let ir = self.get_val(Value(idx), true);
                AddrMode::ScaledIdx { base: br, idx: ir, shift }
            }
            _ => {
                let br = self.get_val(ptr, false);
                self.addr_base_off(br, i64::from(off), size)
            }
        }
    }

    fn lower_add(&mut self, inst: usize, mut a: Value, mut b: Value, res_ty: Ty, pat: SelPattern) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let cls = if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 };

        match pat {
            SelPattern::Madd { a: ma, b: mb, acc } => {
                let r_a = self.get_val(Value(ma), true);
                let r_b = self.get_val(Value(mb), true);
                let r_acc = self.get_val(Value(acc), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::Madd { dst: MReg::V(dst), rn: r_a, rm: r_b, ra: r_acc, wide },
                    MInstFlags::NONE,
                );
                return;
            }
            SelPattern::ShiftedBin { lhs, rhs, shift } => {
                let lr = self.get_val(Value(lhs), true);
                let rr = self.get_val(Value(rhs), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::AddReg {
                        dst: MReg::V(dst),
                        rn: lr,
                        rm: rr,
                        shift: Some(shift),
                        wide,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
            SelPattern::ExtAddSub { lhs, rhs, ext } => {
                let lr = self.get_val(Value(lhs), false);
                let rr = self.get_val(Value(rhs), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::AddExt {
                        dst: MReg::V(dst),
                        rn: lr,
                        rm: rr,
                        ext,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
            _ => {}
        }

        if self.const_u64.get(a.index()).copied().flatten().is_some()
            && self.const_u64.get(b.index()).copied().flatten().is_none()
        {
            core::mem::swap(&mut a, &mut b);
        }

        if let Some(c) = self.const_u64.get(b.index()).copied().flatten() {
            let masked = c & bit_mask(bits);
            if masked == 0 {
                let ar = self.get_val(a, false);
                self.bind_alias_vreg(inst, ar, res_ty);
                return;
            }
        }

        let raw = if bits < 32 {
            self.mfunc.alloc_vreg(cls)
        } else {
            self.alloc_res_vreg(inst, res_ty)
        };
        let mut emitted_imm = false;
        if let Some(c) = self.const_u64.get(b.index()).copied().flatten() {
            let masked = c & bit_mask(bits);
            let ar = self.get_val(a, false);
            if let Some((imm12, shift12)) = encode_add_sub_imm12(masked) {
                self.push_op(
                    MOp::AddImm {
                        dst: MReg::V(raw),
                        rn: ar,
                        imm12,
                        shift12,
                        wide,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                emitted_imm = true;
            } else {
                let neg = (0u64.wrapping_sub(masked)) & bit_mask(bits);
                if let Some((imm12, shift12)) = encode_add_sub_imm12(neg) {
                    self.push_op(
                        MOp::SubImm {
                            dst: MReg::V(raw),
                            rn: ar,
                            imm12,
                            shift12,
                            wide,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                    emitted_imm = true;
                }
            }
        }
        if !emitted_imm {
            let ar = self.get_val(a, true);
            let br = self.get_val(b, true);
            self.push_op(
                MOp::AddReg {
                    dst: MReg::V(raw),
                    rn: ar,
                    rm: br,
                    shift: None,
                    wide,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }

        if bits < 32 {
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.emit_narrow_mask(MReg::V(dst), MReg::V(raw), bits);
        }
    }

    fn lower_sub(&mut self, inst: usize, a: Value, b: Value, res_ty: Ty, pat: SelPattern) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let cls = if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 };

        match pat {
            SelPattern::Msub { a: ma, b: mb, acc } => {
                let r_a = self.get_val(Value(ma), true);
                let r_b = self.get_val(Value(mb), true);
                let r_acc = self.get_val(Value(acc), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::Msub { dst: MReg::V(dst), rn: r_a, rm: r_b, ra: r_acc, wide },
                    MInstFlags::NONE,
                );
                return;
            }
            SelPattern::ShiftedBin { lhs, rhs, shift } => {
                let lr = self.get_val(Value(lhs), true);
                let rr = self.get_val(Value(rhs), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::SubReg {
                        dst: MReg::V(dst),
                        rn: lr,
                        rm: rr,
                        shift: Some(shift),
                        wide,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
            SelPattern::ExtAddSub { lhs, rhs, ext } => {
                let lr = self.get_val(Value(lhs), false);
                let rr = self.get_val(Value(rhs), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::SubExt {
                        dst: MReg::V(dst),
                        rn: lr,
                        rm: rr,
                        ext,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
            _ => {}
        }

        if let Some(c) = self.const_u64.get(b.index()).copied().flatten() {
            let masked = c & bit_mask(bits);
            if masked == 0 {
                let ar = self.get_val(a, false);
                self.bind_alias_vreg(inst, ar, res_ty);
                return;
            }
        }

        let raw = if bits < 32 {
            self.mfunc.alloc_vreg(cls)
        } else {
            self.alloc_res_vreg(inst, res_ty)
        };
        let mut emitted_imm = false;
        if let Some(c) = self.const_u64.get(b.index()).copied().flatten() {
            let masked = c & bit_mask(bits);
            let ar = self.get_val(a, false);
            if let Some((imm12, shift12)) = encode_add_sub_imm12(masked) {
                self.push_op(
                    MOp::SubImm {
                        dst: MReg::V(raw),
                        rn: ar,
                        imm12,
                        shift12,
                        wide,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                emitted_imm = true;
            } else {
                let neg = (0u64.wrapping_sub(masked)) & bit_mask(bits);
                if let Some((imm12, shift12)) = encode_add_sub_imm12(neg) {
                    self.push_op(
                        MOp::AddImm {
                            dst: MReg::V(raw),
                            rn: ar,
                            imm12,
                            shift12,
                            wide,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                    emitted_imm = true;
                }
            }
        }
        if !emitted_imm {
            let ar = self.get_val(a, true);
            let br = self.get_val(b, true);
            self.push_op(
                MOp::SubReg {
                    dst: MReg::V(raw),
                    rn: ar,
                    rm: br,
                    shift: None,
                    wide,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }

        if bits < 32 {
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.emit_narrow_mask(MReg::V(dst), MReg::V(raw), bits);
        }
    }

    fn lower_logical(
        &mut self,
        inst: usize,
        op: Op,
        mut a: Value,
        mut b: Value,
        res_ty: Ty,
        pat: SelPattern,
    ) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let kind = match op {
            Op::And => LogicalKind::And,
            Op::Or => LogicalKind::Orr,
            _ => LogicalKind::Eor,
        };

        match pat {
            SelPattern::Ubfx { x, immr, imms } => {
                let xr = self.get_val(Value(x), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::Ubfm { dst: MReg::V(dst), rn: xr, immr, imms, wide },
                    MInstFlags::NONE,
                );
                return;
            }
            SelPattern::LogicalNot { lhs, rhs } => {
                let lr = self.get_val(Value(lhs), true);
                let rr = self.get_val(Value(rhs), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(dst),
                        rn: lr,
                        rm: rr,
                        shift: None,
                        invert_rm: true,
                        kind,
                        wide,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
            SelPattern::ShiftedBin { lhs, rhs, shift } => {
                let lr = self.get_val(Value(lhs), true);
                let rr = self.get_val(Value(rhs), true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(dst),
                        rn: lr,
                        rm: rr,
                        shift: Some(shift),
                        invert_rm: false,
                        kind,
                        wide,
                    },
                    MInstFlags::NONE,
                );
                return;
            }
            _ => {}
        }

        if self.const_u64.get(a.index()).copied().flatten().is_some()
            && self.const_u64.get(b.index()).copied().flatten().is_none()
        {
            core::mem::swap(&mut a, &mut b);
        }

        if let Some(c) = self.const_u64.get(b.index()).copied().flatten() {
            let masked = c & bit_mask(bits);
            if op == Op::And {
                if masked == 0 {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::MovImm { dst: MReg::V(dst), imm: 0, wide },
                        MInstFlags::REMATERIALIZABLE,
                    );
                    return;
                }
                if masked == bit_mask(bits) {
                    let ar = self.get_val(a, false);
                    self.bind_alias_vreg(inst, ar, res_ty);
                    return;
                }
                // `And(x, 0xff / 0xffff / 0xffff_ffff)` -> single `Ubfm`!
                if masked == 0xff || masked == 0xffff || (wide && masked == 0xffff_ffff) {
                    let imms: u8 = if masked == 0xff {
                        7
                    } else if masked == 0xffff {
                        15
                    } else {
                        31
                    };
                    let ar = self.get_val(a, true);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(dst), rn: ar, immr: 0, imms, wide: false },
                        MInstFlags::NONE,
                    );
                    return;
                }
            } else if masked == 0 {
                let ar = self.get_val(a, false);
                self.bind_alias_vreg(inst, ar, res_ty);
                return;
            }

            let enc_w = if wide { 64 } else { 32 };
            if enc::logical_imm(masked, enc_w).is_some() {
                let ar = self.get_val(a, true);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::LogicalImm { dst: MReg::V(dst), rn: ar, imm: masked, kind, wide },
                    MInstFlags::NONE,
                );
                return;
            }
        }

        let ar = self.get_val(a, true);
        let br = self.get_val(b, true);
        let dst = self.alloc_res_vreg(inst, res_ty);
        self.push_op(
            MOp::LogicalReg {
                dst: MReg::V(dst),
                rn: ar,
                rm: br,
                shift: None,
                invert_rm: false,
                kind,
                wide,
            },
            MInstFlags::NONE,
        );
    }

    fn lower_shift(&mut self, inst: usize, op: Op, a: Value, b: Value, res_ty: Ty) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        if bits == 1 {
            // Shift amount mod 1 is always 0.
            let ar = self.get_val(a, false);
            self.bind_alias_vreg(inst, ar, res_ty);
            return;
        }

        // Constant shift -> single `Ubfm` / `Sbfm` (including narrow `I8`/`I16`!).
        if let Some(c) = self.const_u64.get(b.index()).copied().flatten() {
            let amt = trunc_u32(c) & (bits - 1);
            if amt == 0 {
                let ar = self.get_val(a, false);
                self.bind_alias_vreg(inst, ar, res_ty);
                return;
            }
            let ar = self.get_val(a, true);
            let reg_w = if wide { 64u32 } else { 32u32 };
            match op {
                Op::Shl => {
                    let immr = u8_of((reg_w - amt) & (reg_w - 1)).unwrap_or(0);
                    let imms = u8_of((bits - 1) - amt).unwrap_or(0);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(dst), rn: ar, immr, imms, wide },
                        MInstFlags::NONE,
                    );
                }
                Op::Lshr => {
                    let immr = u8_of(amt).unwrap_or(0);
                    let imms = u8_of(bits - 1).unwrap_or(31);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(dst), rn: ar, immr, imms, wide },
                        MInstFlags::NONE,
                    );
                }
                _ => {
                    // `Ashr`: for narrow `I8`/`I16`, `Sbfm(amt, bits-1)` sign-extends right-shifted
                    // bits, then `Ubfm(0, bits-1)` restores our zero-extended invariant.
                    let immr = u8_of(amt).unwrap_or(0);
                    let imms = u8_of(bits - 1).unwrap_or(31);
                    if bits < 32 {
                        let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                        self.push_op(
                            MOp::Sbfm { dst: MReg::V(tmp), rn: ar, immr, imms, wide: false },
                            MInstFlags::NONE,
                        );
                        let dst = self.alloc_res_vreg(inst, res_ty);
                        self.emit_narrow_mask(MReg::V(dst), MReg::V(tmp), bits);
                    } else {
                        let dst = self.alloc_res_vreg(inst, res_ty);
                        self.push_op(
                            MOp::Sbfm { dst: MReg::V(dst), rn: ar, immr, imms, wide },
                            MInstFlags::NONE,
                        );
                    }
                }
            }
            return;
        }

        // Variable shift (eliding redundant hardware-masked shift-amount masks & casts).
        let req_mask = u64::from(bits - 1);
        let b_peeled = Value(
            u32_of(peel_shift_mask(self.view, &self.const_u64, b.index(), req_mask)).unwrap_or(b.0),
        );
        let ar_raw = self.get_val(a, true);
        let br_raw = self.get_val(b_peeled, true);
        let br = if bits < 32 {
            let masked = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            self.push_op(
                MOp::LogicalImm {
                    dst: MReg::V(masked),
                    rn: br_raw,
                    imm: u64::from(bits - 1),
                    kind: LogicalKind::And,
                    wide: false,
                },
                MInstFlags::NONE,
            );
            MReg::V(masked)
        } else {
            br_raw
        };
        let ar = if op == Op::Ashr && bits < 32 {
            self.sign_extend_operand(ar_raw, bits, false)
        } else {
            ar_raw
        };
        let kind = match op {
            Op::Shl => ShiftKind::Lsl,
            Op::Lshr => ShiftKind::Lsr,
            _ => ShiftKind::Asr,
        };
        if bits < 32 && op != Op::Lshr {
            let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            self.push_op(
                MOp::ShiftVar { dst: MReg::V(raw), rn: ar, rm: br, kind, wide: false },
                MInstFlags::NONE,
            );
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.emit_narrow_mask(MReg::V(dst), MReg::V(raw), bits);
        } else {
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.push_op(
                MOp::ShiftVar { dst: MReg::V(dst), rn: ar, rm: br, kind, wide },
                MInstFlags::NONE,
            );
        }
    }

    fn lower_rotate(&mut self, inst: usize, op: Op, a: Value, b: Value, res_ty: Ty) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        if bits == 1 {
            let ar = self.get_val(a, false);
            self.bind_alias_vreg(inst, ar, res_ty);
            return;
        }

        // Constant rotate.
        if let Some(c) = self.const_u64.get(b.index()).copied().flatten() {
            let raw_k = trunc_u32(c) & (bits - 1);
            let rotr_k = if op == Op::Rotr { raw_k } else { (bits - raw_k) & (bits - 1) };
            if rotr_k == 0 {
                let ar = self.get_val(a, false);
                self.bind_alias_vreg(inst, ar, res_ty);
                return;
            }
            let ar = self.get_val(a, true);
            if bits == 32 || bits == 64 {
                let dst = self.alloc_res_vreg(inst, res_ty);
                let lsb = u8_of(rotr_k).unwrap_or(0);
                self.push_op(
                    MOp::Extr { dst: MReg::V(dst), rn: ar, rm: ar, lsb, wide },
                    MInstFlags::NONE,
                );
            } else {
                // Narrow `I8`/`I16` constant rotate: `(a >> rotr_k) | ((a << (bits - rotr_k)) & mask)`.
                let lo = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                self.push_op(
                    MOp::Ubfm {
                        dst: MReg::V(lo),
                        rn: ar,
                        immr: u8_of(rotr_k).unwrap_or(0),
                        imms: u8_of(bits - 1).unwrap_or(7),
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                let shl_k = bits - rotr_k;
                let hi = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                self.push_op(
                    MOp::Ubfm {
                        dst: MReg::V(hi),
                        rn: ar,
                        immr: u8_of(32 - shl_k).unwrap_or(0),
                        imms: u8_of((bits - 1) - shl_k).unwrap_or(0),
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.push_op(
                    MOp::LogicalReg {
                        dst: MReg::V(dst),
                        rn: MReg::V(lo),
                        rm: MReg::V(hi),
                        shift: None,
                        invert_rm: false,
                        kind: LogicalKind::Orr,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
            }
            return;
        }

        // Variable rotate (eliding redundant hardware-masked shift-amount masks & casts).
        let req_mask = u64::from(bits - 1);
        let b_peeled = Value(
            u32_of(peel_shift_mask(self.view, &self.const_u64, b.index(), req_mask)).unwrap_or(b.0),
        );
        let ar = self.get_val(a, true);
        let br = self.get_val(b_peeled, true);
        if bits == 32 || bits == 64 {
            let amt = if op == Op::Rotl {
                let neg = self.mfunc.alloc_vreg(if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 });
                self.push_op(
                    MOp::SubReg {
                        dst: MReg::V(neg),
                        rn: MReg::XZR,
                        rm: br,
                        shift: None,
                        wide,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                MReg::V(neg)
            } else {
                br
            };
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.push_op(
                MOp::ShiftVar { dst: MReg::V(dst), rn: ar, rm: amt, kind: ShiftKind::Ror, wide },
                MInstFlags::NONE,
            );
        } else {
            let masked = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            self.push_op(
                MOp::LogicalImm {
                    dst: MReg::V(masked),
                    rn: br,
                    imm: u64::from(bits - 1),
                    kind: LogicalKind::And,
                    wide: false,
                },
                MInstFlags::NONE,
            );
            let w_const = self.materialize_int_const(u64::from(bits), false);
            let comp = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            self.push_op(
                MOp::SubReg {
                    dst: MReg::V(comp),
                    rn: MReg::V(w_const),
                    rm: MReg::V(masked),
                    shift: None,
                    wide: false,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            let p1 = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            let p2 = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            let (k1, k2) = if op == Op::Rotl {
                (ShiftKind::Lsl, ShiftKind::Lsr)
            } else {
                (ShiftKind::Lsr, ShiftKind::Lsl)
            };
            self.push_op(
                MOp::ShiftVar { dst: MReg::V(p1), rn: ar, rm: MReg::V(masked), kind: k1, wide: false },
                MInstFlags::NONE,
            );
            self.push_op(
                MOp::ShiftVar { dst: MReg::V(p2), rn: ar, rm: MReg::V(comp), kind: k2, wide: false },
                MInstFlags::NONE,
            );
            let comb = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            self.push_op(
                MOp::LogicalReg {
                    dst: MReg::V(comb),
                    rn: MReg::V(p1),
                    rm: MReg::V(p2),
                    shift: None,
                    invert_rm: false,
                    kind: LogicalKind::Orr,
                    wide: false,
                },
                MInstFlags::NONE,
            );
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.emit_narrow_mask(MReg::V(dst), MReg::V(comb), bits);
        }
    }

    fn lower_div_rem(
        &mut self,
        inst: usize,
        op: Op,
        flags: Flags,
        a: Value,
        b: Value,
        res_ty: Ty,
    ) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let cls = if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 };
        let signed = matches!(op, Op::Sdiv | Op::Srem);
        let is_rem = matches!(op, Op::Urem | Op::Srem);

        let a_raw = self.get_val(a, false);
        let b_raw = self.get_val(b, false);
        let (ar, br) = if signed && bits < 32 {
            (
                self.sign_extend_operand(a_raw, bits, false),
                self.sign_extend_operand(b_raw, bits, false),
            )
        } else {
            (a_raw, b_raw)
        };

        // Trap on divisor == 0 unless `Flags::NONZERO` promises it cannot be zero.
        if !flags.contains(Flags::NONZERO) {
            let trap_blk = self.get_div_zero_trap();
            self.push_op(
                MOp::Cbz { rt: br, target: trap_blk, wide },
                MInstFlags::TERMINATOR,
            );
            self.split_fallthrough();
        }

        // Trap on `MIN / -1` for `Sdiv` unless `Flags::NSW` promises it cannot happen.
        // (`Srem` of `MIN % -1` is defined as `0` in LIR, which AArch64 `sdiv`+`msub` produces natively.)
        if op == Op::Sdiv && !flags.contains(Flags::NSW) {
            let trap_blk = self.get_div_ovf_trap();
            let b_plus_1 = self.mfunc.alloc_vreg(cls);
            self.push_op(
                MOp::AddImm {
                    dst: MReg::V(b_plus_1),
                    rn: br,
                    imm12: 1,
                    shift12: false,
                    wide,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            let min_val: u64 = if bits == 64 {
                1u64 << 63
            } else {
                // `ar` is sign-extended to 32 bits, so MIN of `bits` is sign-extended to 32 bits.
                let min_narrow = 1u64 << (bits - 1);
                sign_extend_u64(min_narrow, bits).cast_unsigned() & 0xffff_ffff
            };
            let min_v = self.materialize_int_const(min_val, wide);
            let (_, vf1) = self.push_flag_writer(
                MOp::CmpImm { rn: MReg::V(b_plus_1), imm12: 0, shift12: false, wide },
                MInstFlags::NONE,
            );
            let (ccmp, vf2) = self.push_flag_writer(
                MOp::CcmpReg { rn: ar, rm: MReg::V(min_v), nzcv: 0, cond: MCond::Eq, wide },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(ccmp, vf1);
            let br_ovf = self.push_op(
                MOp::BCond { cond: MCond::Eq, target: trap_blk },
                MInstFlags::TERMINATOR,
            );
            self.mfunc.set_vflag_use(br_ovf, vf2);
            self.split_fallthrough();
        }

        let needs_mask = bits < 32 && signed;
        let quot = if !is_rem && !needs_mask {
            self.alloc_res_vreg(inst, res_ty)
        } else {
            self.mfunc.alloc_vreg(cls)
        };
        let div_op = if signed {
            MOp::Sdiv { dst: MReg::V(quot), rn: ar, rm: br, wide }
        } else {
            MOp::Udiv { dst: MReg::V(quot), rn: ar, rm: br, wide }
        };
        self.push_op(div_op, MInstFlags::NONE);

        let raw = if is_rem {
            let rem = if !needs_mask {
                self.alloc_res_vreg(inst, res_ty)
            } else {
                self.mfunc.alloc_vreg(cls)
            };
            self.push_op(
                MOp::Msub { dst: MReg::V(rem), rn: MReg::V(quot), rm: br, ra: ar, wide },
                MInstFlags::NONE,
            );
            rem
        } else {
            quot
        };

        if needs_mask {
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.emit_narrow_mask(MReg::V(dst), MReg::V(raw), bits);
        }
    }

    fn lower_bitop(&mut self, inst: usize, op: Op, a: Value, res_ty: Ty) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let ar = self.get_val(a, true);

        match op {
            Op::Clz => {
                if bits == 1 {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::LogicalImm {
                            dst: MReg::V(dst),
                            rn: ar,
                            imm: 1,
                            kind: LogicalKind::Eor,
                            wide: false,
                        },
                        MInstFlags::NONE,
                    );
                } else if bits < 32 {
                    let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                    self.push_op(MOp::Clz { dst: MReg::V(tmp), rn: ar, wide: false }, MInstFlags::NONE);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    let sub_amt = u16_of(32 - bits).unwrap_or(24);
                    self.push_op(
                        MOp::SubImm {
                            dst: MReg::V(dst),
                            rn: MReg::V(tmp),
                            imm12: sub_amt,
                            shift12: false,
                            wide: false,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                } else {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(MOp::Clz { dst: MReg::V(dst), rn: ar, wide }, MInstFlags::NONE);
                }
            }
            Op::Ctz => {
                if bits == 1 {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::LogicalImm {
                            dst: MReg::V(dst),
                            rn: ar,
                            imm: 1,
                            kind: LogicalKind::Eor,
                            wide: false,
                        },
                        MInstFlags::NONE,
                    );
                } else {
                    let src = if bits < 32 {
                        let guarded = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                        self.push_op(
                            MOp::LogicalImm {
                                dst: MReg::V(guarded),
                                rn: ar,
                                imm: 1u64 << bits,
                                kind: LogicalKind::Orr,
                                wide: false,
                            },
                            MInstFlags::NONE,
                        );
                        MReg::V(guarded)
                    } else {
                        ar
                    };
                    let rev = self.mfunc.alloc_vreg(if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 });
                    self.push_op(MOp::Rbit { dst: MReg::V(rev), rn: src, wide }, MInstFlags::NONE);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(MOp::Clz { dst: MReg::V(dst), rn: MReg::V(rev), wide }, MInstFlags::NONE);
                }
            }
            Op::Popcnt => {
                // Uses reserved NEON scratch `V30` (`mra` never allocates `V30` to VRegs) with
                // explicit `defs_fp`/`uses_fp` and `PINNED` so `msched_pre` never interleaves a
                // spilled FP op (which could reload through `V30`) into this 4-inst sequence.
                let dst = self.alloc_res_vreg(inst, res_ty);
                let v30 = MReg::vec(30);
                let v30_mask = 1u32 << 30;
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::MovFromGpr,
                        dst: v30,
                        rn: ar,
                        rm: None,
                        ra: None,
                        double: true,
                    },
                    MInstFlags::PINNED,
                );
                self.push_op(
                    MOp::RawWord {
                        word: enc::cnt_8b(30, 30),
                        defs_gpr: 0,
                        uses_gpr: 0,
                        defs_fp: v30_mask,
                        uses_fp: v30_mask,
                        writes_flags: false,
                        reads_flags: false,
                        is_mem_barrier: false,
                    },
                    MInstFlags::PINNED,
                );
                self.push_op(
                    MOp::RawWord {
                        word: enc::addv_8b(30, 30),
                        defs_gpr: 0,
                        uses_gpr: 0,
                        defs_fp: v30_mask,
                        uses_fp: v30_mask,
                        writes_flags: false,
                        reads_flags: false,
                        is_mem_barrier: false,
                    },
                    MInstFlags::PINNED,
                );
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::MovToGpr,
                        dst: MReg::V(dst),
                        rn: v30,
                        rm: None,
                        ra: None,
                        double: true,
                    },
                    MInstFlags::PINNED,
                );
            }
            Op::Bswap => {
                if bits <= 8 {
                    let ar_nz = self.get_val(a, false);
                    self.bind_alias_vreg(inst, ar_nz, res_ty);
                } else if bits == 16 {
                    let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                    self.push_op(MOp::Rev { dst: MReg::V(tmp), rn: ar, wide: false }, MInstFlags::NONE);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(dst), rn: MReg::V(tmp), immr: 16, imms: 31, wide: false },
                        MInstFlags::NONE,
                    );
                } else {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(MOp::Rev { dst: MReg::V(dst), rn: ar, wide }, MInstFlags::NONE);
                }
            }
            _ => {
                // `Bitrev`
                if bits == 1 {
                    let ar_nz = self.get_val(a, false);
                    self.bind_alias_vreg(inst, ar_nz, res_ty);
                } else if bits < 32 {
                    let tmp = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                    self.push_op(MOp::Rbit { dst: MReg::V(tmp), rn: ar, wide: false }, MInstFlags::NONE);
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    let immr = u8_of(32 - bits).unwrap_or(24);
                    self.push_op(
                        MOp::Ubfm { dst: MReg::V(dst), rn: MReg::V(tmp), immr, imms: 31, wide: false },
                        MInstFlags::NONE,
                    );
                } else {
                    let dst = self.alloc_res_vreg(inst, res_ty);
                    self.push_op(MOp::Rbit { dst: MReg::V(dst), rn: ar, wide }, MInstFlags::NONE);
                }
            }
        }
    }

    fn lower_minmax(&mut self, inst: usize, op: Op, a: Value, b: Value, res_ty: Ty) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let has_cssc = (self.opts.machine.features & Machine::CSSC) != 0;
        let ar_raw = self.get_val(a, true);
        let br_raw = self.get_val(b, true);

        if has_cssc && (bits == 32 || bits == 64) {
            let dst = self.alloc_res_vreg(inst, res_ty);
            let mop = match op {
                Op::Umin => MOp::Umin { dst: MReg::V(dst), rn: ar_raw, rm: br_raw, wide },
                Op::Umax => MOp::Umax { dst: MReg::V(dst), rn: ar_raw, rm: br_raw, wide },
                Op::Smin => MOp::Smin { dst: MReg::V(dst), rn: ar_raw, rm: br_raw, wide },
                _ => MOp::Smax { dst: MReg::V(dst), rn: ar_raw, rm: br_raw, wide },
            };
            self.push_op(mop, MInstFlags::NONE);
            return;
        }

        let signed = matches!(op, Op::Smin | Op::Smax);
        let (cmp_a, cmp_b) = if signed && bits < 32 {
            (
                self.sign_extend_operand(ar_raw, bits, false),
                self.sign_extend_operand(br_raw, bits, false),
            )
        } else {
            (ar_raw, br_raw)
        };
        let (_, vf) = self.push_flag_writer(
            MOp::CmpReg { rn: cmp_a, rm: cmp_b, shift: None, wide },
            MInstFlags::NONE,
        );
        let cond = match op {
            Op::Umin => MCond::Lo,
            Op::Umax => MCond::Hi,
            Op::Smin => MCond::Lt,
            _ => MCond::Gt,
        };
        let dst = self.alloc_res_vreg(inst, res_ty);
        // Select from original zero-extended `ar_raw`/`br_raw` so narrow values stay zero-extended!
        let csel = self.push_op(
            MOp::Csel { dst: MReg::V(dst), rn: ar_raw, rm: br_raw, cond, wide },
            MInstFlags::NONE,
        );
        self.mfunc.set_vflag_use(csel, vf);
    }

    /// Returns `true` if `val` is statically guaranteed to already have bits `[63:to_bits]` all zero,
    /// allowing `Op::Trunc` to zero-cost alias `val`'s SSA `VReg` without emitting `mov`/`and`.
    fn value_fits_zero_extended_bits(&self, val: Value, to_bits: u32) -> bool {
        let vi = val.index();
        let src_bits = int_bits(self.view.ty(val));
        if src_bits == 0 || to_bits == 0 {
            return false;
        }
        if src_bits <= to_bits {
            return true;
        }
        let target_mask = bit_mask(to_bits);
        if let Some(c) = self.const_u64.get(vi).copied().flatten() {
            return (c & !target_mask) == 0;
        }
        match self.view.op(vi) {
            Op::Lshr => {
                let mut o = self.view.operands(vi);
                let _lhs = o.value();
                let amt_v = o.value();
                if let Some(k) = self.const_u64.get(amt_v.index()).copied().flatten() {
                    let amt = trunc_u32(k) & (src_bits - 1);
                    src_bits - amt <= to_bits
                } else {
                    false
                }
            }
            Op::Zext => {
                let inner = self.view.operands(vi).value();
                let inner_bits = int_bits(self.view.ty(inner));
                inner_bits > 0 && inner_bits <= to_bits
            }
            Op::And => {
                let mut o = self.view.operands(vi);
                let x = o.value();
                let y = o.value();
                let src_mask = bit_mask(src_bits);
                let x_ok = self
                    .const_u64
                    .get(x.index())
                    .copied()
                    .flatten()
                    .is_some_and(|c| ((c & src_mask) & !target_mask) == 0);
                let y_ok = self
                    .const_u64
                    .get(y.index())
                    .copied()
                    .flatten()
                    .is_some_and(|c| ((c & src_mask) & !target_mask) == 0);
                x_ok || y_ok
            }
            Op::Clz | Op::Ctz | Op::Popcnt => to_bits >= 8,
            _ => false,
        }
    }

    /// Lowers saturating integer arithmetic (`UsubSat`, `UaddSat`, `SaddSat`, `SsubSat`) across
    /// `I1`, `I8`, `I16`, `I32`, and `I64`, preserving `misel`'s zero-extended narrow SSA invariant.
    fn lower_sat_arith(&mut self, inst: usize, op: Op, a: Value, b: Value, res_ty: Ty) {
        let bits = int_bits(res_ty);
        let wide = bits == 64;
        let cls = if wide { VRegClass::Gpr64 } else { VRegClass::Gpr32 };

        match op {
            Op::UsubSat => {
                let tmp = self.mfunc.alloc_vreg(cls);
                let vf = if let Some(c) = self.const_u64.get(b.index()).copied().flatten()
                    && let Some((imm12, shift12)) = encode_add_sub_imm12(c & bit_mask(bits))
                {
                    let ar = self.get_val(a, false);
                    let (_, vf) = self.push_flag_writer(
                        MOp::SubImm {
                            dst: MReg::V(tmp),
                            rn: ar,
                            imm12,
                            shift12,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                } else {
                    let ar = self.get_val(a, true);
                    let br = self.get_val(b, true);
                    let (_, vf) = self.push_flag_writer(
                        MOp::SubReg {
                            dst: MReg::V(tmp),
                            rn: ar,
                            rm: br,
                            shift: None,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                };
                let dst = self.alloc_res_vreg(inst, res_ty);
                let cs = self.push_op(
                    MOp::Csel {
                        dst: MReg::V(dst),
                        rn: MReg::XZR,
                        rm: MReg::V(tmp),
                        cond: MCond::Lo,
                        wide,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            Op::UaddSat if bits == 32 || bits == 64 => {
                let tmp = self.mfunc.alloc_vreg(cls);
                let vf = if let Some(c) = self.const_u64.get(b.index()).copied().flatten()
                    && let Some((imm12, shift12)) = encode_add_sub_imm12(c & bit_mask(bits))
                {
                    let ar = self.get_val(a, false);
                    let (_, vf) = self.push_flag_writer(
                        MOp::AddImm {
                            dst: MReg::V(tmp),
                            rn: ar,
                            imm12,
                            shift12,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                } else {
                    let ar = self.get_val(a, true);
                    let br = self.get_val(b, true);
                    let (_, vf) = self.push_flag_writer(
                        MOp::AddReg {
                            dst: MReg::V(tmp),
                            rn: ar,
                            rm: br,
                            shift: None,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                };
                let dst = self.alloc_res_vreg(inst, res_ty);
                let cs = self.push_op(
                    MOp::Csinv {
                        dst: MReg::V(dst),
                        rn: MReg::V(tmp),
                        rm: MReg::XZR,
                        cond: MCond::Lo,
                        wide,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            Op::UaddSat => {
                // Narrow `I1`/`I8`/`I16`: 32-bit sum cannot overflow; clamp to `(1 << bits) - 1`.
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let sum = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                self.push_op(
                    MOp::AddReg {
                        dst: MReg::V(sum),
                        rn: ar,
                        rm: br,
                        shift: None,
                        wide: false,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                let max_v = self.materialize_int_const(bit_mask(bits), false);
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpReg {
                        rn: MReg::V(sum),
                        rm: MReg::V(max_v),
                        shift: None,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                let dst = self.alloc_res_vreg(inst, res_ty);
                let cs = self.push_op(
                    MOp::Csel {
                        dst: MReg::V(dst),
                        rn: MReg::V(max_v),
                        rm: MReg::V(sum),
                        cond: MCond::Hi,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            Op::SaddSat | Op::SsubSat if bits == 32 || bits == 64 => {
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let raw = self.mfunc.alloc_vreg(cls);
                let arith_op = if op == Op::SaddSat {
                    MOp::AddReg {
                        dst: MReg::V(raw),
                        rn: ar,
                        rm: br,
                        shift: None,
                        wide,
                        sets_flags: true,
                    }
                } else {
                    MOp::SubReg {
                        dst: MReg::V(raw),
                        rn: ar,
                        rm: br,
                        shift: None,
                        wide,
                        sets_flags: true,
                    }
                };
                let (_, vf) = self.push_flag_writer(arith_op, MInstFlags::NONE);
                // On signed overflow (`Vs`), the wrapped sign bit is inverted:
                // `bound = (raw >>_arith (bits - 1)) ^ (1 << (bits - 1))` gives `SMAX` or `SMIN`.
                let top = u8_of(bits - 1).unwrap_or(63);
                let sign_mask = self.mfunc.alloc_vreg(cls);
                self.push_op(
                    MOp::Sbfm { dst: MReg::V(sign_mask), rn: MReg::V(raw), immr: top, imms: top, wide },
                    MInstFlags::NONE,
                );
                let bound = self.mfunc.alloc_vreg(cls);
                self.push_op(
                    MOp::LogicalImm {
                        dst: MReg::V(bound),
                        rn: MReg::V(sign_mask),
                        imm: 1u64 << (bits - 1),
                        kind: LogicalKind::Eor,
                        wide,
                    },
                    MInstFlags::NONE,
                );
                let dst = self.alloc_res_vreg(inst, res_ty);
                let cs = self.push_op(
                    MOp::Csel {
                        dst: MReg::V(dst),
                        rn: MReg::V(bound),
                        rm: MReg::V(raw),
                        cond: MCond::Vs,
                        wide,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            _ => {
                // Narrow `I1`/`I8`/`I16` signed saturating add/sub: sign-extend to 32 bits, compute,
                // clamp to `[SMIN, SMAX]`, and zero-mask to `bits`.
                let a_raw = self.get_val(a, false);
                let b_raw = self.get_val(b, false);
                let a_s = self.sign_extend_operand(a_raw, bits, false);
                let b_s = self.sign_extend_operand(b_raw, bits, false);
                let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                let arith_op = if op == Op::SaddSat {
                    MOp::AddReg {
                        dst: MReg::V(raw),
                        rn: a_s,
                        rm: b_s,
                        shift: None,
                        wide: false,
                        sets_flags: false,
                    }
                } else {
                    MOp::SubReg {
                        dst: MReg::V(raw),
                        rn: a_s,
                        rm: b_s,
                        shift: None,
                        wide: false,
                        sets_flags: false,
                    }
                };
                self.push_op(arith_op, MInstFlags::NONE);
                let max_s: u64 = (1u64 << (bits - 1)) - 1;
                let min_s: u64 = (0u64.wrapping_sub(1u64 << (bits - 1))) & 0xffff_ffff;
                let max_v = self.materialize_int_const(max_s, false);
                let min_v = self.materialize_int_const(min_s, false);
                let (_, vf1) = self.push_flag_writer(
                    MOp::CmpReg {
                        rn: MReg::V(raw),
                        rm: MReg::V(max_v),
                        shift: None,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                let hi_clamped = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                let cs1 = self.push_op(
                    MOp::Csel {
                        dst: MReg::V(hi_clamped),
                        rn: MReg::V(max_v),
                        rm: MReg::V(raw),
                        cond: MCond::Gt,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs1, vf1);
                let (_, vf2) = self.push_flag_writer(
                    MOp::CmpReg {
                        rn: MReg::V(hi_clamped),
                        rm: MReg::V(min_v),
                        shift: None,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                let clamped = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                let cs2 = self.push_op(
                    MOp::Csel {
                        dst: MReg::V(clamped),
                        rn: MReg::V(min_v),
                        rm: MReg::V(hi_clamped),
                        cond: MCond::Lt,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs2, vf2);
                let dst = self.alloc_res_vreg(inst, res_ty);
                self.emit_narrow_mask(MReg::V(dst), MReg::V(clamped), bits);
            }
        }
    }

    /// Lowers `{T, i1}` overflow ops (`UaddOverflow`, `UsubOverflow`, `SaddOverflow`,
    /// `SsubOverflow`, `UmulOverflow`, `SmulOverflow`) across `I1..=I64`, reusing or allocating
    /// `(val_v, flag_v)` via `get_or_alloc_ovf_pair(inst)` for zero-instruction `Op::Project`.
    fn lower_overflow(&mut self, inst: usize, op: Op, a: Value, b: Value) {
        let op_ty = self.view.ty(a);
        let w = int_bits(op_ty);
        let wide = w == 64;

        let (val_id, flag_id) = self.get_or_alloc_ovf_pair(inst);
        let val_v = VReg(val_id);
        let flag_v = VReg(flag_id);

        match (op, w) {
            (Op::UaddOverflow, 32 | 64) => {
                let vf = if let Some(c) = self.const_u64.get(b.index()).copied().flatten()
                    && let Some((imm12, shift12)) = encode_add_sub_imm12(c & bit_mask(w))
                {
                    let ar = self.get_val(a, false);
                    let (_, vf) = self.push_flag_writer(
                        MOp::AddImm {
                            dst: MReg::V(val_v),
                            rn: ar,
                            imm12,
                            shift12,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                } else {
                    let ar = self.get_val(a, true);
                    let br = self.get_val(b, true);
                    let (_, vf) = self.push_flag_writer(
                        MOp::AddReg {
                            dst: MReg::V(val_v),
                            rn: ar,
                            rm: br,
                            shift: None,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                };
                // `cset flag_v, hs` (`Hs.invert() == Lo`).
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Lo,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            (Op::UaddOverflow, _) => {
                // `w < 32`: `a + b <= 2 * ((1 << w) - 1)`, so bit `w` of `raw` IS the carry flag!
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                self.push_op(
                    MOp::AddReg {
                        dst: MReg::V(raw),
                        rn: ar,
                        rm: br,
                        shift: None,
                        wide: false,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                self.emit_narrow_mask(MReg::V(val_v), MReg::V(raw), w);
                let immr = u8_of(w).unwrap_or(8);
                self.push_op(
                    MOp::Ubfm { dst: MReg::V(flag_v), rn: MReg::V(raw), immr, imms: 31, wide: false },
                    MInstFlags::NONE,
                );
            }
            (Op::UsubOverflow, _) => {
                let raw = if w < 32 {
                    self.mfunc.alloc_vreg(VRegClass::Gpr32)
                } else {
                    val_v
                };
                let vf = if let Some(c) = self.const_u64.get(b.index()).copied().flatten()
                    && let Some((imm12, shift12)) = encode_add_sub_imm12(c & bit_mask(w))
                {
                    let ar = self.get_val(a, false);
                    let (_, vf) = self.push_flag_writer(
                        MOp::SubImm {
                            dst: MReg::V(raw),
                            rn: ar,
                            imm12,
                            shift12,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                } else {
                    let ar = self.get_val(a, true);
                    let br = self.get_val(b, true);
                    let (_, vf) = self.push_flag_writer(
                        MOp::SubReg {
                            dst: MReg::V(raw),
                            rn: ar,
                            rm: br,
                            shift: None,
                            wide,
                            sets_flags: true,
                        },
                        MInstFlags::NONE,
                    );
                    vf
                };
                // `cset flag_v, lo` (`Lo.invert() == Hs`).
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Hs,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
                if w < 32 {
                    self.emit_narrow_mask(MReg::V(val_v), MReg::V(raw), w);
                }
            }
            (Op::SaddOverflow | Op::SsubOverflow, 32 | 64) => {
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let arith_op = if op == Op::SaddOverflow {
                    MOp::AddReg {
                        dst: MReg::V(val_v),
                        rn: ar,
                        rm: br,
                        shift: None,
                        wide,
                        sets_flags: true,
                    }
                } else {
                    MOp::SubReg {
                        dst: MReg::V(val_v),
                        rn: ar,
                        rm: br,
                        shift: None,
                        wide,
                        sets_flags: true,
                    }
                };
                let (_, vf) = self.push_flag_writer(arith_op, MInstFlags::NONE);
                // `cset flag_v, vs` (`Vs.invert() == Vc`).
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Vc,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            (Op::SaddOverflow | Op::SsubOverflow, _) => {
                let a_raw = self.get_val(a, false);
                let b_raw = self.get_val(b, false);
                let a_s = self.sign_extend_operand(a_raw, w, false);
                let b_s = self.sign_extend_operand(b_raw, w, false);
                let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                let arith_op = if op == Op::SaddOverflow {
                    MOp::AddReg {
                        dst: MReg::V(raw),
                        rn: a_s,
                        rm: b_s,
                        shift: None,
                        wide: false,
                        sets_flags: false,
                    }
                } else {
                    MOp::SubReg {
                        dst: MReg::V(raw),
                        rn: a_s,
                        rm: b_s,
                        shift: None,
                        wide: false,
                        sets_flags: false,
                    }
                };
                self.push_op(arith_op, MInstFlags::NONE);
                let sext_raw = self.sign_extend_operand(MReg::V(raw), w, false);
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpReg { rn: MReg::V(raw), rm: sext_raw, shift: None, wide: false },
                    MInstFlags::NONE,
                );
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Eq,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
                self.emit_narrow_mask(MReg::V(val_v), MReg::V(raw), w);
            }
            (Op::UmulOverflow, 64) => {
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let hi = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(MOp::Mul { dst: MReg::V(val_v), rn: ar, rm: br, wide: true }, MInstFlags::NONE);
                self.push_op(MOp::Umulh { dst: MReg::V(hi), rn: ar, rm: br }, MInstFlags::NONE);
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpImm { rn: MReg::V(hi), imm12: 0, shift12: false, wide: true },
                    MInstFlags::NONE,
                );
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Eq,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            (Op::UmulOverflow, 32) => {
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let prod = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(MOp::Mul { dst: MReg::V(prod), rn: ar, rm: br, wide: true }, MInstFlags::NONE);
                self.push_op(
                    MOp::Mov { dst: MReg::V(val_v), src: MReg::V(prod), wide: false },
                    MInstFlags::NONE,
                );
                let hi = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::Ubfm { dst: MReg::V(hi), rn: MReg::V(prod), immr: 32, imms: 63, wide: true },
                    MInstFlags::NONE,
                );
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpImm { rn: MReg::V(hi), imm12: 0, shift12: false, wide: true },
                    MInstFlags::NONE,
                );
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Eq,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            (Op::UmulOverflow, _) => {
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                self.push_op(MOp::Mul { dst: MReg::V(raw), rn: ar, rm: br, wide: false }, MInstFlags::NONE);
                self.emit_narrow_mask(MReg::V(val_v), MReg::V(raw), w);
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpReg { rn: MReg::V(raw), rm: MReg::V(val_v), shift: None, wide: false },
                    MInstFlags::NONE,
                );
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Eq,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            (Op::SmulOverflow, 64) => {
                let ar = self.get_val(a, true);
                let br = self.get_val(b, true);
                let hi = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(MOp::Mul { dst: MReg::V(val_v), rn: ar, rm: br, wide: true }, MInstFlags::NONE);
                self.push_op(MOp::Smulh { dst: MReg::V(hi), rn: ar, rm: br }, MInstFlags::NONE);
                let sign = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(
                    MOp::Sbfm { dst: MReg::V(sign), rn: MReg::V(val_v), immr: 63, imms: 63, wide: true },
                    MInstFlags::NONE,
                );
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpReg { rn: MReg::V(hi), rm: MReg::V(sign), shift: None, wide: true },
                    MInstFlags::NONE,
                );
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Eq,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
            }
            (Op::SmulOverflow, 32) => {
                let a_raw = self.get_val(a, false);
                let b_raw = self.get_val(b, false);
                let a_s = self.sign_extend_operand(a_raw, 32, true);
                let b_s = self.sign_extend_operand(b_raw, 32, true);
                let prod = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                self.push_op(MOp::Mul { dst: MReg::V(prod), rn: a_s, rm: b_s, wide: true }, MInstFlags::NONE);
                let sext_prod = self.sign_extend_operand(MReg::V(prod), 32, true);
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpReg { rn: MReg::V(prod), rm: sext_prod, shift: None, wide: true },
                    MInstFlags::NONE,
                );
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Eq,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
                self.push_op(
                    MOp::Mov { dst: MReg::V(val_v), src: MReg::V(prod), wide: false },
                    MInstFlags::NONE,
                );
            }
            _ => {
                // `SmulOverflow` for `I1`/`I8`/`I16`.
                let a_raw = self.get_val(a, false);
                let b_raw = self.get_val(b, false);
                let a_s = self.sign_extend_operand(a_raw, w, false);
                let b_s = self.sign_extend_operand(b_raw, w, false);
                let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                self.push_op(MOp::Mul { dst: MReg::V(raw), rn: a_s, rm: b_s, wide: false }, MInstFlags::NONE);
                let sext_raw = self.sign_extend_operand(MReg::V(raw), w, false);
                let (_, vf) = self.push_flag_writer(
                    MOp::CmpReg { rn: MReg::V(raw), rm: sext_raw, shift: None, wide: false },
                    MInstFlags::NONE,
                );
                let cs = self.push_op(
                    MOp::Csinc {
                        dst: MReg::V(flag_v),
                        rn: MReg::XZR,
                        rm: MReg::XZR,
                        cond: MCond::Eq,
                        wide: false,
                    },
                    MInstFlags::NONE,
                );
                self.mfunc.set_vflag_use(cs, vf);
                self.emit_narrow_mask(MReg::V(val_v), MReg::V(raw), w);
            }
        }
    }

    /// Emits an adjacent `PINNED` GOT address-load pair (`adrp x17` + `ldr dst, [x17, #0]`) for
    /// `Static` / `Func` / `Vtable` addresses. Using reserved `X17` for `Adrp` guarantees `n.at == r.at + 4`
    /// even under register spilling, and avoids `SiteKind::Call` misclassification.
    fn emit_got_load_addr(&mut self, dst: VReg, target: u32) {
        let adrp = self.push_op(MOp::Adrp { dst: MReg::p(PReg::X17) }, MInstFlags::PINNED);
        self.mfunc.attach_note(
            adrp,
            MNoteKind::Reloc { kind: RelocKind::GotPage21, target, addend: 0 },
        );
        let ldr = self.push_op(
            MOp::Ldr {
                dst: MReg::V(dst),
                addr: AddrMode::BaseOff { base: MReg::p(PReg::X17), off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ.union(MInstFlags::PINNED),
        );
        self.mfunc.attach_note(
            ldr,
            MNoteKind::Reloc { kind: RelocKind::GotPageOff12, target, addend: 0 },
        );
    }

    /// Emits an adjacent `PINNED` `OWN_RELRO` address pair (`adrp x17` + `add dst, x17, #lo12`).
    fn emit_relro_addr(&mut self, dst: VReg, relro_off: u32) {
        let addend = i64::from(relro_off);
        let adrp = self.push_op(MOp::Adrp { dst: MReg::p(PReg::X17) }, MInstFlags::PINNED);
        self.mfunc.attach_note(
            adrp,
            MNoteKind::Reloc { kind: RelocKind::Page21, target: OWN_RELRO, addend },
        );
        let add = self.push_op(
            MOp::AddLo12 { dst: MReg::V(dst), rn: MReg::p(PReg::X17) },
            MInstFlags::PINNED,
        );
        self.mfunc.attach_note(
            add,
            MNoteKind::Reloc {
                kind: RelocKind::PageOff12 { scale: 0 },
                target: OWN_RELRO,
                addend,
            },
        );
    }

    /// Iteratively places `root_row` (and any transitively referenced `RefKind::Data` rows) into
    /// `self.relro`, recording internal pointer `Word`s and `PositionRow`s.
    fn place_data_row(&mut self, root_row: u32, inst: usize) -> Result<u32, TierError> {
        if let Some(off) = self.data_relro_off.get(to_idx(root_row)).copied().flatten() {
            return Ok(off);
        }
        let mut queue: Vec<u32, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pending_data_words: Vec<(u32, u32, i64), Alloc<'a>> = Vec::new_in(self.alloc);
        queue.push(root_row);
        let mut q_idx = 0usize;
        while let Some(&row) = queue.get(q_idx) {
            q_idx += 1;
            if self.data_relro_off.get(to_idx(row)).copied().flatten().is_some() {
                continue;
            }
            let Some(&data) = self.view.datas.get(to_idx(row)) else {
                return Err(refuse(Some(inst), "missing data row"));
            };
            if data.align > u32::from(MOST_PART_ALIGN) {
                return Err(refuse(Some(inst), "data align > 4 KiB"));
            }
            let align_pow = u8_of(data.align).unwrap_or(0);
            let align = 8usize.max(1usize << u32::from(align_pow));
            if align_pow > self.relro.relro_align {
                self.relro.relro_align = align_pow;
            }
            while self.relro.relro.len() % align != 0 {
                self.relro.relro.push(0);
            }
            let start = u32_of(self.relro.relro.len())
                .ok_or(TierError::TooLarge { tier: TIER, what: "relro bytes" })?;
            let bytes = self.view.data_bytes_of(DataId(row));
            if bytes.is_empty() {
                for _ in 0..8 {
                    self.relro.relro.push(0);
                }
            } else {
                for &b in bytes {
                    self.relro.relro.push(b);
                }
            }
            if let Some(slot) = self.data_relro_off.get_mut(to_idx(row)) {
                *slot = Some(start);
            }

            for p in self.view.data_positions_of(DataId(row)) {
                let field = match blaze_lir::PositionField::from_word(p.field) {
                    Some(blaze_lir::PositionField::Line) => PositionField::Line,
                    Some(blaze_lir::PositionField::Col) => PositionField::Col,
                    None => return Err(refuse(Some(inst), "unknown data position field")),
                };
                let at = start
                    .checked_add(p.offset)
                    .ok_or(TierError::TooLarge { tier: TIER, what: "relro position offset" })?;
                let slot_end = to_idx(at).checked_add(4).unwrap_or(usize::MAX);
                if slot_end > self.relro.relro.len() {
                    return Err(refuse(Some(inst), "data position past row end"));
                }
                if let Some(slice) = self.relro.relro.get_mut(to_idx(at)..slot_end) {
                    slice.fill(0);
                }
                self.relro.position_slots.push(PositionRow {
                    part: PositionPart::Relro,
                    at,
                    item: p.item_key(),
                    site: p.site,
                    field,
                });
            }

            for r in self.view.data_relocs_of(DataId(row)) {
                if r.offset % 8 != 0
                    || u64::from(r.offset).saturating_add(8) > u64::from(data.bytes_len)
                {
                    return Err(refuse(Some(inst), "invalid data reloc offset"));
                }
                let word = start
                    .checked_add(r.offset)
                    .ok_or(TierError::TooLarge { tier: TIER, what: "relro word offset" })?;
                let word_end = to_idx(word).checked_add(8).unwrap_or(usize::MAX);
                if let Some(slice) = self.relro.relro.get_mut(to_idx(word)..word_end) {
                    slice.fill(0);
                }
                let Some(target_ref) = self.view.refs.get(RefId(r.target).index()).copied() else {
                    return Err(refuse(Some(inst), "data reloc target out of bounds"));
                };
                match RefKind::from_word(target_ref.kind) {
                    Some(RefKind::Static) => {
                        let t = self.targets.intern_outside(
                            Outside::Static(target_ref.place),
                            Space::Static,
                            true,
                            self.names,
                            inst,
                        )?;
                        self.relro.words.push(Word { at: word, target: t, addend: r.addend });
                    }
                    Some(RefKind::Vtable) => {
                        let t = self.targets.intern_outside(
                            Outside::Vtable(target_ref.place),
                            Space::Vtable,
                            false,
                            self.names,
                            inst,
                        )?;
                        self.relro.words.push(Word { at: word, target: t, addend: r.addend });
                    }
                    Some(RefKind::Func) => {
                        let t = self.targets.intern_outside(
                            Outside::Func(target_ref.place),
                            Space::Func,
                            true,
                            self.names,
                            inst,
                        )?;
                        self.relro.words.push(Word { at: word, target: t, addend: r.addend });
                    }
                    Some(RefKind::Data) => {
                        if self.data_relro_off.get(to_idx(target_ref.place)).copied().flatten().is_none() {
                            queue.push(target_ref.place);
                        }
                        pending_data_words.push((word, target_ref.place, r.addend));
                    }
                    Some(RefKind::Tls) | None => {
                        return Err(refuse(Some(inst), "tls reloc in data"));
                    }
                }
            }
        }

        for &(word, target_row, addend) in pending_data_words.iter() {
            let off = self
                .data_relro_off
                .get(to_idx(target_row))
                .copied()
                .flatten()
                .ok_or_else(|| refuse(Some(inst), "missing data relro offset"))?;
            self.relro.words.push(Word {
                at: word,
                target: OWN_RELRO,
                addend: i64::from(off).wrapping_add(addend),
            });
        }

        self.data_relro_off
            .get(to_idx(root_row))
            .copied()
            .flatten()
            .ok_or_else(|| refuse(Some(inst), "missing root data relro offset"))
    }

    /// Resolves a CFG target edge `(target_lir_blk, args)` to an `MBlock`, allocating a deferred
    /// edge-split block with `ParallelCopy` when the edge carries live block arguments.
    /// Must be called while the dominating branch block is still open so lazy constants dominate!
    fn resolve_deferred_edge(
        &mut self,
        target_lir_blk: Block,
        args: blaze_lir::op::Values<'_>,
        scratch_pairs: &mut Vec<(MReg, MReg), Alloc<'a>>,
    ) -> MBlock {
        let orig = at(&self.mblock_of_lir, target_lir_blk.index(), MBlock(0));
        self.build_edge_copy_pairs(target_lir_blk, args, scratch_pairs);
        if scratch_pairs.is_empty() {
            return orig;
        }
        let split = self.new_mblock(false, self.cur_weight);
        let start = u32_of(self.deferred_pairs.len()).unwrap_or(u32::MAX);
        let len = u16::try_from(scratch_pairs.len()).unwrap_or(u16::MAX);
        for &p in scratch_pairs.iter().take(usize::from(len)) {
            self.deferred_pairs.push(p);
        }
        self.deferred_splits.push(DeferredEdgeSplit {
            split_blk: split,
            target_blk: orig,
            copy_start: start,
            copy_len: len,
        });
        split
    }

    /// Emits `cmp scrut_reg, #val` (using `CmpImm` when representable, else `MovImm` + `CmpReg`).
    fn emit_switch_cmp(&mut self, scrut_reg: MReg, wide: bool, val: u64) -> u32 {
        if let Some((imm12, shift12)) = encode_add_sub_imm12(val) {
            let (_, vf) = self.push_flag_writer(
                MOp::CmpImm { rn: scrut_reg, imm12, shift12, wide },
                MInstFlags::NONE,
            );
            vf
        } else {
            let imm_v = self.materialize_int_const(val, wide);
            let (_, vf) = self.push_flag_writer(
                MOp::CmpReg { rn: scrut_reg, rm: MReg::V(imm_v), shift: None, wide },
                MInstFlags::NONE,
            );
            vf
        }
    }

    /// Emits `if scrut_reg == case_val { goto case_tgt } else { goto fall_tgt }` as the terminator
    /// of the current `MBlock`.
    fn emit_switch_eq_branch(
        &mut self,
        scrut_reg: MReg,
        wide: bool,
        case_val: u64,
        case_tgt: MBlock,
        fall_tgt: MBlock,
    ) {
        if case_val == 0 {
            self.push_op(
                MOp::Cbz { rt: scrut_reg, target: case_tgt, wide },
                MInstFlags::TERMINATOR,
            );
            self.push_op(MOp::B { target: fall_tgt }, MInstFlags::TERMINATOR);
        } else {
            let vf = self.emit_switch_cmp(scrut_reg, wide, case_val);
            let br = self.push_op(
                MOp::BCond { cond: MCond::Eq, target: case_tgt },
                MInstFlags::TERMINATOR,
            );
            self.mfunc.set_vflag_use(br, vf);
            self.push_op(MOp::B { target: fall_tgt }, MInstFlags::TERMINATOR);
        }
    }

    /// Lowers `Op::Switch(scrut, default_tgt, cases)` iteratively (zero recursion) into an SSA
    /// compare-branch chain (<= 4 cases) or `O(log N)` binary-search dispatch tree (> 4 cases),
    /// handling all target edge `ParallelCopy` arguments cleanly.
    fn lower_switch(
        &mut self,
        scrut: Value,
        default_tgt: blaze_lir::op::Target<'_>,
        cases: blaze_lir::op::Cases<'_>,
        scratch_pairs: &mut Vec<(MReg, MReg), Alloc<'a>>,
    ) {
        if self.view.ty(scrut) == Ty::I128 {
            self.lower_switch_i128(scrut, default_tgt, cases, scratch_pairs);
            return;
        }
        let scrut_bits = int_bits(self.view.ty(scrut));
        let wide = scrut_bits == 64;

        // Constant scrutinee fast path: branch directly to the matching case or default.
        if let Some(const_scrut) = self.const_u64.get(scrut.index()).copied().flatten() {
            let masked_scrut = const_scrut & bit_mask(scrut_bits);
            let mut chosen = default_tgt;
            for (words, tgt) in cases {
                let mut val = 0u64;
                for &w in words.iter().rev() {
                    val = (val << 32) | u64::from(w);
                }
                if val == masked_scrut {
                    chosen = tgt;
                    break;
                }
            }
            let dst_mblk = at(&self.mblock_of_lir, chosen.block.index(), MBlock(0));
            self.build_edge_copy_pairs(chosen.block, chosen.args, scratch_pairs);
            if !scratch_pairs.is_empty() {
                self.mfunc.push_parallel_copy(scratch_pairs, MInstFlags::NONE);
            }
            self.push_op(MOp::B { target: dst_mblk }, MInstFlags::TERMINATOR);
            return;
        }

        // Materialize scrutinee & all deferred edge-copy arguments in the dominating `cur_blk`
        // BEFORE splitting into dispatch blocks so every SSA value strictly dominates its uses!
        let scrut_reg = self.get_val(scrut, false);
        let mut resolved: Vec<(u64, MBlock), Alloc<'a>> = Vec::new_in(self.alloc);
        for (words, tgt) in cases {
            let mut val = 0u64;
            for &w in words.iter().rev() {
                val = (val << 32) | u64::from(w);
            }
            let mblk = self.resolve_deferred_edge(tgt.block, tgt.args, scratch_pairs);
            resolved.push((val, mblk));
        }

        if resolved.is_empty() {
            let def_mblk = at(&self.mblock_of_lir, default_tgt.block.index(), MBlock(0));
            self.build_edge_copy_pairs(default_tgt.block, default_tgt.args, scratch_pairs);
            if !scratch_pairs.is_empty() {
                self.mfunc.push_parallel_copy(scratch_pairs, MInstFlags::NONE);
            }
            self.push_op(MOp::B { target: def_mblk }, MInstFlags::TERMINATOR);
            return;
        }

        let def_mblk = self.resolve_deferred_edge(default_tgt.block, default_tgt.args, scratch_pairs);
        resolved.sort_unstable_by_key(|&(v, _)| v);

        // Iterative worklist `(blk, lo, hi)` for binary search (`hi - lo > 4`) + linear leaf (`<= 4`).
        // The first item runs in the already-open `self.cur_blk`; the last emitted block stays open
        // for `select_mfunc_ssa` to call `end_block(self.cur_blk)`.
        let mut work: Vec<(MBlock, usize, usize), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut cur_lo = 0usize;
        let mut cur_hi = resolved.len();
        loop {
            if cur_hi - cur_lo <= 4 {
                for k in cur_lo..cur_hi {
                    let Some(&(cv, ctgt)) = resolved.get(k) else {
                        continue;
                    };
                    let is_last = k + 1 == cur_hi;
                    let fall_blk = if is_last {
                        def_mblk
                    } else {
                        self.new_mblock(false, self.cur_weight)
                    };
                    self.emit_switch_eq_branch(scrut_reg, wide, cv, ctgt, fall_blk);
                    if !is_last {
                        self.mfunc.end_block(self.cur_blk);
                        self.mfunc.begin_block(fall_blk);
                        self.cur_blk = fall_blk;
                        self.live_cmp = None;
                    }
                }
            } else {
                let mid = cur_lo + (cur_hi - cur_lo) / 2;
                let pivot = resolved.get(mid).map_or(0, |&(v, _)| v);
                let lo_blk = self.new_mblock(false, self.cur_weight);
                let hi_blk = self.new_mblock(false, self.cur_weight);
                let vf = self.emit_switch_cmp(scrut_reg, wide, pivot);
                let br_lo = self.push_op(
                    MOp::BCond { cond: MCond::Lo, target: lo_blk },
                    MInstFlags::TERMINATOR,
                );
                self.mfunc.set_vflag_use(br_lo, vf);
                self.push_op(MOp::B { target: hi_blk }, MInstFlags::TERMINATOR);
                work.push((hi_blk, mid, cur_hi));
                work.push((lo_blk, cur_lo, mid));
            }

            let Some((next_blk, next_lo, next_hi)) = work.pop() else {
                break;
            };
            self.mfunc.end_block(self.cur_blk);
            self.mfunc.begin_block(next_blk);
            self.cur_blk = next_blk;
            self.live_cmp = None;
            cur_lo = next_lo;
            cur_hi = next_hi;
        }
    }

    fn lower_fp_to_int(&mut self, inst: usize, op: Op, a: Value, res_ty: Ty) {
        let double = self.view.ty(a) == Ty::F64;
        let dst_bits = int_bits(res_ty);
        let signed = op == Op::FptoSi;
        let ar = self.get_val(a, false);
        let int_wide = dst_bits == 64;
        let fp_op = if signed {
            FpAluOp::Fcvtzs { int_wide }
        } else {
            FpAluOp::Fcvtzu { int_wide }
        };

        if dst_bits == 32 || dst_bits == 64 {
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.push_op(
                MOp::FpAlu { op: fp_op, dst: MReg::V(dst), rn: ar, rm: None, ra: None, double },
                MInstFlags::NONE,
            );
            return;
        }

        // Narrow saturating float -> `I8`/`I16`: convert to 32-bit, clamp to range, mask to width.
        let raw = self.mfunc.alloc_vreg(VRegClass::Gpr32);
        self.push_op(
            MOp::FpAlu { op: fp_op, dst: MReg::V(raw), rn: ar, rm: None, ra: None, double },
            MInstFlags::NONE,
        );
        if !signed {
            let max_u: u64 = (1u64 << dst_bits) - 1;
            let max_v = self.materialize_int_const(max_u, false);
            let (_, vf) = self.push_flag_writer(
                MOp::CmpReg { rn: MReg::V(raw), rm: MReg::V(max_v), shift: None, wide: false },
                MInstFlags::NONE,
            );
            let dst = self.alloc_res_vreg(inst, res_ty);
            let cs = self.push_op(
                MOp::Csel {
                    dst: MReg::V(dst),
                    rn: MReg::V(max_v),
                    rm: MReg::V(raw),
                    cond: MCond::Hi,
                    wide: false,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(cs, vf);
        } else {
            let max_s: u64 = (1u64 << (dst_bits - 1)) - 1;
            let min_s: u64 = (0u64.wrapping_sub(1u64 << (dst_bits - 1))) & 0xffff_ffff;
            let max_v = self.materialize_int_const(max_s, false);
            let min_v = self.materialize_int_const(min_s, false);
            let (_, vf1) = self.push_flag_writer(
                MOp::CmpReg { rn: MReg::V(raw), rm: MReg::V(max_v), shift: None, wide: false },
                MInstFlags::NONE,
            );
            let hi_clamped = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            let cs1 = self.push_op(
                MOp::Csel {
                    dst: MReg::V(hi_clamped),
                    rn: MReg::V(max_v),
                    rm: MReg::V(raw),
                    cond: MCond::Gt,
                    wide: false,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(cs1, vf1);
            let (_, vf2) = self.push_flag_writer(
                MOp::CmpReg { rn: MReg::V(hi_clamped), rm: MReg::V(min_v), shift: None, wide: false },
                MInstFlags::NONE,
            );
            let clamped = self.mfunc.alloc_vreg(VRegClass::Gpr32);
            let cs2 = self.push_op(
                MOp::Csel {
                    dst: MReg::V(clamped),
                    rn: MReg::V(min_v),
                    rm: MReg::V(hi_clamped),
                    cond: MCond::Lt,
                    wide: false,
                },
                MInstFlags::NONE,
            );
            self.mfunc.set_vflag_use(cs2, vf2);
            let dst = self.alloc_res_vreg(inst, res_ty);
            self.emit_narrow_mask(MReg::V(dst), MReg::V(clamped), dst_bits);
        }
    }

    fn lower_bulk(
        &mut self,
        op: Op,
        flags: Flags,
        dst_val: Value,
        src_val: Value,
        len_val: Value,
    ) -> Result<(), TierError> {
        let is_vol = flags.contains(Flags::VOLATILE);
        let const_len = self.const_u64.get(len_val.index()).copied().flatten();
        if const_len == Some(0) {
            return Ok(());
        }

        match op {
            Op::Memcpy => {
                if !is_vol
                    && let Some(n) = const_len
                    && n <= INLINE_BULK_MAX
                {
                    let dst_r = self.get_val(dst_val, false);
                    let src_r = self.get_val(src_val, false);
                    return self.emit_const_memcpy_regs(dst_r, src_r, n);
                }
                self.emit_c_bulk_call("memcpy", dst_val, src_val, len_val)?;
            }
            Op::Memmove => {
                if !is_vol
                    && let Some(n) = const_len
                    && n <= INLINE_MEMMOVE_MAX
                {
                    // Overlap-safe: load ALL chunks into fresh SSA VRegs BEFORE storing any!
                    let dst_r = self.get_val(dst_val, false);
                    let src_r = self.get_val(src_val, false);
                    let mut pieces: [(i32, MemSize, VReg); 6] =
                        [(0, MemSize::B8, VReg(0)); 6];
                    let mut n_pieces = 0usize;
                    let mut off: i32 = 0;
                    let mut rem = n;
                    while rem >= 16 && n_pieces < 6 {
                        let tmp = self.mfunc.alloc_vreg(VRegClass::Vec128);
                        self.push_op(
                            MOp::Ldr {
                                dst: MReg::V(tmp),
                                addr: AddrMode::BaseOff { base: src_r, off },
                                size: MemSize::Q128,
                                signed: false,
                            },
                            MInstFlags::MEM_READ,
                        );
                        set(&mut pieces, n_pieces, (off, MemSize::Q128, tmp));
                        n_pieces += 1;
                        off += 16;
                        rem -= 16;
                    }
                    for (bytes, sz, cls) in [
                        (8u64, MemSize::X64, VRegClass::Gpr64),
                        (4, MemSize::W32, VRegClass::Gpr32),
                        (2, MemSize::H16, VRegClass::Gpr32),
                        (1, MemSize::B8, VRegClass::Gpr32),
                    ] {
                        if rem >= bytes && n_pieces < 6 {
                            let tmp = self.mfunc.alloc_vreg(cls);
                            self.push_op(
                                MOp::Ldr {
                                    dst: MReg::V(tmp),
                                    addr: AddrMode::BaseOff { base: src_r, off },
                                    size: sz,
                                    signed: false,
                                },
                                MInstFlags::MEM_READ,
                            );
                            set(&mut pieces, n_pieces, (off, sz, tmp));
                            n_pieces += 1;
                            off += i32::try_from(bytes).unwrap_or(0);
                            rem -= bytes;
                        }
                    }
                    for &(poff, sz, tmp) in pieces.iter().take(n_pieces) {
                        self.push_op(
                            MOp::Str {
                                src: MReg::V(tmp),
                                addr: AddrMode::BaseOff { base: dst_r, off: poff },
                                size: sz,
                            },
                            MInstFlags::MEM_WRITE,
                        );
                    }
                    return Ok(());
                }
                self.emit_c_bulk_call("memmove", dst_val, src_val, len_val)?;
            }
            _ => {
                // `Memset`
                if !is_vol
                    && let Some(n) = const_len
                    && n <= INLINE_BULK_MAX
                {
                    let dst_r = self.get_val(dst_val, false);
                    let byte_const = self.const_u64.get(src_val.index()).copied().flatten();
                    let is_zero = byte_const == Some(0);
                    let mut off: i32 = 0;
                    let mut rem = n;

                    let mut v_fill: Option<VReg> = None;
                    if rem >= 16 {
                        let vf = self.mfunc.alloc_vreg(VRegClass::Vec128);
                        if is_zero {
                            self.push_op(
                                MOp::VecAlu {
                                    op: VecAluOp::Zero,
                                    dst: MReg::V(vf),
                                    rn: MReg::vec(0),
                                    rm: None,
                                    elem_size: 0,
                                },
                                MInstFlags::REMATERIALIZABLE,
                            );
                        } else {
                            let br = self.get_val(src_val, true);
                            self.push_op(
                                MOp::VecAlu {
                                    op: VecAluOp::DupGen,
                                    dst: MReg::V(vf),
                                    rn: br,
                                    rm: None,
                                    elem_size: 0,
                                },
                                MInstFlags::NONE,
                            );
                        }
                        v_fill = Some(vf);
                        while rem >= 16 {
                            self.push_op(
                                MOp::Str {
                                    src: MReg::V(vf),
                                    addr: AddrMode::BaseOff { base: dst_r, off },
                                    size: MemSize::Q128,
                                },
                                MInstFlags::MEM_WRITE,
                            );
                            off += 16;
                            rem -= 16;
                        }
                    }

                    if rem > 0 {
                        let g_fill: MReg = if is_zero {
                            MReg::XZR
                        } else if rem == 1 {
                            self.get_val(src_val, true)
                        } else if let Some(vf) = v_fill {
                            let g = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                            self.push_op(
                                MOp::VecAlu {
                                    op: VecAluOp::UmovLane { lane: 0 },
                                    dst: MReg::V(g),
                                    rn: MReg::V(vf),
                                    rm: None,
                                    elem_size: 3,
                                },
                                MInstFlags::NONE,
                            );
                            MReg::V(g)
                        } else if let Some(b_c) = byte_const {
                            let rep = (b_c & 0xff).wrapping_mul(0x0101_0101_0101_0101);
                            MReg::V(self.materialize_int_const(rep, true))
                        } else {
                            let br = self.get_val(src_val, true);
                            let ones = self.materialize_int_const(0x0101_0101_0101_0101, true);
                            let g = self.mfunc.alloc_vreg(VRegClass::Gpr64);
                            self.push_op(
                                MOp::Mul { dst: MReg::V(g), rn: br, rm: MReg::V(ones), wide: true },
                                MInstFlags::NONE,
                            );
                            MReg::V(g)
                        };

                        for (bytes, sz) in [
                            (8u64, MemSize::X64),
                            (4, MemSize::W32),
                            (2, MemSize::H16),
                            (1, MemSize::B8),
                        ] {
                            if rem >= bytes {
                                self.push_op(
                                    MOp::Str {
                                        src: g_fill,
                                        addr: AddrMode::BaseOff { base: dst_r, off },
                                        size: sz,
                                    },
                                    MInstFlags::MEM_WRITE,
                                );
                                off += i32::try_from(bytes).unwrap_or(0);
                                rem -= bytes;
                            }
                        }
                    }
                    return Ok(());
                }
                self.emit_c_bulk_call("memset", dst_val, src_val, len_val)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_call(
        &mut self,
        inst: usize,
        callee: CallCallee,
        sig_id: SigId,
        args: blaze_lir::op::Values<'_>,
        res_ty: Ty,
        invoke: Option<(Block, Block)>,
        scratch_pairs: &mut Vec<(MReg, MReg), Alloc<'a>>,
    ) -> Result<(), TierError> {
        let direct_target = match callee {
            CallCallee::Direct(callee_ref) => {
                let ref_rec = self.view.reference(callee_ref);
                Some(self.targets.intern_outside_func(ref_rec.place, self.names)?)
            }
            CallCallee::Indirect(_) => None,
        };
        let indirect_reg = match callee {
            CallCallee::Indirect(callee_ptr) => Some(self.get_val(callee_ptr, false)),
            CallCallee::Direct(_) => None,
        };
        let sig = self.view.sig(sig_id);

        let call_fast = matches!(callee, CallCallee::Direct(_))
            && is_internal_fast_call(self.opts.internal_fast_calls, inst);

        // 0. Copy `ParamKind::Byval` arguments into this function's reserved outgoing byval area
        //    (`[SP + byval_base_off + ...]`) BEFORE storing stacked args or touching arg regs,
        //    because copies > `INLINE_BULK_MAX` invoke C `memcpy`.
        let mut byval_ptrs: Vec<(usize, MReg), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut byval_cur = self.byval_base_off;
        for (idx, (arg_val, param)) in args.iter().zip(sig.params.iter()).enumerate() {
            if ParamKind::from_byte(param.kind) != Some(ParamKind::Byval) {
                continue;
            }
            let Some(facts) = self.view.facts(param) else {
                return Err(refuse(Some(inst), "byval without size/align"));
            };
            let align = 1u32 << u32::from(facts.align);
            let copy_off = align_up_u32(byval_cur, align).ok_or(TierError::TooLarge {
                tier: TIER,
                what: "outgoing byval copies",
            })?;
            byval_cur = copy_off.checked_add(facts.size).ok_or(TierError::TooLarge {
                tier: TIER,
                what: "outgoing byval copies",
            })?;
            let dst_v = self.mfunc.alloc_vreg(VRegClass::Gpr64);
            self.emit_sp_plus_off(MReg::V(dst_v), copy_off);
            let src_r = self.get_val(arg_val, false);
            self.emit_const_memcpy_regs(MReg::V(dst_v), src_r, u64::from(facts.size))?;
            byval_ptrs.push((idx, MReg::V(dst_v)));
        }

        let mut walk = AbiWalk::new(self.opts.machine.os, call_fast);
        scratch_pairs.clear();

        // 1. Store stack arguments first (at `[SP + out_off]`), collect register args for ParallelCopy.
        for (idx, (arg_val, param)) in args.iter().zip(sig.params.iter()).enumerate() {
            let Some(loc) = walk.next_param(param) else {
                return Err(refuse(Some(inst), "call argument placement overflow"));
            };
            let byval_reg = if ParamKind::from_byte(param.kind) == Some(ParamKind::Byval) {
                byval_ptrs.iter().find(|&&(k, _)| k == idx).map(|&(_, r)| r)
            } else {
                None
            };
            match loc {
                AbiLoc::Gpr(r) => {
                    let sr = byval_reg.unwrap_or_else(|| self.get_val(arg_val, true));
                    scratch_pairs.push((MReg::gpr(r), sr));
                }
                AbiLoc::Fp(v) => {
                    let sr = self.get_val(arg_val, false);
                    scratch_pairs.push((MReg::vec(v), sr));
                }
                AbiLoc::Stack { off, size } => {
                    let sr = byval_reg.unwrap_or_else(|| self.get_val(arg_val, !size.is_fp_or_vec()));
                    let addr = self.addr_sp_off(off, size);
                    self.push_op(MOp::Str { src: sr, addr, size }, MInstFlags::MEM_WRITE);
                }
            }
        }

        // 2. Simultaneous register argument setup via `ParallelCopy`.
        if !scratch_pairs.is_empty() {
            self.mfunc.push_parallel_copy(scratch_pairs, MInstFlags::NONE);
        }

        // 3. Emit `Bl` / `GotSlot` / indirect `Blr` with exact `CallClobbers`.
        //    Invokes and indirect calls always use full `C_ABI_ARM64` clobbers so unwinding
        //    (libunwind + personality routine) never clobbers a caller-saved live value.
        let call_mi = if let Some(target_id) = direct_target {
            let clob = if invoke.is_some() {
                CallClobbers::C_ABI_ARM64
            } else {
                lookup_lir_call_clobbers(
                    self.opts.call_clobbers,
                    u32_of(inst).unwrap_or(NONE_U32),
                )
            };
            self.emit_direct_call_site(target_id, clob)
        } else if let Some(callee_reg) = indirect_reg {
            self.has_calls = true;
            self.live_cmp = None;
            self.call_clobbers_seq.push(CallClobbers::C_ABI_ARM64);
            self.push_op(
                MOp::Blr { target: callee_reg },
                MInstFlags::CALL.union(MInstFlags::PINNED),
            )
        } else {
            return Err(refuse(Some(inst), "missing call target"));
        };

        // 4. Attach side-band `MNoteKind::Unwind` row when unwinding is active in this function.
        if let Some((_, unwind_blk)) = invoke {
            let action = match self.view.block_kind(unwind_blk) {
                BlockKind::Cleanup => blaze_code::unwind::Action::Cleanup,
                BlockKind::Catch => blaze_code::unwind::Action::Catch,
                BlockKind::Terminate => blaze_code::unwind::Action::Terminate,
                BlockKind::Normal | BlockKind::Cold => {
                    return Err(refuse(Some(inst), "invoke unwind target is not a landing block"));
                }
            };
            let unwind_mblk = at(&self.mblock_of_lir, unwind_blk.index(), MBlock(0));
            self.attach_unwind_row(call_mi, Some((unwind_mblk, action)))?;
        } else if self.has_landing_blocks
            && !self.is_cleanup_block.get(to_idx(self.cur_lir_blk)).copied().unwrap_or(false)
        {
            // Ordinary calls outside cleanup blocks get a `Continue` row; calls inside cleanup
            // blocks intentionally get NO unwind row (Rust panic-in-drop aborts).
            self.attach_unwind_row(call_mi, None)?;
        }

        // 5. Copy return value(s) from `X0..X7` / `V0..V7` into SSA destination `VReg`(s).
        if sig.rets.len() >= 2 {
            scratch_pairs.clear();
            let mut post_narrow: [(VReg, VReg, u32); 8] = [(VReg(0), VReg(0), 64); 8];
            let mut n_narrow = 0usize;
            let mut gpr_r: u8 = 0;
            let mut fp_r: u8 = 0;
            for (field, r) in sig.rets.iter().enumerate() {
                let ty = Ty(r.ty);
                let Some(dst_id) = self.get_or_alloc_call_ret_vreg(inst, field) else {
                    return Err(refuse(Some(inst), "multi-return call vreg allocation failed"));
                };
                let dst_v = VReg(dst_id);
                if is_scalar_fp(ty) {
                    scratch_pairs.push((MReg::V(dst_v), MReg::vec(fp_r)));
                    fp_r += 1;
                } else {
                    let bits = int_bits(ty);
                    if bits == 64 {
                        scratch_pairs.push((MReg::V(dst_v), MReg::gpr(gpr_r)));
                    } else {
                        let raw_v = self.mfunc.alloc_vreg(VRegClass::Gpr32);
                        scratch_pairs.push((MReg::V(raw_v), MReg::gpr(gpr_r)));
                        if n_narrow < post_narrow.len() {
                            set(&mut post_narrow, n_narrow, (dst_v, raw_v, bits));
                            n_narrow += 1;
                        }
                    }
                    gpr_r += 1;
                }
            }
            if !scratch_pairs.is_empty() {
                self.mfunc.push_parallel_copy(scratch_pairs, MInstFlags::NONE);
            }
            for &(dst_v, raw_v, bits) in post_narrow.iter().take(n_narrow) {
                if bits == 32 {
                    self.push_op(
                        MOp::Mov { dst: MReg::V(dst_v), src: MReg::V(raw_v), wide: false },
                        MInstFlags::NONE,
                    );
                } else {
                    self.emit_narrow_mask(MReg::V(dst_v), MReg::V(raw_v), bits);
                }
            }
        } else if res_ty != Ty::NONE {
            let dst = self.alloc_res_vreg(inst, res_ty);
            if is_scalar_fp(res_ty) {
                self.push_op(
                    MOp::FpAlu {
                        op: FpAluOp::Mov,
                        dst: MReg::V(dst),
                        rn: MReg::vec(0),
                        rm: None,
                        ra: None,
                        double: res_ty == Ty::F64,
                    },
                    MInstFlags::NONE,
                );
            } else {
                let bits = int_bits(res_ty);
                if bits < 32 {
                    self.emit_narrow_mask(MReg::V(dst), MReg::p(PReg::X0), bits);
                } else {
                    self.push_op(
                        MOp::Mov { dst: MReg::V(dst), src: MReg::p(PReg::X0), wide: bits == 64 },
                        MInstFlags::NONE,
                    );
                }
            }
        }

        // 6. Terminate `Op::Invoke` / `Op::InvokeIndirect` with branch to its normal successor.
        if let Some((normal_blk, _)) = invoke {
            let normal_mblk = at(&self.mblock_of_lir, normal_blk.index(), MBlock(0));
            self.push_op(MOp::B { target: normal_mblk }, MInstFlags::TERMINATOR);
        }
        Ok(())
    }
}

/// Counts every value use across the whole LIR function (operands, jump/br args, calls, returns).
fn compute_use_counts<'a>(view: &View<'_>, alloc: Alloc<'a>) -> Vec<u32, Alloc<'a>> {
    let n = view.len();
    let mut uses = vec_filled(n, 0u32, alloc);
    let mut inc = |v: Value| {
        if let Some(slot) = uses.get_mut(v.index()) {
            *slot = slot.saturating_add(1);
        }
    };

    for inst in 0..n {
        let op = view.op(inst);
        let mut o = view.operands(inst);
        match op {
            Op::Param
            | Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Null
            | Op::Uninit
            | Op::SlotAddr
            | Op::SlotLive
            | Op::SlotDead
            | Op::DataAddr
            | Op::FuncAddr
            | Op::Unreachable
            | Op::Trap => {}
            Op::Neg
            | Op::Not
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Bswap
            | Op::Bitrev
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Bitcast
            | Op::PtrAddr
            | Op::Expose
            | Op::FromExposed
            | Op::Freeze
            | Op::Assume
            | Op::Fneg
            | Op::Fabs
            | Op::Fpext
            | Op::Fptrunc
            | Op::UitoFp
            | Op::SitoFp
            | Op::FptoUi
            | Op::FptoSi
            | Op::Project
            | Op::Splat
            | Op::Resume => inc(o.value()),
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Umulhi
            | Op::Smulhi
            | Op::Udiv
            | Op::Sdiv
            | Op::Urem
            | Op::Srem
            | Op::And
            | Op::Or
            | Op::Xor
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
            | Op::UaddOverflow
            | Op::UsubOverflow
            | Op::SaddOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow
            | Op::PtrAdd
            | Op::PtrWithAddr
            | Op::Fadd
            | Op::Fsub
            | Op::Fmul
            | Op::Fdiv => {
                inc(o.value());
                inc(o.value());
            }
            Op::Icmp => {
                let _ = o.int_cond();
                inc(o.value());
                inc(o.value());
            }
            Op::Fcmp => {
                let _ = o.float_cond();
                inc(o.value());
                inc(o.value());
            }
            Op::Select | Op::Memcpy | Op::Memmove | Op::Memset | Op::Fma | Op::Fmuladd => {
                inc(o.value());
                inc(o.value());
                inc(o.value());
            }
            Op::Load => inc(o.value()),
            Op::Store => {
                inc(o.value());
                inc(o.value());
            }
            Op::Call => {
                let _ = o.reference();
                let _ = o.sig();
                for a in o.values().iter() {
                    inc(a);
                }
            }
            Op::CallIndirect => {
                inc(o.value());
                let _ = o.sig();
                for a in o.values().iter() {
                    inc(a);
                }
            }
            Op::Invoke => {
                let _ = o.reference();
                let _ = o.sig();
                let _ = o.block();
                let _ = o.block();
                for a in o.values().iter() {
                    inc(a);
                }
            }
            Op::InvokeIndirect => {
                inc(o.value());
                let _ = o.sig();
                let _ = o.block();
                let _ = o.block();
                for a in o.values().iter() {
                    inc(a);
                }
            }
            Op::TargetIntrinsic => {
                let _ = o.intrinsic();
                for a in o.values().iter() {
                    inc(a);
                }
            }
            Op::Jump => {
                let tgt = o.target();
                for a in tgt.args.iter() {
                    inc(a);
                }
            }
            Op::Br => {
                inc(o.value());
                let t1 = o.target();
                for a in t1.args.iter() {
                    inc(a);
                }
                let t2 = o.target();
                for a in t2.args.iter() {
                    inc(a);
                }
            }
            Op::Switch => {
                let scrut = o.value();
                inc(scrut);
                let def_tgt = o.target();
                for a in def_tgt.args.iter() {
                    inc(a);
                }
                for (_, case_tgt) in o.cases(scrut) {
                    for a in case_tgt.args.iter() {
                        inc(a);
                    }
                }
            }
            Op::Return => {
                for v in o.values().iter() {
                    inc(v);
                }
            }
            _ => {}
        }
    }
    uses
}

/// Scans same-block instruction patterns (`Madd`, `Msub`, shifted ALU, `AddExt`/`SubExt`,
/// `LogicalNot`, `Ubfx`, `BaseOff`/`ScaledIdx`/`SlotAddr` addressing modes, fused `Icmp`/`Fcmp`,
/// boolean condition wrapper peeling, and hardware-masked shift-amount mask elision).
fn match_patterns(
    view: &View<'_>,
    inst_block: &[u32],
    const_u64: &[Option<u64>],
    use_count: &mut [u32],
    slot_off: &[u32],
    skip_inst: &mut [bool],
    cmp_all_fused: &mut [bool],
    pattern: &mut [SelPattern],
    alloc: Alloc<'_>,
) {
    let n = view.len();

    // Pass A1: Peel same-block boolean wrappers on `Br` / `Select`, skip dead wrappers, and
    // identify `Icmp` / single-cond `Fcmp` whose EVERY live use is a same-block `Br` or `Select`.
    let mut bool_peel_child = vec_filled(n, NONE_U32, alloc);
    let mut fused_cmp_uses = vec_filled(n, 0u32, alloc);
    for inst in 0..n {
        let b = at(inst_block, inst, 0);
        let cond_opt = match view.op(inst) {
            Op::Br => Some(view.operands(inst).value()),
            Op::Select => {
                let mut o = view.operands(inst);
                let c = o.value();
                let t = o.value();
                let f = o.value();
                if t != c && f != c { Some(c) } else { None }
            }
            _ => None,
        };
        let Some(c) = cond_opt else {
            continue;
        };
        let ci = c.index();
        if inst_block.get(ci) != Some(&b) {
            continue;
        }
        let mut cur = ci;
        for _ in 0..4 {
            let Some((next, _inv, mid_opt)) =
                peel_bool_cond_step(view, const_u64, inst_block, cur, b)
            else {
                break;
            };
            if let Some(mid) = mid_opt {
                set(&mut bool_peel_child, cur, u32_of(mid).unwrap_or(NONE_U32));
                set(&mut bool_peel_child, mid, u32_of(next).unwrap_or(NONE_U32));
            } else {
                set(&mut bool_peel_child, cur, u32_of(next).unwrap_or(NONE_U32));
            }
            cur = next;
        }
        if cur != ci {
            sat_dec_u32(use_count, ci, 1);
            sat_inc_u32(use_count, cur, 1);
        }
        if inst_block.get(cur) == Some(&b) {
            sat_inc_u32(&mut fused_cmp_uses, cur, 1);
        }
    }
    for _ in 0..8 {
        let mut changed = false;
        for u in 0..n {
            let child = at(&bool_peel_child, u, NONE_U32);
            if child != NONE_U32 && !at(skip_inst, u, true) && at(use_count, u, 0) == 0 {
                set(skip_inst, u, true);
                let ci = to_idx(child);
                if ci < n {
                    sat_dec_u32(use_count, ci, 1);
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for inst in 0..n {
        if at(skip_inst, inst, false) {
            continue;
        }
        let op = view.op(inst);
        let is_fusible_cmp = match op {
            Op::Icmp => true,
            Op::Fcmp => float_cond_to_mcond(view.operands(inst).float_cond()).is_some(),
            _ => false,
        };
        let uc = at(use_count, inst, 0);
        if is_fusible_cmp && uc > 0 && at(&fused_cmp_uses, inst, 0) == uc {
            set(cmp_all_fused, inst, true);
        }
    }

    // Pass A2: Shift-amount mask/cast elision for hardware-masked AArch64 variable shifts/rotates.
    // Marks any `Op::And` / `Op::Trunc` / `Op::Zext` whose live uses are ALL peeled shift counts
    // as `skip_inst` BEFORE Pass B1/B2 so dead `and #0x3f` / `and #0x1f` are never emitted.
    let mut shift_peel_child = vec_filled(n, NONE_U32, alloc);
    for inst in 0..n {
        if at(skip_inst, inst, false) {
            continue;
        }
        let op = view.op(inst);
        if !matches!(op, Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr) {
            continue;
        }
        let shift_ty = ty_at(view, inst);
        if !is_scalar_int_or_ptr(shift_ty) || shift_ty == Ty::PTR {
            continue;
        }
        let bits = int_bits(shift_ty);
        if bits < 8 {
            continue;
        }
        let mut o = view.operands(inst);
        let _lhs = o.value();
        let rhs = o.value();
        let ri = rhs.index();
        if at(const_u64, ri, None).is_some() {
            continue;
        }
        let req_mask = u64::from(bits - 1);
        let mut cur = ri;
        for _ in 0..4 {
            let Some(next) = peel_shift_mask_step(view, const_u64, cur, req_mask) else {
                break;
            };
            set(&mut shift_peel_child, cur, u32_of(next).unwrap_or(NONE_U32));
            cur = next;
        }
        if cur != ri {
            sat_dec_u32(use_count, ri, 1);
            sat_inc_u32(use_count, cur, 1);
        }
    }
    for _ in 0..4 {
        let mut changed = false;
        for u in 0..n {
            let child = at(&shift_peel_child, u, NONE_U32);
            if child != NONE_U32 && !at(skip_inst, u, true) && at(use_count, u, 0) == 0 {
                set(skip_inst, u, true);
                let ci = to_idx(child);
                if ci < n {
                    sat_dec_u32(use_count, ci, 1);
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Pass B1: Match `Load`/`Store` addressing modes BEFORE ALU/`PtrAdd` patterns so both
    // single-use and multi-use (`<= 4`, e.g. `Load` + `Store` to the same hash slot) same-block
    // `PtrAdd(base, const_off)` / `PtrAdd(base, Shl/Mul(idx, scale))` / `PtrAdd(base, idx)` fold
    // directly into `AddrMode::BaseOff` / `AddrMode::ScaledIdx` with zero extra `add`!
    const MAX_ADDR_FOLD_USES: usize = 4;
    const MAX_ADDR_FOLD_USES_U32: u32 = 4;
    let mut slot_folded = vec_filled(n, 0u32, alloc);
    let mut ptr_use_len = vec_filled(n, 0u8, alloc);
    let mut ptr_uses = vec_filled(n, [NONE_U32; MAX_ADDR_FOLD_USES], alloc);

    for inst in 0..n {
        if at(skip_inst, inst, false) {
            continue;
        }
        let op = view.op(inst);
        if !matches!(op, Op::Load | Op::Store) {
            continue;
        }
        let b = at(inst_block, inst, 0);
        let res_ty = ty_at(view, inst);
        let mut o = view.operands(inst);
        let (ptr, off, mem_ty) = if op == Op::Load {
            (o.value(), o.offset(), res_ty)
        } else {
            let v = o.value();
            let p = o.value();
            (p, o.offset(), view.ty(v))
        };
        let Some(size) = mem_size_for_ty(mem_ty) else {
            continue;
        };
        let pi = ptr.index();
        if pi >= n || at(skip_inst, pi, true) {
            continue;
        }
        match view.op(pi) {
            Op::SlotAddr => {
                let slot = view.operands(pi).slot();
                let total = i64::from(at(slot_off, slot.index(), 0)) + i64::from(off);
                if let Some(sp_off) = fits_base_off_checked(total, size) {
                    set(pattern, inst, SelPattern::MemSp { off: sp_off });
                    sat_inc_u32(&mut slot_folded, pi, 1);
                }
            }
            Op::PtrAdd => {
                let pi_uc = at(use_count, pi, 0);
                if inst_block.get(pi) == Some(&b)
                    && at(pattern, pi, SelPattern::None) == SelPattern::None
                    && pi_uc >= 1
                    && pi_uc <= MAX_ADDR_FOLD_USES_U32
                {
                    let slot = usize::from(at(&ptr_use_len, pi, 0));
                    if slot < MAX_ADDR_FOLD_USES {
                        if let Some(row) = ptr_uses.get_mut(pi)
                            && let Some(cell) = row.get_mut(slot)
                        {
                            *cell = u32_of(inst).unwrap_or(NONE_U32);
                        }
                        if let Some(len_slot) = ptr_use_len.get_mut(pi) {
                            *len_slot = len_slot.saturating_add(1);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    for pi in 0..n {
        let uc = at(use_count, pi, 0);
        if !at(skip_inst, pi, true)
            && view.op(pi) == Op::SlotAddr
            && uc > 0
            && at(&slot_folded, pi, 0) == uc
        {
            set(skip_inst, pi, true);
        }
    }

    for pi in 0..n {
        let k_u8 = at(&ptr_use_len, pi, 0);
        let k = usize::from(k_u8);
        let k_u32 = u32::from(k_u8);
        if k == 0
            || at(skip_inst, pi, true)
            || at(pattern, pi, SelPattern::None) != SelPattern::None
            || k_u32 != at(use_count, pi, 0)
        {
            continue;
        }
        let b = at(inst_block, pi, 0);
        let ptr_row = at(&ptr_uses, pi, [NONE_U32; MAX_ADDR_FOLD_USES]);
        let mut po = view.operands(pi);
        let base = po.value();
        let by = po.value();
        let by_bits = int_bits(view.ty(by));

        if let Some(c) = at(const_u64, by.index(), None) {
            let delta = sign_extend_u64(c, by_bits);
            let mut comb_offs = [0i32; MAX_ADDR_FOLD_USES];
            let mut all_fit = true;
            for j in 0..k {
                let u = to_idx(at(&ptr_row, j, NONE_U32));
                if let Some((off, size)) = load_store_off_size(view, u)
                    && let Some(total) = delta.checked_add(i64::from(off))
                    && let Some(comb_off) = fits_base_off_checked(total, size)
                {
                    set(&mut comb_offs, j, comb_off);
                } else {
                    all_fit = false;
                    break;
                }
            }
            if all_fit {
                for j in 0..k {
                    let u = to_idx(at(&ptr_row, j, NONE_U32));
                    set(
                        pattern,
                        u,
                        SelPattern::MemBaseOff { base: base.0, off: at(&comb_offs, j, 0) },
                    );
                }
                set(skip_inst, pi, true);
                sat_inc_u32(use_count, base.index(), k_u32 - 1);
                sat_dec_u32(use_count, by.index(), 1);
            }
        } else if by_bits == 64 {
            let mut all_zero_off = true;
            let mut sizes = [MemSize::B8; MAX_ADDR_FOLD_USES];
            for j in 0..k {
                let u = to_idx(at(&ptr_row, j, NONE_U32));
                if let Some((off, size)) = load_store_off_size(view, u)
                    && off == 0
                {
                    set(&mut sizes, j, size);
                } else {
                    all_zero_off = false;
                    break;
                }
            }
            if !all_zero_off {
                continue;
            }

            let bi = by.index();
            let mut folded_scale: Option<(Value, u8, Value)> = None;
            if bi < n
                && !at(skip_inst, bi, true)
                && inst_block.get(bi) == Some(&b)
                && at(use_count, bi, 0) >= 1
            {
                match view.op(bi) {
                    Op::Shl => {
                        let mut so = view.operands(bi);
                        let idx = so.value();
                        let sh_v = so.value();
                        if idx.index() < n
                            && (at(const_u64, idx.index(), None).is_some()
                                || !at(skip_inst, idx.index(), true))
                            && let Some(kv) = at(const_u64, sh_v.index(), None)
                        {
                            let sh = trunc_u8(kv & 63);
                            let all_compat = sizes
                                .iter()
                                .take(k)
                                .all(|sz| sh == 0 || sh == sz.scale_shift());
                            if all_compat {
                                folded_scale = Some((idx, sh, sh_v));
                            }
                        }
                    }
                    Op::Mul => {
                        let mut mo = view.operands(bi);
                        let a = mo.value();
                        let bv = mo.value();
                        let scale_cand = if let Some(cv) = at(const_u64, bv.index(), None)
                            && at(const_u64, a.index(), None).is_none()
                        {
                            Some((a, cv, bv))
                        } else if let Some(cv) = at(const_u64, a.index(), None)
                            && at(const_u64, bv.index(), None).is_none()
                        {
                            Some((bv, cv, a))
                        } else {
                            None
                        };
                        if let Some((idx, cv, c_val)) = scale_cand
                            && idx.index() < n
                            && !at(skip_inst, idx.index(), true)
                            && cv != 0
                            && (cv & (cv - 1)) == 0
                            && cv <= 16
                        {
                            let sh = u8_of(cv.trailing_zeros()).unwrap_or(0);
                            let all_compat = sizes
                                .iter()
                                .take(k)
                                .all(|sz| sh == 0 || sh == sz.scale_shift());
                            if all_compat {
                                folded_scale = Some((idx, sh, c_val));
                            }
                        }
                    }
                    _ => {}
                }
            }

            if let Some((idx, sh, c_val)) = folded_scale {
                for j in 0..k {
                    let u = to_idx(at(&ptr_row, j, NONE_U32));
                    set(
                        pattern,
                        u,
                        SelPattern::MemScaledIdx {
                            base: base.0,
                            idx: idx.0,
                            shift: sh,
                        },
                    );
                }
                set(skip_inst, pi, true);
                sat_inc_u32(use_count, base.index(), k_u32 - 1);
                sat_dec_u32(use_count, bi, 1);
                if at(use_count, bi, 0) == 0 {
                    set(skip_inst, bi, true);
                    sat_inc_u32(use_count, idx.index(), k_u32 - 1);
                    sat_dec_u32(use_count, c_val.index(), 1);
                } else {
                    sat_inc_u32(use_count, idx.index(), k_u32);
                }
            } else {
                for j in 0..k {
                    let u = to_idx(at(&ptr_row, j, NONE_U32));
                    set(
                        pattern,
                        u,
                        SelPattern::MemScaledIdx {
                            base: base.0,
                            idx: by.0,
                            shift: 0,
                        },
                    );
                }
                set(skip_inst, pi, true);
                sat_inc_u32(use_count, base.index(), k_u32 - 1);
                sat_inc_u32(use_count, by.index(), k_u32 - 1);
            }
        }
    }

    // Freeze `use_count` as immutable for Pass B2/B3 pattern matching.
    let use_count: &[u32] = use_count;

    // Helper closure: true iff producer `p` is single-use, same-block, and not already skipped.
    let can_fold = |p: usize, cur_b: u32, skip: &[bool]| -> bool {
        p < n
            && use_count.get(p) == Some(&1)
            && inst_block.get(p) == Some(&cur_b)
            && skip.get(p) == Some(&false)
    };

    // Pass B2: Match ALU & standalone `PtrAdd` fusions in forward order.
    for inst in 0..n {
        if at(skip_inst, inst, false) {
            continue;
        }
        let b = at(inst_block, inst, 0);
        let op = view.op(inst);
        let res_ty = ty_at(view, inst);
        if is_q128_ty(res_ty) {
            continue;
        }
        let bits = int_bits(res_ty);

        match op {

            Op::Add if bits == 32 || bits == 64 => {
                let mut o = view.operands(inst);
                let lhs = o.value();
                let rhs = o.value();
                // 1. `Add(a, Mul(b, c))` or `Add(Mul(b, c), a)` -> `Madd`!
                let mut matched = false;
                for (m_cand, acc) in [(rhs, lhs), (lhs, rhs)] {
                    let mi = m_cand.index();
                    if view.op(mi) == Op::Mul && can_fold(mi, b, skip_inst) {
                        let mut mo = view.operands(mi);
                        let ma = mo.value();
                        let mb = mo.value();
                        set(pattern, inst, SelPattern::Madd { a: ma.0, b: mb.0, acc: acc.0 });
                        set(skip_inst, mi, true);
                        matched = true;
                        break;
                    }
                }
                if matched {
                    continue;
                }
                // 2. `Add(a, Shl/Lshr/Ashr(b, k))` (commutative) -> shifted `AddReg`!
                for (s_cand, other) in [(rhs, lhs), (lhs, rhs)] {
                    if let Some((src, sh)) =
                        match_shifted_operand(view, s_cand, bits, b, const_u64, use_count, inst_block, skip_inst)
                    {
                        set(pattern, inst, SelPattern::ShiftedBin { lhs: other.0, rhs: src.0, shift: sh });
                        set(skip_inst, s_cand.index(), true);
                        matched = true;
                        break;
                    }
                }
                if matched {
                    continue;
                }
                // 3. `Add(a, Zext/Sext(b))` (64-bit, commutative) -> `AddExt`!
                if bits == 64 {
                    for (e_cand, other) in [(rhs, lhs), (lhs, rhs)] {
                        if at(const_u64, other.index(), None).is_some() {
                            continue;
                        }
                        if let Some((src, ext)) =
                            match_ext_operand(view, e_cand, b, use_count, inst_block, skip_inst)
                        {
                            set(pattern, inst, SelPattern::ExtAddSub { lhs: other.0, rhs: src.0, ext });
                            set(skip_inst, e_cand.index(), true);
                            break;
                        }
                    }
                }
            }

            Op::Sub if bits == 32 || bits == 64 => {
                let mut o = view.operands(inst);
                let lhs = o.value();
                let rhs = o.value();
                let ri = rhs.index();
                // 1. `Sub(acc, Mul(a, b))` -> `Msub`!
                if view.op(ri) == Op::Mul && can_fold(ri, b, skip_inst) {
                    let mut mo = view.operands(ri);
                    let ma = mo.value();
                    let mb = mo.value();
                    set(pattern, inst, SelPattern::Msub { a: ma.0, b: mb.0, acc: lhs.0 });
                    set(skip_inst, ri, true);
                    continue;
                }
                // 2. `Sub(a, Shl/Lshr/Ashr(b, k))` -> shifted `SubReg`!
                if let Some((src, sh)) =
                    match_shifted_operand(view, rhs, bits, b, const_u64, use_count, inst_block, skip_inst)
                {
                    set(pattern, inst, SelPattern::ShiftedBin { lhs: lhs.0, rhs: src.0, shift: sh });
                    set(skip_inst, ri, true);
                    continue;
                }
                // 3. `Sub(a, Zext/Sext(b))` (64-bit) -> `SubExt`!
                if bits == 64
                    && at(const_u64, lhs.index(), None).is_none()
                    && let Some((src, ext)) =
                        match_ext_operand(view, rhs, b, use_count, inst_block, skip_inst)
                {
                    set(pattern, inst, SelPattern::ExtAddSub { lhs: lhs.0, rhs: src.0, ext });
                    set(skip_inst, ri, true);
                }
            }

            Op::PtrAdd => {
                let mut o = view.operands(inst);
                let ptr = o.value();
                let by = o.value();
                if int_bits(view.ty(by)) == 64
                    && let Some((src, sh)) =
                        match_shifted_operand(view, by, 64, b, const_u64, use_count, inst_block, skip_inst)
                {
                    set(pattern, inst, SelPattern::ShiftedBin { lhs: ptr.0, rhs: src.0, shift: sh });
                    set(skip_inst, by.index(), true);
                }
            }

            Op::And | Op::Or | Op::Xor if bits == 32 || bits == 64 => {
                let mut o = view.operands(inst);
                let lhs = o.value();
                let rhs = o.value();

                // 1. `And(Lshr(x, s), mask)` -> single `Ubfm` (`ubfx`)!
                if op == Op::And {
                    let mut matched_ubfx = false;
                    for (sh_cand, mask_cand) in [(lhs, rhs), (rhs, lhs)] {
                        let si = sh_cand.index();
                        if view.op(si) == Op::Lshr
                            && can_fold(si, b, skip_inst)
                            && let Some(mask_c) = at(const_u64, mask_cand.index(), None)
                        {
                            let m = mask_c & bit_mask(bits);
                            if m != 0 && (m & (m.wrapping_add(1))) == 0 {
                                let w = m.count_ones();
                                let mut so = view.operands(si);
                                let x = so.value();
                                let sh_v = so.value();
                                if let Some(s_c) = at(const_u64, sh_v.index(), None) {
                                    let s = trunc_u32(s_c) & (bits - 1);
                                    if s + w <= bits {
                                        set(
                                            pattern,
                                            inst,
                                            SelPattern::Ubfx {
                                                x: x.0,
                                                immr: u8_of(s).unwrap_or(0),
                                                imms: u8_of(s + w - 1).unwrap_or(0),
                                            },
                                        );
                                        set(skip_inst, si, true);
                                        matched_ubfx = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if matched_ubfx {
                        continue;
                    }
                }

                // 2. `And/Or/Xor(a, Not(b))` (commutative) -> `bic`/`orn`/`eon`!
                let mut matched_not = false;
                for (n_cand, other) in [(rhs, lhs), (lhs, rhs)] {
                    let ni = n_cand.index();
                    if view.op(ni) == Op::Not && can_fold(ni, b, skip_inst) {
                        let inner = view.operands(ni).value();
                        set(pattern, inst, SelPattern::LogicalNot { lhs: other.0, rhs: inner.0 });
                        set(skip_inst, ni, true);
                        matched_not = true;
                        break;
                    }
                }
                if matched_not {
                    continue;
                }

                // 3. `And/Or/Xor(a, Shl/Lshr/Ashr(b, k))` (commutative) -> shifted `LogicalReg`!
                for (s_cand, other) in [(rhs, lhs), (lhs, rhs)] {
                    if let Some((src, sh)) =
                        match_shifted_operand(view, s_cand, bits, b, const_u64, use_count, inst_block, skip_inst)
                    {
                        set(pattern, inst, SelPattern::ShiftedBin { lhs: other.0, rhs: src.0, shift: sh });
                        set(skip_inst, s_cand.index(), true);
                        break;
                    }
                }
            }

            _ => {}
        }
    }

    // Pass B3: Fuse single-use `Icmp(Eq | Ne, And(x, mask_const), 0)` into `SelPattern::IcmpTstImm`
    // (`Tbz`/`Tbnz` for single-bit masks, `TstImm` + `BCond`/`Csel` for general logical bitmasks)
    // and mark the dead `And` skipped so neither `and` nor an extra temp register is emitted!
    for inst in 0..n {
        if at(skip_inst, inst, false) || view.op(inst) != Op::Icmp {
            continue;
        }
        let b = at(inst_block, inst, 0);
        let mut o = view.operands(inst);
        let ic = o.int_cond();
        if !matches!(ic, IntCond::Eq | IntCond::Ne) {
            continue;
        }
        let mut a = o.value();
        let mut bv = o.value();
        if at(const_u64, a.index(), None).is_some() && at(const_u64, bv.index(), None).is_none() {
            core::mem::swap(&mut a, &mut bv);
        }
        if at(const_u64, bv.index(), None) != Some(0) {
            continue;
        }
        let ai = a.index();
        let op_bits = int_bits(view.ty(a));
        if op_bits < 8
            || ai >= n
            || view.op(ai) != Op::And
            || inst_block.get(ai) != Some(&b)
            || use_count.get(ai) != Some(&1)
            || at(skip_inst, ai, true)
            || at(pattern, ai, SelPattern::None) != SelPattern::None
        {
            continue;
        }
        let mut ao = view.operands(ai);
        let mut x = ao.value();
        let mut y = ao.value();
        if at(const_u64, x.index(), None).is_some() && at(const_u64, y.index(), None).is_none() {
            core::mem::swap(&mut x, &mut y);
        }
        if at(const_u64, x.index(), None).is_some() || at(skip_inst, x.index(), true) {
            continue;
        }
        let Some(mask_c) = at(const_u64, y.index(), None) else {
            continue;
        };
        let m = mask_c & bit_mask(op_bits);
        let cmp_w = if op_bits == 64 { 64 } else { 32 };
        if enc::logical_imm(m, cmp_w).is_some() {
            set(pattern, inst, SelPattern::IcmpTstImm { x: x.0, mask_val: y.0 });
            set(skip_inst, ai, true);
        }
    }
}

fn match_shifted_operand(
    view: &View<'_>,
    cand: Value,
    bits: u32,
    cur_b: u32,
    const_u64: &[Option<u64>],
    use_count: &[u32],
    inst_block: &[u32],
    skip_inst: &[bool],
) -> Option<(Value, MShift)> {
    let ci = cand.index();
    if ci >= view.len()
        || use_count.get(ci) != Some(&1)
        || inst_block.get(ci) != Some(&cur_b)
        || skip_inst.get(ci) != Some(&false)
    {
        return None;
    }
    let sop = view.op(ci);
    if !matches!(sop, Op::Shl | Op::Lshr | Op::Ashr) {
        return None;
    }
    let mut o = view.operands(ci);
    let src = o.value();
    let amt_v = o.value();
    let k = at(const_u64, amt_v.index(), None)?;
    let amt = u8_of(trunc_u32(k) & (bits - 1))?;
    let shift = match sop {
        Op::Shl => MShift::Lsl(amt),
        Op::Lshr => MShift::Lsr(amt),
        _ => MShift::Asr(amt),
    };
    Some((src, shift))
}

fn match_ext_operand(
    view: &View<'_>,
    cand: Value,
    cur_b: u32,
    use_count: &[u32],
    inst_block: &[u32],
    skip_inst: &[bool],
) -> Option<(Value, MExt)> {
    let ci = cand.index();
    if ci >= view.len()
        || use_count.get(ci) != Some(&1)
        || inst_block.get(ci) != Some(&cur_b)
        || skip_inst.get(ci) != Some(&false)
    {
        return None;
    }
    match view.op(ci) {
        Op::Zext => {
            let src = view.operands(ci).value();
            let ext = match view.ty(src) {
                Ty::I8 => MExt::Uxtb,
                Ty::I16 => MExt::Uxth,
                Ty::I32 => MExt::Uxtw,
                _ => return None,
            };
            Some((src, ext))
        }
        Op::Sext => {
            let src = view.operands(ci).value();
            if view.ty(src) == Ty::I32 {
                Some((src, MExt::Sxtw))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Lowers verified LIR into a complete SSA [`MFunc`] with `VReg`s, explicit `VFlag`s,
/// `ParallelCopy` edge moves, and attached `MNote` metadata.
fn select_mfunc_ssa<'a>(
    view: &View<'_>,
    names: &dyn Names,
    layout: &BlockLayout<'_>,
    opts: MIseOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<SelectOut<'a>, TierError> {
    let n_insts = view.len();
    let n_blocks = view.blocks();

    // 1. Compute max outgoing call stacked-argument bytes and outgoing `byval` area (16B aligned).
    let mut outgoing_args_bytes: u32 = 0;
    let mut outgoing_byval_bytes: u32 = 0;
    for inst in 0..n_insts {
        let op = view.op(inst);
        let (sig_id, call_fast) = match op {
            Op::Call => {
                let mut o = view.operands(inst);
                let _ = o.reference();
                (o.sig(), is_internal_fast_call(opts.internal_fast_calls, inst))
            }
            Op::CallIndirect => {
                let mut o = view.operands(inst);
                let _ = o.value();
                (o.sig(), false)
            }
            Op::Invoke => {
                let mut o = view.operands(inst);
                let _ = o.reference();
                (o.sig(), is_internal_fast_call(opts.internal_fast_calls, inst))
            }
            Op::InvokeIndirect => {
                let mut o = view.operands(inst);
                let _ = o.value();
                (o.sig(), false)
            }
            _ => continue,
        };
        let sig = view.sig(sig_id);
        let mut walk = AbiWalk::new(opts.machine.os, call_fast);
        let mut call_byval: u32 = 0;
        for p in sig.params {
            if ParamKind::from_byte(p.kind) == Some(ParamKind::Byval) {
                let Some(facts) = view.facts(p) else {
                    return Err(refuse(Some(inst), "byval without size/align"));
                };
                let align = 1u32 << u32::from(facts.align);
                let at = align_up_u32(call_byval, align).ok_or(TierError::TooLarge {
                    tier: TIER,
                    what: "outgoing byval copies",
                })?;
                call_byval = at.checked_add(facts.size).ok_or(TierError::TooLarge {
                    tier: TIER,
                    what: "outgoing byval copies",
                })?;
            }
            let _ = walk.next_param(p);
        }
        let need = walk.stack.aligned16().ok_or(TierError::TooLarge {
            tier: TIER,
            what: "outgoing call stack arguments",
        })?;
        if need > outgoing_args_bytes {
            outgoing_args_bytes = need;
        }
        let need_byval = align_up_u32(call_byval, 16).ok_or(TierError::TooLarge {
            tier: TIER,
            what: "outgoing byval copies",
        })?;
        if need_byval > outgoing_byval_bytes {
            outgoing_byval_bytes = need_byval;
        }
    }

    // 2. Reserve outgoing `byval` copies at `[outgoing_args_bytes .. byval_end)`, then pack local
    //    stack slots starting at `byval_end` -> `spill_base_off` (16B aligned).
    let byval_base_off = outgoing_args_bytes;
    let byval_end = byval_base_off
        .checked_add(outgoing_byval_bytes)
        .ok_or(TierError::TooLarge {
            tier: TIER,
            what: "outgoing byval copies",
        })?;
    let mut slot_off: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(view.slots.len(), alloc);
    let mut cursor = byval_end;
    for slot in view.slots {
        let align = 1u32 << u32::from(slot.align);
        let at = align_up_u32(cursor, align).ok_or(TierError::TooLarge {
            tier: TIER,
            what: "local stack slots",
        })?;
        slot_off.push(at);
        cursor = at.checked_add(slot.size).ok_or(TierError::TooLarge {
            tier: TIER,
            what: "local stack slots",
        })?;
    }
    let spill_base_off = align_up_u32(cursor, 16).ok_or(TierError::TooLarge {
        tier: TIER,
        what: "stack frame",
    })?;
    if spill_base_off >= FRAME_LIMIT / 2 {
        return Err(TierError::TooLarge { tier: TIER, what: "local stack frame" });
    }

    // 3. Populate per-instruction metadata (`inst_block`, constants, `use_count`, patterns).
    let mut inst_block = vec_filled(n_insts, 0u32, alloc);
    for b in 0..n_blocks {
        let b_u32 = u32_of(b).unwrap_or(0);
        for inst in view.block(Block(b_u32)) {
            set(&mut inst_block, inst, b_u32);
        }
    }

    let mut const_u64 = vec_filled(n_insts, None, alloc);
    let mut const_i128 = vec_filled(n_insts, None, alloc);
    let mut const_fp_bits = vec_filled(n_insts, None, alloc);
    let mut skip_inst = vec_filled(n_insts, false, alloc);
    for inst in 0..n_insts {
        let op = view.op(inst);
        let ty = ty_at(view, inst);
        let mut o = view.operands(inst);
        match op {
            Op::Iconst => {
                let raw = o.imm();
                if ty == Ty::I128 {
                    let s = raw.cast_signed();
                    let lo = i64::from(s).cast_unsigned();
                    let hi = if s < 0 { u64::MAX } else { 0 };
                    set(&mut const_i128, inst, Some((lo, hi)));
                } else {
                    let bits = int_bits(ty);
                    let val = if bits == 64 {
                        i64::from(raw.cast_signed()).cast_unsigned()
                    } else {
                        u64::from(raw) & bit_mask(bits)
                    };
                    set(&mut const_u64, inst, Some(val));
                }
                set(&mut skip_inst, inst, true);
            }
            Op::IconstWide => {
                let words = o.constant();
                let w0 = words.first().copied().unwrap_or(0);
                let w1 = words.get(1).copied().unwrap_or(0);
                let lo = u64::from(w0) | (u64::from(w1) << 32);
                if ty == Ty::I128 {
                    let w2 = words.get(2).copied().unwrap_or(0);
                    let w3 = words.get(3).copied().unwrap_or(0);
                    let hi = u64::from(w2) | (u64::from(w3) << 32);
                    set(&mut const_i128, inst, Some((lo, hi)));
                } else {
                    set(&mut const_u64, inst, Some(lo));
                }
                set(&mut skip_inst, inst, true);
            }
            Op::Null => {
                set(&mut const_u64, inst, Some(0));
                set(&mut skip_inst, inst, true);
            }
            Op::Fconst => {
                set(&mut const_fp_bits, inst, Some(u64::from(o.imm())));
                set(&mut skip_inst, inst, true);
            }
            Op::FconstWide => {
                let words = o.constant();
                let bits = u64::from(words[0]) | (u64::from(words[1]) << 32);
                set(&mut const_fp_bits, inst, Some(bits));
                set(&mut skip_inst, inst, true);
            }
            Op::Uninit => {
                set(&mut skip_inst, inst, true);
            }
            _ => {}
        }
    }

    let mut use_count = compute_use_counts(view, alloc);
    let mut cmp_all_fused = vec_filled(n_insts, false, alloc);
    let mut pattern = vec_filled(n_insts, SelPattern::None, alloc);
    match_patterns(
        view,
        &inst_block,
        &const_u64,
        &mut use_count,
        &slot_off,
        &mut skip_inst,
        &mut cmp_all_fused,
        &mut pattern,
        alloc,
    );

    // 4. Create `MFunc`, pre-allocate entry `MBlock`s and block-parameter `VReg`s.
    let fn_sig = view.sig(SigId(view.head.sig));
    let mut ret_gpr_mask: u32 = 0;
    let mut ret_fp_mask: u32 = 0;
    {
        let mut gr: u8 = 0;
        let mut fr: u8 = 0;
        for r in fn_sig.rets {
            let ty = Ty(r.ty);
            if is_scalar_fp(ty) {
                ret_fp_mask |= 1u32 << u32::from(fr);
                fr += 1;
            } else {
                ret_gpr_mask |= 1u32 << u32::from(gr);
                gr += 1;
            }
        }
    }

    let mfunc = MFunc::new(MFuncOpts {
        os: opts.machine.os,
        is_ssa: true,
        ret_live_gprs: DEFAULT_RET_LIVE_GPRS | ret_gpr_mask,
        alloc,
    });

    // Identify unwind landing blocks and all blocks reachable from `Cleanup | Terminate` via
    // ordinary edges (`is_cleanup_block`: calls inside cleanup code get NO unwind row).
    let mut has_landing_blocks = false;
    let mut is_cleanup_block = vec_filled(n_blocks, false, alloc);
    let mut cleanup_work: Vec<Block, Alloc<'a>> = Vec::new_in(alloc);
    for b in 0..n_blocks {
        let blk = Block(u32_of(b).unwrap_or(0));
        let kind = view.block_kind(blk);
        if kind.is_landing() {
            has_landing_blocks = true;
        }
        if matches!(kind, BlockKind::Cleanup | BlockKind::Terminate) {
            set(&mut is_cleanup_block, b, true);
            cleanup_work.push(blk);
        }
    }
    while let Some(blk) = cleanup_work.pop() {
        let term = view.terminator(blk);
        let mut o = view.operands(term);
        let mut push_next = |next: Block| {
            let nb = next.index();
            if nb < n_blocks && !at(&is_cleanup_block, nb, true) {
                set(&mut is_cleanup_block, nb, true);
                cleanup_work.push(next);
            }
        };
        match view.op(term) {
            Op::Jump => push_next(o.target().block),
            Op::Br => {
                let _ = o.value();
                push_next(o.target().block);
                push_next(o.target().block);
            }
            Op::Switch => {
                let scrut = o.value();
                push_next(o.target().block);
                for (_, tgt) in o.cases(scrut) {
                    push_next(tgt.block);
                }
            }
            Op::Invoke => {
                let _ = o.reference();
                let _ = o.sig();
                push_next(o.block());
            }
            Op::InvokeIndirect => {
                let _ = o.value();
                let _ = o.sig();
                push_next(o.block());
            }
            _ => {}
        }
    }

    let mut unreach_ord = vec_filled(view.len(), NONE_U32, alloc);
    let mut next_unreach: u32 = 0;
    for i in 0..view.len() {
        if view.op(i) == Op::Unreachable {
            set(&mut unreach_ord, i, next_unreach);
            next_unreach = next_unreach.saturating_add(1);
        }
    }
    // Order-independent static classification of `Ty::I128` values that live in `Q128` (`Vec128`)
    // rather than `(Gpr64, Gpr64)` scalar pairs, so non-RPO Ext-TSP lowering never disagrees across blocks.
    let mut i128_is_q128 = vec_filled(n_insts, false, alloc);
    for inst in 0..n_insts {
        if ty_at(view, inst) != Ty::I128 {
            continue;
        }
        let op = view.op(inst);
        if op == Op::Load {
            set(&mut i128_is_q128, inst, true);
        } else if op == Op::Bitcast {
            let mut o = view.operands(inst);
            if view.ty(o.value()) != Ty::I128 {
                set(&mut i128_is_q128, inst, true);
            }
        }
    }
    // Bounded fixed-point propagation through `Freeze`, `Bitcast`, and `Select`.
    for _ in 0..8 {
        let mut changed = false;
        for inst in 0..n_insts {
            if ty_at(view, inst) != Ty::I128 || at(&i128_is_q128, inst, false) {
                continue;
            }
            let mut o = view.operands(inst);
            let is_q = match view.op(inst) {
                Op::Freeze | Op::Bitcast => at(&i128_is_q128, o.value().index(), false),
                Op::Select => {
                    let _c = o.value();
                    let t = o.value();
                    let f = o.value();
                    at(&i128_is_q128, t.index(), false) && at(&i128_is_q128, f.index(), false)
                }
                _ => false,
            };
            if is_q {
                set(&mut i128_is_q128, inst, true);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut sel = Selector {
        view,
        names,
        opts,
        alloc,
        mfunc,
        targets: TargetTable::new(alloc),
        relro: RelroData::new(alloc),
        data_relro_off: vec_filled(view.datas.len(), None, alloc),
        trap_kinds: Vec::new_in(alloc),
        unwind_entries: Vec::new_in(alloc),
        unreach_ord,
        block_weights: Vec::with_capacity_in(n_blocks * 2, alloc),
        call_clobbers_seq: Vec::new_in(alloc),
        incoming_stack_params: Vec::new_in(alloc),
        mblock_of_lir: Vec::with_capacity_in(n_blocks, alloc),
        is_cleanup_block,
        inst_block,
        const_u64,
        const_i128,
        const_fp_bits,
        use_count,
        skip_inst,
        cmp_all_fused,
        pattern,
        val_vreg: vec_filled(n_insts, NONE_U32, alloc),
        i128_pair: vec_filled(n_insts, (NONE_U32, NONE_U32), alloc),
        i128_is_q128,
        ovf_pair: vec_filled(n_insts, (NONE_U32, NONE_U32), alloc),
        call_ret_off: vec_filled(n_insts, NONE_U32, alloc),
        call_ret_vregs: Vec::new_in(alloc),
        slot_off,
        byval_base_off,
        cur_blk: MBlock(0),
        cur_lir_blk: 0,
        cur_weight: 1,
        next_vflag: 0,
        live_cmp: None,
        deferred_splits: Vec::new_in(alloc),
        deferred_pairs: Vec::new_in(alloc),
        div_zero_trap_blk: None,
        div_ovf_trap_blk: None,
        has_calls: has_landing_blocks,
        has_landing_blocks,
    };

    for b in 0..n_blocks {
        let align16 = layout.is_loop_hdr.get(b).copied().unwrap_or(false);
        let weight = mra_weight_for_lir_block(layout, b);
        let mblk = sel.new_mblock(align16, weight);
        sel.mblock_of_lir.push(mblk);

        // Pre-allocate one SSA `VReg` (or `Gpr64` pair for `Ty::I128` block phis) per LIR `Op::Param`.
        let b_u32 = u32_of(b).unwrap_or(0);
        for p_inst in view.params(Block(b_u32)) {
            let ty = ty_at(view, p_inst);
            if ty == Ty::I128 {
                let _ = sel.alloc_i128_pair(p_inst);
            } else {
                let _ = sel.alloc_res_vreg(p_inst, ty);
            }
        }
    }

    // 5. Lower every LIR block in Ext-TSP `layout.order`.
    let mut scratch_pairs: Vec<(MReg, MReg), Alloc<'a>> = Vec::new_in(alloc);
    let n_ord = layout.order.len();
    for ord_idx in 0..n_ord {
        let Some(&lir_b) = layout.order.get(ord_idx) else {
            continue;
        };
        let next_lir = layout.order.get(ord_idx + 1).copied();
        let entry_mblk = at(&sel.mblock_of_lir, lir_b, MBlock(0));
        sel.cur_blk = entry_mblk;
        sel.cur_lir_blk = u32_of(lir_b).unwrap_or(0);
        sel.cur_weight = mra_weight_for_lir_block(layout, lir_b);
        sel.live_cmp = None;
        sel.mfunc.begin_block(entry_mblk);

        // Entry block: lower incoming register & stack parameters cleanly!
        if lir_b == 0 {
            let mut walk = AbiWalk::new(opts.machine.os, opts.internal_fast);
            let mut reg_param_copies: Vec<(MReg, MReg), Alloc<'a>> = Vec::new_in(alloc);
            let mut post_narrow: Vec<(VReg, VReg, u32), Alloc<'a>> = Vec::new_in(alloc);
            let mut stack_loads: Vec<(VReg, u32, MemSize, u32), Alloc<'a>> = Vec::new_in(alloc);

            for (p_inst, p_rec) in view.params(Block(0)).zip(fn_sig.params.iter()) {
                let ty = Ty(p_rec.ty);
                let Some(loc) = walk.next_param(p_rec) else {
                    return Err(refuse(Some(p_inst), "entry param placement overflow"));
                };
                if at(&sel.use_count, p_inst, 0) == 0 {
                    continue;
                }
                let dst_v = VReg(at(&sel.val_vreg, p_inst, NONE_U32));
                match loc {
                    AbiLoc::Gpr(r) => {
                        let bits = int_bits(ty);
                        if bits == 64 {
                            reg_param_copies.push((MReg::V(dst_v), MReg::gpr(r)));
                        } else {
                            let raw_v = sel.mfunc.alloc_vreg(VRegClass::Gpr32);
                            reg_param_copies.push((MReg::V(raw_v), MReg::gpr(r)));
                            post_narrow.push((dst_v, raw_v, bits));
                        }
                    }
                    AbiLoc::Fp(v) => {
                        reg_param_copies.push((MReg::V(dst_v), MReg::vec(v)));
                    }
                    AbiLoc::Stack { off, size } => {
                        let bits = if is_scalar_fp(ty) { 64 } else { int_bits(ty) };
                        stack_loads.push((dst_v, off, size, bits));
                    }
                }
            }

            if !reg_param_copies.is_empty() {
                sel.mfunc.push_parallel_copy(&reg_param_copies, MInstFlags::NONE);
            }
            for &(dst_v, raw_v, bits) in post_narrow.iter() {
                if bits == 32 {
                    sel.push_op(
                        MOp::Mov { dst: MReg::V(dst_v), src: MReg::V(raw_v), wide: false },
                        MInstFlags::NONE,
                    );
                } else {
                    sel.emit_narrow_mask(MReg::V(dst_v), MReg::V(raw_v), bits);
                }
            }
            for &(dst_v, caller_off, size, bits) in stack_loads.iter() {
                sel.incoming_stack_params.push(IncomingStackParam { caller_off, size });
                let needs_mask = !size.is_fp_or_vec() && bits < 64 && (bits < 32 || size == MemSize::X64);
                let load_dst = if needs_mask {
                    sel.mfunc.alloc_vreg(VRegClass::Gpr32)
                } else {
                    dst_v
                };
                // Placeholder `[SP, #0]` with `PINNED` — patched to `[SP, #frame_size + caller_off]`
                // in `insert_prologue_epilogue` once final `frame_size` is known!
                sel.push_op(
                    MOp::Ldr {
                        dst: MReg::V(load_dst),
                        addr: AddrMode::BaseOff { base: MReg::SP, off: 0 },
                        size,
                        signed: false,
                    },
                    MInstFlags::MEM_READ.union(MInstFlags::PINNED),
                );
                if needs_mask {
                    if bits == 32 {
                        sel.push_op(
                            MOp::Mov { dst: MReg::V(dst_v), src: MReg::V(load_dst), wide: false },
                            MInstFlags::NONE,
                        );
                    } else {
                        sel.emit_narrow_mask(MReg::V(dst_v), MReg::V(load_dst), bits);
                    }
                }
            }
        }

        let b_u32 = u32_of(lir_b).unwrap_or(0);
        let is_landing = view.block_kind(Block(b_u32)).is_landing();
        if is_landing {
            let params = view.params(Block(b_u32));
            if params.len() == 1 {
                let p_inst = params.start;
                if at(&sel.use_count, p_inst, 0) > 0 {
                    let dst_v = VReg(at(&sel.val_vreg, p_inst, NONE_U32));
                    sel.mfunc.push_parallel_copy(&[(MReg::V(dst_v), MReg::gpr(0))], MInstFlags::NONE);
                }
            }
        }

        let range = view.block(Block(b_u32));
        let start_inst = view.params(Block(b_u32)).end;
        for inst in start_inst..range.end {
            sel.lower_inst(inst, next_lir, &mut scratch_pairs)?;
        }

        sel.mfunc.end_block(sel.cur_blk);
        if is_landing {
            // Pin the first non-ParallelCopy instruction of the landing block so `mopt` never
            // folds away or empties the landing pad target.
            let lp_range = sel.mfunc.block_inst_range(entry_mblk);
            for mi in lp_range {
                if let Some(&op) = sel.mfunc.ops.get(mi)
                    && !matches!(op, MOp::ParallelCopy { .. })
                {
                    if let Some(fl) = sel.mfunc.flags.get_mut(mi) {
                        *fl = fl.union(MInstFlags::PINNED);
                    }
                    break;
                }
            }
        }
    }

    // 6. Emit deferred non-fallthrough `Br` edge-split blocks at the tail.
    let n_splits = sel.deferred_splits.len();
    for si in 0..n_splits {
        let Some(&sp) = sel.deferred_splits.get(si) else {
            continue;
        };
        sel.mfunc.begin_block(sp.split_blk);
        let ps = to_idx(sp.copy_start);
        let pe = ps.saturating_add(usize::from(sp.copy_len)).min(sel.deferred_pairs.len());
        if ps < pe {
            let start = u32_of(sel.mfunc.copy_pairs.len()).unwrap_or(u32::MAX);
            let len = u16::try_from(pe - ps).unwrap_or(u16::MAX);
            for k in ps..ps + usize::from(len) {
                if let Some(&pair) = sel.deferred_pairs.get(k) {
                    sel.mfunc.copy_pairs.push(pair);
                }
            }
            sel.mfunc.push_inst(MOp::ParallelCopy { start, len }, MInstFlags::NONE);
        }
        sel.mfunc.push_inst(MOp::B { target: sp.target_blk }, MInstFlags::TERMINATOR);
        sel.mfunc.end_block(sp.split_blk);
    }

    // 7. Emit lazily created cold div-by-zero / div-overflow trap blocks at the very tail.
    if let Some(blk) = sel.div_zero_trap_blk {
        sel.mfunc.begin_block(blk);
        let imm = u16_of(BRK_DIV_ZERO).unwrap_or(0xff00);
        sel.mfunc.push_inst(
            MOp::Brk { imm },
            MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP).union(MInstFlags::COLD),
        );
        sel.mfunc.end_block(blk);
    }
    if let Some(blk) = sel.div_ovf_trap_blk {
        sel.mfunc.begin_block(blk);
        let imm = u16_of(BRK_DIV_OVERFLOW).unwrap_or(0xff01);
        sel.mfunc.push_inst(
            MOp::Brk { imm },
            MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP).union(MInstFlags::COLD),
        );
        sel.mfunc.end_block(blk);
    }

    sel.mfunc.rebuild_cfg(alloc);
    if !sel.unwind_entries.is_empty() {
        add_unwind_cfg_edges(&mut sel.mfunc, &sel.unwind_entries, alloc);
    }

    Ok(SelectOut {
        mfunc: sel.mfunc,
        targets: sel.targets,
        relro: sel.relro,
        trap_kinds: sel.trap_kinds,
        unwind_entries: sel.unwind_entries,
        block_weights: sel.block_weights,
        call_clobbers_seq: sel.call_clobbers_seq,
        incoming_stack_params: sel.incoming_stack_params,
        spill_base_off,
        has_calls: sel.has_calls,
        has_landing_blocks: sel.has_landing_blocks,
        uses_fp: false,
        ret_gpr_mask,
        ret_fp_mask,
    })
}

/// Augments `func`'s CSR CFG (`block_succs_*` / `block_preds_*`) with exceptional `(call_blk -> lp_blk)`
/// edges derived from attached `MNoteKind::Unwind` notes so `mra` backward liveness preserves every
/// `VReg` live into an unwind landing block across `Op::Invoke` / `Op::InvokeIndirect`.
fn add_unwind_cfg_edges<'a>(
    func: &mut MFunc<'a>,
    unwind_entries: &[Option<(MBlock, blaze_code::unwind::Action)>],
    alloc: Alloc<'a>,
) {
    func.rebuild_cfg(alloc);
    let n_blocks = func.block_insts.len();
    if n_blocks == 0 || unwind_entries.is_empty() {
        return;
    }

    let n_notes = func.notes.len();
    let mut extra_edges: Vec<(u32, u32), Alloc<'a>> = Vec::new_in(alloc);
    for &blk in func.block_order.iter() {
        let b_idx = to_idx(blk.0);
        if b_idx >= n_blocks {
            continue;
        }
        let range = func.block_inst_range(blk);
        for i in range {
            let mut cur_note = func.note_idx.get(i).copied().unwrap_or(NO_NOTE);
            let mut steps = 0usize;
            while cur_note != NO_NOTE && steps < n_notes {
                let Some(&note) = func.notes.get(to_idx(cur_note)) else {
                    break;
                };
                if let MNoteKind::Unwind { unwind_idx } = note.kind
                    && let Some(&Some((lp_blk, _))) = unwind_entries.get(to_idx(unwind_idx))
                    && to_idx(lp_blk.0) < n_blocks
                {
                    let mut dup = false;
                    for &s in func.block_succs(blk) {
                        if s == lp_blk {
                            dup = true;
                            break;
                        }
                    }
                    if !dup {
                        for &(eb, elp) in extra_edges.iter() {
                            if eb == blk.0 && elp == lp_blk.0 {
                                dup = true;
                                break;
                            }
                        }
                    }
                    if !dup {
                        extra_edges.push((blk.0, lp_blk.0));
                    }
                }
                cur_note = note.next;
                steps += 1;
            }
        }
    }

    if extra_edges.is_empty() {
        return;
    }

    let mut succ_counts = vec_filled(n_blocks, 0u32, alloc);
    let mut pred_counts = vec_filled(n_blocks, 0u32, alloc);
    for b in 0..n_blocks {
        let Some(b_u32) = u32_of(b) else {
            return;
        };
        for &s in func.block_succs(MBlock(b_u32)) {
            let si = to_idx(s.0);
            if si < n_blocks {
                sat_inc_u32(&mut succ_counts, b, 1);
                sat_inc_u32(&mut pred_counts, si, 1);
            }
        }
    }
    for &(src, dst) in extra_edges.iter() {
        let bi = to_idx(src);
        let di = to_idx(dst);
        if bi < n_blocks && di < n_blocks {
            sat_inc_u32(&mut succ_counts, bi, 1);
            sat_inc_u32(&mut pred_counts, di, 1);
        }
    }

    let mut succ_off: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks + 1, alloc);
    let mut pred_off: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks + 1, alloc);
    let mut s_total: u32 = 0;
    let mut p_total: u32 = 0;
    for b in 0..n_blocks {
        succ_off.push(s_total);
        pred_off.push(p_total);
        s_total = s_total.saturating_add(at(&succ_counts, b, 0));
        p_total = p_total.saturating_add(at(&pred_counts, b, 0));
    }
    succ_off.push(s_total);
    pred_off.push(p_total);

    let mut succ_cur = vec_filled(n_blocks, 0u32, alloc);
    let mut pred_cur = vec_filled(n_blocks, 0u32, alloc);
    for b in 0..n_blocks {
        set(&mut succ_cur, b, at(&succ_off, b, 0));
        set(&mut pred_cur, b, at(&pred_off, b, 0));
    }

    let mut succ_data = vec_filled(to_idx(s_total), MBlock(0), alloc);
    let mut pred_data = vec_filled(to_idx(p_total), MBlock(0), alloc);

    for b in 0..n_blocks {
        let Some(b_u32) = u32_of(b) else {
            return;
        };
        let src_blk = MBlock(b_u32);
        for &dst_blk in func.block_succs(src_blk) {
            let di = to_idx(dst_blk.0);
            if di < n_blocks {
                let sp = to_idx(at(&succ_cur, b, 0));
                if sp < succ_data.len() {
                    set(&mut succ_data, sp, dst_blk);
                    sat_inc_u32(&mut succ_cur, b, 1);
                }
                let pp = to_idx(at(&pred_cur, di, 0));
                if pp < pred_data.len() {
                    set(&mut pred_data, pp, src_blk);
                    sat_inc_u32(&mut pred_cur, di, 1);
                }
            }
        }
    }
    for &(src, dst) in extra_edges.iter() {
        let bi = to_idx(src);
        let di = to_idx(dst);
        if bi < n_blocks && di < n_blocks {
            let sp = to_idx(at(&succ_cur, bi, 0));
            if sp < succ_data.len() {
                set(&mut succ_data, sp, MBlock(dst));
                sat_inc_u32(&mut succ_cur, bi, 1);
            }
            let pp = to_idx(at(&pred_cur, di, 0));
            if pp < pred_data.len() {
                set(&mut pred_data, pp, MBlock(src));
                sat_inc_u32(&mut pred_cur, di, 1);
            }
        }
    }

    func.block_succs_off = succ_off;
    func.block_succs_data = succ_data;
    func.block_preds_off = pred_off;
    func.block_preds_data = pred_data;
}

// -----------------------------------------------------------------------------
// Stage 4: IPRA Call-Site Clobber Binding (post pre-RA mopt/msched)
// -----------------------------------------------------------------------------

fn bind_call_clobbers<'a>(
    func: &MFunc<'_>,
    call_clobbers_seq: &[CallClobbers],
    alloc: Alloc<'a>,
) -> Result<Vec<(u32, CallClobbers), Alloc<'a>>, TierError> {
    let mut bound: Vec<(u32, CallClobbers), Alloc<'a>> =
        Vec::with_capacity_in(call_clobbers_seq.len(), alloc);
    let mut seq_idx = 0usize;
    for &blk in func.block_order.iter() {
        let range = func.block_inst_range(blk);
        for i in range {
            let is_call = func.flags.get(i).is_some_and(|fl| fl.contains(MInstFlags::CALL))
                || func.ops.get(i).is_some_and(|op| matches!(op, MOp::Bl | MOp::Blr { .. }));
            if is_call {
                let mut clob = call_clobbers_seq
                    .get(seq_idx)
                    .copied()
                    .unwrap_or(CallClobbers::C_ABI_ARM64);
                // Always reserve X16/X17 (linker veneers / GotSlot scratch) and X30 (LR).
                clob.clobbered = RegSet(
                    clob.clobbered.0 | (1u64 << 16) | (1u64 << 17) | (1u64 << 30),
                );
                let idx_u32 = u32_of(i)
                    .ok_or(TierError::TooLarge { tier: TIER, what: "instruction count" })?;
                bound.push((idx_u32, clob));
                seq_idx += 1;
            }
        }
    }
    bound.sort_unstable_by_key(|&(idx, _)| idx);
    Ok(bound)
}

// -----------------------------------------------------------------------------
// Stage 5: Exact Prologue / Epilogue Lowering & Incoming Stack-Arg Patching
// -----------------------------------------------------------------------------

struct FrameOut {
    unwind_frame: Option<blaze_code::unwind::Frame>,
    final_clobbers: CallClobbers,
}

#[derive(Clone, Copy)]
enum SaveEntry {
    GprPair { r0: u8, r1: u8, off: u32 },
    GprSingle { r: u8, off: u32 },
    FpPair { v0: u8, v1: u8, off: u32, size: MemSize },
    FpSingle { v: u8, off: u32, size: MemSize },
}

fn push_sp_adjust(func: &mut MFunc<'_>, is_sub: bool, bytes: u32) {
    let hi = bytes >> 12;
    let lo = bytes & 0xfff;
    let fl = MInstFlags::PINNED;
    if hi != 0 {
        let hi16 = u16_of(hi).unwrap_or(0);
        let op = if is_sub {
            MOp::SubImm {
                dst: MReg::SP,
                rn: MReg::SP,
                imm12: hi16,
                shift12: true,
                wide: true,
                sets_flags: false,
            }
        } else {
            MOp::AddImm {
                dst: MReg::SP,
                rn: MReg::SP,
                imm12: hi16,
                shift12: true,
                wide: true,
                sets_flags: false,
            }
        };
        func.push_inst(op, fl);
    }
    if lo != 0 {
        let lo16 = u16_of(lo).unwrap_or(0);
        let op = if is_sub {
            MOp::SubImm {
                dst: MReg::SP,
                rn: MReg::SP,
                imm12: lo16,
                shift12: false,
                wide: true,
                sets_flags: false,
            }
        } else {
            MOp::AddImm {
                dst: MReg::SP,
                rn: MReg::SP,
                imm12: lo16,
                shift12: false,
                wide: true,
                sets_flags: false,
            }
        };
        func.push_inst(op, fl);
    }
}

/// Emits a single `Ldr`/`Str` at `[SP + off]` using `X16` if `off` exceeds `BaseOff` range.
fn push_sp_mem_single(
    func: &mut MFunc<'_>,
    is_load: bool,
    reg: MReg,
    off: u32,
    size: MemSize,
    extra_flags: MInstFlags,
) {
    let fl = if is_load {
        MInstFlags::MEM_READ.union(extra_flags)
    } else {
        MInstFlags::MEM_WRITE.union(extra_flags)
    };
    if let Some(off32) = fits_base_off_checked(i64::from(off), size) {
        let op = if is_load {
            MOp::Ldr {
                dst: reg,
                addr: AddrMode::BaseOff { base: MReg::SP, off: off32 },
                size,
                signed: false,
            }
        } else {
            MOp::Str {
                src: reg,
                addr: AddrMode::BaseOff { base: MReg::SP, off: off32 },
                size,
            }
        };
        func.push_inst(op, fl);
        return;
    }

    let hi = off >> 12;
    let lo = off & 0xfff;
    let hi16 = u16_of(hi).unwrap_or(0);
    let lo16 = u16_of(lo).unwrap_or(0);
    if hi != 0 {
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X16),
                rn: MReg::SP,
                imm12: hi16,
                shift12: true,
                wide: true,
                sets_flags: false,
            },
            extra_flags,
        );
        if lo != 0 {
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::p(PReg::X16),
                    rn: MReg::p(PReg::X16),
                    imm12: lo16,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                extra_flags,
            );
        }
    } else {
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X16),
                rn: MReg::SP,
                imm12: lo16,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            extra_flags,
        );
    }
    let op = if is_load {
        MOp::Ldr {
            dst: reg,
            addr: AddrMode::BaseOff { base: MReg::p(PReg::X16), off: 0 },
            size,
            signed: false,
        }
    } else {
        MOp::Str {
            src: reg,
            addr: AddrMode::BaseOff { base: MReg::p(PReg::X16), off: 0 },
            size,
        }
    };
    func.push_inst(op, fl);
}

#[inline]
fn fits_pair_off(off: u32, size: MemSize) -> Option<i16> {
    let bytes = size.byte_size();
    if off % bytes != 0 {
        return None;
    }
    let scaled = off / bytes;
    if scaled <= 63 {
        i16::try_from(off).ok()
    } else {
        None
    }
}

fn emit_save_entries(func: &mut MFunc<'_>, entries: &[SaveEntry], is_restore: bool) {
    let fl = if is_restore {
        MInstFlags::MEM_READ.union(MInstFlags::PINNED)
    } else {
        MInstFlags::MEM_WRITE.union(MInstFlags::PINNED)
    };
    for &entry in entries {
        match entry {
            SaveEntry::GprPair { r0, r1, off } => {
                if let Some(off16) = fits_pair_off(off, MemSize::X64) {
                    let op = if is_restore {
                        MOp::Ldp {
                            dst0: MReg::gpr(r0),
                            dst1: MReg::gpr(r1),
                            base: MReg::SP,
                            off: off16,
                            size: MemSize::X64,
                            post_idx: false,
                            pre_idx: false,
                        }
                    } else {
                        MOp::Stp {
                            src0: MReg::gpr(r0),
                            src1: MReg::gpr(r1),
                            base: MReg::SP,
                            off: off16,
                            size: MemSize::X64,
                            post_idx: false,
                            pre_idx: false,
                        }
                    };
                    func.push_inst(op, fl);
                } else {
                    push_sp_mem_single(func, is_restore, MReg::gpr(r0), off, MemSize::X64, MInstFlags::PINNED);
                    push_sp_mem_single(
                        func,
                        is_restore,
                        MReg::gpr(r1),
                        off.saturating_add(8),
                        MemSize::X64,
                        MInstFlags::PINNED,
                    );
                }
            }
            SaveEntry::GprSingle { r, off } => {
                push_sp_mem_single(func, is_restore, MReg::gpr(r), off, MemSize::X64, MInstFlags::PINNED);
            }
            SaveEntry::FpPair { v0, v1, off, size } => {
                if let Some(off16) = fits_pair_off(off, size) {
                    let op = if is_restore {
                        MOp::Ldp {
                            dst0: MReg::vec(v0),
                            dst1: MReg::vec(v1),
                            base: MReg::SP,
                            off: off16,
                            size,
                            post_idx: false,
                            pre_idx: false,
                        }
                    } else {
                        MOp::Stp {
                            src0: MReg::vec(v0),
                            src1: MReg::vec(v1),
                            base: MReg::SP,
                            off: off16,
                            size,
                            post_idx: false,
                            pre_idx: false,
                        }
                    };
                    func.push_inst(op, fl);
                } else {
                    let step = size.byte_size();
                    push_sp_mem_single(func, is_restore, MReg::vec(v0), off, size, MInstFlags::PINNED);
                    push_sp_mem_single(
                        func,
                        is_restore,
                        MReg::vec(v1),
                        off.saturating_add(step),
                        size,
                        MInstFlags::PINNED,
                    );
                }
            }
            SaveEntry::FpSingle { v, off, size } => {
                push_sp_mem_single(func, is_restore, MReg::vec(v), off, size, MInstFlags::PINNED);
            }
        }
    }
}

/// Inserts exact callee-saved `Stp`/`Ldp` + `SP` prologue/epilogue and patches incoming stack-arg
/// loads to `[SP, #frame_size + caller_off]`, preserving every attached `MNote` chain.
#[allow(clippy::too_many_arguments)]
fn insert_prologue_epilogue<'a>(
    func: &mut MFunc<'a>,
    mra_out: &MRaOut,
    spill_base_off: u32,
    has_calls: bool,
    preserve_all_cold: bool,
    ret_gpr_mask: u32,
    ret_fp_mask: u32,
    incoming_stack_params: &[IncomingStackParam],
    alloc: Alloc<'a>,
) -> Result<FrameOut, TierError> {
    // 1. Collect registers to save/restore.
    let mut gprs_to_save: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
    let mut fps_to_save: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
    let mut saved_gpr_mask: u32 = 0;
    let mut saved_fp_mask: u32 = 0;

    if preserve_all_cold {
        let touched_gprs = trunc_u32(mra_out.clobbers.clobbered.0);
        let touched_fps = trunc_u32(mra_out.clobbers.clobbered.0 >> 32);
        for r in 0u8..=28u8 {
            if matches!(r, 16 | 17 | 18) {
                continue;
            }
            let bit = 1u32 << u32::from(r);
            if (touched_gprs & bit) != 0 && (ret_gpr_mask & bit) == 0 {
                gprs_to_save.push(r);
                saved_gpr_mask |= bit;
            }
        }
        if !has_calls {
            if (touched_gprs & (1u32 << 29)) != 0 {
                gprs_to_save.push(29);
                saved_gpr_mask |= 1u32 << 29;
            }
            if (touched_gprs & (1u32 << 30)) != 0 {
                gprs_to_save.push(30);
                saved_gpr_mask |= 1u32 << 30;
            }
        }
        for v in 0u8..=31u8 {
            let bit = 1u32 << u32::from(v);
            if (touched_fps & bit) != 0 && (ret_fp_mask & bit) == 0 {
                fps_to_save.push(v);
                saved_fp_mask |= bit;
            }
        }
    } else {
        let max_callee_gpr = if has_calls { 28u8 } else { 30u8 };
        for r in 19u8..=max_callee_gpr {
            let bit = 1u32 << u32::from(r);
            if (mra_out.used_callee_saved_gprs & bit) != 0 {
                gprs_to_save.push(r);
                saved_gpr_mask |= bit;
            }
        }
        for v in 8u8..=15u8 {
            let bit = 1u32 << u32::from(v);
            if (mra_out.used_callee_saved_fps & bit) != 0 {
                fps_to_save.push(v);
                saved_fp_mask |= bit;
            }
        }
    }

    // 2. Lay out save area above `[0 .. spill_base_off + mra_out.spill_bytes)`.
    let saved_base = spill_base_off
        .checked_add(mra_out.spill_bytes)
        .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
    let mut cursor = saved_base;
    let mut save_entries: Vec<SaveEntry, Alloc<'a>> = Vec::new_in(alloc);

    let fp_lr_off = if has_calls {
        let at = cursor;
        save_entries.push(SaveEntry::GprPair { r0: 29, r1: 30, off: at });
        saved_gpr_mask |= (1u32 << 29) | (1u32 << 30);
        cursor = cursor
            .checked_add(16)
            .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
        Some(at)
    } else {
        None
    };

    let mut gi = 0usize;
    while let (Some(&r0), Some(&r1)) = (gprs_to_save.get(gi), gprs_to_save.get(gi + 1)) {
        save_entries.push(SaveEntry::GprPair { r0, r1, off: cursor });
        cursor = cursor
            .checked_add(16)
            .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
        gi += 2;
    }
    if let Some(&r) = gprs_to_save.get(gi) {
        save_entries.push(SaveEntry::GprSingle { r, off: cursor });
        cursor = cursor
            .checked_add(8)
            .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
    }

    let fp_save_size = if preserve_all_cold { MemSize::Q128 } else { MemSize::D64 };
    if !fps_to_save.is_empty() {
        let fp_align = fp_save_size.byte_size();
        cursor = align_up_u32(cursor, fp_align)
            .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
        let mut fi = 0usize;
        while let (Some(&v0), Some(&v1)) = (fps_to_save.get(fi), fps_to_save.get(fi + 1)) {
            save_entries.push(SaveEntry::FpPair { v0, v1, off: cursor, size: fp_save_size });
            cursor = cursor
                .checked_add(fp_align.saturating_mul(2))
                .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
            fi += 2;
        }
        if let Some(&v) = fps_to_save.get(fi) {
            save_entries.push(SaveEntry::FpSingle { v, off: cursor, size: fp_save_size });
            cursor = cursor
                .checked_add(fp_align)
                .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
        }
    }

    let frame_size = align_up_u32(cursor, 16)
        .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
    if frame_size >= FRAME_LIMIT {
        return Err(TierError::TooLarge { tier: TIER, what: "stack frame (>= 16 MiB)" });
    }

    // Compute final caller-visible `CallClobbers`: subtract registers restored by our epilogue!
    let saved_full_mask = u64::from(saved_gpr_mask) | (u64::from(saved_fp_mask) << 32);
    let final_clobbers = CallClobbers {
        clobbered: RegSet(mra_out.clobbers.clobbered.0 & !saved_full_mask),
    };

    // Fast path: frameless leaf with zero incoming stack params -> nothing to rewrite!
    if frame_size == 0 && incoming_stack_params.is_empty() {
        return Ok(FrameOut { unwind_frame: None, final_clobbers });
    }

    // For calling functions (`has_calls`), emit Apple/LLVM canonical ARM64 compact-unwind frame:
    // `stp x29, x30, [sp, #-16]!`; `add x29, sp, #0`; `stp x(20+2k), x(19+2k), [sp, #-16]!`;
    // `stp d(9+2k), d(8+2k), [sp, #-16]!`; then `sub sp, sp, #local_and_cold_bytes`.
    if has_calls {
        let mut x_pairs: u8 = 0;
        let mut d_pairs: u8 = 0;
        let mut canonical_saved_gpr_mask: u32 = (1u32 << 29) | (1u32 << 30);
        let mut canonical_saved_fp_mask: u32 = 0;
        let mut cold_save_entries: Vec<SaveEntry, Alloc<'a>> = Vec::new_in(alloc);
        let mut cold_cursor = saved_base;

        for k in 0u8..5u8 {
            let lo = 19u8 + 2 * k;
            let hi = lo + 1;
            let bits = (1u32 << u32::from(lo)) | (1u32 << u32::from(hi));
            if (saved_gpr_mask & bits) != 0 {
                x_pairs |= 1u8 << k;
                canonical_saved_gpr_mask |= bits;
            }
        }
        for k in 0u8..4u8 {
            let lo = 8u8 + 2 * k;
            let hi = lo + 1;
            let bits = (1u32 << u32::from(lo)) | (1u32 << u32::from(hi));
            if (saved_fp_mask & bits) != 0 && !preserve_all_cold {
                d_pairs |= 1u8 << k;
                canonical_saved_fp_mask |= bits;
            }
        }
        if preserve_all_cold {
            let mut extra_gprs: Vec<u8, Alloc<'a>> = Vec::new_in(alloc);
            for &r in gprs_to_save.iter() {
                if (canonical_saved_gpr_mask & (1u32 << u32::from(r))) == 0 {
                    extra_gprs.push(r);
                    canonical_saved_gpr_mask |= 1u32 << u32::from(r);
                }
            }
            let mut gi = 0usize;
            while let (Some(&r0), Some(&r1)) = (extra_gprs.get(gi), extra_gprs.get(gi + 1)) {
                cold_save_entries.push(SaveEntry::GprPair { r0, r1, off: cold_cursor });
                cold_cursor = cold_cursor
                    .checked_add(16)
                    .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
                gi += 2;
            }
            if let Some(&r) = extra_gprs.get(gi) {
                cold_save_entries.push(SaveEntry::GprSingle { r, off: cold_cursor });
                cold_cursor = cold_cursor
                    .checked_add(8)
                    .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
            }
            if !fps_to_save.is_empty() {
                cold_cursor = align_up_u32(cold_cursor, 16)
                    .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
                let mut fi = 0usize;
                while let (Some(&v0), Some(&v1)) = (fps_to_save.get(fi), fps_to_save.get(fi + 1)) {
                    canonical_saved_fp_mask |= (1u32 << u32::from(v0)) | (1u32 << u32::from(v1));
                    cold_save_entries.push(SaveEntry::FpPair {
                        v0,
                        v1,
                        off: cold_cursor,
                        size: MemSize::Q128,
                    });
                    cold_cursor = cold_cursor
                        .checked_add(32)
                        .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
                    fi += 2;
                }
                if let Some(&v) = fps_to_save.get(fi) {
                    canonical_saved_fp_mask |= 1u32 << u32::from(v);
                    cold_save_entries.push(SaveEntry::FpSingle {
                        v,
                        off: cold_cursor,
                        size: MemSize::Q128,
                    });
                    cold_cursor = cold_cursor
                        .checked_add(16)
                        .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
                }
            }
        }

        let local_bytes = align_up_u32(cold_cursor, 16)
            .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
        let pair_count = 1u32
            .saturating_add(x_pairs.count_ones())
            .saturating_add(d_pairs.count_ones());
        let saved_pair_bytes = pair_count.saturating_mul(16);
        let total_drop = local_bytes
            .checked_add(saved_pair_bytes)
            .ok_or(TierError::TooLarge { tier: TIER, what: "stack frame" })?;
        if total_drop >= FRAME_LIMIT {
            return Err(TierError::TooLarge { tier: TIER, what: "stack frame (>= 16 MiB)" });
        }

        let canon_saved_full =
            u64::from(canonical_saved_gpr_mask) | (u64::from(canonical_saved_fp_mask) << 32);
        let canon_clobbers = CallClobbers {
            clobbered: RegSet(mra_out.clobbers.clobbered.0 & !canon_saved_full),
        };
        let unwind_frame = blaze_code::unwind::Frame { x_pairs, d_pairs };

        let mut framed = MFunc::new(MFuncOpts {
            os: func.os,
            is_ssa: false,
            ret_live_gprs: func.ret_live_gprs,
            alloc,
        });
        for &note in func.notes.iter() {
            framed.notes.push(note);
        }
        for &align in func.block_align16.iter() {
            framed.new_block(align);
        }

        let mut patched_stack_params = 0usize;
        for (ord_pos, &blk) in func.block_order.iter().enumerate() {
            framed.begin_block(blk);
            if ord_pos == 0 {
                // Canonical ARM64 unwind prologue:
                framed.push_inst(
                    MOp::Stp {
                        src0: MReg::p(PReg::FP),
                        src1: MReg::p(PReg::LR),
                        base: MReg::SP,
                        off: -16,
                        size: MemSize::X64,
                        post_idx: false,
                        pre_idx: true,
                    },
                    MInstFlags::MEM_WRITE.union(MInstFlags::PINNED),
                );
                framed.push_inst(
                    MOp::AddImm {
                        dst: MReg::p(PReg::FP),
                        rn: MReg::SP,
                        imm12: 0,
                        shift12: false,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::PINNED,
                );
                for k in 0u8..5u8 {
                    if (x_pairs & (1u8 << k)) != 0 {
                        let lo = 19u8 + 2 * k;
                        let hi = lo + 1;
                        framed.push_inst(
                            MOp::Stp {
                                src0: MReg::gpr(hi),
                                src1: MReg::gpr(lo),
                                base: MReg::SP,
                                off: -16,
                                size: MemSize::X64,
                                post_idx: false,
                                pre_idx: true,
                            },
                            MInstFlags::MEM_WRITE.union(MInstFlags::PINNED),
                        );
                    }
                }
                for k in 0u8..4u8 {
                    if (d_pairs & (1u8 << k)) != 0 {
                        let lo = 8u8 + 2 * k;
                        let hi = lo + 1;
                        framed.push_inst(
                            MOp::Stp {
                                src0: MReg::vec(hi),
                                src1: MReg::vec(lo),
                                base: MReg::SP,
                                off: -16,
                                size: MemSize::D64,
                                post_idx: false,
                                pre_idx: true,
                            },
                            MInstFlags::MEM_WRITE.union(MInstFlags::PINNED),
                        );
                    }
                }
                if local_bytes > 0 {
                    push_sp_adjust(&mut framed, true, local_bytes);
                }
                emit_save_entries(&mut framed, &cold_save_entries, false);
            }

            let range = func.block_inst_range(blk);
            for i in range {
                let Some(&op) = func.ops.get(i) else {
                    continue;
                };
                let fl = at(&func.flags, i, MInstFlags::NONE);
                let vdef = at(&func.vflag_def, i, NO_VFLAG);
                let vuse = at(&func.vflag_use, i, NO_VFLAG);
                let note = at(&func.note_idx, i, NO_NOTE);

                if ord_pos == 0
                    && fl.contains(MInstFlags::PINNED)
                    && let Some(&param) = incoming_stack_params.get(patched_stack_params)
                    && let MOp::Ldr {
                        dst,
                        addr: AddrMode::BaseOff { base: MReg::P(PReg::Sp), off: 0 },
                        size,
                        signed: false,
                    } = op
                {
                    if param.size == size {
                        let abs_off = total_drop
                            .checked_add(param.caller_off)
                            .ok_or(TierError::TooLarge { tier: TIER, what: "incoming stack param offset" })?;
                        push_sp_mem_single(&mut framed, true, dst, abs_off, size, MInstFlags::NONE);
                        patched_stack_params += 1;
                        continue;
                    }
                }

                if matches!(op, MOp::Ret) {
                    emit_save_entries(&mut framed, &cold_save_entries, true);
                    if local_bytes > 0 {
                        push_sp_adjust(&mut framed, false, local_bytes);
                    }
                    for rev in 0u8..4u8 {
                        let k = 3u8 - rev;
                        if (d_pairs & (1u8 << k)) != 0 {
                            let lo = 8u8 + 2 * k;
                            let hi = lo + 1;
                            framed.push_inst(
                                MOp::Ldp {
                                    dst0: MReg::vec(hi),
                                    dst1: MReg::vec(lo),
                                    base: MReg::SP,
                                    off: 16,
                                    size: MemSize::D64,
                                    post_idx: true,
                                    pre_idx: false,
                                },
                                MInstFlags::MEM_READ.union(MInstFlags::PINNED),
                            );
                        }
                    }
                    for rev in 0u8..5u8 {
                        let k = 4u8 - rev;
                        if (x_pairs & (1u8 << k)) != 0 {
                            let lo = 19u8 + 2 * k;
                            let hi = lo + 1;
                            framed.push_inst(
                                MOp::Ldp {
                                    dst0: MReg::gpr(hi),
                                    dst1: MReg::gpr(lo),
                                    base: MReg::SP,
                                    off: 16,
                                    size: MemSize::X64,
                                    post_idx: true,
                                    pre_idx: false,
                                },
                                MInstFlags::MEM_READ.union(MInstFlags::PINNED),
                            );
                        }
                    }
                    framed.push_inst(
                        MOp::Ldp {
                            dst0: MReg::p(PReg::FP),
                            dst1: MReg::p(PReg::LR),
                            base: MReg::SP,
                            off: 16,
                            size: MemSize::X64,
                            post_idx: true,
                            pre_idx: false,
                        },
                        MInstFlags::MEM_READ.union(MInstFlags::PINNED),
                    );
                }

                let new_inst = framed.push_inst(op, fl);
                if vdef != NO_VFLAG {
                    framed.set_vflag_def(new_inst, vdef);
                }
                if vuse != NO_VFLAG {
                    framed.set_vflag_use(new_inst, vuse);
                }
                if note != NO_NOTE
                    && let Some(slot) = framed.note_idx.get_mut(to_idx(new_inst.0))
                {
                    *slot = note;
                }
            }
            framed.end_block(blk);
        }

        framed.rebuild_cfg(alloc);
        *func = framed;
        return Ok(FrameOut { unwind_frame: Some(unwind_frame), final_clobbers: canon_clobbers });
    }

    // 3. Rebuild `MFunc` with prologue at entry, patched incoming stack loads, and epilogue before `Ret`.
    let mut framed = MFunc::new(MFuncOpts {
        os: func.os,
        is_ssa: false,
        ret_live_gprs: func.ret_live_gprs,
        alloc,
    });
    // Preserve every `MNote` node verbatim so `note_idx` links stay 100% intact!
    for &note in func.notes.iter() {
        framed.notes.push(note);
    }
    for &align in func.block_align16.iter() {
        framed.new_block(align);
    }

    let mut patched_stack_params = 0usize;
    for (ord_pos, &blk) in func.block_order.iter().enumerate() {
        framed.begin_block(blk);

        // Insert prologue at the start of entry block (`ord_pos == 0`).
        if ord_pos == 0 && frame_size > 0 {
            push_sp_adjust(&mut framed, true, frame_size);
            emit_save_entries(&mut framed, &save_entries, false);
            if let Some(fp_off) = fp_lr_off {
                let hi = fp_off >> 12;
                let lo = fp_off & 0xfff;
                if hi != 0 {
                    framed.push_inst(
                        MOp::AddImm {
                            dst: MReg::p(PReg::FP),
                            rn: MReg::SP,
                            imm12: u16_of(hi).unwrap_or(0),
                            shift12: true,
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::PINNED,
                    );
                    if lo != 0 {
                        framed.push_inst(
                            MOp::AddImm {
                                dst: MReg::p(PReg::FP),
                                rn: MReg::p(PReg::FP),
                                imm12: u16_of(lo).unwrap_or(0),
                                shift12: false,
                                wide: true,
                                sets_flags: false,
                            },
                            MInstFlags::PINNED,
                        );
                    }
                } else {
                    framed.push_inst(
                        MOp::AddImm {
                            dst: MReg::p(PReg::FP),
                            rn: MReg::SP,
                            imm12: u16_of(lo).unwrap_or(0),
                            shift12: false,
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::PINNED,
                    );
                }
            }
        }

        let range = func.block_inst_range(blk);
        for i in range {
            let Some(&op) = func.ops.get(i) else {
                continue;
            };
            let fl = at(&func.flags, i, MInstFlags::NONE);
            let vdef = at(&func.vflag_def, i, NO_VFLAG);
            let vuse = at(&func.vflag_use, i, NO_VFLAG);
            let note = at(&func.note_idx, i, NO_NOTE);

            // Patch incoming stacked-parameter placeholder loads in entry block.
            if ord_pos == 0
                && fl.contains(MInstFlags::PINNED)
                && let Some(&param) = incoming_stack_params.get(patched_stack_params)
                && let MOp::Ldr {
                    dst,
                    addr: AddrMode::BaseOff { base: MReg::P(PReg::Sp), off: 0 },
                    size,
                    signed: false,
                } = op
            {
                if param.size == size {
                    let abs_off = frame_size
                        .checked_add(param.caller_off)
                        .ok_or(TierError::TooLarge { tier: TIER, what: "incoming stack param offset" })?;
                    push_sp_mem_single(&mut framed, true, dst, abs_off, size, MInstFlags::NONE);
                    patched_stack_params += 1;
                    continue;
                }
            }

            // Insert epilogue immediately before every `MOp::Ret`.
            if matches!(op, MOp::Ret) && frame_size > 0 {
                emit_save_entries(&mut framed, &save_entries, true);
                push_sp_adjust(&mut framed, false, frame_size);
            }

            let new_inst = framed.push_inst(op, fl);
            if vdef != NO_VFLAG {
                framed.set_vflag_def(new_inst, vdef);
            }
            if vuse != NO_VFLAG {
                framed.set_vflag_use(new_inst, vuse);
            }
            if note != NO_NOTE
                && let Some(slot) = framed.note_idx.get_mut(to_idx(new_inst.0))
            {
                *slot = note;
            }
        }

        framed.end_block(blk);
    }

    framed.rebuild_cfg(alloc);
    *func = framed;

    Ok(FrameOut { unwind_frame: None, final_clobbers })
}

// -----------------------------------------------------------------------------
// Stage 6: Convert `EncodedMFunc` -> `Emitted<'a>`
// -----------------------------------------------------------------------------

fn build_emitted<'a>(
    encoded: &EncodedMFunc<'a>,
    unwind_frame: Option<blaze_code::unwind::Frame>,
    targets: TargetTable<'a>,
    relro: RelroData<'a>,
    trap_kinds: &[(TrapKind, Option<u32>)],
    unwind_entries: &[Option<(MBlock, blaze_code::unwind::Action)>],
    alloc: Alloc<'a>,
) -> Result<Emitted<'a>, TierError> {
    let mut out = Emitted::new_in(alloc);
    out.frame = unwind_frame.and_then(|f| f.word().ok()).unwrap_or(0);
    out.unwind_frame = unwind_frame;
    out.targets = targets.targets;
    out.names = targets.names;
    out.text = targets.text;
    out.relro = relro.relro;
    out.relro_align = out.relro_align.max(relro.relro_align);
    out.words = relro.words;
    out.position_slots = relro.position_slots;

    out.code.reserve(encoded.words.len().saturating_mul(4));
    for &w in encoded.words.iter() {
        for b in w.to_le_bytes() {
            out.code.push(b);
        }
    }

    out.relocs.reserve(encoded.reloc_byte_offs.len());
    for &r in encoded.reloc_byte_offs.iter() {
        out.relocs.push(Reloc {
            at: r.byte_off,
            in_relro: false,
            kind: r.kind,
            target: r.target,
            addend: r.addend,
        });
    }
    out.relocs.sort_unstable_by_key(|r| r.at);

    out.traps.reserve(encoded.trap_byte_offs.len());
    for &t in encoded.trap_byte_offs.iter() {
        let (kind, ord) = trap_kinds
            .get(to_idx(t.trap_idx))
            .copied()
            .unwrap_or((TrapKind::Intrinsic, None));
        out.traps.push(TrapRow {
            at: t.byte_off,
            kind,
            site: ord.map(|o| (0, o)),
            held: Held::Unknown,
            subject: None,
        });
    }
    out.traps.sort_unstable_by_key(|t| t.at);

    if !encoded.unwind_byte_offs.is_empty() {
        out.unwind.reserve(encoded.unwind_byte_offs.len());
        out.pads.reserve(encoded.unwind_byte_offs.len());
        for &u in encoded.unwind_byte_offs.iter() {
            let entry = unwind_entries
                .get(to_idx(u.unwind_idx))
                .copied()
                .ok_or(TierError::Refused {
                    tier: TIER,
                    inst: None,
                    what: "unwind note index out of bounds",
                })?;
            let row = match entry {
                None => blaze_code::unwind::Row {
                    start: u.byte_off,
                    len: 4,
                    landing: 0,
                    action: blaze_code::unwind::Action::Continue,
                },
                Some((lp_blk, action)) => {
                    let word_off = encoded
                        .block_word_off
                        .get(to_idx(lp_blk.0))
                        .copied()
                        .ok_or(TierError::Refused {
                            tier: TIER,
                            inst: None,
                            what: "unwind landing block out of bounds",
                        })?;
                    let landing = word_off
                        .checked_mul(4)
                        .ok_or(TierError::TooLarge { tier: TIER, what: "unwind landing offset" })?;
                    blaze_code::unwind::Row {
                        start: u.byte_off,
                        len: 4,
                        landing,
                        action,
                    }
                }
            };
            out.unwind.push(row);
        }
        out.unwind.sort_unstable_by_key(|r| r.start);
        for &row in out.unwind.iter() {
            out.pads.push(row.pad());
        }
    }

    Ok(out)
}

// -----------------------------------------------------------------------------
// Differential & Apple Silicon Hardware Self-Verification Tests
// (Uses `blaze_own::testing::heap().alloc()`, NO `Global` / NO `std::`)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_code::record::{OWN_RELRO, SiteKind, Symbol};
    use blaze_lir::{
        Flags, FloatCond, IntCond, Op, Ordering, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv, ParamKind, RefKind, SlotKind},
        ty::Scalar,
        verify,
    };
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    use blaze_lir::eval::{self, Outcome, Val};
    use blaze_own::{Alloc, Vec};
    use blaze_tier::{Arch, Machine, Named, Names, Os, Outside, RelocKind, RoutinePlaces, TierError};

    use super::{CallMode, MIseOpts, emit_mfunc_ssa_a64};

    struct NoNames;
    impl Names for NoNames {
        fn name(&self, _of: Outside) -> Option<Named<'_>> {
            None
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    const MAC_ARM64: Machine = Machine {
        arch: Arch::Arm64,
        os: Os::MacOs,
        features: Machine::LSE,
    };

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn default_opts() -> MIseOpts<'static> {
        MIseOpts {
            machine: MAC_ARM64,
            call_mode: CallMode::DirectBl,
            scratch_gpr_order: None,
            preserve_all_cold: false,
            call_clobbers: &[],
            internal_fast: false,
            internal_fast_calls: &[],
            fast_mode: false,
        }
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn eval_u64(view: blaze_lir::View<'_>, args: &[u64], alloc: Alloc<'static>) -> u64 {
        let mut vals: Vec<Val, Alloc<'static>> = Vec::with_capacity_in(args.len(), alloc);
        for &a in args {
            vals.push(Val::Int(u128::from(a)));
        }
        match eval::run_in(view, &vals, 200_000, alloc).expect("LIR evaluator must succeed") {
            Outcome::Returned(out) => match out[0] {
                Val::Int(n) => {
                    let lo = n.to_le_bytes();
                    u64::from_le_bytes([lo[0], lo[1], lo[2], lo[3], lo[4], lo[5], lo[6], lo[7]])
                }
                other => panic!("expected Val::Int return, got {other:?}"),
            },
            other => panic!("unexpected evaluator outcome: {other:?}"),
        }
    }

    /// Deterministic xorshift64 PRNG (Rule 25 & Rule 75: explicit seed, zero ambient randomness).
    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn next_usize(&mut self, bound: usize) -> usize {
            if bound <= 1 {
                return 0;
            }
            let b64 = u64::try_from(bound).unwrap_or(1);
            usize::try_from(self.next_u64() % b64).unwrap_or(0)
        }
    }

    #[test]
    fn misel_pipeline_runs_loop_branch_select_and_slot_on_apple_silicon() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let slot = b.slot(32, 3, SlotKind::Temp);

        let entry = b.create_block(BlockKind::Normal);
        let header = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        // Entry: initialize slot[0..32], jump to loop header `(i=0, acc=a, prev=bv)`.
        b.begin(entry);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let sp = b.slot_addr(slot);
        let k0 = b.iconst(Ty::I64, 0);
        let k8 = b.iconst(Ty::I64, 8);
        b.store(Flags::NONE, a, sp, 0, MemAttrs::aligned(3));
        let sp8 = b.ptr_add(Flags::NONE, sp, k8);
        b.store(Flags::NONE, bv, sp8, 0, MemAttrs::aligned(3));
        b.jump((header, &[k0, a, bv]));

        // Loop header: `if i < n -> body else -> exit`.
        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let prev = b.param(Ty::I64);
        let lt = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, lt, (body, &[]), (exit, &[]));

        // Loop body: exercises `Madd`, shifted `Add`, `Umin`, contiguous `Select`, slot `Load`/`Store`,
        // and cyclic `(acc, prev) = (next_acc, acc)` `ParallelCopy`!
        b.begin(body);
        let k1 = b.iconst(Ty::I64, 1);
        let k3 = b.iconst(Ty::I64, 3);
        let loaded = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
        let prod = b.binary(Op::Mul, Flags::NONE, i, k3);
        let ma = b.binary(Op::Add, Flags::NONE, prod, acc);
        let sh = b.binary(Op::Shl, Flags::NONE, prev, k1);
        let shifted = b.binary(Op::Add, Flags::NONE, ma, sh);
        let cmp_hi = b.icmp(IntCond::Ugt, shifted, loaded);
        let sel1 = b.select(Flags::NONE, cmp_hi, shifted, loaded);
        let sel2 = b.select(Flags::NONE, cmp_hi, prev, i);
        let next_acc = b.binary(Op::Xor, Flags::NONE, sel1, sel2);
        b.store(Flags::NONE, next_acc, sp, 0, MemAttrs::aligned(3));
        let next_i = b.binary(Op::Add, Flags::NONE, i, k1);
        b.jump((header, &[next_i, next_acc, acc]));

        b.begin(exit);
        let s0 = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
        let s1 = b.load(Ty::I64, Flags::NONE, sp, 8, MemAttrs::aligned(3));
        let total = b.binary(Op::Add, Flags::NONE, acc, s0);
        let out = b.binary(Op::Add, Flags::NONE, total, s1);
        b.ret(&[out]);

        let func = b.finish_checked().expect("loop+branch+select+slot LIR builds");
        let verified = verify(func.view()).expect("LIR must verify");
        let (emitted, _clobbers, stats) =
            emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                .expect("misel V3 pipeline must emit cleanly");

        assert!(!emitted.code.is_empty(), "emitted code must be non-empty");
        assert!(
            blaze_code::unwind::Frame::of_word(emitted.frame).is_ok(),
            "emitted frame word must be a valid blaze_code::unwind::Frame"
        );
        assert!(stats.mra.vregs_allocated > 0, "mra must allocate vregs");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                .expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 function with signature `extern "C" fn(u64, u64, u64) -> u64`.
            let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
            for &(av, bval, nv) in &[
                (0u64, 0u64, 0u64),
                (5, 9, 1),
                (17, 42, 10),
                (0x1234_5678, 0x9abc_def0, 25),
            ] {
                let got = f(av, bval, nv);
                let want = eval_u64(func.view(), &[av, bval, nv], alloc);
                assert_eq!(got, want, "misel loop/select/slot diverged on ({av}, {bval}, {nv})");
            }
        }
    }

    #[test]
    fn misel_pipeline_handles_high_pressure_spills_and_remat_on_apple_silicon() {
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
        b.begin(entry);

        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);

        // Create 36 simultaneously live values (> 27 allocatable GPRs) + 8 rematerializable constants
        // to force genuine spills, callee-saved register saves/restores, and rematerialization!
        let mut live: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(36, alloc);
        let mut remat_consts: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(8, alloc);
        for k in 0u64..8u64 {
            remat_consts.push(b.iconst(Ty::I64, u128::from(0x1111_0000u64 + k * 37)));
        }

        let mut cur = x;
        for i in 0u64..36u64 {
            let ci = b.iconst(Ty::I64, u128::from(i.wrapping_mul(13).wrapping_add(7)));
            let step = if (i & 1) == 0 {
                b.binary(Op::Add, Flags::NONE, cur, ci)
            } else {
                b.binary(Op::Xor, Flags::NONE, cur, y)
            };
            let mixed = b.binary(Op::Mul, Flags::NONE, step, ci);
            live.push(mixed);
            cur = mixed;
        }

        // Consume all 36 simultaneously live values AND all 8 constants in reverse order so every
        // interval overlaps at the peak!
        let mut acc = cur;
        for i in (0usize..36usize).rev() {
            let v = live[i];
            let rc = remat_consts[i & 7];
            let t = b.binary(Op::Add, Flags::NONE, acc, v);
            acc = b.binary(Op::Xor, Flags::NONE, t, rc);
        }
        b.ret(&[acc]);

        let func = b.finish_checked().expect("high-pressure LIR builds");
        let verified = verify(func.view()).expect("high-pressure LIR verifies");
        let (emitted, _clobbers, stats) =
            emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                .expect("misel must handle 36-live spill+remat cleanly");

        assert!(stats.mra.spill_slots_used > 0, "36 live GPRs must exercise stack spills");
        assert!(
            blaze_code::unwind::Frame::of_word(emitted.frame).is_ok(),
            "spilled function must emit a valid blaze_code::unwind::Frame word"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                .expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 function with signature `extern "C" fn(u64, u64) -> u64`.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            for &(xv, yv) in &[
                (0u64, 0u64),
                (1, 2),
                (0xdead_beef_cafe_babe, 0x0123_4567_89ab_cdef),
                (u64::MAX, 42),
            ] {
                let got = f(xv, yv);
                let want = eval_u64(func.view(), &[xv, yv], alloc);
                assert_eq!(got, want, "high-pressure spill/remat diverged on ({xv:#x}, {yv:#x})");
            }
        }
    }

    #[test]
    fn misel_differential_matches_lir_evaluator_across_64_random_programs() {
        let alloc = test_alloc();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

        let int_conds = [
            IntCond::Eq,
            IntCond::Ne,
            IntCond::Ult,
            IntCond::Ule,
            IntCond::Ugt,
            IntCond::Uge,
            IntCond::Slt,
            IntCond::Sle,
            IntCond::Sgt,
            IntCond::Sge,
        ];

        for prog_idx in 0u64..64u64 {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);

            let has_loop = (prog_idx % 2) == 0;
            let has_slot = (prog_idx % 3) != 0;
            let has_narrow = (prog_idx % 2) == 1 || (prog_idx % 4) == 0;
            let slot = if has_slot {
                Some(b.slot(64, 3, SlotKind::Temp))
            } else {
                None
            };

            let entry = b.create_block(BlockKind::Normal);
            let header = if has_loop { Some(b.create_block(BlockKind::Normal)) } else { None };
            let body_a = if has_loop { Some(b.create_block(BlockKind::Normal)) } else { None };
            let body_b = if has_loop { Some(b.create_block(BlockKind::Normal)) } else { None };
            let exit = if has_loop { Some(b.create_block(BlockKind::Normal)) } else { None };

            b.begin(entry);
            let p0 = b.param(Ty::I64);
            let p1 = b.param(Ty::I64);
            let p2 = b.param(Ty::I64);
            let p3 = b.param(Ty::I64);

            let mut pool64: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(32, alloc);
            pool64.push(p0);
            pool64.push(p1);
            pool64.push(p2);
            pool64.push(p3);

            let slot_ptr = slot.map(|s| {
                let sp = b.slot_addr(s);
                b.store(Flags::NONE, p0, sp, 0, MemAttrs::aligned(3));
                b.store(Flags::NONE, p1, sp, 8, MemAttrs::aligned(3));
                b.store(Flags::NONE, p2, sp, 16, MemAttrs::aligned(3));
                b.store(Flags::NONE, p3, sp, 24, MemAttrs::aligned(3));
                sp
            });

            // Helper: emit a random straight-line mix of scalar, fused, narrow, bitwise, min/max,
            // select, and slot ops into `pool64`.
            let emit_random_ops = |b: &mut Builder<Alloc<'static>, Full>,
                                   rng: &mut Rng,
                                   pool: &mut Vec<blaze_lir::Value, Alloc<'static>>,
                                   sp_opt: Option<blaze_lir::Value>,
                                   do_narrow: bool,
                                   count: usize| {
                for _ in 0..count {
                    let x = pool[rng.next_usize(pool.len())];
                    let y = pool[rng.next_usize(pool.len())];
                    let z = pool[rng.next_usize(pool.len())];
                    let kind = rng.next_usize(18);
                    let res = match kind {
                        0 => {
                            // `Madd`: `Add(z, Mul(x, y))`
                            let m = b.binary(Op::Mul, Flags::NONE, x, y);
                            b.binary(Op::Add, Flags::NONE, m, z)
                        }
                        1 => {
                            // `Msub`: `Sub(z, Mul(x, y))`
                            let m = b.binary(Op::Mul, Flags::NONE, x, y);
                            b.binary(Op::Sub, Flags::NONE, z, m)
                        }
                        2 => {
                            // Shifted `Add` / `Sub`
                            let sh_amt = b.iconst(Ty::I64, u128::from((rng.next_u64() & 15) + 1));
                            let sh = if (rng.next_u64() & 1) == 0 {
                                b.binary(Op::Shl, Flags::NONE, y, sh_amt)
                            } else {
                                b.binary(Op::Lshr, Flags::NONE, y, sh_amt)
                            };
                            if (rng.next_u64() & 1) == 0 {
                                b.binary(Op::Add, Flags::NONE, x, sh)
                            } else {
                                b.binary(Op::Sub, Flags::NONE, x, sh)
                            }
                        }
                        3 => {
                            // `And/Or/Xor(x, Not(y))` (`bic`/`orn`/`eon`)
                            let ny = b.unary(Op::Not, Flags::NONE, y);
                            let bop = match rng.next_usize(3) {
                                0 => Op::And,
                                1 => Op::Or,
                                _ => Op::Xor,
                            };
                            b.binary(bop, Flags::NONE, x, ny)
                        }
                        4 => {
                            // `Ubfx`: `And(Lshr(x, s), (1 << w) - 1)`
                            let s = (rng.next_u64() & 31) + 1;
                            let w = (rng.next_u64() % 16) + 1;
                            let sv = b.iconst(Ty::I64, u128::from(s));
                            let mv = b.iconst(Ty::I64, u128::from((1u64 << w) - 1));
                            let shr = b.binary(Op::Lshr, Flags::NONE, x, sv);
                            b.binary(Op::And, Flags::NONE, shr, mv)
                        }
                        5 => {
                            // `Umin` / `Umax` / `Smin` / `Smax`
                            let mop = match rng.next_usize(4) {
                                0 => Op::Umin,
                                1 => Op::Umax,
                                2 => Op::Smin,
                                _ => Op::Smax,
                            };
                            b.binary(mop, Flags::NONE, x, y)
                        }
                        6 => {
                            // Fused `Icmp` + two contiguous sibling `Select`s!
                            let ic = int_conds[rng.next_usize(int_conds.len())];
                            let c = b.icmp(ic, x, y);
                            let s1 = b.select(Flags::NONE, c, x, z);
                            let s2 = b.select(Flags::NONE, c, z, y);
                            b.binary(Op::Add, Flags::NONE, s1, s2)
                        }
                        7 => {
                            // Materialized `I1` + `Select(c, 1, 0)` / `(0, 1)` + `Zext`
                            let ic = int_conds[rng.next_usize(int_conds.len())];
                            let c1 = b.icmp(ic, x, y);
                            let c2 = b.icmp(IntCond::Ne, y, z);
                            let both = b.binary(Op::And, Flags::NONE, c1, c2);
                            let k0 = b.iconst(Ty::I64, 0);
                            let k1 = b.iconst(Ty::I64, 1);
                            let s10 = b.select(Flags::NONE, both, k1, k0);
                            let s01 = b.select(Flags::NONE, both, k0, k1);
                            let k4 = b.iconst(Ty::I64, 4);
                            let sh = b.binary(Op::Shl, Flags::NONE, s01, k4);
                            b.binary(Op::Add, Flags::NONE, s10, sh)
                        }
                        8 => {
                            // Bitops: `Clz` / `Ctz` / `Popcnt` / `Bswap` / `Bitrev`
                            let uop = match rng.next_usize(5) {
                                0 => Op::Clz,
                                1 => Op::Ctz,
                                2 => Op::Popcnt,
                                3 => Op::Bswap,
                                _ => Op::Bitrev,
                            };
                            b.unary(uop, Flags::NONE, x)
                        }
                        9 => {
                            // `Rotl` / `Rotr` (constant & variable)
                            let rop = if (rng.next_u64() & 1) == 0 { Op::Rotl } else { Op::Rotr };
                            let amt = if (rng.next_u64() & 1) == 0 {
                                b.iconst(Ty::I64, u128::from((rng.next_u64() & 63).max(1)))
                            } else {
                                y
                            };
                            b.binary(rop, Flags::NONE, x, amt)
                        }
                        10 => {
                            // `Umulhi` / `Smulhi`
                            let mop = if (rng.next_u64() & 1) == 0 { Op::Umulhi } else { Op::Smulhi };
                            b.binary(mop, Flags::NONE, x, y)
                        }
                        11 => {
                            // Safe `Udiv` / `Urem` (divisor ORed with 1 so never zero)
                            let k1 = b.iconst(Ty::I64, 1);
                            let nz_y = b.binary(Op::Or, Flags::NONE, y, k1);
                            let dop = if (rng.next_u64() & 1) == 0 { Op::Udiv } else { Op::Urem };
                            b.binary(dop, Flags::NONE, x, nz_y)
                        }
                        12 => {
                            // `LogicalImm` & `AddImm`/`SubImm`
                            let imm = b.iconst(Ty::I64, u128::from((rng.next_u64() & 0xfff) | 1));
                            match rng.next_usize(4) {
                                0 => b.binary(Op::Add, Flags::NONE, x, imm),
                                1 => b.binary(Op::Sub, Flags::NONE, x, imm),
                                2 => b.binary(Op::Xor, Flags::NONE, x, imm),
                                _ => b.binary(Op::Or, Flags::NONE, x, imm),
                            }
                        }
                        13 | 14 if do_narrow => {
                            // Narrow `I8` / `I16` / `I32` arithmetic, shifts, `Sext`, `Zext`, `Icmp`!
                            let nty = match rng.next_usize(3) {
                                0 => Ty::I8,
                                1 => Ty::I16,
                                _ => Ty::I32,
                            };
                            let nx = b.convert(Op::Trunc, Flags::NONE, nty, x);
                            let ny = b.convert(Op::Trunc, Flags::NONE, nty, y);
                            let nres = match rng.next_usize(8) {
                                0 => b.binary(Op::Add, Flags::NONE, nx, ny),
                                1 => b.binary(Op::Sub, Flags::NONE, nx, ny),
                                2 => b.binary(Op::Mul, Flags::NONE, nx, ny),
                                3 => {
                                    let sh = b.iconst(nty, u128::from((rng.next_u64() % 7) + 1));
                                    b.binary(Op::Shl, Flags::NONE, nx, sh)
                                }
                                4 => {
                                    let sh = b.iconst(nty, u128::from((rng.next_u64() % 7) + 1));
                                    b.binary(Op::Lshr, Flags::NONE, nx, sh)
                                }
                                5 => {
                                    let sh = b.iconst(nty, u128::from((rng.next_u64() % 7) + 1));
                                    b.binary(Op::Ashr, Flags::NONE, nx, sh)
                                }
                                6 => b.binary(Op::Smin, Flags::NONE, nx, ny),
                                _ => {
                                    let ic = int_conds[rng.next_usize(int_conds.len())];
                                    let nc = b.icmp(ic, nx, ny);
                                    b.select(Flags::NONE, nc, nx, ny)
                                }
                            };
                            if (rng.next_u64() & 1) == 0 {
                                b.convert(Op::Zext, Flags::NONE, Ty::I64, nres)
                            } else {
                                b.convert(Op::Sext, Flags::NONE, Ty::I64, nres)
                            }
                        }
                        15 | 16 if sp_opt.is_some() => {
                            // Stack `Slot` store + load (both `BaseOff` and `ScaledIdx`!)
                            let sp = sp_opt.expect("checked Some");
                            let idx = u64::try_from(rng.next_usize(4)).unwrap_or(0);
                            let off_i32 = i32::try_from(idx * 8).unwrap_or(0);
                            b.store(Flags::NONE, x, sp, off_i32, MemAttrs::aligned(3));
                            if (rng.next_u64() & 1) == 0 {
                                // `PtrAdd(sp, Shl(idx_masked, 3))` -> `ScaledIdx`!
                                let k3 = b.iconst(Ty::I64, 3);
                                let idx_m = b.binary(Op::And, Flags::NONE, y, k3);
                                let scaled = b.binary(Op::Shl, Flags::NONE, idx_m, k3);
                                let addr = b.ptr_add(Flags::NONE, sp, scaled);
                                b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3))
                            } else {
                                let k_off = b.iconst(Ty::I64, u128::from(idx * 8));
                                let addr = b.ptr_add(Flags::NONE, sp, k_off);
                                b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3))
                            }
                        }
                        _ => {
                            // General `Ashr` / `Lshr` / `Shl` / `Neg`
                            let vop = match rng.next_usize(3) {
                                0 => Op::Shl,
                                1 => Op::Lshr,
                                _ => Op::Ashr,
                            };
                            b.binary(vop, Flags::NONE, x, y)
                        }
                    };
                    pool.push(res);
                }
            };

            let entry_ops = 6 + rng.next_usize(8);
            emit_random_ops(&mut b, &mut rng, &mut pool64, slot_ptr, has_narrow, entry_ops);

            if let (Some(h_blk), Some(a_blk), Some(b_blk), Some(e_blk)) =
                (header, body_a, body_b, exit)
            {
                let trip = b.iconst(Ty::I64, u128::from((rng.next_u64() % 12) + 2));
                let init_acc = pool64[pool64.len() - 1];
                let init_alt = pool64[pool64.len() - 2];
                b.jump((h_blk, &[trip, init_acc, init_alt]));

                b.begin(h_blk);
                let rem = b.param(Ty::I64);
                let acc = b.param(Ty::I64);
                let alt = b.param(Ty::I64);
                let k0 = b.iconst(Ty::I64, 0);
                let done = b.icmp(IntCond::Eq, rem, k0);
                // Conditional branch with block arg on exit edge (tests `Br` edge splitting!).
                let k1 = b.iconst(Ty::I64, 1);
                let bit0 = b.binary(Op::And, Flags::NONE, acc, k1);
                let go_a = b.icmp(IntCond::Eq, bit0, k0);
                let mid_blk = b.create_block(BlockKind::Normal);
                b.br(Flags::NONE, done, (e_blk, &[acc]), (mid_blk, &[]));

                b.begin(mid_blk);
                b.br(Flags::NONE, go_a, (a_blk, &[acc]), (b_blk, &[alt]));

                // Arm A: random ops + loop back-edge swapping `(acc, alt)`!
                b.begin(a_blk);
                let arm_a_in = b.param(Ty::I64);
                let mut pool_a: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(16, alloc);
                pool_a.push(arm_a_in);
                pool_a.push(alt);
                pool_a.push(rem);
                pool_a.push(p2);
                emit_random_ops(&mut b, &mut rng, &mut pool_a, slot_ptr, has_narrow, 4);
                let next_acc_a = pool_a[pool_a.len() - 1];
                let next_rem_a = b.binary(Op::Sub, Flags::NONE, rem, k1);
                b.jump((h_blk, &[next_rem_a, next_acc_a, acc]));

                // Arm B: random ops + loop back-edge!
                b.begin(b_blk);
                let arm_b_in = b.param(Ty::I64);
                let mut pool_b: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(16, alloc);
                pool_b.push(arm_b_in);
                pool_b.push(acc);
                pool_b.push(rem);
                pool_b.push(p3);
                emit_random_ops(&mut b, &mut rng, &mut pool_b, slot_ptr, has_narrow, 4);
                let next_acc_b = pool_b[pool_b.len() - 1];
                let next_rem_b = b.binary(Op::Sub, Flags::NONE, rem, k1);
                b.jump((h_blk, &[next_rem_b, alt, next_acc_b]));

                b.begin(e_blk);
                let final_v = b.param(Ty::I64);
                let out_v = if let Some(sp) = slot_ptr {
                    let s0 = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
                    b.binary(Op::Xor, Flags::NONE, final_v, s0)
                } else {
                    final_v
                };
                b.ret(&[out_v]);
            } else {
                let mut final_v = pool64[pool64.len() - 1];
                if let Some(sp) = slot_ptr {
                    let s0 = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
                    final_v = b.binary(Op::Xor, Flags::NONE, final_v, s0);
                }
                b.ret(&[final_v]);
            }

            let func = b
                .finish_checked()
                .unwrap_or_else(|e| panic!("random program {prog_idx} failed build: {e:?}"));
            let verified = verify(func.view())
                .unwrap_or_else(|e| panic!("random program {prog_idx} failed verify: {e:?}"));

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .unwrap_or_else(|e| panic!("random program {prog_idx} failed misel: {e:?}"));

            assert!(!emitted.code.is_empty(), "program {prog_idx} produced empty code");

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `(u64, u64, u64, u64) -> u64`.
                let f: extern "C" fn(u64, u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
                let test_inputs: [(u64, u64, u64, u64); 4] = [
                    (0, 1, 2, 3),
                    (7, 13, 29, 97),
                    (0xffff_ffff_ffff_fffe, 0x8000_0000_0000_0001, 0x00ff_00ff_00ff_00ff, 42),
                    (
                        rng.next_u64(),
                        rng.next_u64(),
                        rng.next_u64(),
                        rng.next_u64(),
                    ),
                ];
                for &(a0, a1, a2, a3) in &test_inputs {
                    let got = f(a0, a1, a2, a3);
                    let want = eval_u64(func.view(), &[a0, a1, a2, a3], alloc);
                    assert_eq!(
                        got, want,
                        "program {prog_idx} diverged on ({a0:#x}, {a1:#x}, {a2:#x}, {a3:#x})"
                    );
                }
            }
        }
    }

    #[test]
    fn misel_refuses_unsupported_ops_cleanly_and_executes_fp_pipeline() {
        let alloc = test_alloc();

        // 1. Subset checker refuses `Op::Fence` with `TierError::Refused`.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            b.fence(Flags::NONE.with_ordering(Ordering::SeqCst));
            b.ret(&[]);
            let func = b.finish_checked().expect("fence LIR builds");
            let verified = verify(func.view()).expect("fence LIR verifies");
            let err = emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                .err()
                .expect("misel must refuse Op::Fence");
            assert!(matches!(err, TierError::Refused { .. }), "expected Refused, got {err:?}");
        }

        // 2. Scalar FP (`F64` arithmetic, `Fma`, `Fcmp`, `Select`, `FptoSi`, `SitoFp`) executes cleanly.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::F64), Param::of(Ty::F64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let fa = b.param(Ty::F64);
            let fb = b.param(Ty::F64);
            let n = b.param(Ty::I64);
            let fn_val = b.convert(Op::SitoFp, Flags::NONE, Ty::F64, n);
            let fma = b.ternary(Op::Fma, Flags::NONE, fa, fb, fn_val);
            let neg_fma = b.unary(Op::Fneg, Flags::NONE, fma);
            let cmp = b.fcmp(FloatCond::Ogt, fma, fb);
            let chosen = b.select(Flags::NONE, cmp, fma, neg_fma);
            let out = b.convert(Op::FptoSi, Flags::NONE, Ty::I64, chosen);
            b.ret(&[out]);

            let func = b.finish_checked().expect("FP LIR builds");
            let verified = verify(func.view()).expect("FP LIR verifies");
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel FP pipeline must emit cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `(f64, f64, i64) -> i64`.
                let f: extern "C" fn(f64, f64, i64) -> i64 = unsafe { exec.as_fn() };
                for &(a, bv, n_i) in &[(2.5f64, 4.0f64, 3i64), (-1.5, 2.0, -5), (0.0, 1.0, 10)] {
                    let got = f(a, bv, n_i);
                    let want = eval_u64(
                        func.view(),
                        &[a.to_bits(), bv.to_bits(), n_i.cast_unsigned()],
                        alloc,
                    )
                    .cast_signed();
                    assert_eq!(got, want, "FP pipeline diverged on ({a}, {bv}, {n_i})");
                }
            }
        }
    }

    #[test]
    fn misel_sret_x8_incoming_and_outgoing_on_apple_silicon() {
        let alloc = test_alloc();

        // 1. Incoming `ParamKind::Sret` in `X8` + two `I64` args in `X0`, `X1`:
        // writes `{ a + b, a ^ b, a.saturating_sub(b) }` into the 24-byte struct at `X8`.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sret_param = Param { kind: ParamKind::Sret, ..Param::of(Ty::PTR) };
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[sret_param, Param::of(Ty::I64), Param::of(Ty::I64)],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let out_ptr = b.param(Ty::PTR);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let sum = b.binary(Op::Add, Flags::NONE, a, bv);
            let xored = b.binary(Op::Xor, Flags::NONE, a, bv);
            let usub = b.binary(Op::UsubSat, Flags::NONE, a, bv);
            b.store(Flags::NONE, sum, out_ptr, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, xored, out_ptr, 8, MemAttrs::aligned(3));
            b.store(Flags::NONE, usub, out_ptr, 16, MemAttrs::aligned(3));
            b.ret(&[]);

            let func = b.finish_checked().expect("sret callee LIR builds");
            let verified = verify(func.view()).expect("sret callee LIR verifies");
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit sret callee cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                #[repr(C)]
                #[derive(PartialEq, Eq, Debug)]
                struct Out24 {
                    f0: u64,
                    f1: u64,
                    f2: u64,
                }
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function returning a 24-byte composite via `X8` (`sret`).
                let f: extern "C" fn(u64, u64) -> Out24 = unsafe { exec.as_fn() };
                for &(av, bval) in &[(10u64, 3u64), (3, 10), (0, 0), (u64::MAX, 42)] {
                    let got = f(av, bval);
                    let want = Out24 {
                        f0: av.wrapping_add(bval),
                        f1: av ^ bval,
                        f2: av.saturating_sub(bval),
                    };
                    assert_eq!(got, want, "sret X8 diverged on ({av:#x}, {bval:#x})");
                }
            }
        }

        // 2. Outgoing `Op::Call` with `ParamKind::Sret` + 8 regular GPR args (verifies `Sret`
        // uses `X8` without pushing the 8th regular GPR arg onto the stack!).
        {
            struct OneFuncNames;
            impl Names for OneFuncNames {
                fn name(&self, of: Outside) -> Option<Named<'_>> {
                    match of {
                        Outside::Func(0) => Some(Named::Symbol(Symbol::of_name("test_sret_callee"))),
                        _ => None,
                    }
                }
                fn routines(&self) -> RoutinePlaces {
                    RoutinePlaces::NONE
                }
            }
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sret_param = Param { kind: ParamKind::Sret, ..Param::of(Ty::PTR) };
            let callee_sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    sret_param,
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                    Param::of(Ty::I64),
                ],
                &[],
            );
            let caller_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(caller_sig);
            let slot = b.slot(24, 3, SlotKind::Temp);
            let callee_ref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let sp = b.slot_addr(slot);
            let _ = b.call(Flags::NONE, callee_ref, callee_sig, &[sp, x, x, x, x, x, x, x, x]);
            let v0 = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
            b.ret(&[v0]);

            let func = b.finish_checked().expect("sret caller LIR builds");
            let verified = verify(func.view()).expect("sret caller LIR verifies");
            let mut opts = default_opts();
            opts.call_mode = CallMode::GotSlot;
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &OneFuncNames, opts, alloc)
                    .expect("misel must emit outgoing sret call cleanly");
            assert!(!emitted.code.is_empty());
            let (rec, _) = super::super::emitted_to_code_record(emitted, &[], alloc)
                .expect("sret caller must convert cleanly to CodeRecord");
            rec.seal().expect("sret caller CodeRecord must seal");
        }
    }

    #[test]
    fn misel_saturating_arith_all_widths_matches_eval_on_apple_silicon() {
        let alloc = test_alloc();
        let ops = [Op::UsubSat, Op::UaddSat, Op::SaddSat, Op::SsubSat];
        let widths = [Ty::I8, Ty::I16, Ty::I32, Ty::I64];

        for &op in &ops {
            for &ty in &widths {
                let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
                let sig = b.sig(
                    Conv::C,
                    Variadic::No,
                    &[Param::of(Ty::I64), Param::of(Ty::I64)],
                    &[Param::of(Ty::I64)],
                );
                b.set_signature(sig);
                let entry = b.create_block(BlockKind::Normal);
                b.begin(entry);
                let a64 = b.param(Ty::I64);
                let b64 = b.param(Ty::I64);
                let (la, lb) = if ty == Ty::I64 {
                    (a64, b64)
                } else {
                    (
                        b.convert(Op::Trunc, Flags::NONE, ty, a64),
                        b.convert(Op::Trunc, Flags::NONE, ty, b64),
                    )
                };
                let sat = b.binary(op, Flags::NONE, la, lb);
                let out = if ty == Ty::I64 {
                    sat
                } else {
                    b.convert(Op::Zext, Flags::NONE, Ty::I64, sat)
                };
                b.ret(&[out]);

                let func = b.finish_checked().expect("sat arith LIR builds");
                let verified = verify(func.view()).expect("sat arith LIR verifies");
                let (emitted, _clobbers, _stats) =
                    emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                        .expect("misel must emit saturating op");
                assert!(!emitted.code.is_empty());

                #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
                {
                    let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                        .expect("W^X exec mapping must succeed");
                    // SAFETY: Valid AAPCS64 function `(u64, u64) -> u64`.
                    let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
                    let edges: [u64; 14] = [
                        0,
                        1,
                        2,
                        100,
                        127,
                        128,
                        255,
                        32_767,
                        32_768,
                        65_535,
                        0x7fff_ffff,
                        0x8000_0000,
                        0xffff_ffff,
                        u64::MAX,
                    ];
                    let extra: [(u64, u64); 6] = [
                        (0x7fff_ffff_ffff_ffff, 1),
                        (0x8000_0000_0000_0000, 1),
                        (0x8000_0000_0000_0000, u64::MAX),
                        (0x7fff_ffff_ffff_ffff, 0x8000_0000_0000_0000),
                        (0xffff_ffff_ffff_fffe, 5),
                        (5, 10),
                    ];
                    for &av in &edges {
                        for &bv in &edges {
                            let got = f(av, bv);
                            let want = eval_u64(func.view(), &[av, bv], alloc);
                            assert_eq!(
                                got, want,
                                "sat {op:?}/{ty:?} diverged on ({av:#x}, {bv:#x})"
                            );
                        }
                    }
                    for &(av, bv) in &extra {
                        let got = f(av, bv);
                        let want = eval_u64(func.view(), &[av, bv], alloc);
                        assert_eq!(
                            got, want,
                            "sat {op:?}/{ty:?} diverged on ({av:#x}, {bv:#x})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn misel_overflow_and_project_all_widths_matches_eval_on_apple_silicon() {
        let alloc = test_alloc();
        let ops = [
            Op::UaddOverflow,
            Op::UsubOverflow,
            Op::SaddOverflow,
            Op::SsubOverflow,
            Op::UmulOverflow,
            Op::SmulOverflow,
        ];
        let widths = [Ty::I8, Ty::I16, Ty::I32, Ty::I64];

        for &op in &ops {
            for &ty in &widths {
                let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
                let sig = b.sig(
                    Conv::C,
                    Variadic::No,
                    &[Param::of(Ty::I64), Param::of(Ty::I64)],
                    &[Param::of(Ty::I64)],
                );
                b.set_signature(sig);
                let entry = b.create_block(BlockKind::Normal);
                b.begin(entry);
                let a64 = b.param(Ty::I64);
                let b64 = b.param(Ty::I64);
                let (la, lb) = if ty == Ty::I64 {
                    (a64, b64)
                } else {
                    (
                        b.convert(Op::Trunc, Flags::NONE, ty, a64),
                        b.convert(Op::Trunc, Flags::NONE, ty, b64),
                    )
                };
                let pair = b.overflow(op, la, lb);
                let val = b.project(pair, 0);
                let ovf = b.project(pair, 1);
                let val64 = if ty == Ty::I64 {
                    val
                } else {
                    b.convert(Op::Zext, Flags::NONE, Ty::I64, val)
                };
                let ovf64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, ovf);
                // Pack `(val64.rotate_left(1)) ^ (ovf64 * 0x5555)` to check BOTH projected value and flag!
                let k1 = b.iconst(Ty::I64, 1);
                let rot = b.binary(Op::Rotl, Flags::NONE, val64, k1);
                let tag = b.iconst(Ty::I64, 0x5555);
                let ovf_mix = b.binary(Op::Mul, Flags::NONE, ovf64, tag);
                let out = b.binary(Op::Xor, Flags::NONE, rot, ovf_mix);
                b.ret(&[out]);

                let func = b.finish_checked().expect("overflow LIR builds");
                let verified = verify(func.view()).expect("overflow LIR verifies");
                let (emitted, _clobbers, _stats) =
                    emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                        .expect("misel must emit overflow + project");
                assert!(!emitted.code.is_empty());

                #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
                {
                    let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                        .expect("W^X exec mapping must succeed");
                    // SAFETY: Valid AAPCS64 function `(u64, u64) -> u64`.
                    let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
                    let vals: [u64; 12] = [
                        0,
                        1,
                        2,
                        16,
                        127,
                        128,
                        255,
                        32_768,
                        0x7fff_ffff,
                        0x8000_0000,
                        0x7fff_ffff_ffff_ffff,
                        u64::MAX,
                    ];
                    for &av in &vals {
                        for &bv in &vals {
                            let got = f(av, bv);
                            let want = eval_u64(func.view(), &[av, bv], alloc);
                            assert_eq!(
                                got, want,
                                "overflow {op:?}/{ty:?} diverged on ({av:#x}, {bv:#x})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn misel_switch_linear_and_binary_with_edge_args_matches_eval_on_apple_silicon() {
        let alloc = test_alloc();

        // 10-case switch (> 4 -> exercises iterative binary search AND linear leaves AND edge ParallelCopy!).
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let join = b.create_block(BlockKind::Normal);
        let mut case_blks: Vec<blaze_lir::Block, Alloc<'static>> = Vec::with_capacity_in(10, alloc);
        for _ in 0..10 {
            case_blks.push(b.create_block(BlockKind::Normal));
        }
        let def_blk = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let scrut = b.param(Ty::I64);
        let x = b.param(Ty::I64);
        let k_def = b.iconst(Ty::I64, 999);
        let k_c0 = b.iconst(Ty::I64, 11);
        let k_c5 = b.iconst(Ty::I64, 55);
        let k_c9 = b.iconst(Ty::I64, 99);
        let case_keys: [u128; 10] = [0, 1, 3, 7, 15, 42, 100, 4096, 70_000, 0xdead_beef_0000_0001];
        let cases_arr: [(u128, (blaze_lir::Block, &[blaze_lir::Value])); 10] = [
            (case_keys[0], (case_blks[0], &[k_c0])),
            (case_keys[1], (case_blks[1], &[x])),
            (case_keys[2], (case_blks[2], &[])),
            (case_keys[3], (case_blks[3], &[])),
            (case_keys[4], (case_blks[4], &[])),
            (case_keys[5], (case_blks[5], &[k_c5])),
            (case_keys[6], (case_blks[6], &[])),
            (case_keys[7], (case_blks[7], &[])),
            (case_keys[8], (case_blks[8], &[])),
            (case_keys[9], (case_blks[9], &[k_c9])),
        ];
        b.switch(scrut, (def_blk, &[k_def]), &cases_arr);

        for idx in 0usize..10usize {
            b.begin(case_blks[idx]);
            let base = if matches!(idx, 0 | 1 | 5 | 9) {
                b.param(Ty::I64)
            } else {
                b.iconst(Ty::I64, u128::try_from((idx + 1) * 100).unwrap_or(0))
            };
            let res = b.binary(Op::Add, Flags::NONE, base, x);
            b.jump((join, &[res]));
        }
        b.begin(def_blk);
        let d_arg = b.param(Ty::I64);
        let d_res = b.binary(Op::Xor, Flags::NONE, d_arg, x);
        b.jump((join, &[d_res]));

        b.begin(join);
        let out = b.param(Ty::I64);
        b.ret(&[out]);

        let func = b.finish_checked().expect("switch LIR builds");
        let verified = verify(func.view()).expect("switch LIR verifies");
        let (emitted, _clobbers, _stats) =
            emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                .expect("misel must emit 10-case switch");
        assert!(!emitted.code.is_empty());

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                .expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 function `(u64, u64) -> u64`.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            let probes: [u64; 17] = [
                0,
                1,
                2,
                3,
                5,
                7,
                15,
                16,
                42,
                99,
                100,
                4096,
                4097,
                70_000,
                0xdead_beef_0000_0000,
                0xdead_beef_0000_0001,
                u64::MAX,
            ];
            for &p in &probes {
                let got = f(p, 17);
                let want = eval_u64(func.view(), &[p, 17], alloc);
                assert_eq!(got, want, "switch diverged on scrut={p:#x}");
            }
        }
    }

    #[test]
    fn misel_data_addr_func_addr_and_relro_convert_to_code_record() {
        let alloc = test_alloc();
        struct TestNames;
        impl Names for TestNames {
            fn name(&self, of: Outside) -> Option<Named<'_>> {
                match of {
                    Outside::Static(0) => Some(Named::Symbol(Symbol::of_name("test_static_0"))),
                    Outside::Func(0) => Some(Named::Symbol(Symbol::of_name("test_func_0"))),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let data_ref = b.data(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88], 3, &[]);
        let static_ref = b.reference(RefKind::Static, 0);
        let func_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let p_data = b.data_addr(data_ref);
        let p_static = b.data_addr(static_ref);
        let p_func = b.func_addr(func_ref);
        let a0 = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p_data);
        let a1 = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p_static);
        let a2 = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p_func);
        let s0 = b.binary(Op::Xor, Flags::NONE, a0, a1);
        let s1 = b.binary(Op::Xor, Flags::NONE, s0, a2);
        b.ret(&[s1]);

        let func = b.finish_checked().expect("data_addr/func_addr LIR builds");
        let verified = verify(func.view()).expect("data_addr/func_addr LIR verifies");
        let mut opts = default_opts();
        opts.call_mode = CallMode::GotSlot;
        let (emitted, _clobbers, _stats) =
            emit_mfunc_ssa_a64(verified, &TestNames, opts, alloc)
                .expect("misel must emit data_addr + func_addr cleanly");

        assert_eq!(&emitted.relro[..8], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
        assert_eq!(emitted.relocs.len(), 6, "expected 3 adjacent reloc pairs");
        assert_eq!(emitted.relocs[0].kind, RelocKind::Page21);
        assert_eq!(emitted.relocs[0].target, OWN_RELRO);
        assert_eq!(emitted.relocs[1].kind, RelocKind::PageOff12 { scale: 0 });
        assert_eq!(emitted.relocs[1].at, emitted.relocs[0].at + 4);
        assert_eq!(emitted.relocs[2].kind, RelocKind::GotPage21);
        assert_eq!(emitted.relocs[3].kind, RelocKind::GotPageOff12);
        assert_eq!(emitted.relocs[3].at, emitted.relocs[2].at + 4);
        assert_eq!(emitted.relocs[4].kind, RelocKind::GotPage21);
        assert_eq!(emitted.relocs[5].kind, RelocKind::GotPageOff12);
        assert_eq!(emitted.relocs[5].at, emitted.relocs[4].at + 4);

        let (rec, _) = super::super::emitted_to_code_record(emitted, &[], alloc)
            .expect("data_addr/func_addr Emitted must convert to CodeRecord");
        assert_eq!(rec.sites.len(), 3);
        assert_eq!(rec.sites[0].kind, SiteKind::Relro);
        assert_eq!(rec.sites[1].kind, SiteKind::Load);
        assert_eq!(rec.sites[2].kind, SiteKind::Load);
        assert!(rec.seal().is_ok(), "CodeRecord with Relro + Load sites must seal");
    }

    #[test]
    fn misel_prefetch_intrinsic_and_trunc_zero_mov_alias_on_apple_silicon() {
        let alloc = test_alloc();

        // 1. `blaze.prefetch.r1` emits `prfm pldl1keep` and runs cleanly on a real pointer.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let intr_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
            let intr = b.intrinsic("blaze.prefetch.r1", intr_sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let ptr = b.param(Ty::PTR);
            let _ = b.target_intrinsic(intr, &[ptr]);
            let val = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(3));
            b.ret(&[val]);

            let func = b.finish_checked().expect("prefetch LIR builds");
            let verified = verify(func.view()).expect("prefetch LIR verifies");
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit blaze.prefetch.r1 cleanly");

            // Verify a `prfm pldl1keep, [xn, #0]` instruction (`0xf980_0000` with `Rt = 0`) is present!
            let mut saw_prfm = false;
            let mut off = 0usize;
            while off + 4 <= emitted.code.len() {
                let w = u32::from_le_bytes([
                    emitted.code[off],
                    emitted.code[off + 1],
                    emitted.code[off + 2],
                    emitted.code[off + 3],
                ]);
                if (w & 0xffc0_001f) == 0xf980_0000 {
                    saw_prfm = true;
                    break;
                }
                off += 4;
            }
            assert!(saw_prfm, "expected prfm pldl1keep in emitted machine code");

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(*const u64) -> u64`.
                let f: extern "C" fn(*const u64) -> u64 = unsafe { exec.as_fn() };
                let sample: u64 = 0xcafe_babe_dead_beef;
                assert_eq!(f(&raw const sample), sample);
            }
        }

        // 2. `Op::Trunc(I64 -> I32)` after `Op::Lshr(..., 32)` aliases zero-cost (NO redundant `mov w, w`!).
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I32)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let k32 = b.iconst(Ty::I64, 32);
            let hi = b.binary(Op::Lshr, Flags::NONE, x, k32);
            let tr = b.convert(Op::Trunc, Flags::NONE, Ty::I32, hi);
            b.ret(&[tr]);

            let func = b.finish_checked().expect("lshr+trunc LIR builds");
            let verified = verify(func.view()).expect("lshr+trunc LIR verifies");
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit lshr+trunc cleanly");

            // Exact 2 instructions: `lsr x0, x0, #32; ret` (8 bytes, ZERO redundant `mov w0, w0`!).
            assert_eq!(
                emitted.code.len(),
                8,
                "expected 2 instructions (`lsr` + `ret`), got {} bytes",
                emitted.code.len()
            );

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64) -> u32`.
                let f: extern "C" fn(u64) -> u32 = unsafe { exec.as_fn() };
                for &xv in &[0u64, 0x1234_5678_9abc_def0, u64::MAX, 0x8000_0001_0000_0000] {
                    let got = u64::from(f(xv));
                    let want = eval_u64(func.view(), &[xv], alloc);
                    assert_eq!(got, want, "lshr+trunc diverged on {xv:#x}");
                }
            }
        }
    }

    #[test]
    fn misel_out_of_order_dominator_layout_avoids_undefined_vreg_on_apple_silicon() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        let b_def = b.create_block(BlockKind::Normal);
        let b_use = b.create_block(BlockKind::Cold);
        let b_alt = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let c = b.param(Ty::I64);
        b.jump((b_def, &[]));

        // `b_def` strictly dominates `b_use` and defines:
        // - a normal arithmetic VReg (`sum`),
        // - a zero-mov `Trunc(I64 -> I32)` alias (`tr`),
        // - a `FromExposed` + `PtrAddr` alias chain (`addr`),
        // - an `UaddOverflow` projected in `b_def` (`ovf1_val`),
        // - an `UaddOverflow` projected ONLY in `b_use` (`ovf2`, testing lazy `ovf_pair`),
        // - a materialized `Select(cmp, 1, 0)` I1 value (`sel_i1`).
        b.begin(b_def);
        let k0 = b.iconst(Ty::I64, 0);
        let k1 = b.iconst(Ty::I64, 1);
        let k2 = b.iconst(Ty::I64, 2);
        let k32 = b.iconst(Ty::I64, 32);
        let sum = b.binary(Op::Add, Flags::NONE, a, bv);
        let hi = b.binary(Op::Lshr, Flags::NONE, sum, k32);
        let tr = b.convert(Op::Trunc, Flags::NONE, Ty::I32, hi);
        let p = b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, c);
        let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p);
        let ovf1 = b.overflow(Op::UaddOverflow, a, bv);
        let ovf1_val = b.project(ovf1, 0);
        let ovf2 = b.overflow(Op::UaddOverflow, bv, sum);
        let cmp = b.icmp(IntCond::Ugt, a, bv);
        let sel_i1 = b.select(Flags::NONE, cmp, k1, k0);
        let bit = b.binary(Op::And, Flags::NONE, sum, k2);
        let go_use = b.icmp(IntCond::Eq, bit, k0);
        b.br(Flags::NONE, go_use, (b_use, &[]), (b_alt, &[]));

        // `b_use` is laid out BEFORE `b_def` below to trigger forward VReg references on every form.
        b.begin(b_use);
        let tr64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, tr);
        let ovf2_val = b.project(ovf2, 0);
        let ovf2_flag = b.project(ovf2, 1);
        let ovf2_flag64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, ovf2_flag);
        let x0 = b.binary(Op::Xor, Flags::NONE, sum, tr64);
        let x1 = b.binary(Op::Xor, Flags::NONE, x0, addr);
        let x2 = b.binary(Op::Xor, Flags::NONE, x1, ovf1_val);
        let x3 = b.binary(Op::Xor, Flags::NONE, x2, ovf2_val);
        let x4 = b.binary(Op::Add, Flags::NONE, x3, ovf2_flag64);
        let out_use = b.binary(Op::Add, Flags::NONE, x4, sel_i1);
        b.ret(&[out_use]);

        b.begin(b_alt);
        let out_alt = b.binary(Op::Xor, Flags::NONE, sum, ovf1_val);
        b.ret(&[out_alt]);

        let func = b.finish_checked().expect("out-of-order dominator LIR builds");
        let verified = verify(func.view()).expect("out-of-order dominator LIR verifies");

        // 1. Default full pipeline must succeed.
        let _ = emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
            .expect("default emit_mfunc_ssa_a64 must succeed");

        // 2. Hostile Ext-TSP layout: place `b_use` (block 2) BEFORE its dominator `b_def` (block 1)!
        let view = verified.view();
        let mut layout = super::compute_block_layout(&view, alloc);
        layout.order.clear();
        layout.order.push(0); // entry
        layout.order.push(2); // b_use BEFORE its strict dominator b_def!
        layout.order.push(1); // b_def
        layout.order.push(3); // b_alt

        let mut sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
            .expect("select_mfunc_ssa with dominated-before-dominator order must succeed");
        super::mverify(&sel.mfunc).expect("pre-mopt mverify must succeed");

        let target_model = super::TargetModel::from_machine(MAC_ARM64);
        let _ = super::optimize_mfunc(&mut sel.mfunc, alloc);
        super::mverify(&sel.mfunc).expect("post-mopt_pre mverify must succeed");
        let _ = super::schedule_mfunc(&mut sel.mfunc, target_model, alloc);
        super::mverify(&sel.mfunc).expect("post-msched_pre mverify must succeed");

        let mra_out = super::allocate_mfunc(
            &mut sel.mfunc,
            super::MRaOpts {
                allow_x29_leaf: !sel.has_calls && !sel.uses_fp,
                scratch_gpr_order: None,
                block_weights: &sel.block_weights,
                call_clobbers: &[],
                spill_base_off: sel.spill_base_off,
                fast_mode: false,
            },
            alloc,
        )
        .expect("mra must not fail with UndefinedVReg on dominated-before-dominator layout");

        let frame_info = super::insert_prologue_epilogue(
            &mut sel.mfunc,
            &mra_out,
            sel.spill_base_off,
            sel.has_calls,
            false,
            sel.ret_gpr_mask,
            sel.ret_fp_mask,
            &sel.incoming_stack_params,
            alloc,
        )
        .expect("prologue/epilogue insertion must succeed");

        let _ = super::optimize_mfunc(&mut sel.mfunc, alloc);
        let _ = super::schedule_mfunc(&mut sel.mfunc, target_model, alloc);
        super::mverify(&sel.mfunc).expect("post-RA mverify must succeed");
        let encoded = super::mencode(&sel.mfunc, alloc).expect("mencode must succeed");
        let emitted = super::build_emitted(
            &encoded,
            frame_info.unwind_frame,
            sel.targets,
            sel.relro,
            &sel.trap_kinds,
            &sel.unwind_entries,
            alloc,
        )
        .expect("build_emitted must succeed");
        assert!(!emitted.code.is_empty());

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                .expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, u64, u64) -> u64`.
            let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
            let cases: &[(u64, u64, u64)] = &[
                (0x1000_0000_0000_0004, 0x0000_0001_0000_0004, 0xdead_beef_cafe_0001), // b_use, a > b, no ovf
                (0x0000_0000_0000_0004, 0x2000_0000_0000_0004, 0x1234_5678_9abc_def0), // b_use, a <= b, no ovf
                (u64::MAX - 3, 4, 0xfeed_face_0000_0007),                                // b_use, ovf1 wraps, ovf2 wraps
                (0xffff_ffff_ffff_fff0, 0x8000_0000_0000_0010, 0x55aa_55aa_55aa_55aa), // b_use, ovf2 carries
                (1, 1, 0x9999_8888_7777_6666),                                          // b_alt (`sum & 2 != 0`)
            ];
            for &(av, bval, cv) in cases {
                let got = f(av, bval, cv);
                let want = eval_u64(func.view(), &[av, bval, cv], alloc);
                assert_eq!(got, want, "out-of-order dominator layout diverged on ({av:#x}, {bval:#x}, {cv:#x})");
            }
        }
    }

    #[test]
    fn misel_i128_q128_load_store_select_and_vec128_splat_on_apple_silicon() {
        let alloc = test_alloc();
        let v16x8 = Ty::vector(Scalar::I8, 16).expect("<16 x i8> valid");
        let v4x32 = Ty::vector(Scalar::I32, 4).expect("<4 x i32> valid");
        let v2x64 = Ty::vector(Scalar::I64, 2).expect("<2 x i64> valid");
        let v4xf32 = Ty::vector(Scalar::F32, 4).expect("<4 x f32> valid");

        // 1. Adjacent 16-byte unaligned `Load(Ty::I128)` -> `Store(Ty::I128)` lowers to Q128
        //    and pairs cleanly into `ldp q, q` / `stp q, q`.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let src = b.param(Ty::PTR);
            let dst = b.param(Ty::PTR);
            let q0 = b.load(Ty::I128, Flags::NONE, src, 0, MemAttrs::aligned(0));
            let q1 = b.load(Ty::I128, Flags::NONE, src, 16, MemAttrs::aligned(0));
            b.store(Flags::NONE, q0, dst, 0, MemAttrs::aligned(0));
            b.store(Flags::NONE, q1, dst, 16, MemAttrs::aligned(0));
            b.ret(&[]);

            let func = b.finish_checked().expect("i128 32B copy LIR builds");
            let verified = verify(func.view()).expect("i128 32B copy LIR verifies");
            let (emitted, _clobbers, stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit 16B/32B I128 load/store as Q128");
            assert!(!emitted.code.is_empty());
            assert!(
                stats.mopt_pre.pairs_formed + stats.mopt_post.pairs_formed >= 2,
                "expected adjacent Q128 loads & stores to pair into ldp/stp, got pre={} post={}",
                stats.mopt_pre.pairs_formed,
                stats.mopt_post.pairs_formed
            );

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(*const u8, *mut u8)`.
                let f: extern "C" fn(*const u8, *mut u8) = unsafe { exec.as_fn() };
                let mut src_buf = [0u8; 40];
                for (i, slot) in src_buf.iter_mut().enumerate() {
                    let b_val = u8::try_from((i.wrapping_mul(37).wrapping_add(13)) & 0xff).unwrap_or(0);
                    *slot = b_val;
                }
                let mut dst_buf = [0xaau8; 40];
                // Unaligned 1-byte offset to verify unaligned Q128 wild-copy semantics.
                f(src_buf.as_ptr().wrapping_add(1), dst_buf.as_mut_ptr().wrapping_add(1));
                assert_eq!(dst_buf[0], 0xaa, "byte before dst must remain untouched");
                assert_eq!(&dst_buf[1..33], &src_buf[1..33], "all 32 bytes must copy bit-for-bit");
                assert_eq!(&dst_buf[33..40], &[0xaa; 7], "trailing guard bytes must remain untouched");
            }
        }

        // 2. `Select` on `Ty::I128` + `Bitcast(I128 -> <16 x i8>)` + `Store(<16 x i8>)`.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR)],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let cond_i64 = b.param(Ty::I64);
            let pa = b.param(Ty::PTR);
            let pb = b.param(Ty::PTR);
            let pout = b.param(Ty::PTR);
            let k0 = b.iconst(Ty::I64, 0);
            let cond = b.icmp(IntCond::Ne, cond_i64, k0);
            let a128 = b.load(Ty::I128, Flags::NONE, pa, 0, MemAttrs::aligned(0));
            let b128 = b.load(Ty::I128, Flags::NONE, pb, 0, MemAttrs::aligned(0));
            let picked = b.select(Flags::NONE, cond, a128, b128);
            let as_vec = b.convert(Op::Bitcast, Flags::NONE, v16x8, picked);
            b.store(Flags::NONE, as_vec, pout, 0, MemAttrs::aligned(0));
            b.ret(&[]);

            let func = b.finish_checked().expect("i128 select+bitcast LIR builds");
            let verified = verify(func.view()).expect("i128 select+bitcast LIR verifies");
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit I128 select + bitcast cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, *const u8, *const u8, *mut u8)`.
                let f: extern "C" fn(u64, *const u8, *const u8, *mut u8) = unsafe { exec.as_fn() };
                let a_bytes: [u8; 16] =
                    [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];
                let b_bytes: [u8; 16] =
                    [0xa5, 0x5a, 0xf0, 0x0f, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc];
                let mut out_bytes = [0u8; 16];
                f(1, a_bytes.as_ptr(), b_bytes.as_ptr(), out_bytes.as_mut_ptr());
                assert_eq!(out_bytes, a_bytes, "cond != 0 must select a128");
                f(0, a_bytes.as_ptr(), b_bytes.as_ptr(), out_bytes.as_mut_ptr());
                assert_eq!(out_bytes, b_bytes, "cond == 0 must select b128");
            }
        }

        // 3. `Op::Splat` for `<16 x i8>`, `<4 x i32>`, `<2 x i64>`, `<4 x f32>`, and constant-zero `<16 x i8>`.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I8),
                    Param::of(Ty::I32),
                    Param::of(Ty::I64),
                    Param::of(Ty::F32),
                    Param::of(Ty::PTR),
                ],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x8 = b.param(Ty::I8);
            let x32 = b.param(Ty::I32);
            let x64 = b.param(Ty::I64);
            let xf32 = b.param(Ty::F32);
            let out = b.param(Ty::PTR);
            let s8 = b.convert(Op::Splat, Flags::NONE, v16x8, x8);
            let s32 = b.convert(Op::Splat, Flags::NONE, v4x32, x32);
            let s64 = b.convert(Op::Splat, Flags::NONE, v2x64, x64);
            let sf32 = b.convert(Op::Splat, Flags::NONE, v4xf32, xf32);
            let z8 = b.iconst(Ty::I8, 0);
            let sz = b.convert(Op::Splat, Flags::NONE, v16x8, z8);
            b.store(Flags::NONE, s8, out, 0, MemAttrs::aligned(0));
            b.store(Flags::NONE, s32, out, 16, MemAttrs::aligned(0));
            b.store(Flags::NONE, s64, out, 32, MemAttrs::aligned(0));
            b.store(Flags::NONE, sf32, out, 48, MemAttrs::aligned(0));
            b.store(Flags::NONE, sz, out, 64, MemAttrs::aligned(0));
            b.ret(&[]);

            let func = b.finish_checked().expect("splat LIR builds");
            let verified = verify(func.view()).expect("splat LIR verifies");
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit Vec128 Op::Splat cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u8, u32, u64, f32, *mut u8)`.
                let f: extern "C" fn(u8, u32, u64, f32, *mut u8) = unsafe { exec.as_fn() };
                let mut out_buf = [0x55u8; 80];
                let lane8 = 0x3cu8;
                let lane32 = 0x1234_5678u32;
                let lane64 = 0x0123_4567_89ab_cdefu64;
                let lanef32 = 3.25f32;
                f(lane8, lane32, lane64, lanef32, out_buf.as_mut_ptr());

                assert_eq!(&out_buf[0..16], &[lane8; 16], "<16 x i8> splat mismatch");
                let b32 = lane32.to_le_bytes();
                for lane in 0..4usize {
                    let off = 16 + lane * 4;
                    assert_eq!(&out_buf[off..off + 4], &b32, "<4 x i32> splat lane {lane} mismatch");
                }
                let b64 = lane64.to_le_bytes();
                for lane in 0..2usize {
                    let off = 32 + lane * 8;
                    assert_eq!(&out_buf[off..off + 8], &b64, "<2 x i64> splat lane {lane} mismatch");
                }
                let bf32 = lanef32.to_le_bytes();
                for lane in 0..4usize {
                    let off = 48 + lane * 4;
                    assert_eq!(&out_buf[off..off + 4], &bf32, "<4 x f32> splat lane {lane} mismatch");
                }
                assert_eq!(&out_buf[64..80], &[0u8; 16], "zero-const <16 x i8> splat mismatch");
            }
        }

        // 4. `I128` arithmetic and `I128` in function signatures remain cleanly refused.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(32, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let p = b.param(Ty::PTR);
            let x = b.load(Ty::I128, Flags::NONE, p, 0, MemAttrs::aligned(0));
            let s = b.binary(Op::Add, Flags::NONE, x, x);
            b.store(Flags::NONE, s, p, 0, MemAttrs::aligned(0));
            b.ret(&[]);
            let func = b.finish_checked().expect("i128 add LIR builds");
            let verified = verify(func.view()).expect("i128 add LIR verifies");
            assert!(
                matches!(
                    emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc),
                    Err(TierError::Refused { .. })
                ),
                "I128 arithmetic must remain refused"
            );
        }
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(32, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I128)], &[Param::of(Ty::I128)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I128);
            b.ret(&[x]);
            let func = b.finish_checked().expect("i128 sig LIR builds");
            let verified = verify(func.view()).expect("i128 sig LIR verifies");
            assert!(
                matches!(
                    emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc),
                    Err(TierError::Refused { .. })
                ),
                "I128 in function signature must remain refused"
            );
        }
    }

    #[test]
    fn misel_shift_mask_elision_eliminates_and_mask_and_matches_eval_on_apple_silicon() {
        let alloc = test_alloc();

        // 1. `Shl(x: i64, And(y: i64, 63))` emits ZERO `and #0x3f` and matches LIR eval.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
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
            let k63 = b.iconst(Ty::I64, 63);
            let masked = b.binary(Op::And, Flags::NONE, y, k63);
            let sh = b.binary(Op::Shl, Flags::NONE, x, masked);
            b.ret(&[sh]);

            let func = b.finish_checked().expect("shl+and63 LIR builds");
            let verified = verify(func.view()).expect("shl+and63 LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");
            let and_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::LogicalImm { kind: super::LogicalKind::And, .. }))
                .count();
            assert_eq!(and_count, 0, "expected zero `and #0x3f` ops for Shl(x, And(y, 63))");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit shl+and63 cleanly");
            assert_eq!(
                emitted.code.len(),
                8,
                "expected exact 2 instructions (`lslv x0, x0, x1; ret`), got {} bytes",
                emitted.code.len()
            );

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, u64) -> u64`.
                let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
                for &(xv, yv) in &[
                    (0x0123_4567_89ab_cdefu64, 0u64),
                    (0x0123_4567_89ab_cdef, 1),
                    (0x0123_4567_89ab_cdef, 63),
                    (0x0123_4567_89ab_cdef, 64),
                    (0x0123_4567_89ab_cdef, 65),
                    (0xdead_beef_cafe_babe, 131),
                    (u64::MAX, u64::MAX),
                ] {
                    let got = f(xv, yv);
                    let want = eval_u64(func.view(), &[xv, yv], alloc);
                    assert_eq!(got, want, "Shl(x, And(y, 63)) diverged on ({xv:#x}, {yv:#x})");
                }
            }
        }

        // 2. `Lshr(x: i64, Zext(And(w: i32, 63)))` emits ZERO `and #0x3f` and matches LIR eval.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I32)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let w = b.param(Ty::I32);
            let k63 = b.iconst(Ty::I32, 63);
            let masked = b.binary(Op::And, Flags::NONE, w, k63);
            let ext = b.convert(Op::Zext, Flags::NONE, Ty::I64, masked);
            let sh = b.binary(Op::Lshr, Flags::NONE, x, ext);
            b.ret(&[sh]);

            let func = b.finish_checked().expect("lshr+zext+and63 LIR builds");
            let verified = verify(func.view()).expect("lshr+zext+and63 LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");
            let and_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::LogicalImm { kind: super::LogicalKind::And, .. }))
                .count();
            assert_eq!(and_count, 0, "expected zero `and #0x3f` ops for Lshr(x, Zext(And(w, 63)))");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit lshr+zext+and63 cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, u32) -> u64`.
                let f: extern "C" fn(u64, u32) -> u64 = unsafe { exec.as_fn() };
                for &(xv, wv) in &[
                    (0xfedc_ba98_7654_3210u64, 0u32),
                    (0xfedc_ba98_7654_3210, 7),
                    (0xfedc_ba98_7654_3210, 31),
                    (0xfedc_ba98_7654_3210, 32),
                    (0xfedc_ba98_7654_3210, 63),
                    (0xfedc_ba98_7654_3210, 64),
                    (0xfedc_ba98_7654_3210, 129),
                    (0x8000_0000_0000_0001, u32::MAX),
                ] {
                    let got = f(xv, wv);
                    let want = eval_u64(func.view(), &[xv, u64::from(wv)], alloc);
                    assert_eq!(
                        got, want,
                        "Lshr(x, Zext(And(w, 63))) diverged on ({xv:#x}, {wv:#x})"
                    );
                }
            }
        }

        // 3. Narrower mask `Shl(x: i64, And(y: i64, 31))` MUST keep `and #31` (soundness guard!).
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
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
            let k31 = b.iconst(Ty::I64, 31);
            let masked = b.binary(Op::And, Flags::NONE, y, k31);
            let sh = b.binary(Op::Shl, Flags::NONE, x, masked);
            b.ret(&[sh]);

            let func = b.finish_checked().expect("shl+and31 LIR builds");
            let verified = verify(func.view()).expect("shl+and31 LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");
            let and_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        super::MOp::LogicalImm { kind: super::LogicalKind::And, imm: 31, .. }
                    )
                })
                .count();
            assert_eq!(and_count, 1, "narrow mask `and #31` on 64-bit shift must NOT be elided");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit shl+and31 cleanly");
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, u64) -> u64`.
                let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
                for &(xv, yv) in &[(1u64, 32u64), (7, 33), (0x1234, 63), (0xabcd, 31)] {
                    let got = f(xv, yv);
                    let want = eval_u64(func.view(), &[xv, yv], alloc);
                    assert_eq!(got, want, "Shl(x, And(y, 31)) diverged on ({xv:#x}, {yv:#x})");
                }
            }
        }
    }

    #[test]
    fn misel_shared_and_sibling_icmp_selects_emit_single_cmp_on_apple_silicon() {
        let alloc = test_alloc();

        // 1. Two `Select`s sharing the same `Icmp` separated by non-flag-clobbering ALU emit ONE `Cmp`.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let tag = b.param(Ty::I64);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let k2 = b.iconst(Ty::I64, 2);
            let k4 = b.iconst(Ty::I64, 4);
            let k_ff = b.iconst(Ty::I64, 0xff);
            let is_two = b.icmp(IntCond::Eq, tag, k2);
            let sh = b.binary(Op::Lshr, Flags::NONE, a, k4);
            let s1 = b.select(Flags::NONE, is_two, sh, bv);
            let sum = b.binary(Op::Add, Flags::NONE, s1, a);
            let masked = b.binary(Op::And, Flags::NONE, sum, k_ff);
            let s2 = b.select(Flags::NONE, is_two, masked, bv);
            let out = b.binary(Op::Add, Flags::NONE, s1, s2);
            b.ret(&[out]);

            let func = b.finish_checked().expect("shared-icmp select LIR builds");
            let verified = verify(func.view()).expect("shared-icmp select LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");
            let cmp_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::CmpImm { .. } | super::MOp::CmpReg { .. }))
                .count();
            assert_eq!(cmp_count, 1, "two Selects sharing one Icmp must emit only ONE Cmp");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit shared-icmp selects cleanly");
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, u64, u64) -> u64`.
                let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
                for &(t, av, bval) in &[(0u64, 0x1234u64, 99u64), (2, 0x1234, 99), (3, 7, 13), (2, u64::MAX, 5)] {
                    let got = f(t, av, bval);
                    let want = eval_u64(func.view(), &[t, av, bval], alloc);
                    assert_eq!(got, want, "shared-icmp selects diverged on ({t}, {av:#x}, {bval:#x})");
                }
            }
        }

        // 2. Sibling `Icmp(Eq, tag, 2)` and `Icmp(Ne, tag, 2)` separated by non-flag-clobbering
        //    ops reuse the SAME live NZCV flags (ZERO duplicate `cmp w, #2`!).
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I32), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let tag = b.param(Ty::I32);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let k2_a = b.iconst(Ty::I32, 2);
            let k2_b = b.iconst(Ty::I32, 2);
            let k8 = b.iconst(Ty::I64, 8);
            let eq2 = b.icmp(IntCond::Eq, tag, k2_a);
            let s1 = b.select(Flags::NONE, eq2, a, bv);
            let ne2 = b.icmp(IntCond::Ne, tag, k2_b);
            let shifted = b.binary(Op::Lshr, Flags::NONE, a, k8);
            let s2 = b.select(Flags::NONE, ne2, shifted, bv);
            let s3 = b.select(Flags::NONE, ne2, s1, s2);
            let out = b.binary(Op::Xor, Flags::NONE, s2, s3);
            b.ret(&[out]);

            let func = b.finish_checked().expect("sibling-icmp LIR builds");
            let verified = verify(func.view()).expect("sibling-icmp LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");
            let cmp_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::CmpImm { .. } | super::MOp::CmpReg { .. }))
                .count();
            assert_eq!(
                cmp_count, 1,
                "sibling Icmp(Eq, tag, 2) and Icmp(Ne, tag, 2) must share ONE Cmp"
            );

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit sibling-icmp selects cleanly");
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u32, u64, u64) -> u64`.
                let f: extern "C" fn(u32, u64, u64) -> u64 = unsafe { exec.as_fn() };
                for &(t, av, bval) in &[(0u32, 0x123456u64, 42u64), (1, 99, 77), (2, 0x123456, 42), (3, 0xdead, 0xbeef)] {
                    let got = f(t, av, bval);
                    let want = eval_u64(func.view(), &[u64::from(t), av, bval], alloc);
                    assert_eq!(got, want, "sibling-icmp diverged on ({t}, {av:#x}, {bval:#x})");
                }
            }
        }
    }

    #[test]
    fn misel_br_of_icmp_across_load_fuses_to_cmp_bcond_on_apple_silicon() {
        let alloc = test_alloc();

        // Test BOTH direct `Br(Icmp(Ult, a, b))` AND inverted-wrapper `Br(Icmp(Eq, Icmp(Ult, a, b), 0))`
        // separated from the branch by an intervening `Load` (the `lz77_scan_fast_leaf` pattern).
        for wrap_invert in [false, true] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let slot = b.slot(16, 3, SlotKind::Temp);
            let entry = b.create_block(BlockKind::Normal);
            let bb_t = b.create_block(BlockKind::Normal);
            let bb_f = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let payload = b.param(Ty::I64);
            let sp = b.slot_addr(slot);
            b.store(Flags::NONE, payload, sp, 0, MemAttrs::aligned(3));
            let cmp = b.icmp(IntCond::Ult, a, bv);
            let loaded = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(3));
            let br_cond = if wrap_invert {
                let k0 = b.iconst(Ty::I1, 0);
                b.icmp(IntCond::Eq, cmp, k0)
            } else {
                cmp
            };
            b.br(Flags::NONE, br_cond, (bb_t, &[]), (bb_f, &[]));

            b.begin(bb_t);
            let t_res = b.binary(Op::Add, Flags::NONE, loaded, a);
            b.ret(&[t_res]);

            b.begin(bb_f);
            let f_res = b.binary(Op::Xor, Flags::NONE, loaded, bv);
            b.ret(&[f_res]);

            let func = b.finish_checked().expect("br+load+icmp LIR builds");
            let verified = verify(func.view()).expect("br+load+icmp LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");

            let csinc_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::Csinc { .. }))
                .count();
            let cbz_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::Cbz { .. } | super::MOp::Cbnz { .. }))
                .count();
            let bcond_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::BCond { .. }))
                .count();
            assert_eq!(csinc_count, 0, "wrap_invert={wrap_invert}: must emit ZERO cset/csinc");
            assert_eq!(cbz_count, 0, "wrap_invert={wrap_invert}: must emit ZERO cbz/cbnz");
            assert_eq!(bcond_count, 1, "wrap_invert={wrap_invert}: must fuse into Cmp + BCond");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit br+load+icmp cleanly");

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, u64, u64) -> u64`.
                let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
                for &(av, bval, pv) in &[
                    (3u64, 10u64, 0x1000u64),
                    (10, 10, 0x2000),
                    (20, 10, 0x3000),
                    (0, u64::MAX, 0x55aa),
                    (u64::MAX, 0, 0xaa55),
                ] {
                    let got = f(av, bval, pv);
                    let want = eval_u64(func.view(), &[av, bval, pv], alloc);
                    assert_eq!(
                        got, want,
                        "br+load+icmp (wrap_invert={wrap_invert}) diverged on ({av}, {bval}, {pv:#x})"
                    );
                }
            }
        }
    }

    #[test]
    fn misel_internal_fast_12_gpr_args_zero_stack_loads_and_executes_on_apple_silicon() {
        let alloc = test_alloc();

        // 1. Build 12-GPR-arg `InternalFast` callee returning `sum_{k=0..12} p_k * (k + 1)`.
        let mut cb = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let callee_params = [Param::of(Ty::I64); 12];
        let callee_sig = cb.sig(Conv::C, Variadic::No, &callee_params, &[Param::of(Ty::I64)]);
        cb.set_signature(callee_sig);
        let c_entry = cb.create_block(BlockKind::Normal);
        cb.begin(c_entry);
        let mut pvals: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(12, alloc);
        for _ in 0..12 {
            pvals.push(cb.param(Ty::I64));
        }
        let mut acc = pvals[0];
        for k in 1usize..12usize {
            let weight = cb.iconst(Ty::I64, u128::try_from(k + 1).unwrap_or(1));
            let term = cb.binary(Op::Mul, Flags::NONE, pvals[k], weight);
            acc = cb.binary(Op::Add, Flags::NONE, acc, term);
        }
        cb.ret(&[acc]);

        let callee_func = cb.finish_checked().expect("12-arg callee LIR builds");
        let callee_verified = verify(callee_func.view()).expect("12-arg callee LIR verifies");
        let callee_opts = MIseOpts {
            internal_fast: true,
            ..default_opts()
        };

        // Verify ZERO incoming stack params and ZERO `Ldr` instructions in the `InternalFast` callee!
        let callee_view = callee_verified.view();
        let callee_layout = super::compute_block_layout(&callee_view, alloc);
        let callee_sel = super::select_mfunc_ssa(&callee_view, &NoNames, &callee_layout, callee_opts, alloc)
            .expect("InternalFast callee select_mfunc_ssa must succeed");
        assert!(
            callee_sel.incoming_stack_params.is_empty(),
            "12-GPR-arg InternalFast callee must have ZERO incoming stack params"
        );
        let callee_ldr_count = callee_sel
            .mfunc
            .ops
            .iter()
            .filter(|op| matches!(op, super::MOp::Ldr { .. } | super::MOp::Ldp { .. }))
            .count();
        assert_eq!(callee_ldr_count, 0, "12-GPR-arg InternalFast callee must emit ZERO stack loads");

        let (callee_emitted, _callee_clobbers, _callee_stats) =
            emit_mfunc_ssa_a64(callee_verified, &NoNames, callee_opts, alloc)
                .expect("InternalFast 12-GPR-arg callee must emit cleanly");
        assert!(!callee_emitted.code.is_empty());

        // 2. Build AAPCS64 caller `extern "C" fn(a0, a1, a2, a3) -> u64` invoking the 12-arg
        //    `InternalFast` callee with ZERO outgoing stack-arg stores!
        struct InternalCalleeNames;
        impl Names for InternalCalleeNames {
            fn name(&self, of: Outside) -> Option<Named<'_>> {
                match of {
                    Outside::Func(0) => Some(Named::Symbol(Symbol::of_name("internal_fast_12_callee"))),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }
        struct ExternCNames;
        impl Names for ExternCNames {
            fn name(&self, of: Outside) -> Option<Named<'_>> {
                match of {
                    Outside::Func(0) => Some(Named::C("extern_c_fn")),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let mut caller_b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let caller_sig = caller_b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        let call_sig = caller_b.sig(Conv::C, Variadic::No, &callee_params, &[Param::of(Ty::I64)]);
        caller_b.set_signature(caller_sig);
        let callee_ref = caller_b.reference(RefKind::Func, 0);
        let entry = caller_b.create_block(BlockKind::Normal);
        caller_b.begin(entry);
        let a0 = caller_b.param(Ty::I64);
        let a1 = caller_b.param(Ty::I64);
        let a2 = caller_b.param(Ty::I64);
        let a3 = caller_b.param(Ty::I64);
        let k1 = caller_b.iconst(Ty::I64, 1);
        let k2 = caller_b.iconst(Ty::I64, 2);
        let k3 = caller_b.iconst(Ty::I64, 3);
        let k5 = caller_b.iconst(Ty::I64, 5);
        let a4 = caller_b.binary(Op::Add, Flags::NONE, a0, k1);
        let a5 = caller_b.binary(Op::Add, Flags::NONE, a1, k2);
        let a6 = caller_b.binary(Op::Add, Flags::NONE, a2, k3);
        let a7 = caller_b.binary(Op::Add, Flags::NONE, a3, k5);
        let a8 = caller_b.binary(Op::Xor, Flags::NONE, a0, a1);
        let a9 = caller_b.binary(Op::Xor, Flags::NONE, a2, a3);
        let a10 = caller_b.binary(Op::Add, Flags::NONE, a0, a2);
        let a11 = caller_b.binary(Op::Add, Flags::NONE, a1, a3);
        let call_val = caller_b.call(
            Flags::NONE,
            callee_ref,
            call_sig,
            &[a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11],
        );
        caller_b.ret(&[call_val]);

        let caller_func = caller_b.finish_checked().expect("12-arg caller LIR builds");
        let caller_verified = verify(caller_func.view()).expect("12-arg caller LIR verifies");
        let fast_call_list = [call_val.0];
        let caller_opts = MIseOpts {
            internal_fast_calls: &fast_call_list,
            ..default_opts()
        };

        // Guard: marking an `extern "C"` (`Named::C`) call as `InternalFast` MUST be refused!
        assert!(
            matches!(
                emit_mfunc_ssa_a64(caller_verified, &ExternCNames, caller_opts, alloc),
                Err(TierError::Refused { .. })
            ),
            "InternalFast on extern C call must be refused"
        );

        let caller_view = caller_verified.view();
        let caller_layout = super::compute_block_layout(&caller_view, alloc);
        let caller_sel = super::select_mfunc_ssa(
            &caller_view,
            &InternalCalleeNames,
            &caller_layout,
            caller_opts,
            alloc,
        )
        .expect("InternalFast caller select_mfunc_ssa must succeed");
        assert_eq!(
            caller_sel.spill_base_off, 0,
            "12-GPR-arg InternalFast call must allocate ZERO outgoing stack-arg bytes"
        );
        let caller_str_count = caller_sel
            .mfunc
            .ops
            .iter()
            .filter(|op| matches!(op, super::MOp::Str { .. } | super::MOp::Stp { .. }))
            .count();
        assert_eq!(caller_str_count, 0, "12-GPR-arg InternalFast call must emit ZERO stack arg stores");

        let (caller_emitted, _caller_clobbers, _caller_stats) =
            emit_mfunc_ssa_a64(caller_verified, &InternalCalleeNames, caller_opts, alloc)
                .expect("InternalFast caller must emit cleanly");
        assert_eq!(caller_emitted.relocs.len(), 1);
        assert_eq!(caller_emitted.relocs[0].kind, RelocKind::Branch26);

        // 3. Link caller (at offset 0) + callee (16B-aligned) and run end-to-end on Apple Silicon!
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let callee_off = (caller_emitted.code.len() + 15) & !15;
            let total_len = callee_off + callee_emitted.code.len();
            let mut combined: Vec<u8, Alloc<'static>> = Vec::with_capacity_in(total_len, alloc);
            for &byte in caller_emitted.code.iter() {
                combined.push(byte);
            }
            while combined.len() < callee_off {
                combined.push(0);
            }
            for &byte in callee_emitted.code.iter() {
                combined.push(byte);
            }

            let reloc_at = usize::try_from(caller_emitted.relocs[0].at).expect("reloc offset fits");
            let delta_bytes = callee_off.checked_sub(reloc_at).expect("callee follows caller");
            let delta_words = i32::try_from(delta_bytes / 4).expect("Branch26 delta fits i32");
            let bl_word = crate::enc::bl(delta_words).to_le_bytes();
            combined[reloc_at..reloc_at + 4].copy_from_slice(&bl_word);

            let exec = blaze_inproc::exec::Executable::new(&combined, 0)
                .expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 caller invoking our linked `InternalFast` callee.
            let f: extern "C" fn(u64, u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
            for &(v0, v1, v2, v3) in &[
                (1u64, 2u64, 3u64, 4u64),
                (10, 20, 30, 40),
                (0x1111_2222, 0x3333_4444, 0x5555_6666, 0x7777_8888),
                (u64::MAX, 0, 42, 0xdead_beef),
            ] {
                let args: [u64; 12] = [
                    v0,
                    v1,
                    v2,
                    v3,
                    v0.wrapping_add(1),
                    v1.wrapping_add(2),
                    v2.wrapping_add(3),
                    v3.wrapping_add(5),
                    v0 ^ v1,
                    v2 ^ v3,
                    v0.wrapping_add(v2),
                    v1.wrapping_add(v3),
                ];
                let mut want = 0u64;
                for (k, &arg) in args.iter().enumerate() {
                    let w = u64::try_from(k + 1).unwrap_or(1);
                    want = want.wrapping_add(arg.wrapping_mul(w));
                }
                let got = f(v0, v1, v2, v3);
                assert_eq!(
                    got, want,
                    "InternalFast 12-GPR-arg call diverged on ({v0:#x}, {v1:#x}, {v2:#x}, {v3:#x})"
                );
            }
        }
    }

    #[test]
    fn misel_multi_use_scaled_idx_load_store_folds_zero_add_on_apple_silicon() {
        let alloc = test_alloc();

        // 1. Multi-use `ScaledIdx` (`use_count == 2`: `Load(p)` + `Store(p)`):
        //    `p = ptr_add(base, idx << 2); old = load(p); store(p, new); return old`
        //    must emit ZERO `AddReg`/`AddExt`/`Ubfm` and both `Ldr(ScaledIdx)` + `Str(ScaledIdx)`!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let base = b.param(Ty::PTR);
            let idx = b.param(Ty::I64);
            let new_val = b.param(Ty::I32);
            let k2 = b.iconst(Ty::I64, 2);
            let byte_off = b.binary(Op::Shl, Flags::NONE, idx, k2);
            let p = b.ptr_add(Flags::NONE, base, byte_off);
            let old = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
            b.store(Flags::NONE, new_val, p, 0, MemAttrs::aligned(2));
            b.ret(&[old]);

            let func = b.finish_checked().expect("multi-use scaled-idx LIR builds");
            let verified = verify(func.view()).expect("multi-use scaled-idx LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");

            let add_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::AddReg { .. } | super::MOp::AddExt { .. }))
                .count();
            let shift_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::Ubfm { .. }))
                .count();
            let ldr_scaled = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        super::MOp::Ldr {
                            addr: super::AddrMode::ScaledIdx { shift: 2, .. },
                            size: super::MemSize::W32,
                            ..
                        }
                    )
                })
                .count();
            let str_scaled = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        super::MOp::Str {
                            addr: super::AddrMode::ScaledIdx { shift: 2, .. },
                            size: super::MemSize::W32,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(add_count, 0, "multi-use Load+Store PtrAdd must emit ZERO AddReg/AddExt");
            assert_eq!(shift_count, 0, "folded Shl(idx, 2) must emit ZERO Ubfm/lsl");
            assert!(ldr_scaled >= 1, "expected >= 1 Ldr(ScaledIdx shift=2), got {ldr_scaled}");
            assert!(str_scaled >= 1, "expected >= 1 Str(ScaledIdx shift=2), got {str_scaled}");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit multi-use ScaledIdx cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(*mut u32, u64, u32) -> u32`.
                let f: extern "C" fn(*mut u32, u64, u32) -> u32 = unsafe { exec.as_fn() };
                let mut table: [u32; 8] = [10, 20, 30, 40, 50, 60, 70, 80];
                assert_eq!(f(table.as_mut_ptr(), 3, 0xdead_beef), 40);
                assert_eq!(table[3], 0xdead_beef);
                assert_eq!(f(table.as_mut_ptr(), 3, 999), 0xdead_beef);
                assert_eq!(table[3], 999);
                assert_eq!(f(table.as_mut_ptr(), 0, 111), 10);
                assert_eq!(table[0], 111);
                assert_eq!(f(table.as_mut_ptr(), 7, 777), 80);
                assert_eq!(table[7], 777);
                assert_eq!(table[1], 20);
                assert_eq!(table[6], 70);
            }
        }

        // 2. Multi-use `BaseOff` (`use_count == 2`: `Load(p)` + `Store(p)`):
        //    `p = ptr_add(base, 12); old = load(p); store(p, new); return old`
        //    must emit ZERO `AddImm`/`AddReg`/`AddExt` and fold both into `BaseOff { off: 12 }`!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I32)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let base = b.param(Ty::PTR);
            let new_val = b.param(Ty::I32);
            let k12 = b.iconst(Ty::I64, 12);
            let p = b.ptr_add(Flags::NONE, base, k12);
            let old = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
            b.store(Flags::NONE, new_val, p, 0, MemAttrs::aligned(2));
            b.ret(&[old]);

            let func = b.finish_checked().expect("multi-use base-off LIR builds");
            let verified = verify(func.view()).expect("multi-use base-off LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");

            let add_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        super::MOp::AddImm { .. }
                            | super::MOp::AddReg { .. }
                            | super::MOp::AddExt { .. }
                    )
                })
                .count();
            let ldr_base_off = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        super::MOp::Ldr {
                            addr: super::AddrMode::BaseOff { off: 12, .. },
                            size: super::MemSize::W32,
                            ..
                        }
                    )
                })
                .count();
            let str_base_off = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| {
                    matches!(
                        op,
                        super::MOp::Str {
                            addr: super::AddrMode::BaseOff { off: 12, .. },
                            size: super::MemSize::W32,
                            ..
                        }
                    )
                })
                .count();
            assert_eq!(add_count, 0, "multi-use Load+Store BaseOff must emit ZERO Add");
            assert!(ldr_base_off >= 1, "expected >= 1 Ldr(BaseOff off=12)");
            assert!(str_base_off >= 1, "expected >= 1 Str(BaseOff off=12)");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit multi-use BaseOff cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(*mut u32, u32) -> u32`.
                let f: extern "C" fn(*mut u32, u32) -> u32 = unsafe { exec.as_fn() };
                let mut table: [u32; 4] = [1, 2, 3, 4];
                assert_eq!(f(table.as_mut_ptr(), 42), 4);
                assert_eq!(table[3], 42);
                assert_eq!(f(table.as_mut_ptr(), 99), 42);
                assert_eq!(table[3], 99);
            }
        }

        // 3. Guard: if `p` is ALSO stored as a value operand (`Store(val=p, ptr=out_ptr)`),
        //    `p` MUST NOT be skipped!
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I32), Param::of(Ty::PTR)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let base = b.param(Ty::PTR);
            let idx = b.param(Ty::I64);
            let new_val = b.param(Ty::I32);
            let out_ptr = b.param(Ty::PTR);
            let k2 = b.iconst(Ty::I64, 2);
            let byte_off = b.binary(Op::Shl, Flags::NONE, idx, k2);
            let p = b.ptr_add(Flags::NONE, base, byte_off);
            let old = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
            b.store(Flags::NONE, new_val, p, 0, MemAttrs::aligned(2));
            b.store(Flags::NONE, p, out_ptr, 0, MemAttrs::aligned(3));
            b.ret(&[old]);

            let func = b.finish_checked().expect("guarded ptr value-store LIR builds");
            let verified = verify(func.view()).expect("guarded ptr value-store LIR verifies");
            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit guarded ptr value-store cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(*mut u32, u64, u32, *mut *mut u32) -> u32`.
                let f: extern "C" fn(*mut u32, u64, u32, *mut *mut u32) -> u32 =
                    unsafe { exec.as_fn() };
                let mut table: [u32; 4] = [100, 200, 300, 400];
                let mut saved: *mut u32 = core::ptr::null_mut();
                assert_eq!(f(table.as_mut_ptr(), 2, 555, &mut saved), 300);
                assert_eq!(table[2], 555);
                assert_eq!(saved, table.as_mut_ptr().wrapping_add(2));
            }
        }
    }

    #[test]
    fn misel_and_mask_zero_branch_fuses_to_tst_imm_bcond_on_apple_silicon() {
        let alloc = test_alloc();
        const MASK: u64 = 0x00ff_ffff_ffff_ffff;

        for cond in [IntCond::Ne, IntCond::Eq] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let bb_t = b.create_block(BlockKind::Normal);
            let bb_f = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let x = b.param(Ty::I64);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let mask_v = b.iconst(Ty::I64, u128::from(MASK));
            let zero_v = b.iconst(Ty::I64, 0);
            let masked = b.binary(Op::And, Flags::NONE, x, mask_v);
            let cmp = b.icmp(cond, masked, zero_v);
            b.br(Flags::NONE, cmp, (bb_t, &[]), (bb_f, &[]));

            b.begin(bb_t);
            let t_res = b.binary(Op::Add, Flags::NONE, a, bv);
            b.ret(&[t_res]);

            b.begin(bb_f);
            let f_res = b.binary(Op::Xor, Flags::NONE, a, bv);
            b.ret(&[f_res]);

            let func = b.finish_checked().expect("and-mask-zero branch LIR builds");
            let verified = verify(func.view()).expect("and-mask-zero branch LIR verifies");
            let view = verified.view();
            let layout = super::compute_block_layout(&view, alloc);
            let sel = super::select_mfunc_ssa(&view, &NoNames, &layout, default_opts(), alloc)
                .expect("select_mfunc_ssa must succeed");

            let logical_imm_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::LogicalImm { .. }))
                .count();
            let tst_imm_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::TstImm { imm: MASK, wide: true, .. }))
                .count();
            let cbz_cbnz_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::Cbz { .. } | super::MOp::Cbnz { .. }))
                .count();
            let bcond_count = sel
                .mfunc
                .ops
                .iter()
                .filter(|op| matches!(op, super::MOp::BCond { .. }))
                .count();
            assert_eq!(logical_imm_count, 0, "cond={cond:?}: must emit ZERO LogicalImm (and)");
            assert!(tst_imm_count >= 1, "cond={cond:?}: must emit >= 1 TstImm");
            assert_eq!(cbz_cbnz_count, 0, "cond={cond:?}: must emit ZERO Cbz/Cbnz");
            assert_eq!(bcond_count, 1, "cond={cond:?}: must emit BCond fused with TstImm");

            let (emitted, _clobbers, _stats) =
                emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                    .expect("misel must emit TstImm+BCond cleanly");
            assert!(!emitted.code.is_empty());

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                    .expect("W^X exec mapping must succeed");
                // SAFETY: Valid AAPCS64 function `extern "C" fn(u64, u64, u64) -> u64`.
                let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
                for &(xv, av, bval) in &[
                    (0u64, 100u64, 23u64),
                    (0xff00_0000_0000_0000, 100, 23),
                    (0x0100_0000_0000_0000, 0x1234, 0x5678),
                    (0x0080_0000_0000_0000, 0x1234, 0x5678),
                    (1, 77, 88),
                    (u64::MAX, 0xdead, 0xbeef),
                    (0xfe00_0000_0000_0001, 19, 41),
                ] {
                    let got = f(xv, av, bval);
                    let want = eval_u64(func.view(), &[xv, av, bval], alloc);
                    assert_eq!(
                        got, want,
                        "TstImm+BCond (cond={cond:?}) diverged on ({xv:#x}, {av:#x}, {bval:#x})"
                    );
                }
            }
        }
    }

    #[test]
    fn misel_call_indirect_executes_on_apple_silicon() {
        let alloc = test_alloc();
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
        let callee_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let cb = b.param(Ty::PTR);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let c = b.param(Ty::I64);
        // Permute arguments (passing `a` in `X2` while keeping `a` live across `Blr`!).
        let call_res = b.call_indirect(Flags::NONE, cb, callee_sig, &[bv, c, a]);
        let out = b.binary(Op::Add, Flags::NONE, call_res, a);
        b.ret(&[out]);

        let func = b.finish_checked().expect("call_indirect LIR builds");
        let verified = verify(func.view()).expect("call_indirect LIR verifies");
        let (emitted, _clobbers, _stats) =
            emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                .expect("misel must emit Op::CallIndirect cleanly");
        assert!(!emitted.code.is_empty());

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            extern "C" fn host_mix(x: u64, y: u64, z: u64) -> u64 {
                x.wrapping_mul(3)
                    .wrapping_add(y ^ 0x55aa)
                    .wrapping_add(z.rotate_left(7))
            }
            extern "C" fn host_sub_xor(x: u64, y: u64, z: u64) -> u64 {
                x.wrapping_sub(y) ^ z.wrapping_mul(17)
            }

            type HostCb = extern "C" fn(u64, u64, u64) -> u64;
            let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                .expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 function taking `(HostCb, u64, u64, u64) -> u64`.
            let f: extern "C" fn(HostCb, u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
            for &(av, bval, cv) in &[
                (0u64, 0u64, 0u64),
                (7, 13, 29),
                (0x1234_5678, 0x9abc_def0, 0xdead_beef),
                (u64::MAX, 1, 42),
            ] {
                assert_eq!(
                    f(host_mix, av, bval, cv),
                    host_mix(bval, cv, av).wrapping_add(av),
                    "call_indirect(host_mix) diverged on ({av:#x}, {bval:#x}, {cv:#x})"
                );
                assert_eq!(
                    f(host_sub_xor, av, bval, cv),
                    host_sub_xor(bval, cv, av).wrapping_add(av),
                    "call_indirect(host_sub_xor) diverged on ({av:#x}, {bval:#x}, {cv:#x})"
                );
            }
        }
    }

    #[test]
    fn misel_multi_return_4_gprs_compiles_and_matches_eval() {
        let alloc = test_alloc();

        // 1. Callee returning 4 `Ty::I64` values in `X0..X3`: `(a + b, a - b, a ^ b, a * b)`.
        let mut cb = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let ret4 = [
            Param::of(Ty::I64),
            Param::of(Ty::I64),
            Param::of(Ty::I64),
            Param::of(Ty::I64),
        ];
        let callee_sig = cb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &ret4,
        );
        cb.set_signature(callee_sig);
        let c_entry = cb.create_block(BlockKind::Normal);
        cb.begin(c_entry);
        let ca = cb.param(Ty::I64);
        let cbv = cb.param(Ty::I64);
        let r0 = cb.binary(Op::Add, Flags::NONE, ca, cbv);
        let r1 = cb.binary(Op::Sub, Flags::NONE, ca, cbv);
        let r2 = cb.binary(Op::Xor, Flags::NONE, ca, cbv);
        let r3 = cb.binary(Op::Mul, Flags::NONE, ca, cbv);
        cb.ret(&[r0, r1, r2, r3]);

        let callee_func = cb.finish_checked().expect("4-return callee LIR builds");
        let callee_verified = verify(callee_func.view()).expect("4-return callee LIR verifies");
        let (callee_emitted, _callee_clobbers, _callee_stats) =
            emit_mfunc_ssa_a64(callee_verified, &NoNames, default_opts(), alloc)
                .expect("misel must emit 4-GPR-return callee cleanly");
        assert!(!callee_emitted.code.is_empty());

        // 2. Caller invoking the 4-return callee and capturing all 4 results via `Op::Project`.
        struct MultiRetNames;
        impl Names for MultiRetNames {
            fn name(&self, of: Outside) -> Option<Named<'_>> {
                match of {
                    Outside::Func(0) => Some(Named::Symbol(Symbol::of_name("multi_ret_4_callee"))),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let mut caller_b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let caller_sig = caller_b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        let call_sig = caller_b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &ret4,
        );
        caller_b.set_signature(caller_sig);
        let callee_ref = caller_b.reference(RefKind::Func, 0);
        let entry = caller_b.create_block(BlockKind::Normal);
        caller_b.begin(entry);
        let a = caller_b.param(Ty::I64);
        let bv = caller_b.param(Ty::I64);
        let tuple = caller_b.call(Flags::NONE, callee_ref, call_sig, &[a, bv]);
        let p0 = caller_b.project(tuple, 0);
        let p1 = caller_b.project(tuple, 1);
        let p2 = caller_b.project(tuple, 2);
        let p3 = caller_b.project(tuple, 3);
        let k3 = caller_b.iconst(Ty::I64, 3);
        let k5 = caller_b.iconst(Ty::I64, 5);
        let k7 = caller_b.iconst(Ty::I64, 7);
        let t1 = caller_b.binary(Op::Mul, Flags::NONE, p1, k3);
        let t2 = caller_b.binary(Op::Mul, Flags::NONE, p2, k5);
        let t3 = caller_b.binary(Op::Mul, Flags::NONE, p3, k7);
        let s0 = caller_b.binary(Op::Add, Flags::NONE, p0, t1);
        let s1 = caller_b.binary(Op::Add, Flags::NONE, t2, t3);
        let out = caller_b.binary(Op::Add, Flags::NONE, s0, s1);
        caller_b.ret(&[out]);

        let caller_func = caller_b.finish_checked().expect("4-return caller LIR builds");
        let caller_verified = verify(caller_func.view()).expect("4-return caller LIR verifies");
        let (caller_emitted, _caller_clobbers, _caller_stats) =
            emit_mfunc_ssa_a64(caller_verified, &MultiRetNames, default_opts(), alloc)
                .expect("misel must emit 4-GPR-return caller+Project cleanly");
        assert_eq!(caller_emitted.relocs.len(), 1);
        assert_eq!(caller_emitted.relocs[0].kind, RelocKind::Branch26);

        // 3. Link caller + 4-return callee and verify against LIR evaluator on Apple Silicon!
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let callee_off = (caller_emitted.code.len() + 15) & !15;
            let total_len = callee_off + callee_emitted.code.len();
            let mut combined: Vec<u8, Alloc<'static>> = Vec::with_capacity_in(total_len, alloc);
            for &byte in caller_emitted.code.iter() {
                combined.push(byte);
            }
            while combined.len() < callee_off {
                combined.push(0);
            }
            for &byte in callee_emitted.code.iter() {
                combined.push(byte);
            }

            let reloc_at = usize::try_from(caller_emitted.relocs[0].at).expect("reloc offset fits");
            let delta_bytes = callee_off.checked_sub(reloc_at).expect("callee follows caller");
            let delta_words = i32::try_from(delta_bytes / 4).expect("Branch26 delta fits i32");
            let bl_word = crate::enc::bl(delta_words).to_le_bytes();
            combined[reloc_at..reloc_at + 4].copy_from_slice(&bl_word);

            let exec = blaze_inproc::exec::Executable::new(&combined, 0)
                .expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 caller invoking linked 4-GPR-return callee.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            for &(av, bval) in &[
                (0u64, 0u64),
                (17, 5),
                (5, 17),
                (0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321),
                (u64::MAX, 3),
            ] {
                let mut eval_args: Vec<Val, Alloc<'static>> = Vec::with_capacity_in(2, alloc);
                eval_args.push(Val::Int(u128::from(av)));
                eval_args.push(Val::Int(u128::from(bval)));
                let Outcome::Returned(rets) =
                    eval::run_in(callee_func.view(), &eval_args, 200_000, alloc)
                        .expect("4-return callee eval must succeed")
                else {
                    panic!("expected Outcome::Returned from 4-return callee");
                };
                assert_eq!(rets.len(), 4);
                let mut vals = [0u64; 4];
                for (k, rv) in rets.iter().enumerate() {
                    let Val::Int(n) = *rv else {
                        panic!("expected Val::Int return at {k}");
                    };
                    let b8 = n.to_le_bytes();
                    vals[k] = u64::from_le_bytes([b8[0], b8[1], b8[2], b8[3], b8[4], b8[5], b8[6], b8[7]]);
                }
                let want = vals[0]
                    .wrapping_add(vals[1].wrapping_mul(3))
                    .wrapping_add(vals[2].wrapping_mul(5))
                    .wrapping_add(vals[3].wrapping_mul(7));
                let got = f(av, bval);
                assert_eq!(got, want, "4-GPR multi-return diverged on ({av:#x}, {bval:#x})");
            }
        }
    }

    #[test]
    fn misel_invoke_cleanup_resume_emits_valid_unwind_table() {
        let alloc = test_alloc();
        struct UnwindNames;
        impl Names for UnwindNames {
            fn name(&self, of: Outside) -> Option<Named<'_>> {
                match of {
                    Outside::Func(0) => Some(Named::Symbol(Symbol::of_name("may_unwind_callee"))),
                    Outside::Func(1) => Some(Named::Symbol(Symbol::of_name("drop_cleanup_callee"))),
                    Outside::Func(2) => Some(Named::Symbol(Symbol::of_name("normal_post_callee"))),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let fn_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        let unary_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let drop_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
        b.set_signature(fn_sig);

        let may_unwind_ref = b.reference(RefKind::Func, 0);
        let drop_ref = b.reference(RefKind::Func, 1);
        let post_ref = b.reference(RefKind::Func, 2);

        let entry = b.create_block(BlockKind::Normal);
        let normal = b.create_block(BlockKind::Normal);
        let cleanup = b.create_block(BlockKind::Cleanup);

        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let live_val = b.binary(Op::Add, Flags::NONE, x, y);
        let inv_res = b.invoke(Flags::NONE, may_unwind_ref, unary_sig, &[x], normal, cleanup);

        b.begin(normal);
        let post_res = b.call(Flags::NONE, post_ref, unary_sig, &[inv_res]);
        let out = b.binary(Op::Add, Flags::NONE, post_res, live_val);
        b.ret(&[out]);

        b.begin(cleanup);
        let exc = b.param(Ty::PTR);
        let _ = b.call(Flags::NONE, drop_ref, drop_sig, &[live_val]);
        b.resume(exc);

        let func = b.finish_checked().expect("invoke+cleanup+resume LIR builds");
        let verified = verify(func.view()).expect("invoke+cleanup+resume LIR verifies");

        // Callee-side `InternalFast` on a function with unwind landing blocks must be refused.
        let fast_opts = MIseOpts {
            internal_fast: true,
            ..default_opts()
        };
        assert!(
            matches!(
                emit_mfunc_ssa_a64(verified, &UnwindNames, fast_opts, alloc),
                Err(TierError::Refused { .. })
            ),
            "InternalFast callee with landing blocks must be refused"
        );

        let got_opts = MIseOpts {
            call_mode: CallMode::GotSlot,
            ..default_opts()
        };
        let (emitted, _clobbers, _stats) =
            emit_mfunc_ssa_a64(verified, &UnwindNames, got_opts, alloc)
                .expect("misel must emit invoke+cleanup+resume cleanly");

        assert!(!emitted.code.is_empty());
        assert_ne!(emitted.frame, 0, "unwinding function must emit canonical frame word");
        assert!(emitted.unwind_frame.is_some(), "unwinding function must populate unwind_frame");
        // 3 rows: `invoke` (`Cleanup`), `post_ref` (`Continue`), `_Unwind_Resume` (`Continue`);
        // `drop_ref` inside `BlockKind::Cleanup` intentionally gets NO unwind row.
        assert_eq!(emitted.unwind.len(), 3, "expected 3 unwind rows, got {:?}", emitted.unwind);
        assert_eq!(emitted.pads.len(), 3, "expected 3 pads matching unwind rows");

        let mut cleanup_rows = 0usize;
        let mut continue_rows = 0usize;
        let code_len_u32 = u32::try_from(emitted.code.len()).expect("code len fits u32");
        for (idx, &row) in emitted.unwind.iter().enumerate() {
            assert_eq!(row.len, 4, "each ARM64 call unwind row must cover 4 bytes");
            assert_eq!(emitted.pads[idx], row.pad(), "pads[{idx}] must equal unwind[{idx}].pad()");
            if idx > 0 {
                assert!(
                    emitted.unwind[idx - 1].start < row.start,
                    "unwind rows must be strictly sorted by start offset"
                );
            }
            match row.action {
                blaze_code::unwind::Action::Cleanup => {
                    cleanup_rows += 1;
                    assert!(row.landing > 0, "Cleanup row must have non-zero landing byte offset");
                    assert_eq!(row.landing % 4, 0, "landing offset must be 4-byte aligned");
                    assert!(row.landing < code_len_u32, "landing offset must lie inside emitted code");
                }
                blaze_code::unwind::Action::Continue => {
                    continue_rows += 1;
                    assert_eq!(row.landing, 0, "Continue row must have landing == 0");
                }
                other => panic!("unexpected unwind action {other:?}"),
            }
        }
        assert_eq!(cleanup_rows, 1, "expected exactly 1 Action::Cleanup row");
        assert_eq!(continue_rows, 2, "expected exactly 2 Action::Continue rows");

        blaze_code::unwind::of_rows(emitted.unwind_frame, &emitted.unwind, code_len_u32)
            .expect("blaze_code::unwind::of_rows must validate emitted unwind table");

        let (rec, _) = super::super::emitted_to_code_record(emitted, &[], alloc)
            .expect("unwind Emitted must convert cleanly to CodeRecord");
        let sealed = rec.seal().expect("CodeRecord with unwind pads must seal cleanly");
        assert_eq!(sealed.record().pads.len(), 3, "sealed CodeRecord must preserve all 3 unwind pads");
    }

    #[test]
    fn misel_v128_neon_alu_add_sub_mul_xor_executes_on_apple_silicon() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let v4i32 = Ty::vector(Scalar::I32, 4).expect("<4 x i32> valid");
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I32)],
            &[],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let src = b.param(Ty::PTR);
        let dst = b.param(Ty::PTR);
        let k = b.param(Ty::I32);
        let v_src = b.load(v4i32, Flags::NONE, src, 0, MemAttrs::aligned(2));
        let v_k = b.convert(Op::Splat, Flags::NONE, v4i32, k);
        let v_add = b.binary(Op::Add, Flags::NONE, v_src, v_k);
        let v_xor = b.binary(Op::Xor, Flags::NONE, v_add, v_k);
        let v_mul = b.binary(Op::Mul, Flags::NONE, v_xor, v_k);
        b.store(Flags::NONE, v_mul, dst, 0, MemAttrs::aligned(2));
        b.ret(&[]);

        let func = b.finish_checked().expect("finish");
        let verified = verify(func.view()).expect("verify");
        let (emitted, _clob, _stats) =
            emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
                .expect("misel must compile V4xI32 NEON ALU natively");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec =
                blaze_inproc::exec::Executable::new(&emitted.code, 0).expect("exec map");
            // SAFETY: Valid AAPCS64 leaf function matching `(ptr, ptr, u32) -> ()`.
            let f: extern "C" fn(*const u32, *mut u32, u32) = unsafe { exec.as_fn() };
            let inp = [1u32, 10, 100, 0xffff_fffe];
            let mut out = [0u32; 4];
            let kv = 7u32;
            f(inp.as_ptr(), out.as_mut_ptr(), kv);
            for i in 0..4 {
                let want = (inp[i].wrapping_add(kv) ^ kv).wrapping_mul(kv);
                assert_eq!(out[i], want, "lane {i} mismatch");
            }
        }
        let _ = emitted;
    }

    #[test]
    fn misel_byval_incoming_and_outgoing_copy_semantics_on_apple_silicon() {
        let alloc = test_alloc();
        let mut byval_param = Param::of(Ty::PTR);
        byval_param.kind = ParamKind::Byval;
        byval_param.facts = blaze_lir::builder::Facts {
            size: 40,
            align: 3,
            ..blaze_lir::builder::Facts::default()
        };

        // 1. Callee: sums 5 x u64 from `byval` ptr and clobbers `[ptr + 0]` to verify caller's
        //    private copy isolates the caller's original buffer.
        let mut cb = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let callee_sig = cb.sig(Conv::C, Variadic::No, &[byval_param], &[Param::of(Ty::I64)]);
        cb.set_signature(callee_sig);
        let ce = cb.create_block(BlockKind::Normal);
        cb.begin(ce);
        let bp = cb.param(Ty::PTR);
        let mut sum = cb.load(Ty::I64, Flags::NONE, bp, 0, MemAttrs::aligned(3));
        for off in [8i32, 16, 24, 32] {
            let v = cb.load(Ty::I64, Flags::NONE, bp, off, MemAttrs::aligned(3));
            sum = cb.binary(Op::Add, Flags::NONE, sum, v);
        }
        let poison = cb.iconst(Ty::I64, 0xdead_beef_cafe_babe);
        cb.store(Flags::NONE, poison, bp, 0, MemAttrs::aligned(3));
        cb.ret(&[sum]);
        let callee_fn = cb.finish_checked().expect("callee finish");
        let callee_ver = verify(callee_fn.view()).expect("callee verify");
        let (callee_emitted, _, _) =
            emit_mfunc_ssa_a64(callee_ver, &NoNames, default_opts(), alloc)
                .expect("misel must compile incoming byval callee");

        // 2. Caller: takes `(callee_fn_ptr, src_ptr)` and calls `callee_fn_ptr(byval(40) src_ptr)`
        //    via `Op::CallIndirect`, returning the callee's sum.
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let caller_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(caller_sig);
        let call_sig = b.sig(Conv::C, Variadic::No, &[byval_param], &[Param::of(Ty::I64)]);
        let e = b.create_block(BlockKind::Normal);
        b.begin(e);
        let fn_ptr = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let res = b.call_indirect(Flags::NONE, fn_ptr, call_sig, &[src]);
        b.ret(&[res]);
        let caller_fn = b.finish_checked().expect("caller finish");
        let caller_ver = verify(caller_fn.view()).expect("caller verify");
        let (caller_emitted, _, _) =
            emit_mfunc_ssa_a64(caller_ver, &NoNames, default_opts(), alloc)
                .expect("misel must compile outgoing byval caller");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let callee_exec =
                blaze_inproc::exec::Executable::new(&callee_emitted.code, 0).expect("callee map");
            let caller_exec =
                blaze_inproc::exec::Executable::new(&caller_emitted.code, 0).expect("caller map");
            // SAFETY: Valid AAPCS64 functions matching our LIR signatures.
            let callee_f: extern "C" fn(*mut u64) -> u64 = unsafe { callee_exec.as_fn() };
            let caller: extern "C" fn(extern "C" fn(*mut u64) -> u64, *const u64) -> u64 =
                unsafe { caller_exec.as_fn() };
            let buf = [10u64, 20, 30, 40, 50];
            let got = caller(callee_f, buf.as_ptr());
            assert_eq!(got, 150, "byval sum mismatch");
            assert_eq!(buf[0], 10, "byval copy must isolate caller's original buffer");
        }
        let _ = (callee_emitted, caller_emitted);
    }

    #[test]
    fn misel_i128_arith_cmp_shift_and_store_executes_on_apple_silicon() {
        let alloc = test_alloc();
        // Function `(a_ptr: ptr, b_ptr: ptr, out_ptr: ptr, shift: i32) -> i64`:
        // Loads 128-bit `a` and `b`, computes `sum = a + b`, `diff = a - b`, `prod = a * b`,
        // `shl_v = a << shift`, `shr_v = a >>_u shift`, `ashr_v = a >>_s shift`,
        // writes `(sum, diff, prod, shl_v, shr_v, ashr_v)` to `out_ptr`, and returns a bitmask
        // of all 10 `Icmp(a, b)` results (`Eq, Ne, Ult, Ule, Ugt, Uge, Slt, Sle, Sgt, Sge`).
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I32)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let a_ptr = b.param(Ty::PTR);
        let b_ptr = b.param(Ty::PTR);
        let out_ptr = b.param(Ty::PTR);
        let sh = b.param(Ty::I32);
        let a = b.load(Ty::I128, Flags::NONE, a_ptr, 0, MemAttrs::aligned(3));
        let bv = b.load(Ty::I128, Flags::NONE, b_ptr, 0, MemAttrs::aligned(3));
        // Exercise small `Op::Iconst(Ty::I128)` (`-1i128` + `1u128` == 0) too!
        let neg1 = b.iconst(Ty::I128, u128::MAX);
        let pos1 = b.iconst(Ty::I128, 1);
        let zero128 = b.binary(Op::Add, Flags::NONE, neg1, pos1);
        let a = b.binary(Op::Add, Flags::NONE, a, zero128);
        let sh128 = b.convert(Op::Zext, Flags::NONE, Ty::I128, sh);
        let sum = b.binary(Op::Add, Flags::NONE, a, bv);
        let diff = b.binary(Op::Sub, Flags::NONE, a, bv);
        let prod = b.binary(Op::Mul, Flags::NONE, a, bv);
        let shl_v = b.binary(Op::Shl, Flags::NONE, a, sh128);
        let shr_v = b.binary(Op::Lshr, Flags::NONE, a, sh128);
        let ashr_v = b.binary(Op::Ashr, Flags::NONE, a, sh128);
        b.store(Flags::NONE, sum, out_ptr, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, diff, out_ptr, 16, MemAttrs::aligned(3));
        b.store(Flags::NONE, prod, out_ptr, 32, MemAttrs::aligned(3));
        b.store(Flags::NONE, shl_v, out_ptr, 48, MemAttrs::aligned(3));
        b.store(Flags::NONE, shr_v, out_ptr, 64, MemAttrs::aligned(3));
        b.store(Flags::NONE, ashr_v, out_ptr, 80, MemAttrs::aligned(3));

        let conds = [
            IntCond::Eq,
            IntCond::Ne,
            IntCond::Ult,
            IntCond::Ule,
            IntCond::Ugt,
            IntCond::Uge,
            IntCond::Slt,
            IntCond::Sle,
            IntCond::Sgt,
            IntCond::Sge,
        ];
        let mut mask = b.iconst(Ty::I64, 0);
        for (bit, &cond) in conds.iter().enumerate() {
            let cmp = b.icmp(cond, a, bv);
            let ext = b.convert(Op::Zext, Flags::NONE, Ty::I64, cmp);
            let k = b.iconst(Ty::I64, u128::try_from(bit).unwrap_or(0));
            let shifted = b.binary(Op::Shl, Flags::NONE, ext, k);
            mask = b.binary(Op::Or, Flags::NONE, mask, shifted);
        }
        b.ret(&[mask]);

        let func = b.finish_checked().expect("i128 LIR finish");
        let verified = verify(func.view()).expect("i128 LIR verify");
        let (emitted, _, _) = emit_mfunc_ssa_a64(verified, &NoNames, default_opts(), alloc)
            .expect("misel must compile Ty::I128 arithmetic/cmp/shift/store natively");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0).expect("exec map");
            // SAFETY: Valid AAPCS64 leaf function matching `(ptr, ptr, ptr, u32) -> u64`.
            let f: extern "C" fn(*const u128, *const u128, *mut u128, u32) -> u64 =
                unsafe { exec.as_fn() };
            let cases: &[(u128, u128, u32)] = &[
                (0, 0, 0),
                (1, 0, 1),
                (0, 1, 63),
                (u128::MAX, 1, 64),
                (i128::MIN as u128, i128::MAX as u128, 65),
                (0x1234_5678_9abc_def0_fedc_ba98_7654_3210, 0x0000_0001_ffff_ffff_0000_0000_ffff_ffff, 17),
                (0x8000_0000_0000_0000_0000_0000_0000_0005, 0x8000_0000_0000_0000_0000_0000_0000_0005, 127),
            ];
            for &(av, bv, shv) in cases {
                let mut out = [0u128; 6];
                let got_mask = f(&av, &bv, out.as_mut_ptr(), shv);
                assert_eq!(out[0], av.wrapping_add(bv), "add mismatch for ({av:#x}, {bv:#x})");
                assert_eq!(out[1], av.wrapping_sub(bv), "sub mismatch for ({av:#x}, {bv:#x})");
                assert_eq!(out[2], av.wrapping_mul(bv), "mul mismatch for ({av:#x}, {bv:#x})");
                assert_eq!(out[3], av.wrapping_shl(shv), "shl mismatch for ({av:#x}, {shv})");
                assert_eq!(out[4], av.wrapping_shr(shv), "lshr mismatch for ({av:#x}, {shv})");
                assert_eq!(
                    out[5],
                    (av as i128).wrapping_shr(shv) as u128,
                    "ashr mismatch for ({av:#x}, {shv})"
                );
                let sa = av as i128;
                let sb = bv as i128;
                let want_mask = u64::from(av == bv)
                    | (u64::from(av != bv) << 1)
                    | (u64::from(av < bv) << 2)
                    | (u64::from(av <= bv) << 3)
                    | (u64::from(av > bv) << 4)
                    | (u64::from(av >= bv) << 5)
                    | (u64::from(sa < sb) << 6)
                    | (u64::from(sa <= sb) << 7)
                    | (u64::from(sa > sb) << 8)
                    | (u64::from(sa >= sb) << 9);
                assert_eq!(got_mask, want_mask, "icmp mask mismatch for ({av:#x}, {bv:#x})");
            }
        }
        let _ = emitted;
    }
}

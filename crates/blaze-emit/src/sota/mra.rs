#![cfg_attr(not(test), forbid(unsafe_code))]
//! Linear-Time SSA Register Allocator (`mra`) over Typed AArch64 Machine IR (`MFunc`).
//!
//! Transforms a verified SSA [`MFunc`] (`is_ssa == true`, virtual registers [`VReg`] and
//! [`MOp::ParallelCopy`]) into a verified post-RA [`MFunc`] (`is_ssa == false`, 100% physical
//! [`PReg`]s, zero [`VReg`]s, zero [`MOp::ParallelCopy`]s, and exact [`CallClobbers`] summary):
//!
//! 1. **Platform-Safe Physical Register Pools**:
//!    - GPR scratch: `X9..=X15, X8, X0..=X7` (or caller-supplied `opts.scratch_gpr_order`).
//!      `X16`/`X17` stay permanently reserved for reload/spill scratch and cycle breaking;
//!      `X18` is permanently excluded for macOS/cross-OS portability.
//!    - GPR callee-saved: `X19..=X28`, plus `X29` (`PReg::FP`) and `X30` (`PReg::LR`) ONLY when
//!      `opts.allow_x29_leaf && !has_calls`.
//!    - FP/NEON scratch: `V16..=V28, V0..=V7, V29` (`V30`/`V31` permanently reserved);
//!      FP/NEON callee-saved: `V8..=V15`.
//! 2. **Sparse Per-Global-`VReg` Backward BFS + Bounded `PReg` Liveness + Wimmer–Franz Multi-Range Intervals**:
//!    - Exact per-global-`VReg` backward CFG BFS (`O(n_blocks)` scratch, zero `O(n_blocks * n_vregs)`
//!      dense bitsets) paired with bounded `u64` physical-register dataflow (`<= 32` iterations),
//!      producing exact disjoint live ranges with lifetime holes so loop-carried `ParallelCopy`
//!      back-edges coalesce to zero instructions.
//! 3. **Rematerialization & Cold-Exit Live-Range Splitting**:
//!    - Single-def `MovImm`, `Adrp`, `AddImm { rn: SP, sets_flags: false }`, and `VecAlu::Zero`
//!      (or `MInstFlags::REMATERIALIZABLE`) rematerialize at uses with zero stack slots.
//!    - Under `opts.block_weights`, values defined outside hot blocks (`weight >= 8`) that cross
//!      hot blocks with zero hot uses and only cold-exit uses (`weight <= 1`) split before the
//!      hot loop (rematerialized or spilled once before the loop, reloaded only in the cold exit).
//! 4. **Linear-Time Counting-Sort Scan, Coalescing, IPRA Avoidance, & Loop-Weighted Eviction**:
//!    - Coalesces `Mov` and `ParallelCopy` edges weighted by block frequency; avoids per-call
//!      `CallClobbers`; evicts lowest loop-weighted / Belady farthest-end interval on overflow.
//! 5. **Proven Non-Recursive Cycle-Breaking `ParallelCopy` Lowering**:
//!    - Lowers simultaneous GPR, FP/NEON, spill-slot, and rematerialized copies in one bounded
//!      iterative pass, breaking every permutation cycle via `X16` / `V30`.

use core::fmt;

use blaze_own::{Alloc, Vec};
use blaze_tier::{CallClobbers, Reg, RegSet};

use super::mfunc::{
    AddrMode, FpAluOp, MBlock, MFunc, MInst, MInstFlags, MNote, MOp, MReg, MShift,
    MVerifyError, MemSize, NO_NOTE, NO_VFLAG, PReg, VReg, VRegClass, VecAluOp, mverify,
};

// -----------------------------------------------------------------------------
// Constants & Zero-`as` Numeric Helpers
// -----------------------------------------------------------------------------

/// Maximum fixed-point dataflow iterations (bounded progress, Rule 18).
const MAX_DATAFLOW_ITERS: usize = 32;

/// Maximum deduplicated coalescing neighbors retained per `VReg` (Rule 18 bounded O(1) lookup).
const MAX_COPY_EDGES_PER_VREG: usize = 16;

/// Maximum multi-hop fixed-hint propagation rounds along non-overlapping copy chains (Rule 18).
const MAX_HINT_PROP_ITERS: usize = 4;

/// Maximum active dynamic coalescing reservations tracked during linear scan (Rule 18).
const MAX_PENDING_RES: usize = 512;

/// Maximum upcoming fixed-`PReg` hint claimants scanned per register candidate (Rule 18).
const MAX_FIXED_WANT_SCAN: usize = 32;

/// Hot-block weight threshold for cold-exit live-range splitting.
const HOT_BLOCK_WEIGHT_MIN: u32 = 8;

/// Cold-exit weight ceiling for cold-exit live-range splitting.
const COLD_EXIT_WEIGHT_MAX: u32 = 1;

/// Unassigned register sentinel (`u8`).
const NO_REG: u8 = u8::MAX;

/// Unassigned vreg/node sentinel (`u32`).
const NONE_U32: u32 = u32::MAX;

/// Default caller-saved GPR allocation order (`X9..=X15, X8, X0..=X7`).
const DEFAULT_GPR_SCRATCH: [u8; 16] = [9, 10, 11, 12, 13, 14, 15, 8, 0, 1, 2, 3, 4, 5, 6, 7];

/// Callee-saved GPR pool (`X19..=X28`; `X29`/`X30` appended only when `allow_x29_leaf && !has_calls`).
const CALLEE_SAVED_GPRS: [u8; 10] = [19, 20, 21, 22, 23, 24, 25, 26, 27, 28];

/// Caller-saved FP/NEON allocation order (`V16..=V28, V0..=V7, V29`; `V30`/`V31` reserved).
const FP_SCRATCH_POOL: [u8; 22] = [
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 0, 1, 2, 3, 4, 5, 6, 7, 29,
];

/// Callee-saved FP/NEON pool (`V8..=V15`).
const CALLEE_SAVED_FPS: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];

/// Bitmask of callee-saved GPRs (`X19..=X29`: bits 19..=29).
const CALLEE_SAVED_GPR_MASK: u32 = 0x3ff8_0000;

/// Bitmask of callee-saved GPRs including `LR` (`X19..=X30`: bits 19..=30) for zero-call leaves.
const CALLEE_SAVED_GPR_LEAF_LR_MASK: u32 = 0x7ff8_0000;

/// Bitmask of callee-saved FP/NEON registers (`V8..=V15`: bits 8..=15).
const CALLEE_SAVED_FP_MASK: u32 = 0x0000_ff00;

/// Bitmask of AAPCS64 call argument / indirect-result physical regs (`X0..=X8`, `V0..=V7`).
const CALL_ARG_PHYS_MASK: u64 = 0x0000_01ff | (0x0000_00ffu64 << 32);

/// Bitmask of AAPCS64 return-value physical regs (`X0..=X7`, `V0..=V7`).
const RET_VAL_PHYS_MASK: u64 = 0x0000_00ff | (0x0000_00ffu64 << 32);

/// Maximum `SP`-relative byte offset addressable via 1-word `add imm12, lsl #12` + `ldr/str imm12`.
const MAX_SPILL_BYTE_OFF: u32 = 0x00ff_f000;

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
fn i32_of(v: u32) -> Option<i32> {
    i32::try_from(v).ok()
}

#[inline]
const fn align_up(val: u32, align: u32) -> Option<u32> {
    let mask = align - 1;
    match val.checked_add(mask) {
        Some(v) => Some(v & !mask),
        None => None,
    }
}

#[inline]
fn block_raw_weight(weights: &[u32], blk: MBlock) -> u32 {
    weights.get(to_idx(blk.0)).copied().unwrap_or(1)
}

#[inline]
fn block_eff_weight(weights: &[u32], blk: MBlock) -> u64 {
    let raw = weights.get(to_idx(blk.0)).copied().unwrap_or(1);
    u64::from(raw.max(1))
}

#[inline]
fn slice_at<T: Copy>(s: &[T], i: usize, def: T) -> T {
    s.get(i).copied().unwrap_or(def)
}

#[inline]
fn slice_set<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn subslice<T>(s: &[T], start: usize, end: usize) -> &[T] {
    s.get(start..end).unwrap_or(&[])
}

#[inline]
fn swap_remove_fast<T>(v: &mut Vec<T, Alloc<'_>>, idx: usize) {
    if let Some(last_val) = v.pop()
        && let Some(slot) = v.get_mut(idx)
    {
        *slot = last_val;
    }
}

/// Binary-searches a `orig_inst`-sorted call-clobber table in O(log C).
#[inline]
fn lookup_call_clobbers_sorted(table: &[(u32, CallClobbers)], orig_inst: u32) -> CallClobbers {
    match table.binary_search_by_key(&orig_inst, |&(idx, _)| idx) {
        Ok(pos) => table[pos].1,
        Err(_) => CallClobbers::C_ABI_ARM64,
    }
}

// -----------------------------------------------------------------------------
// Public Options, Output, Statistics, & Error Types (Rules 83, 85)
// -----------------------------------------------------------------------------

/// Options configuring [`allocate_mfunc`] (Rule 83).
#[derive(Clone, Copy, Debug)]
pub struct MRaOpts<'a> {
    /// Allow `PReg::FP` (`X29`) and `PReg::LR` (`X30`) as the 27th/28th allocatable GPRs
    /// (strictly for zero-call fixed-frame leaves).
    pub allow_x29_leaf: bool,
    /// Optional preferred GPR scratch allocation order (e.g. from `abi::plan_module_abi`).
    pub scratch_gpr_order: Option<[u8; 16]>,
    /// Optional per-block execution weight (from `layout::BlockFreq` / PGO) for loop-weighted
    /// spilling & cold-exit detection.
    pub block_weights: &'a [u32],
    /// Per-call-instruction clobber mask lookup (or empty slice => standard `CallClobbers::C_ABI_ARM64`).
    pub call_clobbers: &'a [(u32, CallClobbers)],
    /// Base byte offset from `SP` where spill slots begin (16-byte aligned).
    pub spill_base_off: u32,
    /// Fast single-pass linear-scan mode for [`super::EmitProfile::Fast`] cold compilation:
    /// skips cold-exit splitting, CSR copy-edge graph construction, multi-hop hint propagation,
    /// `fixed_want` lookahead, and `pending_res` reservations, using an `O(1)` bitmask scan over
    /// convex-hull intervals while preserving exact liveness, ABI, IPRA `CallClobbers`, and
    /// `ParallelCopy` lowering.
    pub fast_mode: bool,
}

impl Default for MRaOpts<'_> {
    fn default() -> Self {
        Self {
            allow_x29_leaf: false,
            scratch_gpr_order: None,
            block_weights: &[],
            call_clobbers: &[],
            spill_base_off: 0,
            fast_mode: false,
        }
    }
}

/// Telemetry emitted by [`allocate_mfunc`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MRaStats {
    pub vregs_allocated: u32,
    pub vregs_coalesced: u32,
    pub vregs_rematerialized: u32,
    pub vregs_cold_split: u32,
    pub spill_slots_used: u32,
    pub parallel_copies_lowered: u32,
}

/// Summary produced by [`allocate_mfunc`] for downstream frame lowering and IPRA.
#[derive(Clone, Copy, Debug)]
pub struct MRaOut {
    /// Exact set of physical GPRs and FP/NEON registers written by `func` + transitively by calls.
    pub clobbers: CallClobbers,
    /// Bitmask of callee-saved GPRs (`X19..=X28`, plus `X29`/`X30` if allocated in zero-call leaves) written by `func`.
    pub used_callee_saved_gprs: u32,
    /// Bitmask of callee-saved FP/NEON registers (`V8..=V15`) written by `func`.
    pub used_callee_saved_fps: u32,
    /// Total stack spill area size in bytes (16-byte aligned).
    pub spill_bytes: u32,
    /// Allocation telemetry counters.
    pub stats: MRaStats,
}

/// Typed register-allocation failure returned by [`allocate_mfunc`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MRaError {
    /// Input or rewritten `MFunc` failed structural verification.
    VerifyFailed(MVerifyError),
    /// Input `MFunc` is not in SSA form (`func.is_ssa == false`).
    NotInSsa,
    /// Function exceeds `u32` indexing limits.
    FunctionTooLarge,
    /// `MRaOpts::spill_base_off` is not 16-byte aligned or spill slot offsets exceed AArch64 range.
    InvalidSpillOffset { off: u32 },
    /// A `VReg` operand carries unsupported class `VRegClass::Nzcv`.
    UnsupportedVRegClass { vreg: VReg, class: VRegClass },
    /// A `ParallelCopy` contains duplicate destinations, mismatched banks, or illegal registers.
    InvalidParallelCopy { inst: MInst },
    /// Bounded dataflow analysis did not converge within [`MAX_DATAFLOW_ITERS`].
    LivenessDidNotConverge,
    /// Single instruction requires more spilled FP/NEON operands than reserved scratch registers.
    SpillScratchExhausted { inst: MInst },
    /// A virtual register is used in SSA form without any defining instruction or parallel copy.
    UndefinedVReg { vreg: VReg },
    /// A spill slot was read without any store writing to that slot anywhere in the function.
    UninitializedSpillSlot { off: u32 },
}

impl fmt::Display for MRaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

// -----------------------------------------------------------------------------
// Register Pools & Internal Data Structures
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RegBank {
    Gpr,
    Fp,
}

#[derive(Clone, Copy)]
struct RegPools {
    gpr_scratch: [u8; 16],
    gpr_callee: [u8; 12],
    gpr_callee_len: usize,
    fp_scratch: [u8; 22],
    fp_callee: [u8; 8],
}

impl RegPools {
    fn new(
        opts: MRaOpts<'_>,
        has_calls: bool,
        explicitly_uses_fp: bool,
        explicitly_uses_lr: bool,
    ) -> Self {
        let mut gpr_scratch = [0u8; 16];
        let mut len = 0usize;
        let mut seen: u32 = 0;

        if let Some(order) = opts.scratch_gpr_order {
            for &r in &order {
                if r <= 15 {
                    let bit = 1u32 << u32::from(r);
                    if (seen & bit) == 0 {
                        slice_set(&mut gpr_scratch, len, r);
                        len += 1;
                        seen |= bit;
                    }
                }
            }
        }
        for &r in &DEFAULT_GPR_SCRATCH {
            let bit = 1u32 << u32::from(r);
            if (seen & bit) == 0 && len < 16 {
                slice_set(&mut gpr_scratch, len, r);
                len += 1;
                seen |= bit;
            }
        }

        let mut gpr_callee = [0u8; 12];
        for (i, &r) in CALLEE_SAVED_GPRS.iter().enumerate() {
            slice_set(&mut gpr_callee, i, r);
        }
        let mut gpr_callee_len = CALLEE_SAVED_GPRS.len();
        if opts.allow_x29_leaf && !has_calls {
            if !explicitly_uses_fp {
                slice_set(&mut gpr_callee, gpr_callee_len, 29);
                gpr_callee_len += 1;
            }
            if !explicitly_uses_lr {
                slice_set(&mut gpr_callee, gpr_callee_len, 30);
                gpr_callee_len += 1;
            }
        }

        Self {
            gpr_scratch,
            gpr_callee,
            gpr_callee_len,
            fp_scratch: FP_SCRATCH_POOL,
            fp_callee: CALLEE_SAVED_FPS,
        }
    }

    #[inline]
    fn is_allocatable(&self, bank: RegBank, hw: u8) -> bool {
        match bank {
            RegBank::Gpr => {
                if hw <= 15 {
                    return true;
                }
                for &r in subslice(&self.gpr_callee, 0, self.gpr_callee_len) {
                    if r == hw {
                        return true;
                    }
                }
                false
            }
            RegBank::Fp => hw <= 29,
        }
    }

    #[inline]
    fn scratch_slice(&self, bank: RegBank) -> &[u8] {
        match bank {
            RegBank::Gpr => &self.gpr_scratch,
            RegBank::Fp => &self.fp_scratch,
        }
    }

    #[inline]
    fn callee_slice(&self, bank: RegBank) -> &[u8] {
        match bank {
            RegBank::Gpr => subslice(&self.gpr_callee, 0, self.gpr_callee_len),
            RegBank::Fp => &self.fp_callee,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AllocLoc {
    Unassigned,
    Reg(PReg),
    Spill { off: u32, size: MemSize },
    Remat { def_inst: u32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CopyLoc {
    Reg(PReg),
    Spill { off: u32, size: MemSize },
    Remat { def_inst: u32, is_fp: bool },
}

impl CopyLoc {
    #[inline]
    const fn is_fp_bank(self) -> bool {
        match self {
            Self::Reg(PReg::V(_)) => true,
            Self::Reg(PReg::Gpr(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv) => false,
            Self::Spill { size, .. } => size.is_fp_or_vec(),
            Self::Remat { is_fp, .. } => is_fp,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct RangeNode {
    start: u32,
    end: u32,
    next: u32,
}

#[derive(Clone, Copy)]
struct VRegMeta {
    bank: RegBank,
    class: VRegClass,
    def_count: u32,
    use_count: u32,
    def_inst: u32,
    def_lin: u32,
    def_block: u32,
    weight: u64,
    live_len: u64,
    used_in_hot: bool,
    def_in_hot: bool,
    used_in_non_cold: bool,
    is_remat: bool,
    is_cold_split: bool,
    first_start: u32,
    last_end: u32,
    range_start_idx: u32,
    range_len: u32,
    forbidden_phys: RegSet,
    fixed_hint_reg: u8,
    fixed_hint_weight: u64,
    fixed_hint2_reg: u8,
    fixed_hint2_weight: u64,
}

impl VRegMeta {
    fn new(class: VRegClass) -> Self {
        let bank = if class.is_fp_or_vec() { RegBank::Fp } else { RegBank::Gpr };
        Self {
            bank,
            class,
            def_count: 0,
            use_count: 0,
            def_inst: NONE_U32,
            def_lin: NONE_U32,
            def_block: NONE_U32,
            weight: 0,
            live_len: 1,
            used_in_hot: false,
            def_in_hot: false,
            used_in_non_cold: false,
            is_remat: false,
            is_cold_split: false,
            first_start: NONE_U32,
            last_end: 0,
            range_start_idx: 0,
            range_len: 0,
            forbidden_phys: RegSet::EMPTY,
            fixed_hint_reg: NO_REG,
            fixed_hint_weight: 0,
            fixed_hint2_reg: NO_REG,
            fixed_hint2_weight: 0,
        }
    }

    /// Accumulates a direct physical-register hint `(hw, w)` into the top-2 fixed hint slots.
    #[inline]
    fn add_fixed_hint(&mut self, hw: u8, w: u64) {
        if hw == NO_REG || w == 0 {
            return;
        }
        if self.fixed_hint_reg == hw {
            self.fixed_hint_weight = self.fixed_hint_weight.saturating_add(w);
            return;
        }
        if self.fixed_hint2_reg == hw {
            self.fixed_hint2_weight = self.fixed_hint2_weight.saturating_add(w);
            if self.fixed_hint2_weight > self.fixed_hint_weight
                || (self.fixed_hint2_weight == self.fixed_hint_weight
                    && self.fixed_hint2_reg < self.fixed_hint_reg)
            {
                core::mem::swap(&mut self.fixed_hint_reg, &mut self.fixed_hint2_reg);
                core::mem::swap(&mut self.fixed_hint_weight, &mut self.fixed_hint2_weight);
            }
            return;
        }
        if w > self.fixed_hint_weight || (w == self.fixed_hint_weight && hw < self.fixed_hint_reg) {
            self.fixed_hint2_reg = self.fixed_hint_reg;
            self.fixed_hint2_weight = self.fixed_hint_weight;
            self.fixed_hint_reg = hw;
            self.fixed_hint_weight = w;
        } else if w > self.fixed_hint2_weight
            || (w == self.fixed_hint2_weight && hw < self.fixed_hint2_reg)
        {
            self.fixed_hint2_reg = hw;
            self.fixed_hint2_weight = w;
        }
    }

    /// Idempotently propagates an indirect fixed-register hint along a copy edge (monotonic max,
    /// never inflating weights across repeated propagation iterations).
    #[inline]
    fn propagate_fixed_hint(&mut self, hw: u8, w: u64) -> bool {
        if hw == NO_REG || w == 0 {
            return false;
        }
        if self.fixed_hint_reg == hw {
            if w > self.fixed_hint_weight {
                self.fixed_hint_weight = w;
                return true;
            }
            return false;
        }
        if self.fixed_hint2_reg == hw {
            if w > self.fixed_hint2_weight {
                self.fixed_hint2_weight = w;
                if self.fixed_hint2_weight > self.fixed_hint_weight
                    || (self.fixed_hint2_weight == self.fixed_hint_weight
                        && self.fixed_hint2_reg < self.fixed_hint_reg)
                {
                    core::mem::swap(&mut self.fixed_hint_reg, &mut self.fixed_hint2_reg);
                    core::mem::swap(&mut self.fixed_hint_weight, &mut self.fixed_hint2_weight);
                }
                return true;
            }
            return false;
        }
        if w > self.fixed_hint_weight || (w == self.fixed_hint_weight && hw < self.fixed_hint_reg) {
            self.fixed_hint2_reg = self.fixed_hint_reg;
            self.fixed_hint2_weight = self.fixed_hint_weight;
            self.fixed_hint_reg = hw;
            self.fixed_hint_weight = w;
            return true;
        }
        if w > self.fixed_hint2_weight
            || (w == self.fixed_hint2_weight && hw < self.fixed_hint2_reg)
        {
            self.fixed_hint2_reg = hw;
            self.fixed_hint2_weight = w;
            return true;
        }
        false
    }

    /// Length-normalized loop-weighted spill density with sublinear length damping:
    /// `weight * 1024 / ((live_len / 8).min(48) + 16)`.
    ///
    /// Caps the length penalty at 4x (`64 / 16`) so long-lived loop-carried counters and pointers
    /// spanning hundreds of instructions keep registers over low-use values while still preferring
    /// short high-density inner-loop temporaries.
    #[inline]
    fn spill_density(self) -> u64 {
        let denom = (self.live_len / 8).min(48).saturating_add(16);
        let numer = self.weight.saturating_mul(1024);
        (numer / denom).max(1)
    }

    #[inline]
    fn spill_mem_size(self) -> MemSize {
        match self.class {
            VRegClass::Gpr32 | VRegClass::Gpr64 | VRegClass::Nzcv => MemSize::X64,
            VRegClass::Fp32 => MemSize::S32,
            VRegClass::Fp64 => MemSize::D64,
            VRegClass::Vec128 => MemSize::Q128,
        }
    }
}

#[derive(Clone, Copy)]
struct PendingRes {
    vreg: u32,
    from_vreg: u32,
    hw: u8,
    bank: RegBank,
    weight: u64,
}

#[derive(Clone, Copy)]
struct CallSite {
    def_pt: u32,
    clobbers: CallClobbers,
}

#[derive(Clone, Copy)]
struct EvictScore {
    non_remat_count: u32,
    hot_count: u32,
    eff_weight: u64,
    max_end: u32,
}

impl EvictScore {
    /// Returns `true` iff `self` is strictly cheaper to evict than `other`.
    #[inline]
    fn is_cheaper_than(self, other: Self) -> bool {
        // 1. Always rematerialize pure-remat registers (`non_remat_count == 0`) before spilling
        //    ANY non-rematerializable interval to the stack.
        let self_spills = self.non_remat_count > 0;
        let other_spills = other.non_remat_count > 0;
        if self_spills != other_spills {
            return !self_spills;
        }
        // 2. Always evict intervals with ZERO hot-block activity (`hot_count == 0`, or fewer hot
        //    overlapping intervals) before evicting ANY hot-loop value.
        if self.hot_count != other.hot_count {
            return self.hot_count < other.hot_count;
        }
        // 3. Within the same tier, evict lowest length-normalized loop-weighted density first.
        if self.eff_weight != other.eff_weight {
            return self.eff_weight < other.eff_weight;
        }
        // 4. Fewer distinct spilled intervals next.
        if self.non_remat_count != other.non_remat_count {
            return self.non_remat_count < other.non_remat_count;
        }
        // 5. Belady tie-breaker: evict the interval whose last use is farthest away (larger `max_end`).
        self.max_end > other.max_end
    }
}

/// Inserts or upgrades a dynamic coalescing reservation in `pending_res` (bounded by
/// [`MAX_PENDING_RES`]), deduplicating `(vreg, hw, bank)` and evicting the lowest-weight entry
/// when full so low-weight prologue copies never starve hot-loop `ParallelCopy` edges.
fn insert_pending_res(pending_res: &mut Vec<PendingRes, Alloc<'_>>, entry: PendingRes) {
    for existing in pending_res.iter_mut() {
        if existing.vreg == entry.vreg && existing.hw == entry.hw && existing.bank == entry.bank {
            if entry.weight > existing.weight {
                existing.weight = entry.weight;
                existing.from_vreg = entry.from_vreg;
            }
            return;
        }
    }
    if pending_res.len() < MAX_PENDING_RES {
        pending_res.push(entry);
        return;
    }
    let mut min_idx: Option<usize> = None;
    let mut min_w = entry.weight;
    for (i, existing) in pending_res.iter().enumerate() {
        if existing.weight < min_w {
            min_w = existing.weight;
            min_idx = Some(i);
        }
    }
    if let Some(i) = min_idx {
        slice_set(pending_res, i, entry);
    }
}

// -----------------------------------------------------------------------------
// Exhaustive `MOp` Operand Introspection & Rewriting
// -----------------------------------------------------------------------------

/// Bit index in a 64-bit `RegSet` mask for an allocatable/trackable physical register.
#[inline]
const fn preg_bit(preg: PReg) -> Option<u8> {
    match preg {
        PReg::Gpr(r) if r <= 30 => Some(r),
        PReg::V(v) if v <= 31 => Some(32 + v),
        PReg::Gpr(_) | PReg::V(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv => None,
    }
}

#[inline]
const fn preg_mask(preg: PReg) -> u64 {
    match preg_bit(preg) {
        Some(b) => 1u64 << b,
        None => 0,
    }
}

#[inline]
const fn is_call_op(op: &MOp, flags: MInstFlags) -> bool {
    matches!(*op, MOp::Bl | MOp::Blr { .. }) || flags.contains(MInstFlags::CALL)
}

#[inline]
const fn is_vec_rmw(op: VecAluOp) -> bool {
    matches!(
        op,
        VecAluOp::Bsl
            | VecAluOp::Bit
            | VecAluOp::Bif
            | VecAluOp::Mla
            | VecAluOp::Mls
            | VecAluOp::Fmla
            | VecAluOp::Fmls
    )
}

#[inline]
const fn is_sp_write(op: &MOp) -> bool {
    match *op {
        MOp::MovSp { dst, .. }
        | MOp::AddImm { dst, .. }
        | MOp::SubImm { dst, .. }
        | MOp::AddExt { dst, .. }
        | MOp::SubExt { dst, .. }
        | MOp::LogicalImm { dst, .. }
        | MOp::AddLo12 { dst, .. }
        | MOp::Ldr { addr: AddrMode::PostIdx { base: dst, .. } | AddrMode::PreIdx { base: dst, .. }, .. }
        | MOp::Str { addr: AddrMode::PostIdx { base: dst, .. } | AddrMode::PreIdx { base: dst, .. }, .. } => {
            matches!(dst, MReg::P(PReg::Sp))
        }
        MOp::Ldp { base, post_idx, pre_idx, .. } | MOp::Stp { base, post_idx, pre_idx, .. } => {
            (post_idx || pre_idx) && matches!(base, MReg::P(PReg::Sp))
        }
        _ => false,
    }
}

/// Visits every register operand READ by `op` (exhaustive over all `MOp` variants).
fn visit_op_uses(op: &MOp, copy_pairs: &[(MReg, MReg)], mut f: impl FnMut(MReg)) {
    match *op {
        MOp::Mov { src: a, .. }
        | MOp::MovSp { src: a, .. }
        | MOp::AddImm { rn: a, .. }
        | MOp::SubImm { rn: a, .. }
        | MOp::LogicalImm { rn: a, .. }
        | MOp::Mvn { rm: a, .. }
        | MOp::Ubfm { rn: a, .. }
        | MOp::Sbfm { rn: a, .. }
        | MOp::Rbit { rn: a, .. }
        | MOp::Clz { rn: a, .. }
        | MOp::Rev { rn: a, .. }
        | MOp::CmpImm { rn: a, .. }
        | MOp::TstImm { rn: a, .. }
        | MOp::CcmpImm { rn: a, .. }
        | MOp::AddLo12 { rn: a, .. }
        | MOp::Blr { target: a }
        | MOp::Br { target: a }
        | MOp::Cbz { rt: a, .. }
        | MOp::Cbnz { rt: a, .. }
        | MOp::Tbz { rt: a, .. }
        | MOp::Tbnz { rt: a, .. }
        | MOp::Prfm { base: a, .. } => f(a),
        MOp::Bfxil { dst, rn, .. } => {
            f(dst);
            f(rn);
        }
        MOp::AddReg { rn: a, rm: b, .. }
        | MOp::SubReg { rn: a, rm: b, .. }
        | MOp::AddExt { rn: a, rm: b, .. }
        | MOp::SubExt { rn: a, rm: b, .. }
        | MOp::LogicalReg { rn: a, rm: b, .. }
        | MOp::ShiftVar { rn: a, rm: b, .. }
        | MOp::Extr { rn: a, rm: b, .. }
        | MOp::Mul { rn: a, rm: b, .. }
        | MOp::Umulh { rn: a, rm: b, .. }
        | MOp::Smulh { rn: a, rm: b, .. }
        | MOp::CarryAddSub { rn: a, rm: b, .. }
        | MOp::Udiv { rn: a, rm: b, .. }
        | MOp::Sdiv { rn: a, rm: b, .. }
        | MOp::Umin { rn: a, rm: b, .. }
        | MOp::Umax { rn: a, rm: b, .. }
        | MOp::Smin { rn: a, rm: b, .. }
        | MOp::Smax { rn: a, rm: b, .. }
        | MOp::CmpReg { rn: a, rm: b, .. }
        | MOp::TstReg { rn: a, rm: b, .. }
        | MOp::CcmpReg { rn: a, rm: b, .. }
        | MOp::Csel { rn: a, rm: b, .. }
        | MOp::Csinc { rn: a, rm: b, .. }
        | MOp::Csinv { rn: a, rm: b, .. }
        | MOp::Csneg { rn: a, rm: b, .. }
        | MOp::FpCmp { rn: a, rm: b, .. }
        | MOp::Fcsel { rn: a, rm: b, .. } => {
            f(a);
            f(b);
        }
        MOp::Madd { rn, rm, ra, .. } | MOp::Msub { rn, rm, ra, .. } => {
            f(rn);
            f(rm);
            f(ra);
        }
        MOp::Ldr { addr, .. } => match addr {
            AddrMode::BaseOff { base, .. }
            | AddrMode::PostIdx { base, .. }
            | AddrMode::PreIdx { base, .. } => f(base),
            AddrMode::ScaledIdx { base, idx, .. } => {
                f(base);
                f(idx);
            }
        },
        MOp::Str { src, addr, .. } => {
            f(src);
            match addr {
                AddrMode::BaseOff { base, .. }
                | AddrMode::PostIdx { base, .. }
                | AddrMode::PreIdx { base, .. } => f(base),
                AddrMode::ScaledIdx { base, idx, .. } => {
                    f(base);
                    f(idx);
                }
            }
        }
        MOp::Ldp { base, .. } => f(base),
        MOp::Stp { src0, src1, base, .. } => {
            f(src0);
            f(src1);
            f(base);
        }
        MOp::FpAlu { rn, rm, ra, .. } => {
            f(rn);
            if let Some(m) = rm {
                f(m);
            }
            if let Some(a) = ra {
                f(a);
            }
        }
        MOp::VecAlu { op: vec_op, dst, rn, rm, .. } => {
            if !matches!(vec_op, VecAluOp::Zero) {
                if is_vec_rmw(vec_op) {
                    f(dst);
                }
                f(rn);
                if let Some(m) = rm {
                    f(m);
                }
            }
        }
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(copy_pairs.len());
            for &(_, src) in subslice(copy_pairs, s, e) {
                f(src);
            }
        }
        MOp::MovImm { .. }
        | MOp::Adrp { .. }
        | MOp::AdrTable { .. }
        | MOp::Bl
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Ret
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop
        | MOp::RawWord { .. } => {}
    }
}

/// Visits every register operand WRITTEN by `op` (exhaustive over all `MOp` variants).
fn visit_op_defs(op: &MOp, copy_pairs: &[(MReg, MReg)], mut f: impl FnMut(MReg)) {
    match *op {
        MOp::Mov { dst, .. }
        | MOp::MovImm { dst, .. }
        | MOp::MovSp { dst, .. }
        | MOp::AddImm { dst, .. }
        | MOp::SubImm { dst, .. }
        | MOp::AddReg { dst, .. }
        | MOp::SubReg { dst, .. }
        | MOp::AddExt { dst, .. }
        | MOp::SubExt { dst, .. }
        | MOp::LogicalImm { dst, .. }
        | MOp::LogicalReg { dst, .. }
        | MOp::Mvn { dst, .. }
        | MOp::ShiftVar { dst, .. }
        | MOp::Ubfm { dst, .. }
        | MOp::Sbfm { dst, .. }
        | MOp::Bfxil { dst, .. }
        | MOp::Extr { dst, .. }
        | MOp::Rbit { dst, .. }
        | MOp::Clz { dst, .. }
        | MOp::Rev { dst, .. }
        | MOp::Mul { dst, .. }
        | MOp::Madd { dst, .. }
        | MOp::Msub { dst, .. }
        | MOp::Umulh { dst, .. }
        | MOp::Smulh { dst, .. }
        | MOp::CarryAddSub { dst, .. }
        | MOp::Udiv { dst, .. }
        | MOp::Sdiv { dst, .. }
        | MOp::Umin { dst, .. }
        | MOp::Umax { dst, .. }
        | MOp::Smin { dst, .. }
        | MOp::Smax { dst, .. }
        | MOp::Csel { dst, .. }
        | MOp::Csinc { dst, .. }
        | MOp::Csinv { dst, .. }
        | MOp::Csneg { dst, .. }
        | MOp::FpAlu { dst, .. }
        | MOp::Fcsel { dst, .. }
        | MOp::VecAlu { dst, .. }
        | MOp::Adrp { dst }
        | MOp::AddLo12 { dst, .. }
        | MOp::AdrTable { dst, .. } => f(dst),
        MOp::Ldr { dst, addr, .. } => {
            f(dst);
            if let AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } = addr {
                f(base);
            }
        }
        MOp::Str { addr, .. } => {
            if let AddrMode::PostIdx { base, .. } | AddrMode::PreIdx { base, .. } = addr {
                f(base);
            }
        }
        MOp::Ldp { dst0, dst1, base, post_idx, pre_idx, .. } => {
            f(dst0);
            f(dst1);
            if post_idx || pre_idx {
                f(base);
            }
        }
        MOp::Stp { base, post_idx, pre_idx, .. } => {
            if post_idx || pre_idx {
                f(base);
            }
        }
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(copy_pairs.len());
            for &(dst, _) in subslice(copy_pairs, s, e) {
                f(dst);
            }
        }
        MOp::CmpImm { .. }
        | MOp::CmpReg { .. }
        | MOp::TstImm { .. }
        | MOp::TstReg { .. }
        | MOp::CcmpImm { .. }
        | MOp::CcmpReg { .. }
        | MOp::FpCmp { .. }
        | MOp::Bl
        | MOp::Blr { .. }
        | MOp::Br { .. }
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Cbz { .. }
        | MOp::Cbnz { .. }
        | MOp::Tbz { .. }
        | MOp::Tbnz { .. }
        | MOp::Ret
        | MOp::Brk { .. }
        | MOp::Prfm { .. }
        | MOp::Udf
        | MOp::Nop
        | MOp::RawWord { .. } => {}
    }
}

/// Returns `(phys_uses_mask, phys_defs_mask)` for the explicit physical operands of `op`
/// (including `RawWord` bitmasks, excluding implicit call/ret conventions).
fn explicit_phys_masks(op: &MOp, copy_pairs: &[(MReg, MReg)]) -> (u64, u64) {
    if let MOp::RawWord { defs_gpr, uses_gpr, defs_fp, uses_fp, .. } = *op {
        let uses = u64::from(uses_gpr) | (u64::from(uses_fp) << 32);
        let defs = u64::from(defs_gpr) | (u64::from(defs_fp) << 32);
        return (uses, defs);
    }
    let mut uses: u64 = 0;
    let mut defs: u64 = 0;
    visit_op_uses(op, copy_pairs, |r| {
        if let MReg::P(p) = r {
            uses |= preg_mask(p);
        }
    });
    visit_op_defs(op, copy_pairs, |r| {
        if let MReg::P(p) = r {
            defs |= preg_mask(p);
        }
    });
    (uses, defs)
}

/// Exact AArch64 instruction count (`1..=4`) emitted for `MOp::MovImm { imm, wide, .. }`.
#[inline]
fn mov_imm_inst_cost(imm: u64, wide: bool) -> u32 {
    if !wide {
        let val = imm & 0xffff_ffff;
        let lo = val & 0xffff;
        let hi = (val >> 16) & 0xffff;
        if hi == 0 || lo == 0 || hi == 0xffff || lo == 0xffff {
            return 1;
        }
        if crate::enc::logical_imm(val, 32).is_some() {
            return 1;
        }
        return 2;
    }

    let chunks = [
        imm & 0xffff,
        (imm >> 16) & 0xffff,
        (imm >> 32) & 0xffff,
        (imm >> 48) & 0xffff,
    ];
    let mut nonzero: u32 = 0;
    let mut non_ffff: u32 = 0;
    for &c in &chunks {
        if c != 0 {
            nonzero += 1;
        }
        if c != 0xffff {
            non_ffff += 1;
        }
    }
    if nonzero <= 1 || non_ffff <= 1 {
        return 1;
    }
    if crate::enc::logical_imm(imm, 64).is_some() {
        return 1;
    }
    nonzero.min(non_ffff)
}

/// Returns `true` iff `op` is a pure, non-flag-clobbering rematerializable definition of `v`.
fn is_rematerializable_def(
    op: &MOp,
    flags: MInstFlags,
    v: VReg,
    def_lin: u32,
    last_end: u32,
    used_in_hot: bool,
    sp_mod_lins: &[u32],
) -> bool {
    if flags.contains(MInstFlags::PINNED) {
        return false;
    }
    let target = MReg::V(v);
    let sp_unchanged_in_range = || {
        let last_lin = last_end >> 1;
        for &m in sp_mod_lins {
            if m > def_lin && m <= last_lin {
                return false;
            }
        }
        true
    };

    match *op {
        MOp::MovImm { dst, imm, wide } if dst == target => {
            !used_in_hot || mov_imm_inst_cost(imm, wide) <= 1
        }
        MOp::VecAlu { op: VecAluOp::Zero, dst, .. }
        | MOp::FpAlu { op: FpAluOp::MovFromGpr, dst, rn: MReg::P(PReg::Xzr), rm: None, ra: None, .. }
            if dst == target =>
        {
            true
        }
        MOp::Adrp { dst } | MOp::AdrTable { dst, .. } if dst == target => {
            flags.contains(MInstFlags::REMATERIALIZABLE)
        }
        MOp::AddImm { dst, rn: MReg::P(PReg::Sp), sets_flags: false, .. }
        | MOp::SubImm { dst, rn: MReg::P(PReg::Sp), sets_flags: false, .. }
        | MOp::MovSp { dst, src: MReg::P(PReg::Sp) }
            if dst == target =>
        {
            sp_unchanged_in_range()
        }
        _ => false,
    }
}

/// Rewrites every register operand of `op` using `map_use` (for read operands) and
/// `map_write(reg, is_rmw)` (for write-only operands when `is_rmw == false`, and read-modify-write
/// operands when `is_rmw == true`).
fn remap_op_regs(
    op: MOp,
    mut map_use: impl FnMut(MReg) -> MReg,
    mut map_write: impl FnMut(MReg, bool) -> MReg,
) -> MOp {
    match op {
        MOp::Mov { dst, src, wide } => MOp::Mov { dst: map_write(dst, false), src: map_use(src), wide },
        MOp::MovImm { dst, imm, wide } => MOp::MovImm { dst: map_write(dst, false), imm, wide },
        MOp::MovSp { dst, src } => MOp::MovSp { dst: map_write(dst, false), src: map_use(src) },
        MOp::AddImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            MOp::AddImm { dst: map_write(dst, false), rn: map_use(rn), imm12, shift12, wide, sets_flags }
        }
        MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            MOp::SubImm { dst: map_write(dst, false), rn: map_use(rn), imm12, shift12, wide, sets_flags }
        }
        MOp::AddReg { dst, rn, rm, shift, wide, sets_flags } => {
            MOp::AddReg { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), shift, wide, sets_flags }
        }
        MOp::SubReg { dst, rn, rm, shift, wide, sets_flags } => {
            MOp::SubReg { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), shift, wide, sets_flags }
        }
        MOp::AddExt { dst, rn, rm, ext, wide, sets_flags } => {
            MOp::AddExt { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), ext, wide, sets_flags }
        }
        MOp::SubExt { dst, rn, rm, ext, wide, sets_flags } => {
            MOp::SubExt { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), ext, wide, sets_flags }
        }
        MOp::LogicalImm { dst, rn, imm, kind, wide } => {
            MOp::LogicalImm { dst: map_write(dst, false), rn: map_use(rn), imm, kind, wide }
        }
        MOp::LogicalReg { dst, rn, rm, shift, invert_rm, kind, wide } => MOp::LogicalReg {
            dst: map_write(dst, false),
            rn: map_use(rn),
            rm: map_use(rm),
            shift,
            invert_rm,
            kind,
            wide,
        },
        MOp::Mvn { dst, rm, shift, wide } => MOp::Mvn { dst: map_write(dst, false), rm: map_use(rm), shift, wide },
        MOp::ShiftVar { dst, rn, rm, kind, wide } => {
            MOp::ShiftVar { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), kind, wide }
        }
        MOp::Ubfm { dst, rn, immr, imms, wide } => {
            MOp::Ubfm { dst: map_write(dst, false), rn: map_use(rn), immr, imms, wide }
        }
        MOp::Sbfm { dst, rn, immr, imms, wide } => {
            MOp::Sbfm { dst: map_write(dst, false), rn: map_use(rn), immr, imms, wide }
        }
        MOp::Bfxil { dst, rn, lsb, width, wide } => {
            MOp::Bfxil { dst: map_write(dst, true), rn: map_use(rn), lsb, width, wide }
        }
        MOp::Extr { dst, rn, rm, lsb, wide } => {
            MOp::Extr { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), lsb, wide }
        }
        MOp::Rbit { dst, rn, wide } => MOp::Rbit { dst: map_write(dst, false), rn: map_use(rn), wide },
        MOp::Clz { dst, rn, wide } => MOp::Clz { dst: map_write(dst, false), rn: map_use(rn), wide },
        MOp::Rev { dst, rn, wide } => MOp::Rev { dst: map_write(dst, false), rn: map_use(rn), wide },
        MOp::Mul { dst, rn, rm, wide } => {
            MOp::Mul { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), wide }
        }
        MOp::Madd { dst, rn, rm, ra, wide } => {
            MOp::Madd { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), ra: map_use(ra), wide }
        }
        MOp::Msub { dst, rn, rm, ra, wide } => {
            MOp::Msub { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), ra: map_use(ra), wide }
        }
        MOp::Umulh { dst, rn, rm } => MOp::Umulh { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm) },
        MOp::Smulh { dst, rn, rm } => MOp::Smulh { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm) },
        MOp::CarryAddSub { dst, rn, rm, is_sub, sets_flags } => MOp::CarryAddSub {
            dst: map_write(dst, false),
            rn: map_use(rn),
            rm: map_use(rm),
            is_sub,
            sets_flags,
        },
        MOp::Udiv { dst, rn, rm, wide } => {
            MOp::Udiv { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), wide }
        }
        MOp::Sdiv { dst, rn, rm, wide } => {
            MOp::Sdiv { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), wide }
        }
        MOp::Umin { dst, rn, rm, wide } => {
            MOp::Umin { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), wide }
        }
        MOp::Umax { dst, rn, rm, wide } => {
            MOp::Umax { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), wide }
        }
        MOp::Smin { dst, rn, rm, wide } => {
            MOp::Smin { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), wide }
        }
        MOp::Smax { dst, rn, rm, wide } => {
            MOp::Smax { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), wide }
        }
        MOp::CmpImm { rn, imm12, shift12, wide } => {
            MOp::CmpImm { rn: map_use(rn), imm12, shift12, wide }
        }
        MOp::CmpReg { rn, rm, shift, wide } => {
            MOp::CmpReg { rn: map_use(rn), rm: map_use(rm), shift, wide }
        }
        MOp::TstImm { rn, imm, wide } => MOp::TstImm { rn: map_use(rn), imm, wide },
        MOp::TstReg { rn, rm, wide } => MOp::TstReg { rn: map_use(rn), rm: map_use(rm), wide },
        MOp::CcmpImm { rn, imm5, nzcv, cond, wide } => {
            MOp::CcmpImm { rn: map_use(rn), imm5, nzcv, cond, wide }
        }
        MOp::CcmpReg { rn, rm, nzcv, cond, wide } => {
            MOp::CcmpReg { rn: map_use(rn), rm: map_use(rm), nzcv, cond, wide }
        }
        MOp::Csel { dst, rn, rm, cond, wide } => {
            MOp::Csel { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), cond, wide }
        }
        MOp::Csinc { dst, rn, rm, cond, wide } => {
            MOp::Csinc { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), cond, wide }
        }
        MOp::Csinv { dst, rn, rm, cond, wide } => {
            MOp::Csinv { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), cond, wide }
        }
        MOp::Csneg { dst, rn, rm, cond, wide } => {
            MOp::Csneg { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), cond, wide }
        }
        MOp::Ldr { dst, addr, size, signed } => {
            let new_addr = match addr {
                AddrMode::BaseOff { base, off } => AddrMode::BaseOff { base: map_use(base), off },
                AddrMode::ScaledIdx { base, idx, shift } => {
                    AddrMode::ScaledIdx { base: map_use(base), idx: map_use(idx), shift }
                }
                AddrMode::PostIdx { base, off } => AddrMode::PostIdx { base: map_write(base, true), off },
                AddrMode::PreIdx { base, off } => AddrMode::PreIdx { base: map_write(base, true), off },
            };
            MOp::Ldr { dst: map_write(dst, false), addr: new_addr, size, signed }
        }
        MOp::Str { src, addr, size } => {
            let new_src = map_use(src);
            let new_addr = match addr {
                AddrMode::BaseOff { base, off } => AddrMode::BaseOff { base: map_use(base), off },
                AddrMode::ScaledIdx { base, idx, shift } => {
                    AddrMode::ScaledIdx { base: map_use(base), idx: map_use(idx), shift }
                }
                AddrMode::PostIdx { base, off } => AddrMode::PostIdx { base: map_write(base, true), off },
                AddrMode::PreIdx { base, off } => AddrMode::PreIdx { base: map_write(base, true), off },
            };
            MOp::Str { src: new_src, addr: new_addr, size }
        }
        MOp::Ldp { dst0, dst1, base, off, size, post_idx, pre_idx } => {
            let new_base = if post_idx || pre_idx { map_write(base, true) } else { map_use(base) };
            MOp::Ldp {
                dst0: map_write(dst0, false),
                dst1: map_write(dst1, false),
                base: new_base,
                off,
                size,
                post_idx,
                pre_idx,
            }
        }
        MOp::Stp { src0, src1, base, off, size, post_idx, pre_idx } => {
            let new_src0 = map_use(src0);
            let new_src1 = map_use(src1);
            let new_base = if post_idx || pre_idx { map_write(base, true) } else { map_use(base) };
            MOp::Stp { src0: new_src0, src1: new_src1, base: new_base, off, size, post_idx, pre_idx }
        }
        MOp::FpAlu { op: fp_op, dst, rn, rm, ra, double } => MOp::FpAlu {
            op: fp_op,
            dst: map_write(dst, false),
            rn: map_use(rn),
            rm: rm.map(&mut map_use),
            ra: ra.map(&mut map_use),
            double,
        },
        MOp::FpCmp { rn, rm, double } => MOp::FpCmp { rn: map_use(rn), rm: map_use(rm), double },
        MOp::Fcsel { dst, rn, rm, cond, double } => {
            MOp::Fcsel { dst: map_write(dst, false), rn: map_use(rn), rm: map_use(rm), cond, double }
        }
        MOp::VecAlu { op: vec_op, dst, rn, rm, elem_size } => {
            if matches!(vec_op, VecAluOp::Zero) {
                MOp::VecAlu { op: vec_op, dst: map_write(dst, false), rn: MReg::vec(0), rm: None, elem_size }
            } else {
                let new_dst = map_write(dst, is_vec_rmw(vec_op));
                MOp::VecAlu {
                    op: vec_op,
                    dst: new_dst,
                    rn: map_use(rn),
                    rm: rm.map(&mut map_use),
                    elem_size,
                }
            }
        }
        MOp::Adrp { dst } => MOp::Adrp { dst: map_write(dst, false) },
        MOp::AddLo12 { dst, rn } => MOp::AddLo12 { dst: map_write(dst, false), rn: map_use(rn) },
        MOp::AdrTable { dst, table_id } => MOp::AdrTable { dst: map_write(dst, false), table_id },
        MOp::Blr { target } => MOp::Blr { target: map_use(target) },
        MOp::Br { target } => MOp::Br { target: map_use(target) },
        MOp::Cbz { rt, target, wide } => MOp::Cbz { rt: map_use(rt), target, wide },
        MOp::Cbnz { rt, target, wide } => MOp::Cbnz { rt: map_use(rt), target, wide },
        MOp::Tbz { rt, bit, target } => MOp::Tbz { rt: map_use(rt), bit, target },
        MOp::Tbnz { rt, bit, target } => MOp::Tbnz { rt: map_use(rt), bit, target },
        MOp::Prfm { base, imm12 } => MOp::Prfm { base: map_use(base), imm12 },
        other @ (MOp::Bl
        | MOp::B { .. }
        | MOp::BCond { .. }
        | MOp::Ret
        | MOp::Brk { .. }
        | MOp::Udf
        | MOp::Nop
        | MOp::ParallelCopy { .. }
        | MOp::RawWord { .. }) => other,
    }
}

// -----------------------------------------------------------------------------
// Bitset & Disjoint-Range Helpers
// -----------------------------------------------------------------------------

#[inline]
/// Builds a compact CSR `(offsets, data)` from `(key, value)` pairs for `key in 0..n_keys`.
fn build_u32_csr<'a>(
    n_keys: usize,
    pairs: &[(u32, u32)],
    alloc: Alloc<'a>,
) -> Result<(Vec<u32, Alloc<'a>>, Vec<u32, Alloc<'a>>), MRaError> {
    let mut off: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_keys.saturating_add(1), alloc);
    for _ in 0..=n_keys {
        off.push(0);
    }
    for &(k, _) in pairs {
        let ki = to_idx(k);
        if let Some(slot) = off.get_mut(ki.saturating_add(1)) {
            *slot = slot.checked_add(1).ok_or(MRaError::FunctionTooLarge)?;
        }
    }
    let mut running: u32 = 0;
    for slot in off.iter_mut() {
        running = running.checked_add(*slot).ok_or(MRaError::FunctionTooLarge)?;
        *slot = running;
    }
    let total = to_idx(running);
    let mut data: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(total, alloc);
    for _ in 0..total {
        data.push(0);
    }
    let mut cursor: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_keys, alloc);
    for i in 0..n_keys {
        cursor.push(slice_at(&off, i, 0));
    }
    for &(k, v) in pairs {
        let ki = to_idx(k);
        if let Some(cur) = cursor.get_mut(ki) {
            let pos = to_idx(*cur);
            if pos < data.len() {
                slice_set(&mut data, pos, v);
                *cur = cur.saturating_add(1);
            }
        }
    }
    Ok((off, data))
}

/// Compact outputs of sparse per-global-`VReg` backward CFG BFS liveness.
struct SparseVRegLiveness<'a> {
    /// Per-block CSR of `VReg` IDs (`v.0`) that are BOTH `live_out[b]` AND defined in `b`
    /// (non-empty blocks only). Total entries are bounded by `global_defs.len() <= O(n_insts)`.
    def_lo_off: Vec<u32, Alloc<'a>>,
    def_lo_vregs: Vec<u32, Alloc<'a>>,
    /// Per-global-`VReg` (`g in 0..n_global_vregs`) CSR of sorted, coalesced half-point spans
    /// `[blk_start_pt, blk_end_pt]` for non-empty pass-through `live_out` blocks (no defs of `g`).
    pass_through_off: Vec<u32, Alloc<'a>>,
    pass_through_ranges: Vec<(u32, u32), Alloc<'a>>,
    /// Per-global-`VReg` flag: `true` iff `g` is `live_in` or `live_out` at at least one hot block.
    /// Empty when `block_is_hot` is empty (`!has_hot_cold_weights`).
    vreg_crosses_hot: Vec<bool, Alloc<'a>>,
}

/// Computes exact `VReg` liveness via unified per-global-`VReg` backward CFG BFS (Appel / Brandner /
/// regalloc2 style), handling arbitrary multi-def non-SSA `VReg`s and irreducible CFGs using only
/// `O(n_blocks)` reusable epoch scratch (zero `O(n_blocks * n_global_vregs)` bitsets).
///
/// `observe_live(g, blk_id, is_live_out)` is invoked once per newly discovered `(g, blk_id)` live-in
/// (`false`) or live-out (`true`) fact; production callers pass a no-op closure.
#[expect(clippy::too_many_arguments, reason = "internal sparse liveness kernel taking pre-linearized slices")]
fn compute_sparse_vreg_liveness_with<'a>(
    func: &MFunc<'_>,
    block_lin_range: &[(u32, u32)],
    block_ord_pos: &[u32],
    block_is_hot: &[bool],
    global_to_vreg: &[u32],
    upward_uses: &[(u32, u32)],
    global_defs: &[(u32, u32)],
    alloc: Alloc<'a>,
    mut observe_live: impl FnMut(u32, u32, bool),
) -> Result<SparseVRegLiveness<'a>, MRaError> {
    let n_blocks = func.block_insts.len();
    let n_global = global_to_vreg.len();
    if n_global == 0 {
        return Ok(SparseVRegLiveness {
            def_lo_off: Vec::new_in(alloc),
            def_lo_vregs: Vec::new_in(alloc),
            pass_through_off: Vec::new_in(alloc),
            pass_through_ranges: Vec::new_in(alloc),
            vreg_crosses_hot: Vec::new_in(alloc),
        });
    }
    if u32_of(n_global.saturating_add(1)).is_none() {
        return Err(MRaError::FunctionTooLarge);
    }

    let (use_off, use_blocks) = build_u32_csr(n_global, upward_uses, alloc)?;
    let (def_off, def_blocks) = build_u32_csr(n_global, global_defs, alloc)?;

    // Reusable `O(n_blocks)` epoch scratch (zero per-vreg heap allocations).
    let mut def_epoch: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut live_in_epoch: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut live_out_epoch: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    for _ in 0..n_blocks {
        def_epoch.push(0);
        live_in_epoch.push(0);
        live_out_epoch.push(0);
    }
    let mut bfs_stack: Vec<MBlock, Alloc<'a>> = Vec::new_in(alloc);
    let mut pass_through_spans: Vec<(u32, u32), Alloc<'a>> = Vec::new_in(alloc);

    let mut def_and_live_out_pairs: Vec<(u32, u32), Alloc<'a>> = Vec::new_in(alloc);
    let mut pass_through_off: Vec<u32, Alloc<'a>> =
        Vec::with_capacity_in(n_global.saturating_add(1), alloc);
    let mut pass_through_ranges: Vec<(u32, u32), Alloc<'a>> = Vec::new_in(alloc);

    let track_hot = !block_is_hot.is_empty();
    let mut vreg_crosses_hot: Vec<bool, Alloc<'a>> = Vec::new_in(alloc);
    if track_hot {
        vreg_crosses_hot.reserve(n_global);
        for _ in 0..n_global {
            vreg_crosses_hot.push(false);
        }
    }

    for g in 0..n_global {
        let g_u32 = u32_of(g).ok_or(MRaError::FunctionTooLarge)?;
        let epoch = g_u32.checked_add(1).ok_or(MRaError::FunctionTooLarge)?;
        let v_u32 = slice_at(global_to_vreg, g, NONE_U32);
        pass_through_off
            .push(u32_of(pass_through_ranges.len()).ok_or(MRaError::FunctionTooLarge)?);

        let ds = to_idx(slice_at(&def_off, g, 0));
        let de = to_idx(slice_at(&def_off, g + 1, 0));
        for &db in subslice(&def_blocks, ds, de) {
            slice_set(&mut def_epoch, to_idx(db), epoch);
        }

        let mut crosses_hot = false;
        let us = to_idx(slice_at(&use_off, g, 0));
        let ue = to_idx(slice_at(&use_off, g + 1, 0));
        for &ub in subslice(&use_blocks, us, ue) {
            let bi = to_idx(ub);
            if bi < n_blocks
                && slice_at(block_ord_pos, bi, NONE_U32) != NONE_U32
                && slice_at(&live_in_epoch, bi, 0) != epoch
            {
                slice_set(&mut live_in_epoch, bi, epoch);
                observe_live(g_u32, ub, false);
                if track_hot && slice_at(block_is_hot, bi, false) {
                    crosses_hot = true;
                }
                bfs_stack.push(MBlock(ub));
            }
        }

        // Exact backward BFS: each block is marked `live_in_epoch = epoch` before push, so this
        // loop pops at most `n_blocks` blocks per global `VReg`.
        while let Some(blk) = bfs_stack.pop() {
            for &pred in func.block_preds(blk) {
                let pb = to_idx(pred.0);
                if pb >= n_blocks || slice_at(block_ord_pos, pb, NONE_U32) == NONE_U32 {
                    continue;
                }
                if slice_at(&live_out_epoch, pb, 0) != epoch {
                    slice_set(&mut live_out_epoch, pb, epoch);
                    observe_live(g_u32, pred.0, true);
                    if track_hot && slice_at(block_is_hot, pb, false) {
                        crosses_hot = true;
                    }
                    let (ls, le) = slice_at(block_lin_range, pb, (0, 0));
                    if ls < le {
                        if slice_at(&def_epoch, pb, 0) == epoch {
                            if v_u32 != NONE_U32 {
                                def_and_live_out_pairs.push((pred.0, v_u32));
                            }
                        } else {
                            let blk_start_pt = ls.saturating_mul(2);
                            let blk_end_pt = (le - 1).saturating_mul(2).saturating_add(1);
                            pass_through_spans.push((blk_start_pt, blk_end_pt));
                        }
                    }
                }
                if slice_at(&def_epoch, pb, 0) != epoch
                    && slice_at(&live_in_epoch, pb, 0) != epoch
                {
                    slice_set(&mut live_in_epoch, pb, epoch);
                    observe_live(g_u32, pred.0, false);
                    if track_hot && slice_at(block_is_hot, pb, false) {
                        crosses_hot = true;
                    }
                    bfs_stack.push(pred);
                }
            }
        }

        if track_hot && crosses_hot {
            slice_set(&mut vreg_crosses_hot, g, true);
        }

        if !pass_through_spans.is_empty() {
            pass_through_spans.sort_unstable_by_key(|&(s, _)| s);
            let mut iter = pass_through_spans.iter().copied();
            if let Some((mut cur_s, mut cur_e)) = iter.next() {
                for (s, e) in iter {
                    if s <= cur_e.saturating_add(1) {
                        if e > cur_e {
                            cur_e = e;
                        }
                    } else {
                        pass_through_ranges.push((cur_s, cur_e));
                        cur_s = s;
                        cur_e = e;
                    }
                }
                pass_through_ranges.push((cur_s, cur_e));
            }
            pass_through_spans.clear();
        }
    }

    pass_through_off.push(u32_of(pass_through_ranges.len()).ok_or(MRaError::FunctionTooLarge)?);
    let (def_lo_off, def_lo_vregs) = build_u32_csr(n_blocks, &def_and_live_out_pairs, alloc)?;

    Ok(SparseVRegLiveness {
        def_lo_off,
        def_lo_vregs,
        pass_through_off,
        pass_through_ranges,
        vreg_crosses_hot,
    })
}

/// Inserts `[s, e]` into the ascending linked list rooted at `*head`, merging overlapping/adjacent
/// spans in O(1) amortized time during a backward pass.
fn range_list_add(
    head: &mut u32,
    nodes: &mut Vec<RangeNode, Alloc<'_>>,
    s: u32,
    e: u32,
) {
    if *head == NONE_U32 {
        let idx = u32_of(nodes.len()).unwrap_or(NONE_U32);
        nodes.push(RangeNode { start: s, end: e, next: NONE_U32 });
        *head = idx;
        return;
    }

    let h = to_idx(*head);
    let Some(mut h_node) = nodes.get(h).copied() else {
        return;
    };
    if e.saturating_add(1) < h_node.start {
        let idx = u32_of(nodes.len()).unwrap_or(NONE_U32);
        nodes.push(RangeNode { start: s, end: e, next: *head });
        *head = idx;
        return;
    }

    if s <= h_node.end.saturating_add(1) {
        if s < h_node.start {
            h_node.start = s;
        }
        if e > h_node.end {
            h_node.end = e;
        }
        // Absorb any subsequent nodes now overlapping/adjacent to `head`.
        let mut next = h_node.next;
        while next != NONE_U32 {
            let Some(n_node) = nodes.get(to_idx(next)).copied() else {
                break;
            };
            if h_node.end.saturating_add(1) >= n_node.start {
                if n_node.end > h_node.end {
                    h_node.end = n_node.end;
                }
                next = n_node.next;
                h_node.next = next;
            } else {
                break;
            }
        }
        slice_set(nodes, h, h_node);
        return;
    }

    // Rare out-of-order insert: walk until sorted insertion position.
    let mut prev = h;
    let mut cur = h_node.next;
    while cur != NONE_U32 {
        let ci = to_idx(cur);
        let Some(mut c_node) = nodes.get(ci).copied() else {
            break;
        };
        if e.saturating_add(1) < c_node.start {
            let idx = u32_of(nodes.len()).unwrap_or(NONE_U32);
            nodes.push(RangeNode { start: s, end: e, next: cur });
            if let Some(p_node) = nodes.get_mut(prev) {
                p_node.next = idx;
            }
            return;
        }
        if s <= c_node.end.saturating_add(1) {
            if s < c_node.start {
                c_node.start = s;
            }
            if e > c_node.end {
                c_node.end = e;
            }
            let mut next = c_node.next;
            while next != NONE_U32 {
                let Some(n_node) = nodes.get(to_idx(next)).copied() else {
                    break;
                };
                if c_node.end.saturating_add(1) >= n_node.start {
                    if n_node.end > c_node.end {
                        c_node.end = n_node.end;
                    }
                    next = n_node.next;
                    c_node.next = next;
                } else {
                    break;
                }
            }
            slice_set(nodes, ci, c_node);
            return;
        }
        prev = ci;
        cur = c_node.next;
    }
    let idx = u32_of(nodes.len()).unwrap_or(NONE_U32);
    nodes.push(RangeNode { start: s, end: e, next: NONE_U32 });
    if let Some(p_node) = nodes.get_mut(prev) {
        p_node.next = idx;
    }
}

/// Shortens the front of `*head` to `def_pt` when covered by `block_start`, or inserts `[def_pt, def_pt]`
/// for a dead definition.
fn range_list_shorten_def(
    head: &mut u32,
    nodes: &mut Vec<RangeNode, Alloc<'_>>,
    block_start: u32,
    def_pt: u32,
) {
    if *head != NONE_U32
        && let Some(h_node) = nodes.get_mut(to_idx(*head))
    {
        if h_node.start == block_start && def_pt <= h_node.end {
            h_node.start = def_pt;
            return;
        }
        if h_node.start <= def_pt && def_pt <= h_node.end {
            return;
        }
    }
    range_list_add(head, nodes, def_pt, def_pt);
}

/// Overlap test over two sorted disjoint half-point range slices.
///
/// Uses O(1) bounding-box rejection and O(|small| log |large|) binary search when the two slices
/// differ sharply in size (the common `VReg` vs `phys_spans` case on large functions), falling into
/// a linear two-pointer merge when lengths are comparable.
#[inline]
fn ranges_overlap(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
    let (Some(&(a0_s, a0_e)), Some(&(b0_s, b0_e))) = (a.first(), b.first()) else {
        return false;
    };
    let a_last_e = a[a.len() - 1].1;
    let b_last_e = b[b.len() - 1].1;
    if a_last_e < b0_s || b_last_e < a0_s {
        return false;
    }
    if a.len() == 1 {
        let j = b.partition_point(|&(_, b_e)| b_e < a0_s);
        return b.get(j).is_some_and(|&(b_s, _)| b_s <= a0_e);
    }
    if b.len() == 1 {
        let i = a.partition_point(|&(_, a_e)| a_e < b0_s);
        return a.get(i).is_some_and(|&(a_s, _)| a_s <= b0_e);
    }
    if b.len() > a.len().saturating_mul(4) {
        let mut j = 0usize;
        for &(a_s, a_e) in a {
            j += b[j..].partition_point(|&(_, b_e)| b_e < a_s);
            let Some(&(b_s, _)) = b.get(j) else {
                return false;
            };
            if b_s <= a_e {
                return true;
            }
        }
        return false;
    }
    if a.len() > b.len().saturating_mul(4) {
        let mut i = 0usize;
        for &(b_s, b_e) in b {
            i += a[i..].partition_point(|&(_, a_e)| a_e < b_s);
            let Some(&(a_s, _)) = a.get(i) else {
                return false;
            };
            if a_s <= b_e {
                return true;
            }
        }
        return false;
    }
    let mut i = 0usize;
    let mut j = 0usize;
    while let (Some(&(a_s, a_e)), Some(&(b_s, b_e))) = (a.get(i), b.get(j)) {
        if a_e < b_s {
            i += 1;
        } else if b_e < a_s {
            j += 1;
        } else {
            return true;
        }
    }
    false
}

#[inline]
fn meta_ranges<'a>(vreg_ranges: &'a [(u32, u32)], meta: VRegMeta) -> &'a [(u32, u32)] {
    let s = to_idx(meta.range_start_idx);
    let e = to_idx(meta.range_start_idx.saturating_add(meta.range_len));
    subslice(vreg_ranges, s, e)
}

/// Spill-slot bump allocator (16-byte aligned base, 8B scalar / 16B `Vec128` slots).
struct SpillSlots {
    base_off: u32,
    cur_rel_off: u32,
    count: u32,
}

impl SpillSlots {
    #[inline]
    const fn new(base_off: u32) -> Self {
        Self { base_off, cur_rel_off: 0, count: 0 }
    }

    fn alloc_slot(&mut self, size: MemSize) -> Result<u32, MRaError> {
        let align = if matches!(size, MemSize::Q128) { 16u32 } else { 8u32 };
        let slot_bytes = align;
        let rel = align_up(self.cur_rel_off, align)
            .ok_or(MRaError::InvalidSpillOffset { off: u32::MAX })?;
        let abs_off = self
            .base_off
            .checked_add(rel)
            .ok_or(MRaError::InvalidSpillOffset { off: u32::MAX })?;
        if abs_off > MAX_SPILL_BYTE_OFF {
            return Err(MRaError::InvalidSpillOffset { off: abs_off });
        }
        let next_rel = rel
            .checked_add(slot_bytes)
            .ok_or(MRaError::InvalidSpillOffset { off: abs_off })?;
        self.cur_rel_off = next_rel;
        self.count = self.count.saturating_add(1);
        Ok(abs_off)
    }

    #[inline]
    fn total_bytes_aligned16(&self) -> Result<u32, MRaError> {
        align_up(self.cur_rel_off, 16)
            .ok_or(MRaError::InvalidSpillOffset { off: self.cur_rel_off })
    }
}

// -----------------------------------------------------------------------------
// Rewrite Builder & Spill / Remat / ParallelCopy Emission Helpers
// -----------------------------------------------------------------------------

struct RewriteOut<'a> {
    ops: Vec<MOp, Alloc<'a>>,
    flags: Vec<MInstFlags, Alloc<'a>>,
    vflag_def: Vec<u32, Alloc<'a>>,
    vflag_use: Vec<u32, Alloc<'a>>,
    note_idx: Vec<u32, Alloc<'a>>,
    notes: Vec<MNote, Alloc<'a>>,
    spill_base_off: u32,
    spill_read: Vec<bool, Alloc<'a>>,
    spill_written: Vec<bool, Alloc<'a>>,
}

impl<'a> RewriteOut<'a> {
    fn new(cap: usize, orig_notes: &[MNote], spill_base_off: u32, spill_bytes: u32, alloc: Alloc<'a>) -> Self {
        let mut notes = Vec::with_capacity_in(orig_notes.len(), alloc);
        for &n in orig_notes {
            notes.push(n);
        }
        let n_slots = to_idx(spill_bytes / 8).saturating_add(1);
        let mut spill_read = Vec::with_capacity_in(n_slots, alloc);
        let mut spill_written = Vec::with_capacity_in(n_slots, alloc);
        for _ in 0..n_slots {
            spill_read.push(false);
            spill_written.push(false);
        }
        Self {
            ops: Vec::with_capacity_in(cap, alloc),
            flags: Vec::with_capacity_in(cap, alloc),
            vflag_def: Vec::with_capacity_in(cap, alloc),
            vflag_use: Vec::with_capacity_in(cap, alloc),
            note_idx: Vec::with_capacity_in(cap, alloc),
            notes,
            spill_base_off,
            spill_read,
            spill_written,
        }
    }

    #[inline]
    fn mark_spill_read(&mut self, off: u32) {
        if let Some(rel) = off.checked_sub(self.spill_base_off)
            && let Some(slot) = self.spill_read.get_mut(to_idx(rel / 8))
        {
            *slot = true;
        }
    }

    #[inline]
    fn mark_spill_written(&mut self, off: u32) {
        if let Some(rel) = off.checked_sub(self.spill_base_off)
            && let Some(slot) = self.spill_written.get_mut(to_idx(rel / 8))
        {
            *slot = true;
        }
    }

    #[inline]
    fn push(&mut self, op: MOp, flags: MInstFlags, vdef: u32, vuse: u32, note: u32) -> usize {
        let idx = self.ops.len();
        self.ops.push(op);
        self.flags.push(flags);
        self.vflag_def.push(vdef);
        self.vflag_use.push(vuse);
        self.note_idx.push(note);
        idx
    }

    /// Clones any `MNote` chain attached to `orig_note_head` onto newly emitted instruction `new_idx`.
    fn clone_note_chain(&mut self, new_idx: usize, orig_note_head: u32) {
        let mut cur = orig_note_head;
        let max_steps = self.notes.len().saturating_add(1);
        let mut steps = 0usize;
        while cur != NO_NOTE && steps < max_steps {
            let ci = to_idx(cur);
            let Some(&note) = self.notes.get(ci) else {
                break;
            };
            let next_head = slice_at(&self.note_idx, new_idx, NO_NOTE);
            let new_note_idx = u32_of(self.notes.len()).unwrap_or(NO_NOTE);
            self.notes.push(MNote { kind: note.kind, next: next_head });
            slice_set(&mut self.note_idx, new_idx, new_note_idx);
            cur = note.next;
            steps += 1;
        }
    }

    /// Emits a spill-slot load `dst <- [SP + off]` (never touches NZCV).
    fn emit_spill_load(
        &mut self,
        dst: PReg,
        off: u32,
        size: MemSize,
        addr_scratch: PReg,
    ) -> Result<(), MRaError> {
        self.mark_spill_read(off);
        let bytes = size.byte_size();
        let max_direct = 4095u32.saturating_mul(bytes);
        if off <= max_direct && (off % bytes) == 0 {
            let off_i32 = i32_of(off).ok_or(MRaError::InvalidSpillOffset { off })?;
            self.push(
                MOp::Ldr {
                    dst: MReg::P(dst),
                    addr: AddrMode::BaseOff { base: MReg::SP, off: off_i32 },
                    size,
                    signed: false,
                },
                MInstFlags::MEM_READ,
                NO_VFLAG,
                NO_VFLAG,
                NO_NOTE,
            );
            return Ok(());
        }
        let hi_pages = off >> 12;
        let lo12 = off & 0xfff;
        let hi_u16 = u16_of(hi_pages).ok_or(MRaError::InvalidSpillOffset { off })?;
        let lo_i32 = i32_of(lo12).ok_or(MRaError::InvalidSpillOffset { off })?;
        self.push(
            MOp::AddImm {
                dst: MReg::P(addr_scratch),
                rn: MReg::SP,
                imm12: hi_u16,
                shift12: true,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
            NO_VFLAG,
            NO_VFLAG,
            NO_NOTE,
        );
        self.push(
            MOp::Ldr {
                dst: MReg::P(dst),
                addr: AddrMode::BaseOff { base: MReg::P(addr_scratch), off: lo_i32 },
                size,
                signed: false,
            },
            MInstFlags::MEM_READ,
            NO_VFLAG,
            NO_VFLAG,
            NO_NOTE,
        );
        Ok(())
    }

    /// Emits a spill-slot store `[SP + off] <- src` (never touches NZCV).
    fn emit_spill_store(
        &mut self,
        src: PReg,
        off: u32,
        size: MemSize,
        addr_scratch: PReg,
    ) -> Result<(), MRaError> {
        self.mark_spill_written(off);
        let bytes = size.byte_size();
        let max_direct = 4095u32.saturating_mul(bytes);
        if off <= max_direct && (off % bytes) == 0 {
            let off_i32 = i32_of(off).ok_or(MRaError::InvalidSpillOffset { off })?;
            self.push(
                MOp::Str {
                    src: MReg::P(src),
                    addr: AddrMode::BaseOff { base: MReg::SP, off: off_i32 },
                    size,
                },
                MInstFlags::MEM_WRITE,
                NO_VFLAG,
                NO_VFLAG,
                NO_NOTE,
            );
            return Ok(());
        }
        let hi_pages = off >> 12;
        let lo12 = off & 0xfff;
        let hi_u16 = u16_of(hi_pages).ok_or(MRaError::InvalidSpillOffset { off })?;
        let lo_i32 = i32_of(lo12).ok_or(MRaError::InvalidSpillOffset { off })?;
        self.push(
            MOp::AddImm {
                dst: MReg::P(addr_scratch),
                rn: MReg::SP,
                imm12: hi_u16,
                shift12: true,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
            NO_VFLAG,
            NO_VFLAG,
            NO_NOTE,
        );
        self.push(
            MOp::Str {
                src: MReg::P(src),
                addr: AddrMode::BaseOff { base: MReg::P(addr_scratch), off: lo_i32 },
                size,
            },
            MInstFlags::MEM_WRITE,
            NO_VFLAG,
            NO_VFLAG,
            NO_NOTE,
        );
        Ok(())
    }

    /// Re-emits a rematerializable defining instruction `def_inst` directly into physical `dst`.
    fn emit_remat(
        &mut self,
        dst: PReg,
        def_inst: u32,
        orig_ops: &[MOp],
        orig_note_idx: &[u32],
    ) {
        let di = to_idx(def_inst);
        let Some(&def_op) = orig_ops.get(di) else {
            return;
        };
        let rewritten = match def_op {
            MOp::MovImm { imm, wide, .. } => MOp::MovImm { dst: MReg::P(dst), imm, wide },
            MOp::Adrp { .. } => MOp::Adrp { dst: MReg::P(dst) },
            MOp::AdrTable { table_id, .. } => MOp::AdrTable { dst: MReg::P(dst), table_id },
            MOp::AddImm { rn, imm12, shift12, wide, .. } => MOp::AddImm {
                dst: MReg::P(dst),
                rn,
                imm12,
                shift12,
                wide,
                sets_flags: false,
            },
            MOp::SubImm { rn, imm12, shift12, wide, .. } => MOp::SubImm {
                dst: MReg::P(dst),
                rn,
                imm12,
                shift12,
                wide,
                sets_flags: false,
            },
            MOp::MovSp { src, .. } => MOp::MovSp { dst: MReg::P(dst), src },
            MOp::FpAlu { op: FpAluOp::MovFromGpr, rn, double, .. } => MOp::FpAlu {
                op: FpAluOp::MovFromGpr,
                dst: MReg::P(dst),
                rn,
                rm: None,
                ra: None,
                double,
            },
            MOp::VecAlu { op: VecAluOp::Zero, elem_size, .. } => MOp::VecAlu {
                op: VecAluOp::Zero,
                dst: MReg::P(dst),
                rn: MReg::vec(0),
                rm: None,
                elem_size,
            },
            _ => return,
        };
        let new_idx = self.push(rewritten, MInstFlags::REMATERIALIZABLE, NO_VFLAG, NO_VFLAG, NO_NOTE);
        if let Some(&nhead) = orig_note_idx.get(di)
            && nhead != NO_NOTE
        {
            self.clone_note_chain(new_idx, nhead);
        }
    }

    /// Emits a single physical register-to-register move (GPR or 128-bit FP/NEON).
    fn emit_reg_copy(&mut self, dst: PReg, src: PReg) {
        if dst == src {
            return;
        }
        match (dst, src) {
            (PReg::V(_), PReg::V(_)) => {
                self.push(
                    MOp::VecAlu {
                        op: VecAluOp::Mov,
                        dst: MReg::P(dst),
                        rn: MReg::P(src),
                        rm: None,
                        elem_size: 0,
                    },
                    MInstFlags::NONE,
                    NO_VFLAG,
                    NO_VFLAG,
                    NO_NOTE,
                );
            }
            (PReg::Sp, _) | (_, PReg::Sp) => {
                self.push(
                    MOp::MovSp { dst: MReg::P(dst), src: MReg::P(src) },
                    MInstFlags::NONE,
                    NO_VFLAG,
                    NO_VFLAG,
                    NO_NOTE,
                );
            }
            _ => {
                self.push(
                    MOp::Mov { dst: MReg::P(dst), src: MReg::P(src), wide: true },
                    MInstFlags::NONE,
                    NO_VFLAG,
                    NO_VFLAG,
                    NO_NOTE,
                );
            }
        }
    }

    /// Emits one elementary transfer `dst <- src` between any [`CopyLoc`] pair.
    fn emit_copy_loc(
        &mut self,
        dst: CopyLoc,
        src: CopyLoc,
        gpr_scratch: PReg,
        gpr_addr_scratch: PReg,
        fp_scratch: PReg,
        orig_ops: &[MOp],
        orig_note_idx: &[u32],
    ) -> Result<(), MRaError> {
        if dst == src {
            return Ok(());
        }
        match (dst, src) {
            (CopyLoc::Reg(d), CopyLoc::Reg(s)) => {
                self.emit_reg_copy(d, s);
            }
            (CopyLoc::Reg(d), CopyLoc::Spill { off, size }) => {
                let addr_sc = if matches!(d, PReg::Gpr(_)) { d } else { gpr_addr_scratch };
                self.emit_spill_load(d, off, size, addr_sc)?;
            }
            (CopyLoc::Spill { off, size }, CopyLoc::Reg(s)) => {
                let addr_sc = if s == gpr_addr_scratch { gpr_scratch } else { gpr_addr_scratch };
                self.emit_spill_store(s, off, size, addr_sc)?;
            }
            (CopyLoc::Reg(d), CopyLoc::Remat { def_inst, .. }) => {
                self.emit_remat(d, def_inst, orig_ops, orig_note_idx);
            }
            (CopyLoc::Spill { off: d_off, size: d_sz }, CopyLoc::Spill { off: s_off, size: s_sz }) => {
                let tmp = if d_sz.is_fp_or_vec() { fp_scratch } else { gpr_scratch };
                let addr_sc = if tmp == gpr_scratch { gpr_addr_scratch } else { gpr_scratch };
                self.emit_spill_load(tmp, s_off, s_sz, addr_sc)?;
                self.emit_spill_store(tmp, d_off, d_sz, addr_sc)?;
            }
            (CopyLoc::Spill { off: d_off, size: d_sz }, CopyLoc::Remat { def_inst, is_fp }) => {
                let tmp = if is_fp { fp_scratch } else { gpr_scratch };
                let addr_sc = if tmp == gpr_scratch { gpr_addr_scratch } else { gpr_scratch };
                self.emit_remat(tmp, def_inst, orig_ops, orig_note_idx);
                self.emit_spill_store(tmp, d_off, d_sz, addr_sc)?;
            }
            (CopyLoc::Remat { .. }, _) => {}
        }
        Ok(())
    }
}

/// Resolves an SSA `MReg` in a `ParallelCopy` into its post-RA [`CopyLoc`].
fn resolve_copy_loc(
    reg: MReg,
    alloc_loc: &[AllocLoc],
    vreg_meta: &[VRegMeta],
    inst: MInst,
) -> Result<CopyLoc, MRaError> {
    match reg {
        MReg::P(p) => {
            if matches!(p, PReg::Nzcv) {
                return Err(MRaError::InvalidParallelCopy { inst });
            }
            Ok(CopyLoc::Reg(p))
        }
        MReg::V(v) => {
            let vi = to_idx(v.0);
            let Some(&loc) = alloc_loc.get(vi) else {
                return Err(MRaError::InvalidParallelCopy { inst });
            };
            match loc {
                AllocLoc::Reg(p) => Ok(CopyLoc::Reg(p)),
                AllocLoc::Spill { off, size } => Ok(CopyLoc::Spill { off, size }),
                AllocLoc::Remat { def_inst } => {
                    let is_fp = vreg_meta.get(vi).is_some_and(|m| m.bank == RegBank::Fp);
                    Ok(CopyLoc::Remat { def_inst, is_fp })
                }
                AllocLoc::Unassigned => Err(MRaError::InvalidParallelCopy { inst }),
            }
        }
    }
}

/// Lowers one `MOp::ParallelCopy { start, len }` into sequential copies, dropping self-copies and
/// breaking every permutation cycle via `X16` (or `X17` if `X16` appears in the copy) for GPRs
/// and `V30` (or `V31` if `V30` appears) for FP/NEON — bounded iterative loop, zero recursion.
fn lower_parallel_copy(
    inst: MInst,
    pairs: &[(MReg, MReg)],
    alloc_loc: &[AllocLoc],
    vreg_meta: &[VRegMeta],
    orig_ops: &[MOp],
    orig_note_idx: &[u32],
    out: &mut RewriteOut<'_>,
    scratch: Alloc<'_>,
) -> Result<(), MRaError> {
    let mut pending: Vec<(CopyLoc, CopyLoc), Alloc<'_>> = Vec::with_capacity_in(pairs.len(), scratch);
    let mut mentions_x16 = false;
    let mut mentions_v30 = false;

    for &(dst_reg, src_reg) in pairs {
        let dst = resolve_copy_loc(dst_reg, alloc_loc, vreg_meta, inst)?;
        let src = resolve_copy_loc(src_reg, alloc_loc, vreg_meta, inst)?;
        if dst.is_fp_bank() != src.is_fp_bank() {
            return Err(MRaError::InvalidParallelCopy { inst });
        }
        if matches!(dst, CopyLoc::Remat { .. }) || dst == src {
            continue;
        }
        if dst == CopyLoc::Reg(PReg::X16) || src == CopyLoc::Reg(PReg::X16) {
            mentions_x16 = true;
        }
        if dst == CopyLoc::Reg(PReg::V(30)) || src == CopyLoc::Reg(PReg::V(30)) {
            mentions_v30 = true;
        }
        // Reject duplicate destinations in a parallel copy (undefined simultaneous write).
        for &(existing_dst, _) in pending.iter() {
            if existing_dst == dst {
                return Err(MRaError::InvalidParallelCopy { inst });
            }
        }
        pending.push((dst, src));
    }

    if pending.is_empty() {
        return Ok(());
    }

    let gpr_cycle_reg = if mentions_x16 { PReg::X17 } else { PReg::X16 };
    let gpr_addr_reg = if gpr_cycle_reg == PReg::X16 { PReg::X17 } else { PReg::X16 };
    let fp_cycle_reg = if mentions_v30 { PReg::V(31) } else { PReg::V(30) };

    let max_iters = pending.len().saturating_mul(2).saturating_add(1);
    for _ in 0..max_iters {
        if pending.is_empty() {
            break;
        }

        // Phase A: Find a ready move whose `dst` is NOT read as `src` by any remaining move.
        let mut ready_idx: Option<usize> = None;
        for (i, &(dst, _)) in pending.iter().enumerate() {
            let mut dst_is_read = false;
            for &(_, src) in pending.iter() {
                if src == dst {
                    dst_is_read = true;
                    break;
                }
            }
            if !dst_is_read {
                ready_idx = Some(i);
                break;
            }
        }

        if let Some(idx) = ready_idx {
            let Some(&(dst, src)) = pending.get(idx) else {
                break;
            };
            // Swap-remove without calling `Vec::remove`.
            swap_remove_fast(&mut pending, idx);
            out.emit_copy_loc(
                dst,
                src,
                gpr_cycle_reg,
                gpr_addr_reg,
                fp_cycle_reg,
                orig_ops,
                orig_note_idx,
            )?;
            continue;
        }

        // Phase B: All remaining moves form disjoint cycles. Break the first cycle with scratch.
        let Some(&(d0, _)) = pending.first() else {
            break;
        };
        let cycle_loc = if d0.is_fp_bank() {
            CopyLoc::Reg(fp_cycle_reg)
        } else {
            CopyLoc::Reg(gpr_cycle_reg)
        };
        out.emit_copy_loc(
            cycle_loc,
            d0,
            gpr_cycle_reg,
            gpr_addr_reg,
            fp_cycle_reg,
            orig_ops,
            orig_note_idx,
        )?;
        for pair in pending.iter_mut() {
            if pair.1 == d0 {
                pair.1 = cycle_loc;
            }
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// Single-Instruction Spill/Remat Materializer & Rewriter
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ScratchBinding {
    vreg: u32,
    scratch: PReg,
}

#[inline]
fn find_binding(bindings: &[ScratchBinding], len: usize, v: u32) -> Option<PReg> {
    for b in subslice(bindings, 0, len) {
        if b.vreg == v {
            return Some(b.scratch);
        }
    }
    None
}

/// Rewrites one non-`ParallelCopy` SSA instruction into post-RA physical instructions, inserting
/// pre-reloads / rematerializations and post-spill stores using `X16`/`X17` and `V30`/`V31`.
fn rewrite_single_inst(
    orig_idx: usize,
    op: MOp,
    flags: MInstFlags,
    vdef: u32,
    vuse: u32,
    note_head: u32,
    alloc_loc: &[AllocLoc],
    vreg_meta: &[VRegMeta],
    orig_ops: &[MOp],
    orig_note_idx: &[u32],
    out: &mut RewriteOut<'_>,
) -> Result<(), MRaError> {
    let inst = MInst(u32_of(orig_idx).unwrap_or(NONE_U32));

    // Skip the original defining instruction of any rematerialized `VReg`.
    let mut single_def_v: Option<VReg> = None;
    let mut def_count = 0u32;
    visit_op_defs(&op, &[], |r| {
        def_count += 1;
        if let MReg::V(v) = r {
            single_def_v = Some(v);
        }
    });
    if def_count == 1
        && let Some(v) = single_def_v
        && let Some(&AllocLoc::Remat { def_inst }) = alloc_loc.get(to_idx(v.0))
        && to_idx(def_inst) == orig_idx
    {
        return Ok(());
    }

    // Collect unique spilled / rematerialized USE operands split by register bank.
    let mut gpr_use_vregs = [NONE_U32; 4];
    let mut gpr_use_len = 0usize;
    let mut fp_use_vregs = [NONE_U32; 4];
    let mut fp_use_len = 0usize;

    visit_op_uses(&op, &[], |r| {
        if let MReg::V(v) = r {
            let vi = to_idx(v.0);
            let Some(&loc) = alloc_loc.get(vi) else {
                return;
            };
            if matches!(loc, AllocLoc::Spill { .. } | AllocLoc::Remat { .. }) {
                let is_fp = vreg_meta.get(vi).is_some_and(|m| m.bank == RegBank::Fp);
                if is_fp {
                    let mut dup = false;
                    for &existing in subslice(&fp_use_vregs, 0, fp_use_len) {
                        if existing == v.0 {
                            dup = true;
                            break;
                        }
                    }
                    if !dup && fp_use_len < 4 {
                        slice_set(&mut fp_use_vregs, fp_use_len, v.0);
                        fp_use_len += 1;
                    }
                } else {
                    let mut dup = false;
                    for &existing in subslice(&gpr_use_vregs, 0, gpr_use_len) {
                        if existing == v.0 {
                            dup = true;
                            break;
                        }
                    }
                    if !dup && gpr_use_len < 4 {
                        slice_set(&mut gpr_use_vregs, gpr_use_len, v.0);
                        gpr_use_len += 1;
                    }
                }
            }
        }
    });

    // Special decomposition for 3-distinct-spilled-GPR ops using only `X16` and `X17`!
    if gpr_use_len >= 3 {
        return rewrite_three_spilled_gpr_op(
            inst,
            op,
            flags,
            vdef,
            vuse,
            note_head,
            alloc_loc,
            orig_ops,
            orig_note_idx,
            out,
        );
    }
    if fp_use_len >= 3 {
        return Err(MRaError::SpillScratchExhausted { inst });
    }

    // Bind each spilled/remat USE operand to its reserved scratch register and emit materialization.
    let gpr_scratches = [PReg::X16, PReg::X17];
    let fp_scratches = [PReg::V(30), PReg::V(31)];

    let mut use_binds = [ScratchBinding { vreg: NONE_U32, scratch: PReg::X16 }; 4];
    let mut use_bind_len = 0usize;

    for (&v_id, &sc) in subslice(&gpr_use_vregs, 0, gpr_use_len).iter().zip(gpr_scratches.iter()) {
        slice_set(&mut use_binds, use_bind_len, ScratchBinding { vreg: v_id, scratch: sc });
        use_bind_len += 1;
        materialize_vreg_into(v_id, sc, sc, alloc_loc, orig_ops, orig_note_idx, out)?;
    }
    for (&v_id, &sc) in subslice(&fp_use_vregs, 0, fp_use_len).iter().zip(fp_scratches.iter()) {
        slice_set(&mut use_binds, use_bind_len, ScratchBinding { vreg: v_id, scratch: sc });
        use_bind_len += 1;
        materialize_vreg_into(v_id, sc, PReg::X16, alloc_loc, orig_ops, orig_note_idx, out)?;
    }

    // Reject impossible 3-distinct-spilled-GPR or 3-distinct-spilled-FP def instructions
    // (e.g. writeback `Ldp` where `dst0`, `dst1`, and `base` are all 3 spilled).
    let mut spilled_gpr_defs = 0usize;
    let mut spilled_fp_defs = 0usize;
    visit_op_defs(&op, &[], |r| {
        if let MReg::V(v) = r
            && let Some(&AllocLoc::Spill { size, .. }) = alloc_loc.get(to_idx(v.0))
        {
            if size.is_fp_or_vec() {
                spilled_fp_defs += 1;
            } else {
                spilled_gpr_defs += 1;
            }
        }
    });
    if spilled_gpr_defs >= 3 || spilled_fp_defs >= 3 {
        return Err(MRaError::SpillScratchExhausted { inst });
    }

    // Prepare post-instruction spill stores for any spilled DEF / RMW operands.
    let mut post_stores: [(PReg, u32, MemSize); 3] = [(PReg::X16, 0, MemSize::X64); 3];
    let mut post_store_len = 0usize;
    let mut next_gpr_def_sc = 0usize;
    let mut next_fp_def_sc = 0usize;
    let mut rmw_gpr_sc: Option<PReg> = None;
    let mut rmw_fp_sc: Option<PReg> = None;

    let mut def_binds = [ScratchBinding { vreg: NONE_U32, scratch: PReg::X16 }; 4];
    let mut def_bind_len = 0usize;

    let map_use = |r: MReg| -> MReg {
        match r {
            MReg::P(_) => r,
            MReg::V(v) => {
                let vi = to_idx(v.0);
                match alloc_loc.get(vi).copied().unwrap_or(AllocLoc::Unassigned) {
                    AllocLoc::Reg(p) => MReg::P(p),
                    AllocLoc::Spill { .. } | AllocLoc::Remat { .. } => {
                        let sc = find_binding(&use_binds, use_bind_len, v.0).unwrap_or(PReg::X16);
                        MReg::P(sc)
                    }
                    AllocLoc::Unassigned => MReg::P(PReg::X16),
                }
            }
        }
    };

    let mut map_write = |r: MReg, is_rmw: bool| -> MReg {
        match r {
            MReg::P(_) => r,
            MReg::V(v) => {
                let vi = to_idx(v.0);
                match alloc_loc.get(vi).copied().unwrap_or(AllocLoc::Unassigned) {
                    AllocLoc::Reg(p) => MReg::P(p),
                    AllocLoc::Spill { off, size } => {
                        if is_rmw {
                            let sc = find_binding(&use_binds, use_bind_len, v.0).unwrap_or(
                                if size.is_fp_or_vec() { PReg::V(30) } else { PReg::X16 },
                            );
                            if size.is_fp_or_vec() {
                                rmw_fp_sc = Some(sc);
                            } else {
                                rmw_gpr_sc = Some(sc);
                            }
                            if post_store_len < 3 {
                                slice_set(&mut post_stores, post_store_len, (sc, off, size));
                                post_store_len += 1;
                            }
                            MReg::P(sc)
                        } else {
                            if let Some(sc) = find_binding(&def_binds, def_bind_len, v.0) {
                                return MReg::P(sc);
                            }
                            let sc = if size.is_fp_or_vec() {
                                let mut cand = slice_at(&fp_scratches, next_fp_def_sc.min(1), PReg::V(30));
                                next_fp_def_sc += 1;
                                if rmw_fp_sc == Some(cand) {
                                    cand = if cand == PReg::V(30) { PReg::V(31) } else { PReg::V(30) };
                                }
                                cand
                            } else {
                                let mut cand = slice_at(&gpr_scratches, next_gpr_def_sc.min(1), PReg::X16);
                                next_gpr_def_sc += 1;
                                if rmw_gpr_sc == Some(cand) {
                                    cand = if cand == PReg::X16 { PReg::X17 } else { PReg::X16 };
                                }
                                cand
                            };
                            if def_bind_len < 4 {
                                slice_set(&mut def_binds, def_bind_len, ScratchBinding { vreg: v.0, scratch: sc });
                                def_bind_len += 1;
                            }
                            if post_store_len < 3 {
                                slice_set(&mut post_stores, post_store_len, (sc, off, size));
                                post_store_len += 1;
                            }
                            MReg::P(sc)
                        }
                    }
                    AllocLoc::Remat { .. } | AllocLoc::Unassigned => MReg::P(PReg::X16),
                }
            }
        }
    };

    let rewritten = remap_op_regs(op, map_use, &mut map_write);

    // Elide redundant self-moves (64-bit `mov xD, xD`, `movsp`, FP/Vec `mov`) when unpinned & noteless.
    let is_redundant_self_move = note_head == NO_NOTE
        && !flags.contains(MInstFlags::PINNED)
        && post_store_len == 0
        && match rewritten {
            MOp::Mov { dst, src, wide: true }
            | MOp::MovSp { dst, src }
            | MOp::FpAlu { op: FpAluOp::Mov, dst, rn: src, .. }
            | MOp::VecAlu { op: VecAluOp::Mov, dst, rn: src, .. } => dst == src,
            _ => false,
        };

    if !is_redundant_self_move {
        out.push(rewritten, flags, vdef, vuse, note_head);
    }

    // Emit post-instruction spill stores.
    for &(sc, off, size) in subslice(&post_stores, 0, post_store_len) {
        let addr_sc = if sc == PReg::X16 { PReg::X17 } else { PReg::X16 };
        out.emit_spill_store(sc, off, size, addr_sc)?;
    }

    Ok(())
}

fn materialize_vreg_into(
    v_id: u32,
    dst: PReg,
    addr_scratch: PReg,
    alloc_loc: &[AllocLoc],
    orig_ops: &[MOp],
    orig_note_idx: &[u32],
    out: &mut RewriteOut<'_>,
) -> Result<(), MRaError> {
    match alloc_loc.get(to_idx(v_id)).copied().unwrap_or(AllocLoc::Unassigned) {
        AllocLoc::Spill { off, size } => out.emit_spill_load(dst, off, size, addr_scratch),
        AllocLoc::Remat { def_inst } => {
            out.emit_remat(dst, def_inst, orig_ops, orig_note_idx);
            Ok(())
        }
        AllocLoc::Reg(_) | AllocLoc::Unassigned => Ok(()),
    }
}

/// Decomposes a single instruction with 3 distinct spilled GPR inputs (`Madd`, `Msub`,
/// `Str(ScaledIdx)`, non-writeback `Stp`) into 2-input equivalents using strictly `X16`/`X17`.
fn rewrite_three_spilled_gpr_op(
    inst: MInst,
    op: MOp,
    flags: MInstFlags,
    vdef: u32,
    vuse: u32,
    note_head: u32,
    alloc_loc: &[AllocLoc],
    orig_ops: &[MOp],
    orig_note_idx: &[u32],
    out: &mut RewriteOut<'_>,
) -> Result<(), MRaError> {
    let mat_gpr = |reg: MReg, sc: PReg, out: &mut RewriteOut<'_>| -> Result<PReg, MRaError> {
        match reg {
            MReg::P(p) => Ok(p),
            MReg::V(v) => match alloc_loc.get(to_idx(v.0)).copied().unwrap_or(AllocLoc::Unassigned) {
                AllocLoc::Reg(p) => Ok(p),
                AllocLoc::Spill { off, size } => {
                    out.emit_spill_load(sc, off, size, sc)?;
                    Ok(sc)
                }
                AllocLoc::Remat { def_inst } => {
                    out.emit_remat(sc, def_inst, orig_ops, orig_note_idx);
                    Ok(sc)
                }
                AllocLoc::Unassigned => Err(MRaError::SpillScratchExhausted { inst }),
            },
        }
    };

    match op {
        MOp::Madd { dst, rn, rm, ra, wide } | MOp::Msub { dst, rn, rm, ra, wide } => {
            let is_sub = matches!(op, MOp::Msub { .. });
            let rn_p = mat_gpr(rn, PReg::X16, out)?;
            let rm_p = mat_gpr(rm, PReg::X17, out)?;
            out.push(
                MOp::Mul { dst: MReg::p(PReg::X16), rn: MReg::P(rn_p), rm: MReg::P(rm_p), wide },
                MInstFlags::NONE,
                NO_VFLAG,
                NO_VFLAG,
                NO_NOTE,
            );
            let ra_p = mat_gpr(ra, PReg::X17, out)?;
            let (dst_p, dst_spill) = match dst {
                MReg::P(p) => (p, None),
                MReg::V(v) => match alloc_loc.get(to_idx(v.0)).copied().unwrap_or(AllocLoc::Unassigned) {
                    AllocLoc::Reg(p) => (p, None),
                    AllocLoc::Spill { off, size } => (PReg::X16, Some((off, size))),
                    _ => return Err(MRaError::SpillScratchExhausted { inst }),
                },
            };
            let final_op = if is_sub {
                MOp::SubReg {
                    dst: MReg::P(dst_p),
                    rn: MReg::P(ra_p),
                    rm: MReg::p(PReg::X16),
                    shift: None,
                    wide,
                    sets_flags: false,
                }
            } else {
                MOp::AddReg {
                    dst: MReg::P(dst_p),
                    rn: MReg::p(PReg::X16),
                    rm: MReg::P(ra_p),
                    shift: None,
                    wide,
                    sets_flags: false,
                }
            };
            out.push(final_op, flags, vdef, vuse, note_head);
            if let Some((off, size)) = dst_spill {
                out.emit_spill_store(dst_p, off, size, PReg::X17)?;
            }
            Ok(())
        }
        MOp::Str { src, addr: AddrMode::ScaledIdx { base, idx, shift }, size } => {
            let base_p = mat_gpr(base, PReg::X16, out)?;
            let idx_p = mat_gpr(idx, PReg::X17, out)?;
            let sh = if shift == 0 { None } else { Some(MShift::Lsl(shift)) };
            out.push(
                MOp::AddReg {
                    dst: MReg::p(PReg::X16),
                    rn: MReg::P(base_p),
                    rm: MReg::P(idx_p),
                    shift: sh,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
                NO_VFLAG,
                NO_VFLAG,
                NO_NOTE,
            );
            let src_p = mat_gpr(src, PReg::X17, out)?;
            out.push(
                MOp::Str {
                    src: MReg::P(src_p),
                    addr: AddrMode::BaseOff { base: MReg::p(PReg::X16), off: 0 },
                    size,
                },
                flags,
                vdef,
                vuse,
                note_head,
            );
            Ok(())
        }
        MOp::Stp { src0, src1, base, off, size, post_idx: false, pre_idx: false } => {
            let base_p = mat_gpr(base, PReg::X16, out)?;
            let src0_p = mat_gpr(src0, PReg::X17, out)?;
            let off0 = i32::from(off);
            let elem_bytes = i32_of(size.byte_size()).unwrap_or(8);
            let off1 = off0.saturating_add(elem_bytes);
            out.push(
                MOp::Str {
                    src: MReg::P(src0_p),
                    addr: AddrMode::BaseOff { base: MReg::P(base_p), off: off0 },
                    size,
                },
                flags,
                vdef,
                vuse,
                note_head,
            );
            let src1_p = mat_gpr(src1, PReg::X17, out)?;
            out.push(
                MOp::Str {
                    src: MReg::P(src1_p),
                    addr: AddrMode::BaseOff { base: MReg::P(base_p), off: off1 },
                    size,
                },
                flags,
                NO_VFLAG,
                NO_VFLAG,
                NO_NOTE,
            );
            Ok(())
        }
        _ => Err(MRaError::SpillScratchExhausted { inst }),
    }
}

// -----------------------------------------------------------------------------
// Main Linear-Time SSA Register Allocator Entry Point
// -----------------------------------------------------------------------------

/// Allocates physical AArch64 registers for an SSA [`MFunc`], lowering all [`VReg`]s and
/// [`MOp::ParallelCopy`]s in place, setting `func.is_ssa = false`, and returning exact
/// [`CallClobbers`], callee-saved register masks, stack spill size, and telemetry.
pub fn allocate_mfunc<'a>(
    func: &mut MFunc<'a>,
    opts: MRaOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<MRaOut, MRaError> {
    if !func.is_ssa {
        return Err(MRaError::NotInSsa);
    }
    if (opts.spill_base_off & 15) != 0 || opts.spill_base_off > MAX_SPILL_BYTE_OFF {
        return Err(MRaError::InvalidSpillOffset { off: opts.spill_base_off });
    }
    mverify(func).map_err(MRaError::VerifyFailed)?;

    let n_blocks = func.block_insts.len();
    let n_vregs = func.vreg_class.len();

    // Ensure `call_clobbers` is sorted by `orig_inst` once so every per-call lookup across
    // Phases 1, 2, 3, and 5 is O(log C) instead of O(C). Zero allocation when already sorted.
    let mut call_clobbers_owned: Vec<(u32, CallClobbers), Alloc<'a>> = Vec::new_in(alloc);
    let call_clobbers_sorted: &[(u32, CallClobbers)] =
        if opts.call_clobbers.is_sorted_by_key(|&(idx, _)| idx) {
            opts.call_clobbers
        } else {
            call_clobbers_owned.reserve(opts.call_clobbers.len());
            call_clobbers_owned.extend_from_slice(opts.call_clobbers);
            call_clobbers_owned.sort_unstable_by_key(|&(idx, _)| idx);
            call_clobbers_owned.as_slice()
        };

    // Ensure CSR predecessor/successor tables are populated before dataflow.
    if func.block_succs_off.len() != n_blocks + 1 || func.block_preds_off.len() != n_blocks + 1 {
        func.rebuild_cfg(alloc);
    }

    // -------------------------------------------------------------------------
    // Phase 1: Linearize instructions in `block_order` & inspect calls / SP / FP
    // -------------------------------------------------------------------------
    let mut block_lin_range: Vec<(u32, u32), Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut block_ord_pos: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut block_has_backedge: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    for _ in 0..n_blocks {
        block_lin_range.push((0, 0));
        block_ord_pos.push(NONE_U32);
        block_has_backedge.push(false);
    }

    let mut lin_to_orig: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(func.ops.len(), alloc);
    let mut call_sites: Vec<CallSite, Alloc<'a>> = Vec::new_in(alloc);
    let mut all_call_clobbers: u64 = 0;
    let mut sp_mod_lins: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut has_calls = false;
    let mut explicitly_uses_fp = false;
    let mut explicitly_uses_lr = false;

    for (ord_pos, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        slice_set(&mut block_ord_pos, b, u32_of(ord_pos).unwrap_or(NONE_U32));
        let lin_s = u32_of(lin_to_orig.len()).ok_or(MRaError::FunctionTooLarge)?;
        let (orig_s, orig_e) = slice_at(&func.block_insts, b, (0, 0));
        for orig in to_idx(orig_s)..to_idx(orig_e) {
            let lin_p = u32_of(lin_to_orig.len()).ok_or(MRaError::FunctionTooLarge)?;
            let orig_u32 = u32_of(orig).ok_or(MRaError::FunctionTooLarge)?;
            lin_to_orig.push(orig_u32);

            let Some(op) = func.ops.get(orig) else {
                continue;
            };
            let fl = slice_at(&func.flags, orig, MInstFlags::NONE);
            if is_call_op(op, fl) {
                has_calls = true;
                let clob = lookup_call_clobbers_sorted(call_clobbers_sorted, orig_u32);
                all_call_clobbers |= clob.clobbered.0;
                let def_pt = lin_p
                    .checked_mul(2)
                    .and_then(|x| x.checked_add(1))
                    .ok_or(MRaError::FunctionTooLarge)?;
                call_sites.push(CallSite { def_pt, clobbers: clob });
            }
            if is_sp_write(op) {
                sp_mod_lins.push(lin_p);
            }
            let (pu, pd) = explicit_phys_masks(op, &func.copy_pairs);
            if ((pu | pd) & (1u64 << 29)) != 0 {
                explicitly_uses_fp = true;
            }
            if ((pu | pd) & (1u64 << 30)) != 0 {
                explicitly_uses_lr = true;
            }
        }
        let lin_e = u32_of(lin_to_orig.len()).ok_or(MRaError::FunctionTooLarge)?;
        slice_set(&mut block_lin_range, b, (lin_s, lin_e));
    }

    for &blk in func.block_order.iter() {
        let b = to_idx(blk.0);
        let pos = slice_at(&block_ord_pos, b, NONE_U32);
        if pos == NONE_U32 {
            continue;
        }
        for &succ in func.block_succs(blk) {
            let sp = slice_at(&block_ord_pos, to_idx(succ.0), NONE_U32);
            if sp != NONE_U32 && sp <= pos {
                slice_set(&mut block_has_backedge, b, true);
                break;
            }
        }
    }

    let n_lin_insts = lin_to_orig.len();
    let max_half_pt = n_lin_insts
        .checked_mul(2)
        .and_then(|x| x.checked_add(2))
        .ok_or(MRaError::FunctionTooLarge)?;

    let pools = RegPools::new(opts, has_calls, explicitly_uses_fp, explicitly_uses_lr);

    // -------------------------------------------------------------------------
    // Phase 2: Forward `phys_def_reach` + Bounded Backward Liveness (`VReg` & `PReg`)
    // -------------------------------------------------------------------------
    let mut phys_reach_in: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut phys_reach_out: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut block_phys_gen: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut block_phys_kill: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    for _ in 0..n_blocks {
        phys_reach_in.push(0);
        phys_reach_out.push(0);
        block_phys_gen.push(0);
        block_phys_kill.push(0);
    }

    for &blk in func.block_order.iter() {
        let b = to_idx(blk.0);
        let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
        let mut phys_gen: u64 = 0;
        let mut kill: u64 = 0;
        for p in to_idx(ls)..to_idx(le) {
            let orig_u32 = slice_at(&lin_to_orig, p, 0);
            let orig = to_idx(orig_u32);
            let Some(op) = func.ops.get(orig) else {
                continue;
            };
            let fl = slice_at(&func.flags, orig, MInstFlags::NONE);
            let (_, pdefs) = explicit_phys_masks(op, &func.copy_pairs);
            phys_gen |= pdefs;
            if is_call_op(op, fl) {
                let clob = lookup_call_clobbers_sorted(call_clobbers_sorted, orig_u32).clobbered.0;
                phys_gen &= !clob;
                kill |= clob;
            }
        }
        slice_set(&mut block_phys_gen, b, phys_gen);
        slice_set(&mut block_phys_kill, b, kill);
    }

    // Bounded forward dataflow for `phys_def_reach` (tells `Bl`/`Blr`/`Ret` which arg/ret regs are written).
    let mut reach_converged = false;
    for _ in 0..MAX_DATAFLOW_ITERS {
        let mut changed = false;
        for &blk in func.block_order.iter() {
            let b = to_idx(blk.0);
            let mut in_mask: u64 = 0;
            for &pred in func.block_preds(blk) {
                in_mask |= slice_at(&phys_reach_out, to_idx(pred.0), 0);
            }
            let out_mask =
                slice_at(&block_phys_gen, b, 0) | (in_mask & !slice_at(&block_phys_kill, b, 0));
            if in_mask != slice_at(&phys_reach_in, b, 0)
                || out_mask != slice_at(&phys_reach_out, b, 0)
            {
                slice_set(&mut phys_reach_in, b, in_mask);
                slice_set(&mut phys_reach_out, b, out_mask);
                changed = true;
            }
        }
        if !changed {
            reach_converged = true;
            break;
        }
    }
    if !reach_converged {
        return Err(MRaError::LivenessDidNotConverge);
    }

    // Precompute per-linear-instruction implicit physical uses (at `Bl`/`Blr` and `Ret`).
    let mut inst_impl_phys_uses: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_lin_insts, alloc);
    for _ in 0..n_lin_insts {
        inst_impl_phys_uses.push(0);
    }
    let ret_allowed_mask = (u64::from(func.ret_live_gprs) & 0xff) | (0xffu64 << 32);
    for &blk in func.block_order.iter() {
        let b = to_idx(blk.0);
        let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
        let mut reach = slice_at(&phys_reach_in, b, 0);
        for p in to_idx(ls)..to_idx(le) {
            let orig_u32 = slice_at(&lin_to_orig, p, 0);
            let orig = to_idx(orig_u32);
            let Some(op) = func.ops.get(orig) else {
                continue;
            };
            let fl = slice_at(&func.flags, orig, MInstFlags::NONE);
            if is_call_op(op, fl) {
                slice_set(&mut inst_impl_phys_uses, p, reach & CALL_ARG_PHYS_MASK);
                let clob = lookup_call_clobbers_sorted(call_clobbers_sorted, orig_u32).clobbered.0;
                reach &= !clob;
            } else if matches!(*op, MOp::Ret) {
                slice_set(&mut inst_impl_phys_uses, p, reach & RET_VAL_PHYS_MASK & ret_allowed_mask);
            }
            let (_, pdefs) = explicit_phys_masks(op, &func.copy_pairs);
            reach |= pdefs;
        }
    }

    // Prune liveness to upward-exposed (cross-block) `VReg`s and compute per-block `PReg` use/def
    // masks in one pass. A `VReg` never upward-exposed in any block is strictly block-local and
    // can never appear in `live_in` or `live_out` of any block.
    let mut def_stamp: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    let mut use_stamp: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    let mut vreg_to_global: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for _ in 0..n_vregs {
        def_stamp.push(0);
        use_stamp.push(0);
        vreg_to_global.push(NONE_U32);
    }
    let mut global_to_vreg: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    // Deduplicated `(g, b_u32)` pairs for upward-exposed uses of global `VReg`s.
    let mut upward_uses: Vec<(u32, u32), Alloc<'a>> = Vec::new_in(alloc);

    let mut p_use: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut p_def: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut p_live_in: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut p_live_out: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    for _ in 0..n_blocks {
        p_use.push(0);
        p_def.push(0);
        p_live_in.push(0);
        p_live_out.push(0);
    }

    for (ord_idx, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        let b_u32 = blk.0;
        let stamp = u32_of(ord_idx.saturating_add(1)).ok_or(MRaError::FunctionTooLarge)?;
        let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
        let mut pu_acc: u64 = 0;
        let mut pd_acc: u64 = 0;
        for p in to_idx(ls)..to_idx(le) {
            let orig_u32 = slice_at(&lin_to_orig, p, 0);
            let orig = to_idx(orig_u32);
            let Some(op) = func.ops.get(orig) else {
                continue;
            };
            let fl = slice_at(&func.flags, orig, MInstFlags::NONE);
            visit_op_uses(op, &func.copy_pairs, |r| {
                if let MReg::V(v) = r {
                    let vi = to_idx(v.0);
                    if vi < n_vregs
                        && slice_at(&def_stamp, vi, 0) != stamp
                        && slice_at(&use_stamp, vi, 0) != stamp
                    {
                        slice_set(&mut use_stamp, vi, stamp);
                        let g = match slice_at(&vreg_to_global, vi, NONE_U32) {
                            NONE_U32 => {
                                let ng = u32_of(global_to_vreg.len()).unwrap_or(NONE_U32);
                                slice_set(&mut vreg_to_global, vi, ng);
                                global_to_vreg.push(v.0);
                                ng
                            }
                            existing => existing,
                        };
                        if g != NONE_U32 {
                            upward_uses.push((g, b_u32));
                        }
                    }
                }
            });
            let (op_pu, mut op_pd) = explicit_phys_masks(op, &func.copy_pairs);
            let total_pu = op_pu | slice_at(&inst_impl_phys_uses, p, 0);
            pu_acc |= total_pu & !pd_acc;
            if is_call_op(op, fl) {
                op_pd |= lookup_call_clobbers_sorted(call_clobbers_sorted, orig_u32).clobbered.0;
                op_pd |= 1u64 << 30; // `bl`/`blr` writes `LR` (`X30`).
            }
            pd_acc |= op_pd;
            visit_op_defs(op, &func.copy_pairs, |r| {
                if let MReg::V(v) = r {
                    slice_set(&mut def_stamp, to_idx(v.0), stamp);
                }
            });
        }
        slice_set(&mut p_use, b, pu_acc);
        slice_set(&mut p_def, b, pd_acc);
    }

    // Collect deduplicated `(g, b_u32)` pairs for every block defining a global `VReg`.
    let mut global_defs: Vec<(u32, u32), Alloc<'a>> = Vec::new_in(alloc);
    if !global_to_vreg.is_empty() {
        def_stamp.fill(0);
        for (ord_idx, &blk) in func.block_order.iter().enumerate() {
            let b = to_idx(blk.0);
            let b_u32 = blk.0;
            let stamp = u32_of(ord_idx.saturating_add(1)).ok_or(MRaError::FunctionTooLarge)?;
            let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
            for p in to_idx(ls)..to_idx(le) {
                let orig = to_idx(slice_at(&lin_to_orig, p, 0));
                let Some(op) = func.ops.get(orig) else {
                    continue;
                };
                visit_op_defs(op, &func.copy_pairs, |r| {
                    if let MReg::V(v) = r {
                        let vi = to_idx(v.0);
                        let g = slice_at(&vreg_to_global, vi, NONE_U32);
                        if g != NONE_U32 && slice_at(&def_stamp, vi, 0) != stamp {
                            slice_set(&mut def_stamp, vi, stamp);
                            global_defs.push((g, b_u32));
                        }
                    }
                });
            }
        }
    }

    // Bounded worklist backward `PReg` liveness fixed point (`8 * n_blocks` bytes total, seeded in
    // reverse `block_order` via stack so acyclic switch trees process each block once).
    let mut worklist: Vec<MBlock, Alloc<'a>> = Vec::with_capacity_in(func.block_order.len(), alloc);
    let mut in_worklist: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    for _ in 0..n_blocks {
        in_worklist.push(false);
    }
    for &blk in func.block_order.iter() {
        let b = to_idx(blk.0);
        if b < n_blocks && !slice_at(&in_worklist, b, true) {
            slice_set(&mut in_worklist, b, true);
            worklist.push(blk);
        }
    }
    let max_worklist_steps = func
        .block_order
        .len()
        .saturating_mul(MAX_DATAFLOW_ITERS)
        .saturating_add(1);
    let mut steps = 0usize;
    while let Some(blk) = worklist.pop() {
        if steps >= max_worklist_steps {
            return Err(MRaError::LivenessDidNotConverge);
        }
        steps += 1;
        let b = to_idx(blk.0);
        slice_set(&mut in_worklist, b, false);
        let succs = func.block_succs(blk);

        let mut new_po: u64 = 0;
        for &succ in succs {
            new_po |= slice_at(&p_live_in, to_idx(succ.0), 0);
        }
        let new_pi = slice_at(&p_use, b, 0) | (new_po & !slice_at(&p_def, b, 0));
        if new_po != slice_at(&p_live_out, b, 0) {
            slice_set(&mut p_live_out, b, new_po);
        }
        if new_pi != slice_at(&p_live_in, b, 0) {
            slice_set(&mut p_live_in, b, new_pi);
            for &pred in func.block_preds(blk) {
                let pb = to_idx(pred.0);
                if pb < n_blocks
                    && slice_at(&block_ord_pos, pb, NONE_U32) != NONE_U32
                    && !slice_at(&in_worklist, pb, true)
                {
                    slice_set(&mut in_worklist, pb, true);
                    worklist.push(pred);
                }
            }
        }
    }

    let has_hot_cold_weights = !opts.fast_mode && !opts.block_weights.is_empty() && {
        let mut saw_hot = false;
        let mut saw_cold = false;
        for &blk in func.block_order.iter() {
            let rw = block_raw_weight(opts.block_weights, blk);
            if rw >= HOT_BLOCK_WEIGHT_MIN {
                saw_hot = true;
            }
            if rw <= COLD_EXIT_WEIGHT_MAX {
                saw_cold = true;
            }
        }
        saw_hot && saw_cold
    };
    let mut block_is_hot: Vec<bool, Alloc<'a>> = Vec::new_in(alloc);
    if has_hot_cold_weights {
        block_is_hot.reserve(n_blocks);
        for _ in 0..n_blocks {
            block_is_hot.push(false);
        }
        for &blk in func.block_order.iter() {
            if block_raw_weight(opts.block_weights, blk) >= HOT_BLOCK_WEIGHT_MIN {
                slice_set(&mut block_is_hot, to_idx(blk.0), true);
            }
        }
    }

    // Unified sparse per-global-`VReg` backward CFG BFS (`O(n_blocks)` scratch, zero dense
    // `n_blocks * n_global_vregs` matrices).
    let sparse_liveness = compute_sparse_vreg_liveness_with(
        func,
        &block_lin_range,
        &block_ord_pos,
        &block_is_hot,
        &global_to_vreg,
        &upward_uses,
        &global_defs,
        alloc,
        |_, _, _| {},
    )?;

    // -------------------------------------------------------------------------
    // Phase 3: Build Exact Physical Busy Spans + Wimmer–Franz `VReg` Live Ranges
    // -------------------------------------------------------------------------
    let mut phys_range_heads = [NONE_U32; 64];
    let mut phys_range_nodes: Vec<RangeNode, Alloc<'a>> = Vec::new_in(alloc);

    let mut vreg_meta: Vec<VRegMeta, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    let mut vreg_range_heads: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for &cls in func.vreg_class.iter() {
        vreg_meta.push(VRegMeta::new(cls));
        vreg_range_heads.push(NONE_U32);
    }
    let mut vreg_range_nodes: Vec<RangeNode, Alloc<'a>> = Vec::with_capacity_in(n_vregs * 2, alloc);
    let mut raw_copy_edges: Vec<(u32, u32, u64), Alloc<'a>> = if opts.fast_mode {
        Vec::new_in(alloc)
    } else {
        Vec::with_capacity_in(func.copy_pairs.len().saturating_add(16), alloc)
    };

    // Single reverse pass over `block_order` constructs exact multi-range intervals with holes.
    for &blk in func.block_order.iter().rev() {
        let b = to_idx(blk.0);
        let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
        if ls >= le {
            continue;
        }
        let blk_start_pt = ls.saturating_mul(2);
        let blk_end_pt = (le - 1).saturating_mul(2).saturating_add(1);
        let raw_w = block_raw_weight(opts.block_weights, blk);
        let eff_w = block_eff_weight(opts.block_weights, blk);
        let is_backedge = slice_at(&block_has_backedge, b, false);

        // Seed `[blk_start_pt, blk_end_pt]` for every `VReg` that is both `live_out[b]` AND
        // defined in `b` so `range_list_shorten_def` shortens it accurately. Pass-through
        // `live_out` blocks (no defs of `v`) merge directly during CSR compaction below.
        let dlo_s = to_idx(slice_at(&sparse_liveness.def_lo_off, b, 0));
        let dlo_e = to_idx(slice_at(&sparse_liveness.def_lo_off, b + 1, 0));
        for &v_u32 in subslice(&sparse_liveness.def_lo_vregs, dlo_s, dlo_e) {
            if let Some(head) = vreg_range_heads.get_mut(to_idx(v_u32)) {
                range_list_add(head, &mut vreg_range_nodes, blk_start_pt, blk_end_pt);
            }
        }

        // Track exact physical register occupancy backward through `blk`.
        let mut p_live = slice_at(&p_live_out, b, 0);

        let mut p = to_idx(le);
        while p > to_idx(ls) {
            p -= 1;
            let p_u32 = u32_of(p).unwrap_or(0);
            let orig_u32 = slice_at(&lin_to_orig, p, 0);
            let orig = to_idx(orig_u32);
            let Some(op) = func.ops.get(orig) else {
                continue;
            };
            let fl = slice_at(&func.flags, orig, MInstFlags::NONE);
            let use_pt = p_u32.saturating_mul(2);
            let def_pt = use_pt.saturating_add(1);

            // 1. Physical busy spans at `def_pt` and `use_pt` (excluding call clobbers handled via `call_sites`).
            let (op_pu, op_pd) = explicit_phys_masks(op, &func.copy_pairs);
            let call_clob = if is_call_op(op, fl) {
                lookup_call_clobbers_sorted(call_clobbers_sorted, orig_u32).clobbered.0 | (1u64 << 30)
            } else {
                0
            };
            let busy_def = p_live | op_pd;
            let mut m = busy_def;
            while m != 0 {
                let r = to_idx(m.trailing_zeros());
                m &= m - 1;
                if let Some(head) = phys_range_heads.get_mut(r) {
                    range_list_add(head, &mut phys_range_nodes, def_pt, def_pt);
                }
            }
            p_live = (p_live & !(op_pd | call_clob)) | op_pu | slice_at(&inst_impl_phys_uses, p, 0);
            let mut m2 = p_live;
            while m2 != 0 {
                let r = to_idx(m2.trailing_zeros());
                m2 &= m2 - 1;
                if let Some(head) = phys_range_heads.get_mut(r) {
                    range_list_add(head, &mut phys_range_nodes, use_pt, use_pt);
                }
            }

            // 2. Virtual defs FIRST (backward order: shorten active range to `def_pt`).
            visit_op_defs(op, &func.copy_pairs, |r| {
                if let MReg::V(v) = r {
                    let vi = to_idx(v.0);
                    if let Some(head) = vreg_range_heads.get_mut(vi) {
                        range_list_shorten_def(head, &mut vreg_range_nodes, blk_start_pt, def_pt);
                    }
                    if let Some(meta) = vreg_meta.get_mut(vi) {
                        meta.def_count = meta.def_count.saturating_add(1);
                        meta.def_inst = orig_u32;
                        meta.def_lin = p_u32;
                        meta.def_block = blk.0;
                        meta.weight = meta.weight.saturating_add(eff_w);
                        if raw_w >= HOT_BLOCK_WEIGHT_MIN {
                            meta.def_in_hot = true;
                        }
                    }
                }
            });

            // 3. Virtual uses SECOND (extend `[blk_start_pt, use_pt]`).
            visit_op_uses(op, &func.copy_pairs, |r| {
                if let MReg::V(v) = r {
                    let vi = to_idx(v.0);
                    if let Some(head) = vreg_range_heads.get_mut(vi) {
                        range_list_add(head, &mut vreg_range_nodes, blk_start_pt, use_pt);
                    }
                    if let Some(meta) = vreg_meta.get_mut(vi) {
                        meta.use_count = meta.use_count.saturating_add(1);
                        meta.weight = meta.weight.saturating_add(eff_w);
                        if raw_w >= HOT_BLOCK_WEIGHT_MIN {
                            meta.used_in_hot = true;
                        }
                        if raw_w > COLD_EXIT_WEIGHT_MAX {
                            meta.used_in_non_cold = true;
                        }
                    }
                }
            });

            // 4. Coalescing hints from `Mov`, `FpAlu::Mov`, `VecAlu::Mov`, and `ParallelCopy`.
            if !opts.fast_mode {
                collect_copy_hints(
                    op,
                    &func.copy_pairs,
                    eff_w,
                    is_backedge,
                    &mut vreg_meta,
                    &mut raw_copy_edges,
                );
            }
        }
    }

    // Soft coalescing hint for single-use narrow-masked entry parameters (`raw -> and/ubfm -> dst`)
    // so sub-32-bit entry args (`u8`/`u16`) stay in their incoming ABI register instead of
    // dirtying a scratch register.
    if !opts.fast_mode {
        for (vi, &meta) in vreg_meta.iter().enumerate() {
            if meta.def_count == 1 && meta.def_inst != NONE_U32 && meta.def_block != NONE_U32 {
                let di = to_idx(meta.def_inst);
                if let Some(
                    &MOp::LogicalImm { dst: MReg::V(vd), rn: MReg::V(vs), .. }
                    | &MOp::Ubfm { dst: MReg::V(vd), rn: MReg::V(vs), .. },
                ) = func.ops.get(di)
                    && to_idx(vd.0) == vi
                    && vd != vs
                {
                    let si = to_idx(vs.0);
                    if let Some(&smeta) = vreg_meta.get(si)
                        && smeta.use_count == 1
                        && smeta.fixed_hint_reg != NO_REG
                        && smeta.bank == meta.bank
                    {
                        let w = block_eff_weight(opts.block_weights, MBlock(meta.def_block));
                        raw_copy_edges.push((vd.0, vs.0, w));
                    }
                }
            }
        }
    }

    // Compact `phys_range_heads` into CSR `phys_spans`.
    let mut phys_span_off = [0u32; 65];
    let mut phys_spans: Vec<(u32, u32), Alloc<'a>> = Vec::with_capacity_in(phys_range_nodes.len(), alloc);
    for (r, &head) in phys_range_heads.iter().enumerate() {
        slice_set(&mut phys_span_off, r, u32_of(phys_spans.len()).ok_or(MRaError::FunctionTooLarge)?);
        let mut cur = head;
        let mut steps = 0usize;
        while cur != NONE_U32 && steps <= phys_range_nodes.len() {
            let Some(&node) = phys_range_nodes.get(to_idx(cur)) else {
                break;
            };
            phys_spans.push((node.start, node.end));
            cur = node.next;
            steps += 1;
        }
    }
    slice_set(&mut phys_span_off, 64, u32_of(phys_spans.len()).ok_or(MRaError::FunctionTooLarge)?);

    // Compact `vreg_range_heads` merged with pass-through `live_out` spans into contiguous CSR
    // `vreg_ranges`, and populate `first_start`/`last_end`/`live_len`/`forbidden_phys`, `is_remat`,
    // and `is_cold_split`.
    let mut vreg_ranges: Vec<(u32, u32), Alloc<'a>> = Vec::with_capacity_in(
        vreg_range_nodes.len().saturating_add(sparse_liveness.pass_through_ranges.len()),
        alloc,
    );
    for vi in 0..n_vregs {
        let start_idx = u32_of(vreg_ranges.len()).ok_or(MRaError::FunctionTooLarge)?;
        let pt_slice = match slice_at(&vreg_to_global, vi, NONE_U32) {
            NONE_U32 => &[],
            g => {
                let gi = to_idx(g);
                let ps = to_idx(slice_at(&sparse_liveness.pass_through_off, gi, 0));
                let pe = to_idx(slice_at(&sparse_liveness.pass_through_off, gi + 1, 0));
                subslice(&sparse_liveness.pass_through_ranges, ps, pe)
            }
        };
        let mut cur_a = slice_at(&vreg_range_heads, vi, NONE_U32);
        let mut pt_idx = 0usize;
        let max_steps = vreg_range_nodes.len().saturating_add(pt_slice.len());
        let mut steps = 0usize;
        while (cur_a != NONE_U32 || pt_idx < pt_slice.len()) && steps <= max_steps {
            steps += 1;
            let node_a = if cur_a != NONE_U32 {
                vreg_range_nodes.get(to_idx(cur_a)).copied()
            } else {
                None
            };
            let span_b = pt_slice.get(pt_idx).copied();
            let (s, e) = match (node_a, span_b) {
                (Some(na), Some((bs, be))) => {
                    if na.start <= bs {
                        cur_a = na.next;
                        (na.start, na.end)
                    } else {
                        pt_idx += 1;
                        (bs, be)
                    }
                }
                (Some(na), None) => {
                    cur_a = na.next;
                    (na.start, na.end)
                }
                (None, Some((bs, be))) => {
                    pt_idx += 1;
                    (bs, be)
                }
                (None, None) => break,
            };
            if vreg_ranges.len() > to_idx(start_idx)
                && let Some(last) = vreg_ranges.last_mut()
                && s <= last.1.saturating_add(1)
            {
                if e > last.1 {
                    last.1 = e;
                }
            } else {
                vreg_ranges.push((s, e));
            }
        }
        let end_idx = u32_of(vreg_ranges.len()).ok_or(MRaError::FunctionTooLarge)?;
        let len = end_idx.saturating_sub(start_idx);
        let Some(meta) = vreg_meta.get_mut(vi) else {
            continue;
        };
        meta.range_start_idx = start_idx;
        meta.range_len = len;
        if meta.use_count > 0 && meta.def_count == 0 {
            return Err(MRaError::UndefinedVReg { vreg: VReg(u32_of(vi).unwrap_or(0)) });
        }
        if len == 0 {
            continue;
        }
        if meta.class.is_flags() {
            return Err(MRaError::UnsupportedVRegClass {
                vreg: VReg(u32_of(vi).unwrap_or(0)),
                class: meta.class,
            });
        }
        let slice = subslice(&vreg_ranges, to_idx(start_idx), to_idx(end_idx));
        let first_s = slice.first().map_or(NONE_U32, |r| r.0);
        let last_e = slice.last().map_or(0, |r| r.1);
        let mut live_len: u64 = 0;
        for &(rs, re) in slice {
            live_len = live_len.saturating_add(u64::from(re.saturating_sub(rs)).saturating_add(1));
        }
        meta.first_start = first_s;
        meta.last_end = last_e;
        meta.live_len = live_len.max(1);

        // 1. Exact physical-register interference (only check touched regs in `v`'s bank).
        let mut forbid = RegSet::EMPTY;
        let (r_lo, r_hi) = match meta.bank {
            RegBank::Gpr => (0usize, 31usize),
            RegBank::Fp => (32usize, 64usize),
        };
        for r in r_lo..r_hi {
            let ps = to_idx(slice_at(&phys_span_off, r, 0));
            let pe = to_idx(slice_at(&phys_span_off, r + 1, 0));
            if ps < pe && ranges_overlap(slice, subslice(&phys_spans, ps, pe)) {
                forbid = forbid.with(Reg(u8::try_from(r).unwrap_or(0)));
            }
        }

        // 2. Exact crossed-call `CallClobbers` (binary search over sorted `call_sites`).
        //    Uses `def_pt <= re` because a `VReg` live-out past a trailing call in its block
        //    ends at `blk_end_pt == call.def_pt`, whereas a `Blr` target dying at the call ends
        //    at even `use_pt == call.def_pt - 1`.
        if !call_sites.is_empty() && (forbid.0 & all_call_clobbers) != all_call_clobbers {
            for &(rs, re) in slice {
                if (forbid.0 & all_call_clobbers) == all_call_clobbers {
                    break;
                }
                let mut pos = call_sites.partition_point(|c| c.def_pt <= rs);
                while let Some(&cs) = call_sites.get(pos) {
                    if cs.def_pt > re {
                        break;
                    }
                    forbid = forbid.union(cs.clobbers.clobbered);
                    if (forbid.0 & all_call_clobbers) == all_call_clobbers {
                        break;
                    }
                    pos += 1;
                }
            }
        }
        meta.forbidden_phys = forbid;

        // 3. Rematerializability check.
        if meta.def_count == 1 && meta.def_inst != NONE_U32 {
            let di = to_idx(meta.def_inst);
            let v = VReg(u32_of(vi).unwrap_or(0));
            if let Some(def_op) = func.ops.get(di)
                && is_rematerializable_def(
                    def_op,
                    slice_at(&func.flags, di, MInstFlags::NONE),
                    v,
                    meta.def_lin,
                    last_e,
                    meta.used_in_hot,
                    &sp_mod_lins,
                )
            {
                meta.is_remat = true;
            }
        }
    }

    // 4. Cold-exit live-range splitting detection:
    // Any single-def `VReg` defined outside hot blocks (`raw_weight < 8`), live across >= 1 hot
    // block (`raw_weight >= 8`), with ZERO uses in hot/non-cold blocks and >= 1 uses ONLY in cold
    // exit blocks (`raw_weight <= 1`), is split before the hot loop!
    let mut alloc_loc: Vec<AllocLoc, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for _ in 0..n_vregs {
        alloc_loc.push(AllocLoc::Unassigned);
    }
    let mut spills = SpillSlots::new(opts.spill_base_off);
    let mut stats = MRaStats::default();

    if has_hot_cold_weights {
        for (vi, meta) in vreg_meta.iter_mut().enumerate() {
            if meta.range_len == 0
                || meta.def_count != 1
                || meta.use_count == 0
                || meta.used_in_hot
                || meta.used_in_non_cold
                || meta.def_block == NONE_U32
            {
                continue;
            }
            let def_w = block_raw_weight(opts.block_weights, MBlock(meta.def_block));
            if def_w >= HOT_BLOCK_WEIGHT_MIN {
                continue;
            }
            let g_u32 = slice_at(&vreg_to_global, vi, NONE_U32);
            if g_u32 == NONE_U32 {
                continue;
            }
            let crosses_hot_block =
                slice_at(&sparse_liveness.vreg_crosses_hot, to_idx(g_u32), false);
            if crosses_hot_block {
                meta.is_cold_split = true;
                stats.vregs_cold_split = stats.vregs_cold_split.saturating_add(1);
                if meta.is_remat {
                    slice_set(&mut alloc_loc, vi, AllocLoc::Remat { def_inst: meta.def_inst });
                } else {
                    let size = meta.spill_mem_size();
                    let off = spills.alloc_slot(size)?;
                    slice_set(&mut alloc_loc, vi, AllocLoc::Spill { off, size });
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Phase 4: CSR Coalescing Graph, Multi-Hop Hint Propagation, & Linear Scan
    // -------------------------------------------------------------------------
    if opts.fast_mode {
        allocate_linear_scan_fast(&vreg_meta, max_half_pt, &pools, &mut alloc_loc, &mut spills, alloc)?;
    } else {
    // Build deduplicated, non-overlapping CSR coalescing adjacency list (top `MAX_COPY_EDGES_PER_VREG`
    // partners per `VReg` sorted by descending weight).
    let mut write_e = 0usize;
    for read_e in 0..raw_copy_edges.len() {
        let Some(&(u, v, w)) = raw_copy_edges.get(read_e) else {
            break;
        };
        let ui = to_idx(u);
        let vi = to_idx(v);
        if ui == vi || w == 0 {
            continue;
        }
        let (Some(&um), Some(&vm)) = (vreg_meta.get(ui), vreg_meta.get(vi)) else {
            continue;
        };
        if um.range_len == 0
            || vm.range_len == 0
            || um.is_cold_split
            || vm.is_cold_split
            || um.bank != vm.bank
        {
            continue;
        }
        let ur = meta_ranges(&vreg_ranges, um);
        let vr = meta_ranges(&vreg_ranges, vm);
        if !ranges_overlap(ur, vr) {
            slice_set(&mut raw_copy_edges, write_e, (u, v, w));
            write_e += 1;
        }
    }
    raw_copy_edges.truncate(write_e);

    let mut copy_edge_off: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs + 1, alloc);
    let mut copy_cursor: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    let mut copy_edge_len: Vec<u16, Alloc<'a>> = Vec::with_capacity_in(n_vregs, alloc);
    for _ in 0..n_vregs {
        copy_edge_off.push(0);
        copy_cursor.push(0);
        copy_edge_len.push(0);
    }
    copy_edge_off.push(0);
    for &(u, v, _) in raw_copy_edges.iter() {
        let ui = to_idx(u);
        let vi = to_idx(v);
        if let Some(slot) = copy_edge_off.get_mut(ui) {
            *slot = slot.saturating_add(1);
        }
        if let Some(slot) = copy_edge_off.get_mut(vi) {
            *slot = slot.saturating_add(1);
        }
    }
    let mut dir_edge_total: u32 = 0;
    for vi in 0..n_vregs {
        let deg = slice_at(&copy_edge_off, vi, 0);
        slice_set(&mut copy_edge_off, vi, dir_edge_total);
        slice_set(&mut copy_cursor, vi, dir_edge_total);
        dir_edge_total = dir_edge_total
            .checked_add(deg)
            .ok_or(MRaError::FunctionTooLarge)?;
    }
    slice_set(&mut copy_edge_off, n_vregs, dir_edge_total);

    let mut copy_edges: Vec<(u32, u64), Alloc<'a>> =
        Vec::with_capacity_in(to_idx(dir_edge_total), alloc);
    for _ in 0..to_idx(dir_edge_total) {
        copy_edges.push((NONE_U32, 0));
    }
    for &(u, v, w) in raw_copy_edges.iter() {
        let ui = to_idx(u);
        let vi = to_idx(v);
        let upos = to_idx(slice_at(&copy_cursor, ui, u32::MAX));
        if let Some(slot) = copy_edges.get_mut(upos) {
            *slot = (v, w);
            if let Some(cur) = copy_cursor.get_mut(ui) {
                *cur = cur.saturating_add(1);
            }
        }
        let vpos = to_idx(slice_at(&copy_cursor, vi, u32::MAX));
        if let Some(slot) = copy_edges.get_mut(vpos) {
            *slot = (u, w);
            if let Some(cur) = copy_cursor.get_mut(vi) {
                *cur = cur.saturating_add(1);
            }
        }
    }
    for vi in 0..n_vregs {
        let s = to_idx(slice_at(&copy_edge_off, vi, 0));
        let e = to_idx(slice_at(&copy_edge_off, vi + 1, 0));
        let mut out_len = 0usize;
        for k in s..e {
            let (partner, w) = slice_at(&copy_edges, k, (NONE_U32, 0));
            if partner == NONE_U32 || w == 0 {
                continue;
            }
            let mut found: Option<usize> = None;
            for m in 0..out_len {
                if slice_at(&copy_edges, s + m, (NONE_U32, 0)).0 == partner {
                    found = Some(m);
                    break;
                }
            }
            let mut pos = if let Some(m) = found {
                if let Some(slot) = copy_edges.get_mut(s + m) {
                    slot.1 = slot.1.saturating_add(w);
                }
                m
            } else if out_len < MAX_COPY_EDGES_PER_VREG {
                let p = out_len;
                slice_set(&mut copy_edges, s + p, (partner, w));
                out_len += 1;
                p
            } else {
                let last = out_len - 1;
                let last_e = slice_at(&copy_edges, s + last, (NONE_U32, 0));
                if w > last_e.1 || (w == last_e.1 && partner < last_e.0) {
                    slice_set(&mut copy_edges, s + last, (partner, w));
                    last
                } else {
                    continue;
                }
            };
            while pos > 0 {
                let cur_e = slice_at(&copy_edges, s + pos, (NONE_U32, 0));
                let prev_e = slice_at(&copy_edges, s + pos - 1, (NONE_U32, 0));
                if cur_e.1 > prev_e.1 || (cur_e.1 == prev_e.1 && cur_e.0 < prev_e.0) {
                    slice_set(&mut copy_edges, s + pos, prev_e);
                    slice_set(&mut copy_edges, s + pos - 1, cur_e);
                    pos -= 1;
                } else {
                    break;
                }
            }
        }
        slice_set(&mut copy_edge_len, vi, u16::try_from(out_len).unwrap_or(0));
    }

    // Bounded multi-hop fixed-hint propagation along non-overlapping copy edges (`<= 4` hops)
    // so chains like `x5 -> raw5 -> v5 -> v_phi` and `v_init -> v_phi -> v_ret -> x0` inherit
    // physical-register hints end-to-end without inflating weights.
    for _ in 0..MAX_HINT_PROP_ITERS {
        let mut changed = false;
        for vi in 0..n_vregs {
            let Some(&meta) = vreg_meta.get(vi) else {
                continue;
            };
            if meta.range_len == 0 || meta.is_cold_split {
                continue;
            }
            let es = to_idx(slice_at(&copy_edge_off, vi, 0));
            let elen = usize::from(slice_at(&copy_edge_len, vi, 0));
            let bank_bit_base: u8 = match meta.bank {
                RegBank::Gpr => 0,
                RegBank::Fp => 32,
            };
            for &(partner, edge_w) in subslice(&copy_edges, es, es.saturating_add(elen)) {
                let Some(&pm) = vreg_meta.get(to_idx(partner)) else {
                    continue;
                };
                let cands = [
                    (pm.fixed_hint_reg, pm.fixed_hint_weight),
                    (pm.fixed_hint2_reg, pm.fixed_hint2_weight),
                ];
                for (hw, pw) in cands {
                    if hw == NO_REG || pw == 0 || !pools.is_allocatable(meta.bank, hw) {
                        continue;
                    }
                    let full_bit = bank_bit_base.saturating_add(hw);
                    if meta.forbidden_phys.contains(Reg(full_bit)) {
                        continue;
                    }
                    let prop_w = pw.min(edge_w).max(1);
                    if let Some(vm) = vreg_meta.get_mut(vi)
                        && vm.propagate_fixed_hint(hw, prop_w)
                    {
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    // O(N + V) counting sort of all active (non-cold-split, non-empty) `VReg`s by `first_start`.
    let mut bucket_counts: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(max_half_pt, alloc);
    for _ in 0..max_half_pt {
        bucket_counts.push(0);
    }
    let mut active_vreg_count = 0usize;
    for meta in vreg_meta.iter() {
        if meta.range_len > 0 && !meta.is_cold_split {
            let b = to_idx(meta.first_start).min(max_half_pt - 1);
            if let Some(slot) = bucket_counts.get_mut(b) {
                *slot = slot.saturating_add(1);
            }
            active_vreg_count += 1;
        }
    }
    let mut prefix: u32 = 0;
    for slot in bucket_counts.iter_mut() {
        let c = *slot;
        *slot = prefix;
        prefix = prefix.saturating_add(c);
    }
    let mut sorted_vregs: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(active_vreg_count, alloc);
    for _ in 0..active_vreg_count {
        sorted_vregs.push(0);
    }
    for (vi, meta) in vreg_meta.iter().enumerate() {
        if meta.range_len > 0 && !meta.is_cold_split {
            let b = to_idx(meta.first_start).min(max_half_pt - 1);
            let pos = to_idx(slice_at(&bucket_counts, b, u32::MAX));
            if let Some(slot) = bucket_counts.get_mut(b) {
                *slot = slot.saturating_add(1);
            }
            slice_set(&mut sorted_vregs, pos, u32_of(vi).unwrap_or(0));
        }
    }

    // Within each equal-`first_start` group (e.g. entry `ParallelCopy` definitions), prioritize
    // higher `fixed_hint_weight` (breaking ties by smaller `VReg` id) so hotter fixed-reg hints
    // claim their target physical register first.
    let mut g_start = 0usize;
    while g_start < sorted_vregs.len() {
        let Some(v_first) = sorted_vregs.get(g_start).copied() else {
            break;
        };
        let fs = vreg_meta.get(to_idx(v_first)).map_or(NONE_U32, |m| m.first_start);
        let mut g_end = g_start + 1;
        while let Some(&v_cur) = sorted_vregs.get(g_end) {
            if vreg_meta.get(to_idx(v_cur)).is_some_and(|m| m.first_start == fs) {
                g_end += 1;
            } else {
                break;
            }
        }
        let g_len = g_end - g_start;
        if (2..=64).contains(&g_len) {
            for i in (g_start + 1)..g_end {
                let key = slice_at(&sorted_vregs, i, 0);
                let key_w = vreg_meta.get(to_idx(key)).map_or(0, |m| m.fixed_hint_weight);
                let mut j = i;
                while j > g_start {
                    let prev = slice_at(&sorted_vregs, j - 1, 0);
                    let prev_w = vreg_meta.get(to_idx(prev)).map_or(0, |m| m.fixed_hint_weight);
                    if key_w > prev_w || (key_w == prev_w && key < prev) {
                        slice_set(&mut sorted_vregs, j, prev);
                        j -= 1;
                    } else {
                        break;
                    }
                }
                slice_set(&mut sorted_vregs, j, key);
            }
        }
        g_start = g_end;
    }

    // Build per-physical-register CSR list of active VRegs with a `fixed_hint_reg`, ordered by
    // ascending `first_start` (matching `sorted_vregs`), so unhinted temporaries avoid stealing
    // physical registers wanted by upcoming fixed-`PReg` defs/uses (e.g. `X0..X8` / `V0..V7`).
    let mut fixed_want_counts = [0u32; 64];
    for &v_u32 in sorted_vregs.iter() {
        let Some(&meta) = vreg_meta.get(to_idx(v_u32)) else {
            continue;
        };
        if meta.fixed_hint_reg != NO_REG && meta.fixed_hint_weight > 0 {
            let base: u8 = match meta.bank {
                RegBank::Gpr => 0,
                RegBank::Fp => 32,
            };
            let bit = usize::from(base.saturating_add(meta.fixed_hint_reg));
            if let Some(slot) = fixed_want_counts.get_mut(bit) {
                *slot = slot.saturating_add(1);
            }
        }
    }
    let mut fixed_want_off = [0u32; 65];
    let mut fixed_want_total: u32 = 0;
    for (r, &cnt) in fixed_want_counts.iter().enumerate() {
        slice_set(&mut fixed_want_off, r, fixed_want_total);
        fixed_want_total = fixed_want_total.saturating_add(cnt);
    }
    slice_set(&mut fixed_want_off, 64, fixed_want_total);
    let mut fixed_want_cursor = [0u32; 64];
    for (cur, &off) in fixed_want_cursor.iter_mut().zip(fixed_want_off.iter()) {
        *cur = off;
    }
    let mut fixed_want_vregs: Vec<(u32, u64), Alloc<'a>> =
        Vec::with_capacity_in(to_idx(fixed_want_total), alloc);
    for _ in 0..to_idx(fixed_want_total) {
        fixed_want_vregs.push((NONE_U32, 0));
    }
    for &v_u32 in sorted_vregs.iter() {
        let Some(&meta) = vreg_meta.get(to_idx(v_u32)) else {
            continue;
        };
        if meta.fixed_hint_reg != NO_REG && meta.fixed_hint_weight > 0 {
            let base: u8 = match meta.bank {
                RegBank::Gpr => 0,
                RegBank::Fp => 32,
            };
            let bit = usize::from(base.saturating_add(meta.fixed_hint_reg));
            if let Some(cur) = fixed_want_cursor.get_mut(bit) {
                let pos = to_idx(*cur);
                if let Some(slot) = fixed_want_vregs.get_mut(pos) {
                    *slot = (v_u32, meta.fixed_hint_weight);
                    *cur = cur.saturating_add(1);
                }
            }
        }
    }
    for (cur, &off) in fixed_want_cursor.iter_mut().zip(fixed_want_off.iter()) {
        *cur = off;
    }

    // Track non-expired intervals currently assigned to each physical register (`0..64`),
    // plus dynamic coalescing reservations for unallocated partners so temporaries never steal
    // a loop phi's physical register inside its lifetime hole.
    let mut active_list: Vec<(u32, u8), Alloc<'a>> = Vec::with_capacity_in(64, alloc);
    let mut pending_res: Vec<PendingRes, Alloc<'a>> = Vec::with_capacity_in(32, alloc);
    let mut used_callee_bits: u64 = 0;

    for &v_u32 in sorted_vregs.iter() {
        let vi = to_idx(v_u32);
        let Some(&meta) = vreg_meta.get(vi) else {
            continue;
        };
        let cur_start = meta.first_start;
        let cur_ranges = meta_ranges(&vreg_ranges, meta);

        // Expire finished intervals (swap-with-last, zero `Vec::remove`).
        let mut idx = 0usize;
        while idx < active_list.len() {
            let Some((av, _)) = active_list.get(idx).copied() else {
                break;
            };
            if vreg_meta.get(to_idx(av)).is_some_and(|m| m.last_end < cur_start) {
                swap_remove_fast(&mut active_list, idx);
            } else {
                idx += 1;
            }
        }

        // Expire resolved, evicted, or finished coalescing reservations (swap-with-last).
        let mut ridx = 0usize;
        while ridx < pending_res.len() {
            let Some(r) = pending_res.get(ridx).copied() else {
                break;
            };
            let expected_preg = match r.bank {
                RegBank::Gpr => PReg::Gpr(r.hw),
                RegBank::Fp => PReg::V(r.hw),
            };
            let still_valid = slice_at(&alloc_loc, to_idx(r.vreg), AllocLoc::Unassigned)
                == AllocLoc::Unassigned
                && slice_at(&alloc_loc, to_idx(r.from_vreg), AllocLoc::Unassigned)
                    == AllocLoc::Reg(expected_preg)
                && vreg_meta.get(to_idx(r.vreg)).is_some_and(|m| m.last_end >= cur_start);
            if !still_valid {
                swap_remove_fast(&mut pending_res, ridx);
            } else {
                ridx += 1;
            }
        }

        // Helper closure: checks whether hardware register `hw` in `meta.bank` is legal & free for `vi`.
        let bank_bit_base: u8 = match meta.bank {
            RegBank::Gpr => 0,
            RegBank::Fp => 32,
        };
        let is_reg_free = |hw: u8, active: &[(u32, u8)]| -> bool {
            if !pools.is_allocatable(meta.bank, hw) {
                return false;
            }
            let full_bit = bank_bit_base.saturating_add(hw);
            if meta.forbidden_phys.contains(Reg(full_bit)) {
                return false;
            }
            for &(av, a_hw) in active {
                if a_hw == hw
                    && let Some(&am) = vreg_meta.get(to_idx(av))
                    && am.bank == meta.bank
                {
                    let ar = meta_ranges(&vreg_ranges, am);
                    if ranges_overlap(cur_ranges, ar) {
                        return false;
                    }
                }
            }
            true
        };

        let es = to_idx(slice_at(&copy_edge_off, vi, 0));
        let elen = usize::from(slice_at(&copy_edge_len, vi, 0));
        let v_edges = subslice(&copy_edges, es, es.saturating_add(elen));

        // Cost of choosing `hw` when unallocated coalescing partners of `vi` forbid `hw` (e.g.
        // a back-edge value crossing a call forbids caller-saved regs).
        let partner_forbid_cost = |hw: u8| -> u64 {
            let full_bit = bank_bit_base.saturating_add(hw);
            let mut cost: u64 = 0;
            for &(partner, ew) in v_edges {
                let pi = to_idx(partner);
                if slice_at(&alloc_loc, pi, AllocLoc::Unassigned) == AllocLoc::Unassigned
                    && vreg_meta.get(pi).is_some_and(|m| m.forbidden_phys.contains(Reg(full_bit)))
                {
                    cost = cost.saturating_add(ew);
                }
            }
            cost
        };

        // 1. Try coalescing hints (already-allocated CSR copy partners + top-2 fixed `PReg` hints,
        //    merged by `hw` and sorted by descending weight).
        let mut chosen_hw: Option<u8> = None;
        let mut coalesced = false;

        let mut hint_cands: [(u8, u64); MAX_COPY_EDGES_PER_VREG + 2] =
            [(NO_REG, 0); MAX_COPY_EDGES_PER_VREG + 2];
        let mut hint_len = 0usize;

        let mut add_hint_cand = |hw: u8, w: u64| {
            if hw == NO_REG || w == 0 {
                return;
            }
            if let Some(slots) = hint_cands.get_mut(..hint_len) {
                for slot in slots {
                    if slot.0 == hw {
                        slot.1 = slot.1.saturating_add(w);
                        return;
                    }
                }
            }
            if hint_len < hint_cands.len() {
                slice_set(&mut hint_cands, hint_len, (hw, w));
                hint_len += 1;
            }
        };

        if meta.fixed_hint_reg != NO_REG {
            add_hint_cand(meta.fixed_hint_reg, meta.fixed_hint_weight);
        }
        if meta.fixed_hint2_reg != NO_REG {
            add_hint_cand(meta.fixed_hint2_reg, meta.fixed_hint2_weight);
        }
        for &(partner, ew) in v_edges {
            let phw = match alloc_loc.get(to_idx(partner)).copied() {
                Some(AllocLoc::Reg(PReg::Gpr(r))) if meta.bank == RegBank::Gpr => Some(r),
                Some(AllocLoc::Reg(PReg::V(r))) if meta.bank == RegBank::Fp => Some(r),
                _ => None,
            };
            if let Some(hw) = phw {
                add_hint_cand(hw, ew);
            }
        }

        for i in 1..hint_len {
            let key = slice_at(&hint_cands, i, (NO_REG, 0));
            let mut j = i;
            while j > 0 {
                let prev = slice_at(&hint_cands, j - 1, (NO_REG, 0));
                if key.1 > prev.1 || (key.1 == prev.1 && key.0 < prev.0) {
                    slice_set(&mut hint_cands, j, prev);
                    j -= 1;
                } else {
                    break;
                }
            }
            slice_set(&mut hint_cands, j, key);
        }

        for &(hw, hw_weight) in subslice(&hint_cands, 0, hint_len) {
            if is_reg_free(hw, &active_list) && partner_forbid_cost(hw) < hw_weight {
                chosen_hw = Some(hw);
                coalesced = true;
                break;
            }
        }

        // 2 & 3. Search scratch pool, then callee-saved pool (including `X29`/`X30` in zero-call leaves),
        // picking the free register with minimal steal/conflict penalty (O(1) fast path on 0!).
        if chosen_hw.is_none() {
            let steal_cost_for = |hw: u8, cursors: &mut [u32; 64]| -> u64 {
                let mut cost: u64 = 0;
                for res in pending_res.iter() {
                    if res.hw == hw
                        && res.bank == meta.bank
                        && res.vreg != v_u32
                        && slice_at(&alloc_loc, to_idx(res.vreg), AllocLoc::Unassigned)
                            == AllocLoc::Unassigned
                        && let Some(&rm) = vreg_meta.get(to_idx(res.vreg))
                    {
                        let rr = meta_ranges(&vreg_ranges, rm);
                        if ranges_overlap(cur_ranges, rr) {
                            cost = cost.saturating_add(res.weight);
                        }
                    }
                }
                let full_bit = usize::from(bank_bit_base.saturating_add(hw));
                if full_bit < 64 {
                    let end_u32 = slice_at(&fixed_want_off, full_bit + 1, 0);
                    let end = to_idx(end_u32);
                    let mut c = to_idx(slice_at(cursors, full_bit, end_u32));
                    while c < end {
                        let u = slice_at(&fixed_want_vregs, c, (NONE_U32, 0)).0;
                        if u == NONE_U32
                            || slice_at(&alloc_loc, to_idx(u), AllocLoc::Unassigned)
                                != AllocLoc::Unassigned
                            || vreg_meta.get(to_idx(u)).is_some_and(|m| m.last_end < cur_start)
                        {
                            c += 1;
                        } else {
                            break;
                        }
                    }
                    slice_set(cursors, full_bit, u32_of(c).unwrap_or(end_u32));
                    let scan_lim = end.min(c.saturating_add(MAX_FIXED_WANT_SCAN));
                    for &(u, fw) in subslice(&fixed_want_vregs, c, scan_lim) {
                        if u == NONE_U32
                            || u == v_u32
                            || slice_at(&alloc_loc, to_idx(u), AllocLoc::Unassigned)
                                != AllocLoc::Unassigned
                        {
                            continue;
                        }
                        let Some(&um) = vreg_meta.get(to_idx(u)) else {
                            continue;
                        };
                        if um.first_start > meta.last_end {
                            break;
                        }
                        let ur = meta_ranges(&vreg_ranges, um);
                        if ranges_overlap(cur_ranges, ur) {
                            cost = cost.saturating_add(fw);
                        }
                    }
                }
                cost
            };

            let mut best_hw: Option<u8> = None;
            let mut best_pen = u64::MAX;

            for &hw in pools.scratch_slice(meta.bank) {
                if !is_reg_free(hw, &active_list) {
                    continue;
                }
                let pen = steal_cost_for(hw, &mut fixed_want_cursor)
                    .saturating_add(partner_forbid_cost(hw));
                if pen == 0 {
                    best_hw = Some(hw);
                    best_pen = 0;
                    break;
                }
                if pen < best_pen {
                    best_pen = pen;
                    best_hw = Some(hw);
                }
            }

            if best_pen > 0 {
                for &hw in pools.callee_slice(meta.bank) {
                    if !is_reg_free(hw, &active_list) {
                        continue;
                    }
                    let full_bit = bank_bit_base.saturating_add(hw);
                    let already_dirty =
                        full_bit < 64 && (used_callee_bits & (1u64 << full_bit)) != 0;
                    let callee_pen: u64 = if already_dirty { 1 } else { 4 };
                    let pen = steal_cost_for(hw, &mut fixed_want_cursor)
                        .saturating_add(partner_forbid_cost(hw))
                        .saturating_add(callee_pen);
                    if pen < best_pen {
                        best_pen = pen;
                        best_hw = Some(hw);
                    }
                }
            }

            chosen_hw = best_hw;
        }

        if let Some(hw) = chosen_hw {
            let preg = match meta.bank {
                RegBank::Gpr => PReg::Gpr(hw),
                RegBank::Fp => PReg::V(hw),
            };
            slice_set(&mut alloc_loc, vi, AllocLoc::Reg(preg));
            active_list.push((v_u32, hw));
            let is_callee_hw = match meta.bank {
                RegBank::Gpr => (19..=30).contains(&hw),
                RegBank::Fp => (8..=15).contains(&hw),
            };
            if is_callee_hw {
                let full_bit = bank_bit_base.saturating_add(hw);
                if full_bit < 64 {
                    used_callee_bits |= 1u64 << full_bit;
                }
            }
            for &(partner, ew) in v_edges {
                let pi = to_idx(partner);
                if slice_at(&alloc_loc, pi, AllocLoc::Unassigned) == AllocLoc::Unassigned {
                    let full_bit = bank_bit_base.saturating_add(hw);
                    if pools.is_allocatable(meta.bank, hw)
                        && vreg_meta
                            .get(pi)
                            .is_some_and(|m| !m.forbidden_phys.contains(Reg(full_bit)))
                    {
                        insert_pending_res(
                            &mut pending_res,
                            PendingRes {
                                vreg: partner,
                                from_vreg: v_u32,
                                hw,
                                bank: meta.bank,
                                weight: ew,
                            },
                        );
                    }
                }
            }
            if coalesced {
                stats.vregs_coalesced = stats.vregs_coalesced.saturating_add(1);
            }
            continue;
        }

        // 4. All allocatable registers busy: rematerialize first, then evict non-hot intervals
        //    before any hot-loop interval, then lowest length-normalized loop-weighted spill
        //    density (Belady farthest-end tie-breaker).
        let cur_is_hot = if meta.is_remat {
            meta.used_in_hot
        } else {
            meta.used_in_hot || meta.def_in_hot
        };
        let cur_score = EvictScore {
            non_remat_count: if meta.is_remat { 0 } else { 1 },
            hot_count: if cur_is_hot { 1 } else { 0 },
            eff_weight: meta.spill_density(),
            max_end: meta.last_end,
        };

        let mut best_evict_hw: Option<u8> = None;
        let mut best_evict_score = cur_score;

        // Evaluate every non-forbidden allocatable register in `meta.bank`.
        let mut eval_pool = |pool: &[u8]| {
            for &hw in pool {
                let full_bit = bank_bit_base.saturating_add(hw);
                if meta.forbidden_phys.contains(Reg(full_bit)) {
                    continue;
                }
                let mut spill_w: u64 = 0;
                let mut remat_w: u64 = 0;
                let mut non_remat: u32 = 0;
                let mut hot_non_remat: u32 = 0;
                let mut hot_remat: u32 = 0;
                let mut max_e: u32 = 0;
                let mut overlaps_any = false;
                for &(av, a_hw) in active_list.iter() {
                    if a_hw == hw
                        && let Some(&am) = vreg_meta.get(to_idx(av))
                        && am.bank == meta.bank
                    {
                        let ar = meta_ranges(&vreg_ranges, am);
                        if ranges_overlap(cur_ranges, ar) {
                            overlaps_any = true;
                            let d = am.spill_density();
                            if am.is_remat {
                                remat_w = remat_w.saturating_add(d);
                                if am.used_in_hot {
                                    hot_remat = hot_remat.saturating_add(1);
                                }
                            } else {
                                spill_w = spill_w.saturating_add(d);
                                non_remat = non_remat.saturating_add(1);
                                if am.used_in_hot || am.def_in_hot {
                                    hot_non_remat = hot_non_remat.saturating_add(1);
                                }
                            }
                            if am.last_end > max_e {
                                max_e = am.last_end;
                            }
                        }
                    }
                }
                if overlaps_any {
                    let cand_score = EvictScore {
                        non_remat_count: non_remat,
                        hot_count: if non_remat > 0 { hot_non_remat } else { hot_remat },
                        eff_weight: if non_remat > 0 { spill_w } else { remat_w },
                        max_end: max_e,
                    };
                    if cand_score.is_cheaper_than(best_evict_score) {
                        best_evict_score = cand_score;
                        best_evict_hw = Some(hw);
                    }
                }
            }
        };
        eval_pool(pools.scratch_slice(meta.bank));
        eval_pool(pools.callee_slice(meta.bank));

        if let Some(evict_hw) = best_evict_hw {
            // Evict every overlapping interval on `evict_hw` -> `Remat` or `Spill`.
            let mut j = 0usize;
            while j < active_list.len() {
                let Some((av, a_hw)) = active_list.get(j).copied() else {
                    break;
                };
                if a_hw == evict_hw
                    && let Some(&am) = vreg_meta.get(to_idx(av))
                    && am.bank == meta.bank
                {
                    let ar = meta_ranges(&vreg_ranges, am);
                    if ranges_overlap(cur_ranges, ar) {
                        if am.is_remat {
                            slice_set(
                                &mut alloc_loc,
                                to_idx(av),
                                AllocLoc::Remat { def_inst: am.def_inst },
                            );
                        } else {
                            let sz = am.spill_mem_size();
                            let off = spills.alloc_slot(sz)?;
                            slice_set(&mut alloc_loc, to_idx(av), AllocLoc::Spill { off, size: sz });
                        }
                        swap_remove_fast(&mut active_list, j);
                        continue;
                    }
                }
                j += 1;
            }
            let preg = match meta.bank {
                RegBank::Gpr => PReg::Gpr(evict_hw),
                RegBank::Fp => PReg::V(evict_hw),
            };
            slice_set(&mut alloc_loc, vi, AllocLoc::Reg(preg));
            active_list.push((v_u32, evict_hw));
            let is_callee_hw = match meta.bank {
                RegBank::Gpr => (19..=30).contains(&evict_hw),
                RegBank::Fp => (8..=15).contains(&evict_hw),
            };
            if is_callee_hw {
                let full_bit = bank_bit_base.saturating_add(evict_hw);
                if full_bit < 64 {
                    used_callee_bits |= 1u64 << full_bit;
                }
            }
            for &(partner, ew) in v_edges {
                let pi = to_idx(partner);
                if slice_at(&alloc_loc, pi, AllocLoc::Unassigned) == AllocLoc::Unassigned {
                    let full_bit = bank_bit_base.saturating_add(evict_hw);
                    if pools.is_allocatable(meta.bank, evict_hw)
                        && vreg_meta
                            .get(pi)
                            .is_some_and(|m| !m.forbidden_phys.contains(Reg(full_bit)))
                    {
                        insert_pending_res(
                            &mut pending_res,
                            PendingRes {
                                vreg: partner,
                                from_vreg: v_u32,
                                hw: evict_hw,
                                bank: meta.bank,
                                weight: ew,
                            },
                        );
                    }
                }
            }
        } else if meta.is_remat {
            slice_set(&mut alloc_loc, vi, AllocLoc::Remat { def_inst: meta.def_inst });
        } else {
            let sz = meta.spill_mem_size();
            let off = spills.alloc_slot(sz)?;
            slice_set(&mut alloc_loc, vi, AllocLoc::Spill { off, size: sz });
        }
    }
    }

    // Tally final per-`VReg` allocation stats accurately (after any retroactive evictions).
    for (vi, &loc) in alloc_loc.iter().enumerate() {
        if !vreg_meta.get(vi).is_some_and(|m| m.range_len > 0) {
            continue;
        }
        match loc {
            AllocLoc::Reg(_) => {
                stats.vregs_allocated = stats.vregs_allocated.saturating_add(1);
            }
            AllocLoc::Remat { .. } => {
                stats.vregs_rematerialized = stats.vregs_rematerialized.saturating_add(1);
            }
            AllocLoc::Spill { .. } | AllocLoc::Unassigned => {}
        }
    }
    stats.spill_slots_used = spills.count;

    // -------------------------------------------------------------------------
    // Phase 5: Rewrite Every Block (ParallelCopy Lowering + Spills/Remats)
    // -------------------------------------------------------------------------
    let mut out = RewriteOut::new(
        func.ops.len().saturating_add(to_idx(spills.count).saturating_mul(3)),
        &func.notes,
        opts.spill_base_off,
        spills.cur_rel_off,
        alloc,
    );
    let mut new_block_insts: Vec<(u32, u32), Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    for _ in 0..n_blocks {
        new_block_insts.push((0, 0));
    }

    let mut transitive_call_clobbers = RegSet::EMPTY;

    for &blk in func.block_order.iter() {
        let b = to_idx(blk.0);
        let new_s = u32_of(out.ops.len()).ok_or(MRaError::FunctionTooLarge)?;
        let (orig_s, orig_e) = slice_at(&func.block_insts, b, (0, 0));

        for orig in to_idx(orig_s)..to_idx(orig_e) {
            let Some(&op) = func.ops.get(orig) else {
                continue;
            };
            let fl = slice_at(&func.flags, orig, MInstFlags::NONE);
            let vdef = slice_at(&func.vflag_def, orig, NO_VFLAG);
            let vuse = slice_at(&func.vflag_use, orig, NO_VFLAG);
            let note = slice_at(&func.note_idx, orig, NO_NOTE);

            if is_call_op(&op, fl) {
                let orig_u32 = u32_of(orig).unwrap_or(0);
                let clob = lookup_call_clobbers_sorted(call_clobbers_sorted, orig_u32);
                transitive_call_clobbers = transitive_call_clobbers
                    .union(clob.clobbered)
                    .with(Reg(30));
            }

            if let MOp::ParallelCopy { start, len } = op {
                let ps = to_idx(start);
                let pe = ps.saturating_add(usize::from(len)).min(func.copy_pairs.len());
                let inst = MInst(u32_of(orig).unwrap_or(NONE_U32));
                lower_parallel_copy(
                    inst,
                    subslice(&func.copy_pairs, ps, pe),
                    &alloc_loc,
                    &vreg_meta,
                    &func.ops,
                    &func.note_idx,
                    &mut out,
                    alloc,
                )?;
                stats.parallel_copies_lowered = stats.parallel_copies_lowered.saturating_add(1);
            } else {
                rewrite_single_inst(
                    orig,
                    op,
                    fl,
                    vdef,
                    vuse,
                    note,
                    &alloc_loc,
                    &vreg_meta,
                    &func.ops,
                    &func.note_idx,
                    &mut out,
                )?;
            }
        }

        let new_e = u32_of(out.ops.len()).ok_or(MRaError::FunctionTooLarge)?;
        slice_set(&mut new_block_insts, b, (new_s, new_e));
    }

    // Soundness invariant (Rule 53): every spill slot loaded anywhere in the function MUST have
    // at least one spill store writing it.
    for (slot_idx, (&was_read, &was_written)) in
        out.spill_read.iter().zip(out.spill_written.iter()).enumerate()
    {
        if was_read && !was_written {
            let rel = u32_of(slot_idx).unwrap_or(0).saturating_mul(8);
            return Err(MRaError::UninitializedSpillSlot {
                off: opts.spill_base_off.saturating_add(rel),
            });
        }
    }

    // -------------------------------------------------------------------------
    // Phase 6: Finalize `MFunc` (`is_ssa = false`) & Compute Exact `CallClobbers`
    // -------------------------------------------------------------------------
    func.ops = out.ops;
    func.flags = out.flags;
    func.vflag_def = out.vflag_def;
    func.vflag_use = out.vflag_use;
    func.note_idx = out.note_idx;
    func.notes = out.notes;
    func.block_insts = new_block_insts;
    func.copy_pairs.clear();
    func.is_ssa = false;

    let mut touched_gprs: u32 = 0;
    let mut touched_fps: u32 = 0;
    for &blk in func.block_order.iter() {
        let (s, e) = slice_at(&func.block_insts, to_idx(blk.0), (0, 0));
        for i in to_idx(s)..to_idx(e) {
            let Some(op) = func.ops.get(i) else {
                continue;
            };
            if let MOp::RawWord { defs_gpr, defs_fp, .. } = *op {
                touched_gprs |= defs_gpr;
                touched_fps |= defs_fp;
            } else {
                visit_op_defs(op, &[], |r| {
                    if let MReg::P(p) = r {
                        match p {
                            PReg::Gpr(g) if g <= 30 => touched_gprs |= 1u32 << u32::from(g),
                            PReg::V(v) if v <= 31 => touched_fps |= 1u32 << u32::from(v),
                            PReg::Gpr(_) | PReg::V(_) | PReg::Sp | PReg::Xzr | PReg::Nzcv => {}
                        }
                    }
                });
            }
        }
    }

    let callee_gpr_mask = if opts.allow_x29_leaf && !has_calls {
        CALLEE_SAVED_GPR_LEAF_LR_MASK
    } else {
        CALLEE_SAVED_GPR_MASK
    };
    let used_callee_saved_gprs = touched_gprs & callee_gpr_mask;
    let used_callee_saved_fps = touched_fps & CALLEE_SAVED_FP_MASK;
    let body_clobbers = RegSet(u64::from(touched_gprs) | (u64::from(touched_fps) << 32));
    let clobbers = CallClobbers {
        clobbered: body_clobbers.union(transitive_call_clobbers),
    };
    let spill_bytes = spills.total_bytes_aligned16()?;

    mverify(func).map_err(MRaError::VerifyFailed)?;

    Ok(MRaOut {
        clobbers,
        used_callee_saved_gprs,
        used_callee_saved_fps,
        spill_bytes,
        stats,
    })
}

/// Fast single-pass `O(N + V)` bitmask linear-scan register allocator for [`super::EmitProfile::Fast`].
///
/// Uses convex-hull intervals `[first_start, last_end]` (a sound superset of multi-range liveness)
/// so register availability is a single `u64` bitmask test with zero CSR copy-edge graphs, zero
/// multi-hop hint propagation, zero `fixed_want` lookahead, and zero `pending_res` scans.
fn allocate_linear_scan_fast<'a>(
    vreg_meta: &[VRegMeta],
    max_half_pt: usize,
    pools: &RegPools,
    alloc_loc: &mut [AllocLoc],
    spills: &mut SpillSlots,
    alloc: Alloc<'a>,
) -> Result<(), MRaError> {
    let mut bucket_counts: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(max_half_pt, alloc);
    for _ in 0..max_half_pt {
        bucket_counts.push(0);
    }
    let mut active_vreg_count = 0usize;
    for meta in vreg_meta.iter() {
        if meta.range_len > 0 {
            let b = to_idx(meta.first_start).min(max_half_pt - 1);
            if let Some(slot) = bucket_counts.get_mut(b) {
                *slot = slot.saturating_add(1);
            }
            active_vreg_count += 1;
        }
    }
    let mut prefix: u32 = 0;
    for slot in bucket_counts.iter_mut() {
        let c = *slot;
        *slot = prefix;
        prefix = prefix.saturating_add(c);
    }
    let mut sorted_vregs: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(active_vreg_count, alloc);
    for _ in 0..active_vreg_count {
        sorted_vregs.push(0);
    }
    for (vi, meta) in vreg_meta.iter().enumerate() {
        if meta.range_len > 0 {
            let b = to_idx(meta.first_start).min(max_half_pt - 1);
            let pos = to_idx(slice_at(&bucket_counts, b, u32::MAX));
            if let Some(slot) = bucket_counts.get_mut(b) {
                *slot = slot.saturating_add(1);
            }
            slice_set(&mut sorted_vregs, pos, u32_of(vi).unwrap_or(0));
        }
    }

    let mut active_vreg = [NONE_U32; 64];
    let mut busy_mask: u64 = 0;

    for &v_u32 in sorted_vregs.iter() {
        let vi = to_idx(v_u32);
        let Some(&meta) = vreg_meta.get(vi) else {
            continue;
        };
        let cur_start = meta.first_start;

        // Expire intervals whose convex hull ended strictly before `cur_start`.
        let mut bm = busy_mask;
        while bm != 0 {
            let bit = to_idx(bm.trailing_zeros());
            bm &= bm - 1;
            let av = slice_at(&active_vreg, bit, NONE_U32);
            if av != NONE_U32 && vreg_meta.get(to_idx(av)).is_some_and(|m| m.last_end < cur_start) {
                slice_set(&mut active_vreg, bit, NONE_U32);
                busy_mask &= !(1u64 << bit);
            }
        }

        let bank_bit_base: u8 = match meta.bank {
            RegBank::Gpr => 0,
            RegBank::Fp => 32,
        };
        let unavailable = busy_mask | meta.forbidden_phys.0;

        let mut chosen_hw: Option<u8> = None;
        for &hw in pools.scratch_slice(meta.bank) {
            let full_bit = bank_bit_base.saturating_add(hw);
            if full_bit < 64 && (unavailable & (1u64 << full_bit)) == 0 {
                chosen_hw = Some(hw);
                break;
            }
        }
        if chosen_hw.is_none() {
            for &hw in pools.callee_slice(meta.bank) {
                let full_bit = bank_bit_base.saturating_add(hw);
                if full_bit < 64 && (unavailable & (1u64 << full_bit)) == 0 {
                    chosen_hw = Some(hw);
                    break;
                }
            }
        }

        if let Some(hw) = chosen_hw {
            let full_bit = usize::from(bank_bit_base.saturating_add(hw));
            let preg = match meta.bank {
                RegBank::Gpr => PReg::Gpr(hw),
                RegBank::Fp => PReg::V(hw),
            };
            slice_set(alloc_loc, vi, AllocLoc::Reg(preg));
            slice_set(&mut active_vreg, full_bit, v_u32);
            busy_mask |= 1u64 << full_bit;
            continue;
        }

        // All allocatable non-forbidden registers in this bank are occupied.
        if meta.is_remat {
            slice_set(alloc_loc, vi, AllocLoc::Remat { def_inst: meta.def_inst });
            continue;
        }

        // Poletto–Sarkar farthest-end eviction (preferring rematerializable occupants first).
        let mut best_hw: Option<u8> = None;
        let mut best_is_remat = false;
        let mut best_end = meta.last_end;

        let mut scan_evict = |pool: &[u8]| {
            for &hw in pool {
                let full_bit = bank_bit_base.saturating_add(hw);
                if full_bit >= 64 || meta.forbidden_phys.contains(Reg(full_bit)) {
                    continue;
                }
                let av = slice_at(&active_vreg, usize::from(full_bit), NONE_U32);
                if av == NONE_U32 {
                    continue;
                }
                let Some(&am) = vreg_meta.get(to_idx(av)) else {
                    continue;
                };
                if am.is_remat {
                    if !best_is_remat || am.last_end > best_end {
                        best_is_remat = true;
                        best_end = am.last_end;
                        best_hw = Some(hw);
                    }
                } else if !best_is_remat && am.last_end > best_end {
                    best_end = am.last_end;
                    best_hw = Some(hw);
                }
            }
        };
        scan_evict(pools.scratch_slice(meta.bank));
        scan_evict(pools.callee_slice(meta.bank));

        if let Some(evict_hw) = best_hw {
            let full_bit = usize::from(bank_bit_base.saturating_add(evict_hw));
            let av = slice_at(&active_vreg, full_bit, NONE_U32);
            if let Some(&am) = vreg_meta.get(to_idx(av)) {
                if am.is_remat {
                    slice_set(alloc_loc, to_idx(av), AllocLoc::Remat { def_inst: am.def_inst });
                } else {
                    let sz = am.spill_mem_size();
                    let off = spills.alloc_slot(sz)?;
                    slice_set(alloc_loc, to_idx(av), AllocLoc::Spill { off, size: sz });
                }
            }
            let preg = match meta.bank {
                RegBank::Gpr => PReg::Gpr(evict_hw),
                RegBank::Fp => PReg::V(evict_hw),
            };
            slice_set(alloc_loc, vi, AllocLoc::Reg(preg));
            slice_set(&mut active_vreg, full_bit, v_u32);
            busy_mask |= 1u64 << full_bit;
        } else {
            let sz = meta.spill_mem_size();
            let off = spills.alloc_slot(sz)?;
            slice_set(alloc_loc, vi, AllocLoc::Spill { off, size: sz });
        }
    }

    Ok(())
}

/// Records coalescing edges and physical-register hints from copy-like instructions (`Mov`,
/// `FpAlu::Mov`, `VecAlu::Mov`, and `ParallelCopy`) weighted by block frequency & back-edge priority.
fn collect_copy_hints(
    op: &MOp,
    copy_pairs: &[(MReg, MReg)],
    eff_w: u64,
    is_backedge: bool,
    vreg_meta: &mut [VRegMeta],
    raw_copy_edges: &mut Vec<(u32, u32, u64), Alloc<'_>>,
) {
    let mut record_pair = |dst: MReg, src: MReg, v_w: u64, p_w: u64| match (dst, src) {
        (MReg::V(vd), MReg::V(vs)) if vd != vs => {
            let di = to_idx(vd.0);
            let si = to_idx(vs.0);
            if let (Some(&dm), Some(&sm)) = (vreg_meta.get(di), vreg_meta.get(si))
                && dm.bank == sm.bank
            {
                raw_copy_edges.push((vd.0, vs.0, v_w));
            }
        }
        (MReg::V(v), MReg::P(p)) | (MReg::P(p), MReg::V(v)) => {
            let vi = to_idx(v.0);
            if let Some(meta) = vreg_meta.get_mut(vi) {
                match (meta.bank, p) {
                    (RegBank::Gpr, PReg::Gpr(hw)) | (RegBank::Fp, PReg::V(hw)) => {
                        meta.add_fixed_hint(hw, p_w);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    };

    match *op {
        MOp::Mov { dst, src, .. }
        | MOp::FpAlu { op: FpAluOp::Mov, dst, rn: src, .. }
        | MOp::VecAlu { op: VecAluOp::Mov, dst, rn: src, .. } => {
            let v_w = eff_w.saturating_mul(2);
            let p_w = eff_w.saturating_mul(4);
            record_pair(dst, src, v_w, p_w);
        }
        MOp::ParallelCopy { start, len } => {
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len)).min(copy_pairs.len());
            let pw = eff_w.saturating_mul(if is_backedge { 8 } else { 4 });
            for &(dst, src) in subslice(copy_pairs, s, e) {
                record_pair(dst, src, pw, pw);
            }
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------------
// Unit & Apple Silicon Hardware Execution Tests (NO `Global` / NO `std::`)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::{Alloc, Vec};
    use blaze_tier::{CallClobbers, Os, Reg, RegSet};

    use super::{MRaOpts, allocate_mfunc, mov_imm_inst_cost};
    use crate::sota::mfunc::{
        DEFAULT_RET_LIVE_GPRS, MCond, MFunc, MFuncOpts, MInstFlags, MOp, MReg, MShift, PReg,
        VReg, VRegClass, mverify,
    };
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    use super::MRaOut;
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    use crate::sota::mfunc::{AddrMode, MemSize, NO_VFLAG, mencode};

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn new_ssa(os: Os) -> MFunc<'static> {
        MFunc::new(MFuncOpts {
            os,
            is_ssa: true,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc: test_alloc(),
        })
    }

    /// Wraps a post-RA leaf `MFunc` with a compliant AAPCS64 stack frame saving/restoring every
    /// register in `out.used_callee_saved_gprs` / `out.used_callee_saved_fps` and reserving
    /// `[SP + 0 .. SP + out.spill_bytes]` for spill slots.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn wrap_with_aapcs64_frame(
        func: &MFunc<'static>,
        out: &MRaOut,
        alloc: Alloc<'static>,
    ) -> MFunc<'static> {
        let mut save_gprs: Vec<u8, Alloc<'static>> = Vec::new_in(alloc);
        for r in 19u8..=30u8 {
            if (out.used_callee_saved_gprs & (1u32 << u32::from(r))) != 0 {
                save_gprs.push(r);
            }
        }
        let mut save_fps: Vec<u8, Alloc<'static>> = Vec::new_in(alloc);
        for v in 8u8..=15u8 {
            if (out.used_callee_saved_fps & (1u32 << u32::from(v))) != 0 {
                save_fps.push(v);
            }
        }

        let saved_bytes = u32::try_from(save_gprs.len() + save_fps.len())
            .expect("saved reg count fits u32")
            .saturating_mul(8);
        let raw_frame = out.spill_bytes.saturating_add(saved_bytes);
        let frame_bytes = (raw_frame.saturating_add(15)) & !15u32;

        if frame_bytes == 0 {
            let mut copy = MFunc::new(MFuncOpts {
                os: func.os,
                is_ssa: false,
                ret_live_gprs: func.ret_live_gprs,
                alloc,
            });
            for &align in func.block_align16.iter() {
                copy.new_block(align);
            }
            for &blk in func.block_order.iter() {
                copy.begin_block(blk);
                let range = func.block_inst_range(blk);
                for i in range {
                    let inst = copy.push_inst(func.ops[i], func.flags[i]);
                    if func.vflag_def[i] != NO_VFLAG {
                        copy.set_vflag_def(inst, func.vflag_def[i]);
                    }
                    if func.vflag_use[i] != NO_VFLAG {
                        copy.set_vflag_use(inst, func.vflag_use[i]);
                    }
                }
                copy.end_block(blk);
            }
            copy.rebuild_cfg(alloc);
            return copy;
        }

        let frame_imm12 = u16::try_from(frame_bytes).expect("test frame fits imm12");
        let mut wrapped = MFunc::new(MFuncOpts {
            os: func.os,
            is_ssa: false,
            ret_live_gprs: func.ret_live_gprs,
            alloc,
        });
        for &align in func.block_align16.iter() {
            wrapped.new_block(align);
        }

        for (ord_pos, &blk) in func.block_order.iter().enumerate() {
            wrapped.begin_block(blk);
            if ord_pos == 0 {
                wrapped.push_inst(
                    MOp::SubImm {
                        dst: MReg::SP,
                        rn: MReg::SP,
                        imm12: frame_imm12,
                        shift12: false,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                let mut cur_off = i32::try_from(out.spill_bytes).expect("spill bytes fits i32");
                for &r in save_gprs.iter() {
                    wrapped.push_inst(
                        MOp::Str {
                            src: MReg::gpr(r),
                            addr: AddrMode::BaseOff { base: MReg::SP, off: cur_off },
                            size: MemSize::X64,
                        },
                        MInstFlags::MEM_WRITE,
                    );
                    cur_off += 8;
                }
                for &v in save_fps.iter() {
                    wrapped.push_inst(
                        MOp::Str {
                            src: MReg::vec(v),
                            addr: AddrMode::BaseOff { base: MReg::SP, off: cur_off },
                            size: MemSize::D64,
                        },
                        MInstFlags::MEM_WRITE,
                    );
                    cur_off += 8;
                }
            }

            let range = func.block_inst_range(blk);
            for i in range {
                if matches!(func.ops[i], MOp::Ret) {
                    let mut cur_off = i32::try_from(out.spill_bytes).expect("spill bytes fits i32");
                    for &r in save_gprs.iter() {
                        wrapped.push_inst(
                            MOp::Ldr {
                                dst: MReg::gpr(r),
                                addr: AddrMode::BaseOff { base: MReg::SP, off: cur_off },
                                size: MemSize::X64,
                                signed: false,
                            },
                            MInstFlags::MEM_READ,
                        );
                        cur_off += 8;
                    }
                    for &v in save_fps.iter() {
                        wrapped.push_inst(
                            MOp::Ldr {
                                dst: MReg::vec(v),
                                addr: AddrMode::BaseOff { base: MReg::SP, off: cur_off },
                                size: MemSize::D64,
                                signed: false,
                            },
                            MInstFlags::MEM_READ,
                        );
                        cur_off += 8;
                    }
                    wrapped.push_inst(
                        MOp::AddImm {
                            dst: MReg::SP,
                            rn: MReg::SP,
                            imm12: frame_imm12,
                            shift12: false,
                            wide: true,
                            sets_flags: false,
                        },
                        MInstFlags::NONE,
                    );
                }
                let inst = wrapped.push_inst(func.ops[i], func.flags[i]);
                if func.vflag_def[i] != NO_VFLAG {
                    wrapped.set_vflag_def(inst, func.vflag_def[i]);
                }
                if func.vflag_use[i] != NO_VFLAG {
                    wrapped.set_vflag_use(inst, func.vflag_use[i]);
                }
            }
            wrapped.end_block(blk);
        }
        wrapped.rebuild_cfg(alloc);
        wrapped
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn compile_executable(
        func: &MFunc<'static>,
        out: &MRaOut,
        alloc: Alloc<'static>,
    ) -> blaze_inproc::exec::Executable {
        let wrapped = wrap_with_aapcs64_frame(func, out, alloc);
        assert_eq!(mverify(&wrapped), Ok(()), "framed post-RA MFunc must verify");
        let enc = mencode(&wrapped, alloc).expect("mencode must succeed");
        let mut bytes: Vec<u8, Alloc<'static>> = Vec::with_capacity_in(enc.words.len() * 4, alloc);
        for &w in enc.words.iter() {
            for b in w.to_le_bytes() {
                bytes.push(b);
            }
        }
        blaze_inproc::exec::Executable::new(&bytes, 0).expect("W^X exec mapping must succeed")
    }

    #[test]
    fn mra_allocates_loop_with_coalescing_and_executes_on_apple_silicon() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        let v_n = func.alloc_vreg(VRegClass::Gpr64);
        let v_acc_init = func.alloc_vreg(VRegClass::Gpr64);
        let v_i_init = func.alloc_vreg(VRegClass::Gpr64);
        let v_acc = func.alloc_vreg(VRegClass::Gpr64);
        let v_i = func.alloc_vreg(VRegClass::Gpr64);
        let v_acc_next = func.alloc_vreg(VRegClass::Gpr64);
        let v_i_next = func.alloc_vreg(VRegClass::Gpr64);

        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // Entry: v_n = x0; v_acc = 0; v_i = 1; if v_n == 0 -> b_exit
        func.begin_block(b_entry);
        func.push_inst(
            MOp::Mov { dst: MReg::V(v_n), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_acc_init), imm: 0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_i_init), imm: 1, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_parallel_copy(
            &[
                (MReg::V(v_acc), MReg::V(v_acc_init)),
                (MReg::V(v_i), MReg::V(v_i_init)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbz { rt: MReg::V(v_n), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        // Loop: sum_{i=1..=n} i with SSA back-edge ParallelCopy!
        func.begin_block(b_loop);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_acc_next),
                rn: MReg::V(v_acc),
                rm: MReg::V(v_i),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_i_next),
                rn: MReg::V(v_i),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_parallel_copy(
            &[
                (MReg::V(v_acc), MReg::V(v_acc_next)),
                (MReg::V(v_i), MReg::V(v_i_next)),
            ],
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpReg { rn: MReg::V(v_i), rm: MReg::V(v_n), shift: None, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let br = func.push_inst(
            MOp::BCond { cond: MCond::Ls, target: b_loop },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br, 0);
        func.end_block(b_loop);

        // Exit: return v_acc in x0.
        func.begin_block(b_exit);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_acc), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { block_weights: &[1, 64, 1], ..MRaOpts::default() },
            alloc,
        )
        .expect("mra must succeed on SSA loop");

        assert!(!func.is_ssa);
        assert_eq!(out.spill_bytes, 0);
        assert_eq!(out.stats.spill_slots_used, 0);
        assert_eq!(out.stats.parallel_copies_lowered, 2);
        assert!(out.stats.vregs_coalesced >= 2, "loop back-edge copies must coalesce");
        assert_eq!(mverify(&func), Ok(()));

        // Verify the hot loop body has ZERO `Mov` copies left after coalescing!
        for i in func.block_inst_range(b_loop) {
            assert!(
                !matches!(func.ops[i], MOp::Mov { .. }),
                "loop back-edge ParallelCopy must coalesce with zero Movs in b_loop: {:?}",
                func.ops[i]
            );
        }

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = compile_executable(&func, &out, alloc);
            // SAFETY: Valid AAPCS64 function taking `u64` in `X0` and returning `u64` in `X0`.
            let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
            for &n in &[0u64, 1, 2, 5, 10, 100, 1000] {
                assert_eq!(f(n), n * (n + 1) / 2);
            }
        }
    }

    #[test]
    fn mra_rematerializes_movimm_under_pressure_with_zero_spills() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let b0 = func.new_block(false);

        let mut imm_vregs: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(32, alloc);
        for _ in 0..32 {
            imm_vregs.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        func.begin_block(b0);
        for (k, &v) in imm_vregs.iter().enumerate() {
            let val = u64::try_from(k + 1).expect("k+1 fits u64");
            func.push_inst(
                MOp::MovImm { dst: MReg::V(v), imm: val, wide: true },
                MInstFlags::REMATERIALIZABLE,
            );
        }

        // Sum all 32 live `MovImm` vregs so all 32 are live simultaneously (> 26 allocatable GPRs).
        let mut acc = imm_vregs[0];
        for &v in &imm_vregs[1..] {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(acc),
                    rm: MReg::V(v),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next;
        }
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(acc), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        let out = allocate_mfunc(&mut func, MRaOpts::default(), alloc)
            .expect("mra must rematerialize 32 live MovImms");

        assert!(!func.is_ssa);
        assert!(
            out.stats.vregs_rematerialized > 0,
            "32 live MovImms must trigger rematerialization"
        );
        assert_eq!(out.stats.spill_slots_used, 0, "rematerialization must require zero spill slots");
        assert_eq!(out.spill_bytes, 0);
        assert_eq!(mverify(&func), Ok(()));

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = compile_executable(&func, &out, alloc);
            // SAFETY: Valid AAPCS64 function returning `u64` in `X0` with callee-saved regs preserved.
            let f: extern "C" fn() -> u64 = unsafe { exec.as_fn() };
            assert_eq!(f(), 32 * 33 / 2);
        }
    }

    #[test]
    fn mra_spills_35_live_gprs_and_computes_exact_sum_on_hardware() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let b0 = func.new_block(false);

        let v_base = func.alloc_vreg(VRegClass::Gpr64);
        let mut vals: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(35, alloc);
        for _ in 0..35 {
            vals.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        func.begin_block(b0);
        func.push_inst(
            MOp::Mov { dst: MReg::V(v_base), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        // Define 35 non-rematerializable runtime values: vals[k] = base + (k + 1).
        for k in 0..35 {
            let prev = if k == 0 { v_base } else { vals[k - 1] };
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(vals[k]),
                    rn: MReg::V(prev),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }

        // Sum all 35 live values simultaneously -> forces >= 9 real stack spills & reloads!
        let mut acc = vals[0];
        for &v in &vals[1..] {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(acc),
                    rm: MReg::V(v),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next;
        }
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(acc), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        let out = allocate_mfunc(&mut func, MRaOpts::default(), alloc)
            .expect("mra must spill 35 live GPRs cleanly");

        assert!(!func.is_ssa);
        assert!(out.stats.spill_slots_used >= 9, "35 live GPRs must spill >= 9 slots");
        assert!(out.spill_bytes >= 80 && (out.spill_bytes & 15) == 0);
        assert_ne!(out.used_callee_saved_gprs, 0);
        assert_eq!(mverify(&func), Ok(()));

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = compile_executable(&func, &out, alloc);
            // SAFETY: Valid AAPCS64 function taking `base: u64` and returning `35*base + 630`.
            let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
            for &base in &[0u64, 1, 7, 42, 1000] {
                let expected = 35u64.wrapping_mul(base).wrapping_add(35 * 36 / 2);
                assert_eq!(f(base), expected);
            }
        }
    }

    #[test]
    fn mra_cold_exit_splitting_keeps_hot_loop_spill_free() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_hot_exit = func.new_block(false);
        let b_cold_exit = func.new_block(false);

        let v_n = func.alloc_vreg(VRegClass::Gpr64);
        let v_flag = func.alloc_vreg(VRegClass::Gpr64);

        // 14 cold-exit-only non-rematerializable values (zero uses in `b_loop`, only used in `b_cold_exit`).
        let mut cold_vals: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(14, alloc);
        for _ in 0..14 {
            cold_vals.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        // 18 hot loop values (all live & used inside `b_loop`). Total live across `b_loop` = 14 + 18 + 4 = 36 > 26!
        let mut hot_vals: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(18, alloc);
        for _ in 0..18 {
            hot_vals.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        let v_zero = func.alloc_vreg(VRegClass::Gpr64);
        let v_acc = func.alloc_vreg(VRegClass::Gpr64);
        let v_i = func.alloc_vreg(VRegClass::Gpr64);

        // Entry (weight 2): build 14 cold-exit values & 18 hot loop values.
        func.begin_block(b_entry);
        func.push_inst(
            MOp::Mov { dst: MReg::V(v_n), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Mov { dst: MReg::V(v_flag), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        for (k, &cv) in cold_vals.iter().enumerate() {
            let add_imm = u16::try_from((k + 1) * 10).expect("cold offset fits imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(cv),
                    rn: MReg::V(v_flag),
                    imm12: add_imm,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        for k in 0..18 {
            let prev = if k == 0 { v_n } else { hot_vals[k - 1] };
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(hot_vals[k]),
                    rn: MReg::V(prev),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_zero), imm: 0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(v_zero)), (MReg::V(v_i), MReg::V(v_zero))],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbz { rt: MReg::V(v_n), target: b_hot_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        // Hot loop (weight 64): sums all 18 `hot_vals` into `v_acc` every iteration.
        func.begin_block(b_loop);
        let mut cur_acc = v_acc;
        for &hv in hot_vals.iter() {
            let next_acc = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next_acc),
                    rn: MReg::V(cur_acc),
                    rm: MReg::V(hv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            cur_acc = next_acc;
        }
        let v_i_next = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_i_next),
                rn: MReg::V(v_i),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(cur_acc)), (MReg::V(v_i), MReg::V(v_i_next))],
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpReg { rn: MReg::V(v_i), rm: MReg::V(v_n), shift: None, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let br = func.push_inst(
            MOp::BCond { cond: MCond::Lo, target: b_loop },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br, 0);
        func.end_block(b_loop);

        // Hot exit (weight 2): if `v_flag != 0` branch to `b_cold_exit`, else return `v_acc`.
        func.begin_block(b_hot_exit);
        func.push_inst(
            MOp::Cbnz { rt: MReg::V(v_flag), target: b_cold_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_hot_exit);

        let b_ret_hot = func.new_block(false);
        func.begin_block(b_ret_hot);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_acc), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_ret_hot);

        // Cold exit (weight 1): adds all 14 `cold_vals` to `v_acc` and returns.
        func.begin_block(b_cold_exit);
        let mut cold_acc = v_acc;
        for &cv in cold_vals.iter() {
            let next_cold = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next_cold),
                    rn: MReg::V(cold_acc),
                    rm: MReg::V(cv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            cold_acc = next_cold;
        }
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(cold_acc), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_cold_exit);

        // Block order: [b_entry(0), b_loop(1), b_hot_exit(2), b_ret_hot(4), b_cold_exit(3)]
        // Weights indexed by MBlock id: 0=entry(2), 1=loop(64), 2=hot_exit(2), 3=cold_exit(1), 4=ret_hot(2).
        let weights = [2u32, 64, 2, 1, 2];
        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { block_weights: &weights, ..MRaOpts::default() },
            alloc,
        )
        .expect("mra with cold-exit splitting must succeed");

        assert!(!func.is_ssa);
        assert!(
            out.stats.vregs_cold_split >= 14,
            "all 14 cold-exit-only vregs must be cold-split, got {}",
            out.stats.vregs_cold_split
        );
        assert_eq!(mverify(&func), Ok(()));

        // Prove the hot loop `b_loop` has ZERO spill loads or spill stores!
        for i in func.block_inst_range(b_loop) {
            assert!(
                !matches!(func.ops[i], MOp::Ldr { .. } | MOp::Str { .. }),
                "hot loop must remain 100% spill-free, found {:?}",
                func.ops[i]
            );
        }

        // Verify `b_cold_exit` actually reloads the cold-split values!
        let mut cold_reloads = 0u32;
        for i in func.block_inst_range(b_cold_exit) {
            if matches!(func.ops[i], MOp::Ldr { .. }) {
                cold_reloads += 1;
            }
        }
        assert!(cold_reloads >= 14, "cold exit must reload the split values");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = compile_executable(&func, &out, alloc);
            // SAFETY: Valid AAPCS64 function `(n, take_cold) -> u64`.
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };
            let hot_sum_per_iter = |n: u64| -> u64 {
                // hot_vals[k] = n + (k + 1) for k in 0..18 => sum = 18*n + 18*19/2 = 18*n + 171
                18u64.wrapping_mul(n).wrapping_add(171)
            };
            let cold_bonus = |flag: u64| -> u64 {
                // cold_vals[k] = flag + 10*(k + 1) for k in 0..14 => sum = 14*flag + 10*(14*15/2) = 14*flag + 1050
                14u64.wrapping_mul(flag).wrapping_add(1050)
            };
            for &n in &[0u64, 1, 5, 20] {
                let expected_hot = n.wrapping_mul(hot_sum_per_iter(n));
                assert_eq!(f(n, 0), expected_hot);
                assert_eq!(f(n, 1), expected_hot.wrapping_add(cold_bonus(1)));
                assert_eq!(f(n, 7), expected_hot.wrapping_add(cold_bonus(7)));
            }
        }
    }

    #[test]
    fn mra_parallel_copy_breaks_4_reg_cycle_and_executes_on_hardware() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let b0 = func.new_block(false);

        let v_a = func.alloc_vreg(VRegClass::Gpr64);
        let v_b = func.alloc_vreg(VRegClass::Gpr64);
        let v_c = func.alloc_vreg(VRegClass::Gpr64);
        let v_d = func.alloc_vreg(VRegClass::Gpr64);
        let v_s1 = func.alloc_vreg(VRegClass::Gpr64);
        let v_s2 = func.alloc_vreg(VRegClass::Gpr64);
        let v_s3 = func.alloc_vreg(VRegClass::Gpr64);

        // 4-register permutation cycle: `(x0, x1, x2, x3) <- (x1, x2, x3, x0)` via `ParallelCopy`.
        func.begin_block(b0);
        func.push_parallel_copy(
            &[
                (MReg::p(PReg::X0), MReg::p(PReg::X1)),
                (MReg::p(PReg::X1), MReg::p(PReg::X2)),
                (MReg::p(PReg::X2), MReg::p(PReg::X3)),
                (MReg::p(PReg::X3), MReg::p(PReg::X0)),
            ],
            MInstFlags::NONE,
        );
        // Pack rotated `(x0, x1, x2, x3)` into `x0 = x0 + (x1 << 8) + (x2 << 16) + (x3 << 24)`.
        func.push_inst(MOp::Mov { dst: MReg::V(v_a), src: MReg::p(PReg::X0), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Mov { dst: MReg::V(v_b), src: MReg::p(PReg::X1), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Mov { dst: MReg::V(v_c), src: MReg::p(PReg::X2), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Mov { dst: MReg::V(v_d), src: MReg::p(PReg::X3), wide: true }, MInstFlags::NONE);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_s1),
                rn: MReg::V(v_a),
                rm: MReg::V(v_b),
                shift: Some(MShift::Lsl(8)),
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_s2),
                rn: MReg::V(v_s1),
                rm: MReg::V(v_c),
                shift: Some(MShift::Lsl(16)),
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_s3),
                rn: MReg::V(v_s2),
                rm: MReg::V(v_d),
                shift: Some(MShift::Lsl(24)),
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_s3), wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        let out = allocate_mfunc(&mut func, MRaOpts::default(), alloc)
            .expect("mra must break 4-reg ParallelCopy cycle");

        assert!(!func.is_ssa);
        assert_eq!(out.stats.parallel_copies_lowered, 1);
        assert_eq!(out.spill_bytes, 0);
        assert_eq!(mverify(&func), Ok(()));
        // Cycle breaking uses `X16`, which must appear in the exact `CallClobbers` summary!
        assert!(out.clobbers.clobbers_gpr(16));

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = compile_executable(&func, &out, alloc);
            // SAFETY: Valid AAPCS64 frameless leaf taking 4 `u64` args and returning packed `u64`.
            let f: extern "C" fn(u64, u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
            // `(a, b, c, d)` rotated -> `(b, c, d, a)` -> `b + (c << 8) + (d << 16) + (a << 24)`.
            assert_eq!(f(1, 2, 3, 4), 2 + (3 << 8) + (4 << 16) + (1 << 24));
            assert_eq!(
                f(0x11, 0x22, 0x33, 0x44),
                0x22 + (0x33 << 8) + (0x44 << 16) + (0x11 << 24)
            );
        }
    }

    #[test]
    fn mra_respects_ipra_call_clobbers_x29_leaf_and_fp_parallel_copy_cycle() {
        let alloc = test_alloc();

        // 1. IPRA custom `call_clobbers`: callee only clobbers `X0`, preserving `X9..X15`!
        //    A live vreg crossing `Bl` must stay in an IPRA-preserved scratch GPR (`used_callee_saved_gprs == 0`).
        let mut f_ipra = new_ssa(Os::MacOs);
        let b0 = f_ipra.new_block(false);
        let v_live = f_ipra.alloc_vreg(VRegClass::Gpr64);
        let v_sum = f_ipra.alloc_vreg(VRegClass::Gpr64);
        f_ipra.begin_block(b0);
        f_ipra.push_inst(MOp::Mov { dst: MReg::V(v_live), src: MReg::p(PReg::X0), wide: true }, MInstFlags::NONE);
        let bl_inst = f_ipra.push_inst(MOp::Bl, MInstFlags::CALL);
        f_ipra.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_sum),
                rn: MReg::V(v_live),
                rm: MReg::p(PReg::X0),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        f_ipra.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_sum), wide: true }, MInstFlags::NONE);
        f_ipra.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f_ipra.end_block(b0);

        let custom_clob = CallClobbers { clobbered: RegSet::EMPTY.with(Reg(0)) };
        let clob_table = [(bl_inst.0, custom_clob)];
        let out_ipra = allocate_mfunc(
            &mut f_ipra,
            MRaOpts { call_clobbers: &clob_table, ..MRaOpts::default() },
            alloc,
        )
        .expect("IPRA allocation must succeed");
        assert_eq!(out_ipra.used_callee_saved_gprs, 0, "IPRA-preserved scratch X9 must avoid callee-saved regs");
        assert!(out_ipra.clobbers.clobbers_gpr(0));
        assert!(out_ipra.clobbers.clobbers_gpr(9));
        assert!(out_ipra.clobbers.clobbers_gpr(30));
        assert_eq!(mverify(&f_ipra), Ok(()));

        // 2. `allow_x29_leaf`: 27 simultaneously live non-remat GPRs fit in 0 spills with X29 enabled!
        let mut f_leaf = new_ssa(Os::MacOs);
        let lb0 = f_leaf.new_block(false);
        let v_in = f_leaf.alloc_vreg(VRegClass::Gpr64);
        let mut gprs: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(27, alloc);
        for _ in 0..27 {
            gprs.push(f_leaf.alloc_vreg(VRegClass::Gpr64));
        }
        f_leaf.begin_block(lb0);
        f_leaf.push_inst(MOp::Mov { dst: MReg::V(v_in), src: MReg::p(PReg::X0), wide: true }, MInstFlags::NONE);
        for k in 0..27 {
            let prev = if k == 0 { v_in } else { gprs[k - 1] };
            f_leaf.push_inst(
                MOp::AddImm {
                    dst: MReg::V(gprs[k]),
                    rn: MReg::V(prev),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        let mut acc = gprs[0];
        for &v in &gprs[1..] {
            let next = f_leaf.alloc_vreg(VRegClass::Gpr64);
            f_leaf.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(acc),
                    rm: MReg::V(v),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next;
        }
        f_leaf.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(acc), wide: true }, MInstFlags::NONE);
        f_leaf.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f_leaf.end_block(lb0);

        let out_leaf = allocate_mfunc(
            &mut f_leaf,
            MRaOpts { allow_x29_leaf: true, ..MRaOpts::default() },
            alloc,
        )
        .expect("27-GPR leaf with allow_x29_leaf must succeed");
        assert_eq!(out_leaf.stats.spill_slots_used, 0, "X29 as 27th GPR must keep 27 live GPRs spill-free");
        assert_ne!(out_leaf.used_callee_saved_gprs & (1u32 << 29), 0, "X29 must be reported in used_callee_saved_gprs");
        assert_eq!(mverify(&f_leaf), Ok(()));

        // 3. FP/NEON 3-register `ParallelCopy` cycle breaks cleanly using `V30`!
        let mut f_fp = new_ssa(Os::MacOs);
        let fb0 = f_fp.new_block(false);
        f_fp.begin_block(fb0);
        f_fp.push_parallel_copy(
            &[
                (MReg::vec(0), MReg::vec(1)),
                (MReg::vec(1), MReg::vec(2)),
                (MReg::vec(2), MReg::vec(0)),
            ],
            MInstFlags::NONE,
        );
        f_fp.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f_fp.end_block(fb0);

        let out_fp = allocate_mfunc(&mut f_fp, MRaOpts::default(), alloc)
            .expect("FP 3-reg ParallelCopy cycle must lower cleanly");
        assert_eq!(out_fp.stats.parallel_copies_lowered, 1);
        assert!(out_fp.clobbers.clobbers_fp(30), "FP cycle breaker V30 must appear in clobbers");
        assert_eq!(mverify(&f_fp), Ok(()));
    }

    #[test]
    fn mra_entry_param_parallel_copy_hints_zero_prologue_shuffles() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let b0 = func.new_block(false);

        // 8 entry parameters: v0..v4 (64-bit) + raw5..raw7 -> v5..v7 (32-bit zero-extended),
        // matching `misel.rs` entry lowering for leaf functions like `fse_pack_sequences_leaf`.
        let v0 = func.alloc_vreg(VRegClass::Gpr64);
        let v1 = func.alloc_vreg(VRegClass::Gpr64);
        let v2 = func.alloc_vreg(VRegClass::Gpr64);
        let v3 = func.alloc_vreg(VRegClass::Gpr64);
        let v4 = func.alloc_vreg(VRegClass::Gpr64);
        let raw5 = func.alloc_vreg(VRegClass::Gpr32);
        let raw6 = func.alloc_vreg(VRegClass::Gpr32);
        let raw7 = func.alloc_vreg(VRegClass::Gpr32);
        let v5 = func.alloc_vreg(VRegClass::Gpr32);
        let v6 = func.alloc_vreg(VRegClass::Gpr32);
        let v7 = func.alloc_vreg(VRegClass::Gpr32);

        func.begin_block(b0);
        func.push_parallel_copy(
            &[
                (MReg::V(v0), MReg::p(PReg::X0)),
                (MReg::V(v1), MReg::p(PReg::X1)),
                (MReg::V(v2), MReg::p(PReg::X2)),
                (MReg::V(v3), MReg::p(PReg::X3)),
                (MReg::V(v4), MReg::p(PReg::X4)),
                (MReg::V(raw5), MReg::p(PReg::X5)),
                (MReg::V(raw6), MReg::p(PReg::X6)),
                (MReg::V(raw7), MReg::p(PReg::X7)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Mov { dst: MReg::V(v5), src: MReg::V(raw5), wide: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Mov { dst: MReg::V(v6), src: MReg::V(raw6), wide: false },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Mov { dst: MReg::V(v7), src: MReg::V(raw7), wide: false },
            MInstFlags::NONE,
        );
        // Exercise `MOp::Prfm` reading an allocated entry pointer `v0`.
        func.push_inst(MOp::Prfm { base: MReg::V(v0), imm12: 8 }, MInstFlags::NONE);

        let params = [v0, v1, v2, v3, v4, v5, v6, v7];
        let mut acc = params[0];
        for &p in &params[1..] {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(acc),
                    rm: MReg::V(p),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next;
        }
        func.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::V(acc))], MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(&mut func, MRaOpts::default(), alloc)
            .expect("8-param leaf with ParallelCopy + Prfm must allocate");

        assert!(!func.is_ssa);
        assert_eq!(out.stats.spill_slots_used, 0);
        assert_eq!(out.spill_bytes, 0);
        assert_eq!(mverify(&func), Ok(()));

        let mut saw_prfm_x0 = false;
        for i in func.block_inst_range(b0) {
            match func.ops[i] {
                MOp::Mov { dst, src, wide: true } => {
                    panic!("unexpected 64-bit prologue/epilogue Mov shuffle: {dst:?} <- {src:?}");
                }
                MOp::Mov { dst, src, wide: false } => {
                    assert_eq!(
                        dst, src,
                        "32-bit param Mov must coalesce in place (no w13 <- w5 shuffle): {dst:?} <- {src:?}"
                    );
                }
                MOp::Prfm { base, imm12: 8 } => {
                    assert_eq!(base, MReg::p(PReg::X0), "v0 entry param must stay in X0 for Prfm");
                    saw_prfm_x0 = true;
                }
                _ => {}
            }
        }
        assert!(saw_prfm_x0, "Prfm must survive register allocation with X0 base");
    }

    #[test]
    fn mra_deferred_loop_backedge_parallel_copy_coalesces_across_lifetime_hole() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        // Mirrors `misel.rs` loop lowering where the back-edge `ParallelCopy` lives in a deferred
        // split block (`b_latch`) emitted at the tail of `block_order`, creating a lifetime hole
        // between phi uses early in `b_loop` and phi redefs in `b_latch`.
        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);
        let b_latch = func.new_block(false);

        let v_n = func.alloc_vreg(VRegClass::Gpr64);
        let v_a0 = func.alloc_vreg(VRegClass::Gpr64);
        let v_b0 = func.alloc_vreg(VRegClass::Gpr64);
        let v_zero = func.alloc_vreg(VRegClass::Gpr64);
        let v_phi_a = func.alloc_vreg(VRegClass::Gpr64);
        let v_phi_b = func.alloc_vreg(VRegClass::Gpr64);
        let v_i = func.alloc_vreg(VRegClass::Gpr64);
        let v_t0 = func.alloc_vreg(VRegClass::Gpr64);
        let v_t1 = func.alloc_vreg(VRegClass::Gpr64);
        let v_t2 = func.alloc_vreg(VRegClass::Gpr64);
        let v_t3 = func.alloc_vreg(VRegClass::Gpr64);
        let v_next_a = func.alloc_vreg(VRegClass::Gpr64);
        let v_next_b = func.alloc_vreg(VRegClass::Gpr64);
        let v_i_next = func.alloc_vreg(VRegClass::Gpr64);

        func.begin_block(b_entry);
        func.push_parallel_copy(
            &[
                (MReg::V(v_n), MReg::p(PReg::X0)),
                (MReg::V(v_a0), MReg::p(PReg::X1)),
                (MReg::V(v_b0), MReg::p(PReg::X2)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_zero), imm: 0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_parallel_copy(
            &[
                (MReg::V(v_phi_a), MReg::V(v_a0)),
                (MReg::V(v_phi_b), MReg::V(v_b0)),
                (MReg::V(v_i), MReg::V(v_zero)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        func.end_block(b_entry);

        // `b_loop`: uses `v_phi_a` and `v_phi_b` early, then allocates 4 temporaries overlapping
        // the hole before `v_next_a` / `v_next_b` are defined!
        func.begin_block(b_loop);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_t0),
                rn: MReg::V(v_phi_a),
                imm12: 3,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_t1),
                rn: MReg::V(v_phi_b),
                imm12: 5,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_t2),
                rn: MReg::V(v_t0),
                rm: MReg::V(v_t1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_t3),
                rn: MReg::V(v_t2),
                imm12: 7,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_next_a),
                rn: MReg::V(v_t3),
                rm: MReg::V(v_t0),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(v_next_b),
                rn: MReg::V(v_t2),
                rm: MReg::V(v_t1),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_i_next),
                rn: MReg::V(v_i),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpReg { rn: MReg::V(v_i_next), rm: MReg::V(v_n), shift: None, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let br = func.push_inst(
            MOp::BCond { cond: MCond::Lo, target: b_latch },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br, 0);
        func.push_inst(MOp::B { target: b_exit }, MInstFlags::TERMINATOR);
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::V(v_next_a))], MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.begin_block(b_latch);
        func.push_parallel_copy(
            &[
                (MReg::V(v_phi_a), MReg::V(v_next_a)),
                (MReg::V(v_phi_b), MReg::V(v_next_b)),
                (MReg::V(v_i), MReg::V(v_i_next)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        func.end_block(b_latch);

        // Weights: entry=2, loop=64, exit=1, latch=64.
        let weights = [2u32, 64, 1, 64];
        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { block_weights: &weights, ..MRaOpts::default() },
            alloc,
        )
        .expect("deferred-latch loop must allocate");

        assert!(!func.is_ssa);
        assert_eq!(out.stats.spill_slots_used, 0);
        assert_eq!(mverify(&func), Ok(()));

        // Prove ZERO `Mov`s remain in both `b_loop` and `b_latch`!
        for i in func.block_inst_range(b_loop) {
            assert!(
                !matches!(func.ops[i], MOp::Mov { .. }),
                "loop body must contain zero Movs, found {:?}",
                func.ops[i]
            );
        }
        let latch_range = func.block_inst_range(b_latch);
        assert_eq!(
            latch_range.len(),
            1,
            "deferred loop latch must coalesce all 3 phis leaving only `B b_loop`"
        );
        assert!(matches!(func.ops[latch_range.start], MOp::B { target } if target == b_loop));
    }

    #[test]
    fn mra_high_pressure_27_live_gpr_leaf_uses_x0_x15_x19_x29_zero_spills() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);
        let b0 = func.new_block(false);

        // 8 entry params + 19 derived non-remat GPRs = 27 live non-remat GPRs simultaneously,
        // plus 4 rematerializable `MovImm` constants (31 total live VRegs!).
        let mut live: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(27, alloc);
        for _ in 0..27 {
            live.push(func.alloc_vreg(VRegClass::Gpr64));
        }
        let mut remats: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(4, alloc);
        for _ in 0..4 {
            remats.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        func.begin_block(b0);
        func.push_parallel_copy(
            &[
                (MReg::V(live[0]), MReg::p(PReg::X0)),
                (MReg::V(live[1]), MReg::p(PReg::X1)),
                (MReg::V(live[2]), MReg::p(PReg::X2)),
                (MReg::V(live[3]), MReg::p(PReg::X3)),
                (MReg::V(live[4]), MReg::p(PReg::X4)),
                (MReg::V(live[5]), MReg::p(PReg::X5)),
                (MReg::V(live[6]), MReg::p(PReg::X6)),
                (MReg::V(live[7]), MReg::p(PReg::X7)),
            ],
            MInstFlags::NONE,
        );
        for (i, &rv) in remats.iter().enumerate() {
            let imm = u64::try_from(100 + i).expect("imm fits u64");
            func.push_inst(
                MOp::MovImm { dst: MReg::V(rv), imm, wide: true },
                MInstFlags::REMATERIALIZABLE,
            );
        }
        for k in 8..27 {
            let src = live[k - 8];
            let imm12 = u16::try_from(k).expect("k fits imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(live[k]),
                    rn: MReg::V(src),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        let mut acc = live[0];
        for &v in &live[1..] {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(acc),
                    rm: MReg::V(v),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next;
        }
        for &rv in remats.iter() {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(acc),
                    rm: MReg::V(rv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            acc = next;
        }
        func.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::V(acc))], MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b0);

        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { allow_x29_leaf: true, ..MRaOpts::default() },
            alloc,
        )
        .expect("27 non-remat + 4 remat GPR leaf must allocate with zero spills");

        assert!(!func.is_ssa);
        assert_eq!(out.stats.spill_slots_used, 0, "all 27 GPRs + remat must need ZERO stack spills");
        assert_eq!(out.spill_bytes, 0);
        assert!(
            out.stats.vregs_rematerialized >= 3,
            "excess MovImms over 28 GPRs must rematerialize before spilling"
        );
        assert_ne!(out.used_callee_saved_gprs & (1u32 << 29), 0, "X29 must be used as 27th GPR");
        assert_ne!(out.used_callee_saved_gprs & (1u32 << 30), 0, "X30 must be used as 28th GPR");
        assert!(!out.clobbers.clobbers_gpr(18), "X18 must remain untouched on macOS");
        assert_eq!(mverify(&func), Ok(()));
    }

    #[test]
    fn mra_high_pressure_loop_evicts_long_low_use_over_short_hot_and_spills_prfm_cleanly() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        let v_n = func.alloc_vreg(VRegClass::Gpr64);
        let v_base = func.alloc_vreg(VRegClass::Gpr64);
        let v_zero = func.alloc_vreg(VRegClass::Gpr64);
        let v_i = func.alloc_vreg(VRegClass::Gpr64);
        let v_acc = func.alloc_vreg(VRegClass::Gpr64);

        // 24 hot-loop values used multiple times per iteration + 3 long outer-loop low-use values
        // (1 use per iteration, including one fed to `MOp::Prfm`), plus `v_n`, `v_i`, `v_acc`
        // => 30 simultaneously live non-remat GPRs across `b_loop` (> 27 GPRs!).
        let mut hot_invs: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(24, alloc);
        for _ in 0..24 {
            hot_invs.push(func.alloc_vreg(VRegClass::Gpr64));
        }
        let mut sparse_invs: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(3, alloc);
        for _ in 0..3 {
            sparse_invs.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        func.begin_block(b_entry);
        func.push_parallel_copy(
            &[
                (MReg::V(v_n), MReg::p(PReg::X0)),
                (MReg::V(v_base), MReg::p(PReg::X1)),
            ],
            MInstFlags::NONE,
        );
        for k in 0..24 {
            let imm12 = u16::try_from((k + 1) * 8).expect("imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(hot_invs[k]),
                    rn: MReg::V(v_base),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        for k in 0..3 {
            let imm12 = u16::try_from(512 + k * 64).expect("imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(sparse_invs[k]),
                    rn: MReg::V(v_base),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_zero), imm: 0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(v_zero)), (MReg::V(v_i), MReg::V(v_zero))],
            MInstFlags::NONE,
        );
        func.push_inst(MOp::B { target: b_loop }, MInstFlags::TERMINATOR);
        func.end_block(b_entry);

        func.begin_block(b_loop);
        let mut cur = v_acc;
        // Each `hot_invs[k]` is used TWICE inside `b_loop` (high spill density).
        for &hv in hot_invs.iter() {
            let t1 = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(t1),
                    rn: MReg::V(cur),
                    rm: MReg::V(hv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            let t2 = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(t2),
                    rn: MReg::V(t1),
                    rm: MReg::V(hv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            cur = t2;
        }
        // Each `sparse_invs[k]` is used ONLY ONCE inside `b_loop` (lowest spill density!).
        func.push_inst(MOp::Prfm { base: MReg::V(sparse_invs[0]), imm12: 0 }, MInstFlags::NONE);
        for &sv in &sparse_invs[1..] {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(cur),
                    rm: MReg::V(sv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            cur = next;
        }
        let v_i_next = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_i_next),
                rn: MReg::V(v_i),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(cur)), (MReg::V(v_i), MReg::V(v_i_next))],
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpReg { rn: MReg::V(v_i), rm: MReg::V(v_n), shift: None, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let br = func.push_inst(
            MOp::BCond { cond: MCond::Lo, target: b_loop },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br, 0);
        func.push_inst(MOp::B { target: b_exit }, MInstFlags::TERMINATOR);
        func.end_block(b_loop);

        func.begin_block(b_exit);
        func.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::V(v_acc))], MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        let weights = [2u32, 64, 1];
        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { allow_x29_leaf: true, block_weights: &weights, ..MRaOpts::default() },
            alloc,
        )
        .expect("30-live-value hot loop with Prfm must allocate cleanly");

        assert!(!func.is_ssa);
        assert_eq!(mverify(&func), Ok(()));
        assert!(
            out.stats.spill_slots_used <= 5,
            "30-live-value leaf using 27 GPRs should spill only sparse outer values, got {}",
            out.stats.spill_slots_used
        );

        // Count spill reloads (`Ldr` from SP) inside `b_loop`: must stay tiny (<= 6), never
        // spilling the 48 short hot temporaries `t1`/`t2`!
        let mut loop_sp_loads = 0u32;
        for i in func.block_inst_range(b_loop) {
            if matches!(func.ops[i], MOp::Ldr { .. }) {
                loop_sp_loads += 1;
            }
        }
        assert!(
            loop_sp_loads <= 6,
            "hot loop must keep spill reloads tiny (evicting sparse outer values, not short temps), got {loop_sp_loads}"
        );
    }

    #[test]
    fn mra_leaf_28_live_gprs_allocates_x30_with_zero_spills() {
        let alloc = test_alloc();

        let build_28_live = || -> MFunc<'static> {
            let mut func = new_ssa(Os::MacOs);
            let b0 = func.new_block(false);
            let v_in = func.alloc_vreg(VRegClass::Gpr64);
            let mut gprs: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(28, alloc);
            for _ in 0..28 {
                gprs.push(func.alloc_vreg(VRegClass::Gpr64));
            }
            func.begin_block(b0);
            func.push_inst(
                MOp::Mov { dst: MReg::V(v_in), src: MReg::p(PReg::X0), wide: true },
                MInstFlags::NONE,
            );
            for k in 0..28 {
                let prev = if k == 0 { v_in } else { gprs[k - 1] };
                func.push_inst(
                    MOp::AddImm {
                        dst: MReg::V(gprs[k]),
                        rn: MReg::V(prev),
                        imm12: 1,
                        shift12: false,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
            }
            let mut acc = gprs[0];
            for &v in &gprs[1..] {
                let next = func.alloc_vreg(VRegClass::Gpr64);
                func.push_inst(
                    MOp::AddReg {
                        dst: MReg::V(next),
                        rn: MReg::V(acc),
                        rm: MReg::V(v),
                        shift: None,
                        wide: true,
                        sets_flags: false,
                    },
                    MInstFlags::NONE,
                );
                acc = next;
            }
            func.push_inst(
                MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(acc), wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
            func.end_block(b0);
            func.rebuild_cfg(alloc);
            func
        };

        // 1. Leaf mode (`allow_x29_leaf: true`): 28 simultaneously live GPRs allocate `X30` (`LR`)
        //    with ZERO spills and pass `mverify` + Apple Silicon hardware execution.
        let mut func = build_28_live();
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { allow_x29_leaf: true, ..MRaOpts::default() },
            alloc,
        )
        .expect("28-GPR leaf with allow_x29_leaf must allocate cleanly");

        assert!(!func.is_ssa);
        assert_eq!(out.stats.spill_slots_used, 0, "28 live GPRs must fit in X0..X15 + X19..X30 with zero spills");
        assert_eq!(out.spill_bytes, 0);
        assert_ne!(out.used_callee_saved_gprs & (1u32 << 29), 0, "X29 must be reported in used_callee_saved_gprs");
        assert_ne!(out.used_callee_saved_gprs & (1u32 << 30), 0, "X30 must be reported in used_callee_saved_gprs");
        assert!(out.clobbers.clobbers_gpr(30), "X30 must appear in clobbers when allocated in leaf");
        assert!(!out.clobbers.clobbers_gpr(18), "X18 must remain untouched on macOS");
        assert_eq!(mverify(&func), Ok(()));

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = compile_executable(&func, &out, alloc);
            // SAFETY: Valid AAPCS64 function saving/restoring X19..=X30 and returning `28*base + 406`.
            let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
            for &base in &[0u64, 1, 7, 42, 1000] {
                let expected = 28u64.wrapping_mul(base).wrapping_add(28 * 29 / 2);
                assert_eq!(f(base), expected);
            }
        }

        // 2. Non-leaf mode (`allow_x29_leaf: false`): `X29` and `X30` stay strictly excluded.
        let mut func_no_leaf = build_28_live();
        let out_no_leaf = allocate_mfunc(&mut func_no_leaf, MRaOpts::default(), alloc)
            .expect("28-GPR non-leaf allocation must succeed with spills");
        assert_eq!(
            out_no_leaf.used_callee_saved_gprs & ((1u32 << 29) | (1u32 << 30)),
            0,
            "X29 and X30 must be strictly excluded when !allow_x29_leaf"
        );
        assert!(!out_no_leaf.clobbers.clobbers_gpr(30), "X30 must not be clobbered when !allow_x29_leaf");
        assert!(out_no_leaf.stats.spill_slots_used >= 2);
        assert_eq!(mverify(&func_no_leaf), Ok(()));
    }

    #[test]
    fn mra_multi_inst_movimm_in_hot_loop_not_rematerialized() {
        const HASH_MULT: u64 = 0x9e37_79b1_85eb_ca87;
        const CHEAP_IMM: u64 = 0x1234;

        // Verify exact instruction costs of single- and multi-instruction `MovImm` constants.
        assert_eq!(mov_imm_inst_cost(0, true), 1);
        assert_eq!(mov_imm_inst_cost(CHEAP_IMM, true), 1);
        assert_eq!(mov_imm_inst_cost(0xffff_ffff_ffff_0000, true), 1);
        assert_eq!(mov_imm_inst_cost(0x0101_0101_0101_0101, true), 1);
        assert_eq!(mov_imm_inst_cost(0x1234_5678, false), 2);
        assert_eq!(mov_imm_inst_cost(HASH_MULT, true), 4);

        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        let v_n = func.alloc_vreg(VRegClass::Gpr64);
        let v_base = func.alloc_vreg(VRegClass::Gpr64);
        let v_cold_base = func.alloc_vreg(VRegClass::Gpr64);
        let v_hash = func.alloc_vreg(VRegClass::Gpr64);
        let v_cheap = func.alloc_vreg(VRegClass::Gpr64);
        let v_zero = func.alloc_vreg(VRegClass::Gpr64);
        let v_acc = func.alloc_vreg(VRegClass::Gpr64);
        let v_i = func.alloc_vreg(VRegClass::Gpr64);

        let mut cold_vals: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(6, alloc);
        for _ in 0..6 {
            cold_vals.push(func.alloc_vreg(VRegClass::Gpr64));
        }
        let mut hot_vals: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(22, alloc);
        for _ in 0..22 {
            hot_vals.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        // Entry (weight 2): define 4-inst hash constant, 1-inst cheap constant, 6 entry/exit-only
        // values, and 22 hot-loop values -> 34 live values across `b_loop` (> 28 GPRs!).
        func.begin_block(b_entry);
        func.push_parallel_copy(
            &[
                (MReg::V(v_n), MReg::p(PReg::X0)),
                (MReg::V(v_base), MReg::p(PReg::X1)),
                (MReg::V(v_cold_base), MReg::p(PReg::X2)),
            ],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_hash), imm: HASH_MULT, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_cheap), imm: CHEAP_IMM, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        for (k, &cv) in cold_vals.iter().enumerate() {
            let imm12 = u16::try_from((k + 1) * 16).expect("imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(cv),
                    rn: MReg::V(v_cold_base),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        for (k, &hv) in hot_vals.iter().enumerate() {
            let imm12 = u16::try_from((k + 1) * 8).expect("imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(hv),
                    rn: MReg::V(v_base),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_zero), imm: 0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(v_zero)), (MReg::V(v_i), MReg::V(v_zero))],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbz { rt: MReg::V(v_n), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        // Hot loop (weight 64): uses `v_hash` (4-inst), `v_cheap` (1-inst), and all 22 `hot_vals`.
        func.begin_block(b_loop);
        let mut cur = v_acc;
        for &hv in hot_vals.iter() {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(cur),
                    rm: MReg::V(hv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            cur = next;
        }
        let hashed = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::Mul { dst: MReg::V(hashed), rn: MReg::V(cur), rm: MReg::V(v_hash), wide: true },
            MInstFlags::NONE,
        );
        let mixed = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(mixed),
                rn: MReg::V(hashed),
                rm: MReg::V(v_cheap),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let v_i_next = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::AddImm {
                dst: MReg::V(v_i_next),
                rn: MReg::V(v_i),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(mixed)), (MReg::V(v_i), MReg::V(v_i_next))],
            MInstFlags::NONE,
        );
        let cmp = func.push_inst(
            MOp::CmpReg { rn: MReg::V(v_i), rm: MReg::V(v_n), shift: None, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp, 0);
        let br = func.push_inst(
            MOp::BCond { cond: MCond::Lo, target: b_loop },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br, 0);
        func.end_block(b_loop);

        // Exit (weight 2): uses `cold_vals`.
        func.begin_block(b_exit);
        let mut exit_acc = v_acc;
        for &cv in cold_vals.iter() {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(exit_acc),
                    rm: MReg::V(cv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            exit_acc = next;
        }
        func.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::V(exit_acc))], MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        let weights = [2u32, 64, 2];
        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { allow_x29_leaf: true, block_weights: &weights, ..MRaOpts::default() },
            alloc,
        )
        .expect("hot loop with 4-inst MovImm must allocate cleanly");

        assert!(!func.is_ssa);
        assert_eq!(mverify(&func), Ok(()));
        assert!(out.stats.vregs_rematerialized >= 1, "1-inst MovImm should still rematerialize");

        let mut hash_in_entry = 0u32;
        for i in func.block_inst_range(b_entry) {
            if matches!(func.ops[i], MOp::MovImm { imm: HASH_MULT, .. }) {
                hash_in_entry += 1;
            }
        }
        assert_eq!(
            hash_in_entry, 1,
            "4-inst MovImm 0x9e3779b185ebca87 must stay defined once in b_entry (never rematerialized)"
        );

        let mut cheap_in_loop = 0u32;
        for i in func.block_inst_range(b_loop) {
            assert!(
                !matches!(func.ops[i], MOp::MovImm { imm: HASH_MULT, .. }),
                "4-inst MovImm 0x9e3779b185ebca87 must NEVER be rematerialized inside hot b_loop"
            );
            assert!(
                !matches!(func.ops[i], MOp::Ldr { .. } | MOp::Str { .. }),
                "4-inst MovImm and hot loop values must stay in registers with zero spills in b_loop, found {:?}",
                func.ops[i]
            );
            if matches!(func.ops[i], MOp::MovImm { imm: CHEAP_IMM, .. }) {
                cheap_in_loop += 1;
            }
        }
        assert!(cheap_in_loop >= 1, "1-inst MovImm 0x1234 should rematerialize inside b_loop under pressure");
    }

    #[test]
    fn mra_hot_loop_evicts_non_hot_before_long_lived_loop_counter() {
        let alloc = test_alloc();
        let mut func = new_ssa(Os::MacOs);

        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        let v_n = func.alloc_vreg(VRegClass::Gpr64);
        let v_base = func.alloc_vreg(VRegClass::Gpr64);
        let v_cold_base = func.alloc_vreg(VRegClass::Gpr64);
        let v_zero = func.alloc_vreg(VRegClass::Gpr64);
        let v_acc = func.alloc_vreg(VRegClass::Gpr64);
        let v_rem = func.alloc_vreg(VRegClass::Gpr64);

        // 8 entry/exit-only non-hot values (`!used_in_hot`, defined FIRST so they occupy registers
        // before the hot loop values) + 22 hot values + `v_acc` + `v_rem` = 32 live across `b_loop`
        // (> 28 GPRs). Exit has weight 2 so cold-exit pre-splitting does NOT trigger; eviction is
        // decided strictly by `EvictScore` (`hot_count` + damped `spill_density`).
        let mut non_hot_vals: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(8, alloc);
        for _ in 0..8 {
            non_hot_vals.push(func.alloc_vreg(VRegClass::Gpr64));
        }
        let mut hot_vals: Vec<VReg, Alloc<'static>> = Vec::with_capacity_in(22, alloc);
        for _ in 0..22 {
            hot_vals.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        func.begin_block(b_entry);
        func.push_parallel_copy(
            &[
                (MReg::V(v_n), MReg::p(PReg::X0)),
                (MReg::V(v_base), MReg::p(PReg::X1)),
                (MReg::V(v_cold_base), MReg::p(PReg::X2)),
            ],
            MInstFlags::NONE,
        );
        for (k, &cv) in non_hot_vals.iter().enumerate() {
            let imm12 = u16::try_from((k + 1) * 10).expect("imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(cv),
                    rn: MReg::V(v_cold_base),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        for (k, &hv) in hot_vals.iter().enumerate() {
            let imm12 = u16::try_from(k + 1).expect("imm12");
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(hv),
                    rn: MReg::V(v_base),
                    imm12,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
        }
        func.push_inst(
            MOp::MovImm { dst: MReg::V(v_zero), imm: 0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(v_zero)), (MReg::V(v_rem), MReg::V(v_n))],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbz { rt: MReg::V(v_n), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        // Long hot loop (weight 64, ~70 instructions) so `v_rem` spans a long live range while
        // only being decremented/tested at the bottom of `b_loop`.
        func.begin_block(b_loop);
        let mut cur = v_acc;
        for &hv in hot_vals.iter() {
            let t1 = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(t1),
                    rn: MReg::V(cur),
                    rm: MReg::V(hv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            let t2 = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(t2),
                    rn: MReg::V(t1),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            let t3 = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(t3),
                    rn: MReg::V(t2),
                    rm: MReg::V(hv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            cur = t3;
        }
        let v_rem_next = func.alloc_vreg(VRegClass::Gpr64);
        func.push_inst(
            MOp::SubImm {
                dst: MReg::V(v_rem_next),
                rn: MReg::V(v_rem),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_parallel_copy(
            &[(MReg::V(v_acc), MReg::V(cur)), (MReg::V(v_rem), MReg::V(v_rem_next))],
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Cbnz { rt: MReg::V(v_rem), target: b_loop, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_loop);

        // Exit (weight 2): consumes all 8 non-hot values.
        func.begin_block(b_exit);
        let mut exit_acc = v_acc;
        for &cv in non_hot_vals.iter() {
            let next = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(next),
                    rn: MReg::V(exit_acc),
                    rm: MReg::V(cv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            exit_acc = next;
        }
        func.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::V(exit_acc))], MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        let weights = [2u32, 64, 2];
        func.rebuild_cfg(alloc);
        let out = allocate_mfunc(
            &mut func,
            MRaOpts { allow_x29_leaf: true, block_weights: &weights, ..MRaOpts::default() },
            alloc,
        )
        .expect("hot loop with overflowing non-hot values must allocate cleanly");

        assert!(!func.is_ssa);
        assert_eq!(
            out.stats.vregs_cold_split, 0,
            "weight-2 exit must exercise EvictScore hot-vs-cold eviction, not cold-exit pre-splitting"
        );
        assert!(out.stats.spill_slots_used >= 4, "overflowing non-hot values must spill");
        assert_eq!(mverify(&func), Ok(()));

        // Prove `b_loop` has ZERO spill loads or stores (`v_rem` and all hot values stayed in regs!).
        for i in func.block_inst_range(b_loop) {
            assert!(
                !matches!(func.ops[i], MOp::Ldr { .. } | MOp::Str { .. }),
                "long-lived loop counter v_rem and hot values must never spill while !used_in_hot values exist, found {:?}",
                func.ops[i]
            );
        }

        let mut exit_reloads = 0u32;
        for i in func.block_inst_range(b_exit) {
            if matches!(func.ops[i], MOp::Ldr { .. }) {
                exit_reloads += 1;
            }
        }
        assert!(exit_reloads >= 4, "evicted non-hot values must reload in b_exit, got {exit_reloads}");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec = compile_executable(&func, &out, alloc);
            // SAFETY: Valid AAPCS64 function `(n, base, cold_base) -> u64`.
            let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
            for &n in &[0u64, 1, 3, 10] {
                let base = 5u64;
                let cold_base = 11u64;
                let per_iter = 44u64.wrapping_mul(base).wrapping_add(528);
                let cold_sum = 8u64.wrapping_mul(cold_base).wrapping_add(360);
                let expected = n.wrapping_mul(per_iter).wrapping_add(cold_sum);
                assert_eq!(f(n, base, cold_base), expected);
            }
        }
    }

    #[test]
    fn ranges_overlap_matches_linear_oracle_across_asymmetric_shapes() {
        fn oracle(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
            a.iter().any(|&(as_, ae)| b.iter().any(|&(bs, be)| as_ <= be && bs <= ae))
        }
        let long: std::vec::Vec<(u32, u32)> = (0..100).map(|i| (i * 10, i * 10 + 3)).collect();
        let probes: [&[(u32, u32)]; 9] = [
            &[],
            &[(4, 9)],
            &[(990, 990)],
            &[(994, 1050)],
            &[(4, 9), (14, 19), (994, 999)],
            &[(4, 9), (503, 505), (994, 999)],
            &[(0, 0), (10, 10)],
            &[(3, 10)],
            &[(5, 8), (15, 18), (25, 28), (35, 38), (45, 48), (55, 58)],
        ];
        for p in probes {
            assert_eq!(super::ranges_overlap(p, &long), oracle(p, &long), "probe vs long: {p:?}");
            assert_eq!(super::ranges_overlap(&long, p), oracle(&long, p), "long vs probe: {p:?}");
            for q in probes {
                assert_eq!(super::ranges_overlap(p, q), oracle(p, q), "probe vs probe: {p:?} vs {q:?}");
            }
        }
    }

    #[test]
    fn mra_sparse_vreg_liveness_matches_dense_oracle_across_cfgs() {
        use super::{
            HOT_BLOCK_WEIGHT_MIN, MAX_DATAFLOW_ITERS, NONE_U32, RangeNode, block_raw_weight,
            compute_sparse_vreg_liveness_with, range_list_add, range_list_shorten_def, slice_at,
            slice_set, subslice, to_idx, u32_of, visit_op_defs, visit_op_uses,
        };
        use crate::sota::mfunc::MBlock;

        fn dense_bitset_set(bits: &mut [u64], base: usize, id: u32) {
            let idx = to_idx(id);
            bits[base + (idx >> 6)] |= 1u64 << (idx & 63);
        }
        fn dense_bitset_test(bits: &[u64], base: usize, id: u32) -> bool {
            let idx = to_idx(id);
            (bits[base + (idx >> 6)] & (1u64 << (idx & 63))) != 0
        }

        /// Exact differential verification helper: compares `compute_sparse_vreg_liveness_with`
        /// bit-for-bit against the reference dense bitset dataflow oracle on:
        /// 1. `live_in(b, g)` for every block `b` and global VReg `g`,
        /// 2. `live_out(b, g)` for every block `b` and global VReg `g`,
        /// 3. `vreg_crosses_hot[g]` for every global VReg `g`,
        /// 4. exact Wimmer–Franz `vreg_ranges[v]` for every VReg `v` (including holes & redefs).
        fn assert_sparse_matches_dense_oracle(
            name: &str,
            func: &MFunc<'static>,
            block_weights: &[u32],
            alloc: Alloc<'static>,
        ) {
            let n_blocks = func.block_insts.len();
            let n_vregs = func.vreg_class.len();

            // Linearize blocks identically to Phase 1.
            let mut block_lin_range: Vec<(u32, u32), Alloc<'static>> =
                Vec::with_capacity_in(n_blocks, alloc);
            let mut block_ord_pos: Vec<u32, Alloc<'static>> =
                Vec::with_capacity_in(n_blocks, alloc);
            for _ in 0..n_blocks {
                block_lin_range.push((0, 0));
                block_ord_pos.push(NONE_U32);
            }
            let mut lin_to_orig: Vec<u32, Alloc<'static>> =
                Vec::with_capacity_in(func.ops.len(), alloc);
            for (ord_pos, &blk) in func.block_order.iter().enumerate() {
                let b = to_idx(blk.0);
                slice_set(&mut block_ord_pos, b, u32_of(ord_pos).unwrap_or(NONE_U32));
                let lin_s = u32_of(lin_to_orig.len()).expect("lin_s");
                let (orig_s, orig_e) = slice_at(&func.block_insts, b, (0, 0));
                for orig in to_idx(orig_s)..to_idx(orig_e) {
                    lin_to_orig.push(u32_of(orig).expect("orig"));
                }
                let lin_e = u32_of(lin_to_orig.len()).expect("lin_e");
                slice_set(&mut block_lin_range, b, (lin_s, lin_e));
            }

            // Collect upward uses & global defs identically to Phase 2.
            let mut def_stamp: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_vregs, alloc);
            let mut use_stamp: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_vregs, alloc);
            let mut vreg_to_global: Vec<u32, Alloc<'static>> =
                Vec::with_capacity_in(n_vregs, alloc);
            for _ in 0..n_vregs {
                def_stamp.push(0);
                use_stamp.push(0);
                vreg_to_global.push(NONE_U32);
            }
            let mut global_to_vreg: Vec<u32, Alloc<'static>> = Vec::new_in(alloc);
            let mut upward_uses: Vec<(u32, u32), Alloc<'static>> = Vec::new_in(alloc);
            for (ord_idx, &blk) in func.block_order.iter().enumerate() {
                let b = to_idx(blk.0);
                let stamp = u32_of(ord_idx + 1).expect("stamp");
                let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
                for p in to_idx(ls)..to_idx(le) {
                    let orig = to_idx(slice_at(&lin_to_orig, p, 0));
                    let op = &func.ops[orig];
                    visit_op_uses(op, &func.copy_pairs, |r| {
                        if let MReg::V(v) = r {
                            let vi = to_idx(v.0);
                            if vi < n_vregs && def_stamp[vi] != stamp && use_stamp[vi] != stamp {
                                use_stamp[vi] = stamp;
                                let g = match vreg_to_global[vi] {
                                    NONE_U32 => {
                                        let ng = u32_of(global_to_vreg.len()).expect("ng");
                                        vreg_to_global[vi] = ng;
                                        global_to_vreg.push(v.0);
                                        ng
                                    }
                                    existing => existing,
                                };
                                upward_uses.push((g, blk.0));
                            }
                        }
                    });
                    visit_op_defs(op, &func.copy_pairs, |r| {
                        if let MReg::V(v) = r {
                            def_stamp[to_idx(v.0)] = stamp;
                        }
                    });
                }
            }
            let mut global_defs: Vec<(u32, u32), Alloc<'static>> = Vec::new_in(alloc);
            def_stamp.fill(0);
            for (ord_idx, &blk) in func.block_order.iter().enumerate() {
                let b = to_idx(blk.0);
                let stamp = u32_of(ord_idx + 1).expect("stamp");
                let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
                for p in to_idx(ls)..to_idx(le) {
                    let orig = to_idx(slice_at(&lin_to_orig, p, 0));
                    let op = &func.ops[orig];
                    visit_op_defs(op, &func.copy_pairs, |r| {
                        if let MReg::V(v) = r {
                            let vi = to_idx(v.0);
                            let g = vreg_to_global[vi];
                            if g != NONE_U32 && def_stamp[vi] != stamp {
                                def_stamp[vi] = stamp;
                                global_defs.push((g, blk.0));
                            }
                        }
                    });
                }
            }

            let mut block_is_hot: Vec<bool, Alloc<'static>> = Vec::new_in(alloc);
            if !block_weights.is_empty() {
                for _ in 0..n_blocks {
                    block_is_hot.push(false);
                }
                for &blk in func.block_order.iter() {
                    if block_raw_weight(block_weights, blk) >= HOT_BLOCK_WEIGHT_MIN {
                        block_is_hot[to_idx(blk.0)] = true;
                    }
                }
            }

            // 1. Dense bitset fixed-point reference oracle.
            let n_global = global_to_vreg.len();
            let words_per_block = n_global.div_ceil(64);
            let total_words = n_blocks * words_per_block;
            let mut v_use = std::vec![0u64; total_words];
            let mut v_def = std::vec![0u64; total_words];
            let mut dense_live_in = std::vec![0u64; total_words];
            let mut dense_live_out = std::vec![0u64; total_words];
            if words_per_block > 0 {
                for &(g, b_u32) in upward_uses.iter() {
                    dense_bitset_set(&mut v_use, to_idx(b_u32) * words_per_block, g);
                }
                for &(g, b_u32) in global_defs.iter() {
                    dense_bitset_set(&mut v_def, to_idx(b_u32) * words_per_block, g);
                }
                for _ in 0..MAX_DATAFLOW_ITERS {
                    let mut changed = false;
                    for &blk in func.block_order.iter().rev() {
                        let b = to_idx(blk.0);
                        let base = b * words_per_block;
                        for w in 0..words_per_block {
                            let mut new_out = 0u64;
                            for &succ in func.block_succs(blk) {
                                new_out |= dense_live_in[to_idx(succ.0) * words_per_block + w];
                            }
                            let idx = base + w;
                            let new_in = v_use[idx] | (new_out & !v_def[idx]);
                            if new_out != dense_live_out[idx] || new_in != dense_live_in[idx] {
                                dense_live_out[idx] = new_out;
                                dense_live_in[idx] = new_in;
                                changed = true;
                            }
                        }
                    }
                    if !changed {
                        break;
                    }
                }
            }

            // 2. Run production sparse per-global-VReg backward CFG BFS and record every bit.
            let mut sparse_live_in = std::vec![0u64; total_words];
            let mut sparse_live_out = std::vec![0u64; total_words];
            let sparse = compute_sparse_vreg_liveness_with(
                func,
                &block_lin_range,
                &block_ord_pos,
                &block_is_hot,
                &global_to_vreg,
                &upward_uses,
                &global_defs,
                alloc,
                |g, b_u32, is_live_out| {
                    let base = to_idx(b_u32) * words_per_block;
                    if is_live_out {
                        dense_bitset_set(&mut sparse_live_out, base, g);
                    } else {
                        dense_bitset_set(&mut sparse_live_in, base, g);
                    }
                },
            )
            .expect("sparse BFS must succeed");

            // Assert exact bit-for-bit equality of `live_in` and `live_out`.
            assert_eq!(sparse_live_in, dense_live_in, "{name}: live_in mismatch");
            assert_eq!(sparse_live_out, dense_live_out, "{name}: live_out mismatch");

            // Assert exact equality of `vreg_crosses_hot`.
            if !block_is_hot.is_empty() {
                for g in 0..n_global {
                    let g_u32 = u32_of(g).expect("g");
                    let mut dense_hot = false;
                    for &blk in func.block_order.iter() {
                        if block_raw_weight(block_weights, blk) >= HOT_BLOCK_WEIGHT_MIN {
                            let base = to_idx(blk.0) * words_per_block;
                            if dense_bitset_test(&dense_live_in, base, g_u32)
                                || dense_bitset_test(&dense_live_out, base, g_u32)
                            {
                                dense_hot = true;
                                break;
                            }
                        }
                    }
                    assert_eq!(
                        sparse.vreg_crosses_hot[g], dense_hot,
                        "{name}: vreg_crosses_hot mismatch for g={g} (v={})",
                        global_to_vreg[g]
                    );
                }
            }

            // 3. Assert exact Wimmer–Franz `vreg_ranges` equality (dense Phase 3 vs sparse Phase 3 + merge).
            let mut dense_heads: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_vregs, alloc);
            let mut sparse_heads: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(n_vregs, alloc);
            for _ in 0..n_vregs {
                dense_heads.push(NONE_U32);
                sparse_heads.push(NONE_U32);
            }
            let mut dense_nodes: Vec<RangeNode, Alloc<'static>> = Vec::new_in(alloc);
            let mut sparse_nodes: Vec<RangeNode, Alloc<'static>> = Vec::new_in(alloc);
            for &blk in func.block_order.iter().rev() {
                let b = to_idx(blk.0);
                let (ls, le) = slice_at(&block_lin_range, b, (0, 0));
                if ls >= le {
                    continue;
                }
                let blk_start_pt = ls * 2;
                let blk_end_pt = (le - 1) * 2 + 1;
                // Dense seeding
                for w in 0..words_per_block {
                    let mut bits = dense_live_out[b * words_per_block + w];
                    while bits != 0 {
                        let tz = bits.trailing_zeros();
                        bits &= bits - 1;
                        let g_idx = (w << 6) + to_idx(tz);
                        let v_u32 = global_to_vreg[g_idx];
                        range_list_add(
                            &mut dense_heads[to_idx(v_u32)],
                            &mut dense_nodes,
                            blk_start_pt,
                            blk_end_pt,
                        );
                    }
                }
                // Sparse seeding (`def_and_live_out` only)
                let dlo_s = to_idx(slice_at(&sparse.def_lo_off, b, 0));
                let dlo_e = to_idx(slice_at(&sparse.def_lo_off, b + 1, 0));
                for &v_u32 in subslice(&sparse.def_lo_vregs, dlo_s, dlo_e) {
                    range_list_add(
                        &mut sparse_heads[to_idx(v_u32)],
                        &mut sparse_nodes,
                        blk_start_pt,
                        blk_end_pt,
                    );
                }
                // Reverse intra-block instruction walk for both
                let mut p = to_idx(le);
                while p > to_idx(ls) {
                    p -= 1;
                    let orig = to_idx(lin_to_orig[p]);
                    let op = &func.ops[orig];
                    let use_pt = u32_of(p).expect("p") * 2;
                    let def_pt = use_pt + 1;
                    visit_op_defs(op, &func.copy_pairs, |r| {
                        if let MReg::V(v) = r {
                            range_list_shorten_def(
                                &mut dense_heads[to_idx(v.0)],
                                &mut dense_nodes,
                                blk_start_pt,
                                def_pt,
                            );
                            range_list_shorten_def(
                                &mut sparse_heads[to_idx(v.0)],
                                &mut sparse_nodes,
                                blk_start_pt,
                                def_pt,
                            );
                        }
                    });
                    visit_op_uses(op, &func.copy_pairs, |r| {
                        if let MReg::V(v) = r {
                            range_list_add(
                                &mut dense_heads[to_idx(v.0)],
                                &mut dense_nodes,
                                blk_start_pt,
                                use_pt,
                            );
                            range_list_add(
                                &mut sparse_heads[to_idx(v.0)],
                                &mut sparse_nodes,
                                blk_start_pt,
                                use_pt,
                            );
                        }
                    });
                }
            }
            for vi in 0..n_vregs {
                let mut dense_vec = std::vec::Vec::new();
                let mut cur = dense_heads[vi];
                while cur != NONE_U32 {
                    let n = dense_nodes[to_idx(cur)];
                    dense_vec.push((n.start, n.end));
                    cur = n.next;
                }
                let pt_slice = match vreg_to_global[vi] {
                    NONE_U32 => &[],
                    g => {
                        let gi = to_idx(g);
                        let ps = to_idx(slice_at(&sparse.pass_through_off, gi, 0));
                        let pe = to_idx(slice_at(&sparse.pass_through_off, gi + 1, 0));
                        subslice(&sparse.pass_through_ranges, ps, pe)
                    }
                };
                let mut sparse_vec: std::vec::Vec<(u32, u32)> = std::vec::Vec::new();
                let mut cur_a = sparse_heads[vi];
                let mut pt_idx = 0usize;
                while cur_a != NONE_U32 || pt_idx < pt_slice.len() {
                    let na = (cur_a != NONE_U32).then(|| sparse_nodes[to_idx(cur_a)]);
                    let nb = pt_slice.get(pt_idx).copied();
                    let (s, e) = match (na, nb) {
                        (Some(a), Some((bs, _be))) if a.start <= bs => {
                            cur_a = a.next;
                            (a.start, a.end)
                        }
                        (Some(_), Some((bs, be))) => {
                            pt_idx += 1;
                            (bs, be)
                        }
                        (Some(a), None) => {
                            cur_a = a.next;
                            (a.start, a.end)
                        }
                        (None, Some((bs, be))) => {
                            pt_idx += 1;
                            (bs, be)
                        }
                        (None, None) => break,
                    };
                    if let Some(last) = sparse_vec.last_mut()
                        && s <= last.1.saturating_add(1)
                    {
                        last.1 = last.1.max(e);
                    } else {
                        sparse_vec.push((s, e));
                    }
                }
                assert_eq!(sparse_vec, dense_vec, "{name}: live range mismatch for vreg {vi}");
            }
            let _ = MBlock(0);
        }

        let alloc = test_alloc();

        // Case 1: Diamond + partial redef + same-block use-before-def + redef + trailing dead def.
        {
            let mut func = new_ssa(Os::MacOs);
            let b0 = func.new_block(false);
            let b1 = func.new_block(false);
            let b2 = func.new_block(false);
            let b3 = func.new_block(false);
            let v_cond = func.alloc_vreg(VRegClass::Gpr64);
            let v_x = func.alloc_vreg(VRegClass::Gpr64);
            let v_y = func.alloc_vreg(VRegClass::Gpr64);
            let v_local = func.alloc_vreg(VRegClass::Gpr64);
            let v_res = func.alloc_vreg(VRegClass::Gpr64);

            func.begin_block(b0);
            func.push_parallel_copy(
                &[
                    (MReg::V(v_cond), MReg::p(PReg::X0)),
                    (MReg::V(v_x), MReg::p(PReg::X1)),
                    (MReg::V(v_y), MReg::p(PReg::X2)),
                ],
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::Cbz { rt: MReg::V(v_cond), target: b2, wide: true },
                MInstFlags::TERMINATOR,
            );
            func.end_block(b0);

            // b1: use-before-def of v_x, then redef v_x, then use v_x into v_local, then dead redef of v_x!
            func.begin_block(b1);
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(v_x),
                    rn: MReg::V(v_x),
                    imm12: 7,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(v_local),
                    rn: MReg::V(v_x),
                    imm12: 3,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_x),
                    rn: MReg::V(v_local),
                    rm: MReg::V(v_y),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::B { target: b3 }, MInstFlags::TERMINATOR);
            func.end_block(b1);

            // b2: pass-through for v_x, redefs v_y, plus a dead def of v_local at end before fallthrough.
            func.begin_block(b2);
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(v_y),
                    rn: MReg::V(v_y),
                    imm12: 11,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::MovImm { dst: MReg::V(v_local), imm: 99, wide: true },
                MInstFlags::NONE,
            );
            func.end_block(b2);

            func.begin_block(b3);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_res),
                    rn: MReg::V(v_x),
                    rm: MReg::V(v_y),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_res), wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
            func.end_block(b3);

            func.rebuild_cfg(alloc);
            assert_sparse_matches_dense_oracle("diamond_redef", &func, &[], alloc);
            allocate_mfunc(&mut func, MRaOpts::default(), alloc).expect("diamond allocate");
            assert_eq!(mverify(&func), Ok(()));
        }

        // Case 2: Nested loops + empty pass-through block + hot/cold weights + cold-exit value.
        {
            let mut func = new_ssa(Os::MacOs);
            let b_entry = func.new_block(false);
            let b_outer = func.new_block(true);
            let b_empty_hot = func.new_block(false);
            let b_inner = func.new_block(true);
            let b_outer_latch = func.new_block(false);
            let b_exit = func.new_block(false);

            let v_n = func.alloc_vreg(VRegClass::Gpr64);
            let v_m = func.alloc_vreg(VRegClass::Gpr64);
            let v_cold = func.alloc_vreg(VRegClass::Gpr64);
            let v_acc = func.alloc_vreg(VRegClass::Gpr64);
            let v_i = func.alloc_vreg(VRegClass::Gpr64);
            let v_j = func.alloc_vreg(VRegClass::Gpr64);

            func.begin_block(b_entry);
            func.push_parallel_copy(
                &[
                    (MReg::V(v_n), MReg::p(PReg::X0)),
                    (MReg::V(v_m), MReg::p(PReg::X1)),
                    (MReg::V(v_cold), MReg::p(PReg::X2)),
                ],
                MInstFlags::NONE,
            );
            func.push_inst(MOp::MovImm { dst: MReg::V(v_acc), imm: 0, wide: true }, MInstFlags::NONE);
            func.push_inst(MOp::Mov { dst: MReg::V(v_i), src: MReg::V(v_n), wide: true }, MInstFlags::NONE);
            func.push_inst(MOp::Cbz { rt: MReg::V(v_i), target: b_exit, wide: true }, MInstFlags::TERMINATOR);
            func.end_block(b_entry);

            func.begin_block(b_outer);
            func.push_inst(MOp::Mov { dst: MReg::V(v_j), src: MReg::V(v_m), wide: true }, MInstFlags::NONE);
            func.end_block(b_outer);

            // Empty hot pass-through block (0 insts, falls through to b_inner).
            func.begin_block(b_empty_hot);
            func.end_block(b_empty_hot);

            func.begin_block(b_inner);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_acc),
                    rn: MReg::V(v_acc),
                    rm: MReg::V(v_j),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::SubImm {
                    dst: MReg::V(v_j),
                    rn: MReg::V(v_j),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Cbnz { rt: MReg::V(v_j), target: b_inner, wide: true }, MInstFlags::TERMINATOR);
            func.end_block(b_inner);

            func.begin_block(b_outer_latch);
            func.push_inst(
                MOp::SubImm {
                    dst: MReg::V(v_i),
                    rn: MReg::V(v_i),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Cbnz { rt: MReg::V(v_i), target: b_outer, wide: true }, MInstFlags::TERMINATOR);
            func.end_block(b_outer_latch);

            func.begin_block(b_exit);
            let v_out = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_out),
                    rn: MReg::V(v_acc),
                    rm: MReg::V(v_cold),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_out), wide: true }, MInstFlags::NONE);
            func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
            func.end_block(b_exit);

            let weights = [2u32, 16, 32, 64, 16, 1];
            func.rebuild_cfg(alloc);
            assert_sparse_matches_dense_oracle("nested_loops_hot_cold", &func, &weights, alloc);
            let out = allocate_mfunc(
                &mut func,
                MRaOpts { block_weights: &weights, ..MRaOpts::default() },
                alloc,
            )
            .expect("nested loops allocate");
            assert_eq!(out.stats.vregs_cold_split, 1, "v_cold must cold-split across hot nested loop");
            assert_eq!(mverify(&func), Ok(()));
        }

        // Case 3: Irreducible two-entry loop (`entry -> A`, `entry -> B`, `A <-> B`, `A/B -> exit`)
        // plus an unreachable block in `block_order` AND a detached block outside `block_order`.
        {
            let mut func = new_ssa(Os::MacOs);
            let b_entry = func.new_block(false);
            let b_a = func.new_block(false);
            let b_b = func.new_block(false);
            let b_exit = func.new_block(false);
            let b_unreach_in_order = func.new_block(false);
            let b_detached = func.new_block(false);

            let v_sel = func.alloc_vreg(VRegClass::Gpr64);
            let v_p = func.alloc_vreg(VRegClass::Gpr64);
            let v_q = func.alloc_vreg(VRegClass::Gpr64);
            let v_cnt = func.alloc_vreg(VRegClass::Gpr64);

            func.begin_block(b_entry);
            func.push_parallel_copy(
                &[
                    (MReg::V(v_sel), MReg::p(PReg::X0)),
                    (MReg::V(v_p), MReg::p(PReg::X1)),
                    (MReg::V(v_q), MReg::p(PReg::X2)),
                    (MReg::V(v_cnt), MReg::p(PReg::X3)),
                ],
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Cbz { rt: MReg::V(v_sel), target: b_b, wide: true }, MInstFlags::TERMINATOR);
            func.end_block(b_entry);

            // A: uses v_p & v_q, redefs v_p, jumps to B or exit.
            func.begin_block(b_a);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_p),
                    rn: MReg::V(v_p),
                    rm: MReg::V(v_q),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::SubImm {
                    dst: MReg::V(v_cnt),
                    rn: MReg::V(v_cnt),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Cbz { rt: MReg::V(v_cnt), target: b_exit, wide: true }, MInstFlags::TERMINATOR);
            func.end_block(b_a);

            // B: uses v_q & v_p, redefs v_q, branches back to A or falls through to exit.
            func.begin_block(b_b);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_q),
                    rn: MReg::V(v_q),
                    rm: MReg::V(v_p),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::SubImm {
                    dst: MReg::V(v_cnt),
                    rn: MReg::V(v_cnt),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Cbnz { rt: MReg::V(v_cnt), target: b_a, wide: true }, MInstFlags::TERMINATOR);
            func.end_block(b_b);

            func.begin_block(b_exit);
            let v_sum = func.alloc_vreg(VRegClass::Gpr64);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_sum),
                    rn: MReg::V(v_p),
                    rm: MReg::V(v_q),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_sum), wide: true }, MInstFlags::NONE);
            func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
            func.end_block(b_exit);

            // Unreachable block still in `block_order` branching into `b_a`.
            func.begin_block(b_unreach_in_order);
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(v_p),
                    rn: MReg::V(v_p),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::B { target: b_a }, MInstFlags::TERMINATOR);
            func.end_block(b_unreach_in_order);

            // Detached block: has insts & an edge into `b_b`, then removed from `block_order`
            // to verify `block_ord_pos[pb] == NONE_U32` filtering matches dense dataflow!
            func.begin_block(b_detached);
            func.push_inst(MOp::B { target: b_b }, MInstFlags::TERMINATOR);
            func.end_block(b_detached);
            func.block_order.pop();

            let weights = [2u32, 32, 32, 1, 1, 64];
            func.rebuild_cfg(alloc);
            assert_sparse_matches_dense_oracle("irreducible_and_detached", &func, &weights, alloc);

            // Restore `b_detached` in `block_order` so `mverify` accepts the full function for `allocate_mfunc`.
            func.block_order.push(b_detached);
            func.rebuild_cfg(alloc);
            assert_sparse_matches_dense_oracle("irreducible_all_in_order", &func, &weights, alloc);
            allocate_mfunc(
                &mut func,
                MRaOpts { block_weights: &weights, ..MRaOpts::default() },
                alloc,
            )
            .expect("irreducible allocate");
            assert_eq!(mverify(&func), Ok(()));
        }

        // Case 4: Single-block self-loop with use-before-def + multi-def + loop-invariant + local VReg.
        {
            let mut func = new_ssa(Os::MacOs);
            let b_entry = func.new_block(false);
            let b_loop = func.new_block(true);
            let b_exit = func.new_block(false);

            let v_inv = func.alloc_vreg(VRegClass::Gpr64);
            let v_acc = func.alloc_vreg(VRegClass::Gpr64);
            let v_cnt = func.alloc_vreg(VRegClass::Gpr64);
            let v_tmp = func.alloc_vreg(VRegClass::Gpr64);

            func.begin_block(b_entry);
            func.push_parallel_copy(
                &[
                    (MReg::V(v_inv), MReg::p(PReg::X0)),
                    (MReg::V(v_acc), MReg::p(PReg::X1)),
                    (MReg::V(v_cnt), MReg::p(PReg::X2)),
                ],
                MInstFlags::NONE,
            );
            func.end_block(b_entry);

            // Self-loop: reads v_acc & v_inv, writes v_tmp (block-local), redefs v_acc TWICE,
            // decrements v_cnt, branches back to b_loop.
            func.begin_block(b_loop);
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_tmp),
                    rn: MReg::V(v_acc),
                    rm: MReg::V(v_inv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(v_acc),
                    rn: MReg::V(v_tmp),
                    imm12: 3,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::AddReg {
                    dst: MReg::V(v_acc),
                    rn: MReg::V(v_acc),
                    rm: MReg::V(v_inv),
                    shift: None,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::SubImm {
                    dst: MReg::V(v_cnt),
                    rn: MReg::V(v_cnt),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.push_inst(
                MOp::Cbnz { rt: MReg::V(v_cnt), target: b_loop, wide: true },
                MInstFlags::TERMINATOR,
            );
            func.end_block(b_loop);

            func.begin_block(b_exit);
            func.push_inst(
                MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v_acc), wide: true },
                MInstFlags::NONE,
            );
            func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
            func.end_block(b_exit);

            let weights = [1u32, 64, 1];
            func.rebuild_cfg(alloc);
            assert_sparse_matches_dense_oracle("single_block_self_loop", &func, &weights, alloc);
            allocate_mfunc(
                &mut func,
                MRaOpts { block_weights: &weights, ..MRaOpts::default() },
                alloc,
            )
            .expect("self-loop allocate");
            assert_eq!(mverify(&func), Ok(()));
        }
    }

    #[test]
    fn mra_mega_function_sparse_liveness_stays_within_tight_memory_budget() {
        use core::alloc::{AllocError, Allocator, Layout};
        use core::ptr::NonNull;
        use core::sync::atomic::{AtomicUsize, Ordering};
        use blaze_own::Erased;
        use blaze_own::testing::TestHeap;

        /// Test allocator enforcing a hard live-byte ceiling and tracking exact peak live bytes.
        struct BudgetAlloc {
            live: AtomicUsize,
            peak: AtomicUsize,
            limit: AtomicUsize,
        }
        impl BudgetAlloc {
            fn new(limit: usize) -> Self {
                Self {
                    live: AtomicUsize::new(0),
                    peak: AtomicUsize::new(0),
                    limit: AtomicUsize::new(limit),
                }
            }
            fn live_bytes(&self) -> usize {
                self.live.load(Ordering::Relaxed)
            }
            fn peak_bytes(&self) -> usize {
                self.peak.load(Ordering::Relaxed)
            }
            fn reset_peak_and_set_limit(&self, new_limit: usize) {
                let cur = self.live.load(Ordering::Relaxed);
                self.peak.store(cur, Ordering::Relaxed);
                self.limit.store(new_limit, Ordering::Relaxed);
            }
            fn reserve_bytes(&self, add: usize) -> Result<(), AllocError> {
                let mut cur = self.live.load(Ordering::Relaxed);
                loop {
                    let next = cur.checked_add(add).ok_or(AllocError)?;
                    if next > self.limit.load(Ordering::Relaxed) {
                        return Err(AllocError);
                    }
                    match self.live.compare_exchange_weak(
                        cur,
                        next,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            let mut p = self.peak.load(Ordering::Relaxed);
                            while next > p {
                                match self.peak.compare_exchange_weak(
                                    p,
                                    next,
                                    Ordering::AcqRel,
                                    Ordering::Relaxed,
                                ) {
                                    Ok(_) => break,
                                    Err(actual) => p = actual,
                                }
                            }
                            return Ok(());
                        }
                        Err(actual) => cur = actual,
                    }
                }
            }
            fn release_bytes(&self, sub: usize) {
                self.live.fetch_sub(sub, Ordering::AcqRel);
            }
        }
        // SAFETY: Delegates actual allocation to `TestHeap` while atomically tracking live bytes.
        unsafe impl Allocator for BudgetAlloc {
            fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.reserve_bytes(layout.size())?;
                match TestHeap.allocate(layout) {
                    Ok(ptr) => Ok(ptr),
                    Err(e) => {
                        self.release_bytes(layout.size());
                        Err(e)
                    }
                }
            }
            fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
                self.reserve_bytes(layout.size())?;
                match TestHeap.allocate_zeroed(layout) {
                    Ok(ptr) => Ok(ptr),
                    Err(e) => {
                        self.release_bytes(layout.size());
                        Err(e)
                    }
                }
            }
            unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
                // SAFETY: Caller contract matches `TestHeap::deallocate`.
                unsafe { TestHeap.deallocate(ptr, layout) };
                self.release_bytes(layout.size());
            }
            unsafe fn grow(
                &self,
                ptr: NonNull<u8>,
                old: Layout,
                new: Layout,
            ) -> Result<NonNull<[u8]>, AllocError> {
                let delta = new.size().saturating_sub(old.size());
                self.reserve_bytes(delta)?;
                // SAFETY: Caller contract matches `TestHeap::grow`.
                match unsafe { TestHeap.grow(ptr, old, new) } {
                    Ok(p) => Ok(p),
                    Err(e) => {
                        self.release_bytes(delta);
                        Err(e)
                    }
                }
            }
            unsafe fn grow_zeroed(
                &self,
                ptr: NonNull<u8>,
                old: Layout,
                new: Layout,
            ) -> Result<NonNull<[u8]>, AllocError> {
                let delta = new.size().saturating_sub(old.size());
                self.reserve_bytes(delta)?;
                // SAFETY: Caller contract matches `TestHeap::grow_zeroed`.
                match unsafe { TestHeap.grow_zeroed(ptr, old, new) } {
                    Ok(p) => Ok(p),
                    Err(e) => {
                        self.release_bytes(delta);
                        Err(e)
                    }
                }
            }
            unsafe fn shrink(
                &self,
                ptr: NonNull<u8>,
                old: Layout,
                new: Layout,
            ) -> Result<NonNull<[u8]>, AllocError> {
                // SAFETY: Caller contract matches `TestHeap::shrink`.
                let res = unsafe { TestHeap.shrink(ptr, old, new) };
                if res.is_ok() {
                    self.release_bytes(old.size().saturating_sub(new.size()));
                }
                res
            }
        }

        // 16,384 blocks x 16,384 cross-block global VRegs:
        // 4 dense bitsets (`v_use`, `v_def`, `v_live_in`, `v_live_out`) would require:
        //   4 * 16_384 * (16_384 / 64) * 8 bytes = 128 MiB!
        // Plus one global accumulator `v_long` live across ALL 16,384 blocks!
        const N_BLOCKS: usize = 16_384;
        const DENSE_BITSET_BYTES: usize = 4 * N_BLOCKS * (N_BLOCKS / 64) * 8;
        const TIGHT_MRA_BUDGET_BYTES: usize = 12 * 1024 * 1024; // 12 MiB (< 1/10 of 128 MiB!)
        assert_eq!(DENSE_BITSET_BYTES, 128 * 1024 * 1024);

        let budget = Erased::new(BudgetAlloc::new(usize::MAX));
        let alloc = budget.handle();
        let mut func = MFunc::new(MFuncOpts {
            os: Os::MacOs,
            is_ssa: true,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc,
        });

        let mut blocks: Vec<crate::sota::mfunc::MBlock, Alloc<'_>> =
            Vec::with_capacity_in(N_BLOCKS, alloc);
        for _ in 0..N_BLOCKS {
            blocks.push(func.new_block(false));
        }
        let v_long = func.alloc_vreg(VRegClass::Gpr64);
        let mut chain_vregs: Vec<VReg, Alloc<'_>> = Vec::with_capacity_in(N_BLOCKS, alloc);
        for _ in 0..N_BLOCKS {
            chain_vregs.push(func.alloc_vreg(VRegClass::Gpr64));
        }

        // Block 0: defines `v_long` and `chain_vregs[0]`.
        func.begin_block(blocks[0]);
        func.push_parallel_copy(
            &[
                (MReg::V(v_long), MReg::p(PReg::X0)),
                (MReg::V(chain_vregs[0]), MReg::p(PReg::X1)),
            ],
            MInstFlags::NONE,
        );
        func.end_block(blocks[0]);

        // Blocks 1..N_BLOCKS-1: each block uses `chain_vregs[i - 1]` (making it upward-exposed /
        // global!) and defines `chain_vregs[i]`. `v_long` passes through all 16,384 blocks.
        for i in 1..(N_BLOCKS - 1) {
            func.begin_block(blocks[i]);
            func.push_inst(
                MOp::AddImm {
                    dst: MReg::V(chain_vregs[i]),
                    rn: MReg::V(chain_vregs[i - 1]),
                    imm12: 1,
                    shift12: false,
                    wide: true,
                    sets_flags: false,
                },
                MInstFlags::NONE,
            );
            func.end_block(blocks[i]);
        }

        // Final block: consumes both `chain_vregs[N_BLOCKS - 2]` and `v_long` (so all 16,384
        // VRegs are genuinely upward-exposed cross-block globals!).
        let last = N_BLOCKS - 1;
        func.begin_block(blocks[last]);
        func.push_inst(
            MOp::AddReg {
                dst: MReg::V(chain_vregs[last]),
                rn: MReg::V(chain_vregs[last - 1]),
                rm: MReg::V(v_long),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(chain_vregs[last]), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(blocks[last]);
        func.rebuild_cfg(alloc);

        // Lock in a tight 24 MiB headroom ceiling above `func`'s own baseline live bytes.
        let baseline_live = budget.inner().live_bytes();
        budget
            .inner()
            .reset_peak_and_set_limit(baseline_live + TIGHT_MRA_BUDGET_BYTES);

        let t0 = std::time::Instant::now();
        let out = allocate_mfunc(&mut func, MRaOpts::default(), alloc)
            .expect("16384x16384 mega-function must allocate cleanly under tight 24 MiB budget");
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let peak_delta = budget.inner().peak_bytes().saturating_sub(baseline_live);

        assert!(!func.is_ssa);
        assert_eq!(out.stats.spill_slots_used, 0);
        assert_eq!(mverify(&func), Ok(()));
        assert!(
            peak_delta <= TIGHT_MRA_BUDGET_BYTES,
            "peak MRA allocation delta ({peak_delta} bytes) exceeded budget ({TIGHT_MRA_BUDGET_BYTES} bytes); dense bitsets alone would be {DENSE_BITSET_BYTES} bytes"
        );
        std::eprintln!(
            "mega-function 16384x16384: peak_delta={:.2} MiB (dense bitsets would be 128.00 MiB), elapsed={elapsed_ms:.1} ms",
            peak_delta as f64 / (1024.0 * 1024.0)
        );
    }
}
